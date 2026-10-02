# CODEBASE-MAP.md

A file-level index of the rustrouter repository: what each crate and module owns, where the
entry points are, and how the pieces connect. It is a map, not a replacement for the design
docs — `docs/PLAN.md` is still the architecture record, and `AGENTS.md` holds the editing
rules. Use this when you know roughly what you want to change and need the exact file.

Version indexed: `PROJECT_VERSION` = 0.1.2 (workspace 0.1.2, Rust edition 2024, MSRV 1.88).

## The two hard constraints

Everything below is shaped by two contracts. Violating either is a bug even when the code
"works".

1. **Port 20129.** Fixed. `resolve_port`/`resolve_host` in `router-server/src/state.rs`, overridable
   by `-p`/`-H` or `PORT`/`HOSTNAME`.
2. **The SQLite file is shared with 9router, schema byte-identical.** Tables, column order, types,
   defaults, indexes and the PRAGMA set must match byte for byte. A row rustrouter does not
   recognise is left untouched, never deleted.

Two byte-level rules fall out of (2) and apply across `router-db` and the wire layers:

- **JSON columns are byte-compared.** `serde_json` runs with `preserve_order`; values are built as
  `Value`; insertion order is preserved; an absent/`undefined` field becomes `null` **with the key
  kept** (`JSON.stringify(v ?? null)`). Only `null`/`false`/`0`/`""` are falsy.
- **Timestamps are UTC RFC 3339 with milliseconds and a trailing `Z`**
  (`to_rfc3339_opts(SecondsFormat::Millis, true)`), compared as strings. The one exception is
  `usageDaily.dateKey`, which is **local** `YYYY-MM-DD`. The two columns use different clocks on
  purpose.

## Repo at a glance

```
crates/
  router-db/       SQLite: schema, migrations, repos. The shared-schema boundary. No HTTP, no registry.
  router-sse/      Registry data, translators, executors, chat pipeline, modality cores. No HTTP.
  router-server/   axum: /v1* LLM surface, dashboard /api/*, auth, OAuth flows, CLI-tool writers, static assets.
  rustrouter/      bin: clap (serve | start | stop | update-check) + interactive TUI.
web/               Vue 3 + Vite + Tailwind v4 + Biome dashboard, built into the binary.
npm/               Published npm package tree (bin shim + per-platform packages).
scripts/           set-version.mjs, extract-changelog.mjs (release helpers).
docs/              Design docs. Start at docs/PLAN.md.
```

Dependency direction is one-way: `rustrouter` (bin) → `router-server` → `router-sse` →
`router-db`. `router-db` holds no registry knowledge (pricing and catalog are injected), which is
what keeps the graph acyclic.

## The request path

```
client → router-server/routes/v1.rs
           → router-sse/handlers/chat.rs        (auth, combo/fusion/capacity dispatch, account loop)
             → router-sse/handlers/chat_core.rs (format+transport resolution, token-saver, executor call)
               → router-sse/executors/*          (upstream HTTP, retry, URL fallback)
                 → response split:
                     chat_core/streaming.rs      live SSE
                     chat_core/non_streaming.rs  whole JSON body
                     chat_core/sse_to_json.rs    forced-streaming provider, JSON client
```

Format translation sits inside that path: `translator/` converts the client body to the target
wire format before the executor call and converts streamed chunks back on the way out.
`providers/registry.json` and `catalog/catalog.json` supply the routing identity, capabilities and
pricing that the pipeline resolves against.

---

## crate `router-db` — SQLite persistence

Declarative schema, versioned migrations plus additive sync, a small connection pool, and one repo
module per table group. This crate is the shared-schema boundary: it must stay byte-compatible with
9router.

| File | Purpose | Key symbols |
|---|---|---|
| `src/lib.rs` | Crate root: module declarations and public re-exports. | `pub use driver::Db`, `pub use error::{DbError, DbResult}`, `pub use paths::Paths`, `pub use rusqlite` |
| `src/schema.rs` | Declarative, byte-identical table/index definitions and the PRAGMA sets. Column order is load-bearing. | `SCHEMA_VERSION`, `PRAGMA_SQL`, `MEMORY_PRAGMA_SQL`, `TableDef`, `TABLES`, `create_table_sql` |
| `src/migrations.rs` | Versioned migration chain, additive schema sync (never drops/renames), pre-change backup gate at boot. | `MIGRATIONS`, `run_versioned_migrations`, `sync_schema_from_tables`, `migrate_on_boot`, `schema_change_pending` |
| `src/driver.rs` | Connection pool: PRAGMA set, `BEGIN IMMEDIATE` write transactions, busy retry, periodic WAL checkpoint. | `Db`, `open`, `with_conn`, `write`, `retry_busy`, `spawn_checkpoint_task`, `shrink_idle` |
| `src/paths.rs` | Data-directory resolution and the fixed filesystem layout shared with 9router. | `APP_NAME`, `resolve_data_dir`, `Paths`, `ensure_dirs` |
| `src/time.rs` | Timestamp and date-key helpers matching JavaScript byte-for-byte. | `now_iso`, `to_iso`, `parse_iso`, `local_date_key`, `timestamp_slug`, `local_midnight` |
| `src/json_col.rs` | JSON-in-TEXT helpers enforcing `JSON.stringify(v ?? null)`. | `parse_json`, `stringify_json`, `is_falsy`, `falsy_to_null` |
| `src/kv_store.rs` | Scoped key/value access over the single `kv` table (composite PK). | `SCOPE_MODEL_ALIASES`, `SCOPE_CUSTOM_MODELS`, `SCOPE_PRICING`, `get`, `set`, `remove` |
| `src/meta_store.rs` | Key/value access over the `_meta` table. | `get_meta`, `get_meta_i64`, `set_meta` |
| `src/identity.rs` | Machine identity and API-key derivation reproduced exactly so existing keys stay valid. Byte-critical: a wrong byte changes every client's key. | `consistent_machine_id`, `jwt_secret`, `generate_api_key_with_machine`, `parse_api_key`, `verify_api_key_crc`, `ApiKeyParts` |
| `src/backup.rs` | Lite safety backups taken before a schema change, plus mtime-based pruning. | `KEEP_BACKUPS`, `BACKUP_EXCLUDE_TABLES`, `backup_db_lite`, `prune_old_backups` |
| `src/export.rs` | Whole-database export/import; payload shape and field order are a frontend contract. | `export_db`, `import_db`, `kv_map`, `take_rest` |
| `src/stats.rs` | Process-global live-request state (pending counts, error marker, connection cache) behind one `OnceLock`. | `UsageState`, `state`, `track_pending_request`, `sweep_stale_pending`, `active_requests`, `split_model_key` |
| `src/error.rs` | Error type for the persistence layer. | `DbError`, `DbResult`, `is_busy`, `ProviderNameConflict`, `Busy` |
| `src/repos/mod.rs` | Lists the repository modules, one per table group. | `pub mod aliases/api_keys/combos/connections/nodes/pricing/proxy_pools/quota_tracker/settings/usage` |
| `src/repos/settings.rs` | The `settings` singleton; `DEFAULT_SETTINGS` declares every stored key and its order. | `default_settings`, `merge_with_defaults`, `get_settings`, `update_settings`, `export_settings` |
| `src/repos/aliases.rs` | Model aliases, custom models and disabled models over `kv` scopes. | `get_model_aliases`, `set_model_alias`, `get_custom_models`, `disable_models`, `enable_models` |
| `src/repos/api_keys.rs` | The `apiKeys` table: CRUD plus validity check. | `get_api_keys`, `create_api_key`, `update_api_key`, `delete_api_key`, `validate_api_key` |
| `src/repos/combos.rs` | The `combos` table; `models` is JSON-in-TEXT on every path. | `get_combos`, `get_combo_by_name`, `create_combo`, `update_combo`, `delete_combo` |
| `src/repos/connections.rs` | The `providerConnections` table: fixed columns plus a free-form `data` blob, with create-time dedup. | `row_to_conn`, `conn_to_row`, `upsert`, `reset_health_state_on_activation`, `create_provider_connection`, `reorder_in_tx`, `cleanup_provider_connections` |
| `src/repos/nodes.rs` | The `providerNodes` table. | `row_to_node`, `node_to_row`, `get_provider_nodes`, `create_provider_node`, `delete_provider_node` |
| `src/repos/pricing.rs` | User pricing overrides in the `pricing` kv scope; built-in catalog injected as `&Value`. | `get_user_pricing`, `get_pricing_for_model`, `update_pricing`, `reset_pricing` |
| `src/repos/proxy_pools.rs` | The `proxyPools` table. | `row_to_pool`, `pool_to_row`, `get_proxy_pools`, `create_proxy_pool`, `delete_proxy_pool` |
| `src/repos/quota_tracker.rs` | rustrouter-only auto quota tracker: pure decision logic plus its own auto-disable marker. | `is_quota_exhausted`, `plan_quota_action`, `apply_to_connection`, `was_auto_disabled`, `persist` |
| `src/repos/usage.rs` | Usage tracking: the hot save path (dedup + history + daily aggregate + lifetime counter) and all stats/chart/log reads. | `save_request_usage`, `aggregate_entry_to_day`, `get_usage_history`, `get_usage_stats`, `get_chart_data`, `get_recent_logs` |

