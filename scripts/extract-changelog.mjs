#!/usr/bin/env node
// Print the CHANGELOG section for one version, for use as a GitHub Release body.
// `node scripts/extract-changelog.mjs 0.1.0` prints everything under
// `## [0.1.0]` up to the next `## ` heading. Falls back to `## [Unreleased]`
// so a tag cut before the changelog was rolled still produces notes.

import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const version = process.argv[2];

if (!version) {
  console.error("usage: extract-changelog.mjs <version>");
  process.exit(1);
}

const lines = readFileSync(join(ROOT, "CHANGELOG.md"), "utf8").split("\n");

function section(heading) {
  const start = lines.findIndex((line) => line.startsWith(`## [${heading}]`));
  if (start === -1) return null;
  const rest = lines.slice(start + 1);
  const end = rest.findIndex((line) => line.startsWith("## "));
  return (end === -1 ? rest : rest.slice(0, end)).join("\n").trim();
}

const body = section(version) ?? section("Unreleased");
if (!body) {
  console.error(`no changelog section for ${version} or Unreleased`);
  process.exit(1);
}
process.stdout.write(`${body}\n`);
