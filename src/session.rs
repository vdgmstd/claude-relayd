//! The stream side of a session: the clients attached to it, the replay ring and
//! the two pumps that carry claude's output to them.
//!
//! A client going away never reaches claude here. It only removes a row from the
//! client table and tells the supervisor, which owns every decision about the
//! process itself.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ChildStdin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::protocol::{FromClient, FromDaemon, Hello, Replay, read_frame};
use crate::state::{self, Holder};

/// Replay is bounded by constants, not settings: a twelve-hour session must not
/// grow without bound.
const RING_EVENTS: usize = 2000;
const RING_BYTES: usize = 8 * 1024 * 1024;
const EVENT_LOG_BYTES: u64 = 64 * 1024 * 1024;
/// How long a panel that has stopped reading may delay the daemon's exit.
const EXIT_FLUSH: Duration = Duration::from_secs(2);

pub(crate) struct Client {
    tx: SyncSender<Arc<Vec<u8>>>,
    /// Live events this client's queue could not hold. It is told, on its own
    /// socket, rather than left to believe it saw everything.
    dropped: Arc<AtomicU64>,
    /// A second handle on the same socket, so the exit envelope can be given a
    /// deadline without reaching into the writer thread.
    sock: UnixStream,
    writer: std::thread::JoinHandle<()>,
}

#[derive(Default)]
pub(crate) struct Shared {
    pub(crate) seq: u64,
    ring: VecDeque<(u64, Arc<Vec<u8>>)>,
    ring_bytes: usize,
    /// The line a reattach replays, and the daemon's evidence that this session
    /// ever became a conversation.
    pub(crate) init: Option<(u64, Arc<Vec<u8>>)>,
    pub(crate) clients: HashMap<u64, Client>,
    next_client: u64,
    /// The parent of the most recent client: the evidence for tab close.
    pub(crate) last_parent: Option<Holder>,
    /// The permission mode claude was started with, and the one the last panel
    /// asked for. A mismatch is logged, never guessed at over claude's stdin.
    pub(crate) mode: Option<String>,
    /// The model and thinking level claude last reported as applied.
    pub(crate) applied: Option<Applied>,
}

impl Ctx {
    pub(crate) fn new(
        epoch: String,
        ctl: Sender<Ctl>,
        claude_stdin: Option<ChildStdin>,
        mode: Option<String>,
    ) -> Ctx {
        Ctx {
            shared: Mutex::new(Shared {
                mode,
                ..Shared::default()
            }),
            reaped: AtomicBool::new(false),
            epoch,
            ctl,
            claude_stdin: Mutex::new(claude_stdin),
        }
    }