**Tables and their owners.** `_meta` → `meta_store.rs` (also stamped by `migrations.rs`, written by
`repos/usage.rs`); `settings` → `repos/settings.rs`; `providerConnections` → `repos/connections.rs`
(plus `repos/quota_tracker.rs`, `export.rs`, `stats.rs`); `providerNodes` → `repos/nodes.rs`;
`proxyPools` → `repos/proxy_pools.rs`; `apiKeys` → `repos/api_keys.rs`; `combos` → `repos/combos.rs`;
`kv` → `kv_store.rs` with scopes owned by `repos/aliases.rs` and `repos/pricing.rs`;
`usageHistory` / `usageDaily` / `requestDetails` → `repos/usage.rs`. `requestDetails` is excluded
from lite backups by `backup.rs::BACKUP_EXCLUDE_TABLES`.

**Row mappers** (`row_to_conn`, `row_to_node`, `row_to_pool`, `export_db`) spread the `data` blob
first, then overlay the fixed columns — a fixed column wins a name clash but keeps the blob key's
position.

---

## crate `router-sse` — the routing engine

HTTP-free: it takes parsed JSON and credentials and returns values or byte streams, leaving status
mapping, route wiring and scheduler lifecycle to `router-server`. It is the largest crate and is
best read in five parts.

### 1. Registry and catalog — routing identity, capabilities, pricing

Two committed generated JSON dumps parsed once into `LazyLock` tables, plus the lookup modules. No
HTTP. `registry.json` is the source of truth for providers/models; `catalog.json` for capabilities
and pricing. Both are edited directly — never duplicate the data in Rust.

| File | Purpose | Key symbols |
|---|---|---|
| `src/providers/registry.json` | Committed registry dump: providers, transports, per-alias model lists, OAuth and media config. | `registry`, `providers`, `providerModels`, `providerOauth`, `providerMedia` |
| `src/providers/registry.rs` | Parses `registry.json` once into `REGISTRY` plus alias/id/models indexes; the per-request lookup surface. | `REGISTRY`, `Registry::parse`, `Registry::resolve_alias`, `Registry::models_for`, `Registry::transport`, `raw_models_for` |
| `src/providers/model.rs` | Typed shapes for a registry entry's model rows and transport blocks. | `Model`, `Transport`, `Model::kind`, `Model::upstream_id`, `Transport::format_or_default` |
| `src/providers/lookup.rs` | Model-string parsing and per-model lookups (validity, target format, upstream id, thinking suffix). | `parse_model`, `is_valid_model`, `model_upstream_id`, `model_target_format`, `infer_provider_from_model_name` |
| `src/providers/service.rs` | Per-request provider resolution: body-shape format detection, target format, transport matching. | `detect_format`, `get_target_format`, `resolve_transport`, `resolve_openai_compatible_api_type` |
| `src/providers/normalize.rs` | Canonicalizes a user-typed provider id and provider-specific-data blob. | `normalize_provider_id`, `normalize_provider_specific_data` |
| `src/providers/ui.rs` | JSON projection of the registry for the dashboard. | `build_provider_entry`, `provider_categories`, `providers_by_kind`, `media_provider_kinds`, `alias_maps` |
| `src/providers/shared.rs` | Shared provider constants; Anthropic beta-flag selection/merge. | `select_anthropic_beta`, `merge_anthropic_beta`, `ANTHROPIC_API_VERSION` |
| `src/providers/mod.rs` | Module wiring and public re-exports. | `lookup`, `model`, `normalize`, `registry`, `service`, `shared`, `ui` |
| `src/catalog/catalog.json` | Committed capability + pricing catalog: defaults, exact/pattern caps, model and pattern pricing. | `defaultCapabilities`, `modelCapabilities`, `patternCapabilities`, `modelPricing`, `patternPricing` |
| `src/catalog/catalog.rs` | Resolves capabilities, thinking levels and pricing from `catalog.json`, with a synced-catalog hook and cost math. | `get_capabilities_for_model`, `get_pricing_for_model`, `get_thinking_levels`, `calculate_cost_from_tokens`, `aggregate_combo_capabilities`, `CatalogSource` |
| `src/catalog/mod.rs` | Module wiring and public re-exports. | `Capabilities`, `Pricing`, `CatalogSource` |
| `src/constants.rs` | Fixed thinking-signature blobs and the Claude Code system prompt, copied verbatim. | `CLAUDE_SYSTEM_PROMPT`, `DEFAULT_THINKING_TEXT`, `DEFAULT_THINKING_*_SIGNATURE` |
| `src/credentials.rs` | The credential view carried through translation into every executor. | `Credentials`, `Credentials::bearer`, `Credentials::header`, `Credentials::psd_str`, `Credentials::session_scope_id` |
| `src/runtime_config.rs` | Code-owned runtime constants: statuses, TTLs, timeouts, retry table, media-fetch limits, image signatures. | `http_status`, `cache_ttl`, `default_retry_for_status`, `STREAM_STALL_TIMEOUT_MS`, `SSE_KEEPALIVE_INTERVAL_MS`, `IMAGE_SIGNATURES`, `BLOCKED_HOSTS` |

### 2. Translator — wire-format conversion

A pair-keyed registry plus a "concerns" library of format-agnostic helpers. Requests and streaming
responses pivot through OpenAI Chat: a `source:target` pair with a direct registered function wins
(lossless), otherwise the body goes source → openai → target. Response routing is keyed
`(provider_format, client_format)` and selected by `translate_response(target_format, source_format, …)`.
Whole-format conversion lives in `request/` and `response/`; everything cross-cutting (tool-call id
hygiene, usage folding, finish-reason spelling, modality stripping, image prefetch, param stripping)
lives in `concerns/` and is called by the pipeline.

Note: `formats.rs` defines wire-format constants for the whole family the pipeline can see
(including gemini and codex), but the translator registry registers **direct
routes only for Claude / OpenAI Chat / OpenAI Responses / CommandCode**. The Gemini family is
handled outside the registry, in `utils/gemini_bridge.rs` and `rtk/system_inject.rs`.

