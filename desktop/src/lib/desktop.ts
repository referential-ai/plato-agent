export type RunState =
	| 'running'
	| 'finished'
	| 'failed'
	| 'canceled'
	| 'cancel_requested'
	| 'interrupted';

export type ApprovalDecision = 'granted' | 'denied';
export type ApprovalAction = 'grant' | 'deny';

export type DesktopEntry =
	| { kind: 'user'; text: string }
	| { kind: 'assistant'; step: number; text: string }
	| { kind: 'tool_call'; callId: string; tool: string; inputPreview: string }
	| { kind: 'tool_result'; callId: string; summary: string }
	| {
			kind: 'approval';
			callId: string;
			decision: ApprovalDecision;
			actorId: string;
			reason: string | null;
	  }
	| { kind: 'policy_denied'; callId: string; reason: string }
	| { kind: 'tool_failed'; callId: string; error: string };

export interface DesktopRun {
	runId: string;
	sessionIndex: number;
	status: RunState;
	entries: DesktopEntry[];
}

export interface DesktopSession {
	sessionId: string;
	runId: string;
	status: RunState;
	latestQuestion: string;
}

export interface DesktopTranscript {
	runs: DesktopRun[];
}

export interface DesktopPendingApproval {
	runId: string;
	toolCallId: string;
	toolName: string;
	effect: string;
	reason: string | null;
	inputPreview: string | null;
	approvalPreview: string | null;
	diffPreview: string | null;
}

export interface DesktopSubmission {
	runId: string;
	sessionId: string;
	status: RunState;
}

export interface DesktopCommandStatus {
	runId: string;
	status: RunState;
}

interface OffsetEvent {
	offset: number;
}

export type DesktopEvent =
	| (OffsetEvent & {
			kind: 'assistant_delta';
			step: number;
			deltaIndex: number;
			text: string;
	  })
	| (OffsetEvent & { kind: 'assistant_committed'; step: number; text: string })
	| (OffsetEvent & {
			kind: 'tool_call';
			callId: string;
			tool: string;
			inputPreview: string;
	  })
	| (OffsetEvent & { kind: 'tool_result'; callId: string; summary: string })
	| (OffsetEvent & {
			kind: 'approval';
			callId: string;
			decision: ApprovalDecision;
			actorId: string;
			reason: string | null;
	  })
	| (OffsetEvent & { kind: 'policy_denied'; callId: string; reason: string })
	| (OffsetEvent & { kind: 'tool_failed'; callId: string; error: string })
	| (OffsetEvent & { kind: 'approval_requested'; toolCallId: string })
	| (OffsetEvent & { kind: 'cancel_requested' });

export interface DesktopEventPage {
	runId: string;
	fromOffset: number;
	nextOffset: number;
	status: RunState;
	events: DesktopEvent[];
}

export interface DesktopRecovery {
	anchorOffset: number;
	run: DesktopRun;
	pendingApproval: DesktopPendingApproval | null;
	page: DesktopEventPage;
}

export interface DesktopError {
	code: string;
	message: string;
}

export type BootstrapView =
	| { state: 'needs_workspace'; reason: string | null }
	| {
			state: 'ready';
			workspaceRoot: string;
			daemonVersion: string;
			sessions: DesktopSession[];
			selectedRun: DesktopRun | null;
	  };
