import { afterEach, describe, expect, it } from 'vitest';
import { page } from 'vitest/browser';
import { cleanup, render } from 'vitest-browser-svelte';
import { clearMocks, mockIPC } from '@tauri-apps/api/mocks';
import Page from './+page.svelte';
import type {
	BootstrapView,
	DesktopEventPage,
	DesktopPendingApproval,
	DesktopRecovery,
	DesktopRun,
	DesktopSession,
	DesktopTranscript,
	RunState
} from '$lib/desktop';

const firstRun: DesktopRun = {
	runId: 'run-1',
	sessionIndex: 0,
	status: 'finished',
	entries: [
		{ kind: 'user', text: 'Inspect the workspace' },
		{ kind: 'assistant', step: 0, text: 'I found the relevant module.' }
	]
};

const secondRun: DesktopRun = {
	runId: 'run-2',
	sessionIndex: 1,
	status: 'finished',
	entries: [
		{ kind: 'user', text: 'Run the focused proof' },
		{ kind: 'assistant', step: 0, text: 'The focused proof passed.' }
	]
};

const sessionOne: DesktopSession = {
	sessionId: 'session-1',
	runId: 'run-2',
	status: 'finished',
	latestQuestion: 'Run the focused proof'
};

const ready: Extract<BootstrapView, { state: 'ready' }> = {
	state: 'ready',
	workspaceRoot: '/home/jerome/projects/plato-agent',
	daemonVersion: '0.1.0',
	sessions: [sessionOne],
	selectedRun: secondRun
};

function transcript(...runs: DesktopRun[]): DesktopTranscript {
	return { runs };
}

function run(
	runId: string,
	sessionIndex: number,
	status: RunState,
	question: string,
	answer?: string
): DesktopRun {
	return {
		runId,
		sessionIndex,
		status,
		entries: [
			{ kind: 'user', text: question },
			...(answer === undefined
				? []
				: [{ kind: 'assistant' as const, step: 0, text: answer }])
		]
	};
}

function eventPage(
	runId: string,
	status: RunState,
	events: DesktopEventPage['events'] = [],
	fromOffset = 0,
	nextOffset = fromOffset
): DesktopEventPage {
	return { runId, fromOffset, nextOffset, status, events };
}

function recovery(
	runSnapshot: DesktopRun,
	pendingApproval: DesktopPendingApproval | null = null,
	pageResult: DesktopEventPage = eventPage(runSnapshot.runId, runSnapshot.status)
): DesktopRecovery {
	return {
		anchorOffset: pageResult.fromOffset,
		run: runSnapshot,
		pendingApproval,
		page: pageResult
	};
}

function deferred<T>(): {
	promise: Promise<T>;
	resolve: (value: T) => void;
	reject: (reason: unknown) => void;
} {
	let resolve!: (value: T) => void;
	let reject!: (reason: unknown) => void;
	const promise = new Promise<T>((resolvePromise, rejectPromise) => {
		resolve = resolvePromise;
		reject = rejectPromise;
	});
	return { promise, resolve, reject };
}

async function waitFor(predicate: () => boolean, timeout = 2_000): Promise<void> {
	const deadline = Date.now() + timeout;
	while (!predicate()) {
		if (Date.now() >= deadline) throw new Error('timed out waiting for browser state');
		await new Promise((resolve) => setTimeout(resolve, 10));
	}
}

afterEach(() => {
	cleanup();
	clearMocks();
});

