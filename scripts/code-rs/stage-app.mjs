#!/usr/bin/env node
/*---------------------------------------------------------------------------------------------
 *  code-rs: assemble the runtime files bundled into the Tauri installers.
 *
 *  dist-app/
 *    app/   out/ (bundled workbench), extensions/ (built-in, no sources), production
 *           node_modules/, product.json, package.json   -> <resources>/app
 *    node/  node[.exe] (Node runtime for the extension host)  -> <resources>/node
 *
 *  Usage: node scripts/code-rs/stage-app.mjs [--platform win32|linux|darwin] [--arch x64|arm64]
 *         [--node-version 24.22.1] [--out dist-app]
 *  Requires the bundled workbench (out-bundle/) and the built-in extensions to be compiled first.
 *--------------------------------------------------------------------------------------------*/

import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';

const root = path.resolve(import.meta.dirname, '..', '..');
const args = Object.fromEntries(process.argv.slice(2).reduce((acc, a, i, all) => (a.startsWith('--') ? [...acc, [a.slice(2), all[i + 1]]] : acc), []));
const platform = args.platform ?? process.platform;
const arch = args.arch ?? process.arch;
const nodeVersion = args['node-version'] ?? fs.readFileSync(path.join(root, '.nvmrc'), 'utf8').trim();
const outDir = path.resolve(root, args.out ?? 'dist-app');
const appDir = path.join(outDir, 'app');
const nodeDir = path.join(outDir, 'node');

const log = (...m) => console.log('[stage-app]', ...m);

fs.rmSync(outDir, { recursive: true, force: true });
fs.mkdirSync(appDir, { recursive: true });
fs.mkdirSync(nodeDir, { recursive: true });

// --- out/ (bundled + minified workbench) -------------------------------------
// Releases ship the bundled build (build/next bundle --minify --nls --target
// server-web): a handful of files instead of thousands of modules, which makes
// startup several times faster. Source maps are left out.
const outSrc = path.join(root, 'out-bundle');
if (!fs.existsSync(path.join(outSrc, 'vs', 'code', 'browser', 'workbench', 'workbench.html'))) {
	throw new Error('out-bundle/ is missing; run `node build/next/index.ts bundle --minify --nls --target server-web --out out-bundle` first');
}
fs.cpSync(outSrc, path.join(appDir, 'out'), {
	recursive: true,
	filter: src => !src.endsWith('.map'),
});
log('copied out/');

// --- built-in extensions (compiled output + runtime deps, no sources) --------
// Not shipped: the proprietary Copilot extension (and its ~1 GB of deps), and the
// shared extensions/node_modules build tooling except `typescript`, which the
// TypeScript language features extension loads at runtime (tsserver).
const extensionsRoot = path.join(root, 'extensions');
// Copilot is proprietary; the others are VS Code's own test extensions.
const excludedExtensions = new Set(['copilot', 'vscode-api-tests', 'vscode-colorize-tests', 'vscode-colorize-perf-tests', 'vscode-test-resolver']);
const keptSharedExtensionModules = new Set(['typescript']);
const skipInExtensions = new Set(['src', 'test', 'test-workspace', '.vscode-test', '.vscode', 'build', 'cgmanifest.json', 'tsconfig.json', 'tsconfig.tsbuildinfo']);
fs.cpSync(extensionsRoot, path.join(appDir, 'extensions'), {
	recursive: true,
	filter: src => {
		const rel = path.relative(extensionsRoot, src).split(path.sep);
		if (excludedExtensions.has(rel[0])) {
			return false;
		}
		if (rel[0] === 'node_modules' && rel.length > 1 && !keptSharedExtensionModules.has(rel[1])) {
			return false;
		}
		const base = path.basename(src);
		if (skipInExtensions.has(base) && !src.includes(`${path.sep}node_modules${path.sep}`)) {
			return false;
		}
		return !base.endsWith('.map') && !/^tsconfig.*\.json$/.test(base) && !/^esbuild.*\.m?[jt]s$/.test(base);
	},
});
log('copied extensions/');

// --- metadata ----------------------------------------------------------------
for (const file of ['product.json', 'package.json']) {
	fs.copyFileSync(path.join(root, file), path.join(appDir, file));
}

