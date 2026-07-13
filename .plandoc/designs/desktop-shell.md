---
title: Desktop Shell
issue: https://github.com/referential-ai/plato-agent/issues/139
---

# Plato Desktop Shell (Tauri + Web UI over plato-agentd)

Revision 1 — **Draft**, not adopted. Review gate: lead/human review folds findings in; no phase goes `Ready for dev` against a Draft.

## Authority
- Human direction 2026-07-12: Codex-desktop-style app; Tauri accepted; web UI in the stack already in use (Svelte 5 / Tailwind 4 / shadcn-svelte); distribution must feel natural to a Windows user; C# path declined.
- Parent: `docs/ARCHITECTURE.md` runtime topology — daemon owns sessions, providers, tools, approvals; clients render only; connector rule binding (ARCHITECTURE.md:10-22).
- Verified externals (2026-07): Codex desktop = Electron shell over the Rust Codex CLI (Windows release 2026-03) — the UX benchmark and the same shell+sidecar shape. Windows Reactor (WinUI 3 in Rust, microsoft/windows-rs, 2026-05) is experimental single-window — rejected for v1. Azure Artifact Signing open to US/CA individual developers (~$10/mo, since 2026-04); SmartScreen reputation still accumulates.

## Source Grounding (main @ 3f02adb)
- Daemon protocol v1, NDJSON envelopes; methods `hello`, `run.start`, `message.append`, `events.stream`, `approval.decide`, `run.cancel`, `sessions.list`, `transcript.read` (src/daemon/handlers.rs:39-54, src/daemon/protocol.rs:5). `run.start`/`message.append` default `wait: false` (ARCHITECTURE.md:20).
- Transport is synchronous std Unix sockets only (src/daemon/server.rs:10,75; src/daemon/client.rs:15-27); thread-per-connection accept loop already serves multiple clients (server.rs:94-100); socket/lock/ledger paths are XDG, keyed by workspace-id, 0700/0600 (README.md Daemon; ARCHITECTURE.md:18).
- Client precedent: `plato-tui` attaches, never spawns/supervises the daemon, never calls providers or touches SQLite (README.md TUI).
- Live assistant deltas are transient `events.stream` events; ledger `model_responded` stays replay truth (ARCHITECTURE.md:21).

## Design

**D1 — Client, not runtime.** The shell is a pure daemon client with exactly the TUI's verb surface. Presentation state only: no provider calls, no SQLite, no run/policy/approval semantics. Anything semantic the shell seems to need becomes a daemon issue first, never a shell workaround.

**D2 — Protocol reuse.** NDJSON v1 verbatim over a local socket. No shell-private protocol extensions.

**D3 — Stack.** Tauri 2 shell (Rust) + Svelte 5 + SvelteKit static adapter (SPA) + Tailwind 4 + shadcn-svelte. Fluent-flavored theme: Segoe UI Variable, Windows 11 radii, OS dark-mode sync, Mica via Tauri window effects where available. Local assets only, strict CSP, no remote content.

**D4 — Daemon lifecycle (deliberate divergence from plato-tui).** Double-click UX needs a running daemon: the shell may spawn `plato-agentd` for the selected workspace when the lock is free, adopts an already-running one, and stops only a daemon it spawned. It never removes locks or kills daemons it did not start. plato-tui's never-spawn rule is unchanged for the TUI.

**D5 — Windows transport.** The daemon gains a named-pipe listener equivalent to the UDS listener; Unix keeps std UDS untouched. Default: `interprocess` crate local sockets (sync API fits the threaded daemon; UDS on Unix, named pipes on Windows); hand-rolled windows-rs pipes only if that spike fails. Pipe ACLs must match the current-user-only 0700/0600 posture; the Windows path model replacing XDG is decided inside the phase issue.

**D6 — Repo home.** In `plato-agent`: one Tauri shell crate plus a `desktop/` web UI. New repo only if Node/Tauri CI weight demonstrably hurts this repo — boundary ladder requires a trigger for promotion (ARCHITECTURE.md:24-33).

**D7 — Distribution.** tauri-bundler NSIS/MSI installer bundling `plato-agentd.exe` as sidecar; WebView2 bootstrap enabled. Dev/dogfood builds unsigned; public distribution signs via Azure Artifact Signing (US individual) or ships through the Store — decided in phase 5. Updater deferred until signing exists.

## Sequencing
Each phase is its own issue and PR(s); implementation starts only on a `Ready for dev` phase issue.

1. **Shell bootstrap (Linux-first).** Tauri window renders `sessions.list` + `transcript.read` against a live daemon over UDS. Proof: transcript parity with `plato replay`; screenshot; TUI attached simultaneously.
2. **Chat parity.** Composer (`run.start`/`message.append`, `wait: false`), live `events.stream` deltas and status, approval modal (`approval.decide`), cancel, session picker. Proof: fake-daemon tests on the bridge plus live smoke in a scratch workspace.
3. **Daemon Windows transport (D5).** Proof: Windows VM smoke — hello, run, replay; ACL check.
4. **Windows shell + sidecar lifecycle (D4).** Spawn/adopt/stop rules, single-instance guard. Proof: cold double-click on a clean VM reaches a finished run.
5. **Packaging + distribution (D7).** Proof: clean-VM install/uninstall; SmartScreen behavior recorded; sidecar version pinning checked.

Phases 1–2 deliver a usable Linux-dev desktop client early; 3–5 make it a Windows product.

## Non-Goals
- No `platonic-core` changes; no gateway or TUI behavior changes.
- One workspace per window; no multi-workspace switcher in v1.
- No auto-update before signing; no Store decision now; no macOS/Linux packaging in v1.
- No model management, board, or GitHub integration surfaces.

## Open Questions (bounded)
- `interprocess` vs hand-rolled named pipes: resolved by a spike inside phase 3's issue; D5 sets the default.
- Windows path model (runtime/state equivalents of XDG): phase 3's issue.
- Multi-client concurrency (TUI + desktop attached together) is expected to work today (server.rs:94-100); phase 1 proof must include it.

## Acceptance (for this design going Active)
- Lead/human review of D1–D7 with findings folded in.
- Phase issues 1–5 cut on this repo, linked to umbrella #139, each with scope, acceptance, and proof.
- Board: umbrella and phase issues on Project #1; nothing enters `In Progress` while current WIP (#129, #132) holds.

## Proof
Design PR review of this document. No code ships with this PR.
