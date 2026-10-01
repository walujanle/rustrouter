# CLI-Tool Writers

rustrouter ships three CLI-tool writers: **Codex**, **Claude Code**, **Hermes Agent**. Each mutates files in the user's real home directory, so each is local-only guarded and serialises writes per resolved path.

All three live under `/api/cli-tools/` — `codex-settings`, `claude-settings`, `hermes-settings`, each GET/POST/DELETE. Handlers are in `crates/router-server/src/routes/cli_tools.rs`.

## Codex

Writes `~/.codex/config.toml` and `~/.codex/auth.json`.

- TOML is edited with `toml_edit`, so unknown keys and comments survive. A parse/serialize round-trip through a plain TOML value destroys both.
- The `Model` line is replaced in place, and the replacement must not abandon the line when a comment or a table header precedes `model =`. A `strip_prefix("model")?` that returns from the whole function on the first non-matching line silently drops the model from the generated file; find the `model =` line and edit only it.
- Sets `model`, `model_provider = "9router"`, `[model_providers.9router]` with `name = "RustRouter"`, `base_url` (normalised to end in `/v1`), `wire_api = "responses"`, `http_headers.Authorization`; plus `agents.default_subagent_model`, which falls back to `model` when `subagentModel` is absent.
- GET reports `has9Router` by looking for `model_provider = "9router"` or the `[model_providers.9router]` table.
- DELETE removes only the 9router fields, plus `OPENAI_API_KEY` and `auth_mode` from `auth.json` (removing the file when nothing else is left in it).
- The wire identifiers stay `9router` — `model_provider = "9router"` and the `[model_providers.9router]` table — because an already-configured Codex CLI reads them back; only the provider's display `name` is `"RustRouter"`. POST deletes the legacy `agents.subagent` key before writing `agents.default_subagent_model`; DELETE removes both.

POST writes the API key only to `config.toml`, as a static `Authorization` header; it does not write `auth.json`. DELETE does clean `auth.json`. Both paths are as shipped. Making POST also write `auth.json` changes behaviour and needs its own changelog entry.

## Claude Code

Writes `~/.claude/settings.json` and `~/.claude.json` (`mcpServers`).

- `serde_json`, plus a trailing-comma strip.
- Checks for the `claude` binary with `which`, falling back to a settings-file existence check.
- The `exa` MCP entry is a single inlined constant (`https://mcp.exa.ai/mcp`); `write_claude_json_mcp` merges or removes it and leaves the rest of `~/.claude.json` alone.
- POST merges `env` and sets `hasCompletedOnboarding`. A stored `ANTHROPIC_AUTH_TOKEN` wins over an incoming one, so DELETE is the only way to clear it. A truthy `autoCompactWindow` sets `CLAUDE_CODE_AUTO_COMPACT_WINDOW`.
- DELETE removes `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`, the three `ANTHROPIC_DEFAULT_*_MODEL` keys, `API_TIMEOUT_MS` and `CLAUDE_CODE_AUTO_COMPACT_WINDOW`, dropping the `env` object when it empties.
- Only GET tolerates JSONC. POST and DELETE parse `settings.json` strictly, so a trailing comma makes POST treat the file as empty (losing the existing settings) and makes DELETE fail. `write_claude_json_mcp` also parses strictly and treats an unparseable `~/.claude.json` as empty.

### The doubled `/v1` prefix, and GET detection

Claude Code appends `/v1/messages` to whatever `ANTHROPIC_BASE_URL` holds, and the writer normalises
that URL to end in `/v1`, so the client requests `POST /v1/v1/messages`. The LLM API is mounted at
`/v1`, `/v1/v1` and `/api/v1` through one `llm_api(prefix)` helper, so the doubled prefix reaches the
same handlers; dropping any of the three makes a configured Claude Code report the selected model as
unavailable.

GET reports `has9Router` when `env.ANTHROPIC_BASE_URL` is present, and `exaMcpEnabled` when
`~/.claude.json` carries an `mcpServers.exa` entry.

## Hermes Agent

Writes `~/.hermes/config.yaml` and `~/.hermes/.env`.

- `config.yaml` carries `model:` (default/provider/base_url/api_key), an optional `delegation:` block, and an `auxiliary:` block of per-role entries — all with `provider: "custom"`.
- `~/.hermes/.env` carries `OPENAI_API_KEY=`.
- Editing is regex-based on purpose. Routing the file through a YAML serializer would reorder keys and strip comments from a file the user also hand-edits; use the `regex` crate, never a YAML library.
- POST takes a `selections` array of `{role, model}` (a bare `model` is the older single-role shape) and requires a `default` role. Roles other than `default` and `delegation` become `auxiliary` entries.
- `has9Router` is true when a block has `provider: "custom"` and a `base_url` pointing at `localhost`, `127.0.0.1` or `0.0.0.0`.
- DELETE removes the `model:` and `delegation:` blocks and every `auxiliary` role whose provider is `custom`.

## Shared helpers

- Install probe (`which`-style), `dirs`-crate path resolution with a `HOME` override for tests.
- `/v1` normalisation of the base URL.
- JSONC pre-strip for Claude.
- Per-path `tokio::Mutex` keyed by resolved path, so two concurrent writes to the same file serialise.

## Frontend

One card per tool — `ClaudeToolCard.vue`, `CodexToolCard.vue`, `HermesToolCard.vue` — under `web/src/views/cli-tools/components/`, plus `ToolSummaryCard.vue` on the index. `BaseUrlSelect.vue` and `ApiKeySelect.vue` are the shared controls; `cliEndpointPresets.ts` holds the preset stores over `localStorage`; `codexConfig.ts` builds the Codex TOML preview.

Keep the storage keys stable so a user's browser presets survive: `9router.cliToolEndpointPresets` and `9router.cliToolApiKeyPresets` (change events `9router:endpoint-presets-changed` and `9router:api-key-presets-changed`).

`BaseUrlSelect.vue` and `cliEndpointPresets.ts` build the "local" endpoint from the running server's port. They call `liveAppPort()` (`constants/config.ts`), which reads `window.location.port` and falls back to `UPDATER_CONFIG.appPort` (20129) only where there is no browser. The server can be started on another port (`rustrouter start --port N`), and a baked 20129 would write the wrong base URL into a CLI tool's config and stop recognising the real local URL as a built-in preset. `cliEndpointMatch.ts` decides whether a configured URL counts as local by matching `localhost`, `127.0.0.1` or `0.0.0.0`.

## Guarding

Every handler needs the local-only guard, because they mutate the user's real dotfiles. In `crates/router-server/src/auth/guard.rs`, `LOCAL_ONLY_PATHS` holds `/api/auth/reset-password` and the three `cli-tools/*-settings` routes, and `/api/cli-tools` is part of the protected API prefix list.
