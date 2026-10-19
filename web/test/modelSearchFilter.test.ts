import assert from "node:assert/strict";
import { test } from "node:test";

import {
	filterModelsByQuery,
	matchesSearch,
	parseSearchQuery,
} from "../src/utils/modelSearchFilter.ts";

const model = (id: string, name = id, provider = "anthropic") => ({
	id,
	name,
	provider,
});

test("an empty query matches everything and returns the input array", () => {
	for (const raw of ["", "   ", null, undefined]) {
		const parsed = parseSearchQuery(raw as string | null | undefined);
		assert.equal(parsed.isEmpty, true);
		assert.equal(matchesSearch(model("claude-haiku"), parsed), true);
	}
	const items = [model("a"), model("b")];
	assert.equal(filterModelsByQuery(items, ""), items);
});

test("plain terms are case-insensitive substrings", () => {
	assert.equal(matchesSearch(model("claude-haiku-4.5"), "HAIKU"), true);
	assert.equal(matchesSearch(model("claude-haiku-4.5"), "gpt"), false);
});

test("all positive terms must match", () => {
	assert.equal(matchesSearch(model("claude-haiku-4.5"), "claude haiku"), true);
	assert.equal(matchesSearch(model("claude-haiku-4.5"), "claude gpt"), false);
});

test("! and - negate, and a negative-only query is valid", () => {
	assert.equal(matchesSearch(model("claude-haiku"), "claude !haiku"), false);
	assert.equal(matchesSearch(model("claude-sonnet"), "claude -haiku"), true);
	assert.equal(matchesSearch(model("claude-sonnet"), "!haiku"), true);
	assert.equal(matchesSearch(model("claude-haiku"), "!haiku"), false);
});

test("a full regex is parsed with its flags", () => {
	const parsed = parseSearchQuery("/sonnet.*5/i");
	assert.equal(parsed.isRegex, true);
	assert.equal(matchesSearch(model("claude-sonnet-5"), parsed), true);
	assert.equal(matchesSearch(model("claude-haiku-5"), parsed), false);
});

test("live-typed regex works and an invalid one falls back to literal terms", () => {
	assert.equal(matchesSearch(model("claude-sonnet-5"), "/sonnet.*"), true);
	// An unclosed group is not yet a regex; it must not throw.
	assert.doesNotThrow(() => parseSearchQuery("/sonnet("));
	assert.equal(parseSearchQuery("/sonnet(").isRegex, false);
});

test("a negative lookahead is tested against the joined candidate string", () => {
	assert.equal(
		matchesSearch(model("claude-sonnet-5"), "/(?!haiku)sonnet/"),
		true,
	);
	assert.equal(
		matchesSearch(model("claude-haiku-5"), "/(?!haiku)haiku/"),
		false,
	);
});

test("object candidates include provider, alias and value", () => {
	assert.equal(
		matchesSearch(model("x", "x", "openrouter"), "openrouter"),
		true,
	);
	assert.equal(matchesSearch({ value: "gpt-5", name: "GPT" }, "gpt-5"), true);
	assert.equal(matchesSearch({ id: "a", alias: "al" }, "al"), true);
});

test("a target with no string candidates never matches a non-empty query", () => {
	assert.equal(matchesSearch({}, "x"), false);
	assert.equal(matchesSearch(null, "x"), false);
});

test("filterModelsByQuery keeps the order and applies negation", () => {
	const items = [model("claude-haiku"), model("claude-sonnet"), model("gpt-5")];
	const out = filterModelsByQuery(items, "claude -haiku");
	assert.deepEqual(
		out.map((m) => m.id),
		["claude-sonnet"],
	);
});
