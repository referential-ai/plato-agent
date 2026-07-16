---
title: Platonic Runtime Compatibility
issue: https://github.com/referential-ai/plato-agent/issues/216
---

# Platonic Runtime Compatibility

## Authority

- [Workspace #38](https://github.com/referential-ai/platonic-workspace/issues/38)
  defines the names and `0.x` compatibility policy.
- [App #216](https://github.com/referential-ai/plato-agent/issues/216) owns this
  source migration.
- [Workspace #41](https://github.com/referential-ai/platonic-workspace/issues/41)
  separately owns publishing, repository/release changes, and administration.

## Source Grounding

- `Cargo.toml` defines `plato-agent`, `plato_agent`, and four `plato*` binaries.
- `src/config.rs` resolves only legacy environment, workspace, and user config.
- `src/paths.rs` gives all clients one `plato-agent` state/IPC namespace.
- `src/app.rs` writes `agent_id = "plato"`; replay accepts recorded agent ids.
- Windows installer control authenticates the exact bundled sidecar. Desktop
  state and upgrade proofs use `ai.referential.plato` and legacy identities.

## Desired Outcome

New users invoke `platonic`; existing users retain working `plato*` commands,
config, ledgers, and installed state. One workspace must still mean one daemon,
ledger, and desktop identity.

## Scope And Anchor Boundary

One app-repo source PR adds canonical package, library, command, config, and
ledger identities plus aliases. Its anchor proof is a fresh source install and
an existing 0.1.0 workspace before and after a desktop upgrade attempt.

## Design

**D1 - Package and commands.** Source package/library become
`platonic-agent`/`platonic_agent`. Primary binaries are `platonic`,
`platonic-agentd`, `platonic-tui`, and `platonic-gateway-discord`; their four
`plato*` counterparts remain `0.x` aliases. Each pair shares one implementation,
with invocation-correct help and diagnostics. The published `plato-agent` 0.1.0
crate stays untouched; the new package exposes only the canonical library.

**D2 - Config.** Resolve `--config`, `PLATONIC_CONFIG`, `PLATO_CONFIG`, workspace
`platonic.toml`, workspace `plato.toml`, canonical user config, legacy user
config, then defaults. User paths are `~/.config/{platonic,plato}/config.toml`
on Unix and `%APPDATA%\{platonic,plato}\config.toml` on Windows. The first
present source wins; read or parse failure stops. Both workspace files retain
provider-credential restrictions. Discovery never rewrites config.

**D3 - Runtime identity.** Both command families keep the current `plato-agent`
state/runtime directory, workspace-id algorithm, socket/pipe, lock, SQLite
database, installer gate, and desktop mutex. No second namespace is created.

**D4 - Desktop identity.** Visible copy may say Platonic under #215. Tauri
`productName = "Plato"`, identifier `ai.referential.plato`, app-data, main
bundle executable, Windows uninstall identity, and packaged `plato-agentd`
remain unchanged. Other daemon targets share the endpoint/lock; installer
control still rejects a non-exact executable.

**D5 - Ledger identity.** New runs record `agent_id = "platonic"`. Schema
versions do not change. Replay/readback accept old `plato` and new `platonic`
records without rewriting them.

**D6 - Distribution boundary.** Help/docs lead with canonical commands and state
the aliases once, but do not claim the new crate or repository is published.
#216 performs no publish, release, repository, asset, or state migration.

## Constraints

- Preserve one run-driving implementation and current fail-closed boundaries.
- Keep the change source-only and reversible without migrating user data.

## Non-Goals

- No `platonic-core`, run, protocol, policy, tool, or ledger-schema change.
- No alias removal, old-state rewrite, new state/IPC namespace, or installed
  desktop-identity migration.
- No recipe: this is one non-destructive source PR; #41 owns distribution.

## Forbidden Operations

- Do not fall through an invalid selected config or trust a non-exact sidecar.
- Do not publish, rename, remove aliases, or alter historical artifacts.

## Acceptance Criteria

- A source install exposes four canonical binaries and four working aliases.
- Config precedence and selected-source errors match D2 on Unix and Windows.
- Both command families attach to the same daemon and state.
- New ledgers use `platonic`; a 0.1.0 ledger replays unchanged.
- Desktop upgrade retains its state, IPC, installer, and sidecar identities and
  fails closed on a different daemon executable.

## Proof Expectations

- `cargo metadata --no-deps` proves the package/library and eight binaries.
- Tests cover paired help, config precedence, path/endpoint invariance, new
  ledger id, and old-ledger replay.
- Locked format, test, clippy, desktop web/Rust, AppImage, and Windows installer
  checks pass.
- A source-install transcript invokes both command families and config names.

## Risks

- A renamed path splits workspace truth; path invariance is a hard gate.
- Trusting another daemon weakens installer authentication; exact-file checks
  remain fail closed.
- Falling through canonical config may select legacy credentials; errors stop.

## Ownership Map

- Cargo targets/aliases: manifest, bin entrypoints, and lockfiles.
- Config/ledger: `src/config.rs`, `src/app.rs`, ledger/replay tests.
- Runtime/desktop identity: paths, daemon control, Tauri lifecycle/bundling,
  installer hooks, workflows, and proofs.
- Public copy: #215; distribution and administration: workspace #41.

## Open Questions And Drift

None known for #216. Reconcile before implementation if #215 changes Tauri
`productName` or #41 changes the package sequence. Alias removal or state/IPC
migration always requires a later issue and new evidence.

## Rollout, Security, And Privacy

#216 lands source and tests only. Config and installer authentication remain
fail closed; tests contain no secret values. Rollback is a source revert because
no user data is migrated.

## Contract Neighborhood

- Source: workspace #38, app #215/#216, workspace #41, current code/docs.
- Touches: Cargo, bins, config, ledger, paths/control, desktop, CI, tests, docs.
- Producers/consumers: argv, environment, config files, Tauri/installer, Clap,
  daemon clients, replay, Cargo install, and CI.
- N1 [pass]: one legacy runtime namespace preserves single-writer truth.
- N2 [pass]: config precedence retains credential restrictions.
- N3 [pass]: ledger naming needs no core or schema change.
- C1 [resolved]: canonical helpers do not replace the packaged sidecar.
- C2 [resolved]: display copy does not rename opaque installed identities.
- Boundary decision: ready for one #216 source PR after design adoption.

## Verifiable End Condition

Canonical commands and aliases work against one runtime namespace, old data is
usable, all proof passes, and no package/repository/release mutation occurred.

## Goal Handoff

`/goal` implement app #216 from this design in one source PR, prove these
boundaries, reconcile linked docs/issues, and stop before publishing, renaming
the repository, releasing, or performing administration.
