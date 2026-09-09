# claude-relayd — implementation spec (first release)

Status: approved by the owner 2026-09-07. This is the brief an implementer starts from; `CLAUDE.md`
and `.claude/rules/*` are the rules it must obey. A Node.js prototype with a passing self-test lives
at `~/.claude/tools/claude-relay/` — read its `README.md` and code for behaviour, never copy files or
scope from it. Where this document and the prototype differ, this document wins.

## 1. Measured facts about the host (do not re-derive, do not guess)

- The VS Code extension (2.1.263) with `claudeCode.claudeProcessWrapper` set spawns
  `<wrapper> <absolute path of the real claude binary> <claude args...>` and speaks newline-delimited
  JSON (stream-json) on the wrapper's stdin/stdout. Observed args: `--output-format stream-json
  --verbose --input-format stream-json --max-thinking-tokens <n> --permission-prompt-tool <name>
  --resume=<uuid> --setting-sources=user,project,local --permission-mode <mode>
  --allow-dangerously-skip-permissions --debug --debug-to-stderr --enable-auth-status --no-chrome
  --replay-user-messages`. Order may vary; `--resume` is absent for a brand-new conversation.
- The setting passes a path and no arguments, so every flag needs an env fallback.
- In a wrapped setup the extension starts conversations in Manual permission mode unless
  `claudeCode.initialPermissionMode` is set (official docs, "VS Code" page, setting table).
- `--session-id <uuid>` is documented in the bundled binary's `--help`, and a live panel accepts it
  next to the panel's full argument set.
- The extension puts the wrapper in front of **every** claude invocation, not only the panel's:
  `resolveClaudeBinary` returns it as `pathToClaudeCodeExecutable` with the real binary as
  `executableArgs`, and `runClaudeCommand`, `runClaudeCommandRaw` and `getChromeMcpServerConfig` all
  build their command lines from that. Those short commands reject `--session-id`; measured
  2026-09-07, sixteen of them died on it in one afternoon. Only a run carrying both
  `--input-format stream-json` and `--output-format stream-json` is a conversation; everything else is
  passthrough.
- The model never appears in the panel's argv. Changing it in the panel calls `writeUserSettingsAndPush`,
  which rewrites `~/.claude/settings.json` and, in the same callback, sends the **running** claude
  `{"type":"control_request","request":{"subtype":"apply_flag_settings","settings":{"model":…}}}` on its
  stdin; the same call carries permission mode and effort. claude applies it at a turn boundary. The
  SDK's `set_model` request exists in the bundle but has no caller in 2.1.263. Nothing here needs the
  process restarted, and the wrapper relays these lines like any other payload — measured live on
  2026-09-07: one claude process dispatched to `claude-fable-5-1` at 03:01 and to `claude-opus-5[1m]`
  from 03:03.
- A window reload is survived, measured in a real panel on 2026-09-07: `daemon.log` said `the extension
  host is gone, keeping the session`, the daemon and claude pids were unchanged across the gap, events
  kept accumulating while `clients` was 0, and the new wrapper took the cursor and was replayed from it.
- claude writes the transcript itself to `~/.claude/projects/<dir>/<uuid>.jsonl`; the panel loads
  history from that file on open. The live channel carries only new events.
- The extension host is the wrapper's parent process. Closing a conversation tab kills only that
  tab's wrapper while the host lives; window close, reload and crash take the host down.
- The setting has a history of regressions upstream (issues #56648, #10506): expect an extension
  update to break the contract one day. Passthrough must always restore stock behaviour.

## 2. Architecture

```
VS Code panel <-- stdin/stdout --> claude-relayd (wrapper) <-- unix socket --> claude-relayd daemon <-- pipes --> claude
   dies with the window            dies with the panel                    detached, survives           survives
