<script lang="ts">
	import type { DesktopSession } from '$lib/desktop';

	interface Props {
		sessions: DesktopSession[];
		selectedRunId: string | null;
		disabled?: boolean;
		onselect: (runId: string) => void;
	}

	let { sessions, selectedRunId, disabled = false, onselect }: Props = $props();

	function statusLabel(status: DesktopSession['status']): string {
		return status.replace('_', ' ');
	}
</script>

<nav class="session-list" aria-label="Sessions">
	{#each sessions as session (session.runId)}
		<button
			type="button"
			class:current={session.runId === selectedRunId}
			disabled={disabled}
			onclick={() => onselect(session.runId)}
		>
			<span class="session-question">{session.latestQuestion || 'Untitled run'}</span>
			<span class="session-meta">
				<span class:active={session.status === 'running'} class="status-dot"></span>
				{statusLabel(session.status)}
			</span>
		</button>
	{/each}
</nav>
