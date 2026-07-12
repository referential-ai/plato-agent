---
title: Gateway In-Channel Presentation
issue: https://github.com/referential-ai/plato-agent/issues/129
---

# Gateway In-Channel Presentation (Reactions + Typing)

Revision 4 — addresses lead-lane critical review findings F1–F10 and closure fixes R11–R13 (2026-07-12).

## Authority
- Human direction 2026-07-12: status reactions like Hermes/OpenCode (👀 while reading/thinking); plan and document; lead-lane critical review before finalization.
- Parent design: `gateway-readiness.md` — D1–D5 unchanged by this document.
- Comparable evidence, verified in Hermes source: 👀-on-begin and react-only-after-allowlist (gateway/platforms/signal.py:1646,1634); **current** Discord adapter always removes 👀 at completion, ✅ on success, ❌ on failure, cancel gets no terminal emoji (plugins/platforms/discord/adapter.py:1976-1986) — superseding the older leave-👀-on-cancel comment after stale-eyes residue proved misleading; typing refresh loop with bounded sends (gateway/platforms/base.py:3782-3812); typing paused during approval waits (base.py:2398-2400); missed-resume class fixed in Hermes ba2572e54.

## Source Grounding (main @ 571ef73)
- Poll loop: `EVENT_POLL_DELAY = 100ms`, sleeping only on empty pages (src/discord_gateway.rs:34,180) — polling is far faster than any sane typing cadence; the two must be independent.
- Allowlist/content gate before any daemon call (src/discord_gateway.rs `handle_message`).
- Typed status on every poll: `EventsStreamResult.status: RunStateName`; typed comparisons already present.
- `approval_requested` transient event carries `tool_call_id` (src/daemon/runtime.rs:180-185). Resolution arrives as **nested durable ledger records** pushed as `{"kind":"ledger","record":{…,"event":{"event":"approval_granted"|"approval_denied","call_id":…}}}` (runtime.rs:107-112; platonic-core event.rs:96-109) — note the durable field is **`call_id`**, not `tool_call_id`. Presentation must normalize the transient `tool_call_id` against the durable `call_id`, and the test suite must prove the exact production JSON nesting.
- Terminal readback: `TranscriptReadResult { status, final_answer }`.
- Gap (in scope to fix): `DiscordMessage`/`MessageCreateEvent` currently discard the Discord message id (src/discord_gateway.rs:415-419,639-643) — reactions target `channel_id + message_id`, so the id must be carried through.

## Design

### Effects model (F5, F6)
**Classification.** Presentation effects are exactly: reactions and typing. The final-answer reply and terminal notifications are **product messages**, not presentation — their error semantics are unchanged by this design (they propagate per existing gateway flow), and terminal notifications are owned by #102.

All presentation effects execute **serialized on the single gateway loop**, in program order, best-effort: each is a logged-ignored `Result`, **exactly one attempt**, bounded ~1.5s — never retried. On a 429, the effect is dropped **and presentation enters a monotonic not-before gate**: `presentation_not_before = now + retry_after` (capped 60s), during which further presentation calls are dropped and logged — no sleeping, no retrying, product messages unaffected. This honors Discord's rate contract (retry_after is the time before submitting another request to the affected scope; one gateway-wide presentation gate is the smallest rule that cannot re-violate a bucket: https://docs.discord.com/developers/topics/rate-limits). No detached threads or queues — out-of-order effects (late 👀 after a terminal swap, phantom typing) are structurally impossible. Presentation failures never propagate to run flow.

**Terminal cleanup rule:** every exit from the message lifecycle **attempts** cleanup (best-effort, subject to the not-before gate; accepted partial failures below): stop-typing-refresh → remove 👀 → add ❌. This includes exits caused by product-message failures (daemon connect/hello, dispatch, poll, readback, reply errors) — cleanup is attempted first, then the error propagates with its existing semantics. Discord has no typing-off call: stopping the refresh lets the indicator decay within its documented ~10s. Run semantics unchanged.

### Typing (F1)
Independent monotonic deadline, never tied to poll pages: while status is `Running` and no approval is pending, send trigger-typing when `now ≥ next_typing_at`, then `next_typing_at = now + 8s` (Discord documents ~10s expiry; 2s margin). The **first** send fires immediately on first observing `Running` (and again immediately on resume after an approval decision): `next_typing_at` initializes in the past. Catch-up/backfill pages never burst typing sends. Send timeout (~1.5s) stays well below the 8s interval; a slow send delays polls by at most the timeout — accepted for a single serialized loop.

### Reactions per message lifecycle (F1, F3)
Up to **three** reaction calls plus the reply, in this order at terminal: reply first (answer latency wins), then remove 👀, then add the terminal emoji. Accepted partial-failure states: orphan 👀 (remove failed) or missing terminal emoji (add failed) — logged, never retried.