| File | Purpose | Key symbols |
|---|---|---|
| `src/translator/mod.rs` | Registry + the three public entry points and the shared streaming `ResponseState` bag. | `translate_request`, `translate_response`, `init_state`, `needs_translation`, `register_all`, `RequestFn`, `ResponseFn`, `ResponseState` |
| `src/translator/schema.rs` | Pure-data string constants for every format's roles, block types, finish reasons. | `role`, `openai_block`, `claude_block`, `responses_item`, `openai_finish`, `claude_stop` |
| `src/translator/formats.rs` | Wire-format id constants (the registry keys) and endpoint→format detection. | `OPENAI`, `OPENAI_RESPONSES`, `CLAUDE`, `GEMINI`, `COMMANDCODE`, `detect_format_by_endpoint` |
| `src/translator/formats/claude.rs` | Claude request prep: cache-control anchoring, empty-message filtering, tool_use/tool_result ordering, thinking placeholders, image hoisting, OAuth cloaking. | `prepare_claude_request`, `anchor_claude_cache`, `fix_tool_use_ordering`, `hoist_tool_result_images` |
| `src/translator/formats/gemini.rs` | One Gemini text helper used by combo text strategy and the OpenAI→Claude system builder. | `extract_text_content` |
| `src/translator/formats/max_tokens.rs` | `max_tokens` defaulting/auto-raise/clamping against a ceiling. | `adjust_max_tokens` |
| `src/translator/formats/openai.rs` | Final pass before an OpenAI request leaves: fold developer→system, drop thinking blocks, normalize tools. | `filter_to_openai_format`, `OpenAiFilterOptions` |
| `src/translator/formats/responses_api.rs` | Flatten the Responses shape (`input[]` + `instructions`) into chat `messages[]`. | `convert_responses_api_format`, `normalize_responses_input`, `coerce_responses_arguments` |
| `src/translator/request/claude_to_openai.rs` | claude→openai request: system folding, block conversion, tool mapping, missing-tool-response backfill. | `claude_to_openai_request` |
| `src/translator/request/openai_to_claude.rs` | openai→claude request: system extraction, turn assembly, tool_choice sanitizing, `max_tokens` vs model `maxOutput`. | `openai_to_claude_request` |
| `src/translator/request/openai_responses.rs` | Both openai-responses↔openai request legs; reverse leg re-emits reasoning items for `store:false` continuity. | `openai_responses_to_openai_request`, `openai_to_openai_responses_request` |
| `src/translator/request/openai_to_commandcode.rs` | openai→commandcode request: Anthropic-shaped `/alpha/generate` body, top-level system string, threadId/config stub. | `openai_to_commandcode_request` |
| `src/translator/response/claude_to_openai.rs` | claude→openai response: SSE event→chat.chunk; cache tokens captured at `message_start`, merged at `message_delta`. | `claude_to_openai_response` |
| `src/translator/response/openai_to_claude.rs` | openai→claude response: chat chunk→Claude SSE; tool args buffered/sanitized then emitted as one `input_json_delta`. | `openai_to_claude_response` |
| `src/translator/response/commandcode_to_openai.rs` | commandcode→openai response: AI-SDK v5 NDJSON events→chat chunks. | `commandcode_to_openai_response` |
| `src/translator/response/openai_responses.rs` | Both openai↔openai-responses response legs; only framed `{event,data}` translator. | `openai_to_openai_responses_response`, `openai_responses_to_openai_response` |
| `src/translator/concerns/primitives.rs` | JS-semantics primitives every translator relies on. | `build_chunk`, `js_truthy`, `js_nullish`, `js_string`, `js_number`, `js_json_number`, `safe_parse_json` |
| `src/translator/concerns/tool_call.rs` | Tool-call id sanitizing/generation and missing tool-result repair, run in place on the request body. | `ensure_tool_call_ids`, `fix_missing_tool_responses`, `generate_tool_call_id`, `get_tool_call_ids` |
| `src/translator/concerns/usage.rs` | Per-provider raw usage→OpenAI usage folding (claude/commandcode differ). | `to_openai_usage`, `extract_usage`, `build_usage`, `UsageArgs` |
| `src/translator/concerns/finish_reason.rs` | `finish_reason`/`stop_reason` spelling both ways. | `to_openai_finish`, `from_openai_finish` |
| `src/translator/concerns/modality.rs` | Strip multimodal blocks a model cannot read, replacing each with a turn placeholder. | `strip_unsupported_modalities` |
| `src/translator/concerns/prefetch.rs` | Fetch remote image URLs into inline base64 for targets that require it. | `prefetch_remote_images` |
| `src/translator/concerns/param_support.rs` | Config-driven strip of request params a provider/model rejects. | `strip_unsupported_params` |
| `src/translator/concerns/image.rs` | Security-sensitive remote-image fetch (SSRF guards) plus data-URI helpers. | `fetch_image_as_base64`, `parse_data_uri`, `detect_image_mime`, `is_private_ip` |

### 3. Executors and the chat pipeline

The executor layer is a strategy pattern: `executor.rs` defines the `Executor` trait with one shared
async send/retry/URL-fallback loop plus overridable hooks (`build_url`, `build_headers`,
`transform_request`, `parse_error`, `refresh_credentials`). `mod.rs` resolves a provider to a fresh
special executor (`SPECIAL`) or a cached `DefaultExecutor` (`DEFAULT_CACHE`). Most special executors
delegate to an inner `DefaultExecutor` and re-dispatch hooks through a `BaseLoop`/`LoopRunner`
adapter, so the default loop stays single-sourced.

The pipeline is `handlers/chat.rs` (HTTP entry, combo/fusion/account loop) → `handlers/chat_core.rs`
(orchestrator) → one of three response paths. The split is keyed on `client_requested_streaming` vs
`provider_requires_streaming` vs `stream`, not on the client format alone.

| File | Purpose | Key symbols |
|---|---|---|
| `src/executors/executor.rs` | The `Executor` contract: request/response types and the shared send-retry-URL-fallback loop. | `trait Executor`, `ExecuteRequest`, `UpstreamBody`, `UpstreamResponse`, `ExecError`, `execute` |
| `src/executors/mod.rs` | Executor resolution: special executors built fresh per call, everything else cached. | `get_executor`, `has_specialized_executor`, `SPECIAL`, `DEFAULT_CACHE` |
| `src/executors/default.rs` | Registry-driven default transport: auth application, compat URL building, header hooks, credential refresh. | `DefaultExecutor`, `AuthDescriptor`, `compat_build_url`, `apply_auth`, `refresh_for_provider` |
| `src/executors/http.rs` | Outbound HTTP plumbing: proxy config, per-key client cache, redirect policy, MITM DoH bypass. | `ProxyOptions`, `OutboundProxy`, `set_outbound_proxy`, `prepare_send`, `SendTarget` |
| `src/executors/oauth.rs` | Credential freshness and refresh-result merge: lead windows, staleness, error classification. | `should_refresh_credentials`, `refresh_lead_ms`, `merge_refreshed_credentials`, `is_unrecoverable_refresh_error` |
| `src/executors/retry.rs` | Per-status retry policy (attempts + backoff) merged from defaults and provider overrides. | `RetryConfig`, `RetryEntry`, `resolve_retry_entry` |
| `src/executors/identity.rs` | Cross-provider auth identity headers (kilocode org, registry oauth clientId). | `kilocode_org_header`, `oauth_client_id` |
| `src/executors/simple.rs` | CodeBuddyIntl special executor: forces streaming, reasoning summary, leading system prompt. | `CodeBuddyIntlExecutor` |
| `src/executors/codex.rs` | Codex Responses executor: request-shape invariants, session/account headers, image prefetch, SSE-peek retry, and the ECMAScript `\p{...}` pattern sanitizer Codex rejects with a 400. | `CodexExecutor`, `BaseLoop`, `normalize_codex_tools`, `peek_sse_transient_error`, `has_unicode_property_escape` |
| `src/executors/commandcode.rs` | CommandCode executor: forces stream, re-frames upstream NDJSON as SSE, retries transient statuses. | `CommandCodeExecutor`, `inspect_and_wrap`, `frame_as_sse`, `MAX_RETRIES` |
| `src/executors/grok_cli.rs` | Grok CLI executor: full CLI fingerprint headers, per-request turn index, reasoning-effort gating. | `GrokCliExecutor`, `SESSION_TURN_STORE`, `resolve_grok_cli_turn_idx` |
| `src/executors/opencode.rs` | OpenCode free-tier executor plus shared session/request-id derivation and responses-tool normalization. | `OpenCodeExecutor`, `LoopRunner`, `generate_session_id`, `normalize_responses_tools` |
| `src/executors/opencode_go.rs` | OpenCode Go executor: hashed session ids, responses-model routing, delegates to the default loop. | `OpenCodeGoExecutor`, `is_responses_model`, `RESPONSES_BASE_URL` |
| `src/handlers/chat.rs` | HTTP `/v1` chat entry: auth and bypass, combo/fusion/capacity dispatch, per-account selection loop with cooldown and fallback. | `handle_chat`, `ChatRequest`, `handle_single_model_chat`, `run_fusion`, `is_relay_edge_error` |
| `src/handlers/chat_core.rs` | Orchestrator: resolves source/target format and transport, applies token-saver chain, calls the executor, handles refresh-retry, dispatches. | `handle_chat_core`, `ChatContext`, `ChatBody`, `ChatResult`, `finish_non_streaming` |
| `src/handlers/responses_handler.rs` | Responses-API client bridge: folds an SSE body to JSON for a JSON client, passes streaming clients through. | `handle_responses_core`, `convert_responses_stream_to_json` |
| `src/handlers/chat_core/streaming.rs` | Live SSE path: transform-stream selection, non-SSE guard, disconnect/abort terminal bytes, usage save on completion. | `handle_streaming_response`, `build_transform_stream`, `StreamingRequest`, `SaveUsageFn` |
| `src/handlers/chat_core/non_streaming.rs` | Whole-body JSON path: reads the body, parses SSE if upstream streamed anyway, unwraps the Cline envelope, translates shapes. | `handle_non_streaming_response`, `translate_non_streaming_response`, `unwrap_cline_envelope` |
| `src/handlers/chat_core/sse_to_json.rs` | Forced-streaming-provider to JSON-client fold; DB-free, rides payloads back on `ChatResult`. | `handle_forced_sse_to_json`, `parse_sse_to_openai_response`, `is_responses_provider`, `shape_forced_sse_body` |
| `src/handlers/chat_core/request_detail.rs` | Usage extraction from the three provider shapes, done-line formatting, canonical usage row construction. | `extract_usage_from_response`, `format_done_line`, `save_usage_stats` |
| `src/handlers/chat_core/responses_convert.rs` | Shared chat-completion→Responses conversion plus custom-tool input extraction. | `completion_to_responses`, `responses_id`, `extract_custom_tool_input` |