// --- production node_modules (walk `dependencies`, hoisted layout) -----------
const copied = new Set();
function resolvePackageDir(name, fromDir) {
	// Node resolution: nearest node_modules walking up from the requiring package.
	let dir = fromDir;
	for (; ;) {
		const candidate = path.join(dir, 'node_modules', name);
		if (fs.existsSync(path.join(candidate, 'package.json'))) {
			return candidate;
		}
		const parent = path.dirname(dir);
		if (parent === dir || !parent.startsWith(root)) {
			return undefined;
		}
		dir = parent;
	}
}
// Runtime dependencies of features code-rs does not ship (Copilot / local AI
// agents, browser automation). They are loaded lazily, so leaving them out only
// disables those features.
const excludedPackages = [/^@github\/copilot-sdk/, /^foundry-local-sdk$/, /^playwright-core$/];
function copyPackage(name, fromDir, optional) {
	if (excludedPackages.some(re => re.test(name))) {
		return;
	}
	const dir = resolvePackageDir(name, fromDir);
	if (!dir) {
		if (!optional) {
			log(`warning: dependency not found: ${name} (from ${path.relative(root, fromDir) || '.'})`);
		}
		return;
	}
	if (copied.has(dir)) {
		return;
	}
	copied.add(dir);
	const rel = path.relative(root, dir);
	fs.cpSync(dir, path.join(appDir, rel), {
		recursive: true,
		dereference: true,
		// nested node_modules are handled by the dependency walk itself
		filter: src => src === dir || !path.relative(dir, src).split(path.sep).includes('node_modules'),
	});
	const pkg = JSON.parse(fs.readFileSync(path.join(dir, 'package.json'), 'utf8'));
	for (const dep of Object.keys(pkg.dependencies ?? {})) {
		copyPackage(dep, dir, false);
	}
	for (const dep of Object.keys(pkg.optionalDependencies ?? {})) {
		copyPackage(dep, dir, true);
	}
}
const rootPkg = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'));
for (const dep of Object.keys(rootPkg.dependencies ?? {})) {
	copyPackage(dep, root, false);
}
for (const dep of Object.keys(rootPkg.optionalDependencies ?? {})) {
	copyPackage(dep, root, true);
}
log(`copied ${copied.size} production packages`);

// --- Node runtime for the extension host --------------------------------------
const nodePlatform = { win32: 'win', linux: 'linux', darwin: 'darwin' }[platform];
if (!nodePlatform) {
	throw new Error(`unsupported platform: ${platform}`);
}
const archiveExt = platform === 'win32' ? 'zip' : platform === 'linux' ? 'tar.xz' : 'tar.gz';
const archiveName = `node-v${nodeVersion}-${nodePlatform}-${arch}.${archiveExt}`;
const base = `https://nodejs.org/dist/v${nodeVersion}`;

// Retries: a network hiccup reaching nodejs.org must not fail the release.
async function download(url) {
	for (let attempt = 1; ; attempt++) {
		try {
			const res = await fetch(url);
			if (!res.ok) {
				throw new Error(`HTTP ${res.status}`);
			}
			return Buffer.from(await res.arrayBuffer());
		} catch (err) {
			if (attempt >= 5) {
				throw err;
			}
			log(`download failed (${err.message}), retrying in ${attempt * 10} s: ${url}`);
			await new Promise(r => setTimeout(r, attempt * 10_000));
		}
	}
}
const sums = (await download(`${base}/SHASUMS256.txt`)).toString('utf8');
const expected = sums.split('\n').find(l => l.endsWith(`  ${archiveName}`))?.split(/\s+/)[0];
if (!expected) {
	throw new Error(`no checksum for ${archiveName}`);
}
const archive = await download(`${base}/${archiveName}`);
const actual = createHash('sha256').update(archive).digest('hex');
if (actual !== expected) {
	throw new Error(`checksum mismatch for ${archiveName}`);
}

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'code-rs-node-'));
const archivePath = path.join(tmp, archiveName);
fs.writeFileSync(archivePath, archive);
// bsdtar (Windows 10+ System32) and GNU/BSD tar handle zip, xz and gz. On Windows
// avoid the MSYS/Git-Bash GNU tar that may be first on PATH (no zip support).
const tarBinary = process.platform === 'win32' ? path.join(process.env.SystemRoot ?? 'C:\\Windows', 'System32', 'tar.exe') : 'tar';
execFileSync(tarBinary, ['-xf', archivePath, '-C', tmp], { stdio: 'inherit' });
const extracted = path.join(tmp, archiveName.replace(/\.(zip|tar\.xz|tar\.gz)$/, ''));
const nodeBinary = platform === 'win32' ? path.join(extracted, 'node.exe') : path.join(extracted, 'bin', 'node');
const target = path.join(nodeDir, platform === 'win32' ? 'node.exe' : 'node');
fs.copyFileSync(nodeBinary, target);
fs.chmodSync(target, 0o755);
fs.copyFileSync(path.join(extracted, 'LICENSE'), path.join(nodeDir, 'LICENSE'));
fs.rmSync(tmp, { recursive: true, force: true });
log(`bundled Node ${nodeVersion} (${nodePlatform}-${arch}, sha256 verified)`);

