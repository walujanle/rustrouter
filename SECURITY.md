# Security

`rustrouter` is a local AI routing gateway. It binds `0.0.0.0:20129` by
default, serves an OpenAI-compatible `/v1` API plus a dashboard `/api/*`
surface, and shares one SQLite file with a 9router install. That shape is the
threat model: the box is normally a developer machine or a small always-on
host, and the attacker worth designing against is someone else on the network,
a web page the operator visits, or a process running as the same user.

The gateway holds real credentials: upstream API keys, OAuth access and refresh
tokens, and the dashboard password hash. It can also be told to fetch URLs. Both
facts drive everything below.

## Reporting a vulnerability

Report privately through GitHub Security Advisories:
<https://github.com/walujanle/rustrouter/security/advisories/new>. Do not open a
public issue for anything exploitable.

Include the affected version (`PROJECT_VERSION` or the release tag), the
component and file, a request or command that reproduces it, and what an
attacker gains. Say which configuration the issue needs, since most of the
accepted risks below only apply with a non-loopback bind, `requireLogin: false`,
a plain-HTTP deployment, or an empty `jwt-secret` file. Say too whether the
attacker must be local, on the LAN, or already authenticated. Never paste a live
key, token, or password hash into a report; use placeholders.

This is a single-maintainer project. Expect an acknowledgement in days, not
hours, and coordinate any disclosure timeline in the advisory thread.

## Supported versions

Only the latest release is supported. rustrouter is pre-1.0, releases are
frequent, and there are no backports to older lines: a fix lands in the next
release, and the upgrade path is `npm install -g rustrouter@latest` (or
replacing the binary). Security fixes are called out in `CHANGELOG.md` under
`Security`.

## Threat model

**In scope.** A remote caller on the same network reaching the default
`0.0.0.0` bind. A web page the operator visits driving the loopback gateway
through the browser. A local unprivileged process reading `DATA_DIR`. An
attacker who can point a configured URL at an internal address and read the
result back.

**Out of scope.** An attacker who already controls the machine as the operator
(the credentials are readable by design). Compromise of crates.io or the npm
registry. Upstream provider terms-of-service or account-ban risk from routing
OAuth sessions through the gateway, which is a matter between the operator and
the provider. The operator deliberately binding `0.0.0.0`, disabling login, or
running without TLS behind a proxy they did not set up.

### Trust boundaries

| Boundary | What crosses | Enforced by |
|---|---|---|
| Socket to guard | Every HTTP request | `crates/router-server/src/middleware.rs`, `auth/guard.rs` |
| Browser to dashboard API | Session cookie JWT, local CLI token | `auth/session.rs`, `auth/jwt.rs` |
| Gateway to upstream providers | Stored OAuth tokens and API keys | `router-sse/src/executors/` |
| Caller-supplied URL to the network | web-fetch, web-search, provider probes, proxy test | `router-sse/src/modalities/ssrf.rs`, where it is called |
| Process to `DATA_DIR` | The database and the secret files | OS file permissions only |

`DATA_DIR` is `%APPDATA%\9router` on Windows and `~/.9router` elsewhere,
overridable with the `DATA_DIR` environment variable
(`crates/router-db/src/paths.rs`).

## What the gateway does to defend itself

- **Deny-by-default dashboard.** The auth middleware runs on every request and
  protects anything under `/api/*` unless it appears in a public table. A new
  route is protected by default; opting out is an explicit edit to
  `PUBLIC_API_PATHS`, `PUBLIC_PREFIXES`, `ALWAYS_PROTECTED`, or
  `LOCAL_ONLY_PATHS` in `crates/router-server/src/auth/guard.rs`.
- **Localness is decided from the socket, not headers.** A request is "local"
  only when the TCP peer is loopback, no forwarding header is present, and any
  `Origin` is a loopback origin (`RequestFacts::is_local`). `x-forwarded-for`
  and `x-real-ip` are believed only from a loopback peer, so a remote client
  cannot claim to be local.
- **Two credentials for two surfaces.** The `/v1*` API takes an API key stored
  in the `apiKeys` table; the dashboard takes a session cookie or the local CLI
  token. The CLI token is derived per machine from the machine id and a
  `cli-secret` file, and is compared in constant time (`state.rs`).
- **Sessions are HS256 JWTs.** `auth/jwt.rs` pins `alg` to HS256 before it
  touches the key, so `alg: none` and algorithm substitution are rejected. The
  cookie is `HttpOnly` and `SameSite=Lax`, with a 24-hour lifetime.
- **Dashboard login is bcrypt (cost 10) with a progressive lockout.** Five
  failures lock the source IP, and the lock escalates 30s, 2m, 10m, 30m
  (`auth/login_limiter.rs`). While no password hash is stored and
  `INITIAL_PASSWORD` is unset, a non-loopback caller cannot obtain a session at
  all (`routes/auth.rs`).
