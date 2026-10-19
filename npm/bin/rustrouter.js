#!/usr/bin/env node
// npm needs the `bin` target to be a JS file, so this shim resolves the
// platform package that carries the real binary and execs it with the user's
// argv, forwarding the exit code.

const { spawnSync } = require("node:child_process");
const { existsSync } = require("node:fs");
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

// Termux reports itself to npm as linux/arm64 and gets the glibc package, whose
// dynamic loader does not exist under Bionic. The Android build is normally
// fetched by postinstall; when npm 12 blocks that script the glibc binary is all
// that is left, and exec fails with ENOENT rather than a useful message.
function isTermux() {
	if (process.env.PREFIX?.includes("com.termux")) return true;
	return (
		process.platform === "linux" &&
		process.arch === "arm64" &&
		existsSync("/system/bin/linker64")
	);
}

// A failed exec has three causes worth naming, each with a one-line fix.
// Everything else keeps the plain error message.
function launchFailureMessage(error, attempted) {
	if (error.code !== "ENOENT") {
		return `rustrouter: failed to launch binary: ${error.message}`;
	}
	if (isTermux()) {
		return [
			`rustrouter: could not start ${attempted}.`,
			"rustrouter: on Termux this means the Android build was not installed,",
			"rustrouter: because npm 12, pnpm 10 and bun block install scripts.",
			"rustrouter: reinstall with",
			"rustrouter:   npm install -g --allow-scripts=rustrouter rustrouter",
		].join("\n");
	}
	if (!existsSync(attempted)) {
		return [
			`rustrouter: could not start ${attempted}.`,
			"rustrouter: the platform package is missing from this install.",
			"rustrouter: reinstall without --omit=optional:",
			"rustrouter:   npm install -g rustrouter",
		].join("\n");
	}
	return [
		`rustrouter: could not start ${attempted}.`,
		"rustrouter: the binary exists but its dynamic loader was not found,",
		"rustrouter: so this build does not match your libc (glibc is required).",
		"rustrouter: see https://github.com/walujanle/rustrouter/releases",
	].join("\n");
}

// Tell the binary it was installed through npm so its update prompt offers the
// npm command. Set here because this shim is the only thing that knows the
// install channel; the server cannot tell npm from a direct binary on its own.
const binaryPath = resolveBinary();
const result = spawnSync(binaryPath, process.argv.slice(2), {
	stdio: "inherit",
	env: { ...process.env, RUSTROUTER_INSTALL_METHOD: "npm" },
});
if (result.error) {
	console.error(launchFailureMessage(result.error, binaryPath));
	process.exit(1);
}
process.exit(result.status ?? 1);
