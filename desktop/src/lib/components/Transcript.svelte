<script lang="ts">
	import { Check, CircleX, ShieldAlert, Terminal, TriangleAlert } from '@lucide/svelte';
	import type { RunEntry, RunStatus } from '$lib/run-state';

	interface TranscriptRun {
		runId: string;
		sessionIndex: number;
		status: RunStatus;
		entries: readonly RunEntry[];
	}

	interface Props {
		runs: readonly TranscriptRun[];
		activeRunId?: string | null;
	}

	let { runs, activeRunId = null }: Props = $props();

	function approvalText(entry: Extract<RunEntry, { kind: 'approval' }>): string {
		const action = entry.decision === 'granted' ? 'Approved' : 'Denied';
		return entry.reason ? `${action}: ${entry.reason}` : action;
	}

	function statusLabel(status: RunStatus): string {
		return status.replace('_', ' ');
	}
</script>

<article class="transcript" aria-label="Chat transcript">
	{#if runs.every((run) => run.entries.length === 0)}
		<div class="empty-run">This chat has no transcript entries.</div>
	{:else}
		{#each runs as run (run.runId)}
			<section
				class="transcript-run"
				class:active={run.runId === activeRunId}
				aria-label={`Run ${run.sessionIndex + 1}, ${statusLabel(run.status)}`}
			>
				{#if runs.length > 1}
					<div class="run-boundary">
						<span>Run {run.sessionIndex + 1}</span>
						<span>{statusLabel(run.status)}</span>
					</div>
				{/if}

				{#each run.entries as entry (entry.key)}
					{#if entry.kind === 'user'}
						<section class="message user-message" aria-label="You">
							<div class="message-label">You</div>
							<p>{entry.text}</p>
						</section>
					{:else if entry.kind === 'assistant' && entry.text.length > 0}
						<section
							class="message assistant-message"
							class:live={!entry.final}
							aria-label={entry.final ? 'Plato Agent' : 'Plato Agent, responding'}
						>
							<div class="message-label">
								Plato Agent
								{#if !entry.final}<span class="live-label">Live</span>{/if}
							</div>
							<p>{entry.text}</p>
						</section>
					{:else if entry.kind === 'tool_call'}
						<section class="event-block" aria-label={`Tool call ${entry.tool}`}>
							<div class="event-heading"><Terminal size={14} />{entry.tool}</div>
							<pre>{entry.inputPreview}</pre>
						</section>
					{:else if entry.kind === 'tool_result'}
						<section class="event-row success" aria-label="Tool result">
							<Check size={14} />
							<span>{entry.summary}</span>
						</section>
					{:else if entry.kind === 'approval'}
						<section
							class:denied={entry.decision === 'denied'}
							class="event-row"
							aria-label="Approval"
						>
							{#if entry.decision === 'granted'}
								<Check size={14} />
							{:else}
								<CircleX size={14} />
							{/if}
							<span>{approvalText(entry)}</span>
							<span class="event-actor">{entry.actorId}</span>
						</section>
					{:else if entry.kind === 'policy_denied'}
						<section class="event-row denied" aria-label="Policy denied">
							<ShieldAlert size={14} />
							<span>{entry.reason}</span>
						</section>
					{:else if entry.kind === 'tool_failed'}
						<section class="event-row failed" aria-label="Tool failed">
							<TriangleAlert size={14} />
							<span>{entry.error}</span>
						</section>
					{/if}
				{/each}
			</section>
		{/each}
	{/if}
</article>
