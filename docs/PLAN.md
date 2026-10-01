# rustrouter Architecture and Roadmap

**What this is:** rustrouter is a local AI routing gateway plus dashboard. One OpenAI-compatible endpoint (`/v1/*`) routes across the upstream providers it ships with, with format translation, model-combo fallback, multi-account fallback, OAuth/API-key credential management, token refresh, usage tracking and an optional auto quota tracker. A Rust backend serves a Vue 3 dashboard.

**Status:** implemented and shipping. The four Rust crates and the dashboard are built and pass their gates. This document is the architecture record and the forward plan.

## Documents

| File | Covers |
|---|---|
| `PLAN.md` | this file: constraints, architecture, risks, decisions, direction |
| `DB-PARITY.md` | schema, JSON/date byte handling, repo semantics, driver chain, shared-file concurrency |
| `CHAT-PIPELINE.md` | chat stage order, translator registry, executors, streaming, account fallback |
| `OAUTH-AND-CREDENTIALS.md` | OAuth flows, credential storage, refresh, cooldowns, combos |
| `MODALITIES.md` | embeddings, search, web-fetch; the SSRF guard |
| `FRONTEND.md` | the Vue 3 dashboard: routing, stores, library choices |
| `CLI-TOOLS.md` | the Codex, Claude Code and Hermes config writers |
| `RUNTIME.md` | schedulers, ephemeral state, startup order, outbound proxy, updater, launcher |
| `TESTING.md` | fixtures, the database round trip, the test harness |
| `RELEASING.md` | versioning and npm publishing |

---

## 0. Constraints (do not violate)

| # | Constraint |
|---|---|
| C1 | Port **20129**. |
| C2 | The database is **shared with 9router** and the schema is **byte-identical**: tables, column order, types, defaults, indexes and the PRAGMA set. SQLite only. |
| C3 | Rust backend; Vue 3 + Tailwind frontend; Biome for the frontend, rustfmt + clippy for Rust. |
| C4 | Local password login stays. |
| C5 | The settings blob is written whole, so every key the schema declares is preserved — including keys nothing reads. Dropping one changes what an existing row reads. |
| C6 | Writes use `BEGIN IMMEDIATE`, never a deferred transaction that later upgrades. |
| C7 | The auto quota tracker is off by default. |

rustrouter listens on **20129**; `DEFAULT_MITM_ROUTER_BASE` in `router-db` stays `http://localhost:20128`. That value is a key inside the shared settings blob, so it is part of the parity contract rather than a live bind address: rewriting it to 20129 changes the bytes an existing 9router row reads.

The size of the job, in this repo's own terms: four crates (~85,600 lines of Rust), a Vue 3 dashboard of 108 `.vue` files, a generated provider registry of 24 entries (18 with a transport) and 202 model rows, 11 database tables, and 1,158 unit tests.

---

## 1. What rustrouter is

One binary that:

1. Serves an OpenAI-compatible endpoint on `/v1/*` (plus `/v1beta/*`, `/codex/*`, `/responses`, `/systemone`) and routes to the upstream providers it ships with, with format translation.
2. Serves a dashboard SPA (Vue, built to `web/dist`, embedded in the binary) with its own `/api/*` surface.
3. Persists everything to the shared SQLite file, so a database can move between rustrouter and 9router without a migration.

The code splits into two layers, and the split is what keeps it legible:

- `router-sse` is the provider-agnostic engine: provider registry, format translators, upstream executors, chat pipeline, non-chat modality handlers. It carries no HTTP.
- `router-server` is the app glue: HTTP routes, auth, dashboard API, OAuth flows, CLI-tool writers. Persistence lives beside them in `router-db`.

---

## 2. Architecture

### 2.1 Crate layout

The workspace root holds the Rust crates and the Vue frontend side by side.

```
rustrouter/
├── Cargo.toml              # workspace
├── crates/
│   ├── router-db/          # SQLite: schema, migrations, repos. The byte-parity boundary.
│   ├── router-sse/         # registry data, translators, executors, chat pipeline,
│   │                       # modality cores. No HTTP.
│   ├── router-server/      # axum: /v1*, dashboard /api/*, auth, OAuth flows,
│   │                       # CLI-tool writers, static assets, ConnectInfo IP.
│   └── rustrouter/         # bin: clap (serve | start | stop | update-check)
├── web/                    # Vue 3 + Vite + Tailwind + Biome
├── npm/                    # the published npm package (bin shim + platform packages)
├── scripts/                # set-version.mjs, extract-changelog.mjs
└── docs/
```

