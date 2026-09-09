//! The three commands an operator runs by hand. `list` prints lines.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use serde_json::Value;

use crate::protocol::FromClient;
use crate::state;

fn uptime(started_ms: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis()) as u64;
    let s = now.saturating_sub(started_ms) / 1000;
    match (s / 3600, (s % 3600) / 60, s % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m{s:02}s"),
        (h, m, _) => format!("{h}h{m:02}m"),
    }
}

/// One column of the status record, or `?` while the daemon has not written it.
fn column(status: Option<&Value>, key: &str) -> String {
    match status.and_then(|v| v.get(key)) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => "?".to_string(),
    }
}

/// The session ids that have a `<id>.<ext>` file in the state directory.
fn ids_with(dir: &Path, ext: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let suffix = format!(".{ext}");
    entries
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_string_lossy()
                .strip_suffix(&suffix)
                .map(str::to_string)
        })
        .collect()
}

/// The effort level, with ultracode folded in: two facts nobody wants in two
/// columns.
fn thinking(status: Option<&Value>) -> String {
    let effort = column(status, "effort");
    match status.and_then(|v| v.get("ultracode")) {
        Some(Value::Bool(true)) => format!("{effort}+ultra"),
        _ => effort,
    }
}

/// Every id whose daemon is still alive. A dead holder is simply not a session;
/// clearing up after it belongs to the next daemon that claims the lock, not to
/// a command reading the directory.
fn live_sessions(dir: &Path) -> Vec<(String, state::Holder)> {
    ids_with(dir, "lock")
        .into_iter()
        .filter_map(|id| state::live_holder(&state::file(dir, &id, "lock")).map(|h| (id, h)))
        .collect()
}

/// What `list` prints instead of the whole id: the last group of a UUID is short
/// enough to read and long enough to stay unique. Anything shorter than that is
/// not worth abbreviating, so it is shown whole.
fn short(id: &str) -> &str {
    match id.rsplit_once('-') {
        Some((_, tail)) if tail.len() >= 8 => tail,
        _ => id,
    }
}

/// Takes back what `list` printed: a whole id, or just the tail. `Ok(None)` is
/// "nothing matched", so each caller can say what it was looking for; an error
/// is only the ambiguous case, which the caller cannot resolve either.
fn resolve<'a>(arg: &str, ids: &'a [String]) -> Result<Option<&'a str>> {
    if let Some(exact) = ids.iter().find(|id| *id == arg) {
        return Ok(Some(exact));
    }
    let mut hits = ids.iter().filter(|id| short(id) == arg);
    match (hits.next(), hits.next()) {
        (Some(one), None) => Ok(Some(one)),
        (Some(a), Some(b)) => bail!("tell {arg} apart: it matches {a} and {b}"),
        _ => Ok(None),
    }
}

pub(crate) fn list() -> Result<u8> {
    let dir = state::home()?;
    let mut rows = Vec::new();
    for (id, holder) in live_sessions(&dir) {
        let status = state::read_json(&state::file(&dir, &id, "status.json"));
        let started = status
            .as_ref()
            .and_then(|v| v.get("started_ms"))
            .and_then(Value::as_u64);
        rows.push(format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            short(&id),
            holder.pid,
            column(status.as_ref(), "claude_pid"),
            started.map_or_else(|| "?".to_string(), uptime),
            column(status.as_ref(), "clients"),
            column(status.as_ref(), "seq"),
            column(status.as_ref(), "model"),
            thinking(status.as_ref()),
            column(status.as_ref(), "cwd"),
        ));
    }
    if rows.is_empty() {
        println!("no live sessions in {}", dir.display());
        return Ok(0);
    }
    println!("ID\tDAEMON\tCLAUDE\tUPTIME\tCLIENTS\tSEQ\tMODEL\tTHINKING\tCWD");
    for r in rows {
        println!("{r}");
    }
    Ok(0)
}

pub(crate) fn kill(arg: &str) -> Result<u8> {
    let dir = state::home()?;
    let ids: Vec<String> = live_sessions(&dir).into_iter().map(|(id, _)| id).collect();
    let Some(id) = resolve(arg, &ids)? else {
        bail!("kill {arg}: there is no live session with that id");
    };
    if !end_session(&dir, id)? {
        bail!("kill {id}: there is no live session with that id");
    }
    Ok(0)
}

/// `Ok(false)` means nobody was there to end: the session went away between the
/// listing and the connect, which is not a failure to end it. A holder still on
/// the lock with no socket is one, and says so.
fn end_session(dir: &Path, id: &str) -> Result<bool> {
    let path = state::socket(dir, id);
    let Ok(mut sock) = UnixStream::connect(&path) else {
        return match state::live_holder(&state::file(dir, id, "lock")) {
            Some(h) => bail!("kill {id}: daemon {} holds it but its socket is unreachable", h.pid),
            None => Ok(false),
        };
    };
    sock.write_all(&FromClient::kill())?;
    // The daemon terminates claude, cleans up and exits; its socket closing is
    // the acknowledgement.
    sock.set_read_timeout(Some(Duration::from_secs(15)))?;
    let mut sink = Vec::new();
    if let Err(e) = sock.read_to_end(&mut sink)
        && e.kind() != std::io::ErrorKind::WouldBlock
        && e.kind() != std::io::ErrorKind::TimedOut
    {
        bail!("wait for session {id} to end: {e}");
    }
    Ok(true)
}

pub(crate) fn kill_all() -> Result<u8> {
    let dir = state::home()?;
    let ids = live_sessions(&dir);
    if ids.is_empty() {
        println!("no live sessions in {}", dir.display());
        return Ok(0);
    }
    let mut code = 0;
    for (id, _) in ids {
        match end_session(&dir, &id) {
            // Gone on its own between the listing and the connect counts as
            // ended: the sweep asked for exactly that.
            Ok(_) => println!("ended {id}"),
            // One session refusing to end must not hide the others.
            Err(e) => {
                eprintln!("claude-relayd: failed to {e:#}");
                code = 1;
            }
        }
    }
    Ok(code)
}

fn dump(path: &Path, title: &str) {
    let Ok(bytes) = std::fs::read(path) else {
        return;
    };
    println!("==> {title} <==");
    if let Err(e) = std::io::stdout().write_all(&bytes) {
        eprintln!("claude-relayd: failed to write stdout: {e}");
    }
}

pub(crate) fn logs(arg: &str) -> Result<u8> {
    let dir = state::home()?;
    // A session that has ended still has logs, so these come from the files
    // rather than from the live sessions.
    let mut ids = ids_with(&dir, "daemon.log");
    ids.extend(ids_with(&dir, "stderr.log"));
    ids.sort();
    ids.dedup();
    let Some(id) = resolve(arg, &ids)? else {
        bail!("show the logs for {arg}: there is nothing recorded under that id");
    };
    dump(&state::file(&dir, id, "daemon.log"), "daemon log");
    dump(&state::file(&dir, id, "stderr.log"), "claude stderr");
    Ok(0)
}
