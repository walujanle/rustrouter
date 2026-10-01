//! Declarative schema, byte-identical to the 9router schema.
//!
//! Column order inside each `CREATE TABLE` is load-bearing: SQLite stores it in
//! `sqlite_master`, and the additive schema sync diffs against it. Keep the
//! arrays in source order.

/// Bump this whenever a table, column or index below changes. It gates the
/// pre-migration safety backup.
pub const SCHEMA_VERSION: i64 = 1;

/// Applied verbatim on every connection. Byte-identical to the 9router set:
/// `journal_mode`, `auto_vacuum` and the other persisted pragmas live in the
/// file header, so this block is the shared-file contract.
pub const PRAGMA_SQL: &str = "\
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA temp_store = MEMORY;
PRAGMA mmap_size = 30000000;
PRAGMA cache_size = -64000;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
";

/// Memory-only pragma overlay, applied after [`PRAGMA_SQL`] on every connection.
///
/// Kept out of `PRAGMA_SQL` on purpose: every pragma here is connection-local
/// and never written to the file header, so a connection opened by 9router is
/// unaffected and the shared file stays byte-identical. `cache_size = -2000`
/// caps the per-connection page cache at 2 MiB, overriding the 64 MiB default in
/// [`PRAGMA_SQL`], which bounds the heap a pool of connections can hold.
pub const MEMORY_PRAGMA_SQL: &str = "\
PRAGMA cache_size = -2000;
";

/// A table definition: ordered `(column, type)` pairs, an optional composite
/// primary key clause, and any `CREATE INDEX` statements.
pub struct TableDef {
    pub name: &'static str,
    pub columns: &'static [(&'static str, &'static str)],
    /// Appended as the last item in the column list, e.g. `PRIMARY KEY (scope, key)`.
    pub primary_key: Option<&'static str>,
    pub indexes: &'static [&'static str],
}

impl TableDef {
    /// Build the `CREATE TABLE` statement for this definition.
    pub fn create_sql(&self) -> String {
        let mut cols: Vec<String> = self
            .columns
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect();
        if let Some(pk) = self.primary_key {
            cols.push(pk.to_string());
        }
        format!(
            "CREATE TABLE IF NOT EXISTS {} ({})",
            self.name,
            cols.join(", ")
        )
    }
}

