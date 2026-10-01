# AGENTS.md

Guidance for agents and developers working in this repository.

## What this is

`rustrouter` is a local AI routing gateway plus dashboard. One OpenAI-compatible endpoint (`/v1/*`) routes across the upstream providers it ships with format translation, model-combo fallback, multi-account fallback, OAuth/API-key credential management, token refresh, usage tracking and an optional auto quota tracker.

Two hard constraints shape everything:

1. **Port 20129.**
2. **The SQLite file is shared with 9router, schema 100% identical.** Tables, column order, types, defaults, indexes and the PRAGMA set must match byte for byte. SQLite only. This is what lets a database move between the two installs, so treat it as an external contract: a row rustrouter does not recognise is left untouched, never deleted.

## Layout

```
crates/
  router-db/       SQLite: schema, migrations, repos. The shared-schema boundary.
  router-sse/      Registry data, translators, executors, chat pipeline, modality cores. No HTTP.
  router-server/   axum: /v1*, dashboard /api/*, auth, OAuth flows, CLI-tool writers, static assets.
  rustrouter/      bin: clap (serve | start | stop | update-check; -p/--port, -H/--host)
web/               Vue 3 + Vite + Tailwind + Biome
npm/               the published npm package tree (bin shim + platform packages)
scripts/           set-version.mjs, extract-changelog.mjs (release helpers)
docs/              architecture and design docs. Read docs/PLAN.md before changing anything structural.
```

`PROJECT_VERSION` at the root is the single version source; `node
scripts/set-version.mjs` propagates it into every manifest. Cutting a release is
`docs/RELEASING.md`.

## Build and verify

```bash
cargo build --release
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo audit

cd web && npm run build              # vue-tsc -b && biome check && vite build
cd web && ./node_modules/.bin/biome ci
cd web && npm audit
```

The release build embeds `web/dist` into the binary. There is no Docker and no Node at runtime.

For a manual production build outside CI, use `./build.bat` (Windows) or
`./build.sh` (Linux/macOS); both build the frontend first, then the backend, and
write `rustrouter-binary/`. `./build-android.sh` cross-compiles for Android
aarch64 (native Termux or NDK) and sizes the cargo job count to the machine's
free memory so a small device is not OOM-killed; `--help` lists its flags.

Toolchain floor is Rust 1.88 (`edition = "2024"`); that is the highest MSRV among the current
dependency tree, so do not lower it. `npx biome ci` is unreliable here because npm resolves `ci`
as a package name; call the local binary instead. TypeScript is held at 6.x: `vue-tsc` 3.3 needs
TypeScript's `./lib/tsc` export, which TypeScript 7 removes.

## Rules that are easy to get wrong

- **JSON payloads are byte-compared.** `serde_json` needs the `preserve_order` feature and structs must not use `skip_serializing_if = "Option::is_none"`. Every value is written as `JSON.stringify(v ?? null)`: insertion order, and `undefined` becomes `null` with the key kept.
- **Timestamps are UTC RFC 3339 with milliseconds and a trailing `Z`.** Use `to_rfc3339_opts(SecondsFormat::Millis, true)`. Date comparisons are lexicographic string compares.
- **`usageDaily.dateKey` is local time; `usageHistory.timestamp` is UTC.** The two columns use different clocks on purpose.
- **Writes use `BEGIN IMMEDIATE`**, never a deferred transaction that later upgrades. `rusqlite::Transaction` is deferred by default.
- **The provider registry is committed JSON, and it is the source of truth.** `crates/router-sse/src/providers/registry.json` and `crates/router-sse/src/catalog/catalog.json` are edited directly; `registry.rs`, `catalog.rs` and the frontend constants read from them. Do not duplicate the data in Rust — a second copy drifts.
- **Streaming watches raw upstream bytes**, not transform output. The distinction is load-bearing.
- **The frontend has no compile-time API contract.** Freeze JSON response shapes before writing Vue pages.
- **`web/biome.json` disables `noExplicitAny` and `noUnusedVariables`, on purpose.** The first is off
  because `Record<string, any>` is the app's "untyped JSON" type and there is no API contract to type
  against (see above) — a `Record<string, unknown>` swap breaks 555 type checks. The second is off
  because Biome does not scan bare `{{ }}` mustache interpolation, so it reports template-used
  bindings as unused. Turn either back on only alongside the work that makes it pass.
- **Component file names must be multi-word.** `useVueMultiWordComponentNames` reads the *filename*,
  not `defineOptions({ name })`, so a new `Button.vue` fires the rule while `UiButton.vue` does not.

## Where to read before editing

| Touching | Read first |
|---|---|
| anything structural | `docs/PLAN.md` |
| `router-db`, the shared schema | `docs/DB-PARITY.md` |
| providers, models, pricing | `crates/router-sse/src/providers/registry.json` + `docs/PLAN.md` |
| translation, executors, streaming | `docs/CHAT-PIPELINE.md` |
| OAuth, refresh, cooldowns, combos | `docs/OAUTH-AND-CREDENTIALS.md` |
| embeddings / search / web-fetch | `docs/MODALITIES.md` |
| `web/` | `docs/FRONTEND.md` |
| CLI-tool writers | `docs/CLI-TOOLS.md` |
| startup, schedulers, proxy, update check | `docs/RUNTIME.md` |
| tests | `docs/TESTING.md` |
| cutting a release, npm publishing | `docs/RELEASING.md` |

## Conventions

- Conventional Commits (`feat(translator): …`, `fix(db): …`). Subject under 72 characters, imperative mood.
- Every code change updates `CHANGELOG.md` in Keep a Changelog format with semantic versioning.
- Rust: `rustfmt` + `clippy -D warnings`. Frontend: Biome, tab indent, double quotes, organised imports.
- Comments explain why, never what.
