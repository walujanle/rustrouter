# Changelog

All notable changes to this project are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning is semantic.

## [Unreleased]

### Added

- **Netlify relay proxy pool**: a fifth pool type deployed through Netlify's
  digest API (`POST /sites` → `POST /sites/{id}/deploys` → upload only the
  required digests → poll until `ready`). The function is a CJS
  `exports.handler` returning a string body, because ESM and stream returns
  both answer 502 on that runtime. Responses are buffered, so a stream arrives
  in one piece and the 60 s synchronous limit applies. The relay URL is stored
  as `<site>/.netlify/functions/relay`.
- **Nous Research free tier**: eight `:free` models behind a device-code OAuth
  flow (`client_id=hermes-cli`) plus a post-exchange agent-key mint. The OAuth
  pair stays on the row for refresh while the short-lived agent key is what the
  executor sends; both are rotated on every refresh. Thinking uses the nested
  `reasoning: {enabled, effort}` shape, with `auto` mapped to `medium` because
  the upstream rejects `"auto"`. Validation is a one-token inference call, not
  `/models`, which is public and would accept any Bearer value. API-key
  requests through compatible nodes carry neutral `product=9router`,
  `client=9router`, `user=9router` attribution tags.
- **Per-key access control**: `apiKeys` gains `accessRestricted` and
  `accessAllow`, and a restricted key may call only the combos and models in
  its allow list. Matching is exact and case-insensitive on the resolved
  target; a listed combo grants its members when reached through that combo.
  A corrupt allow blob fails closed to zero entries. The dashboard exposes the
  toggle and a picker, and `/v1/models` shows a restricted key only what it may
  call. `SCHEMA_VERSION` is now 2; the columns are additive and existing rows
  read as unrestricted.
- **Model picker search**: `/regex/`, live-typed `/pattern`, `!term` and `-term`
  now work for both models and combos, a provider-name hit surfaces that
  provider's models, and multi-select callers get Select All / Deselect All.
- **Combo fallback on empty output**: a 2xx response with no assistant text, no
  tool call and no reasoning no longer wins the combo. The next model gets a
  turn unless this was the last seat.

### Fixed

- **Combo cycles aborted the process**: a combo whose `models` list names itself,
  or two combos naming each other, made `handle_single_model_chat` re-enter combo
  expansion until the stack ran out. The abort surfaced as `thread
  'tokio-rt-worker' (…) has overflowed its stack`, and raising the stack size only
  moved it. Expansion now carries the chain of combo names being resolved and
  answers 400 `Combo cycle: a -> b -> a` on a repeat, with a 16-level cap for a
  deep but acyclic nest. `POST`/`PUT /api/combos` reject a cycle at save time, so
  the graph never stores one. The bug was present in v0.1.1 as well; it is not a
  regression.
- **Runtime thread stack size is pinned**: `server_runtime` sets an 8 MiB stack
  explicitly. Tokio otherwise passes the choice to `std::thread`, which uses 2 MiB
  on both Unix and Windows, so this is headroom for the recursive JSON walkers,
  not the fix above.
- **npm install as a non-root user**: a distro Node's global prefix is `/usr`,
  owned by root, so `npm install -g rustrouter` fails during npm's reify step with
  `EACCES: permission denied, mkdir '/usr/lib/node_modules/rustrouter'` before any
  lifecycle script runs. The README and `npm/README.md` now name that cause and
  the remedy, `npm config set prefix ~/.local`, rather than a one-off `--prefix`,
  because the in-app update is a bare `npm i -g rustrouter@latest`. Windows is
  unchanged.
- **npm 12 blocked the Termux postinstall**: the shim is script-free, so the
  only casualty is the Android download. `bin/rustrouter.js` now turns the
  resulting `spawnSync … ENOENT` into a message naming the cause and the
  `--allow-scripts=rustrouter` flag, and distinguishes a missing platform
  package from a glibc mismatch. `npm/README.md` is added so the npmjs page
  carries the install instructions.
