# OAuth, Credentials, Token Refresh, Account Fallback

Scope: `crates/router-sse/src/services/{oauth_flow,token_refresh,account_fallback,auth,background_token_refresh,single_flight}.rs`, `crates/router-server/src/routes/oauth.rs`, `crates/router-server/src/services/codex_proxy.rs`, `crates/router-server/src/auth/guard.rs`, `crates/router-db/src/repos/connections.rs`.

## What this covers

**4** provider OAuth flows ship: `codex`, `grok-cli`, `kilocode`,
`codebuddy-intl`. Credential storage in `providerConnections.data`,
proactive and reactive token refresh, per-model cooldown, and the
account-selection loop. These authenticate to **upstream AI providers**; the
dashboard login is a separate mechanism.

## Crates

`reqwest` (rustls, form, json), `tokio`, `serde`/`serde_json` (`preserve_order`),
`sha2`, `base64` (`URL_SAFE_NO_PAD` for PKCE/JWT), `rand`, `url`, `thiserror`,
`dashmap`, `uuid`.

## Module layout

```
crates/router-sse/src/services/
  oauth_flow.rs                authorize URL, token exchange, device code, poll
  token_refresh.rs             refresh_with_retry, per-provider refreshers, single-flight
  background_token_refresh.rs  the "is this connection due?" predicate
  account_fallback.rs          error rules, model locks, cooldown maths
  auth.rs                      account selection, mark_account_unavailable, clear_account_error
  single_flight.rs             generic in-flight dedup
crates/router-server/src/
  routes/oauth.rs              the /api/oauth/* actions
  services/codex_proxy.rs      the Codex 1455 callback listener and its session map
  auth/guard.rs                the local-request origin guard
crates/router-db/src/repos/connections.rs   providerConnections read/write
```

## Provider dispatch

Provider config comes from `registry.json`'s top-level `providerOauth` map, never
from a second table in Rust. `oauth_flow.rs` reads it through
`registry().oauth(provider)` and dispatches on the provider id:

- `flow_type(provider)` returns `authorization_code_pkce` (`codex`) or
  `device_code` (`grok-cli`, `kilocode`, `codebuddy-intl`).
- `generate_auth_data` builds the PKCE pair and the authorize URL.
  `exchange_tokens` completes the auth-code flows; `request_device_code` and
  `poll_for_token` drive the device-code flows.
- `build_auth_url` and the per-provider token mappers are match arms on the
  provider id.

`flow_type` is a hand-maintained table because the registry stores config, not
the flow. A new provider needs both a `providerOauth` entry and a match arm.

## Credential storage

`providerConnections.data` is a JSON blob whose key names and null-vs-absent
semantics are load-bearing. The database is shared with 9router and the schema is
byte-identical, so this mapping is a contract: any serde mismatch breaks DB
compatibility.

`crates/router-db/src/repos/connections.rs` owns the mapping:

- `row_to_conn` parses `data` first, then overwrites the fixed columns `id`,
  `provider`, `authType`, `name`, `email`, `priority`, `isActive`, `createdAt`,
  `updatedAt`. A key already present in the blob keeps its original position, so
  the exported JSON is byte-identical.
- `conn_to_row` strips those columns back out and stringifies the rest into
  `data`. `isActive` is `false ? 0 : 1` — anything not literal `false` is 1.

Everything outside the fixed columns is a free-form map, not fixed fields:
cooldown state is stored as dynamic `modelLock_<model>` keys, and model names
contain `/` and `:`.

## Refresh

- `refresh_with_retry` runs 3 attempts with a linear `attempt * 1000ms` delay.
  `classify_oauth_refresh_error` marks a permanent failure, and
  `is_unrecoverable_refresh_error` treats `unrecoverable_refresh_error`,
  `refresh_token_reused`, `invalid_request` and `invalid_grant` as
  unrecoverable, so a permanently-dead refresh token is not retried.
- Dedup: `dedup_refresh` runs a 10-second single-flight window keyed
  `provider:oldToken`. Two concurrent requests with the same token must share one
  in-flight future, or both hit the provider and one invalidates the other's
  rotated token.
- A second lock, keyed by connection rather than token, stops two requests for one
  account from refreshing even when they arrive with different tokens.
- **Rotating single-use refresh tokens.** `grok-cli` and `codex` issue a new
  refresh token on every refresh, and `refresh_with_retry` mutates the credential
  handle in place between attempts. A clone-per-attempt design re-uses a consumed
  token and gets `invalid_grant`. Mutating in place is required even though the
  attempt is `&mut`, so nothing in the retry loop may take a copy of the token.
- **A connection whose provider has no `providerOauth` entry is never selected for refresh.**
  `background_token_refresh::select_connections_needing_refresh` skips it before the refresher runs,
  so a stale row for a provider with no refresher is inert data the tick stops chasing. Without that
  predicate the row failed on every tick and logged a failed refresh line on every tick.
  `authType` is matched case-insensitively with underscores removed (`to_lowercase().replace('_', "")`),
  so `O_Auth` counts as OAuth and `OAuth_2` does not.
- Background sweep (`crates/router-server/src/services/schedulers.rs`): a
  30-minute default lead (`BACKGROUND_REFRESH_LEAD_MS`, or the provider's own
  larger lead), a 5-minute interval, a 10-second initial delay, and a sequential
  `1.5s + 200ms` inter-account delay so a batch of refreshes does not look like
  credential stuffing.