### 4. Services and modalities — routing policy, credentials, non-chat cores

`services/` owns model/combo resolution, multi-account fallback policy, credential selection, OAuth
and token refresh, usage lookups and the daily catalog sync. `modalities/` owns the four non-chat
cores (embeddings, web-search, web-fetch, systemone) plus the SSRF guard they route through.
`router-server` is the only consumer, calling in from `routes/v1.rs`, `routes/oauth.rs`,
`routes/usage.rs`, `routes/catalog_sync.rs` and `services/schedulers.rs`.

| File | Purpose | Key symbols |
|---|---|---|
| `src/services/account_fallback.rs` | Pure multi-account fallback policy: `ERROR_RULES`, cooldown/backoff math, model-lock helpers. DB writes stay in the caller. | `check_fallback_error`, `FallbackDecision`, `cooldown_ms`, `build_model_lock_update`, `filter_available_accounts` |
| `src/services/auth.rs` | DB-facing account selection and cooldown bookkeeping: free virtual, pinning, round-robin, fill-first; locks/unlocks `modelLock` on failure/success. | `get_provider_credentials`, `SelectedAccount`, `mark_account_unavailable`, `clear_account_error`, `credentials_from_connection` |
| `src/services/background_token_refresh.rs` | Pure predicate for the proactive refresh tick (30-minute or provider lead). Scheduler lifecycle lives in `router-server`. | `select_connections_needing_refresh`, `BACKGROUND_REFRESH_LEAD_MS` |
| `src/services/token_refresh.rs` | Per-provider token refreshers behind single-flight, with rotating-refresh-token retry. | `refresh_with_retry`, `refresh_token_by_provider`, `check_and_refresh_token`, `apply_refresh_patch`, `RefreshOutcome` |
| `src/services/oauth_flow.rs` | OAuth flows for the four providers with a registry `providerOauth` entry (codex, grok-cli, kilocode, codebuddy-intl). | `generate_auth_data`, `exchange_tokens`, `poll_for_token`, `request_device_code`, `flow_type`, `decode_jwt_payload` |
| `src/services/single_flight.rs` | Generic single-flight map shared by token refresh, OAuth refresh locks and usage caches. | `single_flight`, `Slots`, `Slot` |
| `src/services/model.rs` | Model-string resolution over `providers::lookup`: splits provider/model, resolves aliases, signals a combo via an empty provider. | `get_model_info`, `ModelInfo`, `get_combo_models`, `reserved_provider_prefixes` |
| `src/services/combo.rs` | Combo strategies (fallback + sticky round-robin) plus the fusion/panel pass. | `handle_combo_chat`, `handle_fusion_chat`, `reorder_by_capabilities`, `detect_required_capabilities`, `collect_panel` |
| `src/services/combo_presets.rs` | Claude default combo presets; the source is accepted but yields nothing, so the route answers empty rather than 400. | `is_preset_source`, `build_preset_items` |
| `src/services/capacity_adapter.rs` | Capacity-adapter pools per input modality appended behind the primary models, plus history trimming on fall-through. | `get_active_adapter_strategy`, `augment_models_with_capacity_adapter`, `strip_history_for_context`, `get_capacity_adapter_config` |
| `src/services/connection_proxy.rs` | Resolves a connection's `providerSpecificData` into executor proxy config: pool row > legacy inline fields > none, with relay rewrites. | `resolve_connection_proxy_config`, `ResolvedProxyConfig`, `pick_proxy_pool_id`, `proxy_fields_into_psd` |
| `src/services/model_catalog.rs` | Builds the `/v1/models`, `/v1/models/{kind}`, `/v1/models/info`, `/v1beta/models` catalogs, including the grok-cli live resolver. | `build_models_list`, `resolve_live_models`, `model_info_lookup`, `gemini_models_list`, `INTERNAL_MODELS_FETCH_HEADER` |
| `src/services/model_catalog_sync.rs` | Daily models.dev catalog sync (download, diff, write, swallow failures) and the reader half installed into `catalog::catalog`. | `sync_model_catalog`, `start_model_catalog_sync`, `install_catalog_source`, `SYNC_INTERVAL_MS`, `CATALOG_VERSION` |
| `src/services/usage.rs` | Per-provider usage/quota dispatch; missing and numeric-zero differ, only Codex propagates errors. | `get_usage_for_provider`, `UsageConnection`, `parse_reset_time`, `fetch_with_timeout`, `consume_codex_rate_limit_reset_credit` |
| `src/services/usage/codex.rs` | Codex usage lookup: the one handler that throws instead of returning `{message}`; merges three rate-limit key shapes. | `get_codex_usage`, `get_codex_rate_limit_reset_credits` |
| `src/services/usage/grok_cli.rs` | Grok CLI usage: REST protobuf-json billing plus a hand-rolled gRPC-web quota-frame decoder. | `get_grok_cli_usage`, `parse_grok_cli_billing`, `decode_grok_credits_frame` |
| `src/services/usage/codebuddy.rs` | CodeBuddy-intl usage over Tencent's doubly-wrapped billing payload. | `get_codebuddy_intl_usage` |
| `src/services/ping.rs` | Model probe that loops back through the local `/api/v1` surface. | `ping_model_by_kind` |
| `src/services/proxy_test.rs` | Single-HEAD proxy reachability probe; transport failures are returned as bodies, never `Err`. | `test_proxy_url` |
| `src/services/stats_emitter.rs` | Process-wide broadcast emitter for debounced stats Update/Pending SSE frames. | `emit_update`, `emit_pending`, `subscribe`, `StatsEvent` |
| `src/modalities/mod.rs` | Modality core contract: response/error envelopes, the outbound helper, lock keys, header sanitizers. | `ModalityResponse`, `ModalityError`, `ModalityHttp`, `websearch_lock_key`, `sanitize_headers` |
| `src/modalities/embeddings.rs` | Embeddings core for mistral/nvidia/openrouter through one registry `embeddingConfig` read. | `embeddings_core` |
| `src/modalities/search.rs` | Web-search core for brave-search/exa/linkup/tavily/youcom with a single 15s global deadline. | `search_core`, `sanitize_query`, `GLOBAL_TIMEOUT_MS` |
| `src/modalities/fetch.rs` | Web-fetch core for firecrawl/tavily/exa; the target URL is SSRF-validated by the app layer before dispatch. | `fetch_core`, `DEFAULT_TIMEOUT_MS`, `DEFAULT_FORMAT` |
| `src/modalities/systemone.rs` | System One (Jev) decision-payload pass-through for opencode/openrouter. | `systemone_core` |
| `src/modalities/ssrf.rs` | Three-layer SSRF guard: literal host/IP checks, DNS-resolved checks, manual redirect re-validation. | `assert_public_url`, `assert_public_url_resolved`, `fetch_public`, `is_blocked_host`, `is_blocked_ipv4_int`, `PublicResponse` |

### 5. Utils, RTK, transformer, and crate-level helpers