    /// Poisoning is not fatal here: a panicking thread never leaves the ring or
    /// the client table half-updated, and the daemon must not die with it.
    pub(crate) fn shared(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub(crate) enum Ctl {
    Attached,
    Detached,
    Kill,
    Exited(Option<i32>, Option<i32>),
}

pub(crate) struct Ctx {
    pub(crate) shared: Mutex<Shared>,
    /// Set the moment `wait` returns. After that the pid may belong to somebody
    /// else, and a stray SIGTERM would land on a stranger.
    pub(crate) reaped: AtomicBool,
    pub(crate) epoch: String,
    pub(crate) ctl: Sender<Ctl>,
    /// Never closed while the daemon lives. This is the whole point of the tool.
    pub(crate) claude_stdin: Mutex<Option<ChildStdin>>,
}

/// Sends one frame to every attached client. A slow client loses the oldest
/// events it could not take and is told so; claude is never made to wait.
fn broadcast(shared: &mut Shared, bytes: &Arc<Vec<u8>>) {
    for c in shared.clients.values() {
        match c.tx.try_send(Arc::clone(bytes)) {
            Err(TrySendError::Full(_)) => {
                c.dropped.fetch_add(1, Ordering::Relaxed);
            }
            // Taken, or its writer has gone and its reader thread removes it.
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

fn ring_push(shared: &mut Shared, seq: u64, bytes: &Arc<Vec<u8>>) {
    shared.ring.push_back((seq, Arc::clone(bytes)));
    shared.ring_bytes += bytes.len();
    while (shared.ring.len() > RING_EVENTS
        || (shared.ring_bytes > RING_BYTES && shared.ring.len() > 1))
        && let Some((_, old)) = shared.ring.pop_front()
    {
        shared.ring_bytes -= old.len();
    }
}

/// What claude answers a settings push with. Neither the model nor the thinking
/// level is in argv and both change mid-session, so this line is the only source
/// `list` has for either.
#[derive(Clone, Default)]
pub(crate) struct Applied {
    pub(crate) model: String,
    pub(crate) effort: String,
    pub(crate) ultracode: bool,
}

const APPLIED: &[u8] = b"\"applied\":{";

/// Gated on the key rather than on the shape of the line: JSON object order is
/// not guaranteed, and matching a leading `{"type":...` would quietly stop
/// working the day claude emits its keys in another order.
fn applied(line: &[u8]) -> Option<Applied> {
    if !line.windows(APPLIED.len()).any(|w| w == APPLIED) {
        return None;
    }
    let v: Value = serde_json::from_slice(line).ok()?;
    let a = v.get("response")?.get("response")?.get("applied")?;
    Some(Applied {
        model: a.get("model").and_then(Value::as_str)?.to_string(),
        effort: a.get("effort").and_then(Value::as_str).unwrap_or("?").to_string(),
        ultracode: a.get("ultracode").and_then(Value::as_bool).unwrap_or(false),
    })
}

fn is_init(line: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<Value>(line) else {
        return false;
    };
    v.get("type").and_then(Value::as_str) == Some("system")
        && v.get("subtype").and_then(Value::as_str) == Some("init")
}

/// Reads claude's stdout for the life of the process: appends every line to the
/// event log, keeps the replay ring, and fans the line out to the clients.
pub(crate) fn pump_stdout(ctx: Arc<Ctx>, out: std::process::ChildStdout, events: PathBuf) {
    let mut file = match state::create_private(&events) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("relayd: failed to create {}: {e}", events.display());
            return;
        }
    };
    let mut written: u64 = 0;
    let mut reader = BufReader::new(out);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("relayd: failed to read claude stdout: {e}");
                break;
            }
        }
        let mut shared = ctx.shared();
        shared.seq += 1;
        let seq = shared.seq;
        let bytes = Arc::new(FromDaemon::event(seq, &ctx.epoch, &line));
        if shared.init.is_none() && is_init(&line) {
            shared.init = Some((seq, Arc::clone(&bytes)));
        }
        if let Some(a) = applied(&line) {
            shared.applied = Some(a);
        }
        ring_push(&mut shared, seq, &bytes);
        broadcast(&mut shared, &bytes);
        // The log belongs to this run only, which is what keeps seq unambiguous.
        if written + bytes.len() as u64 > EVENT_LOG_BYTES {
            let init = shared.init.as_ref().map(|(_, b)| Arc::clone(b));
            drop(shared);
            match state::create_private(&events) {
                Ok(f) => {
                    file = f;
                    written = 0;
                    if let Some(b) = init {
                        written += write_log(&mut file, &b, &events);
                    }
                }
                Err(e) => eprintln!("relayd: failed to rotate {}: {e}", events.display()),
            }
        } else {
            drop(shared);
        }
        written += write_log(&mut file, &bytes, &events);
    }
}

fn write_log(file: &mut File, bytes: &[u8], path: &Path) -> u64 {
    match file.write_all(bytes) {
        Ok(()) => bytes.len() as u64,
        Err(e) => {
            eprintln!("relayd: failed to write {}: {e}", path.display());
            0
        }
    }
}

/// claude's stderr is a diagnostic channel the extension reads: keep the file
/// copy and forward it live, out of band, so it never moves a cursor.
pub(crate) fn pump_stderr(ctx: Arc<Ctx>, err: std::process::ChildStderr, mut log: File) {
    let mut reader = BufReader::new(err);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("relayd: failed to read claude stderr: {e}");
                break;
            }
        }
        if let Err(e) = log.write_all(&line) {
            eprintln!("relayd: failed to write the claude stderr log: {e}");
        }
        let bytes = Arc::new(FromDaemon::stderr(&line));
        let mut shared = ctx.shared();
        broadcast(&mut shared, &bytes);
    }
}

pub(crate) fn serve_client(ctx: Arc<Ctx>, stream: UnixStream) -> Result<()> {
    let write_half = stream.try_clone().context("clone the client socket")?;
    let mut reader = BufReader::new(stream);
    let mut me: Option<u64> = None;
    let result = loop {
        let frame = match read_frame(&mut reader) {
            Ok(Some(f)) => f,
            Ok(None) => break Ok(()),
            Err(e) => break Err(e),
        };
        let msg = match FromClient::of(frame) {
            Ok(m) => m,
            // Leaving by `?` here would skip the deregistration below and strand a
            // client that never detaches, so the session could never idle out.
            Err(e) => break Err(e),
        };
        match msg {
            FromClient::Hello(hello) => {
                if me.is_none() {
                    me = Some(attach(&ctx, &hello, write_half.try_clone()?));
                }
            }
            FromClient::Detach => break Ok(()),
            FromClient::Kill => {
                if ctx.ctl.send(Ctl::Kill).is_err() {
                    break Ok(());
                }
            }
            // The panel talking to claude: forwarded verbatim, never rewritten.
            FromClient::Payload(body) => {
                let mut guard = ctx.claude_stdin.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(stdin) = guard.as_mut()
                    && let Err(e) = stdin.write_all(&body).and_then(|()| stdin.flush())
                {
                    eprintln!("relayd: failed to write claude stdin: {e}");
                }
            }
        }
    };
    if let Some(idx) = me {
        let mut shared = ctx.shared();
        shared.clients.remove(&idx);
        drop(shared);
        // A client going away never touches claude; it only starts the clock.
        let _ = ctx.ctl.send(Ctl::Detached);
    }
    result
}

