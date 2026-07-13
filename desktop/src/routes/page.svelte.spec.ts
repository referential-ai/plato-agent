import { afterEach, describe, expect, it } from 'vitest';
import { page } from 'vitest/browser';
import { render } from 'vitest-browser-svelte';
import { clearMocks, mockIPC } from '@tauri-apps/api/mocks';
import Page from './+page.svelte';
import type { BootstrapView, DesktopRun } from '$lib/desktop';

const firstRun: DesktopRun = {
	runId: 'run-1',
	sessionIndex: 1,
	status: 'finished',
	entries: [
		{ kind: 'user', text: 'Inspect the workspace' },
		{ kind: 'assistant', text: 'I found the relevant module.' },
		{ kind: 'tool_call', callId: 'call-1', tool: 'shell.exec', input: { command: 'cargo test' } },
		{ kind: 'tool_result', callId: 'call-1', summary: 'Tests passed' },
		{
			kind: 'approval',
			callId: 'call-2',
			decision: 'granted',
			actorId: 'jerome',
			reason: 'Expected command'
		},
		{ kind: 'policy_denied', callId: 'call-3', reason: 'Outside workspace' },
		{ kind: 'tool_failed', callId: 'call-4', error: 'Command exited 1' }
	]
};

const ready: Extract<BootstrapView, { state: 'ready' }> = {
	state: 'ready',
	workspaceRoot: '/home/jerome/projects/plato-agent',
	daemonVersion: '0.1.0',
	sessions: [
		{
			sessionId: 'session-1',
			runId: 'run-1',
			status: 'finished',
			latestQuestion: 'Inspect the workspace'
		},
		{
			sessionId: 'session-2',
			runId: 'run-2',
			status: 'running',
			latestQuestion: 'Run the focused proof'
		}
	],
	selectedRun: firstRun
};

afterEach(() => {
	clearMocks();
});

describe('desktop readback', () => {
	it('opens the native picker on first launch and renders the selected workspace', async () => {
		const commands: string[] = [];
		mockIPC((command) => {
			commands.push(command);
			if (command === 'bootstrap') return { state: 'needs_workspace', reason: null };
			if (command === 'pick_workspace') return ready;
			throw new Error(`unexpected command ${command}`);
		});

		render(Page);

		await expect.element(page.getByRole('heading', { name: 'Select a workspace' })).toBeVisible();
		await page.getByRole('button', { name: 'Choose folder' }).click();
		await expect.element(page.getByRole('heading', { name: 'Sessions' })).toBeVisible();
		await expect.element(page.getByText('plato-agent', { exact: true })).toBeVisible();
		expect(commands).toEqual(['bootstrap', 'pick_workspace']);
	});

	it('shows a missing saved workspace reason without touching the daemon surface', async () => {
		const commands: string[] = [];
		mockIPC((command) => {
			commands.push(command);
			return { state: 'needs_workspace', reason: 'Saved workspace no longer exists' };
		});

		render(Page);

		await expect.element(page.getByText('Saved workspace no longer exists')).toBeVisible();
		expect(commands).toEqual(['bootstrap']);
	});

	it('renders every typed entry without consulting a legacy transcript', async () => {
		mockIPC((command) => {
			if (command === 'bootstrap') {
				return { ...ready, transcript: 'POISON LEGACY TRANSCRIPT' };
			}
			throw new Error(`unexpected command ${command}`);
		});

		render(Page);

		await expect.element(page.getByRole('region', { name: 'You' })).toHaveTextContent('Inspect the workspace');
		await expect.element(page.getByRole('region', { name: 'Plato' })).toHaveTextContent('I found');
		await expect.element(page.getByRole('region', { name: 'Tool call shell.exec' })).toHaveTextContent('cargo test');
		await expect.element(page.getByRole('region', { name: 'Tool result' })).toHaveTextContent('Tests passed');
		await expect.element(page.getByRole('region', { name: 'Approval' })).toHaveTextContent('Approved');
		await expect.element(page.getByRole('region', { name: 'Policy denied' })).toHaveTextContent('Outside workspace');
		await expect.element(page.getByRole('region', { name: 'Tool failed' })).toHaveTextContent('Command exited 1');
		await expect.element(page.getByText('POISON LEGACY TRANSCRIPT')).not.toBeInTheDocument();
	});

	it('requests an exact run using only its opaque id', async () => {
		const secondRun: DesktopRun = {
			runId: 'run-2',
			sessionIndex: 2,
			status: 'running',
			entries: [{ kind: 'assistant', text: 'Focused proof is running.' }]
		};
		let readPayload: unknown;
		mockIPC((command, payload) => {
			if (command === 'bootstrap') return ready;
			if (command === 'read_run') {
				readPayload = payload;
				return secondRun;
			}
			throw new Error(`unexpected command ${command}`);
		});

		render(Page);

		await page.getByRole('button', { name: /Run the focused proof/ }).click();
		await expect.element(page.getByText('Focused proof is running.')).toBeVisible();
		expect(readPayload).toEqual({ runId: 'run-2' });
	});

	it('surfaces daemon errors and retries through bootstrap', async () => {
		let attempts = 0;
		mockIPC((command) => {
			if (command !== 'bootstrap') throw new Error(`unexpected command ${command}`);
			attempts += 1;
			if (attempts === 1) throw new Error('socket refused');
			return ready;
		});

		render(Page);

		await expect.element(page.getByRole('heading', { name: 'Daemon unavailable' })).toBeVisible();
		await expect.element(page.getByText('socket refused')).toBeVisible();
		await page.getByRole('button', { name: 'Reconnect' }).click();
		await expect.element(page.getByRole('heading', { name: 'Sessions' })).toBeVisible();
		expect(attempts).toBe(2);
	});
});
