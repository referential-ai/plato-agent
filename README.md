# Plato Agent

Plato Agent is the first application shell built on `platonic-core`.

The bootstrap surface is intentionally small:

- `plato "question"` runs one bounded CLI invocation and writes `events.jsonl`.
- `plato replay <file>` validates and prints a deterministic readback without network calls or tool execution.

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
enabled = ["file.read", "file.write"]
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

`file.read` is auto-allowed. `file.write` requires stdin approval and defaults to no.

## Commands

```bash
cargo run --bin plato -- "read README.md and summarize it"
cargo run --bin plato -- replay events.jsonl
```

## Boundary

`platonic-core` remains pure. Provider calls, local tools, approval prompts, ledger files, SQLite, daemon runtime, TUI, and connectors belong in this repo.
