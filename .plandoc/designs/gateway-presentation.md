---
title: Gateway In-Channel Presentation
issue: https://github.com/referential-ai/plato-agent/issues/129
---

# Gateway In-Channel Presentation (Reactions + Typing)

Revision 2 — addresses lead-lane critical review findings F1–F6 (2026-07-12).

## Authority
- Human direction 2026-07-12: status reactions like Hermes/OpenCode (👀 while reading/thinking); plan and document; lead-lane critical review before finalization.
- Parent design: `gateway-readiness.md` — D1–D5 unchanged by this document.
- Comparable evidence, verified in Hermes source: 👀-on-begin and react-only-after-allowlist (gateway/platforms/signal.py:1646,1634); **current** Discord adapter always removes 👀 at completion, ✅ on success, ❌ on failure, cancel gets no terminal emoji (plugins/platforms/discord/adapter.py:1976-1986) — superseding the older leave-👀-on-cancel comment after stale-eyes residue proved misleading; typing refresh loop with bounded sends (gateway/platforms/base.py:3782-3812); typing paused during approval waits (base.py:2398-2400); missed-resume class fixed in Hermes ba2572e54.

## Source Grounding (main @ 571ef73)
- Poll loop: `EVENT_POLL_DELAY = 100ms`, sleeping only on empty pages (src/discord_gateway.rs:34,180) — polling is far faster than any sane typing cadence; the two must be independent.
- Allowlist/content gate before any daemon call (src/discord_gateway.rs `handle_message`).
- Typed status on every poll: `EventsStreamResult.status: RunStateName`; typed comparisons already present.
- `approval_requested` transient event (kind + `tool_call_id`, tool, effect, preview: src/daemon/runtime.rs:151-195). Resolution arrives as **nested ledger records**: `record.event.event = approval_granted | approval_denied` carrying `tool_call_id`.
- Terminal readback: `TranscriptReadResult { status, final_answer }`.
- Gap (in scope to fix): `DiscordMessage`/`MessageCreateEvent` currently discard the Discord message id (src/discord_gateway.rs:415-419,639-643) — reactions target `channel_id + message_id`, so the id must be carried through.

## Design

### Effects model (F5, F6)
All presentation effects execute **serialized on the single gateway loop**, in program order, best-effort: each is a logged-ignored `Result`, one attempt, bounded ~1.5s, honoring one `Retry-After` on 429 then dropping. No detached threads or queues — out-of-order effects (late 👀 after a terminal swap, phantom typing) are structurally impossible. Presentation failures never propagate to run flow.

**Terminal cleanup rule:** no code path may leave the message lifecycle with 👀 still present and no terminal decision, except process kill. Any post-👀 gateway-side failure outside run states (daemon connect/hello, dispatch, poll, readback, reply errors that today propagate) performs best-effort typing-off → remove 👀 → add ❌ **before** propagating. Run semantics unchanged.

### Typing (F1)
Independent monotonic deadline, never tied to poll pages: while status is `Running` and no approval is pending, send trigger-typing when `now ≥ next_typing_at`, then `next_typing_at = now + 8s` (Discord documents ~10s expiry; 2s margin). Catch-up/backfill pages never burst typing sends. Send timeout (~1.5s) stays well below the 8s interval; a slow send delays polls by at most the timeout — accepted for a single serialized loop.

### Reactions per message lifecycle (F1, F3)
Up to **three** reaction calls plus the reply, in this order at terminal: reply first (answer latency wins), then remove 👀, then add the terminal emoji. Accepted partial-failure states: orphan 👀 (remove failed) or missing terminal emoji (add failed) — logged, never retried beyond the single Retry-After allowance.

Full status map (exhaustive `match` on `RunStateName`; a seventh state fails at compile time):

| Status observed | Typing | Reactions |
| --- | --- | --- |
| `Running` | on (deadline-based) | 👀 present (added at filter-pass) |
| `CancelRequested` | off (waiting quietly) | unchanged |
| `Finished` | off | reply → remove 👀 → add ✅ |
| `Failed` | off | failure note (existing) → remove 👀 → add ❌ |
| `Canceled` | off | remove 👀, **no terminal emoji** (current-Hermes behavior; stale 👀 falsely says in-progress) |
| `Interrupted` | off | present per the recovered terminal status from `transcript.read`; if recovery yields no terminal, remove 👀, no emoji |

👀 is added on the exact branch where the message passes the allowlist/content gate — never before (silent-ignore must not leak a "seen" signal), never for strangers.

### Approval pause/resume (F2)
- Pending approvals tracked per run, keyed by `tool_call_id`, inserted on `approval_requested`.
- **Event fold order:** all events of a page are processed in offset order **before** acting on that page's `status` — request + decision + terminal arriving in one page resolves correctly.
- Resume: remove the key on a matching `approval_granted`/`approval_denied` ledger record; typing resumes when the set is empty and status is `Running`. Any terminal status clears the set.
- **Resync rule:** lag/reconnect recovery tails from the current tip, so decision events can be legitimately missed. On any resync, clear all pause state and re-derive from current status — accepted best-effort (replaying history for presentation would be disproportionate; Hermes fixed this same missed-resume class in ba2572e54).

### Discord contract (F4)
- Carry `MESSAGE_CREATE.id` through `DiscordMessage` (small in-scope struct change) so reactions can target `channel_id + message_id`.
- Emoji are URL-encoded in REST paths.
- Permissions (guide #127 updates): **Add Reactions** + **Read Message History** (required for Create Reaction), and **Send Messages in Threads** if thread channels are used.
- Rate budget stated plainly, no broader claims: ≤3 reaction calls + 1 reply per message lifecycle; typing ≤1 per 8s per active run; one Retry-After honor per effect.

## Non-Goals
- No per-tool-call or per-event reactions; no configurable emoji sets.
- No 🔒 approval reaction in v1 (revisit with #102's notify UX in hand).
- No reactions on the bot's own replies; no threading changes; no presentation state persisted anywhere; no spawned typing thread.
- No ledger events, no daemon or core changes, no new protocol methods.

## Acceptance (supersedes card #129 acceptance on adoption)
Fake-platform tests:
- Post-filter-only ordering; stranger message produces zero reactions and zero typing.
- Typing deadline math: no send before due; no bursts during catch-up pages; paused while an approval is pending.
- Event fold: two concurrent approvals (pause until both resolved); request + decision + terminal in one page.
- Resync clears pause state.
- Terminal order per status including `Canceled` (remove-👀-only) and `CancelRequested` (typing off, no reaction change).
- Outer-failure cleanup: an induced daemon error after 👀 yields typing-off + remove 👀 + ❌ without changing run flow.
- Serialized effects: no reaction call observable after the terminal swap for the same message.

Real-Discord smoke: one run showing 👀 → typing → reply → ✅; one stranger message showing nothing.

## Proof
`cargo test --locked`; scratch-workspace smoke with reaction readback via REST.
