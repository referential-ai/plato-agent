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

- `Cargo.toml` defines `plato-agent`, the default `plato_agent` library, and
  four `plato*` binaries. The desktop crate imports that library by path.
- `src/config.rs` resolves only legacy environment, workspace, and user config.
  Explicit and environment paths are authorized; auto-discovered workspace
  config cannot set `provider.api_key_env` or `provider.base_url`.
- `src/paths.rs` gives every client one `plato-agent` state/IPC namespace and
  `src/daemon/lock.rs` enforces one workspace writer.
- `src/app.rs` writes `agent_id = "plato"`. Replay is schema-driven, and SQLite
  sessions order runs without requiring one agent id for the whole session.
- Windows installer control authenticates the exact bundled sidecar. Desktop
  state and upgrade proofs use `ai.referential.plato` and legacy identities.

## Desired Outcome

New users invoke `platonic`; existing command/config users retain working
`plato*` entrypoints and existing state. One workspace still means one daemon,
ledger, and desktop identity. Rust dependency compatibility is bounded
separately below.

## Scope And Anchor Boundary

One app-repo implementation PR adds canonical package, library, command,
config, and ledger identities plus executable aliases. Its anchor proof is a
fresh source install and a frozen 0.1.0 workspace fixture exercised in both
compatibility directions. It performs no publish or installed-state migration.

## Design

**D1 - Package, Rust API, and commands.** Source package/library become
`platonic-agent`/`platonic_agent`. Primary binaries are `platonic`,
`platonic-agentd`, `platonic-tui`, and `platonic-gateway-discord`; their four
`plato*` counterparts remain `0.x` executable aliases. Each pair shares one
runner with invocation-correct help and diagnostics.

The `0.x` compatibility promise in #216 covers commands, config, state, IPC,
desktop identity, and ledgers, not Rust dependency names or source API. The
published `plato-agent` 0.1.0 artifact stays available and untouched, but this
source tree exposes no `plato_agent` library target or compatibility crate and
does not promise a drop-in Rust dependency upgrade. Any later
compatibility/deprecation package release belongs to #41.

**D2 - Config.** Precedence is `--config`, `PLATONIC_CONFIG`, `PLATO_CONFIG`,
workspace `platonic.toml`, workspace `plato.toml`, canonical user config,
legacy user config, then defaults. User paths are
`~/.config/{platonic,plato}/config.toml` on Unix and
`%APPDATA%\{platonic,plato}\config.toml` on Windows. Discovery never rewrites
config.

### Config Selection Contract

- A supplied `--config` is selected without an existence check. An empty or
  missing path produces its read/path error and never falls through.
- An absent or zero-length config environment value is unavailable. Any other
  value, including whitespace, is a path and is selected without an existence
  check.
- Workspace and user candidates participate only when the file exists.
- The first selected candidate wins. Its read, parse, or validation error stops
  resolution; lower candidates are not consulted.
- An empty selected TOML file is valid and yields built-in defaults.
- Authorization follows the selection source, not the filename. Explicit,
  environment, and user files may set `provider.api_key_env` and
  `provider.base_url`; either auto-discovered workspace file must reject them.

`src/config.rs` must use a table-driven resolver/load matrix covering:

| Case | Required result |
| --- | --- |
| both config env vars absent or empty | continue to workspace, user, or defaults |
| empty canonical env plus valid legacy env | select legacy env |
| missing explicit path plus valid lower candidates | return explicit read error |
| nonempty missing canonical env plus valid legacy env | return canonical read error |
| both canonical and legacy env values valid | select canonical env |
| both workspace files valid | select canonical workspace file |
| both user files valid | select canonical user file |
| malformed explicit/canonical env/workspace/user plus valid lower peer | return selected parse error |
| canonical or legacy workspace provider credential override | reject without fallback |
| canonical or legacy user provider credential override | allow; canonical wins a collision |
| no candidate exists | use built-in defaults |

The same injected selection table runs on all platforms. Separate Unix and
Windows tests prove their canonical and legacy user paths.

**D3 - Runtime identity and cross-family proof.** Both command families keep
the current `plato-agent` state/runtime directory, workspace-id algorithm,
socket/pipe, lock, SQLite database, installer gate, and desktop mutex. No
second namespace is created.

"Legacy state" means a frozen 0.1.0 config/ledger fixture at those opaque
paths. "Canonical state" means canonical config plus new `platonic` records at
the same opaque paths. The required matrix is:

| State/setup | Executables | Required assertion |
| --- | --- | --- |
| legacy state | `platonic-agentd`, `platonic`, `platonic-tui`, `platonic-gateway-discord` | daemon hello and TUI snapshot use the legacy endpoint/database; CLI replays after shutdown; fake-platform gateway reaches the real daemon |
| canonical state created by `platonic-agentd` | `plato`, `plato-tui`, `plato-gateway-discord` | legacy clients use the canonical daemon/state; CLI replays after shutdown; TUI and fake-platform gateway attach while live |
| live `platonic-agentd` | concurrent `plato-agentd` for the same workspace | second daemon fails on the shared lock without changing endpoint/database; first daemon remains responsive |

