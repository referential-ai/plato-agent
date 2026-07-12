---
title: Gateway In-Channel Presentation
issue: https://github.com/referential-ai/plato-agent/issues/129
---

# Gateway In-Channel Presentation (Reactions + Typing)

## Authority
- Human direction 2026-07-12: status reactions like Hermes/OpenCode (👀 while reading/thinking); plan and document; lead-lane critical review before finalization.
- Parent design: `gateway-readiness.md` — D1–D5 unchanged by this document.
- Comparable evidence, verified in Hermes source: 👀-on-begin (signal.py:1646), swap to ✅/❌ at terminal (1654), leave 👀 on cancel (1656), react-only-after-allowlist (1634), typing refresh loop with bounded sends (base.py:3782-3812), typing paused during approval waits (base.py:2398-2400).

## Source Grounding (existing surfaces only)
- Allowlist/content gate before any daemon call: discord_gateway.rs (~720).
- Typed status on every poll: `EventsStreamResult.status: RunStateName` (protocol.rs; PR #122). Gateway already compares it (discord_gateway.rs:760, 777).
- `approval_requested` transient event with tool, effect, and preview: runtime.rs (already present in the polled `events`).
- Terminal readback: `TranscriptReadResult { status, final_answer }` (PR #104).

## Design
One presentation map over transitions the polling loop already computes. No new inbound calls; two new outbound Discord REST effects (add/remove reaction, trigger typing).

| Observed transition (existing code point) | In-channel effect |
| --- | --- |
| Message passes allowlist + non-empty (dispatch branch) | add 👀 to the user's message |
| Poll shows `Running` | typing indicator; refresh while `Running` (expires ~5s; refresh on poll cadence; each REST send bounded ~1.5s) |
| `approval_requested` seen in `events` | typing paused until a decision/terminal is observed; #102's notify posts here |
| `Finished` | stop typing; 👀 → ✅; reply with `final_answer` (reply is not blocked on reaction calls) |
| `Failed` | stop typing; 👀 → ❌ (failure message unchanged) |
| `Canceled` | stop typing; leave 👀 (Hermes precedent; recorded styling choice) |
| `Interrupted` / recovery paths | present per the final recovered status from `transcript.read`; if none recoverable, leave 👀 |

## Binding rules
- React only after the allowlist/content gate. Silent-ignore must never leak a "seen" signal to strangers.
- Presentation is fire-and-forget: one attempt, failures logged, never propagated to run flow, never retried.
- No ledger events, no daemon or core changes, no new protocol methods. Presentation reads only already-polled data.
- The map is an exhaustive `match` on `RunStateName`: a future seventh state fails at compile time, not silently in a channel.
- Permissions: bot invite gains **Add Reactions**; typing rides Send Messages. The gateway guide (#127) documents both.

## Rate posture
≤2 reaction calls per message lifecycle plus typing refresh on poll cadence while `Running` — far under Discord per-channel limits; every REST call time-bounded so a slow Discord never stalls the poll loop.

## Non-Goals
- No per-tool-call or per-event reactions; no configurable emoji sets.
- No 🔒 approval reaction in v1 (revisit with #102's notify UX in hand).
- No reactions on the bot's own replies; no threading changes.
- No presentation state persisted anywhere.

## Acceptance (mirrors card #129)
- Fake-platform tests: post-filter-only ordering (stranger gets nothing); typing refresh and approval pause; terminal swaps per status.
- Real-Discord smoke: one run showing 👀 → typing → ✅ + reply; one stranger message showing no reaction.

## Proof
`cargo test --locked`; scratch-workspace smoke with reaction readback via REST (screenshots optional).
