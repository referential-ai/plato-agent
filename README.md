# Plato Agent

Plato Agent is the first application shell built on `platonic-core`.

The bootstrap surface is intentionally small:

- `plato "question"` runs one bounded CLI invocation and writes `events.jsonl`.
- `plato replay <file>` validates and prints a deterministic readback without network calls or tool execution.

## Configuration

Create `plato.toml` in the working directory:

```toml
[provider]
model = "claude-sonnet-5"
api_key_env = "ANTHROPIC_API_KEY"

[limits]
token_budget = 4000

[tools]
enabled = ["file.read", "file.write"]
```

`file.read` is auto-allowed. `file.write` requires stdin approval and defaults to no.

## Commands

```bash
cargo run --bin plato -- "read README.md and summarize it"
cargo run --bin plato -- replay events.jsonl
```

## Boundary

`platonic-core` remains pure. Provider calls, local tools, approval prompts, ledger files, SQLite, daemon runtime, TUI, and connectors belong in this repo.

