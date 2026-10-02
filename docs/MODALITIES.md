# Non-Chat Modalities

Alongside the chat surface, `rustrouter` serves four non-chat cores: **embeddings**, **search**,
**web-fetch** and **System One**. They listen on port 20129 next to `/v1`, share the credential
store and the account-fallback machinery with chat, and are implemented without HTTP in the engine.

## Structure

```
crates/router-sse/src/modalities/mod.rs        shared transport, error envelopes, lock keys
crates/router-sse/src/modalities/embeddings.rs core
crates/router-sse/src/modalities/search.rs     core
crates/router-sse/src/modalities/fetch.rs      core
crates/router-sse/src/modalities/systemone.rs  core
crates/router-sse/src/modalities/ssrf.rs       SSRF guard
```

A core takes a parsed request body, the resolved provider id and credentials, and returns a
`ModalityResponse` (status, content type, body, optional usage) or a `ModalityError`. Auth,
credential selection and the multi-account fallback loop stay in the route layer
(`crates/router-server/src/routes/v1.rs`), which drives the cores through `account_loop` and
`run_modality_combo`. Combo expansion reuses the chat combo list.

Provider dispatch is a `match` over the provider id. There is no adapter trait, because each
modality has a fixed, small provider set and a trait per modality would carry more code than the
match. A core reads its provider's config — `embeddingConfig`, `searchConfig`, `fetchConfig` or
`systemoneConfig` — out of the generated registry rather than receiving it as an argument.

## Endpoints

`POST /v1/embeddings`, `POST /v1/search`, `POST /v1/web/fetch`, `POST /v1/systemone`. These are
mounted with the rest of the LLM surface, so each also answers under `/v1/v1` and `/api/v1`;
System One additionally answers on the bare `/systemone`. The Gemini facade at `/v1beta/models`
and `/v1beta/models/{*path}` serves `generateContent` / `streamGenerateContent`.

CORS is `tower_http::cors::CorsLayer::permissive()` on the router, applied outermost so preflight
OPTIONS is answered before the auth guard runs.

## Authentication gate

Every modality handler runs `api_key_gate` (`crates/router-server/src/routes/v1.rs`) before it reaches a core: it reads `settings.requireApiKey` and answers 401 when the key is missing or invalid. A failed settings read is a 503 `Settings unavailable`, never a skipped check — treating it as `requireApiKey: false` would open the gate exactly when the database is unhealthy. `/systemone` is also listed in the guard's public-LLM table (`crates/router-server/src/auth/guard.rs`), so it carries the same guard-level key requirement as `/v1`; it is a root rewrite outside the prefix list, and without that entry a remote keyless caller would reach the handler whenever `requireApiKey` is off.

## Per modality

**Embeddings.** `Json` in/out. Providers: **mistral**, **nvidia**, **openrouter**. All three go
through the OpenAI-compatible path; the core reads `embeddingConfig` and returns `usage` so the
billing hook fires. `input` must be a string or an array of strings.

**Search.** Providers: **brave-search**, **exa**, **linkup**, **tavily**, **youcom**. Five
builders and five normalizers are two `match` statements. `resolve_base_url` honours a
client-supplied `baseUrl` override through the SSRF guard. The **15 s global deadline** is
load-bearing: it spans the dedicated attempt, not each request. Failover follows
`check_fallback_error`: a 4xx cools the account down and falls over only for `401/402/403/429`
(and the message-matched quota rules); any other 4xx returns the upstream error for this request
without a fallback. There is no chat-search lane: a provider without a `searchConfig` has no
search support.

**Fetch.** Providers: **exa**, **firecrawl**, **tavily**. One `match`. `sanitize_header_value`
strips characters above U+00FF from header values before they go out, because a value built from
user input can carry characters HTTP cannot encode.

**System One.** A pass-through: URL and headers come from the provider's `systemoneConfig`, and
the request body and JSON response are forwarded untouched, since decision models have no chat
translation layer. Providers: **opencode** and **openrouter**. The `x-opencode-session` header is
generated for every request.

## SSRF guard

Three layers, all required, all security boundaries:

1. Literal host/IP checks.
2. DNS resolution of non-literal hostnames.
3. Manual redirect re-validation in `fetch_public`.

Use `reqwest` with `redirect(Policy::none())` and follow hops yourself — the default redirect
policy silently drops layer 3. Hand-roll IPv4→u32 and the IPv6 group parser; a crate that
normalises differently defeats the point, which is that `::ffff:127.0.0.1` and `::ffff:7f00:1`
compare equal.

Dedicated test set: loopback, link-local, private ranges, IPv6-mapped forms, DNS rebinding,
redirect-to-private.

## Lock-key scoping

`websearch:{id}` and `webfetch:{id}` exist so a failing search cannot write an account-wide `__all`
lock that takes a shared chat key offline. Do not drop the key argument to
`mark_account_unavailable`.

## Usage recording gap

Only embeddings and System One call `save_request_usage`. Search and web-fetch record nothing. The
database is shared with 9router and the schema is byte-identical, so both installs read the same
rows: either record usage for search and fetch, or keep the gap deliberately — do not let the two
diverge silently.

## Carve-outs

- **Keep `IMAGE_SIGNATURES`.** It lives in `crates/router-sse/src/runtime_config.rs` and is read by
  chat vision attachments (`crates/router-sse/src/translator/concerns/image.rs`) to detect image
  mime types. It is a magic-byte table that happens to sit near the modality code, not a modality
  core.

## Crates

The cores use `reqwest` (`stream`, `cookies`, `json`, `multipart`), `tokio`, `serde`/`serde_json`,
`base64`, `bytes`, `futures`, `tokio-util`, `regex`, `uuid`, `thiserror`, `tracing`; the routes sit
on `axum`.

Do **not** add the `image` crate — mime detection is a manual magic-byte table, not decoding.
