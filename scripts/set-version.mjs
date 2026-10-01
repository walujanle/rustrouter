#!/usr/bin/env node
// Propagate PROJECT_VERSION into every manifest that carries a version, so a
// release has one place to edit. `--check` verifies the manifests agree without
// writing, which is what CI runs before a build.

import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const CHECK = process.argv.includes("--check");
const VERSION = readFileSync(join(ROOT, "PROJECT_VERSION"), "utf8").trim();

if (!/^\d+\.\d+\.\d+$/.test(VERSION)) {
  console.error(`PROJECT_VERSION must be x.y.z, got "${VERSION}"`);
  process.exit(1);
}

// JSON files are rewritten in place; `json` reads and preserves nothing, so the
// original indent is detected and reused to keep the diff to the version line.
function jsonFile(relPath) {
  const path = join(ROOT, relPath);
  const text = readFileSync(path, "utf8");
  const current = JSON.parse(text).version;
  return {
    path,
    relPath,
    current,
    write() {
      const indent = text.match(/^(\s+)"/m)?.[1] ?? "\t";
      const data = JSON.parse(text);
      data.version = VERSION;
      writeFileSync(path, `${JSON.stringify(data, null, indent)}\n`);
    },
  };
}

const CARGO = join(ROOT, "Cargo.toml");
const cargoText = readFileSync(CARGO, "utf8");
const cargoMatch = cargoText.match(/^version = "(\d+\.\d+\.\d+)"/m);
const cargo = {
  path: CARGO,
  relPath: "Cargo.toml",
  current: cargoMatch?.[1],
  write() {
    writeFileSync(
      CARGO,
      cargoText.replace(/^version = "\d+\.\d+\.\d+"/m, `version = "${VERSION}"`),
    );
  },
};

// The main npm package pins each platform package to an exact version in
// `optionalDependencies`. Left unsynced, `npm i -g rustrouter` at the new
// version resolves the previous platform package (or nothing).
function npmMainFile(relPath) {
  const path = join(ROOT, relPath);
  const text = readFileSync(path, "utf8");
  const data = JSON.parse(text);
  const deps = data.optionalDependencies ?? {};
  // `current` must be the value the check compares, so it is only VERSION when
  // both the package version and every pinned dependency already agree.
  const current =
    data.version === VERSION && Object.values(deps).every((v) => v === VERSION)
      ? VERSION
      : `${data.version} deps ${Object.values(deps).join("/")}`;
  return {
    path,
    relPath,
    current,
    write() {
      const indent = text.match(/^(\s+)"/m)?.[1] ?? "\t";
      const next = JSON.parse(text);
      next.version = VERSION;
      for (const name of Object.keys(next.optionalDependencies ?? {})) {
        next.optionalDependencies[name] = VERSION;
      }
      writeFileSync(path, `${JSON.stringify(next, null, indent)}\n`);
    },
  };
}

// `npm install` keeps the lock's root version in step with package.json, so a
// release that only bumps package.json leaves `npm ci` refusing the tree.
function npmLockFile(relPath) {
  const path = join(ROOT, relPath);
  const text = readFileSync(path, "utf8");
  const data = JSON.parse(text);
  return {
    path,
    relPath,
    current: data.version,
    write() {
      const indent = text.match(/^(\s+)"/m)?.[1] ?? "\t";
      const next = JSON.parse(text);
      next.version = VERSION;
      if (next.packages?.[""]) next.packages[""].version = VERSION;
      writeFileSync(path, `${JSON.stringify(next, null, indent)}\n`);
    },
  };
}

// `Cargo.lock` records the version of every workspace member. CI builds with
// `--locked`, so a stale entry fails the release the same way a drifted
// manifest does; rewrite the four member blocks in place rather than shelling
// out to `cargo update`, which would also move dependency pins.
function cargoLockFile() {
  const path = join(ROOT, "Cargo.lock");
  const text = readFileSync(path, "utf8");
  const members = ["router-db", "router-sse", "router-server", "rustrouter"];
  const versions = members
    .map((name) => text.match(new RegExp(`name = "${name}"\\nversion = "(\\d+\\.\\d+\\.\\d+)"`))?.[1])
    .filter(Boolean);
  return {
    path,
    relPath: "Cargo.lock",
    current: versions.length === members.length && new Set(versions).size === 1 ? versions[0] : null,
    write() {
      let next = text;
      for (const name of members) {
        next = next.replace(
          new RegExp(`(name = "${name}"\\nversion = ")\\d+\\.\\d+\\.\\d+(")`),
          `$1${VERSION}$2`,
        );
      }
      writeFileSync(path, next);
    },
  };
}

const files = [
  cargo,
  jsonFile("web/package.json"),
  npmLockFile("web/package-lock.json"),
  cargoLockFile(),
  npmMainFile("npm/package.json"),
  ...["linux-x64", "linux-arm64", "darwin-arm64", "windows-x64", "windows-arm64"].map(
    (p) => jsonFile(`npm/platforms/${p}/package.json`),
  ),
];

let drift = 0;
for (const file of files) {
  if (file.current === VERSION) continue;
  drift += 1;
  if (CHECK) {
    console.error(`${file.relPath}: ${file.current ?? "<missing>"} != ${VERSION}`);
  } else {
    file.write();
    console.log(`${file.relPath}: ${file.current ?? "<missing>"} -> ${VERSION}`);
  }
}

if (CHECK && drift > 0) process.exit(1);
if (!CHECK && drift === 0) console.log(`all manifests already at ${VERSION}`);
