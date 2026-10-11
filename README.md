# edodb – Rust CRUD library and CLI

**Experimental release.** Builds and passes its tests for the memory and SQLite drivers; PostgreSQL, MariaDB/MySQL and SharePoint have not been run against live instances.

## Drivers

- `memory`: reference CRUD implementation.
- `sql`: SQLx AnyPool for SQLite, PostgreSQL, MariaDB/MySQL, optimistic concurrency using `_edodb_version`.
- `sharepoint`: Microsoft Graph Lists driver using ETags and `If-Match`. Requires Graph bearer token, site ID, appropriate permissions.
- `edodb` (binary): runs a single CRUD method per call and prints one JSON result.

## Use as a dependency

```toml
[dependencies]
edodb = { git = "https://github.com/benutzer/edodb.git", tag = "v0.1.0" }
# without the CLI (no tokio runtime dependency):
# edodb = { git = "https://github.com/benutzer/edodb.git", tag = "v0.1.0", default-features = false }
serde = { version = "1", features = ["derive"] }
```

Cargo finds the `edodb` package inside the workspace on its own. Models need `serde` for `Serialize`/`Deserialize`; keep the dependency name `edodb`, because the derive macro generates `::edodb::` paths.

```rust
use edodb::{Database, DbModel, sql::SqlDriver};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, DbModel)]
#[db(table = "people")]
struct Person { #[db(primary_key)] id: String, name: String }

let mut db = Database::new(Box::new(SqlDriver::new("sqlite:///tmp/app.db?mode=rwc")));
db.connect().await?;
db.ensure_schema::<Person>().await?;
db.insert(&Person { id: "p1".into(), name: "Ada".into() }).await?;
```

## Build

```sh
cargo test --workspace
cargo build -p edodb --release        # binary: target/release/edodb
```

Library users who do not need the CLI can depend on `edodb` with `default-features = false` (drops the `cli` feature and its tokio runtime dependency).

## CLI

```sh
edodb -d DRIVER [--url URL] [--site ID] [--token TOKEN] [--set key=value]... -m METHOD -b JSON
```

- `-d`/`--driver`: `memory`, `sqlite`, `postgres`, `mariadb`/`mysql`, `sharepoint`
- `-m`/`--method`: `ensure_schema`, `insert`, `get`, `find`, `update`, `delete`
- `-b`/`--body`: the request as JSON; `-b -` reads it from stdin
- `--url` (SQL), `--site` and `--token` (SharePoint; prefer the `EDODB_GRAPH_TOKEN` environment variable), `--set name=value` for driver options (e.g. `--set max_connections=5` for SQL). Unknown options warn; invalid options abort.

The body always contains the `schema`; depending on the method also `record`, `key`, `version`, `filter` (`{"field":"name","eq":"Ada"}`) and `limit`. `get` and `find` return a `version` per record; pass it to `update` and `delete` for optimistic concurrency.

Output is one JSON object on stdout (`{"ok":true,...}` or `{"ok":false,"error":"..."}`). Exit codes: `0` success, `1` method failed, `2` usage error.

Each call is a separate process, so the `memory` driver keeps nothing between calls; use it only to validate requests.

```sh
SCHEMA='{"table":"people","columns":[{"name":"id","type":"string","primary_key":true},{"name":"name","type":"string"}]}'
db() { edodb -d sqlite --url 'sqlite:///tmp/edodb.db?mode=rwc' "$@"; }   # works in bash and zsh

db -m ensure_schema -b "{\"schema\":$SCHEMA}"
db -m insert -b "{\"schema\":$SCHEMA,\"record\":{\"id\":\"p1\",\"name\":\"Ada\"}}"
db -m get    -b "{\"schema\":$SCHEMA,\"key\":\"p1\"}"
db -m find   -b "{\"schema\":$SCHEMA,\"filter\":{\"field\":\"name\",\"eq\":\"Ada\"},\"limit\":10}"
db -m update -b "{\"schema\":$SCHEMA,\"key\":\"p1\",\"version\":\"1\",\"record\":{\"id\":\"p1\",\"name\":\"Ada L.\"}}"
db -m delete -b "{\"schema\":$SCHEMA,\"key\":\"p1\",\"version\":\"2\"}"
```

Other connections:

```sh
edodb -d postgres --url 'postgres://user:password@localhost/mydb' -m ... -b ...
edodb -d mariadb  --url 'mysql://user:password@localhost/mydb' -m ... -b ...
export EDODB_GRAPH_TOKEN='...'
edodb -d sharepoint --site 'tenant.sharepoint.com,site-guid,web-guid' -m ... -b ...
```

## Known limitations

- SQL schemas are create-only; existing schema migrations are not implemented. SharePoint creates missing columns without migration.
- MariaDB String fields currently map to VARCHAR(255). Native SQL date/time and binary portability require further testing.
- SharePoint supports neither SQL transactions nor arbitrary queries. Graph field filtering may require indexed columns; API semantics and ETag PATCH handling require real-tenant verification.
- SharePoint column names that collide with built-in list fields (`id`, `Title`, `Created`, `Modified`, …) or start with `_`/`@` are stored with an `edodb_` prefix (e.g. `id` → `edodb_id`); the model keeps its original names.
- SharePoint Time maps to seconds since midnight; binary columns are unsupported; integers beyond the exact 53-bit number range are rejected.
- SharePoint auth token refresh, throttling/backoff and integration tests are not implemented.
- SQL PostgreSQL/MariaDB and SharePoint backends have not been exercised against live instances.

## License

MIT, see [LICENSE](LICENSE).
