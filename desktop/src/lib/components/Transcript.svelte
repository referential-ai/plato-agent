<script lang="ts">
	import { Check, CircleX, ShieldAlert, Terminal, TriangleAlert } from '@lucide/svelte';
	import type { DesktopEntry, DesktopRun } from '$lib/desktop';

	interface Props {
		run: DesktopRun;
	}

	let { run }: Props = $props();

	function prettyInput(input: unknown): string {
		if (typeof input === 'string') return input;
		try {
			return JSON.stringify(input, null, 2);
		} catch {
			return String(input);
		}
	}

	function approvalText(entry: Extract<DesktopEntry, { kind: 'approval' }>): string {
		const action = entry.decision === 'granted' ? 'Approved' : 'Denied';
		return entry.reason ? `${action}: ${entry.reason}` : action;
	}
</script>

<article class="transcript" aria-label="Run transcript">
	{#if run.entries.length === 0}
		<div class="empty-run">This run has no transcript entries.</div>
	{:else}
		{#each run.entries as entry, index (`${entry.kind}-${index}`)}
			{#if entry.kind === 'user'}
				<section class="message user-message" aria-label="You">
					<div class="message-label">You</div>
					<p>{entry.text}</p>
				</section>
			{:else if entry.kind === 'assistant'}
				<section class="message assistant-message" aria-label="Plato">
					<div class="message-label">Plato</div>
					<p>{entry.text}</p>
				</section>
			{:else if entry.kind === 'tool_call'}
				<section class="event-block" aria-label={`Tool call ${entry.tool}`}>
					<div class="event-heading"><Terminal size={14} />{entry.tool}</div>
					<pre>{prettyInput(entry.input)}</pre>
				</section>
			{:else if entry.kind === 'tool_result'}
				<section class="event-row success" aria-label="Tool result">
					<Check size={14} />
					<span>{entry.summary}</span>
				</section>
			{:else if entry.kind === 'approval'}
				<section class:denied={entry.decision === 'denied'} class="event-row" aria-label="Approval">
					{#if entry.decision === 'granted'}<Check size={14} />{:else}<CircleX size={14} />{/if}
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
	{/if}
</article>
