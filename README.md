# claude-relayd

Keeps the Claude Code process behind the VS Code panel alive across window close, reload and
disconnects, and reattaches the panel to the still-running process when the session is reopened.

The extension spawns `<wrapper> <path of the real claude binary> <claude args...>` and speaks
newline-delimited JSON over the wrapper's stdin and stdout. The wrapper relays those bytes to a
detached per-session daemon that owns the real `claude`.

```
VS Code panel <-- stdin/stdout --> claude-relayd <-- unix socket --> daemon <-- pipes --> claude
   dies with the window            dies with the panel              detached, survives    survives
```

Linux only: it is built on unix sockets, `setsid`, `flock` and signals.

## Install

```sh
cargo build --release
install -m 755 target/release/claude-relayd ~/.local/bin/claude-relayd
```

Then set, in VS Code **user or machine** settings:

```json
"claudeCode.claudeProcessWrapper": "/absolute/path/to/claude-relayd"
```

Point it at that copy rather than at `target/release`, so a rebuild cannot pull the binary out from
under a running panel. The extension declares the setting `"scope": "machine"`, so VS Code ignores it
in a workspace's `.vscode/settings.json` — putting it there looks like it worked and does nothing. On
a remote host the file is `~/.vscode-server/data/Machine/settings.json`.

## Commands

```sh
claude-relayd list        # id tail, daemon pid, claude pid, uptime, clients, last seq, model, thinking, cwd
claude-relayd kill <id>   # end one session: claude first, then its daemon
claude-relayd kill --all  # the same for every live session
claude-relayd logs <id>   # the daemon log, then claude's stderr log
claude-relayd help        # this guide, for somebody who has only the binary in front of them
```

`list` prints only the last group of the session id, and `kill` and `logs` take that short form back:

```
$ claude-relayd list
ID            DAEMON   CLAUDE   UPTIME CLIENTS SEQ  MODEL          THINKING CWD
7dd3a025017a  1114358  1114359  1h20m  1       2042 claude-opus-5  xhigh    /home/me/project

$ claude-relayd kill 7dd3a025017a
```

An id that is not a UUID is printed whole. `kill` and `logs` exit non-zero on an unknown id and on a
short form that matches two sessions; `kill --all` on a session that would not end.

## Flags and environment

The VS Code setting passes a path and no arguments, so every flag has an environment fallback, and a
flag always wins over its fallback. Flags go before the claude binary path or the subcommand, which
means that inside a wrapped panel the environment variable is the only way in.

| flag | environment | default | what it does |
|---|---|---|---|
| `--idle-timeout <minutes>` | `CLAUDE_RELAYD_IDLE_TIMEOUT` | `720` | ends a session that has had no attached client for this long. `0` disables it; fractional values are accepted |
| `--passthrough` | `CLAUDE_RELAYD_PASSTHROUGH=1` | off | run the real binary directly |
| | `CLAUDE_RELAYD_HOME` | `~/.claude/relayd` | state directory, created 0700 with every file 0600 |

## When a session ends

| | |
|---|---|
| the conversation tab is closed | it ends |
| the window is closed, reloaded or lost | it is kept, and the panel reattaches on reopen |
| a panel was opened and never used | it ends with its window |
| no attached client for the idle timeout | it ends |
| `claude-relayd kill` | it ends now |

A client going away never touches claude: its stdin is never closed and it is never signalled. Only a
run carrying both `--input-format stream-json` and `--output-format stream-json` becomes a session at
all; every other claude command the extension runs goes straight through to the real binary.

## Getting stock behaviour back

Passthrough replaces the wrapper process with the real binary: inherited stdio, no daemon, no socket,
nothing written to the state directory, and the exit status is the binary's own, signals included.
Either of these turns it on, and the environment variable is the one a panel can reach:

```json
"claudeCode.environmentVariables": { "CLAUDE_RELAYD_PASSTHROUGH": "1" }
```

```sh
claude-relayd --passthrough <claude-binary> [claude args...]
```

Run `claude-relayd kill --all` before the reload that turns it on. A daemon surviving the reload is
the whole point of this tool, and passthrough does not consult the lock, so the stock claude it execs
would be a second one on that session id.

Clearing `claudeCode.claudeProcessWrapper` removes the wrapper entirely.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The suite drives the real binary against a fake claude that exits non-zero the moment its stdin is
closed, which is what proves the invariant this tool exists for. It never touches a real state
directory, a real claude binary or a real session id.

`docs/SPEC.md` carries the rest: the measured facts about the extension, the control protocol, the
state directory, the policies and the live-test plan.