Four crates. `router-sse` stays HTTP-free so it can be unit-tested on its own, and so a future embedder can use it without axum.

### 2.2 Dependency set

`axum 0.8`, `tokio`, `tower-http`, `reqwest` (rustls, `stream`, `socks`, `multipart`, `json`, `cookies`), `rusqlite` (bundled), `serde` + `serde_json` **with `preserve_order`**, `bcrypt`, `dashmap`, `futures`, `tokio-util`, `async-stream`, `crc32fast`, `flate2`, `base64`, `sha2`, `uuid`, `regex`, `thiserror`, `tracing`, `rust-embed`, `toml_edit`, `dirs`, `which`, `chrono`. The dashboard signing is a hand-rolled HS256 (`crates/router-server/src/auth/jwt.rs`), so there is no JWT crate.

Resolved versions live in `Cargo.lock`. The workspace pins `edition = "2024"` and `rust-version = "1.88"`, the highest MSRV in the current tree. Frontend versions are in `web/package.json`; TypeScript is deliberately held at 6.x because `vue-tsc` 3.3 imports TypeScript's `./lib/tsc` export, which TypeScript 7 drops.

Session signing is byte-compatible with the `jose` library (`crates/router-server/src/auth/jwt.rs`). The protected header is exactly `{"alg":"HS256"}` with no `typ`, base64url is unpadded, the payload keeps insertion order, and `exp` is rejected at `exp <= now` with no clock tolerance. The `alg` is pinned before the key is touched, so an `alg: "none"` or `alg: "HS512"` token cannot be accepted. A token minted by one side verifies on the other only while these hold.

### 2.3 Request path

```
axum router
  /v1/*, /v1beta/*, /codex/*, /responses, /systemone   → api_v1 handlers
  /api/*                                               → dashboard handlers (guarded)
  everything else                                      → embedded web/dist, SPA fallback
        ↓
  router-server::routes::v1::chat   (auth, requireApiKey, combo expansion, account loop)
        ↓
  router-sse::handlers::chat_core   (detect format → translate request → executor → stream)
        ↓
  router-sse::translator / executors
        ↓
  SSE back to client (axum Body::from_stream)
```

The LLM surface is mounted at `/v1`, `/v1/v1` and `/api/v1`, so all three spellings reach the same handlers. The doubled prefix is deliberate: the CLI-tool writer stores `ANTHROPIC_BASE_URL` ending in `/v1`, and Claude Code appends `/v1/messages` to whatever it is given.

Auth is one axum middleware (`crates/router-server/src/auth/guard.rs`) over a deny-by-default shape: a new route is protected unless it is added to a public table. It carries the path tables for public LLM paths, protected dashboard paths and local-only paths, plus the CLI-token / API-key / loopback checks. The `ConnectInfo` layer supplies the real client IP; forwarding headers are trusted only from loopback.

Two axum layers sit under the guard (`crates/router-server/src/app.rs`). `RequestDecompressionLayer` accepts gzip, brotli, zstd and deflate request bodies and passes an unrecognised encoding through unchanged instead of answering 415; the 8 MB body limit (`MAX_REQUEST_BODY_BYTES`) is measured after decompression, so a long conversation pasted into the dashboard is accepted without opening a memory-exhaustion path. Response compression never touches `text/event-stream`, so an SSE response is never buffered.

The guard fails closed. `/systemone` is listed among the public LLM prefixes so it carries the same guard-level API-key requirement as `/v1`; without that entry a remote keyless caller reaches the handler whenever `requireApiKey` is off. The shared `api_key_gate` answers `503 Settings unavailable` when the settings read fails, rather than treating a failed read as `requireApiKey: false` and opening the gate exactly when the database is unhealthy.

### 2.4 Key design decisions

**Translators are data, not traits.** The registry is `HashMap<(Format, Format), TranslatorFn>` built once into a `LazyLock` (`crates/router-sse/src/translator/mod.rs`). Registration is one explicit `register_all` called by the registry constructor, so a pair cannot be lost by a missing side-effecting import. Direct routes keep their exact-pair lookup so the lossy OpenAI pivot is skipped where it is not needed. Trait objects are the wrong shape for pure functions.

**Executors are traits.** `trait Executor` behind `Arc<dyn Executor>` in a `HashMap<&str, Arc<dyn Executor>>` (`crates/router-sse/src/executors/mod.rs`). The special executors are codex, commandcode, grok-cli, opencode, opencode-go and codebuddy-intl; every other provider resolves to a cached `DefaultExecutor` that covers the OpenAI-compatible ones.

