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

## Commands

```bash
cargo run --bin plato -- "read README.md and summarize it"
cargo run --bin plato -- --yolo "write local-proof.txt with hello from Plato"
cargo run --bin plato -- replay events.jsonl
cargo run --bin plato -- --db "read README.md and summarize it"
cargo run --bin plato -- --db=/tmp/plato-agent.db "read README.md and summarize it"
cargo run --bin plato -- replay --db
cargo run --bin plato -- replay --db=/tmp/plato-agent.db --run run_123
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
