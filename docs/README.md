# rustrouter Documentation

rustrouter is a local AI routing gateway: one OpenAI-compatible endpoint (`/v1/*`) that routes
across the upstream providers it ships with. The project README and `../AGENTS.md` cover what it is
and the hard constraints; this page is the map of the docs.

## Start here

`PLAN.md` — architecture, risk register, decisions, future direction. Read it first; everything else
expands a section of it. `CODEBASE-MAP.md` is the file-level index: every crate, module, and
frontend file with its purpose, for finding where a thing lives before reading code.

## The rest

| File | Read it when |
|---|---|
| `CODEBASE-MAP.md` | you need to locate a file, symbol, or entry point |
| `DB-PARITY.md` | you are touching `router-db` — schema, byte-exact JSON and date handling, repo semantics, the shared-file concurrency verdict |
| `CHAT-PIPELINE.md` | you are working on translation, executors, streaming, or account fallback |
| `OAUTH-AND-CREDENTIALS.md` | you are working on OAuth flows, token refresh, cooldowns, or combos |
| `MODALITIES.md` | you are working on embeddings, search, or web-fetch |
| `FRONTEND.md` | you are working in `web/` |
| `CLI-TOOLS.md` | you are working on the Codex, Claude Code, or Hermes config writers |
| `RUNTIME.md` | you are touching startup, schedulers, outbound proxy, or the launcher behaviour |
| `TESTING.md` | you are setting up tests or the test harness |
| `RELEASING.md` | you are cutting a release or publishing the npm packages |

## Four things to know before reading any of it

1. **Port 20129, same SQLite file, schema 100% identical.** The database is shared with 9router and the schema is byte-identical, so a schema or payload-byte divergence is a bug even when it "works". Both installs can run at once.
2. **The process is a daemon.** Three schedulers run on timers, and about a dozen state stores are deliberately ephemeral. `RUNTIME.md` lists them.
3. **The frontend is a Vue 3 app.** `web/` is Vue 3 + Vite + Tailwind, built into the binary.
4. **The provider registry is committed data.** `crates/router-sse/src/providers/registry.json` is the source of truth, edited directly (no generator); `registry.rs` and the frontend constants read from it, so a second copy would drift.
