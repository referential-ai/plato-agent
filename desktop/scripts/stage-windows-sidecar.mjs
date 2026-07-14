import { execFileSync } from 'node:child_process';
import { copyFileSync, mkdirSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

if (process.platform !== 'win32') {
	throw new Error('the Windows sidecar must be staged on Windows');
}

const desktop = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const root = resolve(desktop, '..');
const target = 'x86_64-pc-windows-msvc';
const cargoTarget = process.env.CARGO_TARGET_DIR
	? resolve(root, process.env.CARGO_TARGET_DIR)
	: join(root, 'target');

execFileSync(
	'cargo',
	['build', '--locked', '--release', '--target', target, '--bin', 'plato-agentd'],
	{ cwd: root, stdio: 'inherit' }
);

const source = join(cargoTarget, target, 'release', 'plato-agentd.exe');
const destination = join(
	desktop,
	'src-tauri',
	'binaries',
	`plato-agentd-${target}.exe`
);
mkdirSync(dirname(destination), { recursive: true });
copyFileSync(source, destination);