fn attach(ctx: &Arc<Ctx>, hello: &Hello, sock: UnixStream) -> u64 {
    let (tx, rx) = sync_channel(RING_EVENTS);
    let dropped = Arc::new(AtomicU64::new(0));
    let dropped_for_writer = Arc::clone(&dropped);
    let mut shared = ctx.shared();
    let batch = replay_batch(&shared, hello, &ctx.epoch);
    let idx = shared.next_client;
    shared.next_client += 1;
    let mirror = match sock.try_clone() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("relayd: failed to clone a client socket: {e}");
            return idx;
        }
    };
    let writer = std::thread::spawn(move || write_client(sock, batch, rx, dropped_for_writer));
    shared.clients.insert(
        idx,
        Client {
            tx,
            dropped,
            sock: mirror,
            writer,
        },
    );
    // Always the newest client, even when it is None: keeping a previous panel's
    // parent would let a stale, still-living process argue that the tab was closed.
    shared.last_parent = hello.parent;
    if let Some(want) = &hello.permission_mode
        && shared.mode.as_deref() != Some(want.as_str())
    {
        // Sending claude a set-permission-mode request would put an unsolicited
        // control_response on the panel's stream; that stays an owner decision.
        eprintln!(
            "relayd: panel asked for permission mode {want}, claude is running with {:?}",
            shared.mode
        );
    }
    drop(shared);
    if let Err(e) = ctx.ctl.send(Ctl::Attached) {
        eprintln!("relayd: failed to announce a client: {e}");
    }
    idx
}

/// Everything this client must receive before the live stream, computed under
/// the same lock that registers it so no event is lost or sent twice.
fn replay_batch(shared: &Shared, hello: &Hello, epoch: &str) -> Vec<Arc<Vec<u8>>> {
    let mut out = Vec::new();
    let init_seq = shared.init.as_ref().map(|(s, _)| *s);
    if let Some((_, b)) = &shared.init {
        out.push(Arc::clone(b));
    }
    let same_epoch = hello.epoch.as_deref() == Some(epoch);
    let Some(since) = hello.since.filter(|_| same_epoch) else {
        return out;
    };
    if hello.replay != Replay::InitAndSince {
        return out;
    }
    let oldest = shared.ring.front().map_or(shared.seq + 1, |(s, _)| *s);
    if since + 1 < oldest {
        out.push(Arc::new(FromDaemon::truncated(oldest - 1 - since)));
    }
    for (seq, b) in &shared.ring {
        if *seq > since && Some(*seq) != init_seq {
            out.push(Arc::clone(b));
        }
    }
    out
}

/// One writer per client: the replay batch first, then the live queue, with the
/// socket providing the backpressure.
fn write_client(
    mut sock: UnixStream,
    batch: Vec<Arc<Vec<u8>>>,
    rx: Receiver<Arc<Vec<u8>>>,
    dropped: Arc<AtomicU64>,
) {
    for b in batch {
        if sock.write_all(&b).is_err() {
            return;
        }
    }
    while let Ok(b) = rx.recv() {
        let missed = dropped.swap(0, Ordering::Relaxed);
        if missed > 0 && sock.write_all(&FromDaemon::truncated(missed)).is_err() {
            return;
        }
        if sock.write_all(&b).is_err() {
            return;
        }
    }
}

/// The exit envelope is the panel's only news that claude finished, so it is
/// queued with a blocking send rather than dropped like a live event, and the
/// writers are joined rather than slept on. A deadline on each socket keeps a
/// panel that has stopped reading from holding the daemon open.
pub(crate) fn deliver_exit(ctx: &Arc<Ctx>, code: Option<i32>, signal: Option<i32>) {
    let bytes = Arc::new(FromDaemon::exit(code, signal));
    let clients: Vec<Client> = ctx.shared().clients.drain().map(|(_, c)| c).collect();
    for c in &clients {
        if let Err(e) = c.sock.set_write_timeout(Some(EXIT_FLUSH)) {
            eprintln!("relayd: failed to put a deadline on a client socket: {e}");
        }
        if let Err(e) = c.tx.send(Arc::clone(&bytes)) {
            eprintln!("relayd: failed to queue the exit envelope: {e}");
        }
    }
    for c in clients {
        let Client { tx, writer, .. } = c;
        drop(tx);
        if writer.join().is_err() {
            eprintln!("relayd: a client writer panicked on the way out");
        }
    }
}
