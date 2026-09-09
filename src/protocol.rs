//! The only lines this tool parses; every payload line stays opaque bytes.
//!
//! A frame is a JSON header line, optionally followed by exactly `len` bytes of
//! body. Because the body is never re-encoded, a payload of any length and any
//! byte content crosses the socket unchanged, and no side needs to decode UTF-8.

use std::io::{BufRead, Read};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::state::Holder;

pub(crate) struct Frame {
    pub(crate) head: Value,
    pub(crate) body: Vec<u8>,
}

pub(crate) fn read_frame(r: &mut impl BufRead) -> Result<Option<Frame>> {
    // No resynchronising: a header is always followed by exactly `len` body
    // bytes, so the stream is always positioned at the start of the next header,
    // and anything else should fail loudly rather than be skipped.
    let mut line = Vec::new();
    if r.read_until(b'\n', &mut line).context("read a frame header")? == 0 {
        return Ok(None);
    }
    let head: Value = serde_json::from_slice(&line).context("parse a frame header")?;
    if !head.is_object() {
        bail!("parse a frame header: it is not an object");
    }
    let mut body = Vec::new();
    if let Some(len) = head.get("len").and_then(Value::as_u64) {
        // take() rather than a pre-allocation: a garbled header cannot make us
        // reserve a gigabyte before the read fails.
        r.take(len).read_to_end(&mut body).context("read a frame body")?;
        if body.len() as u64 != len {
            bail!("read a frame body: it ended after {} of {len} bytes", body.len());
        }
    }
    Ok(Some(Frame { head, body }))
}

/// Serialises one frame. `len` is filled in here so no caller can disagree with
/// the body it passed.
pub(crate) fn frame(head: Value, body: &[u8]) -> Vec<u8> {
    let mut head = head;
    if let Some(obj) = head.as_object_mut()
        && !body.is_empty()
    {
        obj.insert("len".to_string(), json!(body.len()));
    }
    let mut out = head.to_string().into_bytes();
    out.push(b'\n');
    out.extend_from_slice(body);
    out
}

/// How much of the stream an attaching client wants replayed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Replay {
    InitAndSince,
    Init,
}

impl Replay {
    fn parse(raw: Option<&str>) -> Replay {
        match raw {
            Some("init") => Replay::Init,
            _ => Replay::InitAndSince,
        }
    }

    fn text(self) -> &'static str {
        match self {
            Replay::InitAndSince => "init+since",
            Replay::Init => "init",
        }
    }
}

pub(crate) struct Hello {
    pub(crate) since: Option<u64>,
    pub(crate) epoch: Option<String>,
    pub(crate) replay: Replay,
    /// The wrapper's parent, so the daemon can tell a closed tab (parent alive)
    /// from a dead extension host (parent gone).
    pub(crate) parent: Option<Holder>,
    pub(crate) permission_mode: Option<String>,
}

impl Hello {
    pub(crate) fn frame(&self) -> Vec<u8> {
        frame(
            json!({"hello": {
                "since": self.since,
                "epoch": self.epoch,
                "replay": self.replay.text(),
                "parent": self.parent.map(|p| p.text()),
                "permission_mode": self.permission_mode,
            }}),
            &[],
        )
    }

    fn parse(v: &Value) -> Hello {
        Hello {
            since: v.get("since").and_then(Value::as_u64),
            epoch: v.get("epoch").and_then(Value::as_str).map(str::to_string),
            replay: Replay::parse(v.get("replay").and_then(Value::as_str)),
            parent: v.get("parent").and_then(Value::as_str).and_then(Holder::parse),
            permission_mode: v.get("permission_mode").and_then(Value::as_str).map(str::to_string),
        }
    }
}

/// Wrapper (or `kill`) to daemon.
pub(crate) enum FromClient {
    Hello(Box<Hello>),
    Detach,
    Kill,
    /// A panel line for claude's stdin, verbatim.
    Payload(Vec<u8>),
}

impl FromClient {
    pub(crate) fn of(f: Frame) -> Result<FromClient> {
        if let Some(h) = f.head.get("hello") {
            return Ok(FromClient::Hello(Box::new(Hello::parse(h))));
        }
        if f.head.get("detach").is_some() {
            return Ok(FromClient::Detach);
        }
        if f.head.get("kill").is_some() {
            return Ok(FromClient::Kill);
        }
        if f.head.get("len").is_some() {
            return Ok(FromClient::Payload(f.body));
        }
        bail!("read a client frame: {}", f.head)
    }

    pub(crate) fn payload(body: &[u8]) -> Vec<u8> {
        frame(json!({}), body)
    }

    pub(crate) fn detach() -> Vec<u8> {
        frame(json!({"detach": true}), &[])
    }

    pub(crate) fn kill() -> Vec<u8> {
        frame(json!({"kill": true}), &[])
    }
}

/// Daemon to wrapper.
pub(crate) enum FromDaemon {
    /// A claude stdout line, with the sequence number the cursor tracks.
    Event {
        seq: u64,
        epoch: String,
        body: Vec<u8>,
    },
    /// claude's stderr, out of band: no seq, it never moves the cursor.
    Stderr(Vec<u8>),
    /// The replay could not reach back far enough.
    Truncated(u64),
    Exit {
        code: Option<i32>,
        signal: Option<i32>,
    },
}

impl FromDaemon {
    pub(crate) fn of(f: Frame) -> Result<FromDaemon> {
        if let Some(seq) = f.head.get("seq").and_then(Value::as_u64) {
            let epoch = f
                .head
                .get("epoch")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            return Ok(FromDaemon::Event { seq, epoch, body: f.body });
        }
        if f.head.get("stderr").is_some() {
            return Ok(FromDaemon::Stderr(f.body));
        }
        if let Some(n) = f.head.get("dropped").and_then(Value::as_u64) {
            return Ok(FromDaemon::Truncated(n));
        }
        if let Some(e) = f.head.get("exit") {
            return Ok(FromDaemon::Exit {
                code: e.get("code").and_then(Value::as_i64).map(|c| c as i32),
                signal: e.get("signal").and_then(Value::as_i64).map(|c| c as i32),
            });
        }
        bail!("read a daemon frame: {}", f.head)
    }

    pub(crate) fn event(seq: u64, epoch: &str, body: &[u8]) -> Vec<u8> {
        frame(json!({"seq": seq, "epoch": epoch}), body)
    }

    pub(crate) fn stderr(body: &[u8]) -> Vec<u8> {
        frame(json!({"stderr": true}), body)
    }

    pub(crate) fn truncated(dropped: u64) -> Vec<u8> {
        frame(json!({"dropped": dropped}), &[])
    }

    pub(crate) fn exit(code: Option<i32>, signal: Option<i32>) -> Vec<u8> {
        frame(json!({"exit": {"code": code, "signal": signal}}), &[])
    }
}
