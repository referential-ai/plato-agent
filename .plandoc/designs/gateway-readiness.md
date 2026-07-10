---
title: Gateway Readiness
issue: https://github.com/referential-ai/plato-agent/issues/93
---

# Gateway Readiness (Spine Slice 6)

## Authority
- Human direction 2026-07-09: proceed with the gateway design track.
- Issue #93 is the scope contract.
- Spine (`hermes-light-product-spine.md`): gateways are thin daemon clients, after the local spine; remote channels may notify or deny approvals, never grant.
- MVP decisions (`mvp-decisions.md`): remote channels must not become a grant surface.
- `docs/ARCHITECTURE.md`: connectors never own sessions, policy, approvals, provider fallback, or run semantics.

## Source Grounding
Checked:
- `src/daemon/protocol.rs`: envelope `v: 1` with strict equality reject; `deny_unknown_fields` on envelope and all param structs; typed error codes including `lagged`, `unsupported_method`, `unsupported_version`; `hello` returns `daemon_version` + `capabilities` (method list); `events.stream` is cursor polling (`from_offset` → `next_offset`); `approval.decide` and `transcript.read` exist; `message.append` takes `session_id`.
- `src/daemon/handlers.rs`: full method surface is `hello`, `run.start`, `message.append`, `events.stream`, `approval.decide`, `run.cancel`, `sessions.list`, `transcript.read`.
- `src/daemon/runtime.rs` (`approval_handler`): a pending approval waits indefinitely on a condvar; it resolves only by `approval.decide` or run cancel. No timeout exists.
- `src/daemon/server.rs` (`bind`): `create_dir_all` + `UnixListener::bind` with no explicit permissions; the non-systemd runtime fallback lives under `/tmp/plato-agent/$USER`.
- `src/tools.rs` / daemon flow: approval requests carry tool name, effect, and diff preview as transient events.

## Desired Outcome
The owner can message Plato from one remote channel and get answers from runs executed by the local workspace daemon, with the same ledger, policy, and approval semantics as the TUI — and no new grant surface.

## Definition
Gateway v1 is one new binary in this repo (boundary ladder: module/binary first, crate only on a second consumer). It is a daemon-protocol client exactly like `plato-tui`: it connects to the Unix socket, speaks NDJSON v1, and owns zero run semantics. It additionally connects outbound to one messaging platform.

## Decisions

### D1. Socket security
The daemon never listens on any network transport. Remote reach happens only via the gateway's outbound connection to the platform (long-poll/websocket). Trust model: any same-user local process may connect — unchanged from today.

Hardening (implementation slice, evidence above): create the socket parent dir `0700`, set the socket `0600` at bind, and refuse to start if either ends up wider. `SO_PEERCRED` uid assertion: deferred until a real second-user threat exists.