describe('desktop chat', () => {
	it('uses the first-launch picker and renders the full typed session history', async () => {
		const calls: Array<{ command: string; payload: unknown }> = [];
		mockIPC((command, payload) => {
			calls.push({ command, payload });
			if (command === 'bootstrap') return { state: 'needs_workspace', reason: null };
			if (command === 'pick_workspace') return ready;
			if (command === 'read_session') return transcript(firstRun, secondRun);
			throw new Error(`unexpected command ${command}`);
		});

		render(Page);

		await expect.element(page.getByLabelText('Plato Agent')).toBeVisible();
		await expect.element(page.getByRole('heading', { name: 'Select a workspace' })).toBeVisible();
		await page.getByRole('button', { name: 'Choose folder' }).click();
		await expect.element(page.getByPlaceholder('Message Plato Agent')).toBeVisible();
		await expect.element(page.getByText('I found the relevant module.')).toBeVisible();
		await expect.element(page.getByText('The focused proof passed.')).toBeVisible();
		await expect.element(page.getByRole('region', { name: 'Run 1, finished' })).toBeVisible();
		await expect.element(page.getByRole('region', { name: 'Run 2, finished' })).toBeVisible();
		expect(calls).toContainEqual({ command: 'read_session', payload: { sessionId: 'session-1' } });
	});

	it('renders the workspace guard conflict separately from daemon failure', async () => {
		mockIPC((command) => {
			if (command === 'bootstrap') {
				return Promise.reject({
					code: 'desktop_already_open',
					message: 'This workspace is already open in Plato Agent'
				});
			}
			throw new Error(`unexpected command ${command}`);
		});

		render(Page);

		await expect.element(page.getByRole('heading', { name: 'Workspace already open' })).toBeVisible();
		await expect.element(page.getByText('This workspace is already open in Plato Agent')).toBeVisible();
	});

	it('detects an idle daemon exit without restarting it', async () => {
		let listCalls = 0;
		mockIPC((command) => {
			if (command === 'bootstrap') return ready;
			if (command === 'read_session') return transcript(firstRun, secondRun);
			if (command === 'list_sessions') {
				listCalls += 1;
				return Promise.reject({ code: 'daemon_unavailable', message: 'Daemon process exited' });
			}
			throw new Error(`unexpected command ${command}`);
		});

		render(Page);

		await expect.element(page.getByText('The focused proof passed.')).toBeVisible();
		await expect.element(page.getByRole('heading', { name: 'Daemon unavailable' })).toBeVisible();
		expect(listCalls).toBe(1);
	});

	it('stops old-workspace polling before the picker and restores it after cancel', async () => {
		const live = run('run-picker', 0, 'running', 'Keep this workspace active');
		const liveReady: Extract<BootstrapView, { state: 'ready' }> = {
			...ready,
			sessions: [{ ...sessionOne, runId: live.runId, status: 'running' }],
			selectedRun: live
		};
		const picked = deferred<BootstrapView | null>();
		let bootstrapCalls = 0;
		let pollCalls = 0;
		mockIPC((command) => {
			if (command === 'bootstrap') {
				bootstrapCalls += 1;
				return liveReady;
			}
			if (command === 'read_session') return transcript(live);
			if (command === 'recover_run') return recovery(live);
			if (command === 'poll_run') {
				pollCalls += 1;
				return eventPage(live.runId, 'running');
			}
			if (command === 'pick_workspace') return picked.promise;
			if (command === 'list_sessions') return liveReady.sessions;
			throw new Error(`unexpected command ${command}`);
		});

		render(Page);
		await waitFor(() => pollCalls > 0);
		await page.getByRole('button', { name: 'Choose workspace' }).click();
		const pollsAtPicker = pollCalls;
		await new Promise((resolve) => setTimeout(resolve, 300));
		expect(pollCalls).toBe(pollsAtPicker);

		picked.resolve(null);
		await waitFor(() => bootstrapCalls === 2);
		await waitFor(() => pollCalls > pollsAtPicker);
	});

	it('submits a continuation to the selected session and a new chat without one', async () => {
		const submissions: unknown[] = [];
		let submittedRun: DesktopRun | null = null;
		let newChat = false;
		const newRun = run('run-new', 0, 'finished', 'Start from scratch', 'Fresh answer');
		mockIPC((command, payload) => {
			if (command === 'bootstrap') return ready;
			if (command === 'read_session') {
				if ((payload as { sessionId: string }).sessionId === 'session-new') {
					return transcript(newRun);
				}
				return submittedRun ? transcript(firstRun, secondRun, submittedRun) : transcript(firstRun, secondRun);
			}
			if (command === 'submit_message') {
				submissions.push(payload);
				const request = payload as { message: string; sessionId: string | null };
				if (request.sessionId === null) {
					newChat = true;
					return { runId: 'run-new', sessionId: 'session-new', status: 'finished' };
				}
				submittedRun = run('run-3', 2, 'finished', request.message, 'Continuation answer');
				return { runId: 'run-3', sessionId: 'session-1', status: 'finished' };
			}
			if (command === 'list_sessions') {
				return newChat
					? [sessionOne, { ...sessionOne, sessionId: 'session-new', runId: 'run-new' }]
					: [sessionOne];
			}
			if (command === 'poll_run') {
				const runId = (payload as { runId: string }).runId;
				return eventPage(runId, 'finished');
			}
			if (command === 'recover_run') {
				const runId = (payload as { runId: string }).runId;
				if (runId === 'run-new') return recovery(newRun);
				if (!submittedRun) throw new Error('missing submitted run');
				return recovery(submittedRun);
			}
			throw new Error(`unexpected command ${command}`);
		});

		render(Page);

		await expect.element(page.getByText('The focused proof passed.')).toBeVisible();
		await page.getByRole('textbox', { name: 'Message' }).fill('Continue this chat');
		await page.getByRole('button', { name: 'Send message' }).click();
		await expect.element(page.getByText('Continuation answer')).toBeVisible();
		await page.getByRole('button', { name: 'New chat' }).click();
		await page.getByRole('textbox', { name: 'Message' }).fill('Start from scratch');
		await page.getByRole('button', { name: 'Send message' }).click();
		await expect.element(page.getByText('Fresh answer')).toBeVisible();

		expect(submissions).toEqual([
			{ message: 'Continue this chat', sessionId: 'session-1' },
			{ message: 'Start from scratch', sessionId: null }
		]);
	});

	it('ignores a stale session read after switching without canceling either run', async () => {
		const oldRead = deferred<DesktopTranscript>();
		const commands: string[] = [];
		let sessionOneReads = 0;
		const sessions: DesktopSession[] = [
			{ ...sessionOne, runId: 'run-1', latestQuestion: 'Session one' },
			{
				sessionId: 'session-2',
				runId: 'run-other',
				status: 'finished',
				latestQuestion: 'Session two'
			}
		];
		const current = run('run-1', 0, 'finished', 'Session one', 'Current session answer');
		const stale = run('run-other', 0, 'finished', 'Session two', 'Stale session answer');
		mockIPC((command, payload) => {
			commands.push(command);
			if (command === 'bootstrap') return { ...ready, sessions, selectedRun: current };
			if (command === 'read_session') {
				const sessionId = (payload as { sessionId: string }).sessionId;
				if (sessionId === 'session-2') return oldRead.promise;
				sessionOneReads += 1;
				return transcript(current);
			}
			throw new Error(`unexpected command ${command}`);
		});

		render(Page);

		await expect.element(page.getByText('Current session answer')).toBeVisible();
		await page.getByRole('button', { name: /Session two/ }).click();
		await expect.element(page.getByText('Loading chat...')).toBeVisible();
		await page.getByRole('button', { name: /Session one/ }).click();
		await waitFor(() => sessionOneReads === 2);
		oldRead.resolve(transcript(stale));
		await new Promise((resolve) => setTimeout(resolve, 0));

		await expect.element(page.getByText('Current session answer')).toBeVisible();
		await expect.element(page.getByText('Stale session answer')).not.toBeInTheDocument();
		expect(commands).not.toContain('cancel_run');
	});

	it('binds recovery and cancel to the latest typed run when the session summary is stale', async () => {
		const old = run('run-old', 0, 'finished', 'Old question', 'Old answer');
		const latest = run('run-latest', 1, 'running', 'Latest question', 'Latest answer');
		const blockedPoll = deferred<DesktopEventPage>();
		const recoveredRunIds: string[] = [];
		let cancelPayload: unknown;
		mockIPC((command, payload) => {
			if (command === 'bootstrap') {
				return {
					...ready,
					sessions: [
						{
							sessionId: 'session-stale',
							runId: old.runId,
							status: 'finished',
							latestQuestion: 'Old question'
						}
					],
					selectedRun: old
				};
			}
			if (command === 'read_session') return transcript(old, latest);
			if (command === 'recover_run') {
				const runId = (payload as { runId: string }).runId;
				recoveredRunIds.push(runId);
				return recovery(latest);
			}
			if (command === 'poll_run') return blockedPoll.promise;
			if (command === 'cancel_run') {
				cancelPayload = payload;
				return { runId: latest.runId, status: 'cancel_requested' };
			}
			throw new Error(`unexpected command ${command}`);
		});

		render(Page);

		await expect.element(page.getByText('Latest answer')).toBeVisible();
		await expect.element(page.getByRole('button', { name: 'Cancel run' })).toBeEnabled();
		await page.getByRole('button', { name: 'Cancel run' }).click();

		expect(recoveredRunIds).toEqual(['run-latest']);
		expect(cancelPayload).toEqual({ runId: 'run-latest' });
	});

	it('clears composer drafts at session, new-chat, and workspace boundaries', async () => {
		const initial = run('run-initial', 0, 'finished', 'Initial question', 'Initial answer');
		const other = run('run-other', 0, 'finished', 'Other question', 'Other answer');
		const workspaceRun = run('run-workspace', 0, 'finished', 'Workspace question', 'Workspace answer');
		const sessions: DesktopSession[] = [
			{
				sessionId: 'session-initial',
				runId: initial.runId,
				status: 'finished',
				latestQuestion: 'Initial question'
			},
			{
				sessionId: 'session-other',
				runId: other.runId,
				status: 'finished',
				latestQuestion: 'Other question'
			}
		];
		mockIPC((command, payload) => {
			if (command === 'bootstrap') return { ...ready, sessions, selectedRun: initial };
			if (command === 'pick_workspace') {
				return {
					...ready,
					workspaceRoot: '/home/jerome/projects/other-workspace',
					sessions: [
						{
							sessionId: 'session-workspace',
							runId: workspaceRun.runId,
							status: 'finished',
							latestQuestion: 'Workspace question'
						}
					],
					selectedRun: workspaceRun
				};
			}
			if (command === 'read_session') {
				switch ((payload as { sessionId: string }).sessionId) {
					case 'session-initial':
						return transcript(initial);
					case 'session-other':
						return transcript(other);
					case 'session-workspace':
						return transcript(workspaceRun);
					default:
						throw new Error('unexpected session');
				}
			}
			throw new Error(`unexpected command ${command}`);
		});

		render(Page);

		const message = page.getByRole('textbox', { name: 'Message' });
		await expect.element(page.getByText('Initial answer')).toBeVisible();
		await message.fill('Session boundary draft');
		await expect.element(message).toHaveValue('Session boundary draft');
		await page.getByRole('button', { name: /Other question/ }).click();
		await expect.element(page.getByText('Other answer')).toBeVisible();
		await expect.element(message).toHaveValue('');

		await message.fill('New chat boundary draft');
		await expect.element(message).toHaveValue('New chat boundary draft');
		await page.getByRole('button', { name: 'New chat' }).click();
		await expect.element(page.getByRole('heading', { name: 'New chat' })).toBeVisible();
		await expect.element(message).toHaveValue('');

		await message.fill('Workspace boundary draft');
		await expect.element(message).toHaveValue('Workspace boundary draft');
		await page.getByRole('button', { name: 'Choose workspace' }).click();
		await expect.element(page.getByText('Workspace answer')).toBeVisible();
		await expect.element(message).toHaveValue('');
	});

	it('replaces live assistant deltas with the committed response without duplication', async () => {
		const live = run('run-live', 0, 'running', 'Stream an answer');
		const poll = deferred<DesktopEventPage>();
		const blockedRecovery = deferred<DesktopRecovery>();
		let recoveryCalls = 0;
		let pollCalls = 0;
		const liveReady: Extract<BootstrapView, { state: 'ready' }> = {
			...ready,
			sessions: [
				{
					sessionId: 'session-live',
					runId: 'run-live',
					status: 'running',
					latestQuestion: 'Stream an answer'
				}
			],
			selectedRun: live
		};
		mockIPC((command) => {
			if (command === 'bootstrap') return liveReady;
			if (command === 'read_session') return transcript(live);
			if (command === 'recover_run') {
				recoveryCalls += 1;
				if (recoveryCalls > 1) return blockedRecovery.promise;
				return recovery(
					live,
					null,
					eventPage(
						'run-live',
						'running',
						[
							{
								kind: 'assistant_delta',
								offset: 7,
								step: 0,
								deltaIndex: 0,
								text: 'Draft answer'
							}
						],
						7,
						8
					)
				);
			}
			if (command === 'poll_run') {
				pollCalls += 1;
				return poll.promise;
			}
			throw new Error(`unexpected command ${command}`);
		});

		render(Page);

		await expect.element(page.getByRole('region', { name: 'Plato Agent, responding' })).toHaveTextContent('Draft answer');
		await waitFor(() => pollCalls === 1);
		poll.resolve(
			eventPage(
				'run-live',
				'finished',
				[{ kind: 'assistant_committed', offset: 8, step: 0, text: 'Canonical answer' }],
				8,
				9
			)
		);

		await expect.element(page.getByRole('region', { name: 'Plato Agent' })).toHaveTextContent('Canonical answer');
		await expect.element(page.getByText('Draft answer', { exact: true })).not.toBeInTheDocument();
		await waitFor(() => document.querySelectorAll('.assistant-message').length === 1);
	});

	it('retries a failed lag recovery and resumes exact-run polling', async () => {
		const live = run('run-lagged', 0, 'running', 'Recover after lag', 'Stable answer');
		const blockedPoll = deferred<DesktopEventPage>();
		let recoveryCalls = 0;
		let pollCalls = 0;
		const recoveryRunIds: string[] = [];
		mockIPC((command, payload) => {
			if (command === 'bootstrap') {
				return {
					...ready,
					sessions: [
						{
							sessionId: 'session-lagged',
							runId: live.runId,
							status: 'running',
							latestQuestion: 'Recover after lag'
						}
					],
					selectedRun: live
				};
			}
			if (command === 'read_session') return transcript(live);
			if (command === 'recover_run') {
				recoveryCalls += 1;
				recoveryRunIds.push((payload as { runId: string }).runId);
				if (recoveryCalls === 2) {
					return Promise.reject({ code: 'socket_reset', message: 'Recovery connection reset' });
				}
				return recovery(live, null, eventPage(live.runId, 'running', [], 12, 12));
			}
			if (command === 'poll_run') {
				pollCalls += 1;
				if (pollCalls === 1) {
					return Promise.reject({ code: 'lagged', message: 'Event cursor lagged' });
				}
				return blockedPoll.promise;
			}
			throw new Error(`unexpected command ${command}`);
		});

		render(Page);

		await expect.element(page.getByText('Stable answer')).toBeVisible();
		await expect.element(page.getByText('Recovery connection reset')).toBeVisible();
		await waitFor(() => recoveryCalls >= 3 && pollCalls >= 2, 3_000);

		await expect.element(page.getByText('Recovery connection reset')).not.toBeInTheDocument();
		expect(recoveryRunIds).toEqual(['run-lagged', 'run-lagged', 'run-lagged']);
	});

	it('stops active polling after daemon loss until explicit reconnect', async () => {
		const live = run('run-disconnected', 0, 'running', 'Keep streaming', 'Partial answer');
		const liveReady: Extract<BootstrapView, { state: 'ready' }> = {
			...ready,
			sessions: [
				{
					sessionId: 'session-disconnected',
					runId: live.runId,
					status: 'running',
					latestQuestion: 'Keep streaming'
				}
			],
			selectedRun: live
		};
		let bootstrapCalls = 0;
		let pollCalls = 0;
		let requestCount = 0;
		mockIPC((command) => {
			requestCount += 1;
			if (command === 'bootstrap') {
				bootstrapCalls += 1;
				return bootstrapCalls === 1 ? liveReady : ready;
			}
			if (command === 'read_session') {
				return bootstrapCalls === 1 ? transcript(live) : transcript(firstRun, secondRun);
			}
			if (command === 'recover_run') return recovery(live);
			if (command === 'poll_run') {
				pollCalls += 1;
				return Promise.reject({ code: 'daemon_unavailable', message: 'Daemon process exited' });
			}
			throw new Error(`unexpected command ${command}`);
		});

		render(Page);

		await expect.element(page.getByRole('heading', { name: 'Daemon unavailable' })).toBeVisible();
		const requestsAtDisconnect = requestCount;
		await new Promise((resolve) => setTimeout(resolve, 650));
		expect(requestCount).toBe(requestsAtDisconnect);
		expect(pollCalls).toBe(1);

		await page.getByRole('button', { name: 'Reconnect' }).click();
		await expect.element(page.getByText('The focused proof passed.')).toBeVisible();
		expect(bootstrapCalls).toBe(2);
		expect(requestCount).toBeGreaterThan(requestsAtDisconnect);
	});

	it('does not retry recovery after daemon loss', async () => {
		const live = run('run-recovery-disconnected', 0, 'running', 'Recover this run');
		let recoveryCalls = 0;
		let requestCount = 0;
		mockIPC((command) => {
			requestCount += 1;
			if (command === 'bootstrap') {
				return {
					...ready,
					sessions: [{ ...sessionOne, runId: live.runId, status: 'running' }],
					selectedRun: live
				};
			}
			if (command === 'read_session') return transcript(live);
			if (command === 'recover_run') {
				recoveryCalls += 1;
				return Promise.reject({ code: 'daemon_unavailable', message: 'Daemon process exited' });
			}
			throw new Error(`unexpected command ${command}`);
		});

		render(Page);

		await expect.element(page.getByRole('heading', { name: 'Daemon unavailable' })).toBeVisible();
		const requestsAtDisconnect = requestCount;
		await new Promise((resolve) => setTimeout(resolve, 650));
		expect(requestCount).toBe(requestsAtDisconnect);
		expect(recoveryCalls).toBe(1);
	});

	it('restores a pending approval on reconnect and clears it after external resolution', async () => {
		const live = run('run-approval', 0, 'running', 'Write the file');
		const finished: DesktopRun = {
			...live,
			status: 'finished',
			entries: [
				...live.entries,
				{
					kind: 'approval',
					callId: 'call-1',
					decision: 'granted',
					actorId: 'jerome',
					reason: null
				}
			]
		};
		const pending: DesktopPendingApproval = {
			runId: 'run-approval',
			toolCallId: 'call-1',
			toolName: 'shell.exec',
			effect: 'writes_workspace',
			reason: 'The command writes a file.',
			inputPreview: 'printf hello > test.txt',
			approvalPreview: null,
			diffPreview: null
		};
		const poll = deferred<DesktopEventPage>();
		let recoveryCalls = 0;
		let resolved = false;
		mockIPC((command) => {
			if (command === 'bootstrap') {
				return {
					...ready,
					sessions: [{ ...sessionOne, runId: live.runId, status: 'running' }],
					selectedRun: live
				};
			}
			if (command === 'read_session') return transcript(resolved ? finished : live);
			if (command === 'recover_run') {
				recoveryCalls += 1;
				return recovery(recoveryCalls === 1 ? live : finished, recoveryCalls === 1 ? pending : null);
			}
			if (command === 'poll_run') return poll.promise;
			if (command === 'list_sessions') return [{ ...sessionOne, runId: finished.runId, status: 'finished' }];
			throw new Error(`unexpected command ${command}`);
		});

		render(Page);

		await expect.element(page.getByRole('dialog', { name: 'shell.exec' })).toBeVisible();
		await waitFor(() => recoveryCalls === 1);
		resolved = true;
		poll.resolve(
			eventPage(
				'run-approval',
				'finished',
				[
					{
						kind: 'approval',
						offset: 4,
						callId: 'call-1',
						decision: 'granted',
						actorId: 'jerome',
						reason: null
					}
				],
				4,
				5
			)
		);

		await expect.element(page.getByRole('dialog', { name: 'shell.exec' })).not.toBeInTheDocument();
		await expect.element(page.getByRole('region', { name: 'Approval' })).toHaveTextContent('Approved');
	});

	it('clears a resolved snapshot approval even when the next recovery attempt fails', async () => {
		const live = run('run-fold-race', 0, 'running', 'Approve the write', 'Ready transcript');
		const blockedRecovery = deferred<DesktopRecovery>();
		const pending: DesktopPendingApproval = {
			runId: live.runId,
			toolCallId: 'call-fold-race',
			toolName: 'shell.exec',
			effect: 'writes_workspace',
			reason: null,
			inputPreview: 'printf hello > test.txt',
			approvalPreview: null,
			diffPreview: null
		};
		let recoveryCalls = 0;
		mockIPC((command) => {
			if (command === 'bootstrap') {
				return {
					...ready,
					sessions: [{ ...sessionOne, runId: live.runId, status: 'running' }],
					selectedRun: live
				};
			}
			if (command === 'read_session') return transcript(live);
			if (command === 'recover_run') {
				recoveryCalls += 1;
				if (recoveryCalls === 2) {
					return Promise.reject({ code: 'socket_reset', message: 'Continuation recovery failed' });
				}
				if (recoveryCalls > 2) return blockedRecovery.promise;
				return recovery(
					live,
					pending,
					eventPage(
						live.runId,
						'running',
						[
							{
								kind: 'approval',
								offset: 8,
								callId: pending.toolCallId,
								decision: 'granted',
								actorId: 'remote-user',
								reason: null
							}
						],
						8,
						9
					)
				);
			}
			throw new Error(`unexpected command ${command}`);
		});

		render(Page);

		await expect.element(page.getByText('Ready transcript')).toBeVisible();
		await expect.element(page.getByRole('dialog', { name: 'shell.exec' })).not.toBeInTheDocument();
		await expect.element(page.getByText('Continuation recovery failed')).toBeVisible();
		await expect.element(page.getByRole('heading', { name: 'Daemon unavailable' })).not.toBeInTheDocument();
		expect(recoveryCalls).toBeGreaterThanOrEqual(2);
	});

	it('keeps a raced approval error inline after recovery and a later successful poll', async () => {
		const live = run('run-race', 0, 'running', 'Run the command');
		const pending: DesktopPendingApproval = {
			runId: 'run-race',
			toolCallId: 'call-race',
			toolName: 'shell.exec',
			effect: 'executes_process',
			reason: null,
			inputPreview: 'cargo test',
			approvalPreview: null,
			diffPreview: null
		};
		const successfulPoll = deferred<DesktopEventPage>();
		const blockedPoll = deferred<DesktopEventPage>();
		let recoveryCalls = 0;
		let pollCalls = 0;
		mockIPC((command) => {
			if (command === 'bootstrap') {
				return {
					...ready,
					sessions: [{ ...sessionOne, runId: live.runId, status: 'running' }],
					selectedRun: live
				};
			}
			if (command === 'read_session') return transcript(live);
			if (command === 'recover_run') {
				recoveryCalls += 1;
				return recovery(live, recoveryCalls === 1 ? pending : null);
			}
			if (command === 'poll_run') {
				pollCalls += 1;
				return pollCalls === 1 ? successfulPoll.promise : blockedPoll.promise;
			}
			if (command === 'decide_approval') {
				return Promise.reject({ code: 'not_found', message: 'Approval already resolved' });
			}
			throw new Error(`unexpected command ${command}`);
		});

		render(Page);

		await expect.element(page.getByRole('dialog', { name: 'shell.exec' })).toBeVisible();
		await page.getByRole('button', { name: 'Grant' }).click();
		await expect.element(page.getByRole('dialog', { name: 'shell.exec' })).not.toBeInTheDocument();
		await expect.element(page.getByText('Approval already resolved')).toBeVisible();
		await waitFor(() => pollCalls === 1);
		successfulPoll.resolve(
			eventPage(
				live.runId,
				'running',
				[{ kind: 'tool_result', offset: 3, callId: 'call-ordinary', summary: 'Ordinary poll applied' }],
				3,
				4
			)
		);
		await expect.element(page.getByText('Ordinary poll applied')).toBeVisible();
		await expect.element(page.getByText('Approval already resolved')).toBeVisible();
		await expect.element(page.getByRole('heading', { name: 'Daemon unavailable' })).not.toBeInTheDocument();
		expect(recoveryCalls).toBe(2);
	});

	it('keeps cancel requested sticky after the cancel response', async () => {
		const live = run('run-cancel', 0, 'running', 'Keep working');
		const blockedPoll = deferred<DesktopEventPage>();
		let cancelPayload: unknown;
		mockIPC((command, payload) => {
			if (command === 'bootstrap') {
				return {
					...ready,
					sessions: [{ ...sessionOne, runId: live.runId, status: 'running' }],
					selectedRun: live
				};
			}
			if (command === 'read_session') return transcript(live);
			if (command === 'recover_run') return recovery(live);
			if (command === 'poll_run') return blockedPoll.promise;
			if (command === 'cancel_run') {
				cancelPayload = payload;
				return { runId: 'run-cancel', status: 'cancel_requested' };
			}
			throw new Error(`unexpected command ${command}`);
		});

		render(Page);

		await expect.element(page.getByRole('button', { name: 'Cancel run' })).toBeVisible();
		await page.getByRole('button', { name: 'Cancel run' }).click();
		await expect.element(page.getByRole('button', { name: 'Cancel run' })).not.toBeInTheDocument();
		await waitFor(() => document.body.textContent?.includes('cancel requested') === true);
		expect(cancelPayload).toEqual({ runId: 'run-cancel' });
	});

	it('keeps the ready transcript and composer when the selected session is overloaded', async () => {
		mockIPC((command) => {
			if (command === 'bootstrap') return ready;
			if (command === 'read_session') return transcript(firstRun, secondRun);
			if (command === 'submit_message') {
				return Promise.reject({
					code: 'overload',
					message: 'This chat already has an active run'
				});
			}
			throw new Error(`unexpected command ${command}`);
		});

		render(Page);

		await expect.element(page.getByText('The focused proof passed.')).toBeVisible();
		await page.getByRole('textbox', { name: 'Message' }).fill('Overlapping message');
		await page.getByRole('button', { name: 'Send message' }).click();

		await expect.element(page.getByText('This chat already has an active run')).toBeVisible();
		await expect.element(page.getByText('The focused proof passed.')).toBeVisible();
		await expect.element(page.getByRole('textbox', { name: 'Message' })).toBeEnabled();
		await expect.element(page.getByRole('heading', { name: 'Daemon unavailable' })).not.toBeInTheDocument();
	});

	it('leaves the ready screen when a command reports daemon loss', async () => {
		mockIPC((command) => {
			if (command === 'bootstrap') return ready;
			if (command === 'read_session') return transcript(firstRun, secondRun);
			if (command === 'submit_message') {
				return Promise.reject({ code: 'daemon_unavailable', message: 'Daemon process exited' });
			}
			throw new Error(`unexpected command ${command}`);
		});

		render(Page);

		await expect.element(page.getByText('The focused proof passed.')).toBeVisible();
		await page.getByRole('textbox', { name: 'Message' }).fill('Continue after disconnect');
		await page.getByRole('button', { name: 'Send message' }).click();
		await expect.element(page.getByRole('heading', { name: 'Daemon unavailable' })).toBeVisible();
		await expect.element(page.getByText('Daemon process exited')).toBeVisible();
	});

	it('isolates two active sessions when an old poll completes after switching', async () => {
		const runOne = run('run-active-1', 0, 'running', 'First active chat', 'First current text');
		const runTwo = run('run-active-2', 0, 'running', 'Second active chat', 'Second current text');
		const firstPoll = deferred<DesktopEventPage>();
		const secondPoll = deferred<DesktopEventPage>();
		const commands: string[] = [];
		let firstPollStarted = false;
		const sessions: DesktopSession[] = [
			{
				sessionId: 'active-1',
				runId: runOne.runId,
				status: 'running',
				latestQuestion: 'First active chat'
			},
			{
				sessionId: 'active-2',
				runId: runTwo.runId,
				status: 'running',
				latestQuestion: 'Second active chat'
			}
		];
		mockIPC((command, payload) => {
			commands.push(command);
			if (command === 'bootstrap') return { ...ready, sessions, selectedRun: runOne };
			if (command === 'read_session') {
				return transcript((payload as { sessionId: string }).sessionId === 'active-1' ? runOne : runTwo);
			}
			if (command === 'recover_run') {
				return recovery((payload as { runId: string }).runId === runOne.runId ? runOne : runTwo);
			}
			if (command === 'poll_run') {
				if ((payload as { runId: string }).runId === runOne.runId) {
					firstPollStarted = true;
					return firstPoll.promise;
				}
				return secondPoll.promise;
			}
			throw new Error(`unexpected command ${command}`);
		});

		render(Page);

		await expect.element(page.getByText('First current text')).toBeVisible();
		await waitFor(() => firstPollStarted);
		await page.getByRole('button', { name: /Second active chat/ }).click();
		await expect.element(page.getByText('Second current text')).toBeVisible();
		firstPoll.resolve(
			eventPage(
				runOne.runId,
				'running',
				[
					{
						kind: 'assistant_delta',
						offset: 20,
						step: 1,
						deltaIndex: 0,
						text: 'Leaked from the old chat'
					}
				],
				20,
				21
			)
		);
		await new Promise((resolve) => setTimeout(resolve, 0));

		await expect.element(page.getByText('Second current text')).toBeVisible();
		await expect.element(page.getByText('Leaked from the old chat')).not.toBeInTheDocument();
		expect(commands).not.toContain('cancel_run');
	});
});
