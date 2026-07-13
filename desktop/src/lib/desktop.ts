export type RunState =
	| 'running'
	| 'finished'
	| 'failed'
	| 'canceled'
	| 'cancel_requested'
	| 'interrupted';

export type ApprovalDecision = 'granted' | 'denied';

export type DesktopEntry =
	| { kind: 'user'; text: string }
	| { kind: 'assistant'; text: string }
	| { kind: 'tool_call'; callId: string; tool: string; input: unknown }
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

export type BootstrapView =
	| { state: 'needs_workspace'; reason: string | null }
	| {
			state: 'ready';
			workspaceRoot: string;
			daemonVersion: string;
			sessions: DesktopSession[];
			selectedRun: DesktopRun | null;
	  };