`utils/` is the cross-cutting toolbox the translators and executors lean on. `rtk/` is the
**token saver** — a real feature, not plumbing: it rewrites `tool_result` text in place before and
after translation, is fail-open (any error or panic leaves the text untouched), and also injects
the caveman/ponytail system prompts. `transformer/` folds a Responses SSE stream back to one JSON
object. Everything here is byte-compare sensitive.

| File | Purpose | Key symbols |
|---|---|---|
| `src/utils/sse.rs` | **Load-bearing.** SSE wire constants and framing; byte-exact because frames are byte-compared downstream. | `SSE_DONE`, `SSE_HEADERS`, `sse_chunk`, `chat_chunk_sse` |
| `src/utils/stream.rs` | **Load-bearing (largest, ~40K).** The single SSE transform state machine with two modes: Passthrough and Translate. Per-stream incremental UTF-8 decoder. | `SseStream`, `SseStreamOptions`, `create_sse_stream`, `StreamMode`, `StreamHooks`, `finalize_stream` |
| `src/utils/stream_handler.rs` | **Load-bearing.** `pipe_with_disconnect`: one cancellation token for abort+disconnect, stall watchdog taps raw upstream bytes (not transform output), idle keepalive frame. | `pipe_with_disconnect`, `AbortTerminalFn`, `SSE_KEEPALIVE_FRAME` |
| `src/utils/stream_helpers.rs` | SSE line parse/format helpers. | `parse_sse_line`, `format_sse`, `has_valuable_content`, `fix_invalid_id` |
| `src/utils/responses_stream_helpers.rs` | Responses-API stream termination: event-name resolution, terminal detection, synthetic failure frame for an aborted stream. | `get_openai_responses_event_name`, `is_openai_responses_terminal_event`, `TERMINAL_EVENTS` |
| `src/utils/usage_tracking.rs` | **Load-bearing (money/context).** Extract, normalize, canonicalize (idempotent fold of cache counts into `prompt_tokens`), max-merge, estimate, format usage. | `normalize_usage`, `canonicalize_usage`, `merge_usage`, `estimate_input_tokens`, `estimate_output_tokens`, `format_usage`, `BUFFER_TOKENS` |
| `src/utils/claude_cloaking.rs` | **Load-bearing (anti-ban).** Claude OAuth cloaking: injects the Claude Code billing header and a deterministic fake `metadata.user_id`. | `apply_cloaking`, `CLAUDE_CLI_VERSION`, `CC_ENTRYPOINT` |
| `src/utils/fingerprint.rs` | **Load-bearing (provider gate).** OpenCode free-tier fingerprint: rename client tool variants to the canonical lowercase quartet; restore caller spelling on the response side. | `OPENCODE_FINGERPRINT_TOOLS`, `ToolNameMap`, `apply_fingerprint_tools`, `restore_tool_names` |
| `src/utils/in_flight.rs` | **Load-bearing (small).** `InFlightGuard` claims a static `AtomicBool` and clears it on `Drop`, so a panicking scheduled task does not leak the flag. | `InFlightGuard`, `acquire` |
| `src/utils/bypass_handler.rs` | Answers Claude Code bookkeeping requests (warmup, token count, title, terminal-clear) locally with canned responses. | `handle_bypass_request`, `BypassResponse`, `BypassKind` |
| `src/utils/error.rs` | Client-facing OpenAI-compatible error shapes: status→(type,code) table and body builder. | `build_error_body`, `error_type`, `default_error_message` |
| `src/utils/client_detector.rs` | Detects the calling CLI tool and whether the request is native to a provider (a native passthrough skips the translator). | `detect_client_tool`, `native_providers`, `NATIVE_PAIRS` |
| `src/utils/claude_signature.rs` | Claude thinking-signature validation (E-form and R-form). | `has_claude_signature_prefix`, `is_valid_claude_signature`, `lenient_base64_decode` |
| `src/utils/model_markers.rs` | Strips Claude Code's `[1m]` 1M-context model annotation (client-side only). | `strip_model_context_marker`, `strip_model_marker_from_body` |
| `src/utils/chat_log.rs` | The pipeline's tracing-backed logger: rotating/per-session tags, key masking, `ChatLog`/`ExecutorLog` impls. | `next_tag`, `tag_for_session`, `mask_key`, `TracingChatLog` |
| `src/utils/gemini_bridge.rs` | Gemini CLI wire facade: Gemini GenerateContent → internal OpenAI shape, run the pipeline, convert back. Native Gemini TTS unsupported. | `convert_gemini_to_internal`, `finish_reason_map` |
| `src/utils/ollama_transform.rs` | OpenAI SSE → Ollama NDJSON bridge for `POST /v1/api/chat`. | `PendingToolCall`, `done_line` |
| `src/utils/reasoning_injector.rs` | Injects a single-space `reasoning_content` placeholder into assistant tool-call turns for providers that reject it without one. | `inject_reasoning_content`, `PLACEHOLDER` |
| `src/utils/tool_deduper.rs` | Drops built-in client tools when an equivalent MCP tool is present. | `RULES`, `Pattern`, `Rule` |
| `src/rtk/mod.rs` | **Token saver entry.** `compress_messages` runs in place on the source-format body before translation (and again post-translate); fail-open; never grows, never empties. Also `inject_caveman`/`inject_ponytail`. | `compress_messages`, `compress_text`, `compress_kiro_format`, `RtkStats`, `inject_caveman`, `inject_ponytail` |
| `src/rtk/autodetect.rs` | Picks a filter from the text shape; detection order is load-bearing (build-output before porcelain). | `auto_detect_filter`, `RE_BUILD_OUTPUT`, `RE_GIT_DIFF`, `RE_PORCELAIN` |
| `src/rtk/filters/mod.rs` | Filter registry + shared caps and `safe_apply` (panic-catch). | `resolve_filter`, `apply_filter`, `safe_apply`, `RAW_CAP`, `MIN_COMPRESS_SIZE`, `DETECT_WINDOW` |
| `src/rtk/filters/git.rs` | git-diff, git-status, git-log compressors. | `git_diff` |
| `src/rtk/filters/misc.rs` | dedup-log, smart-truncate, read-numbered, build-output compressors. | `dedup_log` |
| `src/rtk/filters/search.rs` | grep, find, ls, tree, search-list compressors. | `SEARCH_LIST_HEADER_RE` |
| `src/rtk/system_inject.rs` | `inject_system_prompt`: appends an instruction into the system slot, dispatching by format (Kiro, Claude, Gemini, OpenAI). | `inject_system_prompt`, `inject_kiro_system`, `inject_claude_system`, `inject_gemini_system` |
| `src/rtk/prompts.rs` | Prompt text for the caveman/ponytail injectors. | `caveman_prompt`, `ponytail_prompt`, `SHARED_BOUNDARIES` |
| `src/transformer/stream_to_json.rs` | Folds a Responses-API SSE stream back into one response object when the client asked for JSON but the provider forced streaming. | `process_message`, `State`, `empty_usage` |
| `src/config/codex_instructions.rs` | `CODEX_DEFAULT_INSTRUCTIONS`: byte-identical copy of the Codex backend's default instruction prompt. | `CODEX_DEFAULT_INSTRUCTIONS` |
| `src/session_manager.rs` | Session-id resolution: three `LazyLock<Mutex<HashMap>>` stores with lazy TTL eviction and size caps. | `generate_binary_style_id`, `derive_session_id`, `resolve_session_identity`, `resolve_session_id`, `to_numeric_session_id` |
| `src/thinking.rs` | Thinking/reasoning normalization: reads client intent and rewrites it into the target provider's wire shape. | `EFFORT_LEVELS`, `effort_to_budget`, `ThinkingMode`, `extract_thinking`, `parse_suffix`, `apply_thinking` |
| `src/lib.rs` | Crate root: the module tree. No logic. | — |

---

## crate `router-server` — axum HTTP surface

`app.rs` builds one `Router<AppState>`: all dashboard `/api/*` routes, the LLM surface merged at
`/v1`, `/v1/v1`, `/api/v1` (the doubled prefix is deliberate, for Claude Code), plus `/v1beta`,
`/codex`, `/responses`, `/systemone`, then a static-asset fallback. A single deny-by-default
`middleware::guard` layer wraps everything, so a route is protected unless listed in the guard's
public tables. Every HTTP method+path is registered centrally in `app.rs`; the files under `routes/`
only export handler functions (there is no per-file router/nest).

