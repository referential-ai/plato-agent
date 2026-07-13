<script lang="ts">
	import { onMount } from 'svelte';
	import { Check, CircleX, ShieldCheck } from '@lucide/svelte';
	import type { DesktopPendingApproval } from '$lib/desktop';

	interface Props {
		approval: DesktopPendingApproval;
		busy: boolean;
		error?: string | null;
		ongrant: () => void;
		ondeny: () => void;
	}

	let { approval, busy, error = null, ongrant, ondeny }: Props = $props();
	let denyButton: HTMLButtonElement | undefined;

	onMount(() => denyButton?.focus());
</script>

<div class="approval-backdrop">
	<div
		class="approval-dialog"
		role="dialog"
		aria-modal="true"
		aria-labelledby="approval-title"
		aria-describedby={approval.reason ? 'approval-reason' : undefined}
	>
		<header class="approval-header">
			<ShieldCheck size={20} />
			<div>
				<div class="dialog-label">Approval required</div>
				<h2 id="approval-title">{approval.toolName}</h2>
			</div>
		</header>

		<div class="approval-content">
			<div class="approval-effect">{approval.effect}</div>
			{#if approval.reason}
				<p id="approval-reason">{approval.reason}</p>
			{/if}
			{#if approval.inputPreview}
				<section class="approval-preview" aria-label="Input preview">
					<div class="preview-label">Input</div>
					<pre>{approval.inputPreview}</pre>
				</section>
			{/if}
			{#if approval.approvalPreview}
				<section class="approval-preview" aria-label="Approval preview">
					<div class="preview-label">Action</div>
					<pre>{approval.approvalPreview}</pre>
				</section>
			{/if}
			{#if approval.diffPreview}
				<section class="approval-preview" aria-label="Diff preview">
					<div class="preview-label">Changes</div>
					<pre>{approval.diffPreview}</pre>
				</section>
			{/if}
		</div>

		{#if error}
			<p class="approval-error" role="alert">{error}</p>
		{/if}

		<footer class="approval-actions">
			<button bind:this={denyButton} type="button" disabled={busy} onclick={ondeny}>
				<CircleX size={16} />
				Deny
			</button>
			<button class="primary" type="button" disabled={busy} onclick={ongrant}>
				<Check size={16} />
				Grant
			</button>
		</footer>
	</div>
</div>