pub const TABLES: &[TableDef] = &[
    TableDef {
        name: "_meta",
        columns: &[("key", "TEXT PRIMARY KEY"), ("value", "TEXT NOT NULL")],
        primary_key: None,
        indexes: &[],
    },
    TableDef {
        name: "settings",
        columns: &[
            ("id", "INTEGER PRIMARY KEY CHECK (id = 1)"),
            ("data", "TEXT NOT NULL"),
        ],
        primary_key: None,
        indexes: &[],
    },
    TableDef {
        name: "providerConnections",
        columns: &[
            ("id", "TEXT PRIMARY KEY"),
            ("provider", "TEXT NOT NULL"),
            ("authType", "TEXT NOT NULL"),
            ("name", "TEXT"),
            ("email", "TEXT"),
            ("priority", "INTEGER"),
            ("isActive", "INTEGER DEFAULT 1"),
            ("data", "TEXT NOT NULL"),
            ("createdAt", "TEXT NOT NULL"),
            ("updatedAt", "TEXT NOT NULL"),
        ],
        primary_key: None,
        indexes: &[
            "CREATE INDEX IF NOT EXISTS idx_pc_provider ON providerConnections(provider)",
            "CREATE INDEX IF NOT EXISTS idx_pc_provider_active ON providerConnections(provider, isActive)",
            "CREATE INDEX IF NOT EXISTS idx_pc_priority ON providerConnections(provider, priority)",
        ],
    },
    TableDef {
        name: "providerNodes",
        columns: &[
            ("id", "TEXT PRIMARY KEY"),
            ("type", "TEXT"),
            ("name", "TEXT"),
            ("data", "TEXT NOT NULL"),
            ("createdAt", "TEXT NOT NULL"),
            ("updatedAt", "TEXT NOT NULL"),
        ],
        primary_key: None,
        indexes: &["CREATE INDEX IF NOT EXISTS idx_pn_type ON providerNodes(type)"],
    },
    TableDef {
        name: "proxyPools",
        columns: &[
            ("id", "TEXT PRIMARY KEY"),
            ("isActive", "INTEGER DEFAULT 1"),
            ("testStatus", "TEXT"),
            ("data", "TEXT NOT NULL"),
            ("createdAt", "TEXT NOT NULL"),
            ("updatedAt", "TEXT NOT NULL"),
        ],
        primary_key: None,
        indexes: &[
            "CREATE INDEX IF NOT EXISTS idx_pp_active ON proxyPools(isActive)",
            "CREATE INDEX IF NOT EXISTS idx_pp_status ON proxyPools(testStatus)",
        ],
    },
    TableDef {
        name: "apiKeys",
        columns: &[
            ("id", "TEXT PRIMARY KEY"),
            ("key", "TEXT UNIQUE NOT NULL"),
            ("name", "TEXT"),
            ("machineId", "TEXT"),
            ("isActive", "INTEGER DEFAULT 1"),
            ("createdAt", "TEXT NOT NULL"),
        ],
        primary_key: None,
        indexes: &["CREATE INDEX IF NOT EXISTS idx_ak_key ON apiKeys(key)"],
    },
    TableDef {
        name: "combos",
        columns: &[
            ("id", "TEXT PRIMARY KEY"),
            ("name", "TEXT UNIQUE NOT NULL"),
            ("kind", "TEXT"),
            ("models", "TEXT NOT NULL"),
            ("createdAt", "TEXT NOT NULL"),
            ("updatedAt", "TEXT NOT NULL"),
        ],
        primary_key: None,
        indexes: &["CREATE INDEX IF NOT EXISTS idx_combo_name ON combos(name)"],
    },
    TableDef {
        name: "kv",
        columns: &[
            ("scope", "TEXT NOT NULL"),
            ("key", "TEXT NOT NULL"),
            ("value", "TEXT NOT NULL"),
        ],
        primary_key: Some("PRIMARY KEY (scope, key)"),
        indexes: &["CREATE INDEX IF NOT EXISTS idx_kv_scope ON kv(scope)"],
    },
    TableDef {
        name: "usageHistory",
        columns: &[
            ("id", "INTEGER PRIMARY KEY AUTOINCREMENT"),
            ("timestamp", "TEXT NOT NULL"),
            ("provider", "TEXT"),
            ("model", "TEXT"),
            ("connectionId", "TEXT"),
            ("apiKey", "TEXT"),
            ("endpoint", "TEXT"),
            ("promptTokens", "INTEGER DEFAULT 0"),
            ("completionTokens", "INTEGER DEFAULT 0"),
            ("cost", "REAL DEFAULT 0"),
            ("status", "TEXT"),
            ("tokens", "TEXT"),
            ("meta", "TEXT"),
        ],
        primary_key: None,
        indexes: &[
            "CREATE INDEX IF NOT EXISTS idx_uh_ts ON usageHistory(timestamp DESC)",
            "CREATE INDEX IF NOT EXISTS idx_uh_provider ON usageHistory(provider)",
            "CREATE INDEX IF NOT EXISTS idx_uh_model ON usageHistory(model)",
            "CREATE INDEX IF NOT EXISTS idx_uh_conn ON usageHistory(connectionId)",
        ],
    },
    TableDef {
        name: "usageDaily",
        columns: &[("dateKey", "TEXT PRIMARY KEY"), ("data", "TEXT NOT NULL")],
        primary_key: None,
        indexes: &[],
    },
    TableDef {
        name: "requestDetails",
        columns: &[
            ("id", "TEXT PRIMARY KEY"),
            ("timestamp", "TEXT NOT NULL"),
            ("provider", "TEXT"),
            ("model", "TEXT"),
            ("connectionId", "TEXT"),
            ("status", "TEXT"),
            ("data", "TEXT NOT NULL"),
        ],
        primary_key: None,
        indexes: &[
            "CREATE INDEX IF NOT EXISTS idx_rd_ts ON requestDetails(timestamp DESC)",
            "CREATE INDEX IF NOT EXISTS idx_rd_provider ON requestDetails(provider)",
            "CREATE INDEX IF NOT EXISTS idx_rd_model ON requestDetails(model)",
            "CREATE INDEX IF NOT EXISTS idx_rd_conn ON requestDetails(connectionId)",
        ],
    },
];

/// Look up a table by name.
pub fn table(name: &str) -> Option<&'static TableDef> {
    TABLES.iter().find(|t| t.name == name)
}

/// The `CREATE TABLE` statement for a named table, or `None` when unknown.
pub fn create_table_sql(name: &str) -> Option<String> {
    table(name).map(TableDef::create_sql)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eleven_tables_in_reference_order() {
        let names: Vec<&str> = TABLES.iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec![
                "_meta",
                "settings",
                "providerConnections",
                "providerNodes",
                "proxyPools",
                "apiKeys",
                "combos",
                "kv",
                "usageHistory",
                "usageDaily",
                "requestDetails",
            ]
        );
    }

    #[test]
    fn create_sql_matches_reference_bytes() {
        assert_eq!(
            create_table_sql("settings").unwrap(),
            "CREATE TABLE IF NOT EXISTS settings (id INTEGER PRIMARY KEY CHECK (id = 1), data TEXT NOT NULL)"
        );
        assert_eq!(
            create_table_sql("kv").unwrap(),
            "CREATE TABLE IF NOT EXISTS kv (scope TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY (scope, key))"
        );
        assert_eq!(
            create_table_sql("usageHistory").unwrap(),
            "CREATE TABLE IF NOT EXISTS usageHistory (id INTEGER PRIMARY KEY AUTOINCREMENT, timestamp TEXT NOT NULL, provider TEXT, model TEXT, connectionId TEXT, apiKey TEXT, endpoint TEXT, promptTokens INTEGER DEFAULT 0, completionTokens INTEGER DEFAULT 0, cost REAL DEFAULT 0, status TEXT, tokens TEXT, meta TEXT)"
        );
    }

    #[test]
    fn every_index_is_idempotent_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for t in TABLES {
            for idx in t.indexes {
                assert!(idx.starts_with("CREATE INDEX IF NOT EXISTS "), "{idx}");
                let name = idx
                    .split_whitespace()
                    .nth(5)
                    .expect("index name")
                    .to_string();
                assert!(seen.insert(name.clone()), "duplicate index {name}");
            }
        }
    }
}
