# Plato Agent

Plato Agent is the first application shell built on `platonic-core`.

The bootstrap surface is intentionally small:

- `plato "question"` runs one bounded CLI invocation and writes `events.jsonl`.
- `plato --db "question"` writes the run ledger to the default XDG SQLite path.
- `plato replay <file>` validates and prints a deterministic JSONL readback without network calls or tool execution.
- `plato replay --db[=<path>] [--run <id>]` replays a SQLite run; omitted `--run` selects the latest run.

## Configuration

Create `plato.toml` in the working directory:

```toml
[provider]
kind = "open_ai"
model = "gpt-5.5"
api_key_env = "OPENAI_API_KEY"

[limits]
token_budget = 4000
max_output_tokens = 1024

[tools]
enabled = ["file.read", "file.list", "file.write"]
```

For OpenRouter:

```toml
[provider]
kind = "open_router"
model = "~openai/gpt-latest"
api_key_env = "OPENROUTER_API_KEY"
http_referer = "https://example.invalid"
app_title = "Plato Agent"
```

`file.read` and `file.list` are auto-allowed. `file.write` requires stdin approval and defaults to no.
Use `--yolo` to auto-approve enabled tools that would otherwise prompt. Yolo
mode does not enable disabled or unknown tools, permit deny-class effects such
as external side effects or secret access, or bypass workspace path checks.

## SQLite Ledgers

- `--db` writes to the default XDG state path.
- `--db=<path>` writes to that SQLite file; relative paths resolve against the current workspace.
- Use `=` for explicit paths because `--db` also has a bare default form.
- Successful `--db` runs print `run_id`, `ledger_path`, and a replay command to stderr. Stdout remains only the final answer.

Replay forms:

```bash
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

The daemon holds the lock while it is active. SIGINT and SIGTERM trigger
a graceful shutdown: the daemon stops accepting new connections, then removes
the socket and lock before exiting. Do not remove a lock for a live daemon.

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

## TUI

`plato-tui` is a terminal client for a manually started `plato-agentd`. It does
not spawn, supervise, restart, or stop the daemon, and it does not call
providers, execute tools, or write SQLite directly.

```bash
cargo run --bin plato-agentd -- --workspace "$PWD"
cargo run --bin plato-tui -- --workspace "$PWD"
```

Use `--socket <path>` when connecting to a non-default socket, `--config <path>`
to pass a config file to daemon-started runs, and `--run <run_id>` to open a
specific transcript.

Keys:

- `Enter`: submit the composer to the daemon (`run.start`, then `message.append`
  while a run is active).
- `g` / `d`: grant or deny the focused approval request.
- `Ctrl-C`: request `run.cancel` for the active run; a second `Ctrl-C` exits the
  TUI. Exiting the TUI does not stop the daemon.
- `r`: reconnect and reload daemon state.
- `q` or `Esc`: exit the TUI.

## Commands

```bash
cargo run --bin plato -- "read README.md and summarize it"
cargo run --bin plato -- --yolo "write local-proof.txt with hello from Plato"
cargo run --bin plato -- replay events.jsonl
cargo run --bin plato -- --db "read README.md and summarize it"
cargo run --bin plato -- --db=/tmp/plato-agent.db "read README.md and summarize it"
cargo run --bin plato -- replay --db
cargo run --bin plato -- replay --db=/tmp/plato-agent.db --run run_123
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

[tools]
enabled = ["file.read", "file.list", "file.write"]
TOML

OPENROUTER_API_KEY="$(cat /path/to/your/openrouter-key)" \
  cargo run --bin plato -- --config "$tmp/plato.toml" --db="$tmp/agent.db" \
  "list the files in this workspace and summarize what you see"
```

## Boundary

`platonic-core` remains pure. Provider calls, local tools, approval prompts, ledger files, SQLite, daemon runtime, TUI, and connectors belong in this repo.
