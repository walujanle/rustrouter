# Releasing rustrouter

One tag on the default branch (`main`) builds six platform binaries, attaches
them to a GitHub Release with notes pulled from `CHANGELOG.md`, and publishes the
`rustrouter` npm packages. The workflow is `.github/workflows/release.yml`.

## Version: one source of truth

`PROJECT_VERSION` at the repo root holds `x.y.z` — nothing else, no `v`, no
`-beta`. `scripts/set-version.mjs` validates it against `^\d+\.\d+\.\d+$` and
exits 1 on anything else, so a prerelease version is not expressible and no
workflow that runs `--check` can ever accept a `-rc` tag. `scripts/set-version.mjs`
propagates it into every manifest that carries a version:

```
Cargo.toml                    [workspace.package] version
web/package.json              version  (shown in the UI as APP_CONFIG.version)
npm/package.json              version + optionalDependencies (the five platform pins)
npm/platforms/*/package.json  version
```

The running binary reports `APP_VERSION`, which is `env!("CARGO_PKG_VERSION")`,
so syncing `Cargo.toml` is what makes `/api/version` agree with the release.

`set-version.mjs` rewrites the two lock files as well — `web/package-lock.json` and the four
workspace-member entries in `Cargo.lock`. Every build path runs `--locked`, so a lock left at the
old version makes `npm ci` refuse the tree and `cargo build --locked` fail on the stale member.
`PROJECT_VERSION` stays the only place a version is edited.

```bash
node scripts/set-version.mjs          # write PROJECT_VERSION into every manifest
node scripts/set-version.mjs --check  # exit 1 if any manifest disagrees (CI runs this)
```

## Cut a release

1. Roll the changelog: move the `## [Unreleased]` entries under a new
   `## [x.y.z] — YYYY-MM-DD` heading.
2. Write the version:

   ```bash
   echo "x.y.z" > PROJECT_VERSION
   node scripts/set-version.mjs
   ```

3. Commit, then tag and push:

   ```bash
   git add -A && git commit -m "chore(release): x.y.z"
   git tag "vx.y.z"
   git push origin main "vx.y.z"
   ```

The tag push starts the workflow. It refuses to build if the tag (minus `v`)
does not equal `PROJECT_VERSION`, or if any manifest has drifted.

`workflow_dispatch` runs the build matrix without publishing — use it to smoke a
leg before tagging.

Every push to `main` and every pull request runs `.github/workflows/ci.yml`,
which gates the same checks a release depends on: `scripts/set-version.mjs
--check`, `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --
-D warnings`, `cargo test --workspace`, the frontend `npm run build`, `cargo
audit` and `npm audit --audit-level=high`.

## What the workflow produces

| Asset | Runner | Target |
|---|---|---|
| `rustrouter-linux-amd64-x.y.z` | `ubuntu-24.04` | `x86_64-unknown-linux-gnu` |
| `rustrouter-linux-arm64-x.y.z` | `ubuntu-24.04-arm` | `aarch64-unknown-linux-gnu` |
| `rustrouter-linux-android-aarch64-x.y.z` | `ubuntu-24.04` (own job) | `aarch64-linux-android` |
| `rustrouter-macos-arm64-x.y.z` | `macos-latest` | `aarch64-apple-darwin` |
| `rustrouter-windows-x64-x.y.z.exe` | `windows-latest` | `x86_64-pc-windows-msvc` |
| `rustrouter-windows-arm64-x.y.z.exe` | `windows-11-arm` | `aarch64-pc-windows-msvc` |

The Linux assets are glibc, not musl. The rustls backend is `aws-lc-rs`, which
compiles C and assembly through cmake; a musl cross-build would need a cross C
toolchain for `aws-lc-sys`, `libsqlite3-sys` and `zstd-sys` at once. Native
runners avoid that entirely. Revisit musl (via `cargo-zigbuild`) only if someone
needs a static binary.

Android is a separate job on `ubuntu-24.04` (x64), not a matrix leg. `cargo ndk`
needs the NDK, and Google publishes it only for an x86_64 Linux host — there is
no `linux-aarch64` NDK archive — while the `ubuntu-24.04-arm` image ships no
Android SDK at all. The x64 image presets `ANDROID_NDK_HOME` to NDK 27.3, so the
cross-compile to `arm64-v8a` runs there.

Each leg downloads the frontend `web/dist` artifact — `router-server` embeds it
with `rust-embed`, so the frontend is built once and shared. Every asset is
uploaded with a `<asset>.sha256` sidecar carrying its hex SHA-256, which is what
the update check's `digest` comparison and the Termux postinstall read.

## npm packages

Six packages, all published by the workflow:

```
rustrouter                 bin/rustrouter.js shim + optionalDependencies + postinstall
rustrouter-linux-x64       the binary for linux/x64 (glibc)
rustrouter-linux-arm64     the binary for linux/arm64 (glibc)
rustrouter-darwin-arm64    the binary for darwin/arm64
rustrouter-windows-x64     the binary for Windows x64
rustrouter-windows-arm64   the binary for Windows arm64
```

`npm install -g rustrouter` installs the main package, whose `optionalDependencies`
pull in only the one matching `os`/`cpu`. The `bin` target is a JS shim because
npm requires a shebang'd JS file; it resolves the platform package and execs the
real binary.

### Termux

