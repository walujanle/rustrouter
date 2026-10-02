# rustrouter

A local AI routing gateway, written in Rust, with a Vue 3 dashboard. One
OpenAI-compatible endpoint (`/v1/*`) fans out across the upstream providers you
configure, with format translation, model-combo fallback, multi-account
fallback, OAuth and API-key credential management, token refresh, usage
tracking, and an optional quota tracker.

rustrouter listens on **20129**. It shares one SQLite file with
[9router](https://github.com/decolua/9router) using a byte-identical schema, so a
database can move between the two installs with no migration step — and the two
can run side by side.

## Install

```bash
npm install -g rustrouter
```

The npm package ships a JS shim plus one platform binary per OS/arch. On
Termux, where npm reports `linux/arm64` but the glibc binary will not load under
Bionic, the postinstall script downloads the Android asset and verifies its
sha256 against the GitHub Release before swapping it in.

Or build from source:

```bash
cargo build --release      # embeds web/dist into the binary
./build.sh                 # or build.bat on Windows: frontend first, then cargo
```

### Docker

Every release publishes a ready-to-run image to GHCR, so there is nothing to
build — pull it and run it:

```bash
docker pull ghcr.io/walujanle/rustrouter:latest

docker run -d --name rustrouter \
  -p 20129:20129 \
  -v "$HOME/.9router:/app/data" \
  -e DATA_DIR=/app/data \
  -e INITIAL_PASSWORD=change-me \
  ghcr.io/walujanle/rustrouter:latest
```

`INITIAL_PASSWORD` is the dashboard password you log in with; the built-in
`123456` is refused through a published port, so set it (and change it in the UI
afterward). `-p 20129:20129` is required — without it the container starts but
the host has no route to it and the browser gets an empty response. The image is
multi-platform (`linux/amd64`, `linux/arm64`). Pin a version instead of `latest`
with `ghcr.io/walujanle/rustrouter:0.1.2`.

From the repo, `docker compose up -d` does the same thing in one command; set
`INITIAL_PASSWORD` in the environment to override the default. `DOCKER.md` covers
login, persistence, updates, and running as a non-root user.

## Run

```bash
rustrouter serve           # run in the foreground
rustrouter start           # kill whatever holds the port, run, restart on crash
rustrouter stop            # stop the process holding the port
rustrouter --port 8080     # override the port (also -p / -H)
```

Then open `http://localhost:20129/dashboard`, add a provider, and point your
client at `http://localhost:20129/v1`.

## What it does

One endpoint, many providers. A request in OpenAI, Anthropic, Gemini, or
Responses format is translated to whatever the selected provider speaks, and the
reply is translated back.

When a model fails, the combo moves to the next model; when an account fails,
the request moves to the next connection for that provider. Providers that need
OAuth get an OAuth flow, the rest take API keys, and tokens refresh proactively
and on a 401/403. A model that keeps erroring is cooled down.

Usage is accounted per provider, model, and account — tokens and cost — into the
shared SQLite tables. An optional quota tracker (off by default) disables a
connection whose quota is exhausted and re-enables it once usage is available
again.

Everything is local: no cloud sync, no telemetry. A container image is published
to GHCR for those who want one (`DOCKER.md`), but the binary has no runtime
dependency on Docker or Node.

## Layout

```
crates/
  router-db/       SQLite: schema, migrations, repos. The shared-schema boundary.
  router-sse/      Registry data, translators, executors, chat pipeline. No HTTP.
  router-server/   axum: /v1*, dashboard /api/*, auth, OAuth, static assets.
  rustrouter/      bin: clap (serve | start | stop | update-check)
web/               Vue 3 + Vite + Tailwind + Biome dashboard
npm/               the published npm package tree
scripts/           set-version.mjs, extract-changelog.mjs
docs/              design docs; start at docs/README.md
```

`PROJECT_VERSION` at the root is the single version source; `node
scripts/set-version.mjs` propagates it into every manifest. Cutting a release is
`docs/RELEASING.md`.

## Build and verify

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo audit

cd web && npm run build       # vue-tsc -b && biome check && vite build
cd web && ./node_modules/.bin/biome ci
cd web && npm audit
```

Toolchain floor is Rust 1.88 (`edition = "2024"`). TypeScript is held at 6.x:
`vue-tsc` 3.3 needs TypeScript's `./lib/tsc` export, which TypeScript 7 removes.

## Contributing

`AGENTS.md` has the rules that are easy to get wrong (JSON byte parity, date
formats, the registry, the shared database). Read it before changing
anything structural; the per-area docs it points to are the next step.

## Security

`SECURITY.md` covers the trust model, the accepted risks, and what the gateway
stores and logs. Report anything exploitable privately, not in a public issue.

## License

MIT.