// --- drop native binaries built for other platforms ---------------------------
// npm packages and extensions ship prebuilt binaries for every OS/arch (and musl).
// They are dead weight in an installer for one target, and on Linux they break the
// AppImage: linuxdeploy tries to resolve the libraries of every ELF file it finds.
// Detected by file header, not by name: ELF (machine + musl), Mach-O, PE.
const elfMachine = { x64: 0x3e, arm64: 0xb7 }[arch];
const machoCpu = { x64: 0x01000007, arm64: 0x0100000c }[arch];
const peMachine = { x64: 0x8664, arm64: 0xaa64 }[arch];
function keepBinary(file, head) {
	if (head.length >= 20 && head.readUInt32BE(0) === 0x7f454c46) { // ELF
		if (platform !== 'linux') {
			return false;
		}
		const machine = head[5] === 1 ? head.readUInt16LE(18) : head.readUInt16BE(18);
		if (machine !== elfMachine) {
			return false;
		}
		// musl builds need ld-musl/libc.musl, absent on glibc systems.
		const data = fs.readFileSync(file);
		return !data.includes('ld-musl-') && !data.includes('libc.musl-');
	}
	if (head.length >= 8 && [0xfeedfacf, 0xcffaedfe].includes(head.readUInt32BE(0))) { // Mach-O 64
		const cpu = head.readUInt32BE(0) === 0xcffaedfe ? head.readUInt32LE(4) : head.readUInt32BE(4);
		return platform === 'darwin' && cpu === machoCpu;
	}
	if (head.length >= 4 && head.readUInt32BE(0) === 0xcafebabe && /\.(node|dylib)$/.test(file)) { // universal Mach-O
		return platform === 'darwin';
	}
	if (head.length >= 64 && head.readUInt16LE(0) === 0x5a4d && /\.(node|dll|exe)$/i.test(file)) { // PE
		if (platform !== 'win32') {
			return false;
		}
		const peOffset = head.readUInt32LE(60);
		try {
			const fd = fs.openSync(file, 'r');
			const pe = Buffer.alloc(6);
			fs.readSync(fd, pe, 0, 6, peOffset);
			fs.closeSync(fd);
			return pe.readUInt32BE(0) !== 0x50450000 || pe.readUInt16LE(4) === peMachine;
		} catch {
			return true;
		}
	}
	return true; // not a native binary
}
let pruned = 0;
let prunedBytes = 0;
function pruneForeignBinaries(dir) {
	for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
		const p = path.join(dir, entry.name);
		if (entry.isDirectory()) {
			pruneForeignBinaries(p);
		} else if (entry.isFile()) {
			const fd = fs.openSync(p, 'r');
			const head = Buffer.alloc(64);
			const n = fs.readSync(fd, head, 0, 64, 0);
			fs.closeSync(fd);
			if (!keepBinary(p, head.subarray(0, n))) {
				prunedBytes += fs.statSync(p).size;
				fs.rmSync(p);
				pruned++;
			}
		}
	}
}
pruneForeignBinaries(appDir);
log(`removed ${pruned} native binaries for other platforms (${(prunedBytes / 1024 / 1024).toFixed(0)} MB)`);

const size = dir => fs.readdirSync(dir, { withFileTypes: true }).reduce((n, e) => n + (e.isDirectory() ? size(path.join(dir, e.name)) : fs.statSync(path.join(dir, e.name)).size), 0);
log(`done: ${outDir} (${(size(outDir) / 1024 / 1024).toFixed(0)} MB)`);
