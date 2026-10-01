# Database Contract

The schema is declared in `crates/router-db/src/schema.rs`. `SCHEMA_VERSION = 1`. The SQLite file is shared with 9router and the schema is byte-identical: same tables, column order, types, defaults, indexes and PRAGMA set. SQLite only.

That identity is the contract. A database can move between the two installs with no migration step, so a row rustrouter does not recognise is left untouched, never rewritten or deleted. `crates/router-db` is the module that owns every rule on this page.

## PRAGMA set

Applied on open, in this order, from `PRAGMA_SQL`:

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA temp_store = MEMORY;
PRAGMA mmap_size = 30000000;
PRAGMA cache_size = -64000;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
```

`busy_timeout = 5000` is not optional. Without it SQLite returns `SQLITE_BUSY` immediately when the other process holds the write lock.

### Memory overlay (rustrouter-local, not part of the contract)

`PRAGMA_SQL` above stays byte-identical to 9router. On top of it every connection also runs `MEMORY_PRAGMA_SQL`:

```sql
PRAGMA cache_size = -2000;
```

This is a connection-local setting: it is never written to the file header, so a connection 9router opens is unaffected and the shared file stays byte-identical. The pool holds up to 4 connections and each one keeps its own page cache, so the 9router default of `-64000` (about 62 MB per connection) is the dominant memory term under load. Reads are served from the `mmap_size` mapping, which is file-backed and reclaimable, so the smaller heap cache costs little. `PRAGMA_SQL` remains the frozen text; only this overlay may differ.

### Checkpoint ticker and idle page-cache release

`Db::spawn_checkpoint_task` runs `PRAGMA wal_checkpoint(TRUNCATE)` every `CHECKPOINT_INTERVAL` (60 s). The first tick of a `tokio::time::interval` fires immediately and is skipped, so startup does not block on a checkpoint nobody asked for. After each checkpoint the same task calls `shrink_idle`: when every pooled connection is idle (`open == idle.len()`), all but one are dropped and the survivor runs `PRAGMA shrink_memory`. The call is a no-op while any connection is checked out, so it can never shrink the pool under a live request. `shrink_memory` frees the connection's heap cache into the process C heap and does not change `cache_size`; handing those pages back to the OS is `router_server::reclaim`'s job, not this call's.

## The 11 tables

Column order and type strings are declared in `TABLES` in `schema.rs`. `TableDef::create_sql` joins `"{name} {type}"` with `", "` and appends `primary_key` last.

```sql
CREATE TABLE IF NOT EXISTS _meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
)

CREATE TABLE IF NOT EXISTS settings (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  data TEXT NOT NULL
)

CREATE TABLE IF NOT EXISTS providerConnections (
  id TEXT PRIMARY KEY,
  provider TEXT NOT NULL,
  authType TEXT NOT NULL,
  name TEXT,
  email TEXT,
  priority INTEGER,
  isActive INTEGER DEFAULT 1,
  data TEXT NOT NULL,
  createdAt TEXT NOT NULL,
  updatedAt TEXT NOT NULL
)
CREATE INDEX IF NOT EXISTS idx_pc_provider ON providerConnections(provider)
CREATE INDEX IF NOT EXISTS idx_pc_provider_active ON providerConnections(provider, isActive)
CREATE INDEX IF NOT EXISTS idx_pc_priority ON providerConnections(provider, priority)

CREATE TABLE IF NOT EXISTS providerNodes (
  id TEXT PRIMARY KEY,
  type TEXT,
  name TEXT,
  data TEXT NOT NULL,
  createdAt TEXT NOT NULL,
  updatedAt TEXT NOT NULL
)
CREATE INDEX IF NOT EXISTS idx_pn_type ON providerNodes(type)

CREATE TABLE IF NOT EXISTS proxyPools (
  id TEXT PRIMARY KEY,
  isActive INTEGER DEFAULT 1,
  testStatus TEXT,
  data TEXT NOT NULL,
  createdAt TEXT NOT NULL,
  updatedAt TEXT NOT NULL
)
CREATE INDEX IF NOT EXISTS idx_pp_active ON proxyPools(isActive)
CREATE INDEX IF NOT EXISTS idx_pp_status ON proxyPools(testStatus)

