import { fileURLToPath, URL } from "node:url";

import tailwindcss from "@tailwindcss/vite";
import vue from "@vitejs/plugin-vue";
import { minifySync } from "rolldown/experimental";
import { defineConfig, type Plugin } from "vite";

// Vite minifies the JS (oxc) and the CSS (lightningcss) but emits the entry
// HTML as written, so `dist/index.html` kept every newline and comment. This
// collapses it to one line. The inline `<script>` blocks are minified with the
// same oxc pass Vite runs on the bundles, which folds their newlines without
// breaking automatic semicolon insertion; every other run of whitespace is
// insignificant between tags, so it collapses to a single space.
const RAW_TEXT = /<(script|style)\b([^>]*)>([\s\S]*?)<\/\1>/gi;
// Private-use sentinel: not a control character, so it trips no lint, and it
// cannot appear in the entry HTML.
const MARK = "\uE000";
const MARK_SLOT = new RegExp(`${MARK}(\\d+)${MARK}`, "g");

function oneLineHtml(html: string): string {
	const raw: string[] = [];
	const masked = html.replace(
		RAW_TEXT,
		(_match, tag: string, attrs: string, code: string) => {
			// An external `<script src>` has no inline body to fold.
			const body =
				tag === "script" && !/\bsrc\s*=/i.test(attrs)
					? minifySync("inline.js", code).code
					: code;
			const block = `<${tag}${attrs}>${body}</${tag}>`;
			return `${MARK}${raw.push(block) - 1}${MARK}`;
		},
	);
	return masked
		.replace(/<!--[\s\S]*?-->/g, "")
		.replace(/\s+/g, " ")
		.replace(MARK_SLOT, (_, i: string) => raw[Number(i)])
		.trim();
}

function htmlOneLine(): Plugin {
	return {
		name: "html-one-line",
		apply: "build",
		transformIndexHtml: {
			order: "post",
			handler: oneLineHtml,
		},
	};
}

// Imports resolve through `@/`, so the alias is load-bearing. `/api` and `/v1`
// proxy to the Rust server on 20129 during `vite dev`; the release build is
// embedded into that same server, so production needs no proxy.
export default defineConfig({
	plugins: [tailwindcss(), vue(), htmlOneLine()],
	resolve: {
		alias: {
			"@": fileURLToPath(new URL("./src", import.meta.url)),
		},
	},
	server: {
		proxy: {
			"/api": "http://localhost:20129",
			"/v1": "http://localhost:20129",
		},
	},
});
