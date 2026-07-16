# Changelog

## Unreleased

- Present current product and UI copy as **Platonic by Referential.ai** while
  preserving existing technical identifiers.

## 0.1.0 - 2026-07-15

First release. Local CLI, daemon, TUI, desktop shell, and Discord gateway over
the replayable `platonic-core` ledger, with explicit tool policy and local
approvals.

Known limitation: `shell.exec` is bounded and approval-gated but does not yet
run in an OS or container sandbox ([#81](https://github.com/referential-ai/plato-agent/issues/81)).