CREATE TABLE IF NOT EXISTS apiKeys (
  id TEXT PRIMARY KEY,
  key TEXT UNIQUE NOT NULL,
  name TEXT,
  machineId TEXT,
  isActive INTEGER DEFAULT 1,
  createdAt TEXT NOT NULL
)
CREATE INDEX IF NOT EXISTS idx_ak_key ON apiKeys(key)

CREATE TABLE IF NOT EXISTS combos (
  id TEXT PRIMARY KEY,
  name TEXT UNIQUE NOT NULL,
  kind TEXT,
  models TEXT NOT NULL,
  createdAt TEXT NOT NULL,
  updatedAt TEXT NOT NULL
)
CREATE INDEX IF NOT EXISTS idx_combo_name ON combos(name)

CREATE TABLE IF NOT EXISTS kv (
  scope TEXT NOT NULL,
  key TEXT NOT NULL,
  value TEXT NOT NULL,
  PRIMARY KEY (scope, key)
)
CREATE INDEX IF NOT EXISTS idx_kv_scope ON kv(scope)

CREATE TABLE IF NOT EXISTS usageHistory (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  timestamp TEXT NOT NULL,
  provider TEXT,
  model TEXT,
  connectionId TEXT,
  apiKey TEXT,
  endpoint TEXT,
  promptTokens INTEGER DEFAULT 0,
  completionTokens INTEGER DEFAULT 0,
  cost REAL DEFAULT 0,
  status TEXT,
  tokens TEXT,
  meta TEXT
)
CREATE INDEX IF NOT EXISTS idx_uh_ts ON usageHistory(timestamp DESC)
CREATE INDEX IF NOT EXISTS idx_uh_provider ON usageHistory(provider)
CREATE INDEX IF NOT EXISTS idx_uh_model ON usageHistory(model)
CREATE INDEX IF NOT EXISTS idx_uh_conn ON usageHistory(connectionId)

CREATE TABLE IF NOT EXISTS usageDaily (
  dateKey TEXT PRIMARY KEY,
  data TEXT NOT NULL
)

