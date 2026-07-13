<script lang="ts">
	import { onMount } from 'svelte';
	import { FolderOpen, RefreshCw } from '@lucide/svelte';
	import { bootstrap, pickWorkspace, readRun } from '$lib/bridge';
	import SessionList from '$lib/components/SessionList.svelte';
	import Transcript from '$lib/components/Transcript.svelte';
	import { Button } from '$lib/components/ui/button';
	import type { BootstrapView, DesktopRun } from '$lib/desktop';

	type Screen =
		| { state: 'loading' }
		| { state: 'needs_workspace'; reason: string | null }
		| { state: 'unavailable'; message: string }
		| { state: 'ready'; data: Extract<BootstrapView, { state: 'ready' }>; run: DesktopRun | null };

	let screen = $state<Screen>({ state: 'loading' });
	let selectingWorkspace = $state(false);
	let loadingRunId: string | null = $state(null);
	let viewGeneration = 0;

	const workspaceName = $derived(
		screen.state === 'ready'
			? screen.data.workspaceRoot.split(/[\\/]/).filter(Boolean).at(-1) || screen.data.workspaceRoot
			: null
	);

	onMount(() => {
		void loadBootstrap();
	});

	function errorMessage(error: unknown): string {
		return error instanceof Error ? error.message : String(error);
	}

	function applyBootstrap(result: BootstrapView): void {
		viewGeneration += 1;
		loadingRunId = null;
		if (result.state === 'needs_workspace') {
			screen = result;
			return;
		}
		screen = { state: 'ready', data: result, run: result.selectedRun };
	}

	async function loadBootstrap(): Promise<void> {
		const generation = ++viewGeneration;
		loadingRunId = null;
		screen = { state: 'loading' };
		try {
			const result = await bootstrap();
			if (generation === viewGeneration) applyBootstrap(result);
		} catch (error) {
			if (generation === viewGeneration) {
				screen = { state: 'unavailable', message: errorMessage(error) };
			}
		}
	}

	async function chooseWorkspace(): Promise<void> {
		const generation = ++viewGeneration;
		loadingRunId = null;
		selectingWorkspace = true;
		try {
			const result = await pickWorkspace();
			if (result && generation === viewGeneration) applyBootstrap(result);
		} catch (error) {
			if (generation === viewGeneration) {
				screen = { state: 'unavailable', message: errorMessage(error) };
			}
		} finally {
			selectingWorkspace = false;
		}
	}

	async function selectRun(runId: string): Promise<void> {
		if (screen.state !== 'ready' || runId === screen.run?.runId) return;
		const generation = viewGeneration;
		loadingRunId = runId;
		try {
			const run = await readRun(runId);
			if (
				generation === viewGeneration &&
				loadingRunId === runId &&
				screen.state === 'ready'
			) {
				screen = { ...screen, run };
			}
		} catch (error) {
			if (generation === viewGeneration) {
				screen = { state: 'unavailable', message: errorMessage(error) };
			}
		} finally {
			if (generation === viewGeneration && loadingRunId === runId) loadingRunId = null;
		}
	}
</script>

<svelte:head><title>{workspaceName ? `${workspaceName} - Plato` : 'Plato'}</title></svelte:head>

<main>
	<header class="app-header">
		<div class="brand" aria-label="Plato">
			<span class="brand-mark">P</span>
			<span>Plato</span>
		</div>
		{#if screen.state === 'ready'}
			<div class="workspace-label" title={screen.data.workspaceRoot}>
				<span class="connection-dot"></span>
				<span>{workspaceName}</span>
			</div>
		{/if}
		<div class="header-actions">
			{#if screen.state === 'ready'}
				<span class="daemon-version">daemon {screen.data.daemonVersion}</span>
			{/if}
			<Button
				variant="ghost"
				size="icon"
				title="Choose workspace"
				aria-label="Choose workspace"
				disabled={selectingWorkspace || screen.state === 'loading'}
				onclick={chooseWorkspace}
			>
				<FolderOpen />
			</Button>
		</div>
	</header>

	{#if screen.state === 'loading'}
		<section class="center-state" aria-live="polite">
			<div class="loading-mark"></div>
			<p>Connecting to plato-agentd...</p>
		</section>
	{:else if screen.state === 'needs_workspace'}
		<section class="center-state">
			<div class="state-icon"><FolderOpen size={22} /></div>
			<h1>Select a workspace</h1>
			{#if screen.reason}<p class="state-detail">{screen.reason}</p>{/if}
			<Button disabled={selectingWorkspace} onclick={chooseWorkspace}>
				<FolderOpen data-icon="inline-start" />
				{selectingWorkspace ? 'Opening...' : 'Choose folder'}
			</Button>
		</section>
	{:else if screen.state === 'unavailable'}
		<section class="center-state" role="alert">
			<div class="offline-mark"></div>
			<h1>Daemon unavailable</h1>
			<p class="state-detail">{screen.message}</p>
			<div class="state-actions">
				<Button onclick={loadBootstrap}><RefreshCw data-icon="inline-start" />Reconnect</Button>
				<Button variant="outline" disabled={selectingWorkspace} onclick={chooseWorkspace}>
					<FolderOpen data-icon="inline-start" />Choose folder
				</Button>
			</div>
		</section>
	{:else}
		<div class="app-shell">
			<aside>
				<div class="pane-title">
					<h1>Sessions</h1>
					<span>{screen.data.sessions.length}</span>
				</div>
				{#if screen.data.sessions.length === 0}
					<p class="empty-list">No sessions yet.</p>
				{:else}
					<SessionList
						sessions={screen.data.sessions}
						selectedRunId={screen.run?.runId ?? null}
						disabled={loadingRunId !== null}
						onselect={selectRun}
					/>
				{/if}
			</aside>
			<section class="run-pane">
				{#if loadingRunId}
					<div class="loading-run" aria-live="polite">Loading run...</div>
				{:else if screen.run}
					<header class="run-header">
						<div>
							<h2>Run {screen.run.sessionIndex}</h2>
							<span>{screen.run.runId}</span>
						</div>
						<span class={`run-status status-${screen.run.status}`}>
							{screen.run.status.replace('_', ' ')}
						</span>
					</header>
					<Transcript run={screen.run} />
				{:else}
					<div class="empty-run-pane">Select a session to read its transcript.</div>
				{/if}
			</section>
		</div>
	{/if}
</main>
