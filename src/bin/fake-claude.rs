//! Stands in for the real claude binary in the test suite.
//!
//! The contract it exists to prove: the relay never closes claude's stdin (this
//! exits non-zero if it does) and never signals claude when a panel goes away.
//! It reads the session id itself rather than sharing the wrapper's parser, so a
//! bug in that parser cannot hide behind the same bug here.

use std::io::{BufRead, Write};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};

/// stdout is shared with the heartbeat thread; one line must never interleave
/// with another. Poisoning is not fatal here: the byte stream stays intact.
fn emit(out: &Mutex<std::io::Stdout>, v: &Value) {
    let mut line = v.to_string().into_bytes();
    line.push(b'\n');
    let mut h = out.lock().unwrap_or_else(PoisonError::into_inner);
    if h.write_all(&line).is_err() || h.flush().is_err() {
        std::process::exit(5);
    }
}

/// Writes one line a single byte at a time, so a reader is guaranteed to see a
/// multi-byte UTF-8 character split across two reads.
fn drip(out: &Mutex<std::io::Stdout>, v: &Value) {
    let mut line = v.to_string().into_bytes();
    line.push(b'\n');
    let mut h = out.lock().unwrap_or_else(PoisonError::into_inner);
    for b in line {
        if h.write_all(&[b]).is_err() || h.flush().is_err() {
            std::process::exit(5);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn session_id(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        for name in ["--resume", "--session-id"] {
            if let Some(v) = a.strip_prefix(name).and_then(|r| r.strip_prefix('=')) {
                return Some(v.to_string());
            }
        }
        if matches!(a.as_str(), "--resume" | "--session-id" | "-r") {
            return it.next().filter(|v| !v.starts_with('-')).cloned();
        }
    }
    None
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let out: &'static Mutex<std::io::Stdout> = Box::leak(Box::new(Mutex::new(std::io::stdout())));

    // A panel that was opened and never used never gets an init line, and the
    // daemon has to be able to tell that from a conversation.
    if !args.iter().any(|a| a == "--no-init") {
        emit(
            out,
            &json!({
                "type": "system",
                "subtype": "init",
                "session_id": session_id(&args),
                "cwd": std::env::current_dir().unwrap_or_default(),
                "pid": std::process::id(),
                "args": args,
            }),
        );
    }

    // What the real claude answers a settings push with, and the only place the
    // model and the thinking level appear on the wire.
    emit(
        out,
        &json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": "fake-settings",
                "response": {"applied": {
                    "model": "fake-claude-model",
                    "effort": "high",
                    "advisor": null,
                    "ultracode": true,
                }},
            },
        }),
    );

    std::thread::spawn(move || {
        for n in 1u64.. {
            std::thread::sleep(Duration::from_millis(500));
            emit(out, &json!({"type": "heartbeat", "n": n}));
        }
    });

    let mut stdin = std::io::stdin().lock();
    let mut n = 0u64;
    loop {
        let mut line = Vec::new();
        match stdin.read_until(b'\n', &mut line) {
            // Losing stdin is a failure: the relay must keep it open across panel restarts.
            Ok(0) => {
                eprintln!("fake-claude: stdin closed - the relay broke the contract");
                std::process::exit(3);
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("fake-claude: stdin read: {e}");
                std::process::exit(4);
            }
        }
        while line.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
            line.pop();
        }
        if line.is_empty() {
            continue;
        }
        // Bytes, never lossy: a relay that corrupted UTF-8 must fail loudly here.
        let Ok(text) = String::from_utf8(line) else {
            eprintln!("fake-claude: stdin carried invalid utf-8");
            std::process::exit(4);
        };
        let msg: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        match msg.get("type").and_then(Value::as_str) {
            Some("quit") => std::process::exit(0),
            Some("exit") => {
                let code = msg.get("code").and_then(Value::as_i64).unwrap_or(0);
                std::process::exit(code as i32);
            }
            Some("err") => {
                eprintln!("fake-claude: DEBUG diagnostic line");
            }
            Some("utf8") => drip(out, &json!({"type": "assistant", "text": "привет 🚀 конец"})),
            // Emits many events with no further input, so a test can build a backlog
            // while no client is attached.
            Some("burst") => {
                let count = msg.get("n").and_then(Value::as_u64).unwrap_or(10);
                let after = msg.get("after_ms").and_then(Value::as_u64).unwrap_or(0);
                emit(out, &json!({"type": "assistant", "burst_scheduled": count}));
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(after));
                    for i in 1..=count {
                        emit(out, &json!({"type": "assistant", "burst": i}));
                    }
                });
            }
            Some("big") => {
                let bytes = msg.get("bytes").and_then(Value::as_u64).unwrap_or(100_000) as usize;
                emit(out, &json!({"type": "assistant", "big": "ы".repeat(bytes / 2)}));
            }
            _ => {
                n += 1;
                emit(out, &json!({"type": "assistant", "echo": text, "n": n}));
            }
        }
    }
}