### Core: app assembly, auth, schedulers, embedded dashboard

| File | Purpose | Key symbols |
|---|---|---|
| `src/app.rs` | Assembles the axum router and runs the server; owns route wiring, layer order and the LLM-prefix aliases. | `router`, `serve`, `llm_api`, `MAX_REQUEST_BODY_BYTES`, `shutdown_signal` |
| `src/lib.rs` | Crate root: module declarations and public re-exports. | `router`, `serve`, `ApiError`, `AppState`, `APP_VERSION` |
| `src/state.rs` | Process-wide `AppState` (DB, paths, session signer, limiter, CLI-token cache) plus blocking-pool helpers and env-resolved host/port. | `AppState`, `cli_token`, `has_valid_cli_token`, `require_login`, `read`, `write`, `resolve_port`, `resolve_host` |
| `src/middleware.rs` | The guard middleware: builds `RequestFacts`, drives `auth::guard::evaluate`, converts the verdict to a response. | `guard`, `facts_from`, `cookie`, `LiveGuard` |
| `src/error.rs` | The uniform JSON error type every dashboard route returns. | `ApiError`, `bad_request`, `unauthorized`, `forbidden`, `too_many_requests`, `with_extra` |
| `src/reclaim.rs` | Idle-gated memory reclaim: trims the C/mimalloc heaps once the process has been quiet for a few ticks. | `tracked`, `spawn`, `in_flight`, `trim`, `INFLIGHT`, `IDLE_TICKS` |
| `src/static_assets.rs` | Embeds `web/dist` via `rust-embed` and serves the SPA: real files, else `index.html` for client routes, 404 for unmatched API paths. | `serve`, `Assets`, `cache_control`, `missing_shell` |
| `src/auth/guard.rs` | Pure, tested auth policy: the public/protected/local-only path tables, client-IP/loopback trust rules, API-key extraction, the `evaluate` verdict. | `evaluate`, `RequestFacts`, `PUBLIC_API_PATHS`, `ALWAYS_PROTECTED`, `LOCAL_ONLY_PATHS`, `extract_api_key` |
| `src/auth/jwt.rs` | Hand-rolled HS256 JWS encode/decode, byte-compatible with `jose`. | `encode`, `decode`, `HEADER_JSON`, `hmac_sha256` |
| `src/auth/session.rs` | The `auth_token` session cookie: token mint/verify, bcrypt password check, cookie header construction. | `Session`, `create_token`, `verify_token`, `verify_dashboard_password`, `hash_password`, `SESSION_MAX_AGE_SEC` |
| `src/auth/login_limiter.rs` | In-memory progressive login lockout keyed by client IP (5 fails → 30s/2m/10m/30m). | `Limiter`, `check_lock`, `record_fail`, `LockStatus`, `MAX_FAILS_BEFORE_LOCK`, `client_ip` |
| `src/services/schedulers.rs` | The process-level timers: OAuth token refresh sweep (5 min), Codex quota auto-ping (60 s tick), the auto quota tracker; starts catalog sync. | `start_all`, `token_refresh_tick`, `run_quota_tick`, `send_codex_ping`, `REFRESH_INTERVAL_MS`, `QUOTA_TICK_MS` |
| `src/services/codex_proxy.rs` | Second axum listener on the fixed Codex OAuth callback port 1455: registers pending exchanges, completes token exchange, writes the connection row. | `start_proxy`, `register_session`, `session_status`, `CodexSession`, `CODEX_PORT` |
| `src/services/console_log.rs` | Process-global console-log ring buffer (200 lines) with batched fan-out over a broadcast channel; fed by a `tracing` layer. | `ConsoleLogLayer`, `append_line`, `logs`, `subscribe`, `MAX_LINES` |
| `src/services/update_check.rs` | Daily GitHub-Releases update check (tag version + running-exe sha256 vs asset digest), cached for `/api/version`; install-method detection. | `configure`, `refresh_if_stale`, `check_once`, `UpdateStatus`, `InstallMethod`, `CHECK_INTERVAL` |

### Routes

| File | Purpose |
|---|---|
| `src/routes/v1.rs` | The public OpenAI-compatible LLM surface (`/v1`, `/v1/v1`, `/api/v1`, `/v1beta`, `/codex`): chat, messages, responses, embeddings, search, web-fetch, models, gemini. Handlers delegate to `router_sse::handlers::chat::handle_chat`. |
| `src/routes/auth.rs` | Dashboard session auth: login, logout, status, reset-password. |
| `src/routes/misc.rs` | Public unauthenticated dashboard routes: health, init, version, require-login, shutdown. |
| `src/routes/settings.rs` | Settings GET/PATCH (never emits password hash or OIDC secret), database export/import, proxy-test. |
| `src/routes/keys.rs` | API-key CRUD; machine id always taken server-side, never from the request body. |
| `src/routes/combos.rs` | Model-combo CRUD plus preset listing/creation; invalidates process-local rotation state on rename/delete. |
| `src/routes/models.rs` | Model list, alias, disabled-model, custom-model and availability routes. |
| `src/routes/models_test.rs` | `POST /api/models/test` — probes one model by calling back into the server's own `/v1` pipeline over loopback. |
| `src/routes/registry.rs` | `GET /api/registry` — the client-side constants contract (providers, models, capabilities, icons) served once at SPA boot. |
| `src/routes/pricing.rs` | Pricing table read/patch/reset: merges built-in defaults with user overrides in the `pricing` kv scope. |
| `src/routes/providers.rs` | The dashboard connection routes: list/create/get/update/delete, client, suggested-models, test, test-batch, test-models, validate, models. |
| `src/routes/provider_nodes.rs` | Provider-node (self-hosted OpenAI/Anthropic-compatible or embedding endpoint) CRUD and validate; node id is the provider id. |
| `src/routes/proxy_pools.rs` | Proxy-pool CRUD, test, and edge-deploy (vercel/cloudflare/deno) routes. |
| `src/routes/oauth.rs` | OAuth action route for the kept providers plus codex/grok-cli bulk-import and token-import actions. |
| `src/routes/usage.rs` | Dashboard usage routes: stats, chart, history, logs, providers, request-logs, stream, per-connection usage, codex reset-credits. |
| `src/routes/catalog_sync.rs` | Model-catalog sync status + trigger, delegating to `router_sse::services::model_catalog_sync`. |
| `src/routes/cli_tools.rs` | Writers for the three kept CLI tools' settings files (claude/codex/hermes) under a per-path write lock; local-only. |
| `src/routes/skills.rs` | `GET /api/skills/{id}/SKILL.md` — bundled agent-skill markdown (`include_str!`), on the public allow-list. |
| `src/routes/tags.rs` | Static Ollama-shaped model tag list and OPTIONS; no DB, no upstream call. |
| `src/routes/translator.rs` | Debug routes: console-log get/delete/stream, trace-file load/save, translate/send pipeline introspection. |
| `src/routes/version_update.rs` | `POST /api/version/update` — always refuses with 403 (a static binary cannot self-update). |
| `src/routes/mod.rs` | Module declarations only. |

**Bundled agent skills.** `src/skills/9router{,-chat,-embeddings,-web-search,-web-fetch}/SKILL.md`
are served verbatim by `routes/skills.rs` on the public allow-list. They are the agent-facing
instructions for the modality endpoints.

---

## crate `rustrouter` — binary, CLI and TUI

The binary entry point. clap exposes four subcommands (`serve`, `start`, `stop`, `update-check`).
`start` runs the axum server on a background thread and drives an interactive TUI (Providers, API
Keys, Combos, CLI Tools, Settings) over the same HTTP API the dashboard uses.