- **CodeBuddy Intl**: `www.codebuddy.ai` now rejects chat requests lacking the
  `X-Conversation-ID`, `X-Conversation-Request-ID` and `X-Conversation-Message-ID`
  header family, returning HTTP 400 code 11128 ("conversation id missing").
  `CodeBuddyIntlExecutor` overrides `build_headers` to inject all three: the
  conversation id is a stable UUID v5 (RFC 4122) derived from the session key
  (`prompt_cache_key` / `session_id` / `conversation_id`, then the first user
  message text), so the upstream keeps conversation state pinned across retries
  and failovers; the request and message ids are fresh UUIDs v4 per call.
- **`PUT /api/keys/{id}`** answered 405 while the dashboard sends PUT, so the
  pause toggle did nothing. Both PUT and PATCH are registered now.
- **Netlify pool type on update**: `VALID_UPDATE_TYPES` omitted `deno` and
  `netlify`, and the update path rewrote an unknown type to `http` in the
  shared database. Both lists now carry every deployable type.
- **OpenCode Go**: `claude-haiku-5-5` is served by `/zen/go/v1/messages` only,
  so it declares `targetFormat: "claude"` and the pipeline falls back to that
  transport when the client's wire format is not in `supportedFormats`. Without
  the fallback an OpenAI-wire client translated a Claude body and POSTed it to
  `/chat/completions`.
- **OpenCode Go deepseek**: `response_format` is stripped for the deepseek
  family, which rejects both `json_schema` and `json_object` with a 400 that
  names neither the field nor the reason.
- **OpenCode session ids**: the time field is now the low 36 bits of the digest,
  complemented and masked to 48 bits, with the counter pinned to 1. The old
  shape was the raw first six digest bytes, which the upstream decodes as a
  time stamp and can reject.
- **Claude PDF input**: every Claude capability row and family pattern now
  reports `pdf: true`; only one unrelated row had it before. A `*claude*sonnet-5*`
  pattern is added ahead of the generic sonnet row so a sonnet-5 id that is not
  in the model table still gets the adaptive format and the PDF flag.
- **Claude requests to non-Anthropic providers**: `context_management` is
  stripped, and thinking blocks with no visible text are dropped. Both are
  rejected upstream with a 400.
- **Codex tools**: an explicit `strict` on a tool now survives normalization
  instead of being discarded with the rest of the original object.
- **Retired Codex models**: `gpt-5.5`, `gpt-5.5-review`, `gpt-5.4*`,
  `gpt-5.3-codex-spark*` and the three matching image ids are gone from both
  registry tables. The auto-ping loop targets `gpt-5.6-terra`.
- **`/v1/models`**: no-auth free providers (opencode) stay listed once any other
  provider is connected, and a per-provider disable now applies in both
  branches.
- **Node deletion**: custom models registered under a deleted node are removed
  with it instead of lingering in the picker.
- **Usage stats**: the 24h branch falls back to `input_tokens` / `output_tokens`
  and then to the stored `promptTokens` / `completionTokens` columns. Keys are
  trimmed before lookup and masking, and the live branch buckets by the full
  key, so two keys sharing an 8-character prefix no longer merge.
- **Tooltips and the proxy dropdown** are teleported to `body` with fixed
  positioning, so a scroll container no longer clips them.
- **Flaky pending-counter test**: it used a `now_ms` 60 s in the past, so a
  parallel usage-stats test swept its counters mid-test. It now uses a future
  timestamp.

- **CodeBuddy Intl**: `www.codebuddy.ai` now rejects chat requests lacking the
  `X-Conversation-ID`, `X-Conversation-Request-ID` and `X-Conversation-Message-ID`
  header family, returning HTTP 400 code 11128 ("conversation id missing").
  `CodeBuddyIntlExecutor` overrides `build_headers` to inject all three: the
  conversation id is a stable UUID v5 (RFC 4122) derived from the session key
  (`prompt_cache_key` / `session_id` / `conversation_id`, then the first user
  message text), so the upstream keeps conversation state pinned across retries
  and failovers; the request and message ids are fresh UUIDs v4 per call.

## [0.1.5] - 2026-10-06

### Fixed

- **Quota tracker**: the CodeBuddy reset time showed `N/A` because
  `parse_reset_time` only accepted RFC-3339 and bare dates, while the billing
  endpoint emits the space-separated `YYYY-MM-DD HH:MM:SS` shape that JS
  `new Date()` accepts. The parser now normalizes that form (and a `UTC+8` /
  colonless `+0800` offset) before parsing.