CREATE TABLE IF NOT EXISTS requestDetails (
  id TEXT PRIMARY KEY,
  timestamp TEXT NOT NULL,
  provider TEXT,
  model TEXT,
  connectionId TEXT,
  status TEXT,
  data TEXT NOT NULL
)
CREATE INDEX IF NOT EXISTS idx_rd_ts ON requestDetails(timestamp DESC)
CREATE INDEX IF NOT EXISTS idx_rd_provider ON requestDetails(provider)
CREATE INDEX IF NOT EXISTS idx_rd_model ON requestDetails(model)
CREATE INDEX IF NOT EXISTS idx_rd_conn ON requestDetails(connectionId)
```

## Traps

**`kv` keeps its composite primary key.** `INSERT ... ON CONFLICT(scope, key) DO UPDATE` requires that exact PK or every write throws. Do not "simplify" it to a single-column key.

**`settings` is a singleton.** `CHECK (id = 1)`. Writing `id != 1` violates the constraint.

**`isActive` is INTEGER 0/1, not bool.** A row is active when the column is 1. `row_to_conn` emits the JSON boolean; store `i64` 0/1.

**`priority` is nullable.** `conn_to_row` writes NULL when the column is NULL, and sorting treats 0 and NULL as 999, so both sort last.

**JSON-in-TEXT is `JSON.stringify(v ?? null)`.** `undefined` becomes literal `null` and the key stays. In Rust:

- Enable `serde_json`'s `preserve_order` feature (IndexMap). Without it the `Map` is a BTreeMap and keys come out sorted, which changes the TEXT bytes for the same logical value.
- Do **not** annotate `skip_serializing_if = "Option::is_none"`. Keys are never skipped; a struct that skips one drops a key the other app keeps.
- **Whole numbers are written without a decimal point.** `serde_json` renders an `f64` whole value as `35.0`, where JS `JSON.stringify(35)` writes `35`. Any float that reaches a byte-compared payload (quota bars, thinking budgets, percentages, embedding dimensions) goes through `js_json_number` (`translator/concerns/primitives.rs`) so the bytes match.

**Timestamps are UTC, millisecond precision, trailing `Z`:** `2026-09-26T12:34:56.789Z`. Use `to_rfc3339_opts(SecondsFormat::Millis, true)`; `chrono::Utc::now()` Display omits milliseconds and uses `+00:00`. All date comparisons in the usage repo are lexicographic string compares on this format, so it must be exact.

**`usageDaily.dateKey` is LOCAL time.** `local_date_key` uses the local calendar, not UTC, while `usageHistory.timestamp` is UTC. Compute the key with `chrono::Local` or the day buckets shift and the two apps disagree on which day a request belongs to.

"Today" boundaries resolve through `crate::time::local_midnight()`, which returns the first valid local instant of the day. `chrono`'s `from_local_datetime(...).single()` is `None` for the hour a DST spring-forward skips, and a `None` fallback of `now` silently collapses "today" to the seconds since the gap — every period query then reads almost nothing. Resolve a gap to the first valid instant, not to now.

## Repository semantics

**`save_request_usage` is three writes in one transaction** (`repos/usage.rs`):

1. Dedup `SELECT` with `COALESCE(provider,'') = COALESCE(?,'')` (and model, connectionId, apiKey, endpoint) `AND promptTokens = ? AND completionTokens = ? ORDER BY id DESC LIMIT 1`. `COALESCE` with a NULL parameter binds to `''`, so NULL and `''` compare equal.
2. `INSERT INTO usageHistory` (12 columns).
3. `usageDaily` upsert `ON CONFLICT(dateKey) DO UPDATE SET data = excluded.data`, plus an atomic `totalRequestsLifetime` increment.

`aggregate_entry_to_day` builds `byProvider` / `byModel` (`${model}|${provider}`) / `byAccount` / `byApiKey` (`${apiKeyVal}|${model}|${provider}`) / `byEndpoint` counters, each carrying `requests`, `promptTokens`, `completionTokens`, `cachedTokens`, `cost` and a `meta` object.

**Token fields chain with `||`, not `??`.** `token_field` walks `prompt_tokens`/`input_tokens`, `completion_tokens`/`output_tokens` and `cached_tokens`/`cache_read_input_tokens`; a present-but-zero first key falls through to the next, and the first non-zero value wins.

**Counter keys.** `byProvider` is the provider id. `byModel` is `${model}|${provider}`, or the bare model when the row has no provider. `byAccount` is the connection id. `byApiKey` is `${apiKey}|${model}|${provider}` with the sentinel `local-no-key` when the row carries no key. `byEndpoint` is `${endpoint}|${model}|${provider}` with `Unknown` for a missing endpoint. A missing provider is written as the literal `unknown` in the `byApiKey` and `byEndpoint` keys.

**`row_to_conn` / `conn_to_row`** (`repos/connections.rs`). `row_to_conn` spreads the parsed `data` blob then overrides `id`, `provider`, `authType`, `name`, `email`, `priority`, `isActive`, `createdAt`, `updatedAt`. `conn_to_row` strips those back and stringifies the rest. `upsert` is `INSERT ... ON CONFLICT(id) DO UPDATE`. `reorder_in_tx` reassigns priority 1..n by `(priority, updatedAt DESC)`.

**Create-time dedup is provider-specific.** Codex matches only when **both** rows share `chatgptAccountId`. Other OAuth rows match email+username with a tri-state rule (a one-sided username means distinct). Collapsing this to email-only overwrites a second account's token pair.

**`reset_health_state_on_activation`** clears `modelLock_*` keys. `modelLock_<model>` keys live as dynamic entries in `providerConnections.data`, and the model name may contain `/` and `:`. A Rust struct with fixed fields cannot represent this; `provider_specific_data` must be a free-form map.

**`update_settings` is read-merge-write.** `DEFAULT_SETTINGS` is the full key set of the shared blob, in declaration order; that order is the key order of the stored JSON, so it is part of the contract. rustrouter's own keys (`quotaAutoTrackerEnabled`, `autoUpdateCheck`) are appended after the shared set so every shared key keeps its position. `merge_with_defaults` overlays the stored row on the defaults and rewrites the `oc/mimo-v2.5-free` capacity-adapter entry to `oc/mimo-v2.6-flash-free`; `oc` is the `opencode` alias, so that rewrite and the capacity adapter's `DEFAULT_FALLBACK_MODEL` (`oc/mimo-v2.6-flash-free`) agree. Keep every key in the list even when only the other app reads it: the blob is written whole, and a pruned key changes the bytes the other side reads. Unknown keys from a stored row survive a round trip.

**`requestDetails.id` shape:** `${ISO timestamp}-${6 base36 chars}-${model with non-alphanumerics replaced by '-'}`. Nothing in this build writes the table; `get_distinct_providers` reads its `provider` column for `GET /api/usage/providers`, and the table and its indexes stay because the schema is shared.

### Auto quota tracker (rustrouter-only)

`repos/quota_tracker.rs` layers a rustrouter behaviour on the shared rows, gated by `settings.quotaAutoTrackerEnabled` (default off) and run from the auto-ping loop in `services/schedulers.rs` on the same usage read. A connection with any exhausted quota window is set `isActive: false` and marked with `quotaAutoDisabled` and `quotaAutoDisabledAt` in its `data` blob; once a reading is available again the tracker clears both keys and re-activates the row. A window is exhausted when `remaining <= 0`, or when `remaining` is absent and `used >= total` with a positive `total`; `unlimited: true` never exhausts, and a numeric string reads as a number. Two rules keep it from fighting the user: only a connection carrying the marker is re-enabled (a user-disabled row has none and is left alone), and a connection with no quota reading is never disabled. Both marker keys live in the free-form connection blob, so 9router preserves them across a round trip.

**`_meta` keys.** Five: `schemaVersion`, `backupSchemaVersion`, `totalRequestsLifetime`, `appVersion`, `migratedAt`. rustrouter writes the first three. Migration is `run_versioned_migrations` (bootstrap `_meta`, read `schemaVersion`, apply pending in a transaction, stamp) plus `sync_schema_from_tables` (additive: `CREATE TABLE IF NOT EXISTS`, `PRAGMA table_info` diff, `ALTER TABLE ADD COLUMN` with PRIMARY KEY/UNIQUE **stripped**, idempotent index creation).

**There is no JSON import step.** The data lives in `DATA_DIR/db/data.sqlite`. `Paths` declares the legacy JSON paths and the `db/.migrated-from-json` marker for layout compatibility, and nothing reads or writes them.

**KV scopes in use:** `modelAliases`, `customModels`, `mitmAlias`, `pricing`, `disabledModels`. `kv_store::get_all` relies on rowid order, which is insertion order, and has no `ORDER BY`; callers serialise the result, so sorting here changes response bytes.

**KV key shapes.** `modelAliases` stores the model value as JSON under the alias. `customModels` keys are `${providerAlias}|${id}|${type}` and the value is the whole model object. `disabledModels` holds one row per provider alias whose value is the JSON array of disabled model ids. `mitmAlias` has no accessor: rows written by 9router stay readable, but this build never writes the scope.

**`DEFAULT_MITM_ROUTER_BASE` stays `http://localhost:20128`.** The value is the stored default of `settings.mitmRouterBaseUrl`, part of the shared blob, not a bind address; the server listens on 20129 and the two are unrelated. Rewriting it to 20129 changes the bytes 9router reads.