Full status map (exhaustive `match` on `RunStateName`; a seventh state fails at compile time):

| Status observed | Typing | Reactions |
| --- | --- | --- |
| `Running` | on (deadline-based) | 👀 present (added at filter-pass) |
| `CancelRequested` | stop refresh (waiting quietly) | unchanged |
| `Finished` | stop refresh | reply → remove 👀 → add ✅ |
| `Failed` | stop refresh | remove 👀 → add ❌. The one-time terminal failure notification (today the gateway sends nothing on failure — `terminal_answer` errors when `final_answer` is absent, src/discord_gateway.rs:149-163,253-260) is **#102's surface, single owner; duplication prohibited here** — canonical copy proposed on #102 |
| `Canceled` | stop refresh | no reply (the operator canceled it); remove 👀, **no terminal emoji** (current-Hermes behavior; stale 👀 falsely says in-progress) |
| `Interrupted` | stop refresh | no reply (the session is resumable and the recovered status is itself `Interrupted` — no recursion into readback); remove 👀, no emoji |

👀 is added on the exact branch where the message passes the allowlist/content gate — never before (silent-ignore must not leak a "seen" signal), never for strangers.

### Approval pause/resume (F2)
- Pending approval tracked per run as a **single `Option<call id>`** — the runtime admits at most one at a time (multiple tool calls per response are rejected, src/app.rs:503-511, and the approval callback blocks synchronously). A set and concurrent-approval proofs would be speculative under the simplicity directive.
- **Event fold order:** all events of a page are processed in offset order **before** acting on that page's `status` — request + decision + terminal arriving in one page resolves correctly.
- Resume: clear the pending id on a durable `approval_granted`/`approval_denied` record whose `call_id` matches the stored transient `tool_call_id` (normalized per Source Grounding); typing resumes (immediately) when cleared and status is `Running`. Any terminal status clears it.
- **Resync rule:** lag/reconnect recovery tails from the current tip, so decision events can be legitimately missed. On any resync, clear all pause state and re-derive from current status — accepted best-effort (replaying history for presentation would be disproportionate; Hermes fixed this same missed-resume class in ba2572e54).

### Discord contract (F4)
- Carry `MESSAGE_CREATE.id` through `DiscordMessage` (small in-scope struct change) so reactions can target `channel_id + message_id`.
- Emoji are URL-encoded in REST paths.
- Permissions (guide #127 updates): **Add Reactions** + **Read Message History** (required for Create Reaction), and **Send Messages in Threads** if thread channels are used.
- Rate budget stated plainly, no broader claims: ≤3 reaction calls + `ceil(len/2000)` reply POSTs per message lifecycle (`send_message` chunks at 2000 chars, src/discord_gateway.rs:357-368); typing ≤1 per 8s per active run; zero retries.

### Alignment with #102 (in progress)
#102 owns **all one-time terminal notifications** (approval notify and the terminal failure notification; its card states terminal states notify once) — this design sends no terminal text of its own, only reactions and typing, and must not duplicate #102's messages. Proposed literal failure copy is recorded on #102 so implementation and tests do not invent wording. The shared event-fold loop is reconciled with #102's implementation before card #129 goes Ready.

## Non-Goals
- No per-tool-call or per-event reactions; no configurable emoji sets.
- No 🔒 approval reaction in v1 (revisit with #102's notify UX in hand).
- No reactions on the bot's own replies; no threading changes; no presentation state persisted anywhere; no spawned typing thread.
- No ledger events, no daemon or core changes, no new protocol methods.

## Acceptance (supersedes card #129 acceptance on adoption)
Fake-platform tests:
- Post-filter-only ordering; stranger message produces zero reactions and zero typing.
- Typing deadline math: first send immediate on `Running`; no send before due thereafter; no bursts during catch-up pages; paused while an approval is pending; immediate send on resume.
- Event fold: sequential request → decision folding, including request + decision + terminal in one page; the test fixture uses the **exact production JSON nesting** (`kind=ledger` / `record.event.event` / `call_id`) to prove the key normalization.
- Resync clears pause state.
- Terminal order per status including `Canceled` (remove-👀-only) and `CancelRequested` (typing off, no reaction change).
- Outer-failure cleanup: an induced daemon error after 👀 attempts stop-refresh + remove 👀 + ❌ before the error propagates, without changing run semantics.
- Serialized effects: no reaction call observable after the terminal swap for the same message.
- Rate gate: after an injected 429, presentation calls are dropped until the gate expires; product replies are unaffected during the gate.

Real-Discord smoke: one run showing 👀 → typing → reply → ✅; one stranger message showing nothing.

Docs: the implementation PR updates README.md/docs/QUICKSTART.md for the user-visible changes (reactions, typing, new failure reply) in the same PR per plato-agent/AGENTS.md — not deferred to the guide card #127.

## Proof
`cargo test --locked`; scratch-workspace smoke with reaction readback via REST.