**`serde_json::Value` is the hub format.** Formats are lossy, provider-shaped and mutated in place; typed structs would mean one struct per format plus every provider quirk, and unknown-field passthrough is required. Type only the binary boundaries.

**Registry is committed data, embedded.** `crates/router-sse/src/providers/registry.json` is the source of truth; Rust does `include_str!` + serde into a `LazyLock`. The capability and pricing tables work the same way in `crates/router-sse/src/catalog/catalog.json`. Both are edited directly, so a change is a JSON diff with no code change unless a wire behaviour changes too.

**Frontend is built, not generated.** `web/` is Vue 3 + Vite + Tailwind + Biome; the release build embeds `web/dist` into the binary.

**The process is a daemon, not a router.** Three schedulers run on timers (proactive token refresh, quota auto-ping, model-catalog sync), several process-global state maps are deliberately ephemeral, and the outbound proxy is applied at boot and on every settings write. A request-path reading of the code misses all of this. See `docs/RUNTIME.md`.

**Naming splits by audience.** Every user-visible string reads RustRouter; every functional identifier stays `9router`. The `~/.9router` data directory (`APP_NAME` in `crates/router-db/src/paths.rs`), the `sk_9router` default key, the `9router.cliTool*` localStorage keys, the `model_provider = "9router"` / `[model_providers.9router]` TOML keys and the `x-9router-token-saver` header are each a stored or on-the-wire contract with something outside this process: a shared database, a saved browser preset, a configured CLI tool. Renaming one breaks that contract, so the two spellings are deliberate.

**A relay-generated 5xx does not cool the account.** A `520`–`527` status on a request that went through the relay worker (`is_relay_edge_error`, `crates/router-sse/src/handlers/chat.rs`) is an edge error, not a provider one, so the connection is returned to rotation instead of being locked. Direct provider 5xx handling, including the rate-limit URL fallback, is unchanged. The relay worker asks the origin for `accept-encoding: identity` and strips `content-encoding`, `content-length`, `transfer-encoding` and `connection` before building its response, so the body it forwards and the framing it declares agree.

---

## 3. Database contract

Full detail in `docs/DB-PARITY.md`. The short version:

- 11 tables, declared in `crates/router-db/src/schema.rs` with exact column order and types. `kv` keeps its composite `PRIMARY KEY (scope, key)`; `settings` keeps `CHECK (id = 1)`.
- PRAGMA set at open: `journal_mode=WAL`, `synchronous=NORMAL`, `temp_store=MEMORY`, `mmap_size=30000000`, `cache_size=-64000`, `foreign_keys=ON`, `busy_timeout=5000`. Those are the shared-file contract and stay byte-identical. rustrouter then applies a connection-local `MEMORY_PRAGMA_SQL` overlay (`cache_size = -2000`) that never reaches the file header; see `docs/DB-PARITY.md`.
- JSON-in-TEXT is `JSON.stringify(v ?? null)`. In Rust that means `preserve_order` (IndexMap) and **no** `skip_serializing_if`: `undefined` becomes `null` and the key stays.
- Timestamps are `Date.toISOString()`: UTC, millisecond precision, trailing `Z`. Use `to_rfc3339_opts(SecondsFormat::Millis, true)`.
- `usageDaily.dateKey` is computed in **local** time; `usageHistory.timestamp` is UTC. Both must match.
- Read-modify-write paths (`updateSettings`, connection upsert, `saveRequestUsage`) use `BEGIN IMMEDIATE` so `busy_timeout` can retry instead of deadlocking on a lock upgrade.
- The database is shared with 9router and the schema is byte-identical. Treat it as an external contract: a row rustrouter does not recognise is left untouched, never deleted.

---

## 4. Frontend

Detail in `docs/FRONTEND.md`. Summary:

- The dashboard is Vue 3 + Vite + Tailwind, with `vue-router`, `pinia`, `vue-chartjs`, `@vue-flow/core`, `@guolao/vue-monaco-editor` and `vue-draggable-plus`.
- `web/src/style.css` carries the global stylesheet. The Tailwind `source("../../")` scan root matters: get it wrong and the utility sheet comes out empty and the app builds unstyled.
- `index.html` runs a pre-paint theme script (keeping the `localStorage` key and the `{state:{theme}}` envelope so the Pinia persist plugin stays compatible), a fonts-loaded script, and the Material Symbols stylesheet.
- `vite.config.ts` aliases `@` to `src`; imports use `@/`.
- Biome needs `html.experimentalFullSupportEnabled: true` and `css.parser.tailwindDirectives: true`.
- English-only. There is no locale switcher, no translation runtime, and dates/numbers pin to `en-US`.
- Auth enforcement lives in the Rust backend. A SPA cannot enforce `/dashboard` redirects; the backend owns the 401 and the login redirect or the boundary is lost.

