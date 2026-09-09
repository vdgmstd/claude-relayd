//! The detached owner of one real claude process, keyed by session id.
//!
//! It outlives every panel: a client going away never closes claude's stdin and
//! never signals it. Only an explicit kill or the idle timeout ends the session.

use std::fs::File;
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::session::{Applied, Ctl, Ctx, deliver_exit, pump_stderr, pump_stdout, serve_client};
use crate::state::{self, Claim, Holder};

/// How long the daemon waits after the last client leaves before deciding
/// whether that was a closed tab or a dead extension host.
const TAB_GRACE: Duration = Duration::from_secs(10);
const KILL_GRACE: Duration = Duration::from_secs(5);
const STATUS_INTERVAL: Duration = Duration::from_secs(2);

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// Detaches from the panel's session and terminal before serving. A daemon still
/// holding the panel's stdio dies with the panel, which defeats the whole tool.
fn detach_process(log: &Path) -> Result<()> {
    let file = state::append_log(log)?;
    let null = File::open("/dev/null").context("open /dev/null")?;
    // SAFETY: setsid and dup2 on fds this process owns.
    unsafe {
        // A daemon left in the panel's session dies with the panel, which defeats
        // the whole tool, so a failure here is fatal rather than ignored.
        if libc::setsid() < 0 {
            bail!("leave the panel's session: {}", std::io::Error::last_os_error());
        }
        if libc::dup2(std::os::fd::AsRawFd::as_raw_fd(&null), 0) < 0
            || libc::dup2(std::os::fd::AsRawFd::as_raw_fd(&file), 1) < 0
            || libc::dup2(std::os::fd::AsRawFd::as_raw_fd(&file), 2) < 0
        {
            bail!("redirect the daemon stdio to {}", log.display());
        }
    }
    Ok(())
}

fn ignore_panel_signals() {
    // A window close arrives as signals to the whole process group.
    // SAFETY: setting SIG_IGN for three signals.
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
    }
}

fn spawn_claude(bin: &str, args: &[String], spawn_record: &Path) -> Result<(Child, String)> {
    let record = state::read_json(spawn_record);
    // It carries the panel's whole environment and is wanted exactly once, so it
    // stops existing here rather than at the end of a session that may not come.
    state::remove_quietly(spawn_record);
    let cwd = record
        .as_ref()
        .and_then(|r| r.get("cwd"))
        .and_then(Value::as_str)
        .map_or_else(|| ".".to_string(), str::to_string);
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(env) = record.as_ref().and_then(|r| r.get("env")).and_then(Value::as_object) {
        cmd.env_clear();
        for (k, v) in env {
            if let Some(v) = v.as_str() {
                cmd.env(k, v);
            }
        }
    }
    // SAFETY: async-signal-safe calls only. The daemon ignores these signals and
    // that disposition is inherited, so claude must get its own back or a kill
    // would never reach it.
    unsafe {
        cmd.pre_exec(|| {
            libc::signal(libc::SIGHUP, libc::SIG_DFL);
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
            Ok(())
        });
    }
    let child = cmd.spawn().with_context(|| format!("spawn {bin}"))?;
    Ok((child, cwd))
}

fn signal_claude(ctx: &Ctx, pid: i32, sig: i32) {
    if ctx.reaped.load(Ordering::SeqCst) {
        return;
    }
    // SAFETY: kill on a pid this process owns as an unreaped child.
    unsafe {
        libc::kill(pid, sig);
    }
}