- **Grok CLI**: the client identity was pinned to `0.2.99`, below the minimum
  `cli-chat-proxy.grok.com` now accepts, so upstream rejected every request with
  HTTP 426. The version is bumped to `1.0.46` (the current `@xai-official/grok`
  release) everywhere it is emitted — the executor constant, both copies of the
  `grok-cli` transport in `registry.json`, the models endpoint User-Agent, the
  OAuth test fallback, and the device-code User-Agent.

## [0.1.4] - 2026-10-02

### Fixed

- The release workflow published the main npm package with `npm publish npm`.
  `npm` is a registry package name — the CLI itself — so npm packed and tried to
  publish `npm@12.2.0` instead of the local `npm/` tree. The 0.1.3 run published
  the five platform packages and the GitHub Release, then failed here, so
  `rustrouter@0.1.3` was never published. The argument is now `./npm`.
- The GitHub Release step is re-runnable: it skips when the release for the tag
  already exists, so a re-run after a failed publish leg no longer aborts on
  `gh release create`.

## [0.1.3] - 2026-10-02

### Fixed

- `POST /api/cli-tools/hermes` writes the role into the YAML as a key and into a
  `Regex`, so an unescaped metacharacter in the request body either panicked
  `Regex::new` or matched the wrong block. The role is now escaped before it
  reaches the pattern, and the endpoint rejects any role outside the
  `[A-Za-z0-9_]` charset the read path already enforces.
- An upstream `error` event on the CommandCode stream panicked the response
  generator. It now emits an error frame and stops the stream, matching the
  openai-responses translator, so a client sees the failure instead of a
  dropped connection.
- Credential-refresh and OAuth HTTP calls, and the non-streaming response body
  read, had no deadline; a stalled upstream could hold the task open
  indefinitely. Both now time out (30s for OAuth, the streaming first-chunk
  budget for the body read) and surface a gateway timeout.
- `GET /api/keys` create/update and `POST /api/auth/login` returned a 500
  `"Failed to …"` / `"Unexpected end of JSON input"` for a malformed request
  body. They now return 400 `"Invalid JSON body"`.
- MITM DNS bypass matched hosts by substring, so `api2.cursor.sh.attacker.example`
  was treated as `api2.cursor.sh`. The match is now exact or on a dot boundary.
- `ApiError` no longer renders `DbError` to the client. The `Display` carries
  absolute filesystem paths and SQLite internals, which now go to the log while
  the response is a plain 500.
- A Docker install no longer reports a permanent "update available". The image's binary is
  compiled inside the image, so its hash can never match the release asset's digest; the
  `binaryChanged` signal made the banner nag on every container even when it was up to date. The
  signal is now suppressed for Docker, where the release asset is not the running binary at all.
- Outbound HTTPS no longer panics on Android/Termux with
  `Expect rustls-platform-verifier to be initialized`. `reqwest`'s `rustls`
  feature selects `rustls-platform-verifier`, whose Android backend is
  JNI-based and needs an Android `Context` and a JVM; a Termux process has
  neither, so the first TLS handshake aborted on a tokio worker thread. Every
  outbound client now builds through `router_sse::executors::http::tls_builder`,
  which on `target_os = "android"` calls `ClientBuilder::tls_certs_only` with a
  trust store loaded from Termux's own bundle unioned with the bundled Mozilla
  roots. The union matters: `tls_certs_only` with an empty store trusts nothing,
  so a Termux install without `ca-certificates` would have swapped the panic for
  a silent `UnknownIssuer` on every call. Other targets keep reqwest's default
  OS trust store unchanged.

### Changed

- Dead code removed, including the leftover Claude-provider branches, tool-cloaking
  helpers, and their unused constants. No behaviour changes.
- `tower` moved to `router-server` dev-dependencies (route tests only) and
  unused `async-trait`, `rand`, and `tokio-util` dependencies dropped.
- The duplicate `EditConnectionModal.vue` under `views/providers/components` is
  gone; both call sites use the `components/EditConnectionModal.vue` copy, which
  now also populates the form when opened with a connection already set.
- Toasts are announced to screen readers (`aria-live`, `role="alert"` for
  errors), row and card checkboxes carry an accessible name, and the tooltip
  wrapper reveals on `focus-within` so it is not hover-only.
- The dashboard is marked `noindex, nofollow` and ships a `robots.txt` that
  disallows crawling, so an instance on a public address is not indexed.
