# Plato Agent

Plato Agent is the first application shell built on `platonic-core`.

**New here? Start with [docs/QUICKSTART.md](docs/QUICKSTART.md) — build, run, and test in five minutes.**

The bootstrap surface is intentionally small:

- `plato "question"` runs one bounded CLI invocation, streams live assistant text to stderr, and writes the run ledger to the default XDG SQLite path.
- `plato -c "follow-up"` continues the latest workspace session from the SQLite ledger.
- `plato --events <file> "question"` writes an explicit JSONL ledger.
- `plato replay <file>` validates and prints a deterministic JSONL readback without network calls or tool execution.
- `plato replay [--run <id>]` replays the default SQLite ledger; omitted `--run` selects the latest session.
- `plato replay --db[=<path>] [--run <id>]` replays an explicit SQLite ledger.

## Configuration

Config resolution order:

1. `--config <path>`
2. `$PLATO_CONFIG`
3. `./plato.toml`
4. `~/.config/plato/config.toml`
5. built-in defaults

Leading `~` expands in explicit config paths. Relative explicit paths resolve
against the workspace root. Built-in defaults use OpenRouter:

```toml
[provider]
kind = "open_router"
model = "~openai/gpt-latest"
api_key_env = "OPENROUTER_API_KEY"

[limits]
token_budget = 4000
max_output_tokens = 1024
max_turns = 8

[tools]
enabled = ["file.read", "file.list", "file.write", "file.edit", "shell.exec"]
```

OpenAI-compatible direct OpenAI config remains available:

```toml
[provider]
kind = "open_ai"
model = "gpt-5.5"
api_key_env = "OPENAI_API_KEY"
```

`file.read` and `file.list` are auto-allowed. `file.write`, `file.edit`, and
`shell.exec` require stdin approval and default to no. `shell.exec` runs from
the workspace root with a scrubbed child environment, no provider credentials,
bounded stdout/stderr, and a timeout.
Use `--yolo` to auto-approve enabled workspace-write tools that would otherwise
prompt. Yolo mode does not enable disabled or unknown tools, approve network
tools, permit deny-class effects such as external side effects or secret access,
approve `shell.exec`, or bypass workspace path checks.

## SQLite Ledgers

- Bare `plato "..."` writes to the default XDG state path.
- `plato -c "..."` continues the latest session from that store.
- `--db` also writes to the default XDG state path.
- `--db=<path>` writes to that SQLite file; relative paths resolve against the current workspace.
- Use `=` for explicit paths because `--db` also has a bare default form.
- Live assistant text, `run_id`, `ledger_path`, and replay hints print to stderr. Stdout remains only the final answer.
- Replay shows final assistant messages, not partial streaming deltas.
- Streamed runs request provider usage chunks; providers that omit usage still record zero usage.
- `plato replay` without arguments replays the latest session from the default XDG SQLite ledger.
- `plato replay --run <id>` replays a single run.
- `--events <file>` is the explicit JSONL export/debug path.
- If the workspace daemon lock is held, SQLite CLI run/replay paths fail closed instead of competing with the daemon-owned store.

Replay forms:

```bash
cargo run --bin plato -- replay
cargo run --bin plato -- replay --db
cargo run --bin plato -- replay --db=/tmp/plato-agent.db
cargo run --bin plato -- replay --db=/tmp/plato-agent.db --run run_123
```

## Daemon

