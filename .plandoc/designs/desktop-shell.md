---
title: Desktop Shell
issue: https://github.com/referential-ai/plato-agent/issues/139
---

# Plato Desktop Shell (Tauri + Web UI over plato-agentd)

Revision 3 — **Draft**, not adopted. Rev 2 folded PR #140 closure review R1–R9 (direction confirmed there: Tauri client, shared daemon ownership, in-repo home, Windows named pipes). Rev 3 adds cross-platform scope per human direction: the app ships on Windows, macOS, and Linux; Windows remains the first shipped target. No phase goes `Ready for dev` against a Draft.

## Authority
- Human direction 2026-07-12: Codex-desktop-style app; Tauri accepted; web UI in the stack already in use (Svelte 5 / Tailwind 4 / shadcn-svelte); distribution must feel natural to a Windows user; C# path declined.
- Human direction 2026-07-12 (rev 3): the app must work on macOS, Linux, and Windows — all three are shipping targets; Windows first.
- Parent: `docs/ARCHITECTURE.md` runtime topology — daemon owns sessions, providers, tools, approvals; clients render only; connector rule binding (ARCHITECTURE.md:10-22).
- Lead closure review 2026-07-12 (PR #140, R1–R9) is design input for this revision.
- Externals: Codex desktop (Electron shell over the Rust Codex CLI) is the UX benchmark and the same shell+sidecar shape. Windows Reactor (WinUI 3 in Rust) is experimental — rejected for v1.

## Source Grounding (main @ 3f02adb)
- Daemon protocol v1, NDJSON envelopes; methods `hello`, `run.start`, `message.append`, `events.stream`, `approval.decide`, `run.cancel`, `sessions.list`, `transcript.read` (src/daemon/handlers.rs:39-54, src/daemon/protocol.rs:5). `run.start`/`message.append` default `wait: false` (ARCHITECTURE.md:20).
- `TranscriptReadResult.transcript` is preformatted CLI replay text; the TUI recognizes entries by display prefixes (src/daemon/protocol.rs:227-242, src/daemon/handlers.rs:495-548, src/replay.rs:34-97, src/tui/render.rs:355-407). No typed history exists on the wire today (R1).
- The event buffer is transient and capped at 256; `lagged` + omitted `from_offset` resumes after the current tip; pending-approval truth lives only in daemon memory/transient events; `transcript.read` has no pending snapshot (src/daemon/runtime.rs:17,42,93-122,151-195; src/daemon/handlers.rs:277-327) (R2).
- Client precedent: `plato --tui` already attaches-or-starts and stops only its embedded daemon; only raw `plato-tui` is never-spawn (src/bin/plato.rs:126-180,230-249). Daemon bind/lock are atomic arbiters; stale locks are never reclaimed (src/daemon/server.rs:65-75, src/daemon/lock.rs:72-115,119-130,200-213) (R4).
- Transport and process model are Unix-only (std UDS, signals, process groups, HOME/XDG paths); the daemon does not build natively on Windows today (src/daemon/client.rs:15-27, src/daemon/server.rs:9-10, src/bin/plato-agentd.rs:7,47, src/tools.rs:317-326, src/paths.rs:83-102, src/config.rs:274-309) (R9).
- `plato replay` fails closed while the daemon lock is held (README.md:75) (R9).
- The Unix-only daemon code is `cfg(unix)`-shaped, and the runtime/state paths are XDG-with-fallback (README.md Daemon; src/paths.rs:83-102), so macOS is expected to build and run on the existing UDS path unverified today; phase 6 verifies. Windows is the only true port.
- Live assistant deltas are transient `events.stream` events; ledger `model_responded` stays replay truth (ARCHITECTURE.md:21). Thread-per-connection already serves multiple clients (server.rs:94-100).

## Design

**D1 — Client, not runtime.** The shell is a pure daemon client. Presentation state only: no provider calls, no SQLite, no run/policy/approval semantics. Anything semantic the shell needs becomes a daemon issue first, never a shell workaround.

**D2 — Protocol: v1 envelope and shared methods; no shell-private extensions.** Result shapes are not frozen: the extension path is daemon-owned, additive, backward-compatible fields/capabilities, each landed by its own daemon issue. Two are already required and gate the phases that consume them (R1, R2):
- a typed transcript payload (structured user/assistant/tool history; clients never parse the preformatted `transcript` string);
- a pending-approval snapshot readable after lag/reconnect, so a paused run always re-renders its approval modal.

**D3 — Stack and privileged boundary.** Tauri 2 shell (Rust) + Svelte 5 + SvelteKit static adapter (SPA) + Tailwind 4 + shadcn-svelte; one shared UI — Fluent-flavored on Windows, platform-default window chrome on macOS/Linux; local assets only, strict CSP. The security boundary is the Tauri capability model, not CSP (R3): the Rust shell exclusively owns `DaemonClient`, workspace validation, and the spawned child handle; the webview receives typed commands/events only and gets no generic shell, filesystem, raw-socket, or arbitrary-path capability. Phase-1 bridge tests prove this boundary.

**D4 — Daemon lifecycle: attach-or-spawn with an explicit race and lifetime contract (R4).** This extends the existing `plato --tui` embedded precedent, not a new divergence. Contract:
- Attach first: validate via `hello` (workspace, protocol version, required capabilities). Never pre-check the lock.
- On connect failure: spawn `plato-agentd`; the daemon's atomic bind/lock arbitrates concurrent starters. On spawn conflict, bounded-retry `hello`; if it never validates, fail closed with the socket/lock paths in the error.
- Stop only the exact child it spawned, and only on shell exit when the daemon is idle (no active or approval-paused runs) and no other client is attached; otherwise detach and leave it running — runs are daemon-owned and closing a UI must not kill work.
- Spawned-child crash: show disconnected state and re-enter the attach-or-spawn path on user action; no automatic restart loop. The shell never deletes locks; if a stranded stale lock blocks respawn, fail closed and surface the lock path (reclamation is a non-goal, below).
- One-shot SQLite CLI paths remain fail-closed while a daemon holds the workspace (unchanged).

**D5 — Transport matrix (R5).** Unix (Linux and macOS) keeps std UDS untouched — no dependency change on Unix; macOS uses the same UDS and XDG-fallback paths as Linux. Windows-only: named pipes via the `interprocess` crate as the spiked default, behind a hard proof gate: an explicit current-user DACL is required (the Windows default DACL grants Everyone/anonymous read), with other-user rejection and remote-access rejection tests. Ownership model decided now, names refined in-phase: pipe name derived from workspace-id; ledger and lock under per-user `LocalAppData`; user config under `RoamingAppData`.

**D6 — Repo home (R6).** In `plato-agent`: one Tauri shell crate plus a `desktop/` web UI. Desktop checks run as separate CI jobs so the existing root Cargo job stays intact; native Windows CI lands with phase 3. CI isolation is a phase-1 acceptance item. No future-repo clause.

**D7 — Distribution: one coherent route per platform (R7).** Windows first: a single directly distributed **signed NSIS installer** containing the pinned `plato-agentd.exe` sidecar; signatures verified on the installer, app executable, uninstaller, and sidecar. Then, phase 6: macOS as a **signed and notarized DMG** (Developer ID; Gatekeeper blocks anything less — the hard gate for shipping mac), and Linux as an **AppImage** (no signing gate). Each platform ships only when its own gate is met; unsigned builds everywhere are dev-only artifacts and are never distributed. MSI, Microsoft Store, and auto-updater are deferred; each returns as its own issue (Store requires a signed offline self-updating installer, so it cannot precede the updater).

## Sequencing
Each phase is its own issue and PR(s); implementation starts only on a `Ready for dev` phase issue. The two D2 protocol additions are their own daemon issues, cut with the phase issues; phases 1–2 gate on them.

1. **Shell bootstrap + protocol adequacy gate (Linux).** Tauri window renders `sessions.list` and typed history for a selected run **without parsing the `transcript` string** (consumes the typed-transcript daemon issue). Capability-boundary bridge tests (D3). Live comparison via exact-run `transcript.read` against a running daemon; offline `plato replay` parity checked only after daemon shutdown (replay fails closed under the lock). TUI attached simultaneously. CI: separate desktop job; root Cargo job untouched.
2. **Chat parity + state isolation (R2, R8).** Composer (`run.start`/`message.append`, `wait: false`), keyed delta folding, approval modal (`approval.decide`), cancel, session picker. Rendering and actions bind to one selected session and exact run; switching sessions invalidates stale in-flight pages and never cancels or splices another client's run; terminal recovery uses exact-run `transcript.read`, never latest-session readback. Acceptance proofs: full-page catch-up; folding without duplication; lag/resync; reconnect while approval-paused recovering the modal from the pending-approval snapshot; approval resolved by another client; all six `RunStateName` values; `CancelRequested` before terminal; same-session overload; two concurrently active sessions with no cross-session content or decisions.
3. **Windows daemon parity (R9).** More than transport: native Windows build and tests; `LocalAppData`/`RoamingAppData` path model (D5); lock and pipe ACLs with the D5 proof gate; shutdown wake without Unix signals; `shell.exec` cancel/timeout on Windows. Native Windows CI starts here.
4. **Windows shell + sidecar lifecycle on a provisioned dev-VM (R4, R9).** Attach/spawn race proofs (concurrent starters), exact-child graceful stop, app/child crash policy, second-client lifetime, CLI fail-closed preserved. Not a clean-VM proof — packaging does not exist yet.
5. **Windows packaging + distribution (R7).** Signed NSIS installer with pinned sidecar; clean-VM install/uninstall and cold launch with an explicitly pre-provisioned workspace/config and user-scoped provider credential; SmartScreen behavior recorded; signature checks on all four artifacts.
6. **macOS + Linux distribution.** macOS: verify the daemon builds and passes tests natively (expected near-parity via the Unix path), WKWebView UI acceptance, signed + notarized DMG with sidecar, cold-launch proof on a clean machine/VM. Linux: AppImage with sidecar, webkit2gtk already proven by phases 1–2, cold-launch proof on a distro without the dev toolchain. macOS CI runner joins here.

Phases 1–2 deliver a usable Linux-dev desktop client early; 3–5 make it a Windows product; 6 makes it cross-platform.

## Non-Goals
- No `platonic-core` changes; no gateway or TUI behavior changes.
- One workspace per window; no multi-workspace switcher in v1.
- No MSI, no Store, no auto-updater in v1 (deferred per D7). macOS/Linux distribution is phase 6, after the Windows v1 — not dropped, sequenced.
- AppImage only on Linux (no Flatpak/Snap/deb/rpm in v1); no macOS-idiomatic path migration (the XDG-fallback paths stay).
- No automatic stale-lock reclamation (fail closed and surface the path; revisit as a daemon issue if phase 4 proofs make it bite).
- No model management, board, or GitHub integration surfaces.

## Open Questions (bounded)
- Exact pipe/path names and the DACL SDDL string: fixed inside phase 3 under the D5 ownership model and proof gate.
- Typed-transcript and pending-approval-snapshot wire shapes: fixed in their daemon issues under D2's additive rule.
- macOS-idiomatic paths (`~/Library/...`): deliberately not adopted (XDG fallbacks work); revisit only if real macOS friction appears.

## Acceptance (for this design going Active)
- Lead re-review confirms R1–R9 are folded (this revision).
- Phase issues 1–6 plus the two D2 daemon protocol issues cut on this repo, linked to umbrella #139, each with scope, acceptance, and proof.
- Board: umbrella and phase issues on Project #1; nothing enters `In Progress` while current WIP (#129, #132) holds.

## Proof
Design PR review of this document. No code ships with this PR.