The namespace and concurrent-lock assertions run on Unix and Windows. Gateway
proof uses a fake Discord endpoint and real daemon transport; no live Discord
or provider is required.

**D4 - Desktop identity.** Visible copy may say Platonic under #215. Tauri
`productName = "Plato"`, identifier `ai.referential.plato`, app-data, main
bundle executable, Windows uninstall identity, and packaged `plato-agentd`
remain unchanged. Other daemon targets share the endpoint/lock; installer
control still rejects a non-exact executable.

**D5 - Ledger and mixed-session identity.** New runs record
`agent_id = "platonic"`. JSONL ledger version remains `1`; SQLite schema version
remains `2`. Replay/readback accept old `plato` and new `platonic` records.

An existing session may contain a `plato` run followed by a `platonic` run.
Replay and continuation process both in session order; a new continuation
writes `platonic`. Agent identity remains per `RunStarted`; no session-wide
normalization or new identifier allowlist is added. Reading/replaying never
rewrites either identity.

Proof must show both preservation modes:

- Offline replay leaves the frozen 0.1.0 fixture byte-for-byte unchanged and
  keeps ledger/schema versions unchanged.
- Appending a canonical run to a copied legacy SQLite session leaves every old
  row's `seq`, `occurred_at_ms`, `v`, and serialized `event_json` unchanged,
  writes `platonic` for the new run, and replays both runs in session order.

**D6 - Distribution boundary.** Help/docs lead with canonical commands and
state the aliases once, but do not claim the new crate or repository is
published. #216 performs no publish, release, repository, asset, or state
migration.

## Constraints

- Preserve one run-driving implementation and current fail-closed boundaries.
- Keep the implementation source-only and reversible without migrating data.
- Do not add a Rust compatibility crate or agent-id validation policy.

## Non-Goals

- No `platonic-core`, run, protocol, policy, tool, or ledger-schema change.
- No Rust API compatibility for the `plato_agent` library name in this source
  tree.
- No alias removal, old-state rewrite, new state/IPC namespace, or installed
  desktop-identity migration.
- No publish, package reservation, repository rename, release, or
  administration; #41 owns distribution.
- No separate recipe: the bounded file order below is sufficient for one
  non-destructive source PR.

## Forbidden Operations

- Do not fall through an invalid selected config or trust a non-exact sidecar.
- Do not publish, rename, remove aliases, rewrite old records, or alter
  historical artifacts.

## File-Level Implementation Order

1. Extract the current four runners from `src/bin/plato.rs`,
   `src/bin/plato-agentd.rs`, `src/bin/plato-tui.rs`, and
   `src/bin/plato-gateway-discord.rs` into
   `src/bin/shared/{cli,daemon,tui,discord_gateway}.rs`; leave the four legacy
   files as thin shims and prove legacy help first.
2. Update `Cargo.toml`, `Cargo.lock`, `desktop/src-tauri/Cargo.toml`, and
   `desktop/src-tauri/Cargo.lock`; add four canonical shims, verify the public
   exports in `src/lib.rs`, and update `plato_agent` imports in
   `tests/{unix_runtime_fallback.rs,windows_daemon.rs}` and
   `desktop/src-tauri/src/{lib.rs,unix_proof.rs,windows_proof.rs,windows_installer_proof.rs}`.
3. Implement D2 in `src/config.rs`; update dependent expectations in
   `src/app.rs` and `src/discord_gateway.rs` only where messages or forwarding
   reflect the selected source.
4. Implement D5 in `src/app.rs`; add the 0.1.0 fixture and preservation tests
   under `tests/fixtures/`, `src/ledger.rs`, and `src/replay.rs`.
5. Add the D3 process matrix in `tests/runtime_compatibility.rs`, extend
   `tests/windows_daemon.rs`, and add focused TUI/gateway assertions in
   `src/tui/app.rs` and `src/discord_gateway.rs`. `src/paths.rs` and
   `src/daemon/{lock.rs,control.rs,installer_gate.rs}` remain identity owners
   and change only if a test exposes a compatibility defect.
6. Verify opaque identities remain unchanged in
   `desktop/src-tauri/src/{lifecycle.rs,unix_lifecycle.rs}` and
   `desktop/scripts/stage-{linux,windows}-sidecar.mjs`,
   `desktop/src-tauri/{tauri.conf.json,tauri.linux-package.conf.json,tauri.windows-package.conf.json}`,
   and `desktop/src-tauri/windows/installer-hooks.nsh`; they must still
   package/control `plato-agentd` unchanged.
7. Update `README.md`, `docs/QUICKSTART.md`, `docs/ARCHITECTURE.md`, and
   `.github/workflows/{ci.yml,windows.yml,desktop.yml}` last so commands and
   mandatory proof match the final targets.