- Documentation corrected against the source: test counts, file counts, the
  Tailwind scan base, OAuth listener behaviour, search failover rules, the
  registry-derived CLI fingerprint, and the `jwt-secret` / `_meta` behaviour.
  `.env.example` gained the six environment variables the code reads but the
  file did not list, and `docs/CODEBASE-MAP.md` is now indexed from
  `docs/README.md`.

### Security

- `web/package.json` pins `dompurify` 3.4.16 through an override; `monaco-editor`
  pins 3.4.15 exactly, which carries GHSA-p98j-92pf-mc4p. `npm audit` is clean.

### Added

- The update prompt follows how rustrouter was installed. `GET /api/version` reports a new
  `installMethod` (`docker` | `npm` | `binary`), and `installCmd` is the command for that channel
  instead of the hardcoded npm string. The sidebar and the Settings page show a Docker update
  (pull the image, remove the container), the npm command, or a link to the latest GitHub release
  for a direct binary. `rustrouter update-check` prints the same method and command offline.

## [0.1.2] - 2026-10-01

### Fixed

- The Windows npm platform packages are named `rustrouter-windows-x64` and
  `rustrouter-windows-arm64`. npm's registry name filter rejects any new package
  whose name contains `win32` with `E403 Package name triggered spam detection`,
  so `rustrouter-win32-x64` and `rustrouter-win32-arm64` could never be created
  and `npm install -g rustrouter` on Windows installed the shim with no binary.
  The `os` field still reads `win32` — that is what `process.platform` reports
  and what npm matches against. A published version is immutable, so the fix
  could not be applied to 0.1.1.
- The npm publish steps in `release.yml` skip a package version that is already
  on the registry instead of failing. The bootstrap publish of a brand-new name
  puts that version on the registry before the first tag for it, so the tag run
  would otherwise hit `E403` on the existing version and abort the step.

### Added

- The README carries a Docker quick start. It calls out the required
  `-p 20129:20129`: without it the container starts but nothing on the host can
  reach it, which reads as an empty response in the browser.
- `DOCKER.md` and the README document the dashboard login, folded into the
  Docker quick start so there is one command to run. A container reached through
  a published port is not loopback — Docker's NAT makes the browser arrive from
  the bridge gateway — so the built-in `123456` is refused with "Default
  password must be changed before remote access". `INITIAL_PASSWORD` is the
  bootstrap that works; it is listed in `.env.example` and wired into
  `docker-compose.yml` with a default.

## [0.1.1] - 2026-10-01

### Fixed

- `get_executor` no longer races on a cold provider key. It did a `get` then an
  `insert` on the `DashMap`, so two threads could both miss, both build a
  `DefaultExecutor`, and hand back different instances; the
  "cached per provider" test failed intermittently. A `entry().or_insert_with`
  holds the shard lock across the check and the insert.
- The Android reclaim trim resolves `mallopt` with `dlsym` instead of calling it
  directly. `mallopt` is `__INTRODUCED_IN(26)` and `cargo-ndk` links against API
  21, so the direct call failed to link with an undefined symbol. The constant
  was wrong too: bionic's `M_PURGE` is `-101`, not `-6`. Where the platform
  predates the symbol the trim is a no-op.
- The Android aarch64 release leg runs as its own job on `ubuntu-24.04` instead
  of a matrix entry on `ubuntu-24.04-arm`. The ARM runner image ships no Android
  SDK, and Google publishes the NDK only for an x86_64 Linux host, so
  `cargo ndk` failed with "Could not find any NDK". The x64 image presets
  `ANDROID_NDK_HOME`. The native legs no longer carry the android-only steps.

## [0.1.0] - 2026-10-01

Initial release.

[Unreleased]: https://github.com/walujanle/rustrouter/compare/v0.1.4...HEAD
[0.1.4]: https://github.com/walujanle/rustrouter/releases/tag/v0.1.4
[0.1.3]: https://github.com/walujanle/rustrouter/releases/tag/v0.1.3
[0.1.2]: https://github.com/walujanle/rustrouter/releases/tag/v0.1.2
[0.1.1]: https://github.com/walujanle/rustrouter/releases/tag/v0.1.1
[0.1.0]: https://github.com/walujanle/rustrouter/releases/tag/v0.1.0