pub(crate) fn run(id: &str, bin: &str, args: &[String], idle: Duration) -> Result<u8> {
    let dir = state::ensure_home()?;
    // Fork into the background before any thread exists: the caller reaps this
    // process at once, so no zombie is left and the daemon is orphaned to init.
    // SAFETY: single-threaded at this point.
    match unsafe { libc::fork() } {
        -1 => bail!("fork the session daemon"),
        0 => {}
        _ => return Ok(0),
    }
    detach_process(&state::file(&dir, id, "daemon.log"))?;
    ignore_panel_signals();

    let me = Holder::current()?;
    let lock_path = state::file(&dir, id, "lock");
    let Some(lock) = state::claim(&lock_path, me)? else {
        eprintln!("relayd: session {id} is already held; this daemon exits");
        return Ok(0);
    };

    let sock_path = state::socket(&dir, id);
    // A long state directory pushes the socket into a shared temp directory; it
    // must land in one of ours, 0700, before bind creates it under the umask.
    if let Some(parent) = sock_path.parent() {
        state::ensure_private_dir(parent)?;
    }
    if let Err(e) = std::fs::remove_file(&sock_path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Err(e).with_context(|| format!("clear {}", sock_path.display()));
    }
    // Listen before spawning: a listen failure must never orphan a claude.
    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("listen on {}", sock_path.display()))?;
    std::fs::set_permissions(&sock_path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .with_context(|| format!("tighten {} to 0600", sock_path.display()))?;

    // Opened before the spawn for the same reason the listener is: after this
    // point an error would exit the daemon, closing claude's stdin and leaving it
    // orphaned, unreaped, and its lock removed by the guard on the way out.
    let stderr_log = state::append_log(&state::file(&dir, id, "stderr.log"))?;
    let spawn_record = state::file(&dir, id, "spawn.json");
    let (mut child, cwd) = spawn_claude(bin, args, &spawn_record)?;
    let claude_pid = child.id();
    let started_ms = now_ms();

    let (ctl_tx, ctl_rx) = std::sync::mpsc::channel();
    let mode = crate::flag_value(args, "--permission-mode").map(str::to_string);
    let ctx = Arc::new(Ctx::new(
        format!("{:x}-{}", started_ms, std::process::id()),
        ctl_tx.clone(),
        child.stdin.take(),
        mode,
    ));

    if let Some(out) = child.stdout.take() {
        let c = Arc::clone(&ctx);
        let events = state::file(&dir, id, "events.jsonl");
        std::thread::spawn(move || pump_stdout(c, out, events));
    }
    if let Some(err) = child.stderr.take() {
        let c = Arc::clone(&ctx);
        std::thread::spawn(move || pump_stderr(c, err, stderr_log));
    }
    {
        let c = Arc::clone(&ctx);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(s) => {
                        let c = Arc::clone(&c);
                        std::thread::spawn(move || {
                            if let Err(e) = serve_client(c, s) {
                                eprintln!("relayd: failed to {e:#}");
                            }
                        });
                    }
                    Err(e) => {
                        eprintln!("relayd: failed to accept a client: {e}");
                        return;
                    }
                }
            }
        });
    }
    {
        let tx = ctl_tx;
        let c = Arc::clone(&ctx);
        std::thread::spawn(move || {
            let status = child.wait();
            // Before anything else: the pid is free the instant wait returns.
            c.reaped.store(true, Ordering::SeqCst);
            let (code, signal) = match status {
                Ok(s) => (s.code(), std::os::unix::process::ExitStatusExt::signal(&s)),
                Err(e) => {
                    eprintln!("relayd: failed to reap claude: {e}");
                    (None, None)
                }
            };
            let _ = tx.send(Ctl::Exited(code, signal));
        });
    }

    let status_path = state::file(&dir, id, "status.json");
    let ended = supervise(
        &ctx,
        &ctl_rx,
        idle,
        &Status {
            path: &status_path,
            epoch: &ctx.epoch,
            cwd: &cwd,
            claude_pid: claude_pid.cast_signed(),
            started_ms,
        },
    );
    cleanup(&dir, id, &sock_path, lock, matches!(ended, Ok(true)));
    ended.map(|_| 0)
}

/// What `list` reads: written whenever it would say something new.
struct Status<'a> {
    path: &'a Path,
    epoch: &'a str,
    cwd: &'a str,
    claude_pid: i32,
    started_ms: u128,
}

impl Status<'_> {
    fn write(&self, clients: usize, seq: u64, applied: Option<&Applied>) {
        let status = json!({
            "epoch": self.epoch,
            "claude_pid": self.claude_pid,
            "started_ms": self.started_ms,
            "cwd": self.cwd,
            "clients": clients,
            "seq": seq,
            "model": applied.map(|a| a.model.as_str()),
            "effort": applied.map(|a| a.effort.as_str()),
            "ultracode": applied.map(|a| a.ultracode),
        });
        if let Err(e) = state::write_atomic(self.path, status.to_string().as_bytes()) {
            eprintln!("relayd: failed to {e:#}");
        }
    }
}