```

One binary. `argv[1] ∈ {daemon, list, kill, logs}` selects a subcommand; anything else is wrapper mode
and `argv[1]` is the claude binary path.

### State directory — `$CLAUDE_RELAYD_HOME`, default `~/.claude/relayd`, mode 0700, files 0600

| file | content |
|---|---|
| `<id>.sock` | attach socket; when the path would exceed `sun_path`, `$TMPDIR/claude-relayd-<uid>/<hash>.sock`, in a directory created 0700 |
| `<id>.lock` | held with `flock` for the daemon's life, naming its pid + process start time — the one-claude-per-id guarantee |
| `<id>.spawn.json` | cwd + env captured by the wrapper for the daemon's spawn of claude (holds secrets: 0600, never logged, removed as soon as claude is running) |
| `<id>.events.jsonl` | the same frame the socket carries: a header line plus the raw claude line; truncated per run, rotated at 64 MiB, removed when the session ends |
| `<id>.cursor` | `{epoch, seq}` — last seq the panel actually received (written after the stdout write completed) |
| `<id>.cursor.owner` | pid + start time of the one wrapper allowed to move the cursor |
| `<id>.status.json` | epoch, claude pid, started_ms, cwd, clients, seq, model, effort, ultracode — the facts `list` cannot read from the lock file or the filename |
| `<id>.stderr.log` | claude's stderr (also forwarded live to the attached wrapper's stderr) |
| `<id>.daemon.log` | the daemon's own diagnostics |
| `<id>.panel.log` | panel lines with an unknown `type`, and everything received after the panel's stdin EOF began — the evidence for the live test |

Write every state file to a temp name in the same directory and `rename` it into place.

### Control protocol (the only lines this tool parses; everything else is opaque bytes)

Every frame is a JSON header line, optionally followed by exactly `len` bytes of body. The body is
relayed byte for byte and never re-encoded, which is what `.claude/rules/rust.md` demands and what
keeps a 64 KiB line cheap; the approved brief carried the payload as a JSON string instead.

Of claude's own output the daemon reads two lines and forwards everything untouched: the `system/init`
it remembers for replay, and the settings answer carrying `"applied":{"model","effort","ultracode"}`,
whose values go into `status.json` for `list`. Neither is in argv and both change mid-session, so
there is nowhere else to read them from. The gate is the `"applied"` key, not the position of any key.

Wrapper → daemon, first frame after connect:
`{"hello":{"since":<seq|null>,"epoch":"<epoch>"|null,"replay":"init+since"|"init","parent":"<pid> <start>"|null,"permission_mode":"<mode from --permission-mode>"|null}}`

Wrapper → daemon: `{"len":n}` plus the raw panel line, `{"detach":true}` on a graceful exit, and
`{"kill":true}` from `kill <id>`.

Daemon → wrapper: the remembered `system/init` line (per replay mode), then events with
`seq > since` from the same epoch, then the live stream, each as `{"seq":n,"epoch":"..","len":k}`
plus the raw claude line. Out of band: `{"stderr":true,"len":k}` plus the raw stderr line, and
`{"dropped":n}` when the in-memory ring (2000 events / 8 MiB) could not serve the whole backlog.
Finally `{"exit":{"code":<n>|null,"signal":<n>|null}}` when claude exits. Every daemon run has a
fresh `epoch`; a cursor from another epoch is treated as `since: null` (init only).

The wrapper never forwards control envelopes to the panel. It exits with claude's code, `128+signum`
when claude was signalled, and **70** with a stderr message when the socket died without
`relay-exit` — a lost daemon must never look like a finished conversation.

## 3. Policies

1. **One claude per session id.** `flock(LOCK_EX|LOCK_NB)` on `<id>.lock`, which the kernel releases
   however the holder dies; the pid + start time in the file is what `list` reports and what refuses a
   holder from a build that locked by content alone. Never check a lock for staleness and then unlink
   it: two daemons reading one dead holder both unlink, and the second unlinks a live claim. A wrapper
   that finds a live lock attaches; if it cannot attach it exits non-zero — never passthrough over a
   live lock.
2. **A client going away never touches claude.** No stdin close, no signal, no exit. The daemon
   ignores SIGHUP/SIGINT/SIGTERM.
3. **Tab close ends the session; host death keeps it — decided by the daemon.** The hello carries the
   wrapper's parent (pid + start time). When the last client disconnects, the daemon waits a grace
   period (10 s) and checks that parent: alive → the user closed the tab → terminate claude and exit;
   gone, or pid recycled (start time differs), or unreadable → keep the session. A reconnect during
   the grace cancels the check. Exception, measured 2026-09-07: the extension keeps a live wrapper for
   every conversation it has opened, including the blank one a window starts on, and switching topics
   does not end the old one. So a session whose stream never carried a `system/init` is terminated at
   that same check rather than kept — it has no history, no turn in flight and nothing to replay, and
   keeping it would hold a claude for the whole idle timeout after every window close.
4. **Idle timeout.** `--idle-timeout <minutes>` / `CLAUDE_RELAYD_IDLE_TIMEOUT`, default 720, `0`
   disables, fractional accepted. Counted only while no client is attached; a reattach resets it.
   Expiry terminates claude (SIGTERM, SIGKILL after 5 s) and the daemon.
5. **Termination** = SIGTERM to claude, SIGKILL after 5 s, reap, then remove the socket, the status,
   spawn and event files and the cursor; keep `stderr.log` and `daemon.log`. The lock and the cursor
   owner are each removed by the process that holds them, never by anybody else.
6. **Permission mode follows the panel, as far as it safely can.** The panel passes
   `--permission-mode <mode>` on every spawn, including a reattach, while the running claude keeps
   whatever mode it last had. **Decision, 2026-09-07: the daemon logs the mismatch to `daemon.log`
   and sends nothing.** The exact control request was verified against the shipped extension
   (`{"request_id":..,"type":"control_request","request":{"subtype":"set_permission_mode","mode":..}}`),
   but claude would answer it with a `control_response` carrying a `request_id` the panel never
   issued, and what the panel does with that cannot be known without a live panel. Sending it is a
   change to make after the live test in section 6, not before.
7. **Passthrough** (`CLAUDE_RELAYD_PASSTHROUGH=1`, or a leading `--passthrough`): exec the real
   binary with inherited stdio — no daemon, no socket, no state written, exit code transparent through
   signals. Also automatic when the session cannot be keyed (`--resume`/`-r` with no value,
   `--continue`, `--from-pr`, `--teleport`, `--fork-session`) or the daemon fails to start with no
   live lock present.
8. **Replay is bounded** by constants, not flags (ring 2000 events / 8 MiB, log 64 MiB).

## 4. CLI

```
claude-relayd <claude-binary> <claude args...>      # wrapper mode (what the extension runs)
claude-relayd list                                  # id tail · daemon pid · claude pid · uptime · clients · last seq · model · thinking · cwd
claude-relayd kill <id>                             # relay-kill over the socket; non-zero on unknown id
claude-relayd kill --all                            # the same for every session with a live daemon
claude-relayd logs <id>                             # daemon.log then stderr.log; non-zero on unknown id
claude-relayd daemon <id> <claude-binary> <args...> # internal; spawned detached by the wrapper
```

Global flags come before the mode selector, so an absolute claude path can never be read as one:
`--idle-timeout <minutes>` (env `CLAUDE_RELAYD_IDLE_TIMEOUT`), `--passthrough`
(env `CLAUDE_RELAYD_PASSTHROUGH=1`), `CLAUDE_RELAYD_HOME`. Flag beats env. `--help`/`--version`.

## 5. Delivery order and commit milestones

Each milestone is one commit (`type(scope): subject`, see `.claude/rules/git.md`), all four gates
green, tests written with the code, README updated when the CLI changes:

1. `chore(repo): crate skeleton` — `Cargo.toml` (edition 2024, pinned deps, `[lints.clippy]
   unwrap_used/expect_used = "deny"`), `.gitignore`, `src/main.rs` dispatch, `README.md` skeleton,
   GitHub Actions running exactly the four DoD commands on push.
2. `feat(tests): fake claude` — `src/bin/fake-claude.rs`: prints init with the session id from
   `--resume`/`--session-id`, echoes stdin lines as `{"type":"assistant","echo":..,"n":k}`, emits
   `{"type":"heartbeat","n":k}` every 500 ms, writes to stderr, prints its pid, **exits non-zero when
   its stdin closes**, exits on SIGTERM or `{"type":"quit"}`.
3. `feat(daemon): detached session owner` — lock, spawn from `spawn.json`, event log + ring, socket,
   hello/replay, exit envelope, kill, idle timeout, tab-close policy.
4. `feat(relay): wrapper mode` — session key + `--session-id` injection, daemon start, attach, byte
   relay both ways, cursor + owner, exit codes, passthrough.
5. `feat(cli): list, kill, logs`.
6. `test(tests): scenario suite` — every scenario in `.claude/rules/testing.md`, one test each.
7. `docs(readme): install, flags, escape hatch, live-test notes`.

Stop and ask the owner at the points `scope.md` names; do not proceed past a milestone with a red gate.

## 6. Live test with the owner (after milestone 7, throwaway session, owner present)

1. Build release; set in VS Code (machine scope): `claudeCode.claudeProcessWrapper` = absolute path of
   the binary; `claudeCode.initialPermissionMode` = the owner's usual mode.
2. Open a NEW conversation in the panel (exercises `--session-id` injection). Start a long task
   (a background `sleep 600` via Bash is enough).
3. Window close → `claude-relayd list` shows the session alive, `<id>.panel.log` shows what the panel
   sent at close. Reopen VS Code and the session: the task is still running; check the panel for
   duplicated messages. Decide `replay` (`init+since` vs `init`) from what is seen.
4. Close the conversation TAB with the window open → the session must end within the grace period.
5. Reload Window → the session must survive. Switch the permission mode in the panel, close and reopen
   → the panel and claude agree on the mode.
6. Only when all five pass: keep the setting for daily use. Roll back at any time with
   `CLAUDE_RELAYD_PASSTHROUGH=1` in `claudeCode.environmentVariables` or by clearing the setting.

## 7. Out of scope

See `.claude/rules/scope.md`. In particular: no config file, no TUI, no service unit, no second
process per id, and no alternative tools mentioned anywhere in this repo.
