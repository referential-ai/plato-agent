import type { ApprovalDecision, DesktopEntry, RunState } from '$lib/desktop';

export type RunStatus = RunState;
export type TypedRunEntry = DesktopEntry;

export interface TypedRunSnapshot {
	runId: string;
	status: RunStatus;
	entries: readonly TypedRunEntry[];
}

export type RunEntry =
	| { key: string; kind: 'user'; text: string }
	| { key: string; kind: 'assistant'; ordinal: number; text: string; final: boolean }
	| { key: string; kind: 'tool_call'; callId: string; tool: string; inputPreview: string }
	| { key: string; kind: 'tool_result'; callId: string; summary: string }
	| {
			key: string;
			kind: 'approval';
			callId: string;
			decision: ApprovalDecision;
			actorId: string;
			reason: string | null;
	  }
	| { key: string; kind: 'policy_denied'; callId: string; reason: string }
	| { key: string; kind: 'tool_failed'; callId: string; error: string };

export type RunEvent =
	| {
			kind: 'assistant_delta';
			runId: string;
			offset: number;
			deltaIndex: number;
			assistantOrdinal: number;
			text: string;
	  }
	| {
			kind: 'assistant';
			runId: string;
			assistantOrdinal: number;
			text: string;
	  }
	| ({ runId: string } & Exclude<TypedRunEntry, { kind: 'user' | 'assistant' }>);

interface DeltaChunk {
	key: string;
	assistantOrdinal: number;
	offset: number;
	deltaIndex: number;
	text: string;
}

export interface ExactRunState {
	runId: string;
	status: RunStatus;
	entries: readonly RunEntry[];
	finalAssistantOrdinals: readonly number[];
	deltaChunks: readonly DeltaChunk[];
}

export function createRunState(runId: string, status: RunStatus = 'running'): ExactRunState {
	return {
		runId,
		status,
		entries: [],
		finalAssistantOrdinals: [],
		deltaChunks: []
	};
}

export function applyTypedSnapshot(
	state: ExactRunState,
	snapshot: TypedRunSnapshot
): ExactRunState {
	if (snapshot.runId !== state.runId) return state;

	let userOrdinal = 0;
	let entries: RunEntry[] = [];
	const finalAssistantOrdinals: number[] = [];

	for (const entry of snapshot.entries) {
		if (entry.kind === 'assistant') {
			const ordinal = entry.step;
			finalAssistantOrdinals.push(ordinal);
			if (entry.text.length > 0) {
				entries = upsert(entries, {
					key: assistantKey(ordinal),
					kind: 'assistant',
					ordinal,
					text: entry.text,
					final: true
				});
			}
			continue;
		}

		if (entry.kind === 'user') {
			entries.push({ key: `user:${userOrdinal++}`, ...entry });
			continue;
		}

		entries = upsert(entries, toRunEntry(entry));
	}

	return {
		...state,
		status: advanceStatus(state.status, snapshot.status),
		entries,
		finalAssistantOrdinals,
		deltaChunks: []
	};
}

export function applyRunEvent(state: ExactRunState, event: RunEvent): ExactRunState {
	if (event.runId !== state.runId) return state;

	if (event.kind === 'assistant_delta') return applyAssistantDelta(state, event);

	if (event.kind === 'assistant') {
		const key = assistantKey(event.assistantOrdinal);
		const finalAssistantOrdinals = addNumber(
			state.finalAssistantOrdinals,
			event.assistantOrdinal
		);
		const deltaChunks = state.deltaChunks.filter(
			(chunk) => chunk.assistantOrdinal !== event.assistantOrdinal
		);
		const entries = event.text.length
			? upsert(state.entries, {
					key,
					kind: 'assistant',
					ordinal: event.assistantOrdinal,
					text: event.text,
					final: true
				})
			: state.entries.filter((entry) => entry.key !== key);

		return { ...state, entries, finalAssistantOrdinals, deltaChunks };
	}

	const { runId: _runId, ...entry } = event;
	return { ...state, entries: upsert(state.entries, toRunEntry(entry)) };
}

export function applyRunEvents(
	state: ExactRunState,
	events: readonly RunEvent[]
): ExactRunState {
	return events.reduce(applyRunEvent, state);
}

export function applyRunStatus(state: ExactRunState, status: RunStatus): ExactRunState {
	const nextStatus = advanceStatus(state.status, status);
	return nextStatus === state.status ? state : { ...state, status: nextStatus };
}

function applyAssistantDelta(
	state: ExactRunState,
	event: Extract<RunEvent, { kind: 'assistant_delta' }>
): ExactRunState {
	if (state.finalAssistantOrdinals.includes(event.assistantOrdinal)) return state;

	const chunkKey = `${event.offset}:${event.deltaIndex}`;
	if (state.deltaChunks.some((chunk) => chunk.key === chunkKey)) return state;

	const deltaChunks = [
		...state.deltaChunks,
		{
			key: chunkKey,
			assistantOrdinal: event.assistantOrdinal,
			offset: event.offset,
			deltaIndex: event.deltaIndex,
			text: event.text
		}
	];
	const text = deltaChunks
		.filter((chunk) => chunk.assistantOrdinal === event.assistantOrdinal)
		.sort((left, right) => left.offset - right.offset || left.deltaIndex - right.deltaIndex)
		.map((chunk) => chunk.text)
		.join('');
	const entries = upsert(state.entries, {
		key: assistantKey(event.assistantOrdinal),
		kind: 'assistant',
		ordinal: event.assistantOrdinal,
		text,
		final: false
	});

	return { ...state, entries, deltaChunks };
}

function toRunEntry(
	entry: Exclude<TypedRunEntry, { kind: 'user' | 'assistant' }>
): RunEntry {
	return { key: `${entry.kind}:${entry.callId}`, ...entry };
}

function upsert(entries: readonly RunEntry[], next: RunEntry): RunEntry[] {
	const index = entries.findIndex((entry) => entry.key === next.key);
	if (index === -1) return [...entries, next];
	const updated = [...entries];
	updated[index] = next;
	return updated;
}

function addNumber(values: readonly number[], value: number): number[] {
	return values.includes(value) ? [...values] : [...values, value];
}

function assistantKey(ordinal: number): string {
	return `assistant:${ordinal}`;
}

function advanceStatus(current: RunStatus, next: RunStatus): RunStatus {
	if (isTerminal(current)) return current;
	if (current === 'cancel_requested' && next === 'running') return current;
	return next;
}

function isTerminal(status: RunStatus): boolean {
	return (
		status === 'finished' ||
		status === 'failed' ||
		status === 'canceled' ||
		status === 'interrupted'
	);
}