### D2. Protocol evolution
- Strictness stays: `deny_unknown_fields` fails closed and is a feature.
- Methods are additive; clients discover them via `hello.capabilities` before use. Unknown method → existing `unsupported_method` error.
- Adding a param field is a daemon-first upgrade: daemon must be upgraded before or with clients. Same host, same user, usually same build — document the order, do not engineer skew tolerance.
- `v` bumps only on envelope-shape breaks; strict-equality reject stays.
- Error codes are contract; new codes are additive (#89 adds the `sessions.list` code).

### D3. Pairing and identity
Single-operator v1. The gateway config holds one platform bot/app token (via env var, `api_key_env` pattern — never a config value) and an allowlist of owner platform-user ids. Messages from anyone else are silently ignored. The daemon keeps no principal model: every local socket client is the owner.

Recorded trigger: any multi-user or shared-channel ambition makes ingress identity semantic — per the core decision-boundary rule it must become a typed, recorded event, which requires a new design before implementation.

### D4. Event-stream recovery
The ledger is truth; deltas are ephemera (existing law). The gateway polls `events.stream` from its last `next_offset`. On reconnect, `lagged`, or daemon restart: resync via `sessions.list` + `transcript.read`, reply from final ledger state, and resume polling at the current tip. No server-side persistent cursors, no delivery guarantee for deltas; final answers are always recoverable from the ledger.

Session mapping: one remote channel/thread ↔ one daemon session, via the existing `message.append` `session_id`. `wait:false` (the #82 default) + polling; the gateway never blocks a connection on a run.

### D5. Remote approval posture
The gateway never sends an approve decision — structurally: the approve branch does not exist in gateway code. On an `approval_requested` event it notifies the channel (tool, effect, short preview) and states that granting happens locally in the TUI/CLI. The pending approval keeps waiting (verified: indefinite condvar wait); `run.cancel` remains the escape hatch. No approval timeout in v1 — silent auto-deny changes unattended run outcomes and would need its own recorded-policy design.

Optional, config-off by default: `remote_deny = true` lets allowlisted owners reply deny, relayed as `approval.decide` deny with reason `remote deny via <platform>`. Deny-only is safe: it can never cause a side effect.

## Human Decision Slots

### Q1. Platform target
One platform for v1. Recommended default: Telegram — single bot token, outbound long-poll, numeric-uid allowlist; smallest pairing surface (verify at implementation). Alternatives: Discord, Slack, SMS.

Jerome: default (Telegram). Accepted 2026-07-09 by ratifying the architecture-lane recommendation; recorded on issue #93.

### Q2. Remote deny relay
Ship v1 notify-only, or include the config-off `remote_deny` relay? Recommended default: notify-only.

Jerome: default (notify-only). Accepted 2026-07-09 by ratifying the architecture-lane recommendation; recorded on issue #93.

## Constraints
- `platonic-core` unchanged; no new event variants for gateway v1.
- Gateway process runs without provider credentials — the daemon holds those. Its environment needs only the platform token.
- One gateway instance ↔ one workspace daemon.
- All existing daemon invariants hold: single writer, one active run per session, transient deltas.

## Non-Goals
- No remote approval grants, ever, in this design.
- No TCP/remote socket, no reverse tunnels, no inbound listeners.
- No multi-workspace routing, no multi-user support, no shared channels.
- No message-history sync into context beyond existing session hydration.
- No new core crates; no connector crate split.

## Forbidden Operations
- Gateway must not write SQLite, spawn runs outside the daemon protocol, hold sessions/policy/approvals/fallback/run semantics, or receive provider credentials.
- No platform tokens in config values or the ledger.
- Remote channels must never grant approval-required effects.

## First Slices (issues to cut after acceptance, in order)
1. Socket hardening: `0700` dir / `0600` socket enforced at bind, fail-closed test. (Independent of platform choice.)
2. Stream-recovery contract test: client-visible `lagged`/restart resync path proven against a live daemon.
3. Gateway binary skeleton for the chosen platform: hello/capabilities check, allowlist filter, session map, `message.append` + polling, final-answer reply.
4. Approval notify relay (plus `remote_deny` only if Q2 accepts it).

No implementation starts from this document; each slice needs its own `Ready for dev` issue.

## Acceptance Criteria
- Both `Jerome:` slots answered; slice issues cut accordingly.
- D1–D5 hold as written or are amended here before coding.

## Verifiable End Condition
From the remote channel, the owner: sends a message, gets the final answer; triggers an approval-required effect, sees the notification, grants it locally in the TUI, sees the completed result remotely; a non-allowlisted sender gets nothing. `plato replay` shows one coherent session; the ledger shows no gateway-originated grant.

## Proof Expectations
- Slice tests per issue (`cargo test --locked`; daemon integration tests for recovery and notify paths).
- One scratch-workspace product proof with the real platform: message, answer, notify, local grant, ignore-stranger.

## Risks
- Platform SDK sprawl: keep the platform client thin; no framework adoption for one bot.
- Notification spam on busy runs: notify on approval and terminal states only, not per-event.
- The `/tmp` runtime fallback stays same-user-writable-parent even after hardening; slice 1 must verify the full path chain, not just the leaf.

## Goal Handoff
None until both slots are answered and this design is accepted on #93.
