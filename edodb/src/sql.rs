//! SQLx Any backend for SQLite, PostgreSQL and MariaDB/MySQL.
use super::*;
use sqlx::{any::{AnyPoolOptions, AnyRow}, AnyPool, Row};

pub struct SqlDriver { url: String, pool: Option<AnyPool>, max_connections: u32 }
impl SqlDriver {
    pub fn new(url: impl Into<String>) -> Self { Self { url: url.into(), pool: None, max_connections: 5 } }
    fn pool(&self) -> Result<&AnyPool, DbError> { self.pool.as_ref().ok_or(DbError::NotConnected) }
    fn dialect(&self) -> &'static str {
        if self.url.starts_with("postgres:") || self.url.starts_with("postgresql:") { "pg" }
        else if self.url.starts_with("mysql:") || self.url.starts_with("mariadb:") { "mysql" } else { "sqlite" }
    }
    fn ident(&self, s: &str) -> Result<String, DbError> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') || s.as_bytes()[0].is_ascii_digit() {
            return Err(DbError::Invalid(format!("invalid identifier: {s}")));
        }
        Ok(if self.dialect() == "mysql" { format!("`{s}`") } else { format!("\"{s}\"") })
    }
    fn placeholder(&self, index: usize) -> String {
        if self.dialect() == "pg" { format!("${index}") } else { "?".into() }
    }
    fn sql_type(&self, kind: DbType) -> &'static str {
        match (self.dialect(), kind) {
            ("pg", DbType::Boolean) => "BOOLEAN", ("pg", DbType::Integer) => "BIGINT",
            ("pg", DbType::Float) => "DOUBLE PRECISION", ("pg", DbType::Binary) => "BYTEA",
            ("mysql", DbType::Boolean) => "BOOLEAN", ("mysql", DbType::Integer) => "BIGINT",
            ("mysql", DbType::Float) => "DOUBLE", ("mysql", DbType::Binary) => "LONGBLOB",
            ("mysql", DbType::String) => "VARCHAR(255)",
            // sqlx Any cannot decode SQLite BOOLEAN columns; store 0/1 and decode via i64.
            (_, DbType::Boolean | DbType::Integer) => "INTEGER",
            (_, DbType::Float) => "REAL", (_, DbType::Binary) => "BLOB",
            _ => "TEXT",
        }
    }
    fn pk<'a>(schema: &'a Schema) -> Result<&'a Column, DbError> {
        schema.columns.iter().find(|c| c.primary_key).ok_or_else(|| DbError::Invalid("missing primary key".into()))
    }
    fn bind<'q>(mut query: sqlx::query::Query<'q, sqlx::Any, <sqlx::Any as sqlx::Database>::Arguments<'q>>, kind: DbType, value: &'q Value)
        -> sqlx::query::Query<'q, sqlx::Any, <sqlx::Any as sqlx::Database>::Arguments<'q>> {
        query = match value {
            // Typed NULLs: Postgres rejects a text NULL for non-text columns.
            Value::Null => match kind {
                DbType::Boolean => query.bind(Option::<bool>::None),
                DbType::Integer => query.bind(Option::<i64>::None),
                DbType::Float => query.bind(Option::<f64>::None),
                DbType::Binary => query.bind(Option::<Vec<u8>>::None),
                _ => query.bind(Option::<String>::None),
            },
            Value::Boolean(v) => query.bind(*v), Value::Integer(v) => query.bind(*v),
            Value::Float(v) => query.bind(*v), Value::String(v) => query.bind(v.as_str()),
            Value::Binary(v) => query.bind(v.as_slice()),
            Value::Date(v) => query.bind(v.to_string()),
            Value::Time(v) => query.bind(v.to_string()),
            Value::DateTime(v) => query.bind(v.to_rfc3339()),
        }; query
    }
    fn decode(row: &AnyRow, schema: &Schema) -> Result<VersionedRecord, DbError> {
        let mut record = Record::new();
        for c in schema.columns {
            let value = match c.kind {
                // SQLite stores booleans as INTEGER, which Any decodes as BigInt.
                DbType::Boolean => row.try_get::<Option<bool>,_>(c.name)
                    .or_else(|_| row.try_get::<Option<i64>,_>(c.name).map(|v| v.map(|i| i != 0)))
                    .map(|v| v.map(Value::Boolean)),
                DbType::Integer => row.try_get::<Option<i64>,_>(c.name).map(|v| v.map(Value::Integer)),
                DbType::Float => row.try_get::<Option<f64>,_>(c.name).map(|v| v.map(Value::Float)),
                DbType::Binary => row.try_get::<Option<Vec<u8>>,_>(c.name).map(|v| v.map(Value::Binary)),
                // String, Date, Time and DateTime are stored as text.
                _ => row.try_get::<Option<String>,_>(c.name).map(|v| v.map(Value::String)),
            }.map_err(|e| DbError::Backend(e.to_string()))?;
            let value = match value {
                Some(Value::String(s)) => Some(match c.kind {
                    DbType::String => Value::String(s),
                    DbType::Date => Value::Date(s.parse().map_err(|e: chrono::ParseError| DbError::Invalid(e.to_string()))?),
                    DbType::Time => Value::Time(s.parse().map_err(|e: chrono::ParseError| DbError::Invalid(e.to_string()))?),
                    DbType::DateTime => Value::DateTime(chrono::DateTime::parse_from_rfc3339(&s).map_err(|e| DbError::Invalid(e.to_string()))?.with_timezone(&Utc)),
                    _ => unreachable!(),
                }),
                other => other,
            };
            record.insert(c.name.into(), value.unwrap_or(Value::Null));
        }
        let version: i64 = row.try_get("_edodb_version").map_err(|e| DbError::Backend(e.to_string()))?;
        Ok(VersionedRecord { record, version: version.to_string() })
    }
}
fn sql_error(e: sqlx::Error) -> DbError {
    if let sqlx::Error::Database(ref d) = e {
        if d.is_unique_violation() { return DbError::Conflict; }
    }
    DbError::Backend(e.to_string())
}
#[async_trait]
impl Driver for SqlDriver {
    fn set(&mut self, name: &str, value: &Value) -> SetResult {
        match name {
            "max_connections" => match value { Value::Integer(v) if *v > 0 && *v <= 1000 => { self.max_connections = *v as u32; SetResult::Ok }, _ => SetResult::Invalid },
            _ => SetResult::Unknown,
        }
    }
    async fn connect(&mut self) -> Result<(), DbError> {
        if self.pool.is_some() { return Ok(()); }
        sqlx::any::install_default_drivers();
        let pool = AnyPoolOptions::new().max_connections(self.max_connections).connect(&self.url).await.map_err(sql_error)?;
        self.pool = Some(pool); Ok(())
    }
    fn is_connected(&self) -> bool { self.pool.as_ref().is_some_and(|p| !p.is_closed()) }
    async fn ensure_schema(&self, schema: &Schema) -> Result<(), DbError> {
        let mut cols = Vec::new();
        for c in schema.columns {
            let mut decl = format!("{} {}", self.ident(c.name)?, self.sql_type(c.kind));
            if !c.nullable || c.primary_key { decl.push_str(" NOT NULL"); }
            if c.primary_key { decl.push_str(" PRIMARY KEY"); }
            if c.unique { decl.push_str(" UNIQUE"); }
            cols.push(decl);
        }
        cols.push("_edodb_version BIGINT NOT NULL DEFAULT 1".into());
        let sql = format!("CREATE TABLE IF NOT EXISTS {} ({})", self.ident(schema.table)?, cols.join(", "));
        sqlx::query(&sql).execute(self.pool()?).await.map_err(sql_error)?; Ok(())
    }
    async fn insert(&self, schema: &Schema, record: Record) -> Result<(), DbError> {
        let names = schema.columns.iter().map(|c| self.ident(c.name)).collect::<Result<Vec<_>,_>>()?;
        let values = schema.columns.iter().map(|c| record.get(c.name).ok_or_else(|| DbError::Invalid(format!("missing {}", c.name)))).collect::<Result<Vec<_>,_>>()?;
        let params = (1..=names.len()).map(|i| self.placeholder(i)).collect::<Vec<_>>().join(",");
        let sql = format!("INSERT INTO {} ({}) VALUES ({})", self.ident(schema.table)?, names.join(","), params);
        let mut q = sqlx::query(&sql);
        for (c, v) in schema.columns.iter().zip(values) { q = Self::bind(q, c.kind, v); }
        q.execute(self.pool()?).await.map_err(sql_error)?; Ok(())
    }
    async fn get(&self, schema: &Schema, key: &Value) -> Result<Option<VersionedRecord>, DbError> {
        let pk = Self::pk(schema)?;
        let sql = format!("SELECT * FROM {} WHERE {} = {}", self.ident(schema.table)?, self.ident(pk.name)?, self.placeholder(1));
        let row = Self::bind(sqlx::query(&sql), pk.kind, key).fetch_optional(self.pool()?).await.map_err(sql_error)?;
        row.map(|r| Self::decode(&r, schema)).transpose()
    }
    async fn find(&self, schema: &Schema, query: Query) -> Result<Vec<VersionedRecord>, DbError> {
        let mut sql = format!("SELECT * FROM {}", self.ident(schema.table)?);
        let mut filter_kind = None;
        if let Some(Filter::Eq(ref name, _)) = query.filter {
            let col = schema.columns.iter().find(|c| c.name == name).ok_or_else(|| DbError::Invalid("unknown filter column".into()))?;
            filter_kind = Some(col.kind);
            sql.push_str(&format!(" WHERE {} = {}", self.ident(name)?, self.placeholder(1)));
        }
        sql.push_str(&format!(" ORDER BY {}", self.ident(Self::pk(schema)?.name)?));
        if let Some(limit) = query.limit { sql.push_str(&format!(" LIMIT {limit}")); }
        let mut q = sqlx::query(&sql);
        if let (Some(Filter::Eq(_, value)), Some(kind)) = (&query.filter, filter_kind) { q = Self::bind(q, kind, value); }
        let rows = q.fetch_all(self.pool()?).await.map_err(sql_error)?;
        rows.iter().map(|r| Self::decode(r, schema)).collect()
    }
    async fn update(&self, schema: &Schema, key: &Value, version: &str, record: Record) -> Result<String, DbError> {
        let pk_col = Self::pk(schema)?;
        let pk = pk_col.name;
        if record.get(pk) != Some(key) { return Err(DbError::Invalid("cannot change primary key".into())); }
        let cols = schema.columns.iter().filter(|c| !c.primary_key).collect::<Vec<_>>();
        let assigns = cols.iter().enumerate().map(|(i,c)| Ok(format!("{} = {}", self.ident(c.name)?, self.placeholder(i+1)))).collect::<Result<Vec<_>,DbError>>()?;
        let sql = format!("UPDATE {} SET {}, _edodb_version = _edodb_version + 1 WHERE {} = {} AND _edodb_version = {}",
            self.ident(schema.table)?, assigns.join(", "), self.ident(pk)?, self.placeholder(cols.len()+1), self.placeholder(cols.len()+2));
        let mut q = sqlx::query(&sql);
        for c in cols { q = Self::bind(q, c.kind, record.get(c.name).ok_or_else(|| DbError::Invalid(format!("missing {}", c.name)))?); }
        q = Self::bind(q, pk_col.kind, key);
        let ver = version.parse::<i64>().map_err(|_| DbError::Invalid("bad version".into()))?;
        q = q.bind(ver);
        let affected = q.execute(self.pool()?).await.map_err(sql_error)?.rows_affected();
        if affected == 0 { return Err(if self.get(schema, key).await?.is_some() { DbError::Conflict } else { DbError::NotFound }); }
        Ok((ver+1).to_string())
    }
    async fn delete(&self, schema: &Schema, key: &Value, version: &str) -> Result<(), DbError> {
        let pk = Self::pk(schema)?;
        let sql = format!("DELETE FROM {} WHERE {} = {} AND _edodb_version = {}", self.ident(schema.table)?, self.ident(pk.name)?, self.placeholder(1), self.placeholder(2));
        let ver = version.parse::<i64>().map_err(|_| DbError::Invalid("bad version".into()))?;
        let result = Self::bind(sqlx::query(&sql), pk.kind, key).bind(ver).execute(self.pool()?).await.map_err(sql_error)?;
        if result.rows_affected() == 0 { return Err(if self.get(schema,key).await?.is_some() { DbError::Conflict } else { DbError::NotFound }); }
        Ok(())
    }
}