An empty or whitespace-only `oidcClientSecret` in a settings patch is removed before the write instead of stored, so a blank field in the dashboard cannot overwrite a configured secret with `""`.

## Paths

`DATA_DIR` = env `DATA_DIR`, else Windows `%APPDATA%/9router`, else `~/.9router`. Windows ignores Unix-style `/...` paths. `DB_DIR = DATA_DIR/db`, `DATA_FILE = DATA_DIR/db/data.sqlite`, plus `BACKUPS_DIR`.

Files rustrouter also writes under `DATA_DIR`: `jwt-secret` (generated if absent), `auth/cli-secret`, `machine-id`, `model-catalog.json` (and `model-catalog-raw.json`).

**`jwt-secret` resolution.** `JWT_SECRET` wins when it is set and non-empty; otherwise the file is read and trimmed, and the result is used even when it is empty. The file is not regenerated over a blank one, so an empty or whitespace-only `jwt-secret` yields an empty HMAC key and a forgeable session. Only an absent file is created, as 32 random bytes in hex with mode 0600.

## Transactions

Use `BEGIN IMMEDIATE` for read-modify-write paths (`update_settings`, connection upsert, `save_request_usage`, `disable_models`, `update_pricing`). `rusqlite::Transaction` defaults to `BEGIN DEFERRED`; a deferred transaction that reads then writes must upgrade its lock and can deadlock against another writer before `busy_timeout` helps. `BEGIN IMMEDIATE` takes the write lock up front so `busy_timeout` can retry cleanly.

