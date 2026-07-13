import { describe, expect, it } from 'vitest';
import {
	applyRunEvent,
	applyRunEvents,
	applyRunStatus,
	applyTypedSnapshot,
	createRunState,
	type RunEvent,
	type RunEntry,
	type TypedRunEntry
} from './run-state';

function assistantEntries(entries: readonly RunEntry[]) {
	return entries.filter((entry) => entry.kind === 'assistant');
}

describe('exact-run presentation reducer', () => {
	it('counts empty model steps when assigning assistant ordinals', () => {
		const state = applyTypedSnapshot(createRunState('run-1'), {
			runId: 'run-1',
			status: 'running',
			entries: [
				{ kind: 'assistant', step: 0, text: '' },
				{ kind: 'tool_call', callId: 'call-1', tool: 'shell.exec', inputPreview: '{}' },
				{ kind: 'assistant', step: 1, text: 'Second model step' }
			]
		});

		expect(assistantEntries(state.entries)).toEqual([
			{
				key: 'assistant:1',
				kind: 'assistant',
				ordinal: 1,
				text: 'Second model step',
				final: true
			}
		]);
		expect(state.finalAssistantOrdinals).toEqual([0, 1]);
		expect(
			applyRunEvent(state, {
				kind: 'assistant_delta',
				runId: 'run-1',
				offset: 1,
				deltaIndex: 0,
				assistantOrdinal: 0,
				text: 'stale draft'
			}).entries
		).toEqual(state.entries);
	});

	it('deduplicates replayed chunks and orders a draft by delta index', () => {
		const chunks: RunEvent[] = [
			{
				kind: 'assistant_delta',
				runId: 'run-1',
				offset: 11,
				deltaIndex: 1,
				assistantOrdinal: 0,
				text: 'world'
			},
			{
				kind: 'assistant_delta',
				runId: 'run-1',
				offset: 10,
				deltaIndex: 0,
				assistantOrdinal: 0,
				text: 'hello '
			}
		];
		const once = applyRunEvents(createRunState('run-1'), chunks);
		const replayed = applyRunEvents(once, chunks);

		expect(assistantEntries(replayed.entries)).toEqual([
			{
				key: 'assistant:0',
				kind: 'assistant',
				ordinal: 0,
				text: 'hello world',
				final: false
			}
		]);
		expect(replayed.deltaChunks).toHaveLength(2);
		expect(once).toEqual(replayed);
	});

	it('uses offset and delta index together as the chunk identity', () => {
		const state = applyRunEvents(createRunState('run-1'), [
			{
				kind: 'assistant_delta',
				runId: 'run-1',
				offset: 4,
				deltaIndex: 0,
				assistantOrdinal: 0,
				text: 'a'
			},
			{
				kind: 'assistant_delta',
				runId: 'run-1',
				offset: 5,
				deltaIndex: 0,
				assistantOrdinal: 0,
				text: 'b'
			}
		]);

		expect(assistantEntries(state.entries)[0]).toMatchObject({ text: 'ab' });
		expect(state.deltaChunks).toHaveLength(2);
	});

	it('orders chunks by stream offset before delta index', () => {
		const state = applyRunEvents(createRunState('run-1'), [
			{
				kind: 'assistant_delta',
				runId: 'run-1',
				offset: 20,
				deltaIndex: 0,
				assistantOrdinal: 0,
				text: 'second'
			},
			{
				kind: 'assistant_delta',
				runId: 'run-1',
				offset: 10,
				deltaIndex: 1,
				assistantOrdinal: 0,
				text: 'first '
			}
		]);

		expect(assistantEntries(state.entries)[0]).toMatchObject({ text: 'first second' });
	});

	it('replaces a live assistant draft with the final model response', () => {
		const draft = applyRunEvent(createRunState('run-1'), {
			kind: 'assistant_delta',
			runId: 'run-1',
			offset: 3,
			deltaIndex: 0,
			assistantOrdinal: 0,
			text: 'partial'
		});
		const final = applyRunEvent(draft, {
			kind: 'assistant',
			runId: 'run-1',
			assistantOrdinal: 0,
			text: 'Canonical answer'
		});

		expect(assistantEntries(final.entries)).toEqual([
			{
				key: 'assistant:0',
				kind: 'assistant',
				ordinal: 0,
				text: 'Canonical answer',
				final: true
			}
		]);
		expect(final.deltaChunks).toEqual([]);
		expect(
			applyRunEvent(final, {
				kind: 'assistant_delta',
				runId: 'run-1',
				offset: 3,
				deltaIndex: 0,
				assistantOrdinal: 0,
				text: 'partial'
			})
		).toBe(final);
	});

	it('removes a draft when a tool-use model step commits empty text', () => {
		const draft = applyRunEvent(createRunState('run-1'), {
			kind: 'assistant_delta',
			runId: 'run-1',
			offset: 3,
			deltaIndex: 0,
			assistantOrdinal: 0,
			text: 'discarded'
		});
		const committed = applyRunEvent(draft, {
			kind: 'assistant',
			runId: 'run-1',
			assistantOrdinal: 0,
			text: ''
		});

		expect(committed.entries).toEqual([]);
		expect(committed.finalAssistantOrdinals).toEqual([0]);
		expect(committed.deltaChunks).toEqual([]);
	});

	it('lets an authoritative typed snapshot replace transient state', () => {
		const live = applyRunEvents(createRunState('run-1'), [
			{
				kind: 'assistant_delta',
				runId: 'run-1',
				offset: 1,
				deltaIndex: 0,
				assistantOrdinal: 0,
				text: 'draft'
			},
			{
				kind: 'tool_failed',
				runId: 'run-1',
				callId: 'stale-call',
				error: 'stale'
			}
		]);
		const snapshot = applyTypedSnapshot(live, {
			runId: 'run-1',
			status: 'finished',
			entries: [
				{ kind: 'user', text: 'Question' },
				{ kind: 'assistant', step: 0, text: 'Answer' }
			]
		});

		expect(snapshot.entries).toEqual([
			{ key: 'user:0', kind: 'user', text: 'Question' },
			{
				key: 'assistant:0',
				kind: 'assistant',
				ordinal: 0,
				text: 'Answer',
				final: true
			}
		]);
		expect(snapshot.deltaChunks).toEqual([]);
		expect(snapshot.status).toBe('finished');
	});

	it.each([
		[
			{
				kind: 'tool_call',
				callId: 'call-1',
				tool: 'shell.exec',
				inputPreview: 'old'
			},
			{
				kind: 'tool_call',
				callId: 'call-1',
				tool: 'shell.exec',
				inputPreview: 'new'
			}
		],
		[
			{ kind: 'tool_result', callId: 'call-1', summary: 'old' },
			{ kind: 'tool_result', callId: 'call-1', summary: 'new' }
		],
		[
			{
				kind: 'approval',
				callId: 'call-1',
				decision: 'denied',
				actorId: 'old',
				reason: null
			},
			{
				kind: 'approval',
				callId: 'call-1',
				decision: 'granted',
				actorId: 'new',
				reason: null
			}
		],
		[
			{ kind: 'policy_denied', callId: 'call-1', reason: 'old' },
			{ kind: 'policy_denied', callId: 'call-1', reason: 'new' }
		],
		[
			{ kind: 'tool_failed', callId: 'call-1', error: 'old' },
			{ kind: 'tool_failed', callId: 'call-1', error: 'new' }
		]
	] as const)('keys repeated %s entries by kind and call id', (first, second) => {
		const events = [first, second].map((entry) => ({ ...entry, runId: 'run-1' })) as RunEvent[];
		const state = applyRunEvents(createRunState('run-1'), events);

		expect(state.entries).toHaveLength(1);
		expect(state.entries[0]).toMatchObject(second);
	});

	it('does not collide different call-scoped kinds sharing one call id', () => {
		const entries: TypedRunEntry[] = [
			{ kind: 'tool_call', callId: 'call-1', tool: 'shell.exec', inputPreview: '{}' },
			{ kind: 'tool_result', callId: 'call-1', summary: 'done' },
			{
				kind: 'approval',
				callId: 'call-1',
				decision: 'granted',
				actorId: 'human',
				reason: null
			},
			{ kind: 'policy_denied', callId: 'call-1', reason: 'blocked' },
			{ kind: 'tool_failed', callId: 'call-1', error: 'failed' }
		];
		const state = applyTypedSnapshot(createRunState('run-1'), {
			runId: 'run-1',
			status: 'finished',
			entries
		});

		expect(state.entries.map((entry) => entry.key)).toEqual([
			'tool_call:call-1',
			'tool_result:call-1',
			'approval:call-1',
			'policy_denied:call-1',
			'tool_failed:call-1'
		]);
	});

	it('keeps cancel requested sticky across stale running readback', () => {
		const cancelRequested = applyRunStatus(createRunState('run-1'), 'cancel_requested');
		const stalePage = applyRunStatus(cancelRequested, 'running');
		const staleSnapshot = applyTypedSnapshot(stalePage, {
			runId: 'run-1',
			status: 'running',
			entries: []
		});
		const canceled = applyRunStatus(staleSnapshot, 'canceled');

		expect(cancelRequested.status).toBe('cancel_requested');
		expect(stalePage.status).toBe('cancel_requested');
		expect(staleSnapshot.status).toBe('cancel_requested');
		expect(canceled.status).toBe('canceled');
		expect(applyRunStatus(canceled, 'running')).toBe(canceled);
	});

	it.each([
		'running',
		'finished',
		'failed',
		'canceled',
		'cancel_requested',
		'interrupted'
	] as const)('preserves the typed %s run status', (status) => {
		expect(createRunState('run-1', status).status).toBe(status);
	});

	it('folds the same reconnect page idempotently', () => {
		const page: RunEvent[] = [
			{
				kind: 'assistant_delta',
				runId: 'run-1',
				offset: 8,
				deltaIndex: 0,
				assistantOrdinal: 0,
				text: 'draft'
			},
			{
				kind: 'tool_call',
				runId: 'run-1',
				callId: 'call-1',
				tool: 'shell.exec',
				inputPreview: '{}'
			}
		];
		const once = applyRunEvents(createRunState('run-1'), page);
		const twice = applyRunEvents(once, page);

		expect(twice).toEqual(once);
		expect(twice.entries).toHaveLength(2);
	});

	it('ignores snapshots and events belonging to another run', () => {
		const runOne = createRunState('run-1');
		const foreignSnapshot = applyTypedSnapshot(runOne, {
			runId: 'run-2',
			status: 'finished',
			entries: [{ kind: 'assistant', step: 0, text: 'wrong run' }]
		});
		const foreignEvent = applyRunEvent(runOne, {
			kind: 'assistant_delta',
			runId: 'run-2',
			offset: 1,
			deltaIndex: 0,
			assistantOrdinal: 0,
			text: 'wrong run'
		});
		const runTwo = applyRunEvent(createRunState('run-2'), {
			kind: 'assistant_delta',
			runId: 'run-2',
			offset: 1,
			deltaIndex: 0,
			assistantOrdinal: 0,
			text: 'right run'
		});

		expect(foreignSnapshot).toBe(runOne);
		expect(foreignEvent).toBe(runOne);
		expect(runOne.entries).toEqual([]);
		expect(assistantEntries(runTwo.entries)[0]).toMatchObject({ text: 'right run' });
	});
});