---

## 5. Testing and verification

- **Rust unit tests** per module. The suite is the primary correctness tool.
- **Database round trip.** Write a settings update, a connection, a combo, an API key and a usage record through rustrouter, then compare `sqlite3 <file> .schema` output and the raw TEXT of one settings row and one connection row. Byte equality, not JSON equality — this is what catches key ordering and date precision.
- **Golden fixtures** for translation, read as plain JSON. Requires `preserve_order` or key order alone produces false failures.
- **Per-change gate:** `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`, and `biome ci` in `web/`.

---

## 6. Risk register

| # | Risk | Mitigation |
|---|---|---|
| R1 | **Schema drift.** The schema is byte-frozen and shared, so a reordered or added column breaks the contract. | The schema is declarative in `router-db/src/schema.rs`. After any change, diff a `.schema` dump and the raw TEXT of a settings and a connection row. |
| R2 | **Shared-file concurrency.** Two applications open the same file; a writer that rebuilds it from memory would clobber the other's writes. | Pin one driver, open in WAL with `busy_timeout = 5000`, and write with `BEGIN IMMEDIATE`. See `docs/DB-PARITY.md` for the verdict. |
| R3 | **JSON byte-parity.** `serde_json` sorts keys by default and omits `None`. | `preserve_order`; no `skip_serializing_if`; explicit tests on a round-tripped settings and connection blob. |
| R4 | **No compile-time API contract.** The frontend fetches hand-written URLs and reads untyped fields; the Rust side can rename one and nothing fails until runtime. | Accepted. Response shapes are frozen and exercised end-to-end. Verify a changed `/api/*` response in the browser before merging. |
| R5 | **Registry data drift.** `registry.json` and `catalog.json` bake in derived behaviours (format defaults, OAuth injection, the TTS tables); a careless edit drops one silently. | Keep the two files the single source of truth and assert the parsed shape in tests; never mirror the data in Rust. |
| R6 | **Rotating refresh tokens.** Some providers issue a new refresh token per refresh; a clone-per-attempt retry re-uses a consumed token. | One shared mutable credential handle across retry attempts. |
| R7 | **Streaming semantics.** The stall watchdog must watch raw upstream bytes, not transform output; the null-chunk flush contract and `[DONE]` emission are load-bearing. | Keep the stage order; test with recorded upstream byte streams. |
| R8 | **Multipart corruption.** Parsing and re-encoding a multipart body changes the boundary. | Read the file part to `Bytes` and rebuild a `reqwest::multipart::Form`, and raise the axum body limit. |
| R9 | **SSRF guard erosion.** Three layers — literal host, DNS resolution, manual redirect re-validation; reqwest's default redirect policy silently drops the third. | `redirect(Policy::none())` and follow hops manually; dedicated test set. |
| R10 | **Auth boundary loss.** Client-side route guards are cosmetic. | The backend owns the 401 and the redirect; the Vue guard only mirrors it. |
| R11 | **Frontend constants coupling.** Client files need provider/model/capability data. | Serve it from the Rust API; do not duplicate the tables in TypeScript. |
| R12 | **Translator edge cases.** Translation is lossy by design and some pairs carry documented quirks. | Per-pair tests; a fix carries its own changelog entry. |
| R13 | **Launcher behaviour.** Kill-by-port, crash-restart, readiness poll, update check. | Folded into the binary as `rustrouter serve` / `start`. |
| R14 | **Autonomous schedulers.** Three timer loops fire outbound calls with no inbound request; a request-path reading of the code misses them entirely. | Enumerated in `RUNTIME.md`. |
| R15 | **Ephemeral state with load-bearing semantics.** Combo rotation resets on restart, and losing it changes which account serves a request. | Keep the semantics; mark the rest explicitly "reset on restart, correct". |
| R16 | **Files outside the database.** `jwt-secret`, `cli-secret`, machine ID, `model-catalog.json`, backups, updater status. Regenerating the secrets invalidates live API keys and CLI tokens. | Keep every path and format; see the table in `DB-PARITY.md`. |
| R17 | **Startup ordering.** The DB has to be migrated before any scheduler starts, or the schedulers read connections the moment they come up. | Recreate the sequence explicitly in `main.rs`; DB adapter before schedulers. |
| R18 | **Updater.** A static binary has no `npm i -g` self-update. | The check runs against GitHub Releases and the banner offers the `npm i -g rustrouter@latest` command. See `RUNTIME.md`. |