- **A three-layer SSRF guard** for outbound URLs the caller influences:
  literal host and IP checks, DNS resolution, and manual redirect handling that
  re-validates each hop (`router-sse/src/modalities/ssrf.rs`).
- **Credential redaction on read.** Provider list and detail responses remove
  `apiKey`, `accessToken`, `refreshToken`, and `idToken` rather than nulling
  them, and the client-facing provider route uses an allow-list of safe fields
  (`routes/providers.rs`).
- **Audited dependencies.** `cargo audit` and `npm audit --audit-level=high`
  run in CI on every push and pull request.

## Accepted risks and known limitations

These are real, they are deliberate or unfixed, and an operator should know
them before exposing the port. Ranked roughly by what an attacker gets.

### `requireLogin: false` removes authentication for remote callers

`LiveGuard::is_authenticated` returns true for every caller when the stored
setting `requireLogin` is false (`crates/router-server/src/middleware.rs`). That
shortcut exists so the dashboard works without login, but it applies to remote
peers too: with `requireLogin: false`, any host that can reach the port can
read `/api/keys`, `/api/providers`, `/api/oauth`, and the rest of `/api/*`
without a credential. The four `ALWAYS_PROTECTED` routes are the exception:
they require a real CLI token or session JWT regardless of `requireLogin`, so
the database export is not reachable this way. `requireLogin` defaults to true,
and this is the single most important reason not to disable it on a `0.0.0.0`
bind.

### `PATCH /api/settings` can disable authentication

The same route that writes `requireLogin` is gated only by an ordinary session
(`crates/router-server/src/app.rs`). One authenticated session can set
`requireLogin: false` and persist it to the shared database, after which the
dashboard is open. There is no re-authentication prompt for that change.

### A bogus `x-9r-cli-token` header passes the database export gate

`/api/settings/database` sits in `ALWAYS_PROTECTED`, so the guard validates the
CLI token before the handler runs. The handler's own `is_cli_request` check is
presence-only, testing that the header exists rather than that its value is
valid (`crates/router-server/src/routes/settings.rs`). That is defence in depth
inverted: if the route ever leaves `ALWAYS_PROTECTED`, a header of any value is
enough. The guard's check is what holds today.

### An empty `jwt-secret` file was a forgeable session key

`jwt_secret` used to return the file's trimmed contents without checking that
they are non-empty (`crates/router-db/src/identity.rs`). An empty or whitespace
file, through a truncated write or a bad restore, made the HS256 key the empty
string and let anyone mint an `auth_token` cookie that passed verification. It
now regenerates the file, like the machine-id loader. Set `JWT_SECRET`
explicitly anyway.

### The database and its secrets are not encrypted, and are mode-restricted only on Unix

Every upstream token and API key is stored in plaintext in `data.sqlite`
(`providerConnections.data`, `apiKeys.key`), and there is no encryption at
rest. `write_secret_file` applies `0600` only on Unix and only after the write;
the database file itself and `ensure_dirs` get the default umask, so on a shared
Unix host `data.sqlite` can land world-readable. Anyone who can read the file
owns every credential in it, and the safety backups in `db/backups/` duplicate
them. Keep `DATA_DIR` on a private path.

### No TLS, and the `Secure` cookie flag is opt-in

The server speaks plain HTTP; there is no TLS listener
(`crates/router-server/src/app.rs`). A dashboard login from another machine
sends the password and the session cookie in the clear, and a network observer
can replay the cookie. The `Secure` attribute is set only when
`AUTH_COOKIE_SECURE=true` or the request carries `x-forwarded-proto: https`,
and that header is read without checking the peer
(`crates/router-server/src/auth/session.rs`, `routes/auth.rs`). The supported
remote deployment is a TLS-terminating reverse proxy; set
`AUTH_COOKIE_SECURE=true` alongside it.

### Some outbound requests bypass the SSRF guard

The guard is only as good as its call sites. These make outbound requests
without it and remain the current SSRF surface
(`crates/router-server/src/routes/`):

- `providers::validate` probes a stored `baseUrl` with no check, for the
  OpenAI-compatible, custom-embedding and Anthropic-compatible node kinds.
- `proxy_pools::test` fetches a stored relay `proxyUrl` unchecked.

A caller who can configure one of those URLs can point the server at an
internal address and read the response back through the probe's result.
`providers::suggested_models`, `provider_nodes::validate` and
`settings::proxy_test` are now guarded. The guard's own remaining gap:
`fetch_public` validates and then lets reqwest resolve the name a second time,
so a short-TTL rebinding host can slip through; a resolution failure now denies
rather than allowing the request.

### A hardcoded default password, and a non-constant-time fallback compare

A fresh install with no stored hash and no `INITIAL_PASSWORD` accepts the
literal `123456` (`crates/router-server/src/auth/session.rs`). The remote-login
block stops a network attacker from using it, but a local process can log in and
receive a session that reads every credential. The literal compare used while no
hash is stored is not constant-time. Set `INITIAL_PASSWORD`, then change the
password.