## Per-provider quirks

- **Claude** aligns `x-claude-code-session-id` with `metadata.user_id.session_id`
  when the client sends no such header and the token carries `sk-ant-oat`. The
  `sk-ant-oat` gate keeps this to OAuth connections; an API-key connection is left
  alone.
- **Codex** refreshes proactively on a stale stamp: `maxRefreshAgeMs` is 8 days
  and `refreshLeadMs` is 5 days, both read from its registry entry. It also has a
  bulk-import path.
- **CodeBuddy** posts the literal `{}` (two bytes) to its billing endpoint
  (`/v2/billing/meter/get-user-resource`). The endpoint answers 400 to a zero-byte
  body, so the request is a JSON POST with a body, not an empty one.
- **grok-cli** resolves its quota fields with two different fallback operators.
  The numeric picks use nullish coalescing, so a present `0` stops the chain; the
  reset-time picks use truthy fallback, so `0` and `""` fall through.
  `subscription_tier` resolves nullish first and only then coerces, so a present
  non-string yields `""` rather than falling through to the next key.

## Loopback origin guard

Legit OAuth redirects send **no** `Origin` header.
`crates/router-server/src/auth/guard.rs` allows an absent `Origin` and rejects a
non-loopback one, which blocks login-CSRF without breaking the redirect.
`is_loopback_hostname` handles IPv6 and `::ffff:` forms.

## Import paths

The `/api/oauth/*` routes carry three import flows: `POST
/api/oauth/codex/bulk-import`, `POST /api/oauth/codex/import-token` and `POST
/api/oauth/grok-cli/bulk-import`. Each writes a connection row through the same
repo as a normal connect. The `exchange` action also accepts a raw JWT pasted in
place of a code and stores it as an `access_token` connection.

## Browser launch

The frontend opens the auth URL (`window.open` in
`web/src/components/OAuthModal.vue`); the backend only supplies it.
`web/src/views/CallbackPage.vue` relays the code: `postMessage` to an allowlist
(`window.location.origin` and `http://localhost:1455`), then `BroadcastChannel
'oauth_callback'`, then a `localStorage` fallback. The Codex path depends on the
`1455` listener in `crates/router-server/src/services/codex_proxy.rs`; if it
breaks, the popup closes with no error.

## Account selection

`get_provider_credentials(provider, excludeConnectionIds, model)` in
`crates/router-sse/src/services/auth.rs` returns `AccountSelection::Selected`,
`AllRateLimited` or `None`. The loop in `crates/router-sse/src/handlers/chat.rs`:

1. Get credentials for the next candidate, excluding already-failed connection ids.
2. `check_and_refresh_token`.
3. Call the chat core.
4. On failure: `mark_account_unavailable(...).should_fallback` decides.
5. On fallback: add the connection id to the exclusion set, record the error, loop.
6. On success: `clear_account_error`.

`mark_account_unavailable` writes a per-model cooldown as `modelLock_<model>` with
exponential backoff (`BACKOFF_BASE_MS` 2s, doubling, capped at `BACKOFF_MAX_MS` of
5 minutes), classified by `ERROR_RULES`. A provider-reported rate-limit reset is
capped at `MAX_RATE_LIMIT_COOLDOWN_MS` (30 minutes).

`clear_account_error` resets `testStatus`/`lastError` **only when no active locks
remain**, clears the succeeded model's lock, and drops expired locks. Getting this
wrong permanently disables an account.

A status generated at the relay hop is not the provider's. A `520`–`527` from the
Vercel relay is Cloudflare's edge error for the worker, so `is_relay_edge_error`
returns it as-is and skips `mark_account_unavailable` — the credential is healthy
and must not be cooled or rotated out. A provider's own 4xx/5xx, `429` included,
still cools. The relay worker asks the origin for `accept-encoding: identity` and
strips `content-encoding`, `content-length`, `transfer-encoding` and `connection`
before rebuilding the response, so the body and its framing agree and the edge
does not answer 520 for a request the provider never saw.

## Quota reads

`parse_reset_time` in `crates/router-sse/src/services/usage.rs` normalises the
reset stamps every provider sends. A falsy value yields nothing; a number or
all-digit string below `1e12` is read as seconds and at or above as milliseconds;
anything else is parsed as RFC-3339 first, then as a bare `YYYY-MM-DD` at UTC
midnight. The two zeroes diverge on purpose: numeric `0` is missing, the string
`"0"` is the epoch.

## Combo state

`handle_combo_chat` and `handle_fusion_chat` keep rotation state in a
process-global map (`COMBO_ROTATION_STATE`) keyed by combo name. Not persisted,
lost on restart — acceptable, but note it.

`detect_required_capabilities` + `augment_models_with_capacity_adapter` +
`strip_for_adapter_model` add a capability-bearing model to the candidate list and
strip the adapter-only fields on the way out.

## Open decisions

- `flow_type` is hand-maintained while provider config is registry-driven. Whether
  to generate the flow table from `registry.json` is open.
- Combo and fusion rotation state is in-process. Whether it should survive a
  restart is open.

## Auto quota tracker

The tracker in `crates/router-db/src/repos/quota_tracker.rs` reuses the
account-selection machinery: a connection whose quota is exhausted is disabled the
same way a manual bulk "Turn off Empty" would, and re-enabled the same way a
"Turn on Available" would. It is off unless `settings.quotaAutoTrackerEnabled` is
true. See `RUNTIME.md` for the rules.