Termux reports itself to npm as `linux`/`arm64`, so npm installs the glibc
package, whose binary cannot load under Android's Bionic loader. There is no
`android`/bionic `libc` value npm would match, so `scripts/postinstall.js`
detects Termux and downloads the `rustrouter-linux-android-aarch64-*` Release
asset over it, verifying the sha256 from the Release API `digest` first. The
download is fail-soft: a failure warns and never breaks `npm install`.

## Docker image (GHCR)

A `v*` tag also runs `.github/workflows/docker-publish.yml`, which builds and publishes
`ghcr.io/walujanle/rustrouter`. The image contract mirrors the 9router container: `DATA_DIR=/app/data`
is the volume, the health route is `GET /api/health` → `{"ok":true}`, and the port is **20129**.

The `Dockerfile` is three stages. `web` runs `npm ci && npm run build`; `build` compiles the release
binary with that `web/dist` embedded (and installs `cmake` + `clang`, because `aws-lc-sys` compiles C
and assembly through cmake); `runtime` is `debian:bookworm-slim` carrying only the binary. Two runtime
details are load-bearing and easy to lose:

- **`ca-certificates` is required.** The TLS verifier chain is
  `rustls-platform-verifier` → `rustls-native-certs` → `openssl-probe`, which reads the OS trust store.
  `debian-slim` ships none, and `reqwest` builds the verifier eagerly inside `ClientBuilder::build()`,
  so without the package every outbound HTTPS call fails at client construction — while the loopback
  `/api/health` check stays green, because it never leaves the host. The publish workflow asserts
  `/etc/ssl/certs/ca-certificates.crt` exists in each platform image before its digest is saved.
- **`HOSTNAME` is pinned to `0.0.0.0`.** `resolve_host()` reads `HOSTNAME`, and Docker sets it to the
  container id, so an unpinned image tries to bind an address that does not exist.

`gosu` and a root entrypoint chown the mounted data dir and drop to uid 10001, so a bind mount owned
by the host user still works; started with `--user` the chown is skipped. `HOME=/app/data-home` is
writable for the CLI-tool writers, the one feature that does not route through `DATA_DIR`.

The workflow refuses to build unless the tag (minus `v`) equals `PROJECT_VERSION` and
`set-version.mjs --check` passes, builds `linux/amd64` and `linux/arm64` on native runners,
smoke-tests each platform image, assembles the manifest and asserts it carries exactly those two
platforms, then moves `latest` on a tag push. A `workflow_dispatch` republish moves `latest` only
when `promote_latest` is set. Numbered tags are mutable, so pin a digest where a deployment must be
immutable. `docker-compose.yml` and `DOCKER.md` cover the run.

## Publishing: Trusted Publishing (OIDC)

The workflow publishes with `--provenance` and no stored token. It needs
`id-token: write` (set), npm ≥ 11.5.1 and Node ≥ 22.14 (set), and a trusted
publisher configured on npmjs.com for **each** package: GitHub Actions, repo
`walujanle/rustrouter`, workflow filename `release.yml`.

**A brand-new package has no settings page, so a trusted publisher cannot be
attached until the package exists, and OIDC cannot create a package.** The first
publish of each of the six names must be bootstrapped once with a token, from a
maintainer machine that is logged in (`npm whoami`). Six names, published in this
order:

1. `rustrouter-linux-x64`, `rustrouter-linux-arm64`, `rustrouter-darwin-arm64`,
   `rustrouter-windows-x64`, `rustrouter-windows-arm64`
2. `rustrouter`

Platforms first: the main package lists the five as `optionalDependencies`, so
they have to resolve when it is installed. The platform `bin/` directories are
gitignored and empty in a fresh checkout, so pull the binaries from the release
that already exists:

```bash
base=https://github.com/walujanle/rustrouter/releases/download/v0.1.1
ver=0.1.1
place() { mkdir -p "npm/platforms/$1/bin"; curl -fsSL "$base/$2" -o "npm/platforms/$1/bin/$3"; chmod +x "npm/platforms/$1/bin/$3"; }
place linux-x64     "rustrouter-linux-amd64-$ver"       rustrouter
place linux-arm64   "rustrouter-linux-arm64-$ver"       rustrouter
place darwin-arm64  "rustrouter-macos-arm64-$ver"       rustrouter
place windows-x64   "rustrouter-windows-x64-$ver.exe"   rustrouter.exe
place windows-arm64 "rustrouter-windows-arm64-$ver.exe" rustrouter.exe

for d in linux-x64 linux-arm64 darwin-arm64 windows-x64 windows-arm64; do
  (cd "npm/platforms/$d" && npm publish --access public)
done
(cd npm && npm publish --access public)
```

Then add the trusted publisher on each of the six package settings pages
(`npmjs.com/package/<name>/access` → Trusted Publisher) and remove the token.
Every release after that is token-free. Until a package is configured, only its
own `npm publish` step fails — the GitHub Release itself still succeeds.

This bootstrap is once per name. After it, a `v*` tag publishes through OIDC with
no stored token, so a new version needs no manual step.

## Verifying a release

- `node scripts/extract-changelog.mjs x.y.z` prints the notes that will be used.
- The Release should carry six assets, each with a `digest`.
- `npm i -g rustrouter` on a clean machine, then `rustrouter --version` prints
  the released version. On Termux the same command works through the postinstall
  path.
- A running older binary reports `updateAvailable: true` on `/api/version` and
  shows the sidebar banner within a couple of seconds of load.