---

## 7. Decisions

These changed scope or correctness. The right-hand column is the resolution that is implemented.

| # | Decision | Resolution |
|---|---|---|
| D1 | Do two applications run simultaneously against one file, or is this a one-way cutover? | One-way cutover. Simultaneous is supported when both sides use a native SQLite driver. |
| D2 | Byte-identical TEXT payloads, or identical schema + semantically equal JSON? | Byte-identical. `preserve_order` + exact date format costs nothing. |
| D3 | Who owns `SCHEMA_VERSION` going forward? | The newer side owns it. rustrouter opens a database whose stored `schemaVersion` is ahead of its own `SCHEMA_VERSION` read-only in effect: it never invents a migration and never downgrades, and the additive sync only creates what it knows. A version it does not recognise is left untouched. |
| D4 | Keys in the settings blob that nothing reads? | Keep. The blob is written whole, and dropping a key changes what an existing row reads. |
| D5 | Preserve or fix translator quirks? | Fix only where a quirk is a security or data-loss issue; each fix carries its own changelog entry. |
| D6 | Usage recording for non-chat modalities? | Only embeddings record usage; search and fetch record nothing, so both applications agree about quota. |
| D7 | Language support? | English-only. No locale switcher, no translation runtime; dates and numbers pin to `en-US`. |
| D8 | How many CLI-tool writers? | Three: Codex, Claude Code, Hermes Agent. |
| D9 | `/dashboard/basic-chat` | Kept; it talks to the local `/v1/chat/completions`. |
| D10 | `/dashboard/settings/pricing` | Kept; it renders under `DashboardLayout` and is reachable by direct URL, with no nav entry. |
| D11 | Tray icon and OS autostart? | Tray: no. Autostart: yes when the platform supports it. |
| D12 | Runtime `models.dev` catalog sync (writes `model-catalog.json` next to the shared database)? | Keep; it only ever turns capabilities on. |
| D13 | Codex writer's TOML / `auth.json` split? | The split is intentional; a change carries its own changelog entry. |
| D14 | Updater? | No detached updater. The version check runs against GitHub Releases; `version/update` stays a 403. |
| D15 | Proxy-pool remote deploys (Cloudflare / Deno / Vercel relay workers)? | Keep; they are relays, not tunnels. |
| D16 | Quota auto-ping POSTs to a provider on a timer? | Keep; it is what keeps quota windows warm. |
| D17 | Auto quota tracker: when quota is exhausted, disable the connection; re-enable when usage returns? | Off by default (`quotaAutoTrackerEnabled: false`). Only tracker-disabled connections are re-enabled; a user-disabled one is untouched. |

---

## 8. Future direction

The registry and the capability/pricing tables are committed data, so most provider movement is a JSON edit rather than a code change. The extension points:

- **A provider changes upstream.** Edit `crates/router-sse/src/providers/registry.json` (and `catalog.json` for its capabilities and pricing) and commit the diff. The Rust side reads the new data with no code change. Only a new wire behaviour needs code.
- **Adding a provider.** Add its entry to `registry.json`. Add a special executor only if it does not round-trip through OpenAI; otherwise the cached `DefaultExecutor` covers it.
- **Adding a format pair.** Add a request and a response translator under `crates/router-sse/src/translator/` and list it in `register_all`, so it cannot be forgotten.
- **Adding a modality.** A core under `crates/router-sse/src/modalities/` plus routes under `/v1/*`, with its own lock key so a failing call cannot take the shared chat key offline.
- **Adding a dashboard page.** A view under `web/src/views/`, a route in `web/src/router/`, and `/api/*` handlers in `router-server`.
- **Adding a CLI-tool writer.** A module under `crates/router-server/src/routes/cli_tools.rs`, local-only guarded, serialising writes per resolved path.
- **Adding a table or column.** Not a local decision. The schema is shared and byte-frozen; a change is a contract change and needs a migration story on both sides.

When in doubt, read the document that owns the area (see the table at the top) before changing anything structural.
