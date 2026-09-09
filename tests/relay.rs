//! Every scenario in `.claude/rules/testing.md`, driving the real binary with a
//! fake claude. Nothing here touches a real state directory, binary or session.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_claude-relayd");
const FAKE: &str = env!("CARGO_BIN_EXE_fake-claude");
/// The wiring the panel always passes. Without it a run is a one-shot claude
/// command, not a conversation, and the wrapper stays out of its way.
const STREAM: [&str; 4] = ["--input-format", "stream-json", "--output-format", "stream-json"];
const PATIENCE: Duration = Duration::from_secs(30);
/// The daemon's own tab-close grace, plus room for the SIGTERM that follows.
const AFTER_GRACE: Duration = Duration::from_secs(16);
const UFFFD: [u8; 3] = [0xef, 0xbf, 0xbd];

static SEQ: AtomicU32 = AtomicU32::new(0);

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only probes for the process.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn slay(pid: i32) {
    // SAFETY: SIGKILL to a pid this test spawned.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
}

/// A named pipe. Opening one for writing blocks until somebody opens the read
/// end, which is what the race test below uses to hold a daemon in place.
fn fifo(path: &Path) {
    let raw = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: a path this test owns, in its own temp directory.
    let made = unsafe { libc::mkfifo(raw.as_ptr(), 0o600) };
    assert_eq!(made, 0, "mkfifo {}: {}", path.display(), errno());
}

fn errno() -> std::io::Error {
    std::io::Error::last_os_error()
}

/// How many daemons for this session are running, read from /proc. `daemon <id>`
/// is the wrapper's own spawn form, so nothing else matches it.
fn daemons(id: &str) -> usize {
    let needle = format!("daemon\0{id}\0");
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| {
            let cmdline = std::fs::read(e.path().join("cmdline")).unwrap_or_default();
            String::from_utf8_lossy(&cmdline).contains(&needle)
        })
        .count()
}

fn slurp(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_default()
}

fn size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

/// The cursor number, compared as a number: `"seq":2` is a prefix of
/// `"seq":2604`, and matching it as text made a test race the stream.
/// How many events the daemon has recorded, as `list` reports it.
fn daemon_seq(fix: &Fix) -> u64 {
    let out = fix.cmd(&["list"]);
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|l| l.starts_with(&fix.id))
        .and_then(|l| l.split('\t').nth(5).and_then(|s| s.parse().ok()))
        .unwrap_or(0)
}

fn cursor_seq(path: &Path) -> u64 {
    serde_json::from_slice::<serde_json::Value>(&slurp(path))
        .ok()
        .and_then(|v| v.get("seq").and_then(serde_json::Value::as_u64))
        .unwrap_or(0)
}

fn text(path: &Path) -> String {
    String::from_utf8_lossy(&slurp(path)).into_owned()
}

/// Polls for the thing itself rather than sleeping, and on timeout says what it
/// actually saw.
fn until(what: &str, diag: impl Fn() -> String, mut done: impl FnMut() -> bool) {
    until_for(PATIENCE, what, diag, &mut done);
}