**Writes retry `SQLITE_BUSY` with linear backoff.** `Db::write` runs its closure inside `retry_busy`: up to `BUSY_RETRIES = 6` attempts, sleeping `BUSY_BACKOFF_MS * attempt` (25 ms, then 50, 75, …) between them. The retry exists for `SQLITE_BUSY_SNAPSHOT`, which `busy_timeout` does not cover: a writer whose read snapshot went stale must restart the whole transaction rather than wait. The closure is `FnMut`, because a retried attempt runs it again, so it must not consume captured state and each attempt must be a single transaction that a rollback leaves clean.

**A pooled connection is checked in from a guard's `Drop`, never after the closure.** `Db::with_conn` and `Db::write` wrap the checked-out connection in a `ConnGuard` whose `Drop` returns it to the pool on every path, including a panic that unwinds out of the closure. Checking in after the call instead leaks the slot: the connection drops while the pool's `open` count still holds it, and after `DEFAULT_POOL_SIZE` such leaks `checkout` blocks on a `Condvar` that nothing will ever signal. `checkout` waits at most `CHECKOUT_TIMEOUT` (30 s) and then returns `DbError::PoolClosed`, so a leaked slot or a wedged database fails one request instead of pinning a task forever. A 30 s wait is already pathological — reaching it means the pool or the file is broken, not that the request was unlucky.

## Machine ID and API keys

`consistent_machine_id(salt)` = `sha256(raw + saltValue + extra).hex[..16]`, where `extra` is the `auth/cli-secret` contents **only when** the resolved salt is `9r-cli-auth`. The same SQLite file carries the same derived machine id on both sides.

API key format: `sk-${machineId}-${keyId}-${crc8}`, where `crc8 = HMAC-SHA256(API_KEY_SECRET, machineId + keyId).hex[..8]`. `parse_api_key` accepts 4 parts (new format, CRC validated) or 2 parts (legacy `sk-{random8}`). Defaults: `API_KEY_SECRET = "endpoint-proxy-api-key-secret"`, `MACHINE_ID_SALT = "endpoint-proxy-salt"`.

## Backup

`backup_db_lite` uses `ATTACH DATABASE`, recreates tables with `CREATE TABLE bak.<name>` and copies with `INSERT INTO bak.x SELECT * FROM main.x`. `requestDetails` is excluded; `KEEP_BACKUPS = 3` prunes. Reproduce both or backups balloon. The pre-change backup triggers when the stored `_meta.backupSchemaVersion` is lower than the code's `SCHEMA_VERSION`, and only when the DB is not fresh. The `requestDetails` exclusion stays: the table is shared and its rows can be large.

The boot sequence is `migrate_on_boot` in `crates/router-db/src/migrations.rs`: capture freshness, prune, bootstrap `_meta`, take the pre-change backup (best-effort, a failure logs and continues), run the versioned chain, sync additive columns and indexes, then stamp `backupSchemaVersion`.

## Export / import

`export_db` / `import_db` (`crates/router-db/src/export.rs`) wipes tables except `_meta` and re-inserts. Route-level: password re-auth (`x-9r-password`) or CLI token (`x-9r-cli-token`) on `GET`/`POST /api/settings/database`. `PROTECTED_SETTING_KEYS = ["password", "mitmSudoEncrypted"]`; `GET /api/settings` strips `password` and `oidcClientSecret` and adds `oidcConfigured`, `hasPassword`.

