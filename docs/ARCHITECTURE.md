# Plato Agent Architecture

This document copies the topology decision from the Platonic workspace issue that bootstrapped `referential-ai/plato-agent`.

## Runtime Topology

- One repo: `referential-ai/plato-agent`.
- End-state binaries:
  - `plato`: one-shot execution and offline replay.
  - `plato-agentd`: runtime daemon for sessions, event store, providers, tool execution, approvals, and connector-facing API.
  - `plato-tui`: terminal client for rendering and keyboard UX only.
- Bootstrap rule: create only `plato.rs` at repo creation. No stub binaries.
- Permanent invariant: `plato` one-shot and `plato replay` work without a daemon.
- Host-loop rule: `plato` and `plato-agentd` share one run-driving implementation. Binaries do not fork model/tool/policy event choreography.
- Fallback rule: provider fallback is per-run ledger evidence. The process that computes it is mechanics; unrecorded fallback is forbidden.
- TUI decision: `plato-tui` is a separate binary once it exists.
- Connector rule: connectors never own sessions, policy, approvals, provider fallback, or run semantics. Process placement is host mechanics; the semantic boundary is binding.

## Sequence

1. Build one-shot JSONL CLI.
2. Use it for real before spending on daemon/TUI.
3. Add SQLite as a concrete second persistence path inside the CLI.
4. Introduce a store trait only when the daemon creates a second caller or consumer that needs the abstraction.
5. Build daemon.
6. Build TUI.
7. Build connectors.

## Ledger Versioning

The CLI writes a `plato-agent` JSONL envelope around `platonic-core::RecordedEvent`:

```json
{ "v": 1, "record": { "seq": 0, "occurred_at_ms": 0, "event": { "event": "run_started" } } }
```

Bare `RecordedEvent` lines are not persisted by this app shell.
