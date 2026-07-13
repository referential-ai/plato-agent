<script lang="ts">
	import type { DesktopSession } from '$lib/desktop';

	interface Props {
		sessions: DesktopSession[];
		selectedSessionId: string | null;
		loadingSessionId?: string | null;
		onselect: (sessionId: string) => void;
	}

	let { sessions, selectedSessionId, loadingSessionId = null, onselect }: Props = $props();

	function statusLabel(status: DesktopSession['status']): string {
		return status.replace('_', ' ');
	}
</script>

<nav class="session-list" aria-label="Sessions">
	{#each sessions as session (session.sessionId)}
		<button
			type="button"
			class:current={session.sessionId === selectedSessionId}
			class:loading={session.sessionId === loadingSessionId}
			aria-current={session.sessionId === selectedSessionId ? 'page' : undefined}
			aria-busy={session.sessionId === loadingSessionId}
			onclick={() => onselect(session.sessionId)}
		>
			<span class="session-question">{session.latestQuestion || 'Untitled chat'}</span>
			<span class="session-meta">
				<span class:active={session.status === 'running'} class="status-dot"></span>
				{statusLabel(session.status)}
			</span>
		</button>
	{/each}
</nav>