fn until_for(
    limit: Duration,
    what: &str,
    diag: impl Fn() -> String,
    done: &mut impl FnMut() -> bool,
) {
    let deadline = Instant::now() + limit;
    loop {
        if done() {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}\n{}", diag());
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// One test's private world: its own state directory and every process it
/// started, all torn down even when the test panics.
struct Fix {
    root: PathBuf,
    home: PathBuf,
    id: String,
    kids: Vec<Child>,
    groups: Vec<i32>,
    strays: Vec<i32>,
}

impl Fix {
    fn new(name: &str) -> Fix {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("claude-relayd-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let home = root.join("home");
        Fix {
            root,
            home,
            id: format!("test-{name}-{}-{n}", std::process::id()),
            kids: Vec::new(),
            groups: Vec::new(),
            strays: Vec::new(),
        }
    }

    fn state(&self, ext: &str) -> PathBuf {
        self.home.join(format!("{}.{ext}", self.id))
    }

    fn daemon_pid(&self) -> Option<i32> {
        text(&self.state("lock")).split_whitespace().next()?.parse().ok()
    }

    /// Runs one subcommand of the tool under test.
    fn cmd(&self, args: &[&str]) -> std::process::Output {
        Command::new(BIN)
            .args(args)
            .env("CLAUDE_RELAYD_HOME", &self.home)
            .output()
            .unwrap()
    }

    fn panel(&mut self) -> Panel {
        self.panel_with(&[], &[])
    }

    fn panel_with(&mut self, env: &[(&str, &str)], flags: &[&str]) -> Panel {
        self.panel_full(env, flags, &[])
    }

    fn panel_full(&mut self, env: &[(&str, &str)], flags: &[&str], claude_args: &[&str]) -> Panel {
        let n = self.kids.len();
        let out = self.root.join(format!("out{n}"));
        let err = self.root.join(format!("err{n}"));
        let mut cmd = Command::new(BIN);
        cmd.args(flags)
            .arg(FAKE)
            .args(STREAM)
            .arg(format!("--resume={}", self.id))
            .args(claude_args)
            .env("CLAUDE_RELAYD_HOME", &self.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::from(std::fs::File::create(&out).unwrap()))
            .stderr(Stdio::from(std::fs::File::create(&err).unwrap()));
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().unwrap();
        let stdin = child.stdin.take();
        let pid = child.id().cast_signed();
        self.kids.push(child);
        Panel {
            pid,
            stdin,
            out,
            err,
            index: n,
        }
    }

    fn child(&mut self, index: usize) -> &mut Child {
        &mut self.kids[index]
    }

    /// A claude to kill on the way out that the tool itself may not name. A
    /// duplicate is invisible to `list` and to the lock, and it is exactly what
    /// the one-claude-per-id test is looking for.
    fn stray(&mut self, pid: i32) {
        self.strays.push(pid);
    }

    /// A stub extension host: a shell that owns the panel and outlives it, so a
    /// test controls whether the parent is alive when the panel goes away.
    fn hosted_panel(&mut self, host_exits: bool) -> (i32, i32, usize) {
        self.hosted_panel_full(host_exits, "", "\"subtype\":\"init\"")
    }

    /// `marker` is what proves claude is up: a session told not to emit init has
    /// to be waited for on something else.
    fn hosted_panel_full(
        &mut self,
        host_exits: bool,
        claude_args: &str,
        marker: &str,
    ) -> (i32, i32, usize) {
        let out = self.root.join("hosted-out");
        let err = self.root.join("hosted-err");
        let wpid = self.root.join("wpid");
        let tail = if host_exits { "exit 0" } else { "wait" };
        let script = format!(
            "sleep 300 | {BIN} {FAKE} {stream} --resume={id} {claude_args} > {out} 2> {err} & echo $! > {wpid}; {tail}",
            stream = STREAM.join(" "),
            id = self.id,
            out = out.display(),
            err = err.display(),
            wpid = wpid.display(),
        );
        let child = Command::new("sh")
            .arg("-c")
            .arg(script)
            .env("CLAUDE_RELAYD_HOME", &self.home)
            .process_group(0)
            .spawn()
            .unwrap();
        let host = child.id().cast_signed();
        let index = self.kids.len();
        self.groups.push(host);
        self.kids.push(child);
        until(
            "the hosted panel to report its pid",
            || format!("wpid file: {:?}", text(&wpid)),
            || text(&wpid).trim().parse::<i32>().is_ok(),
        );
        let panel = text(&wpid).trim().parse().unwrap();
        until(
            "the hosted panel to reach claude",
            || format!("out: {}", text(&out)),
            || text(&out).contains(marker),
        );
        (host, panel, index)
    }
}

/// The claude pid out of the init line a panel received. It reads the file and
/// nothing of the fixture, so it is not a method on it.
fn claude_pid(out: &Path) -> i32 {
    let s = text(out);
    let at = s.find("\"pid\":").expect("no init line with a pid");
    s[at + 6..]
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

impl Drop for Fix {
    fn drop(&mut self) {
        // The daemon is deliberately not our child, so it is hunted down by the
        // state it wrote. A leaked daemon would hold a lock and idle for hours.
        if let Some(pid) = self.daemon_pid() {
            slay(pid);
        }
        if let Some(v) = std::fs::read(self.state("status.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            && let Some(pid) = v.get("claude_pid").and_then(serde_json::Value::as_u64)
        {
            slay(pid as i32);
        }
        for pid in &self.strays {
            slay(*pid);
        }
        for g in &self.groups {
            // SAFETY: the group this test created.
            unsafe {
                libc::kill(-g, libc::SIGKILL);
            }
        }
        for kid in &mut self.kids {
            let _ = kid.kill();
            let _ = kid.wait();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct Panel {
    pid: i32,
    stdin: Option<ChildStdin>,
    out: PathBuf,
    err: PathBuf,
    index: usize,
}

impl Panel {
    fn send(&mut self, line: &str) {
        let s = self.stdin.as_mut().unwrap();
        s.write_all(line.as_bytes()).unwrap();
        s.write_all(b"\n").unwrap();
        s.flush().unwrap();
    }

    /// One byte at a time, so a multi-byte character is guaranteed to straddle
    /// two reads on the way in.
    fn dribble(&mut self, line: &str) {
        let s = self.stdin.as_mut().unwrap();
        let mut bytes = line.as_bytes().to_vec();
        bytes.push(b'\n');
        for b in bytes {
            s.write_all(&[b]).unwrap();
            s.flush().unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn close_stdin(&mut self) {
        self.stdin.take();
    }

    fn out(&self) -> String {
        text(&self.out)
    }

    fn err(&self) -> String {
        text(&self.err)
    }

    fn saw(&self, needle: &str) -> bool {
        self.out().contains(needle)
    }

    fn count(&self, needle: &str) -> usize {
        self.out().matches(needle).count()
    }

    fn wait_for_init(&self) {
        until(
            "the panel to receive system/init",
            || format!("out: {}\nerr: {}", self.out(), self.err()),
            || self.saw("\"subtype\":\"init\""),
        );
    }

    /// Window close: the panel process is killed outright.
    fn window_close(&mut self, fix: &mut Fix) {
        slay(self.pid);
        let _ = fix.child(self.index).wait();
        self.stdin.take();
    }
}

fn code_of(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

// --- the scenarios ---------------------------------------------------------

#[test]
fn claude_survives_a_window_close() {
    let mut fix = Fix::new("survive");
    let mut a = fix.panel();
    a.wait_for_init();
    a.send(r#"{"type":"user","m":"one"}"#);
    until("the first echo", || a.out(), || a.saw("\"echo\""));
    let claude = claude_pid(&a.out);
    let events = fix.state("events.jsonl");
    let before = slurp(&events).len();

    a.window_close(&mut fix);

    until(
        "claude to keep producing events with no panel attached",
        || format!("events: {} bytes", slurp(&events).len()),
        || slurp(&events).len() > before,
    );
    assert!(alive(claude), "claude {claude} died with the panel");
}

#[test]
fn reattach_replays_init_then_only_new_events() {
    let mut fix = Fix::new("reattach");
    let mut a = fix.panel();
    a.wait_for_init();
    a.send(r#"{"type":"user","m":"one"}"#);
    until("the first echo", || a.out(), || a.saw("\"echo\""));
    let claude = claude_pid(&a.out);
    // A backlog that starts only after this panel is gone, so the reattach has
    // something it must be given, not merely something it must not repeat.
    a.send(r#"{"type":"burst","n":40,"after_ms":1500}"#);
    until("the backlog to be scheduled", || a.out(), || a.saw("burst_scheduled"));
    // The cursor only moves once the bytes are out; wait for it rather than
    // racing its debounce.
    let cursor = fix.state("cursor");
    until(
        "the cursor to record what this panel saw",
        || format!("cursor seq: {}", cursor_seq(&cursor)),
        || cursor_seq(&cursor) >= 3,
    );
    a.window_close(&mut fix);
    // The backlog must be behind the daemon before the next panel attaches, or it
    // would arrive live and the replay would never be exercised at all.
    until(
        "the backlog to be produced with no panel attached",
        || format!("daemon seq: {}", daemon_seq(&fix)),
        || daemon_seq(&fix) >= 43,
    );

    let b = fix.panel();
    b.wait_for_init();
    until(
        "the backlog produced while no panel was attached",
        || format!("burst lines: {}", b.count("\"burst\":")),
        || b.count("\"burst\":") >= 40,
    );
    until(
        "live events after the reattach",
        || b.out(),
        || b.count("heartbeat") > 0,
    );
    assert_eq!(b.count("\"burst\":"), 40, "the backlog was replayed short or twice");
    assert_eq!(b.count("\"subtype\":\"init\""), 1, "init replayed more than once");
    assert!(b.out().starts_with("{\"args\""), "init was not first: {}", b.out());
    assert_eq!(b.count("\"echo\""), 0, "an acknowledged event was replayed");
    assert_eq!(claude_pid(&b.out), claude, "a second claude was started");
}

#[test]
fn a_second_panel_shares_one_claude_and_never_moves_the_cursor() {
    let mut fix = Fix::new("two");
    let mut a = fix.panel();
    a.wait_for_init();
    a.send(r#"{"type":"user","m":"one"}"#);
    until("the first echo", || a.out(), || a.saw("\"echo\""));
    let claude = claude_pid(&a.out);

    let mut b = fix.panel();
    b.wait_for_init();
    assert_eq!(claude_pid(&b.out), claude, "the second panel started its own claude");
    assert_eq!(b.count("\"echo\""), 0, "the second panel was served a replay");

    let (ha, hb) = (a.count("heartbeat"), b.count("heartbeat"));
    until(
        "both panels to receive the live stream",
        || format!("a: {}, b: {}", a.count("heartbeat"), b.count("heartbeat")),
        || a.count("heartbeat") > ha && b.count("heartbeat") > hb,
    );
    let owner = text(&fix.state("cursor.owner"));
    assert!(
        owner.starts_with(&format!("{} ", a.pid)),
        "cursor owner is {owner:?}, expected the first panel {}",
        a.pid
    );

    // With the owner gone the cursor freezes, even though the second panel keeps
    // reading. A cursor moved by a panel that does not own it would skip events
    // the owner never saw.
    let cursor = fix.state("cursor");
    a.window_close(&mut fix);
    let frozen = cursor_seq(&cursor);
    b.send(r#"{"type":"burst","n":40,"after_ms":0}"#);
    until(
        "the stream to run well past the frozen cursor",
        || format!("frozen at {frozen}, daemon at {}", daemon_seq(&fix)),
        || daemon_seq(&fix) > frozen + 40,
    );
    until(
        "the surviving panel to receive the backlog",
        || format!("burst lines: {}", b.count("\"burst\":")),
        || b.count("\"burst\":") >= 40,
    );
    assert_eq!(
        cursor_seq(&cursor),
        frozen,
        "the second panel moved a cursor it does not own"
    );

    // And that is what the freeze buys: a later panel is given everything the
    // owner never acknowledged, however much the second panel had already read.
    b.window_close(&mut fix);
    let c = fix.panel();
    c.wait_for_init();
    until(
        "the third panel to be given what the owner never acknowledged",
        || format!("burst lines: {}", c.count("\"burst\":")),
        || c.count("\"burst\":") >= 40,
    );
}

#[test]
fn a_closed_tab_ends_the_session() {
    let mut fix = Fix::new("tab");
    let (host, panel, _idx) = fix.hosted_panel(false);
    let claude = claude_pid(&fix.root.join("hosted-out"));

    slay(panel);
    assert!(alive(host), "the stub extension host died with the tab");

    until_for(
        AFTER_GRACE,
        "the session to end after the tab closed",
        || format!("claude {claude} alive: {}", alive(claude)),
        &mut || !alive(claude),
    );
}

/// The counterpart of the test below: a dead host keeps a conversation, but a
/// session that never became one has nothing worth keeping.
#[test]
fn a_session_that_never_started_dies_with_its_window() {
    let mut fix = Fix::new("unstarted");
    let (host, _panel, idx) = fix.hosted_panel_full(false, "--no-init", "\"type\":\"heartbeat\"");
    let daemon = fix.daemon_pid().expect("no lock file");

    // SAFETY: the group this test created.
    unsafe {
        libc::kill(-host, libc::SIGKILL);
    }
    let _ = fix.child(idx).wait();
    until("the stub host to die", String::new, || !alive(host));

    until_for(
        AFTER_GRACE,
        "the daemon to end a session that never started",
        || format!("daemon {daemon} still alive: {}", alive(daemon)),
        &mut || !alive(daemon),
    );
    assert!(
        !fix.state("lock").exists(),
        "the unstarted session was kept: its lock is still there"
    );
    // It ended as intended, so it leaves nothing to explain.
    assert!(!fix.state("daemon.log").exists());
}

#[test]
fn a_dead_extension_host_keeps_the_session() {
    let mut fix = Fix::new("host");
    let (host, _panel, idx) = fix.hosted_panel(false);
    let claude = claude_pid(&fix.root.join("hosted-out"));

    // SAFETY: the group this test created.
    unsafe {
        libc::kill(-host, libc::SIGKILL);
    }
    // Reap it first: a zombie still answers signal 0.
    let _ = fix.child(idx).wait();
    until("the stub host to die", String::new, || !alive(host));

    // Killing the group took the panel with the host, so the disconnect really
    // happens and the daemon really consults the parent it was given.
    let logs = || String::from_utf8_lossy(&fix.cmd(&["logs", &fix.id]).stdout).into_owned();
    until_for(
        AFTER_GRACE,
        "the daemon to decide what the dead host means",
        || format!("daemon log: {}", logs()),
        &mut || logs().contains("keeping the session"),
    );
    assert!(alive(claude), "claude {claude} was killed by a window close");
}

#[test]
fn an_unresolvable_parent_keeps_the_session() {
    let mut fix = Fix::new("orphan");
    // The host exits at once, so the panel is reparented and its parent can no
    // longer be identified.
    let (_host, panel, _idx) = fix.hosted_panel(true);
    let claude = claude_pid(&fix.root.join("hosted-out"));

    // Without a disconnect the daemon never consults the parent at all, and this
    // test would watch an attached session that no policy could ever end.
    slay(panel);
    let logs = || String::from_utf8_lossy(&fix.cmd(&["logs", &fix.id]).stdout).into_owned();
    until_for(
        AFTER_GRACE,
        "the daemon to decide what the missing parent means",
        || format!("daemon log: {}", logs()),
        &mut || logs().contains("keeping the session"),
    );
    assert!(alive(claude), "claude {claude} was killed on an unresolvable parent");
}

#[test]
fn the_idle_timeout_only_ends_an_unattached_session() {
    let mut fix = Fix::new("idle");
    // The environment says never, the flag says 1.2 seconds: the flag wins, and
    // reaching the daemon at all proves the wrapper forwards it.
    let never = [("CLAUDE_RELAYD_IDLE_TIMEOUT", "0")];
    let mut a = fix.panel_with(&never, &["--idle-timeout", "0.02"]);
    a.wait_for_init();
    let claude = claude_pid(&a.out);

    // 1.2 seconds of idle, and an attached panel resets it every time.
    std::thread::sleep(Duration::from_secs(4));
    assert!(alive(claude), "an attached session was ended by the idle timeout");

    a.window_close(&mut fix);
    until_for(
        // Well under the ten second tab-close grace, which would otherwise end the
        // session on its own and let this pass with no idle timeout at all.
        Duration::from_secs(5),
        "the idle session to end",
        || format!("claude {claude} alive: {}", alive(claude)),
        &mut || !alive(claude),
    );
    let logs = String::from_utf8_lossy(&fix.cmd(&["logs", &fix.id]).stdout).into_owned();
    assert!(
        logs.contains("idle with no client"),
        "the session ended, but not through the idle timeout: {logs}"
    );
}

#[test]
fn kill_ends_claude_the_daemon_and_the_run_state() {
    let mut fix = Fix::new("kill");
    let a = fix.panel();
    a.wait_for_init();
    let claude = claude_pid(&a.out);
    let daemon = fix.daemon_pid().expect("no lock file");

    let out = fix.cmd(&["kill", &fix.id]);
    assert!(out.status.success(), "kill failed: {out:?}");

    until("claude to be gone", String::new, || !alive(claude));
    until("the daemon to be gone", String::new, || !alive(daemon));
    // The panel exits first, because it owns the cursor claim and removes it
    // itself. A daemon that deleted a live wrapper's claim would let a second
    // wrapper take the cursor out from under it.
    let status = fix.child(a.index).wait().unwrap();
    assert_eq!(code_of(status), 143, "the panel did not report the signal");
    for ext in [
        "lock",
        "sock",
        "events.jsonl",
        "cursor",
        "cursor.owner",
        "status.json",
    ] {
        assert!(!fix.state(ext).exists(), "{ext} was left behind");
    }
    // The session ended the way it was asked to, so it leaves nothing behind.
    for ext in ["daemon.log", "stderr.log"] {
        assert!(!fix.state(ext).exists(), "{ext} outlived the session");
    }
    assert!(!fix.cmd(&["logs", &fix.id]).status.success());
    assert!(!a.saw("\"exit\""), "an exit envelope leaked to the panel");
}

#[test]
fn kill_all_ends_every_live_session() {
    let mut fix = Fix::new("killall");
    let a = fix.panel();
    a.wait_for_init();
    let claude = claude_pid(&a.out);
    // A second session in the same state directory. Its idle timeout is short so
    // a failing assertion cannot leave a daemon behind for hours.
    let other = format!("{}-b", fix.id);
    let out_b = fix.root.join("out-b");
    let mut second = Command::new(BIN)
        .args(["--idle-timeout", "0.05"])
        .arg(FAKE)
        .args(STREAM)
        .arg(format!("--resume={other}"))
        .env("CLAUDE_RELAYD_HOME", &fix.home)
        .stdin(Stdio::piped())
        .stdout(Stdio::from(std::fs::File::create(&out_b).unwrap()))
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    until(
        "the second panel to reach claude",
        || text(&out_b),
        || text(&out_b).contains("\"subtype\":\"init\""),
    );

    let out = fix.cmd(&["kill", "--all"]);
    assert!(out.status.success(), "kill --all failed: {out:?}");
    let ended = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(ended.contains(&fix.id) && ended.contains(&other), "{ended}");

    until("claude to be gone", String::new, || !alive(claude));
    let listed = String::from_utf8_lossy(&fix.cmd(&["list"]).stdout).into_owned();
    assert!(!listed.contains(&fix.id), "a session survived kill --all: {listed}");
    let _ = second.wait();
    let status = fix.child(a.index).wait().unwrap();
    assert_eq!(code_of(status), 143, "the panel did not report the signal");
}

#[test]
fn the_short_id_list_prints_is_enough_for_logs_and_kill() {
    let fix = Fix::new("shortid");
    // Shaped like the panel's session id, so `list` abbreviates it the same way.
    let id = format!("aaaaaaaa-bbbb-cccc-dddd-{:012x}", std::process::id());
    let tail = id.rsplit('-').next().unwrap().to_string();
    let out = fix.root.join("short-out");
    let mut panel = Command::new(BIN)
        .args(["--idle-timeout", "0.05"])
        .arg(FAKE)
        .args(STREAM)
        .arg(format!("--resume={id}"))
        .env("CLAUDE_RELAYD_HOME", &fix.home)
        .stdin(Stdio::piped())
        .stdout(Stdio::from(std::fs::File::create(&out).unwrap()))
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    until(
        "the panel to reach claude",
        || text(&out),
        || text(&out).contains("\"subtype\":\"init\""),
    );

    let listed = String::from_utf8_lossy(&fix.cmd(&["list"]).stdout).into_owned();
    assert!(listed.contains(&tail), "list did not show the tail: {listed}");
    assert!(
        !listed.contains(&id),
        "list printed the whole id, not the tail: {listed}"
    );
    // Both commands have to take back what list printed, or the short form is a
    // trap rather than a convenience.
    assert!(fix.cmd(&["logs", &tail]).status.success());
    assert!(fix.cmd(&["kill", &tail]).status.success());
    let _ = panel.wait();
    assert!(
        !String::from_utf8_lossy(&fix.cmd(&["list"]).stdout).contains(&tail),
        "the session survived a kill by its tail"
    );
}

/// A holder still on the lock with no socket to answer on, which `kill --all`
/// must call a failure — as opposed to a session that ended by itself between
/// the listing and the connect, which is what the sweep asked for.
#[test]
fn kill_all_fails_loudly_on_a_session_it_cannot_reach() {
    let fix = Fix::new("unreachable");
    std::fs::create_dir_all(&fix.home).unwrap();
    // A live process this test owns stands in for the daemon; its start time is
    // what makes the lock a live claim rather than a stale one.
    let mut holder = Command::new("sleep").arg("300").spawn().unwrap();
    let pid = holder.id();
    let line = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let start = line
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .to_string();
    std::fs::write(fix.state("lock"), format!("{pid} {start}")).unwrap();

    let out = fix.cmd(&["kill", "--all"]);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !out.status.success(),
        "an unreachable session was reported as ended: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(err.contains("socket is unreachable"), "stderr was: {err}");
    let _ = holder.kill();
    let _ = holder.wait();
}

#[test]
fn help_says_how_to_use_the_tool_and_how_to_switch_it_off() {
    let fix = Fix::new("help");
    for form in [vec!["help"], vec!["--help"], vec!["-h"]] {
        let out = fix.cmd(&form);
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "{form:?} failed: {out:?}");
        // The escape hatch is the one thing somebody reads this for in a hurry.
        for needle in [
            "CLAUDE_RELAYD_PASSTHROUGH",
            "claudeCode.claudeProcessWrapper",
            "kill --all",
            "--idle-timeout",
        ] {
            assert!(text.contains(needle), "{form:?} does not mention {needle}");
        }
    }
    // With nothing to do it says the same thing, on stderr, and fails.
    let bare = fix.cmd(&[]);
    assert_eq!(code_of(bare.status), 2);
    assert!(
        String::from_utf8_lossy(&bare.stderr).contains("CLAUDE_RELAYD_PASSTHROUGH"),
        "an empty command line printed no guidance"
    );
}

#[test]
fn a_one_shot_claude_command_is_never_a_session() {
    let fix = Fix::new("oneshot");
    // What the extension runs for `claude mcp …` and its other short commands:
    // no stream-json wiring, and the real binary rejects an injected session id.
    let out = Command::new(BIN)
        .arg(FAKE)
        .args(["mcp", "list"])
        .env("CLAUDE_RELAYD_HOME", &fix.home)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(code_of(out.status), 3, "the run was not passed through");
    assert!(
        stdout.contains("\"subtype\":\"init\""),
        "passthrough lost claude's output"
    );
    assert!(
        !stdout.contains("--session-id"),
        "a one-shot command was given a session id: {stdout}"
    );
    assert!(!fix.home.exists(), "passthrough wrote a state directory");
}

#[test]
fn list_and_logs_report_a_session_and_refuse_an_unknown_id() {
    let mut fix = Fix::new("list");
    let mut a = fix.panel();
    a.wait_for_init();
    a.send(r#"{"type":"err"}"#);
    until(
        "claude stderr to reach the panel",
        || a.err(),
        || a.err().contains("DEBUG diagnostic line"),
    );

    let listed = || String::from_utf8_lossy(&fix.cmd(&["list"]).stdout).into_owned();
    assert!(
        listed().contains(&fix.id),
        "list did not show the session: {}",
        listed()
    );
    // The model and the thinking level as claude last reported them. The daemon
    // writes the status file on its own beat, so this is polled, not assumed.
    until("list to report model and thinking", listed, || {
        let l = listed();
        l.contains("fake-claude-model") && l.contains("high+ultra")
    });
    let logs = fix.cmd(&["logs", &fix.id]);
    assert!(
        String::from_utf8_lossy(&logs.stdout).contains("DEBUG diagnostic line"),
        "logs did not show claude stderr: {}",
        String::from_utf8_lossy(&logs.stdout)
    );

    assert!(!fix.cmd(&["logs", "no-such-id"]).status.success());
    assert!(!fix.cmd(&["kill", "no-such-id"]).status.success());

    fix.cmd(&["kill", &fix.id]);
    until(
        "list to forget the killed session",
        || String::from_utf8_lossy(&fix.cmd(&["list"]).stdout).into_owned(),
        || !String::from_utf8_lossy(&fix.cmd(&["list"]).stdout).contains(&fix.id),
    );
}

#[test]
fn the_panel_exit_code_follows_claude() {
    let mut fix = Fix::new("code");
    let mut a = fix.panel();
    a.wait_for_init();
    a.send(r#"{"type":"exit","code":7}"#);
    let status = fix.child(a.index).wait().unwrap();
    assert_eq!(code_of(status), 7, "stdout was: {}", a.out());
    // Nobody asked for this ending, so the only record of it is kept.
    for ext in ["daemon.log", "stderr.log"] {
        assert!(fix.state(ext).exists(), "{ext} was removed after a failure");
    }
}

#[test]
fn a_signalled_claude_reports_128_plus_the_signal() {
    let mut fix = Fix::new("signal");
    let a = fix.panel();
    a.wait_for_init();
    slay(claude_pid(&a.out));
    let status = fix.child(a.index).wait().unwrap();
    assert_eq!(code_of(status), 128 + 9, "stdout was: {}", a.out());
}

#[test]
fn a_lost_daemon_is_never_a_finished_conversation() {
    let mut fix = Fix::new("lost");
    let a = fix.panel();
    a.wait_for_init();
    slay(fix.daemon_pid().expect("no lock file"));
    let status = fix.child(a.index).wait().unwrap();
    assert_eq!(code_of(status), 70, "stdout was: {}", a.out());
    assert!(a.err().contains("lost the connection"), "stderr was: {}", a.err());
}

#[test]
fn passthrough_is_stock_behaviour() {
    let mut fix = Fix::new("passthrough");
    let on = [("CLAUDE_RELAYD_PASSTHROUGH", "1")];
    let mut a = fix.panel_with(&on, &[]);
    a.wait_for_init();
    // exec replaces this process, so the pid the panel talks to is the wrapper's
    // own. A fork-and-wait passthrough would report a different one.
    assert_eq!(
        claude_pid(&a.out),
        a.pid,
        "passthrough did not replace the wrapper with claude"
    );
    a.send(r#"{"type":"user","m":"one"}"#);
    until("the echo", || a.out(), || a.saw("\"echo\""));
    a.send(r#"{"type":"quit"}"#);
    let status = fix.child(a.index).wait().unwrap();
    assert_eq!(code_of(status), 0);
    assert!(!fix.home.exists(), "passthrough wrote a state directory");

    // Signal transparency: the real binary replaces the wrapper, so a signal
    // lands on claude itself.
    let b = fix.panel_with(&on, &[]);
    b.wait_for_init();
    // SAFETY: SIGQUIT to a process this test spawned.
    unsafe {
        libc::kill(b.pid, libc::SIGQUIT);
    }
    let status = fix.child(b.index).wait().unwrap();
    assert_eq!(code_of(status), 128 + 3);
    assert!(!fix.home.exists(), "passthrough wrote a state directory");
}

#[test]
fn an_unkeyable_session_falls_back_to_passthrough() {
    let fix = Fix::new("unkeyable");
    let out = Command::new(BIN)
        .arg(FAKE)
        .args(STREAM)
        .arg("--continue")
        .env("CLAUDE_RELAYD_HOME", &fix.home)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    // stdin closed at once: the fake reports the broken contract itself.
    assert_eq!(
        code_of(out.status),
        3,
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("\"subtype\":\"init\""),
        "passthrough lost claude's output"
    );
    assert!(!fix.home.exists(), "passthrough wrote a state directory");
}

#[test]
fn multibyte_and_huge_lines_survive_both_directions() {
    let mut fix = Fix::new("bytes");
    let mut a = fix.panel();
    a.wait_for_init();

    a.dribble(r#"{"type":"user","m":"привет 🚀"}"#);
    until(
        "the multi-byte echo",
        || a.out(),
        || a.saw("привет \\uD83D\\uDE80") || a.saw("привет 🚀"),
    );

    a.send(r#"{"type":"utf8"}"#);
    until("the dripped multi-byte line", || a.out(), || a.saw("привет 🚀 конец"));

    let big = "ы".repeat(120_000);
    a.send(&format!("{{\"type\":\"user\",\"m\":\"{big}\"}}"));
    until(
        "the echo of a line well over 64 KiB",
        || format!("{} bytes on stdout", slurp(&a.out).len()),
        || a.out().contains(&"ы".repeat(120_000)),
    );

    a.send(r#"{"type":"big","bytes":200000}"#);
    until(
        "a claude line well over 64 KiB",
        || format!("{} bytes on stdout", slurp(&a.out).len()),
        || a.out().contains(&"ы".repeat(100_000)),
    );

    let raw = slurp(&a.out);
    assert!(
        !raw.windows(3).any(|w| w == UFFFD),
        "a replacement character appeared: the relay decoded what it should have copied"
    );
}

#[test]
fn the_event_log_stays_under_its_cap() {
    let mut fix = Fix::new("cap");
    let mut a = fix.panel();
    a.wait_for_init();
    let events = fix.state("events.jsonl");
    // Seventy lines of a megabyte each: past the 64 MiB cap, so it must rotate.
    for _ in 0..70 {
        a.send(r#"{"type":"big","bytes":1000000}"#);
    }
    until(
        "the stream to pass the cap",
        || format!("{} bytes relayed", size(&a.out)),
        || size(&a.out) > 69_000_000,
    );
    // After a rotation the log restarts, so it is megabytes rather than tens of
    // them. Bounding it just above the wait threshold would pass unrotated.
    let grown = size(&events);
    assert!(
        grown < 8 * 1024 * 1024,
        "the event log is {grown} bytes: it never rotated at its 64 MiB cap"
    );
}

#[test]
fn a_replay_that_dropped_events_says_so() {
    let mut fix = Fix::new("truncated");
    let mut a = fix.panel();
    a.wait_for_init();
    // The backlog is produced after this panel is gone, so its cursor stays near
    // the beginning while the ring rolls past it.
    a.send(r#"{"type":"burst","n":2600,"after_ms":3000}"#);
    until(
        "claude to acknowledge the backlog request",
        || a.out(),
        || a.saw("burst_scheduled"),
    );
    let cursor = fix.state("cursor");
    until(
        "the cursor to record the acknowledgement",
        || format!("cursor seq: {}", cursor_seq(&cursor)),
        || cursor_seq(&cursor) >= 2,
    );
    a.window_close(&mut fix);

    until(
        "the daemon to record the whole backlog",
        || {
            format!(
                "seq: {}\nlist: {}",
                daemon_seq(&fix),
                String::from_utf8_lossy(&fix.cmd(&["list"]).stdout)
            )
        },
        || daemon_seq(&fix) > 2600,
    );

    let b = fix.panel();
    b.wait_for_init();
    until(
        "the truncation notice",
        || {
            format!(
                "err: {}\ncursor: {}\ndaemon log: {}\nlist: {}",
                b.err(),
                text(&fix.state("cursor")),
                text(&fix.state("daemon.log")),
                String::from_utf8_lossy(&fix.cmd(&["list"]).stdout)
            )
        },
        || b.err().contains("dropped"),
    );
}

#[test]
fn a_graceful_goodbye_never_reaches_claude() {
    let mut fix = Fix::new("goodbye");
    let mut a = fix.panel();
    a.wait_for_init();
    let claude = claude_pid(&a.out);

    // The panel's stdin ending is a detach, not a shutdown: claude's own stdin
    // must stay open, and the fake exits non-zero the moment it does not.
    a.close_stdin();
    let status = fix.child(a.index).wait().unwrap();
    assert_eq!(code_of(status), 0, "stderr: {}", a.err());
    assert!(alive(claude), "claude {claude} died when the panel said goodbye");

    let events = fix.state("events.jsonl");
    let before = slurp(&events).len();
    until(
        "claude to keep running after the detach",
        || format!("events: {} bytes", slurp(&events).len()),
        || slurp(&events).len() > before,
    );
}

#[test]
fn the_idle_timeout_can_come_from_the_environment() {
    let mut fix = Fix::new("idle-env");
    let mut a = fix.panel_with(&[("CLAUDE_RELAYD_IDLE_TIMEOUT", "0.02")], &[]);
    a.wait_for_init();
    let claude = claude_pid(&a.out);
    a.window_close(&mut fix);
    until_for(
        Duration::from_secs(5),
        "the idle session to end on the environment fallback alone",
        || format!("claude {claude} alive: {}", alive(claude)),
        &mut || !alive(claude),
    );
    let logs = String::from_utf8_lossy(&fix.cmd(&["logs", &fix.id]).stdout).into_owned();
    assert!(
        logs.contains("idle with no client"),
        "the session ended, but not through the idle timeout: {logs}"
    );
}

#[test]
fn a_failed_spawn_leaves_no_copy_of_the_panel_environment() {
    let fix = Fix::new("badspawn");
    let out = Command::new(BIN)
        .arg("/nonexistent/claude")
        .args(STREAM)
        .arg(format!("--resume={}", fix.id))
        .env("CLAUDE_RELAYD_HOME", &fix.home)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success(), "a missing claude binary reported success");
    // The spawn record is the panel's whole environment, API keys included. It
    // must not outlive the one spawn it exists for, successful or not.
    assert!(
        !fix.state("spawn.json").exists(),
        "the spawn record survived a failed spawn: {:?}",
        std::fs::read_dir(&fix.home)
            .map(|d| d.flatten().map(|e| e.file_name()).collect::<Vec<_>>())
    );
}

#[test]
fn a_permission_mode_the_panel_changed_is_reported_not_guessed_at() {
    let mut fix = Fix::new("mode");
    let mut a = fix.panel_full(&[], &[], &["--permission-mode", "plan"]);
    a.wait_for_init();
    a.window_close(&mut fix);

    // The panel comes back asking for a different mode. Acting on that would mean
    // writing a control request whose response the panel never asked for, so the
    // daemon says so in its log and leaves claude alone.
    let b = fix.panel_full(&[], &[], &["--permission-mode", "acceptEdits"]);
    b.wait_for_init();
    until(
        "the daemon to report the permission mode mismatch",
        || {
            format!(
                "daemon log: {}",
                String::from_utf8_lossy(&fix.cmd(&["logs", &fix.id]).stdout)
            )
        },
        || {
            let logs = fix.cmd(&["logs", &fix.id]);
            let s = String::from_utf8_lossy(&logs.stdout).into_owned();
            s.contains("acceptEdits") && s.contains("plan")
        },
    );
}

#[test]
fn racing_panels_never_start_a_second_claude() {
    let mut fix = Fix::new("race");
    // Six panels for one session id, started back to back with nothing between
    // them: whoever wins the lock, the others must attach to it.
    let panels: Vec<Panel> = (0..6).map(|_| fix.panel()).collect();
    for p in &panels {
        p.wait_for_init();
    }
    let first = claude_pid(&panels[0].out);
    for p in &panels {
        assert_eq!(
            claude_pid(&p.out),
            first,
            "a second claude was started for one session id"
        );
    }
    let locks = std::fs::read_dir(&fix.home)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".lock"))
        .count();
    assert_eq!(locks, 1, "more than one lock file for one session id");
}

#[test]
fn panels_racing_a_lock_left_by_a_dead_daemon_still_share_one_claude() {
    let mut fix = Fix::new("stale-race");
    std::fs::create_dir_all(&fix.home).unwrap();
    // What a daemon that died without unwinding leaves behind: a lock naming a
    // process that is gone. Every panel below therefore has to take it from a
    // dead holder, and that is the step which must stay atomic.
    //
    // The filler makes the taking observable. A daemon reads the whole lock file
    // to decide whether its holder is alive, so sixteen megabytes turn a decision
    // that takes microseconds into one that takes milliseconds, and the rest of
    // the panels arrive while the first is still inside it. Holder::parse reads
    // the first two words, so this is still an ordinary lock file.
    let ghost = Command::new("true").spawn().unwrap();
    let gone = ghost.id().cast_signed();
    let mut reaped = ghost;
    reaped.wait().unwrap();
    let mut stale = format!("{gone} 1\n");
    stale.push_str(&"#".repeat(16 << 20));
    std::fs::write(fix.state("lock"), stale).unwrap();
    // A daemon opens its log before it claims the lock, so a FIFO in that place
    // is a starting gate: it blocks there until this test opens the read end.
    // Only the first panel is held, because the point is to catch the others
    // mid-decision rather than to start them all at once — released together
    // they decide together, which is the harmless half of the race.
    let log = fix.state("daemon.log");
    fifo(&log);

    let mut panels = vec![fix.panel()];
    until(
        "the first daemon to reach the starting gate",
        || format!("daemons at the gate: {}", daemons(&fix.id)),
        || daemons(&fix.id) == 1,
    );
    // Opening the gate releases it into the read; the thread keeps draining so a
    // full pipe can never stall a daemon that logs.
    let mut gate = std::fs::File::open(&log).unwrap();
    std::thread::spawn(move || {
        let mut sink = Vec::new();
        let _ = gate.read_to_end(&mut sink);
    });
    for _ in 1..10 {
        panels.push(fix.panel());
    }

    for p in &panels {
        p.wait_for_init();
    }
    let claudes: Vec<i32> = panels.iter().map(|p| claude_pid(&p.out)).collect();
    for pid in &claudes {
        fix.stray(*pid);
    }
    assert!(
        claudes.iter().all(|p| *p == claudes[0]),
        "one session id ended up with several claudes: {claudes:?}"
    );
}

#[test]
fn a_cursor_from_an_earlier_run_is_rewritten_not_left_stale() {
    let mut fix = Fix::new("epoch");
    std::fs::create_dir_all(&fix.home).unwrap();
    // What a SIGKILLed daemon leaves behind: a cursor whose run is gone and whose
    // sequence number the next run will not reach for the best part of an hour.
    std::fs::write(fix.state("cursor"), br#"{"epoch":"an-earlier-run","seq":9999}"#).unwrap();

    let a = fix.panel();
    a.wait_for_init();
    until(
        "live events to reach the panel",
        || a.out(),
        || a.count("heartbeat") > 0,
    );
    let cursor = fix.state("cursor");
    until(
        "the cursor to be rewritten for this run",
        || format!("cursor: {}", text(&cursor)),
        || {
            let c = text(&cursor);
            !c.contains("an-earlier-run") && cursor_seq(&cursor) < 100
        },
    );
}

#[test]
fn a_client_that_sends_nonsense_does_not_strand_the_session() {
    let mut fix = Fix::new("nonsense");
    let mut a = fix.panel_with(&[], &["--idle-timeout", "0.02"]);
    a.wait_for_init();
    let claude = claude_pid(&a.out);

    // A client that greets properly and then says something unparseable must
    // still be forgotten, or the daemon counts it as attached for ever and no
    // idle timeout or tab close can ever end the session.
    let mut ghost = UnixStream::connect(fix.state("sock")).unwrap();
    ghost
        .write_all(b"{\"hello\":{\"since\":null,\"epoch\":null,\"replay\":\"init\"}}\n")
        .unwrap();
    until(
        "the daemon to count the extra client",
        || format!("clients: {}", text(&fix.state("status.json"))),
        || text(&fix.state("status.json")).contains("\"clients\":2"),
    );
    ghost.write_all(b"{\"nonsense\":true}\n").unwrap();

    a.window_close(&mut fix);
    until_for(
        Duration::from_secs(12),
        "the session to end with no real client left",
        || {
            format!(
                "claude alive: {}, status: {}",
                alive(claude),
                text(&fix.state("status.json"))
            )
        },
        &mut || !alive(claude),
    );
}

#[test]
fn a_socket_outside_the_state_directory_still_lands_somewhere_private() {
    // A state directory long enough to overflow sun_path pushes the socket into
    // the shared temp directory, where it must land inside one of ours.
    let mut fix = Fix::new("longpath");
    fix.home = fix.root.join("d".repeat(60)).join("home");
    let a = fix.panel();
    a.wait_for_init();

    // SAFETY: geteuid always succeeds.
    let uid = unsafe { libc::geteuid() };
    let dir = std::env::temp_dir().join(format!("claude-relayd-{uid}"));
    let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "{} is {mode:o}, not 0700", dir.display());
    fix.cmd(&["kill", &fix.id]);
}

#[test]
fn a_finished_claude_never_reports_as_a_lost_daemon() {
    let fix = Fix::new("exitenv");
    let mut cmd = Command::new(BIN);
    cmd.arg(FAKE)
        .args(STREAM)
        .arg(format!("--resume={}", fix.id))
        .env("CLAUDE_RELAYD_HOME", &fix.home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();

    // Nothing reads this panel's stdout yet, so three big lines fill the pipe and
    // then the socket: the wrapper stops reading and the daemon's queue backs up.
    for _ in 0..3 {
        stdin.write_all(b"{\"type\":\"big\",\"bytes\":400000}\n").unwrap();
    }
    stdin.write_all(b"{\"type\":\"exit\",\"code\":7}\n").unwrap();
    stdin.flush().unwrap();
    // The panel comes back to its stdout shortly after claude exits. A daemon
    // that only hoped the envelope had gone out has already closed the socket.
    std::thread::sleep(Duration::from_millis(400));
    let mut seen = Vec::new();
    stdout.read_to_end(&mut seen).unwrap();
    let status = child.wait().unwrap();
    assert_eq!(
        code_of(status),
        7,
        "claude exited 7 but the panel was told {}; it read {} bytes",
        code_of(status),
        seen.len()
    );
}

/// Processes whose command line mentions this session id, so a test can prove
/// nothing was left running under it.
fn claudes_for(id: &str) -> Vec<i32> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Ok(pid) = name.to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Ok(cmd) = std::fs::read(e.path().join("cmdline")) else {
            continue;
        };
        let cmd = String::from_utf8_lossy(&cmd);
        if cmd.contains(id) && cmd.contains("fake-claude") {
            out.push(pid);
        }
    }
    out
}

fn proc_state(pid: i32) -> Option<char> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(") ")?.1.chars().next()
}

#[test]
fn fork_session_after_resume_is_still_unkeyable() {
    let fix = Fix::new("fork");
    // The panel puts --resume in the middle of its arguments, so a marker after
    // it must count: --fork-session makes claude write a different session id.
    let out = Command::new(BIN)
        .arg(FAKE)
        .args(STREAM)
        .arg(format!("--resume={}", fix.id))
        .arg("--fork-session")
        .env("CLAUDE_RELAYD_HOME", &fix.home)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(code_of(out.status), 3, "the run was not passed through");
    assert!(!fix.home.exists(), "passthrough wrote a state directory");
}

#[test]
fn a_daemon_that_cannot_start_leaves_no_state_and_no_claude() {
    let fix = Fix::new("nostart");
    std::fs::create_dir_all(&fix.home).unwrap();
    // The daemon cannot open its claude stderr log, which must fail before claude
    // is spawned: afterwards it would exit holding claude's stdin.
    std::fs::create_dir_all(fix.state("stderr.log")).unwrap();

    let out = Command::new(BIN)
        .arg(FAKE)
        .args(STREAM)
        .arg(format!("--resume={}", fix.id))
        .env("CLAUDE_RELAYD_HOME", &fix.home)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(
        code_of(out.status),
        3,
        "the run did not fall back to passthrough: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        claudes_for(&fix.id).is_empty(),
        "a claude was left running with nobody holding its stdin"
    );
    assert!(
        !fix.state("spawn.json").exists(),
        "the panel environment was left behind by a passthrough run"
    );
    assert!(!fix.state("lock").exists(), "a lock was left behind");
}

#[test]
fn a_zombie_extension_host_is_not_a_closed_tab() {
    let mut fix = Fix::new("zombie");
    let (host, _panel, _idx) = fix.hosted_panel(false);
    let claude = claude_pid(&fix.root.join("hosted-out"));

    // Killed and deliberately not reaped: its /proc entry stays readable, so a
    // liveness check that stops at "the pid answers" calls a dead host alive.
    // SAFETY: the group this test created.
    unsafe {
        libc::kill(-host, libc::SIGKILL);
    }
    until(
        "the stub host to become a zombie",
        || format!("host state: {:?}", proc_state(host)),
        || proc_state(host) == Some('Z'),
    );

    let logs = || String::from_utf8_lossy(&fix.cmd(&["logs", &fix.id]).stdout).into_owned();
    until_for(
        AFTER_GRACE,
        "the daemon to decide what the zombie host means",
        || format!("daemon log: {}", logs()),
        &mut || logs().contains("keeping the session"),
    );
    assert!(
        alive(claude),
        "claude {claude} was killed because a zombie host looked alive"
    );
}

#[test]
fn a_burst_inside_one_debounce_window_is_not_replayed_whole() {
    let mut fix = Fix::new("burstcursor");
    let mut a = fix.panel();
    a.wait_for_init();
    // Two hundred events land well inside one debounce interval. A cursor bounded
    // only by time would still point before all of them when the window closes,
    // and the next panel would be shown the whole turn a second time.
    a.send(r#"{"type":"burst","n":200,"after_ms":0}"#);
    until(
        "the panel to receive the whole burst",
        || format!("burst lines: {}", a.count("\"burst\":")),
        || a.count("\"burst\":") >= 200,
    );
    a.window_close(&mut fix);

    // The cursor itself, because "nothing was replayed" is also what a cursor
    // that was never written at all produces.
    let recorded = cursor_seq(&fix.state("cursor"));
    assert!(
        recorded >= 200 - 32,
        "the cursor stopped at {recorded} with 200 events already shown to the panel"
    );

    let b = fix.panel();
    b.wait_for_init();
    until("the reattach to settle", || b.out(), || b.count("heartbeat") > 0);
    let repeated = b.count("\"burst\":");
    assert!(
        repeated <= 32,
        "the reattach replayed {repeated} events the panel had already been shown"
    );
}

#[test]
fn an_unfamiliar_panel_message_is_noted_and_still_forwarded() {
    let mut fix = Fix::new("panellog");
    let mut a = fix.panel();
    a.wait_for_init();
    a.send(r#"{"type":"user","m":"ordinary"}"#);
    a.send(r#"{"type":"mystery","m":"what the panel sent at close"}"#);
    until("both lines to reach claude", || a.out(), || a.count("\"echo\"") >= 2);

    // The log exists to answer one live-test question: what does a real panel
    // send when the window closes? So it records what this tool does not know,
    // and nothing it does.
    let log = text(&fix.state("panel.log"));
    assert!(
        log.contains("what the panel sent at close"),
        "the unfamiliar line was not recorded: {log:?}"
    );
    assert!(
        !log.contains("ordinary"),
        "an ordinary panel line was recorded too: {log:?}"
    );
}
