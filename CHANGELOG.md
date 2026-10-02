# Changelog

All notable changes to this project are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versioning is semantic.

## [Unreleased]

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

[Unreleased]: https://github.com/walujanle/rustrouter/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/walujanle/rustrouter/releases/tag/v0.1.2
[0.1.1]: https://github.com/walujanle/rustrouter/releases/tag/v0.1.1
[0.1.0]: https://github.com/walujanle/rustrouter/releases/tag/v0.1.0