| File | Purpose | Key symbols |
|---|---|---|
| `src/main.rs` | Binary entry point and clap CLI; owns runtime sizing, env overrides, tracing capture. | `Command`, `serve`, `start`, `stop`, `server_runtime`, `apply_overrides`, `wait_ready`, `init_tracing` |
| `src/launcher.rs` | Startup/stop behaviours: platform-specific port-holder kill and crash-restart supervision. | `kill_port_holder`, `supervise`, `RESTART_ATTEMPTS`, `pids_listening_on` |
| `src/mem_report.rs` | Env-gated resident-memory sampler (`RUSTROUTER_MEM_REPORT=1`), diagnostic only. | `spawn_if_enabled`, `sample`, `SAMPLE_INTERVAL` |
| `src/cli/mod.rs` | TUI shared surface: ANSI colours, `Ctx` (Api + port), key masking, truncation, time formatting, clipboard/browser helpers. | `Ctx`, `mask_key`, `truncate`, `relative_time`, `copy_to_clipboard`, `open_browser` |
| `src/cli/interface.rs` | Launcher first screen: `Choose Interface` (Web / Terminal / Exit) and dashboard open+wait. | `Interface`, `choose`, `open_dashboard` |
| `src/cli/term.rs` | Terminal primitives on crossterm: raw-mode key input, arrow-key select menu, prompt/confirm/pause, status lines. | `select_menu`, `prompt`, `confirm`, `pause`, `interactive` |
| `src/cli/menu.rs` | The two menu drivers the TUI is built on. | `Entry`, `show_menu_with_back`, `ListMenu`, `show_list_menu` |
| `src/cli/api.rs` | Blocking reqwest client the TUI drives the server with over `x-9r-cli-token`; typed methods for every menu endpoint. | `Api`, `get_providers`, `get_api_keys`, `get_combos`, `get_cli_tool_settings` |
| `src/cli/tui.rs` | Terminal UI main menu: header with endpoint and first key, five rows routing to the menus module. | `start`, `render_header` |
| `src/cli/model_selector.rs` | Pick a model from the live catalog grouped by `/v1/models` `owned_by`, filtered to active connections. | `select_model_from_list`, `ProviderIndex`, `ALIAS_ORDER` |
| `src/cli/menus/providers.rs` | Provider list, per-provider connections, connection actions, custom provider-node management; OAuth login flows. | `show_provider_detail`, `handle_add_auth_code`, `handle_add_device_code` |
| `src/cli/menus/api_keys.rs` | API Keys menu: list, create, copy, delete. | `show`, `handle_create_key`, `handle_delete_key` |
| `src/cli/menus/combos.rs` | Combos menu: list, create, edit, delete model-fallback chains. | `handle_create_combo`, `handle_edit_single_combo`, `models_chain` |
| `src/cli/menus/cli_tools.rs` | CLI-tools menu configuring Claude Code, Codex CLI and Hermes to point at this server. | `claude_quick_setup`, `codex_quick_setup`, `hermes_quick_setup` |
| `src/cli/menus/settings.rs` | Settings menu: RTK Token Saver toggle, password reset to default, auth-mode reset. | `toggle_rtk`, `reset_password`, `reset_auth_mode` |

---

## `web/` — Vue 3 dashboard

Vue 3 + Vite + Tailwind v4 + Biome SPA, embedded into the binary from `web/dist` and served on port
20129. It boots a Pinia registry payload fetched from `GET /api/registry` **before** the router
mounts, then drives ~24 dashboard routes under `DashboardLayout` plus bare landing/login/callback
routes. There is no compile-time API contract: every page talks to the backend through
`utils/api.ts` (fetch wrapper with dedup, TTL cache, timeout) against `/api/*`, with Vite proxying
`/api` and `/v1` to `localhost:20129` in dev.

### Scaffold

| File | Purpose |
|---|---|
| `package.json` | Vue 3.5, vue-router 5, pinia 4, Tailwind 4, chart.js, @vue-flow, monaco, vue-draggable-plus. |
| `vite.config.ts` | `@` → src alias, Tailwind plugin, custom html-one-line build plugin, dev proxy `/api` + `/v1` → localhost:20129. |
| `biome.json` | Tab indent, double quotes, organize imports; `noExplicitAny`/`noUnusedVariables` off **by design** (see `AGENTS.md`). |
| `tsconfig.json` / `tsconfig.app.json` / `tsconfig.node.json` | Solution + app + node TS configs; `@/*` path alias; TypeScript held at 6.x. |
| `index.html` | SPA shell: pre-paint dark-theme script, font-load detection, noscript fallback. |
| `src/main.ts` | Boot: awaits the registry store and theme, then creates app + pinia + router. |
| `src/App.vue` | Root: bare `<RouterView />`; all layout lives in routes. |
| `src/style.css` | Tailwind v4 import, self-hosted Geist + JetBrains Mono variable fonts, theme tokens. |
| `src/router/index.ts` | Routes: landing/login/callback bare; `/dashboard` children (endpoint, providers, combos, usage, quota, cli-tools, media-providers, translator, proxy-pools, skills, profile, basic-chat, pricing); lazy chunks; auth guard mirroring backend 401. |

### State, utils, constants, hooks

| File | Purpose |
|---|---|
| `src/stores/registry.ts` | Static client contract from `GET /api/registry`; getters merge provider category maps. |
| `src/stores/settings.ts` | Server settings blob: TTL-cached GET, coalesced fetch, PATCH merge, invalidate. |
| `src/stores/theme.ts` | light/dark/system; persists to localStorage `theme` in `{state:{theme}}` envelope read by `index.html`. |
| `src/stores/notification.ts` | Global toast queue with auto-dismiss timers. |
| `src/stores/headerSearch.ts` | Shared header search input; pages register/unregister on mount/unmount. |
| `src/utils/api.ts` | HTTP layer: 30s AbortController timeout, JSON error handling, concurrent-GET dedup, opt-in `cacheMs` TTL cache. |
| `src/utils/index.ts` | Barrel + `getErrorCode`, `getRelativeTime`. |
| `src/utils/cn.ts` | Class-name joiner. |
| `src/utils/overlayStack.ts` | Shared modal/drawer overlay stack: scroll lock, Escape/Tab routing, focus management. |
| `src/utils/bulkAdd.ts` | Bulk API-key planner: parses pasted lines, gap-fills non-colliding names. |
| `src/utils/providerIcon.ts` | Provider icon path resolver with session-level 404 memoization. |
| `src/utils/providerCustomModels.ts` | Builds custom/alias model rows per provider type for display. |
| `src/utils/providerModelsFetcher.ts` | Fetches suggested models for providers with a public models API, 10-min cache. |
| `src/utils/thinkingLevels.ts` | Derives a model's thinking-level set from reasoning flag, registry rows, pattern overrides. |
| `src/constants/config.ts` | App config: name/version, updater, live port, theme, endpoints, `CLIENT_STORE_TTL_MS`. |
| `src/constants/colors.ts` | Claude-inspired light/dark palette for the endpoint proxy UI. |
| `src/constants/models.ts` | `CAPACITY_META`, model kind, `useModels` accessors. |
| `src/constants/providers.ts` | `THINKING_CONFIG`, auth methods, compatible/custom-embedding prefixes, `useProviders`. |
| `src/constants/cliTools.ts` | CLI-tool descriptors (claude, codex, hermes) driving the CLI-tools pages. |
| `src/constants/skills.ts` | Agent-skill metadata for the skills page; builds copy URLs from the live origin. |
| `src/hooks/*` | `useCopyToClipboard`, `useModelCaps`, `useTheme`. |

### Views and components

Dashboard pages (`src/views/`): `EndpointPage.vue` and `EndpointConfigPage.vue` (both wrap
`endpoint/EndpointPageClient.vue`), `ProvidersPage.vue`, `ProviderDetailPage.vue`,
`ProviderNewPage.vue`, `CombosPage.vue`, `UsagePage.vue`, `QuotaPage.vue`, `TokenSaverPage.vue`,
`CliToolsPage.vue`, `CliToolDetailPage.vue`, `ConsoleLogPage.vue`, `TranslatorPage.vue`,
`ProxyPoolsPage.vue`, `SkillsPage.vue`, `ProfilePage.vue`, `BasicChatPage.vue`, `PricingPage.vue`,
plus bare `LandingPage.vue`, `LoginPage.vue`, `CallbackPage.vue`, `NotFoundPage.vue`.

Nested view folders:

- `views/endpoint/` — `EndpointPageClient.vue`, `components/EndpointRow.vue`, `components/SecurityWarning.vue`.
- `views/providers/components/` — `ProviderCard`, `ConnectionsCard`, `ConnectionRow`, `ModelsCard`,
  `ModelRow`, `AddApiKeyModal`, `AddCompatibleModal`, `AddCustomModelModal`,
  `CompatibleModelsSection`, `EditCompatibleNodeModal`, `EditConnectionModal`,
  `ModelAvailabilityBadge`, `CooldownTimer`, `BulkImportCodexModal`, `BulkImportGrokCliModal`.
