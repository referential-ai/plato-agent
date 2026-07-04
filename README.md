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

## Commands

```bash
cargo run --bin plato -- "read README.md and summarize it"
cargo run --bin plato -- --yolo "write local-proof.txt with hello from Plato"
cargo run --bin plato -- replay events.jsonl
cargo run --bin plato -- --db "read README.md and summarize it"
cargo run --bin plato -- replay --db
```

## Boundary

`platonic-core` remains pure. Provider calls, local tools, approval prompts, ledger files, SQLite, daemon runtime, TUI, and connectors belong in this repo.
