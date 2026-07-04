# Plato Agent Agent Guide

`plato-agent` is the application/runtime shell built on `platonic-core`.

## Repo Boundary

- This repo owns app IO: CLI, config, provider calls, local tools, approvals, persistence, daemon, TUI, and connectors.
- `platonic-core` owns pure typed harness primitives only.
- Do not move provider clients, tool implementations, stores, daemon code, TUI code, or connector code into `platonic-core`.

## Workflow

- GitHub Issues are the scope and acceptance contract.
- GitHub PRs are the implementation and proof surface.
- Link every PR to its issue and include verification commands or manual proof.
- A generic “proceed” is not merge authority. CI green is necessary, not sufficient; merge requires explicit human “merge” or “land” instruction.
- Do not use local TODOs, wiki pages, tmux pane names, or chat history as active-work authority.
- Do not start implementation unless a GitHub issue or direct human task has clear scope, non-goals, acceptance, target surface, and proof.

## Runtime Topology

- Bootstrap with `src/bin/plato.rs` only.
- `plato` one-shot execution and `plato replay` must work without a daemon permanently.
- `plato` and future `plato-agentd` must share one run-driving implementation. Do not duplicate model/tool/policy event choreography.
- Provider fallback changes run outcome and must be recorded in the run ledger. Unrecorded fallback is forbidden.
- Add `plato-agentd` only when a second client needs a persistent runtime.
- Add `plato-tui` only after a daemon/client API exists.
- Connectors must not own sessions, policy, approvals, provider fallback, or run semantics.

## Verification

```bash
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```
