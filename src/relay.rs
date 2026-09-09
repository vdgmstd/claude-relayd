//! Wrapper mode: what the VS Code extension actually runs.
//!
//! It keys the session, makes sure a daemon owns it, and relays bytes both ways.
//! Its exit code is the panel's only signal about claude, so a lost daemon must
//! never look like a finished conversation.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::protocol::{FromClient, FromDaemon, Hello, Replay, read_frame};
use crate::state::{self, Holder};

/// A socket that died without an exit envelope. Distinct on purpose: the panel
/// must be able to tell a lost daemon from a claude that finished.
const LOST_DAEMON: u8 = 70;
const ATTACH_TIMEOUT: Duration = Duration::from_secs(8);
const CURSOR_INTERVAL: Duration = Duration::from_millis(250);
/// The cursor is bounded by events as well as by time: a burst can deliver a
/// whole turn inside one interval, and a window close would then replay it.
const CURSOR_EVENTS: u32 = 32;

/// Panel message types this tool already understands. Anything else is written
/// to `<id>.panel.log` so a real window close can be inspected afterwards; the
/// line itself is still forwarded untouched.
const KNOWN_PANEL_TYPES: [&str; 4] = [
    "user",
    "control_response",
    "control_request",
    "control_cancel_request",
];

/// The session key, or `None` when this run cannot be keyed and must fall back
/// to passthrough.
enum Key {
    Session { id: String, args: Vec<String> },
    Unkeyable,
}

/// A conversation, as opposed to one of the short commands the extension also
/// routes through the wrapper (`claude mcp …`, `--claude-in-chrome-mcp`): those
/// have no session, reject an injected `--session-id`, and must reach the real
/// binary untouched.
fn is_conversation(args: &[String]) -> bool {
    crate::flag_value(args, "--input-format") == Some("stream-json")
        && crate::flag_value(args, "--output-format") == Some("stream-json")
}

/// The id is the value of `--resume`/`-r`/`--session-id`. With none, a UUID is
/// generated and injected so the panel resumes the same id on reopen and the
/// daemon has a stable key.
fn session_key(args: &[String]) -> Key {
    if !is_conversation(args) {
        return Key::Unkeyable;
    }
    // Whole-list first: the panel puts `--resume=<uuid>` in the middle of its
    // arguments, so a marker after it would otherwise never be seen, and the run
    // would be keyed onto a session id that is about to become a different one.
    if args.iter().any(|a| {
        matches!(
            a.as_str(),
            "--continue" | "-c" | "--from-pr" | "--teleport" | "--fork-session"
        ) || a.starts_with("--from-pr=")
            || a.starts_with("--teleport=")
    }) {
        return Key::Unkeyable;
    }
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        for name in ["--resume", "--session-id"] {
            if let Some(v) = a.strip_prefix(name).and_then(|r| r.strip_prefix('=')) {
                return Key::Session {
                    id: v.to_string(),
                    args: args.to_vec(),
                };
            }
        }
        if matches!(a.as_str(), "--resume" | "-r" | "--session-id") {
            return match args.get(i + 1) {
                Some(v) if !v.starts_with('-') => Key::Session {
                    id: v.clone(),
                    args: args.to_vec(),
                },
                // `--resume` with no value is the interactive picker.
                _ => Key::Unkeyable,
            };
        }
        i += 1;
    }
    let id = uuid::Uuid::new_v4().to_string();
    let mut args = args.to_vec();
    args.push("--session-id".to_string());
    args.push(id.clone());
    Key::Session { id, args }
}

/// Stock behaviour, exactly: the real binary replaces this process, so stdio,
/// signals and the exit status are the binary's own.
fn passthrough(bin: &str, args: &[String]) -> Result<u8> {
    let e = Command::new(bin).args(args).exec();
    Err(e).with_context(|| format!("run {bin}"))
}

fn current_ppid() -> i32 {
    // SAFETY: getppid always succeeds.
    unsafe { libc::getppid() }
}

/// The extension host as it was when this process began. Starting the daemon and
/// attaching can take seconds, and a host that dies in that window would leave
/// whatever adopted this process standing in for it.
fn parent_at_startup() -> Option<Holder> {
    let ppid = current_ppid();
    if ppid <= 1 {
        return None;
    }
    state::proc_start(ppid).map(|start| Holder { pid: ppid, start })
}

pub(crate) fn run(
    bin: &str,
    args: &[String],
    passthrough_wanted: bool,
    idle: Duration,
) -> Result<u8> {
    let parent = parent_at_startup();
    if passthrough_wanted {
        return passthrough(bin, args);
    }
    let (id, args) = match session_key(args) {
        Key::Session { id, args } => (id, args),
        Key::Unkeyable => return passthrough(bin, args),
    };
    let dir = match state::ensure_home() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("claude-relayd: failed to {e:#}, falling back to passthrough");
            return passthrough(bin, &args);
        }
    };
    let Some(sock) = connect(&dir, &id, bin, &args, idle)? else {
        return passthrough(bin, &args);
    };
    relay(&dir, &id, sock, &args, parent)
}