**What `import_db` actually clears.** The wipe is six tables — `settings`, `providerConnections`, `providerNodes`, `proxyPools`, `apiKeys`, `combos` — plus `kv` rows in the `modelAliases`, `customModels`, `mitmAlias` and `pricing` scopes. `usageHistory`, `usageDaily`, `requestDetails` and the `disabledModels` scope are not touched, and `export_db` never emits them, so the payload carries configuration only and an import replaces configuration while leaving usage and disabled-model state in place. Only a literal JSON `false` in `isActive` deactivates a row on import; `null`, a missing key, `0` and `""` all import as active. `combos.models` goes through `JSON.stringify(models || [])`, so any falsy value becomes `[]`. `export_db` spreads each row's `data` blob first and then overlays the fixed columns, so a column always wins over a same-named key in the blob and the payload's key order is the order the dashboard sees.

## Driver chain

The 9router side picks its SQLite driver at startup, in order: `bun:sqlite` → `better-sqlite3` → `node:sqlite` → `sql.js`.

- `bun:sqlite` runs only under Bun.
- `better-sqlite3` is skipped on Node ≥ 24; on Node 22 it attempts a dynamic import and falls through when the optional dependency is absent.
- `node:sqlite` needs Node 22.5 or newer.
- `sql.js` is reached only when `node:sqlite` fails to import.

So on Windows, Node 22+ and no native dependencies, the driver is `node:sqlite`. On a Node minor where `node:sqlite` still needs `--experimental-sqlite`, the import throws, the error is swallowed, and the process silently lands on `sql.js`. That depends on the exact Node patch version, not on anything in the repo. **Verify the target Node before relying on a native peer.**

## Shared-file concurrency verdict

rustrouter listens on port 20129 and 9router uses 20128, so both can run at once. That is **unsafe with a `sql.js` peer** and **safe with conditions against a native peer** (`better-sqlite3`, `node:sqlite`, `bun:sqlite`).

### Why sql.js disqualifies

The sql.js adapter slurps the file into memory once, then every write schedules a 100 ms debounced full-file overwrite: it exports the in-memory database and writes the whole buffer over `data.sqlite`, with no temp file and no rename. `run`, `exec` and `transaction` all schedule a save. Consequences:

- **Silent total clobber.** sql.js holds a boot-time snapshot. Any rustrouter write since the process started is overwritten wholesale on the next flush. No error, no `SQLITE_BUSY`; the write simply replaces the bytes.
- **WAL is invisible to sql.js.** It reads only `data.sqlite`, never the `-wal`/`-shm` sidecars. If a native driver committed to WAL without checkpointing, sql.js loads stale data, clobbers the file, and leaves a stale `-wal` whose salt no longer matches; the next native open discards it.
- **Windows sharing violation.** `close()` calls `persist()` unguarded, unlike the debounced path which catches. It throws when rustrouter holds the file open.
- **`transaction()` is `SAVEPOINT`/`RELEASE`** on the in-memory database, atomic in memory only; the durable write is the non-atomic file replace.

### Conditions for co-running with a native peer

1. **Refuse to co-run if the peer is sql.js.** There is no machine-readable driver marker. The best available signal: sql.js cannot set WAL on disk and leaves no `-wal`/`-shm` sidecars, so if the on-disk `journal_mode` is not `wal`, or the sidecars are absent while the DB is known to be in use, assume a sql.js peer and abort with a clear message. This is a heuristic, not proof.
2. **Do not let the 9router side fall back to sql.js.** Guarantee `better-sqlite3` or `node:sqlite`. If neither is guaranteed, abandon the simultaneous-operation plan.
3. **Set the same PRAGMAs on every connection.** The 9router adapters apply the set once per connection at construction. `journal_mode` is a persistent DB property; the rest are per-connection.
4. **Use `BEGIN IMMEDIATE` for every write transaction.** The 9router side opens only deferred transactions: `better-sqlite3` and `bun:sqlite` use `db.transaction(fn)()`, and `node:sqlite` and `sql.js` use `SAVEPOINT`/`RELEASE`, where a savepoint outside a transaction starts a deferred one. rustrouter takes the write lock up front.
5. **Retry `SQLITE_BUSY` / `SQLITE_BUSY_SNAPSHOT` with backoff.** In WAL mode `busy_timeout` does **not** retry `SQLITE_BUSY_SNAPSHOT`; it fails immediately. Two writers that both hold a read snapshot and then try to upgrade is the failure pattern, and the settings update, the connection upsert and the usage save all do it on the 9router side. The usage save swallows the error; the settings update does not, so a settings update can be lost today between two 9router processes. Adding rustrouter widens the window.
6. **Never hold a read snapshot across a write.** Read-then-write must be one `IMMEDIATE` transaction.

