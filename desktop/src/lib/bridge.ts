import { invoke } from '@tauri-apps/api/core';
import type { BootstrapView, DesktopRun } from '$lib/desktop';

export function bootstrap(): Promise<BootstrapView> {
	return invoke<BootstrapView>('bootstrap');
}

export function pickWorkspace(): Promise<BootstrapView | null> {
	return invoke<BootstrapView | null>('pick_workspace');
}

export function readRun(runId: string): Promise<DesktopRun> {
	return invoke<DesktopRun>('read_run', { runId });
}