### The default bind is `0.0.0.0` and CORS is permissive

`resolve_host` defaults to `0.0.0.0` and `CorsLayer::permissive()` reflects any
origin (`crates/router-server/src/state.rs`, `app.rs`). A response from the
loopback service is readable by any page the user visits, and the dashboard's
unauthenticated `/api/*` reads are reachable from the network. Bind loopback
(`-H 127.0.0.1`) unless a proxy is intended, and scope CORS to the dashboard
origin.

### Local file writes from request data

The CLI-tool writers mutate the operator's real dotfiles and are gated
`LOCAL_ONLY_PATHS`, but the values they interpolate are not all escaped
(`crates/router-server/src/routes/cli_tools.rs`): the Hermes YAML scalars are
hand-formatted, so a model or base URL containing a quote and a newline injects
YAML into `~/.hermes/config.yaml`; the `OPENAI_API_KEY` line is written with
newlines intact, so it can add further variables to `~/.hermes/.env`; and the
auxiliary-role regex is compiled from unescaped input and unwrapped. The Claude
writer goes through `serde_json` and the Codex writer through `toml_edit`, so
both are safe from this class.

### Supply chain

Dependencies come from crates.io with a committed `Cargo.lock`, and the npm
package is published with provenance. Two gaps: the publish job installs
`npm@latest` at run time while holding an OIDC token, and the release workflow
does not run the dependency audits that CI does, so a tagged artifact is not
tied to the CI vulnerability gate. The Termux postinstall verifies its download
against a hash fetched from the same response, which is a consistency check,
not a trust check.

### Smaller items

- Tokens without an `exp` claim are accepted and never expire, and logout or a
  password reset does not revoke an issued token (`auth/jwt.rs`).
- `/api/auth/status` reports `hasPassword` and `authenticated` to
  unauthenticated callers (`routes/auth.rs`).
- The dashboard loads Monaco from `cdn.jsdelivr.net` at runtime
  (`web/src/views/TranslatorPage.vue`), so a compromised CDN runs script in the
  authenticated origin. Vendor it, or pin it with an SRI hash and a CSP.
- The OAuth callback `postMessage` handler now checks the exact dashboard origin
  or `http://localhost:1455` rather than any string containing `localhost`
  (`web/src/components/OAuthModal.vue`).
- The `?key=` API-key query parameter leaks keys into logs and `Referer`
  headers (`auth/guard.rs`).
- `kill_port_holder` force-kills whatever process holds the port
  (`crates/rustrouter/src/launcher.rs`).

## Privacy: what is stored, logged, and sent

**Stored.** The shared SQLite file holds settings (including the bcrypt
password hash), provider connections with their OAuth tokens and API keys, the
gateway's own API keys, combos, and usage records. Request and response bodies
are **not** persisted: the `requestDetails` table exists for schema parity and
nothing writes to it. Usage rows do store the caller's API key verbatim in
`usageHistory.apiKey` and in the daily aggregate; it is masked only when read
(`crates/router-db/src/repos/usage.rs`), so a key read from the database file
is usable directly.

**Logged.** The console-log ring buffer captures every `info`-level event
verbatim into a dashboard-readable buffer (`router-server/src/services/console_log.rs`).
Request and response bodies are only rendered into log events at `debug` level,
which the shipped filter does not enable. `GET /api/settings/database` exports
the whole database as JSON: connections and their tokens, API keys, and the
password hash. Treat a copy of that response as a copy of every credential.

**Sent on its own.** The process makes exactly three outbound calls without a
request driving them, all to fixed hosts: the update check against
`api.github.com`, the model-catalog sync against `https://models.dev/api.json`,
and OAuth token exchange and refresh against each provider's committed
`tokenUrl`. There is no telemetry and no analytics. Everything else outbound is
a response to a request or a chat the operator started.

## Deployment checklist

1. Set `JWT_SECRET` to at least 32 random bytes, and never leave `jwt-secret`
   empty.
2. Set `INITIAL_PASSWORD`, then change the dashboard password.
3. Set `API_KEY_SECRET` to a random value.
4. Terminate TLS in front of the gateway and set `AUTH_COOKIE_SECURE=true`.
5. Keep `DATA_DIR` private: `0700` the directory, `0600` the database and its
   `-wal`/`-shm` siblings and the backups, on Unix. On Windows, keep it on a
   path only the operator's account can read.
6. Bind loopback (`-H 127.0.0.1`) unless a reverse proxy is intended.
7. Leave `requireLogin` and `requireApiKey` on, and keep the dashboard password
   non-default.

## Verifying a release

`cargo audit` and `npm audit --audit-level=high` run in CI. To check a download
before running it, compare the running executable's SHA-256 against the digest
the release API reports for your platform's asset. The update check does exactly
this and reports `binaryChanged` on `GET /api/version`.
