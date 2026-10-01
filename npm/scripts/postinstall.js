#!/usr/bin/env node
// Termux reports itself to npm as `linux`/`arm64`, so npm installs the glibc
// linux-arm64 package, whose binary cannot load under Android's Bionic loader.
// Detect Termux and swap in the android Release asset. Every other platform
// already has a working binary from its optional dependency, so this is a no-op
// there. A download failure warns rather than failing the install.

const { createHash } = require("node:crypto");
const { chmodSync, existsSync, writeFileSync } = require("node:fs");
const path = require("node:path");

const REPO = "walujanle/rustrouter";
const BINARY = "rustrouter";

function isTermux() {
	if (process.env.PREFIX?.includes("com.termux")) return true;
	return (
		process.platform === "linux" &&
		process.arch === "arm64" &&
		existsSync("/system/bin/linker64")
	);
}

async function main() {
	if (!isTermux()) return;

	const { version } = require("../package.json");
	const asset = `rustrouter-linux-android-aarch64-${version}`;
	const release = await fetch(
		`https://api.github.com/repos/${REPO}/releases/tags/v${version}`,
		{ headers: { "User-Agent": "rustrouter-postinstall" } },
	);
	if (!release.ok) throw new Error(`release lookup failed (${release.status})`);
	const { assets } = await release.json();
	const target = assets?.find((a) => a.name === asset);
	if (!target) throw new Error(`asset ${asset} not found in the release`);

	const download = await fetch(target.browser_download_url, {
		headers: { "User-Agent": "rustrouter-postinstall" },
	});
	if (!download.ok) throw new Error(`download failed (${download.status})`);
	const bytes = Buffer.from(await download.arrayBuffer());

	// The Release API exposes each asset's digest as `sha256:<hex>`. Verify it
	// before the binary is made executable.
	const expected = target.digest?.replace(/^sha256:/, "");
	const actual = createHash("sha256").update(bytes).digest("hex");
	if (expected && expected !== actual) {
		throw new Error(`checksum mismatch for ${asset}`);
	}

	const dest = path.join(__dirname, "..", "bin", BINARY);
	writeFileSync(dest, bytes);
	chmodSync(dest, 0o755);
	console.log(`rustrouter: installed Termux binary from ${asset}`);
}

main().catch((error) => {
	console.warn(`rustrouter: could not install the Termux binary (${error.message}).`);
	console.warn(
		`rustrouter: install it manually from https://github.com/${REPO}/releases`,
	);
});