`plato-agentd` is the local runtime daemon for session-facing clients such as
the future `plato-tui`. The runtime topology and verb set are defined in
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md#runtime-topology) and issue
[#11](https://github.com/referential-ai/plato-agent/issues/11).

Start it for a workspace:

```bash
cargo run --bin plato-agentd -- --workspace "$PWD"
```

On startup it prints:

```text
workspace_id: <workspace-id>
socket_path: <runtime-path>/agent.sock
ledger_path: <state-path>/agent.db
```

Default paths are keyed by the workspace id:

- socket: `${XDG_RUNTIME_DIR:-/tmp/plato-agent/$USER}/plato-agent/workspaces/<workspace-id>/agent.sock`
- lock: `${XDG_RUNTIME_DIR:-/tmp/plato-agent/$USER}/plato-agent/workspaces/<workspace-id>/agent.lock`
- ledger: `${XDG_STATE_HOME:-$HOME/.local/state}/plato-agent/workspaces/<workspace-id>/agent.db`

Runtime directories are restricted to `0700` and the daemon socket to `0600`.
A custom `--socket` parent is restricted to `0700` at startup.

The daemon holds the lock while it is active. SIGINT and SIGTERM trigger
a graceful shutdown: the daemon stops accepting new connections, then removes
the socket and lock before exiting. Do not remove a lock for a live daemon.
Live assistant deltas are transient `events.stream` events and are not written
to the ledger. After a `lagged` response, omitting `from_offset` resumes at the
current tip; `transcript.read` returns ledger-backed status and final answer.

Minimal NDJSON-over-Unix-socket check, using the `workspace_id` and
`socket_path` printed by the daemon:

```bash
WORKSPACE_ROOT="$PWD" \
WORKSPACE_ID="<workspace-id>" \
SOCKET_PATH="<socket-path>" \
python3 - <<'PY'
import json
import os
import socket

def send(file, request):
    file.write(json.dumps(request) + "\n")
    file.flush()
    print(file.readline(), end="")

with socket.socket(socket.AF_UNIX) as sock:
    sock.connect(os.environ["SOCKET_PATH"])
    file = sock.makefile("rw")
    send(file, {
        "v": 1,
        "id": "hello_1",
        "kind": "request",
        "method": "hello",
        "params": {
            "workspace_root": os.environ["WORKSPACE_ROOT"],
            "workspace_id": os.environ["WORKSPACE_ID"],
        },
    })
    send(file, {
        "v": 1,
        "id": "sessions_1",
        "kind": "request",
        "method": "sessions.list",
    })
PY
```

NDJSON `run.start` and `message.append` default to `wait: false`, returning a
`running` response immediately. Send `"wait": true` only when the connection can
block until the run finishes.

## Telegram Gateway

`plato-gateway-telegram` long-polls Telegram and forwards allowlisted text
messages to a running workspace daemon. Add the bot token variable name and
numeric owner user ids to `plato.toml`:

```toml
[gateway.telegram]
api_key_env = "TELEGRAM_BOT_TOKEN"
owner_user_ids = [123456789]
```

Run the gateway in an environment that contains the bot token but no provider
credentials:

```bash
unset OPENAI_API_KEY OPENROUTER_API_KEY
export TELEGRAM_BOT_TOKEN="$(cat /path/to/telegram-bot-token)"
cargo run --bin plato-gateway-telegram -- --workspace "$PWD"
```

Messages from other user ids are ignored. Each allowed chat or topic continues
one daemon session; final answers are recovered from the ledger after daemon
reconnects. Remote approval notifications are not part of this binary yet.

## TUI

`plato --tui` is the interactive local entrypoint. It attaches to the workspace
daemon if one is running, or starts an embedded daemon for the TUI session.
It renders a chat-first transcript surface with an intro, live activity,
status rule, composer, session picker, and approval modal.

```bash
cargo run --bin plato -- --tui --config plato.toml
```

`plato-tui` remains a terminal client for a manually started `plato-agentd`. It
does not spawn, supervise, restart, or stop the daemon, and it does not call
providers, execute tools, or write SQLite directly.
Assistant text appears live through daemon `events.stream`; replay remains
based on final ledger messages.
Session picker statuses are `running`, `finished`, `failed`, `canceled`, or
`interrupted`; `interrupted` means a daemon restart closed a previously running
session so it can be resumed.
On attach, the TUI selects the latest session by default; submitted messages
continue that session until `/new` clears the selection.

```bash
cargo run --bin plato-agentd -- --workspace "$PWD"
cargo run --bin plato-tui -- --workspace "$PWD"
```

Use `--socket <path>` when connecting to a non-default socket, `--config <path>`
to pass a config file to daemon-started runs, and `--run <run_id>` to open a
specific transcript.

Keys:

- `Enter`: submit the composer to the daemon. A session can have only one
  active run.
- `/sessions`: open the session picker. `Enter` resumes the focused session;
  `Esc` closes the picker.
- `/new`: clear the selected session so the next submitted message starts fresh.
- `g` / `d`: grant or deny the focused approval request.
- `Ctrl-C`: request `run.cancel` for the active run; a second `Ctrl-C` exits the
  TUI. Exiting the TUI does not stop the daemon.
- `r`: reconnect and reload daemon state.
- `q` or `Esc`: exit the TUI.

## Commands

```bash
cargo run --bin plato -- "read README.md and summarize it"
cargo run --bin plato -- -c "what did you just summarize?"
cargo run --bin plato -- --yolo "write local-proof.txt with hello from Plato"
cargo run --bin plato -- "run cargo test --locked and summarize the result"
cargo run --bin plato -- replay
cargo run --bin plato -- replay events.jsonl
cargo run --bin plato -- --db "read README.md and summarize it"
cargo run --bin plato -- --db=/tmp/plato-agent.db "read README.md and summarize it"
cargo run --bin plato -- replay --db
cargo run --bin plato -- replay --db=/tmp/plato-agent.db --run run_123
cargo run --bin plato -- --tui --config plato.toml
cargo run --bin plato-tui -- --workspace "$PWD"
```

## Dogfood Recipe

```bash
tmp=$(mktemp -d)
cat > "$tmp/plato.toml" <<'TOML'
[provider]
kind = "open_router"
model = "~openai/gpt-latest"
api_key_env = "OPENROUTER_API_KEY"
http_referer = "https://example.invalid"
app_title = "Plato Agent"

[limits]
token_budget = 4000
max_output_tokens = 512
max_turns = 8

[tools]
enabled = ["file.read", "file.list", "file.write", "file.edit"]
TOML

OPENROUTER_API_KEY="$(cat /path/to/your/openrouter-key)" \
  cargo run --bin plato -- --config "$tmp/plato.toml" --db="$tmp/agent.db" \
  "list the files in this workspace and summarize what you see"
```

## Boundary

`platonic-core` remains pure. Provider calls, local tools, approval prompts, ledger files, SQLite, daemon runtime, TUI, and connectors belong in this repo.