/// Connects to the session's daemon, starting one if nobody answers. `Ok(None)`
/// means no daemon could be reached and none holds the lock, so passthrough is
/// safe. While a live daemon does hold the lock, this fails instead: passthrough
/// there would start a second claude on the same session.
fn connect(
    dir: &Path,
    id: &str,
    bin: &str,
    args: &[String],
    idle: Duration,
) -> Result<Option<UnixStream>> {
    let path = state::socket(dir, id);
    let deadline = Instant::now() + ATTACH_TIMEOUT;
    let mut started = false;
    loop {
        if let Ok(s) = UnixStream::connect(&path) {
            return Ok(Some(s));
        }
        // A stale socket file is cleared by the daemon just before it binds.
        // Doing it here as well could only unlink the socket of a daemon that
        // came up between the failed connect and the check.
        if !started {
            started = true;
            start_daemon(dir, id, bin, args, idle)?;
        }
        if Instant::now() >= deadline {
            // No daemon of ours will read the record now, either way, and it
            // holds the panel's whole environment. Passthrough writes no state
            // at all; the refusal below leaves none either.
            state::remove_quietly(&state::file(dir, id, "spawn.json"));
            if let Some(h) = state::live_holder(&state::file(dir, id, "lock")) {
                bail!(
                    "attach to session {id}: daemon {} holds it but its socket is unreachable, so a second claude will not be started",
                    h.pid
                );
            }
            eprintln!("claude-relayd: the daemon did not come up, falling back to passthrough");
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn start_daemon(dir: &Path, id: &str, bin: &str, args: &[String], idle: Duration) -> Result<()> {
    // cwd and the full environment must reach the real claude intact. The file
    // holds every secret in the panel's environment, so it is 0600 and the
    // daemon removes it as soon as it has spawned.
    let env: serde_json::Map<String, Value> =
        std::env::vars().map(|(k, v)| (k, Value::String(v))).collect();
    let cwd = std::env::current_dir().context("read the working directory")?;
    state::write_atomic(
        &state::file(dir, id, "spawn.json"),
        json!({"cwd": cwd, "env": env}).to_string().as_bytes(),
    )?;
    let me = std::env::current_exe().context("locate this binary")?;
    // The daemon forks itself into the background, so this child exits at once
    // and is reaped here: no zombie, no daemon tied to the panel's process group.
    // The resolved value, so the flag the panel could not pass still wins over
    // the daemon.s own environment fallback.
    let status = Command::new(me)
        .arg("--idle-timeout")
        .arg(format!("{}", idle.as_secs_f64() / 60.0))
        .arg("daemon")
        .arg(id)
        .arg(bin)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("start the session daemon")?;
    if !status.success() {
        bail!("start the session daemon for {id}: it exited with {status}");
    }
    Ok(())
}

/// The wrapper's half of the cursor: only its owner may move it, and only once
/// the bytes have actually been written to stdout.
struct Cursor {
    path: std::path::PathBuf,
    epoch: String,
    seq: Option<u64>,
    dirty: bool,
    pending: u32,
    last: Instant,
    owner: Option<state::Claim>,
}

impl Cursor {
    fn open(dir: &Path, id: &str, me: Holder) -> Result<Cursor> {
        let owner = state::claim(&state::file(dir, id, "cursor.owner"), me)?;
        let path = state::file(dir, id, "cursor");
        let mut c = Cursor {
            path,
            epoch: String::new(),
            seq: None,
            dirty: false,
            pending: 0,
            last: Instant::now(),
            owner,
        };
        // A second concurrent panel gets init only and never advances the cursor.
        if c.owner.is_some()
            && let Some(v) = state::read_json(&c.path)
        {
            c.seq = v.get("seq").and_then(Value::as_u64);
            c.epoch = v.get("epoch").and_then(Value::as_str).unwrap_or_default().to_string();
        }
        Ok(c)
    }

    fn owns(&self) -> bool {
        self.owner.is_some()
    }

    fn advance(&mut self, seq: u64, epoch: &str) {
        if !self.owns() {
            return;
        }
        if self.epoch != epoch {
            // A new daemon run restarts at seq 1. Keeping the old high-water mark
            // would leave the file unwritten for the whole run, and the next
            // reattach would replay init alone and lose everything in between.
            self.epoch = epoch.to_string();
            self.seq = None;
        }
        if self.seq.is_none_or(|s| seq > s) {
            self.seq = Some(seq);
            self.dirty = true;
            self.pending += 1;
        }
    }

    fn flush(&mut self, force: bool) {
        if !self.dirty || !self.owns() {
            return;
        }
        if !force && self.pending < CURSOR_EVENTS && self.last.elapsed() < CURSOR_INTERVAL {
            return;
        }
        self.last = Instant::now();
        self.dirty = false;
        self.pending = 0;
        let v = json!({"epoch": self.epoch, "seq": self.seq});
        if let Err(e) = state::write_atomic(&self.path, v.to_string().as_bytes()) {
            eprintln!("claude-relayd: failed to {e:#}");
        }
    }

    /// claude is gone: the cursor has nothing left to point at, and rewriting it
    /// would only resurrect the file the daemon just removed.
    fn discard(&mut self) {
        self.dirty = false;
        if self.owns() {
            state::remove_quietly(&self.path);
        }
    }
}

fn relay(
    dir: &Path,
    id: &str,
    sock: UnixStream,
    args: &[String],
    parent: Option<Holder>,
) -> Result<u8> {
    let me = Holder::current()?;
    let mut cursor = Cursor::open(dir, id, me)?;
    // Reparented since startup means the recorded host is gone. Whatever adopted
    // this process, init or a subreaper, is not the panel's host and must never
    // speak for it: unidentifiable keeps the session, a live stranger ends it.
    let parent = parent.filter(|p| current_ppid() == p.pid);
    let hello = Hello {
        since: cursor.seq,
        epoch: (!cursor.epoch.is_empty()).then(|| cursor.epoch.clone()),
        replay: if cursor.owns() {
            Replay::InitAndSince
        } else {
            Replay::Init
        },
        parent,
        // What the panel asked for on this spawn. The daemon cannot act on a
        // mismatch without guessing at claude's control protocol, but it can say so.
        permission_mode: crate::flag_value(args, "--permission-mode").map(str::to_string),
    };

    let mut to_daemon = sock.try_clone().context("clone the daemon socket")?;
    to_daemon
        .write_all(&hello.frame())
        .context("greet the session daemon")?;

    // Set once this side says goodbye, so the socket closing afterwards is not
    // mistaken for a daemon that vanished.
    let leaving = Arc::new(AtomicBool::new(false));
    let said_goodbye = Arc::clone(&leaving);
    let panel_log = state::file(dir, id, "panel.log");
    std::thread::spawn(move || pump_panel(to_daemon, &panel_log, &said_goodbye));

    let mut out = std::io::stdout();
    let mut reader = BufReader::new(sock);
    loop {
        let frame = match read_frame(&mut reader) {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                eprintln!("claude-relayd: failed to {e:#}");
                break;
            }
        };
        match FromDaemon::of(frame)? {
            FromDaemon::Event { seq, epoch, body } => {
                // The cursor may only advance once the bytes have left this
                // process: a kill with a full pipe must not claim delivery.
                match out.write_all(&body).and_then(|()| out.flush()) {
                    Ok(()) => cursor.advance(seq, &epoch),
                    // The panel closed its read end; that is not a claude crash.
                    Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                        cursor.flush(true);
                        return Ok(0);
                    }
                    Err(e) => {
                        cursor.flush(true);
                        return Err(e).context("write to the panel");
                    }
                }
                cursor.flush(false);
            }
            FromDaemon::Stderr(body) => {
                if let Err(e) = std::io::stderr().write_all(&body) {
                    eprintln!("claude-relayd: failed to write stderr: {e}");
                }
            }
            FromDaemon::Truncated(n) => {
                eprintln!("claude-relayd: the daemon dropped {n} event(s) before this attach");
            }
            FromDaemon::Exit { code, signal } => {
                cursor.discard();
                return Ok(match (code, signal) {
                    (Some(c), _) => u8::try_from(c).unwrap_or(1),
                    (None, Some(s)) => u8::try_from(128 + s).unwrap_or(LOST_DAEMON),
                    (None, None) => LOST_DAEMON,
                });
            }
        }
    }
    cursor.flush(true);
    if leaving.load(Ordering::SeqCst) {
        return Ok(0);
    }
    eprintln!("claude-relayd: lost the connection to the session daemon for {id}");
    Ok(LOST_DAEMON)
}

/// Panel to daemon. Lines are forwarded verbatim; the only inspection is to note
/// an unfamiliar message type in the panel log.
fn pump_panel(mut to_daemon: UnixStream, panel_log: &Path, leaving: &AtomicBool) {
    let mut stdin = BufReader::new(std::io::stdin());
    let mut line = Vec::new();
    loop {
        line.clear();
        match stdin.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("claude-relayd: failed to read the panel: {e}");
                break;
            }
        }
        note_unknown(&line, panel_log);
        if to_daemon.write_all(&FromClient::payload(&line)).is_err() {
            return;
        }
    }
    // The panel's stdin ended: say goodbye, but never touch claude.
    leaving.store(true, Ordering::SeqCst);
    if to_daemon.write_all(&FromClient::detach()).is_err() {
        return;
    }
    if let Err(e) = to_daemon.shutdown(std::net::Shutdown::Write) {
        eprintln!("claude-relayd: failed to close the daemon socket: {e}");
    }
}

fn note_unknown(line: &[u8], panel_log: &Path) {
    let Ok(v) = serde_json::from_slice::<Value>(line) else {
        return;
    };
    let t = v.get("type").and_then(Value::as_str).unwrap_or("");
    if KNOWN_PANEL_TYPES.contains(&t) {
        return;
    }
    match state::append_log(panel_log) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(line) {
                eprintln!("claude-relayd: failed to write {}: {e}", panel_log.display());
            }
        }
        Err(e) => eprintln!("claude-relayd: failed to {e:#}"),
    }
}
