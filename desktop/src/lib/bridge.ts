import { invoke } from '@tauri-apps/api/core';
import type {
	ApprovalAction,
	BootstrapView,
	DesktopCommandStatus,
	DesktopEventPage,
	DesktopRecovery,
	DesktopSession,
	DesktopSubmission,
	DesktopTranscript
} from '$lib/desktop';

export function bootstrap(): Promise<BootstrapView> {
	return invoke<BootstrapView>('bootstrap');
}

export function pickWorkspace(): Promise<BootstrapView | null> {
	return invoke<BootstrapView | null>('pick_workspace');
}

export function listSessions(): Promise<DesktopSession[]> {
	return invoke<DesktopSession[]>('list_sessions');
}

export function readSession(sessionId: string): Promise<DesktopTranscript> {
	return invoke<DesktopTranscript>('read_session', { sessionId });
}

export function submitMessage(
	message: string,
	sessionId: string | null
): Promise<DesktopSubmission> {
	return invoke<DesktopSubmission>('submit_message', { message, sessionId });
}

export function pollRun(runId: string, fromOffset: number): Promise<DesktopEventPage> {
	return invoke<DesktopEventPage>('poll_run', { runId, fromOffset });
}

export function recoverRun(runId: string): Promise<DesktopRecovery> {
	return invoke<DesktopRecovery>('recover_run', { runId });
}

export function decideApproval(
	runId: string,
	toolCallId: string,
	decision: ApprovalAction,
	reason: string | null = null
): Promise<DesktopCommandStatus> {
	return invoke<DesktopCommandStatus>('decide_approval', {
		runId,
		toolCallId,
		decision,
		reason
	});
}

export function cancelRun(runId: string): Promise<DesktopCommandStatus> {
	return invoke<DesktopCommandStatus>('cancel_run', { runId });
}