### Checkpoint and maintenance contention

`wal_checkpoint(TRUNCATE)` runs on a 60 s timer on the 9router side and is wrapped in `try {} catch {}`. A TRUNCATE checkpoint issued by 9router while rustrouter holds a read snapshot returns `SQLITE_BUSY` and is a no-op: annoying, not corrupting. It does briefly contend for the write lock, so rustrouter sees occasional `SQLITE_BUSY` bursts at 60 s boundaries; `busy_timeout` covers it. Raising rustrouter's `busy_timeout` to 10000 is defensible; 5000 matches the peer.

`VACUUM` appears nowhere. Backups and pruning only ever touch files inside `BACKUPS_DIR`, never `data.sqlite`. `import_db` wipes tables with `DELETE FROM` inside one transaction, at table level, not file level. The only code that replaces the live file wholesale is the sql.js adapter.

### Migration race

`run_versioned_migrations` reads `schemaVersion`, filters pending, and applies each in a transaction stamping the new version. Two processes starting together both read `0`, both compute `pending=[001]`, both run the initial migration (idempotent `CREATE TABLE IF NOT EXISTS` / `CREATE INDEX IF NOT EXISTS`), both stamp `1`. No corruption, but:

- The deferred transaction starts with a write, so the loser blocks on `busy_timeout` and can fail with `SQLITE_BUSY` if the winner is slow.
- `prune_old_backups` can delete a backup directory the other process just created.
- The `backupSchemaVersion` and `appVersion` stamps are `ON CONFLICT DO UPDATE`, so the last writer wins. Harmless.

There is no compare-and-swap on `schemaVersion` and no startup lock file. The design assumes one process at a time, and the 9router CLI reinforces that by killing other instances before start. rustrouter takes the same stance: **do not run both at once against the same file** unless every condition above holds, and prefer a one-way cutover.

### Files that must keep their paths

Schema-identical is necessary but not sufficient. An upgrade of an existing install also depends on these files, and regenerating any of them invalidates live credentials:

| File | Written by | Why it must match |
|---|---|---|
| `DATA_DIR/jwt-secret` | `identity::jwt_secret` (`crates/router-db/src/identity.rs`), read by `auth/session.rs` | Regenerating invalidates every session cookie |
| `auth/cli-secret` | `identity::consistent_machine_id` (`load_cli_secret`) | Feeds the CLI-token machine ID |
| `DATA_DIR/machine-id` | `identity::raw_machine_id` | Same |
| `DATA_DIR/model-catalog.json` (+ `-raw`) | `services::model_catalog_sync` (`crates/router-sse/src/services/model_catalog_sync.rs`) | Capability overlay |
| `DATA_DIR/db/backups/` | `backup.rs` | Backup retention |
| `cwd/logs/translator` | `routes/translator.rs` | Note: cwd-relative, not `DATA_DIR` |

## Verification

The contract test is a round trip, not an assertion:

1. Create a fresh DB with 9router (unmodified).
2. Write through rustrouter: a settings update, a connection upsert, a combo, an API key, a usage record.
3. Reopen with 9router. It must report the same `schemaVersion` and read every value back.
4. `sqlite3 <file> .schema` from both must be identical after step 1 and after step 2.
5. Compare the raw TEXT of one settings row and one connection row written by each side: byte equality, not JSON equality.

Step 5 is what catches key ordering and date precision. Steps 1 to 4 will pass with a sorted-key serializer and a second-precision clock.