/// `Ok(true)` when the session ended the way it was meant to: claude exited
/// cleanly, or somebody asked for the end — a kill, a closed tab. An idle
/// reclamation does not count: the only explanation of where the session went
/// is in the log, so that one is kept.
fn supervise(
    ctx: &Arc<Ctx>,
    ctl: &Receiver<Ctl>,
    idle: Duration,
    status: &Status<'_>,
) -> Result<bool> {
    let mut tab_check: Option<Instant> = Some(Instant::now() + TAB_GRACE);
    let mut idle_at: Option<Instant> = (!idle.is_zero()).then(|| Instant::now() + idle);
    let mut kill_at: Option<Instant> = None;
    let mut asked_to_end = false;
    let mut last_status = Instant::now();
    let mut seen_seq = u64::MAX;
    let mut seen_clients = usize::MAX;
    loop {
        let now = Instant::now();
        let next = [tab_check, idle_at, kill_at]
            .into_iter()
            .flatten()
            .chain(std::iter::once(last_status + STATUS_INTERVAL))
            .filter_map(|d| d.checked_duration_since(now))
            .min()
            .unwrap_or(STATUS_INTERVAL);
        match ctl.recv_timeout(next) {
            Ok(Ctl::Attached) => {
                tab_check = None;
                idle_at = None;
            }
            // The client table is the only count; a second one kept here would
            // drift the moment a client left without announcing itself.
            Ok(Ctl::Detached) => {
                if ctx.shared().clients.is_empty() {
                    tab_check = Some(Instant::now() + TAB_GRACE);
                    idle_at = (!idle.is_zero()).then(|| Instant::now() + idle);
                }
            }
            Ok(Ctl::Kill) => {
                asked_to_end = true;
                signal_claude(ctx, status.claude_pid, libc::SIGTERM);
                kill_at = Some(Instant::now() + KILL_GRACE);
                tab_check = None;
                idle_at = None;
            }
            Ok(Ctl::Exited(code, signal)) => {
                deliver_exit(ctx, code, signal);
                return Ok(asked_to_end || code == Some(0));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            // claude's fate is unknown here, so the logs stay.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(false),
        }

        let now = Instant::now();
        if tab_check.is_some_and(|t| t <= now) {
            tab_check = None;
            let (parent, started) = {
                let s = ctx.shared();
                (s.last_parent, s.init.is_some())
            };
            // Parent alive means the user closed the tab. Parent gone, recycled
            // or unreadable means the host died: keep the conversation.
            let end = match parent {
                Some(p) if p.is_running() => {
                    eprintln!("relayd: the extension host is alive, so the tab was closed");
                    true
                }
                // Unless there is no conversation to keep. A session that never
                // emitted system/init is a panel that was opened and never used:
                // no history, no turn in flight, nothing a reopen could want.
                _ if !started => {
                    eprintln!("relayd: the panel is gone and this session never started");
                    true
                }
                Some(_) => {
                    eprintln!("relayd: the extension host is gone, keeping the session");
                    false
                }
                None => {
                    eprintln!(
                        "relayd: the extension host cannot be identified, keeping the session"
                    );
                    false
                }
            };
            if end {
                asked_to_end = true;
                signal_claude(ctx, status.claude_pid, libc::SIGTERM);
                kill_at = Some(now + KILL_GRACE);
                idle_at = None;
            }
        }
        if idle_at.is_some_and(|t| t <= now) {
            idle_at = None;
            eprintln!("relayd: idle with no client, ending the session");
            signal_claude(ctx, status.claude_pid, libc::SIGTERM);
            kill_at = Some(now + KILL_GRACE);
        }
        if kill_at.is_some_and(|t| t <= now) {
            kill_at = None;
            signal_claude(ctx, status.claude_pid, libc::SIGKILL);
        }
        if last_status + STATUS_INTERVAL <= now {
            last_status = now;
            let (n, seq, applied) = {
                let s = ctx.shared();
                (s.clients.len(), s.seq, s.applied.clone())
            };
            // Settings arrive as an event of their own, so they can never change
            // without seq moving.
            if seq != seen_seq || n != seen_clients {
                seen_clients = n;
                seen_seq = seq;
                status.write(n, seq, applied.as_ref());
            }
        }
    }
}

/// Removes what belongs to this run. The two logs go with it once the session
/// ended as intended; after anything else they are the only record of why, and
/// `logs <id>` still has them.
fn cleanup(dir: &Path, id: &str, sock: &Path, lock: Claim, ended_well: bool) {
    for ext in state::RUN_EXTS {
        state::remove_quietly(&state::file(dir, id, ext));
    }
    if ended_well {
        for ext in ["daemon.log", "stderr.log"] {
            state::remove_quietly(&state::file(dir, id, ext));
        }
    }
    state::remove_quietly(sock);
    drop(lock);
}