- `views/combos/` — `utils.ts` (types + pure helpers), `components/ComboCard.vue`,
  `components/CapacityAdapterSection.vue`, `components/CapacityAdapterCap.vue`.
- `views/cli-tools/components/` — `ClaudeToolCard`, `CodexToolCard`, `HermesToolCard`,
  `ToolSummaryCard`, `ApiKeySelect`, `BaseUrlSelect`, `cliEndpointPresets.ts`,
  `cliEndpointMatch.ts`, `codexConfig.ts`.
- `views/media-providers/` — `MediaProvidersWebPage`, `MediaProviderKindPage`,
  `MediaProviderDetailPage`, `MediaComboDetailPage`, plus `components/` (`MediaProviderCard`,
  `ComboList`, `EmbeddingExampleCard`, `GenericExampleCard`, `MediaRow`, `exampleShared.ts`).
- `views/usage/components/` — `OverviewCards`, `ProviderBarChart`, `TopModelsChart`, `UsageChart`,
  `UsageTable`, `ProviderTopology`, and `ProviderLimits/` (`ProviderLimitsPanel`, `QuotaTable`, `utils.ts`).
- `views/landing/components/` — `HeroSection`, `LandingNavigation`, `LandingFeatures`, `HowItWorks`,
  `FlowAnimation`, `GetStarted`, `LandingFooter`.

Shared components (`src/components/`): `layouts/DashboardLayout.vue`, `AppHeader.vue`,
`AppSidebar.vue`, `HeaderMenu.vue`, `ComboFormModal.vue`, `ModelSelectModal.vue`, `OAuthModal.vue`,
`EditConnectionModal.vue`, `AddCustomEmbeddingModal.vue`, `NoAuthProxyCard.vue`,
`ProviderInfoCard.vue`, `ManualConfigModal.vue`, `PricingModal.vue`, `RequestLogger.vue`,
`UsageStats.vue`.

UI primitives (`src/components/ui/`): `UiButton`, `UiCard`, `UiInput`, `UiSelect`, `UiModal`,
`UiToggle`, `UiBadge`, `UiTooltip`, `UiSpinner`, `UiSkeleton`, `CardSkeleton`, `CardSection`,
`ConfirmModal`, `SegmentedControl`, `ThemeToggle`, `ProviderIcon`, `CapacityBadges`.

---

## Build, CI, npm and release tooling

| File | Purpose |
|---|---|
| `build.sh` | Linux/macOS production build: frontend first then backend, memory-sized cargo jobs, emits `rustrouter-binary/rustrouter`. |
| `build.bat` | Windows production build: `npm run build` then `cargo build --release`, emits `rustrouter-binary\rustrouter.exe`. |
| `build-android.sh` | Android aarch64 build (Termux native or NDK cross), memory-sized, emits `rustrouter-binary/rustrouter-linux-android-aarch64`. |
| `Dockerfile` | Three-stage image (web/Vite → rust build → debian-slim runtime) embedding `web/dist`; entrypoint drops privileges, runs `rustrouter serve` on 20129. |
| `docker-compose.yml` | Compose service for `ghcr.io/walujanle/rustrouter:latest`: port 20129, named data volume, `INITIAL_PASSWORD`. |
| `.github/workflows/ci.yml` | CI on push/PR: fmt, clippy `-D warnings`, workspace tests, MSRV 1.88 check, frontend build, advisory cargo/npm audit. |
| `.github/workflows/release.yml` | `v*` tag release: verify version, build web-dist, six platform binaries (+sha256) incl. Android, create GitHub Release from CHANGELOG, publish npm packages via OIDC trusted publishing. |
| `.github/workflows/docker-publish.yml` | `v*` tag (or manual dispatch) publishes a multi-arch (amd64/arm64) GHCR image, smoke-tests health, verifies manifest platforms, optionally promotes `latest`. |
| `npm/package.json` | Main npm package: bin shim + postinstall, pins five platform packages as exact-version `optionalDependencies`. |
| `npm/bin/rustrouter.js` | Node bin shim: resolves the platform package binary and execs it, setting `RUSTROUTER_INSTALL_METHOD=npm`. |
| `npm/scripts/postinstall.js` | Termux-only postinstall: swaps the glibc linux-arm64 binary for the Android release asset, verifying the sha256 digest. |
| `npm/platforms/{linux-x64,linux-arm64,darwin-arm64,windows-x64,windows-arm64}/package.json` | Per-platform packages carrying the real binary, gated by os/cpu/libc. |
| `scripts/set-version.mjs` | Propagates `PROJECT_VERSION` into every manifest (Cargo.toml, Cargo.lock, web package+lock, npm main + platform packages); `--check` verifies without writing. |
| `scripts/extract-changelog.mjs` | Prints the CHANGELOG section for a version (falls back to Unreleased) for the GitHub Release body. |
| `rust-toolchain.toml` | Pins the verified toolchain (1.97.1) with rustfmt + clippy; MSRV floor stays 1.88. |
| `PROJECT_VERSION` | Single version source (0.1.2). |
| `Cargo.toml` | Workspace root: four members, shared dependency versions, release profile (thin LTO, codegen-units=1, stripped). |
| `crates/*/Cargo.toml` | Per-crate manifests. Feature flags live here, versions in the workspace. |

---

## Documentation index

| File | Read it when |
|---|---|
| `docs/PLAN.md` | Anything structural. The architecture record; every other doc expands a section of it. |
| `docs/DB-PARITY.md` | Touching `router-db` — schema, byte-exact JSON and date handling, repo semantics, the shared-file concurrency verdict. |
| `docs/CHAT-PIPELINE.md` | Translation, executors, streaming, or account fallback. |
| `docs/OAUTH-AND-CREDENTIALS.md` | OAuth flows, token refresh, cooldowns, combos. |
| `docs/MODALITIES.md` | Embeddings, search, web-fetch. |
| `docs/FRONTEND.md` | Working in `web/`. |
| `docs/CLI-TOOLS.md` | The Codex, Claude Code, Hermes config writers. |
| `docs/RUNTIME.md` | Startup, schedulers, outbound proxy, launcher behaviour. |
| `docs/TESTING.md` | Setting up tests or the test harness. |
| `docs/RELEASING.md` | Cutting a release or publishing npm packages. |
| `README.md` | Installing, running, evaluating the project. |
| `AGENTS.md` | Editing rules and the "easy to get wrong" list. `CLAUDE.md` is a one-line import of it. |
| `SECURITY.md` | Threat model, trust boundaries, deployment checklist. |
| `DOCKER.md` | Running or publishing the image. |
| `CHANGELOG.md` | Recent behaviour changes; every code change updates it. |
| `.env.example` | Every env var rustrouter reads, all optional. |

---

## Coverage notes

This map was produced by reading the source tree directly. Known gaps and deliberate exclusions:

- **`mod.rs` files are not itemized.** Every `mod.rs` in the workspace is pure module wiring
  (a `pub mod` list and re-exports) with no logic. The crate root `lib.rs` files are the one
  exception and are listed.
- **Web components are listed by bare filename** under their folder heading in the `web/` section,
  not by full `web/src/...` path. Resolve them against the heading, e.g. `UiButton` under
  `src/components/ui/`.
- **Not indexed:** `.git/`, `target/`, `web/node_modules/`, `web/dist/`, and the compiled
  binaries under `npm/platforms/*/bin/`. Generated/derived, not source.
- **Static assets** (`web/public/fonts`, `web/public/icons`, `web/public/providers`) are binary
  assets served as-is; the code that resolves them is `web/src/utils/providerIcon.ts`.
- **`providers/registry.json` and `catalog/catalog.json`** are committed generated data, not code.
  Edit them directly when adding a provider or model — do not add a second copy in Rust.
- The Gemini wire family is visible in `translator/formats.rs` and the `concerns/` modules but has
  no direct registry route; it is handled in `utils/gemini_bridge.rs` and `rtk/system_inject.rs`.
- Kiro survives only as a body-shape probe (`rtk::compress_kiro_format`,
  `rtk::system_inject::is_kiro_body`); no format constant or registry provider produces it.