## Acceptance Criteria

- Cargo metadata exposes `platonic-agent`, `platonic_agent`, and exactly eight
  binaries, with no `plato_agent` library target or compatibility crate.
- The config table and Unix/Windows user-path tests pass exactly as D2 states.
- Every D3 matrix row passes, including the concurrent single-writer case.
- D5 fixture, new-record, and mixed-session assertions pass without a version
  or old-record mutation.
- Desktop upgrade retains its state, IPC, installer, and sidecar identities and
  fails closed on a different daemon executable.
- Canonical help/docs lead; aliases remain usable and distribution is not
  claimed.

## Proof Expectations

All items below are mandatory before the implementation PR may merge:

- A machine assertion over `cargo metadata --no-deps --format-version 1` for
  the package, library, eight binaries, and absent legacy library target.
- Locked focused tests for the D2 config table, D3 cross-family/lock matrix,
  and D5 fixture/mixed-session contract on their required platforms.
- `cargo fmt --check`, `cargo test --locked`, and
  `cargo clippy --locked --all-targets -- -D warnings` for the root and locked
  desktop Rust/web checks.
- A temporary-root `cargo install --path . --locked` transcript listing eight
  executables and invoking both command families without network services.
- Required PR checks green: Rust, Web, Windows daemon, Linux shell, Windows
  lifecycle, and Windows unsigned installer.
- Linux AppImage contents/checksum and Windows install/upgrade/uninstall proof
  showing the opaque desktop and packaged `plato-agentd` identities unchanged.

Publishing, registry, repository-redirect, release, and public-site readbacks
are not #216 proof and remain blocked on #41.

## Risks

- A renamed path splits workspace truth; path invariance is a hard gate.
- Trusting another daemon weakens installer authentication; exact-file checks
  remain fail closed.
- Falling through canonical config may select legacy credentials; selected
  source errors stop.
- Rust consumers may assume executable aliases imply a `plato_agent` library
  alias; docs and metadata proof must state the exclusion.

## Ownership Map

- Package/Rust API/entrypoints: Cargo manifests, shared runners, bin shims.
- Config/ledger: `src/config.rs`, `src/app.rs`, ledger/replay tests and fixture.
- Runtime/desktop: paths, daemon lock/control, Tauri lifecycle/bundling,
  installer hooks, workflows, and platform proofs.
- Public copy: #215; distribution and administration: workspace #41.

## Open Questions And Drift

None known for #216. Reconcile before implementation if #215 changes Tauri
`productName` or #41 changes the package sequence. Rust compatibility beyond
the published 0.1.0 artifact, alias removal, or state/IPC migration requires a
later issue and evidence.

## Rollout, Security, And Privacy

#216 lands source and tests only. Config and installer authentication remain
fail closed; fixtures and tests contain no secrets. Rollback is a source revert
because no user data is migrated.

## Contract Neighborhood

- Source artifacts: workspace #38, app #215/#216, workspace #41, PR #218,
  current code/docs, and the Hermes PM findings.
- Planned touch surface: Cargo/lib/bin targets, config selection, ledger/session
  identity, preserved paths/lock/control, desktop packaging, CI, tests, docs.
- Upstream producers: argv, environment, config files, old/new ledger writers,
  Tauri staging, installer hooks.
- Downstream consumers: Clap, daemon clients, TUI, Discord gateway, replay,
  Cargo/desktop builds, installers, AppImage, CI.
- Proof surfaces: metadata, table-driven unit tests, cross-process platform
  tests, frozen fixture/hash, source install, required CI and package proofs.
- N1 [pass]: Rust `0.x` compatibility excludes source/library dependency names;
  the old published artifact remains distribution history.
- N2 [pass]: config absence, collision, failure, and authorization semantics are
  executable and fail closed.
- N3 [pass]: both runtime directions and one concurrent writer are required.
- N4 [pass]: file order names each implementation/proof owner without adding a
  second plan.
- N5 [pass]: mixed sessions accept both ids without schema or old-row rewrite.
- C1 [resolved]: canonical helpers do not replace the packaged sidecar.
- C2 [resolved]: display copy does not rename opaque installed identities.
- Boundary decision: ready for one #216 implementation PR after design
  adoption; no implementation or distribution action is authorized by this PR.

## Verifiable End Condition

Canonical commands and aliases pass the config, runtime/lock, and replay
matrices against one opaque runtime namespace; Cargo exposes only the canonical
Rust library; every mandatory proof passes; and no package publication,
repository/release mutation, or user-data migration occurred.

## Goal Handoff

`/goal` implement app #216 from this design in one source PR, follow the
file-level order, satisfy every mandatory pre-merge proof, reconcile linked
docs/issues after proof review, and stop before publishing, renaming the
repository, releasing, merging, or performing administration.
