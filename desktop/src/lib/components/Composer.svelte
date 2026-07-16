<script lang="ts">
	import { Send, Square } from '@lucide/svelte';
	import type { RunStatus } from '$lib/run-state';

	interface Props {
		message: string;
		disabled: boolean;
		submitting: boolean;
		activeStatus: RunStatus | null;
		canCancel: boolean;
		error: string | null;
		onsubmit: (message: string) => void;
		oncancel: () => void;
	}

	let {
		message = $bindable(),
		disabled,
		submitting,
		activeStatus,
		canCancel,
		error,
		onsubmit,
		oncancel
	}: Props = $props();

	const canSubmit = $derived(!disabled && !submitting && message.trim().length > 0);

	function submit(): void {
		if (!canSubmit) return;
		onsubmit(message.trim());
	}

	function handleSubmit(event: SubmitEvent): void {
		event.preventDefault();
		submit();
	}

	function handleKeydown(event: KeyboardEvent): void {
		if (event.key !== 'Enter' || event.shiftKey || event.isComposing) return;
		event.preventDefault();
		submit();
	}

	function statusLabel(status: RunStatus): string {
		return status.replace('_', ' ');
	}
</script>

<form class="composer" aria-label="Message composer" onsubmit={handleSubmit}>
	<textarea
		bind:value={message}
		disabled={disabled}
		rows="1"
		aria-label="Message"
		placeholder="Message Plato Agent"
		onkeydown={handleKeydown}
	></textarea>

	<div class="composer-bar">
		<div class="composer-state">
			{#if activeStatus}<span>{statusLabel(activeStatus)}</span>{/if}
			{#if error}<span class="composer-error" aria-live="polite">{error}</span>{/if}
		</div>
		<div class="composer-actions">
			{#if canCancel}
				<button
					type="button"
					class="icon-button"
					aria-label="Cancel run"
					title="Cancel run"
					onclick={oncancel}
				>
					<Square size={16} fill="currentColor" />
				</button>
			{/if}
			<button
				type="submit"
				class="icon-button primary"
				disabled={!canSubmit}
				aria-label="Send message"
				title="Send message"
			>
				<Send size={17} />
			</button>
		</div>
	</div>
</form>
