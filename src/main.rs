#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
//! Process wrapper for the VS Code setting `claudeCode.claudeProcessWrapper`.
//!
//! `argv[1]` selects the mode: one of the subcommand names, or anything
//! else, which is the absolute path of the real claude binary (wrapper mode).

mod cli;
mod daemon;
mod protocol;
mod relay;
mod session;
mod state;

use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Result, bail};

const DEFAULT_IDLE_MINUTES: f64 = 720.0;

const USAGE: &str = "\
claude-relayd keeps the Claude Code process behind the VS Code panel alive across
window close, reload and disconnects, and reattaches the panel to it on reopen.

usage
  claude-relayd [global flags] <claude-binary> [claude args...]   what VS Code runs
  claude-relayd list | kill <id> | kill --all | logs <id>
  claude-relayd help | --version

setting it up
  \"claudeCode.claudeProcessWrapper\": \"/absolute/path/to/claude-relayd\"
  in VS Code user or machine settings. The setting is machine-scoped, so VS Code
  ignores it in a workspace .vscode/settings.json. Point it at a copy, not at
  target/release, so a rebuild cannot pull it out from under a running panel.

commands
  list        one line per live session: id tail, daemon pid, claude pid, uptime,
              attached clients, last seq, model, thinking level, cwd
  kill <id>   end one session: claude first, then its daemon
  kill --all  the same for every live session
  logs <id>   the daemon log, then claude's stderr log

  <id> is what list prints: the last group of the session id is enough, so
  `claude-relayd kill 7dd3a025017a` works. A short form matching two sessions is
  refused rather than guessed at.

global flags, before the claude binary path (env fallback in brackets; the flag wins)
  --idle-timeout <minutes>  end a session that has had no attached client for this
                            long. Default 720 (twelve hours), 0 disables, fractional
                            accepted [CLAUDE_RELAYD_IDLE_TIMEOUT]
  --passthrough             run the real binary directly: no daemon, no state, no
                            socket [CLAUDE_RELAYD_PASSTHROUGH=1]
  CLAUDE_RELAYD_HOME        state directory, default ~/.claude/relayd

when a session ends
  the conversation tab is closed        it ends
  the window is closed, reloaded, lost  it is kept, and the panel reattaches on reopen
  a panel was opened and never used     it ends with its window
  no attached client for the idle timeout  it ends
  claude-relayd kill                    it ends now

  Only a stream-json conversation becomes a session at all; every other claude
  command the extension runs goes straight through to the real binary.

if this tool ever misbehaves
  Set \"claudeCode.environmentVariables\": { \"CLAUDE_RELAYD_PASSTHROUGH\": \"1\" } and
  reload the window: every run then execs the real binary with stock behaviour,
  and nothing here is in the way. Run kill --all first: passthrough does not
  consult the lock, so a daemon that survived the reload would leave a second
  claude on that id. Clearing claudeCode.claudeProcessWrapper removes the wrapper
  entirely.";

/// Settings shared by every mode. Both carry an env fallback because the VS Code
/// setting passes a path and no arguments.
struct Options {
    idle_timeout: Duration,
    passthrough: bool,
}

fn minutes(raw: &str) -> Result<Duration> {
    let m = match raw.parse::<f64>() {
        Ok(m) if m >= 0.0 && m.is_finite() => m,
        _ => bail!("read {raw} as a number of minutes"),
    };
    Ok(Duration::from_secs_f64(m * 60.0))
}

/// Strips the global flags off the front of `args`, leaving the mode selector at
/// `args[0]`. The extension only ever passes an absolute path there, so a flag
/// and a claude binary can never be confused.
fn take_options(args: &mut Vec<String>) -> Result<Options> {
    let mut opts = Options {
        idle_timeout: match std::env::var("CLAUDE_RELAYD_IDLE_TIMEOUT") {
            Ok(v) => minutes(&v)?,
            Err(_) => Duration::from_secs_f64(DEFAULT_IDLE_MINUTES * 60.0),
        },
        passthrough: std::env::var("CLAUDE_RELAYD_PASSTHROUGH").is_ok_and(|v| v == "1"),
    };
    while let Some(arg) = args.first() {
        match arg.as_str() {
            "--passthrough" => {
                opts.passthrough = true;
                args.remove(0);
            }
            "--idle-timeout" => {
                let Some(v) = args.get(1) else {
                    bail!("read --idle-timeout: it needs a value in minutes");
                };
                opts.idle_timeout = minutes(v)?;
                args.drain(..2);
            }
            _ => break,
        }
    }
    Ok(opts)
}

fn run(mut args: Vec<String>) -> Result<u8> {
    // Only in first position: a claude argument must never be read as ours.
    if args.first().is_some_and(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return Ok(0);
    }
    if args.first().is_some_and(|a| a == "--version" || a == "-V") {
        println!("claude-relayd {}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }
    let opts = take_options(&mut args)?;
    let Some(mode) = args.first().cloned() else {
        eprintln!("{USAGE}");
        return Ok(2);
    };
    let rest = args.split_off(1);
    match mode.as_str() {
        "help" => {
            println!("{USAGE}");
            Ok(0)
        }
        "list" => cli::list(),
        "kill" => match rest.first().map(String::as_str) {
            Some("--all") => cli::kill_all(),
            Some(id) => cli::kill(id),
            None => bail!("run kill: it needs a session id or --all"),
        },
        "logs" => match rest.first() {
            Some(id) => cli::logs(id),
            None => bail!("run logs: it needs a session id"),
        },
        // Internal: spawned detached by the wrapper, not a user command.
        "daemon" => match rest
            .split_first()
            .and_then(|(id, r)| r.split_first().map(|(b, a)| (id, b, a)))
        {
            Some((id, bin, args)) => daemon::run(id, bin, args, opts.idle_timeout),
            None => bail!("run daemon: it needs a session id and a claude binary"),
        },
        // Wrapper mode: `mode` is the real claude binary, `rest` its arguments.
        _ => relay::run(&mode, &rest, opts.passthrough, opts.idle_timeout),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(args) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("claude-relayd: failed to {e:#}");
            ExitCode::from(2)
        }
    }
}

/// The value of a claude flag, in either `--name value` or `--name=value` form.
/// The wrapper and the daemon both need to read one out of the same argument
/// list, and two slightly different parsers would disagree eventually.
pub(crate) fn flag_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some(v) = a.strip_prefix(name).and_then(|r| r.strip_prefix('=')) {
            return Some(v);
        }
        if a == name {
            return it.next().map(String::as_str).filter(|v| !v.starts_with('-'));
        }
    }
    None
}
