#!/usr/bin/env node
// npm needs the `bin` target to be a JS file, so this shim resolves the
// platform package that carries the real binary and execs it with the user's
// argv, forwarding the exit code.

const { spawnSync } = require("node:child_process");
const path = require("node:path");

const PLATFORM_PACKAGES = {
	linux: { x64: "rustrouter-linux-x64", arm64: "rustrouter-linux-arm64" },
	darwin: { arm64: "rustrouter-darwin-arm64" },
	// Package names say `windows`, not `win32`: npm's name filter rejects any
	// name containing `win32`. The key stays `win32` because that is what
	// process.platform reports.
	win32: { x64: "rustrouter-windows-x64", arm64: "rustrouter-windows-arm64" },
};

const pkg = PLATFORM_PACKAGES[process.platform]?.[process.arch];
const binary = process.platform === "win32" ? "rustrouter.exe" : "rustrouter";

function resolveBinary() {
	if (pkg) {
		try {
			// Resolve the package rather than guess a node_modules path, so
			// hoisting and pnpm layouts both work.
			const pkgDir = path.dirname(require.resolve(`${pkg}/package.json`));
			return path.join(pkgDir, "bin", binary);
		} catch {
			/* platform package missing; fall through to the Termux download */
		}
	}
	// Termux: npm installs rustrouter-linux-arm64 (glibc), which cannot load
	// under Bionic. postinstall replaces it with the android build.
	return path.join(__dirname, binary);
}

const result = spawnSync(resolveBinary(), process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
	console.error(`rustrouter: failed to launch binary: ${result.error.message}`);
	process.exit(1);
}
process.exit(result.status ?? 1);
