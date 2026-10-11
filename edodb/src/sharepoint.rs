//! Microsoft Graph SharePoint Lists driver.
//! Requires a bearer access token with appropriate Graph site/list permissions.
//! Primary key is a unique indexed Text field; no cross-item transactions.
use super::*;
use reqwest::{Client, Method, StatusCode};
use serde_json::{json, Value as Json};

pub struct SharePointDriver {
    site_id: String, token: String, client: Client, connected: bool,
    graph_base: String,
}
impl SharePointDriver {
    pub fn new(site_id: impl Into<String>, token: impl Into<String>) -> Self {
        Self { site_id: site_id.into(), token: token.into(), client: Client::new(), connected: false,
            graph_base: "https://graph.microsoft.com/v1.0".into() }
    }
    fn check_connected(&self) -> Result<(), DbError> {
        if self.connected { Ok(()) } else { Err(DbError::NotConnected) }
    }
    fn endpoint(&self, path: &str) -> String {
        format!("{}/sites/{}/{}", self.graph_base, urlencoding::encode(&self.site_id), path)
    }
    async fn request(&self, method: Method, path: &str, body: Option<Json>, etag: Option<&str>) -> Result<Json, DbError> {
        self.request_url(method, &self.endpoint(path), body, etag).await
    }
    async fn request_url(&self, method: Method, url: &str, body: Option<Json>, etag: Option<&str>) -> Result<Json, DbError> {
        let mut request = self.client.request(method, url).bearer_auth(&self.token);
        if let Some(v) = body { request = request.json(&v); }
        if let Some(v) = etag { request = request.header("If-Match", v); }
        let response = request.send().await.map_err(|e| DbError::Backend(e.to_string()))?;
        let status = response.status();
        if status == StatusCode::PRECONDITION_FAILED || status == StatusCode::CONFLICT { return Err(DbError::Conflict); }
        if status == StatusCode::NOT_FOUND { return Err(DbError::NotFound); }
        let text = response.text().await.map_err(|e| DbError::Backend(e.to_string()))?;
        if !status.is_success() { return Err(DbError::Backend(format!("Graph HTTP {status}: {text}"))); }
        if text.is_empty() { return Ok(Json::Null); }
        serde_json::from_str(&text).map_err(|e| DbError::Backend(e.to_string()))
    }
    /// Graph returns the site ID in continuation links unencoded (and may change case),
    /// so compare decoded and case-insensitively; the token is only sent to this site.
    fn is_trusted_next_link(&self, next: &str) -> bool {
        let prefix = format!("{}/sites/{}/", self.graph_base, self.site_id).to_ascii_lowercase();
        urlencoding::decode(next).is_ok_and(|n| n.to_ascii_lowercase().starts_with(&prefix))
    }
    /// SharePoint Number columns are doubles; accept `42` and `42.0`, reject fractions.
    fn json_int(raw: &Json) -> Option<i64> {
        raw.as_i64().or_else(|| raw.as_f64().filter(|f| f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_991.0).map(|f| f as i64))
    }
    /// Internal SharePoint column name for a schema column. Names that collide with
    /// built-in list fields (e.g. `id` is the Graph item id inside `fields`) or that
    /// SharePoint treats specially are prefixed, so models can use them freely.
    fn sp_column(name: &str) -> std::borrow::Cow<'_, str> {
        const RESERVED: &[&str] = &["id", "title", "created", "modified", "author", "editor",
            "attachments", "contenttype", "contenttypeid", "edit", "linktitle", "linktitlenomenu",
            "docicon", "itemchildcount", "folderchildcount", "appauthor", "appeditor", "guid",
            "fileref", "fileleafref", "fsobjtype", "owshiddenversion", "version"];
        if name.starts_with(['_', '@']) || RESERVED.contains(&name.to_ascii_lowercase().as_str()) {
            format!("edodb_{name}").into()
        } else { name.into() }
    }
    fn list_path(schema: &Schema) -> String { format!("lists/{}", urlencoding::encode(schema.table)) }
    fn pk(schema: &Schema) -> Result<&str, DbError> {
        schema.columns.iter().find(|c| c.primary_key).map(|c| c.name).ok_or_else(|| DbError::Invalid("missing primary key".into()))
    }
    fn to_fields(schema: &Schema, record: &Record) -> Result<Json, DbError> {
        let mut map = serde_json::Map::new();
        for c in schema.columns {
            let v = record.get(c.name).ok_or_else(|| DbError::Invalid(format!("missing {}", c.name)))?;
            let value = match v {
                Value::Null => Json::Null, Value::Boolean(v) => json!(v),
                Value::Integer(v) => { if v.abs_diff(0) > 9_007_199_254_740_991 { return Err(DbError::Invalid("SharePoint Number cannot represent this i64 exactly".into())); } json!(v) },
                Value::Float(v) => { if !v.is_finite() { return Err(DbError::Invalid("non-finite float".into())); } json!(v) },
                Value::String(v) => json!(v),
                Value::Date(v) => json!(v.format("%Y-%m-%d").to_string()),
                Value::DateTime(v) => json!(v.to_rfc3339()),
                Value::Time(v) => json!(v.num_seconds_from_midnight()),
                Value::Binary(_) => return Err(DbError::Unsupported("SharePoint binary columns: use attachments or drive items".into())),
            };
            map.insert(Self::sp_column(c.name).into_owned(), value);
        }
        Ok(Json::Object(map))
    }
    fn parse_item(schema: &Schema, item: &Json) -> Result<VersionedRecord, DbError> {
        let fields = item.get("fields").and_then(Json::as_object).ok_or_else(|| DbError::Backend("missing Graph fields".into()))?;
        let mut record = Record::new();
        for c in schema.columns {
            let raw = fields.get(Self::sp_column(c.name).as_ref()).unwrap_or(&Json::Null);
            let value = if raw.is_null() { Value::Null } else { match c.kind {
                DbType::Boolean => Value::Boolean(raw.as_bool().ok_or_else(|| DbError::Invalid(c.name.into()))?),
                DbType::Integer => Value::Integer(Self::json_int(raw).ok_or_else(|| DbError::Invalid(c.name.into()))?),
                DbType::Float => Value::Float(raw.as_f64().ok_or_else(|| DbError::Invalid(c.name.into()))?),
                DbType::String => Value::String(raw.as_str().ok_or_else(|| DbError::Invalid(c.name.into()))?.into()),
                DbType::Date => Value::Date(raw.as_str().ok_or_else(|| DbError::Invalid(c.name.into()))?.get(..10).ok_or_else(|| DbError::Invalid(c.name.into()))?.parse().map_err(|e: chrono::ParseError| DbError::Invalid(e.to_string()))?),
                DbType::DateTime => Value::DateTime(chrono::DateTime::parse_from_rfc3339(raw.as_str().ok_or_else(|| DbError::Invalid(c.name.into()))?).map_err(|e| DbError::Invalid(e.to_string()))?.with_timezone(&Utc)),
                DbType::Time => Value::Time(chrono::NaiveTime::from_num_seconds_from_midnight_opt(Self::json_int(raw).and_then(|v| u32::try_from(v).ok()).ok_or_else(|| DbError::Invalid(c.name.into()))?,0).ok_or_else(|| DbError::Invalid(c.name.into()))?),
                DbType::Binary => return Err(DbError::Unsupported("binary".into())),
            }};
            record.insert(c.name.into(), value);
        }
        let version = item.get("@odata.etag").and_then(Json::as_str).or_else(|| item.get("eTag").and_then(Json::as_str))
            .ok_or_else(|| DbError::Backend("Graph item has no ETag".into()))?.to_string();
        Ok(VersionedRecord { record, version })
    }
    async fn find_item(&self, schema: &Schema, key: &Value) -> Result<Option<Json>, DbError> {
        let pk = Self::pk(schema)?;
        let literal = match key {
            Value::String(s) => format!("'{}'", s.replace('\'', "''")),
            Value::Integer(i) => i.to_string(),
            _ => return Err(DbError::Invalid("SharePoint key must be string or integer".into())),
        };
        let filter = format!("fields/{} eq {literal}", Self::sp_column(pk));
        let path = format!("{}/items?$expand=fields&$filter={}&$top=2", Self::list_path(schema), urlencoding::encode(&filter));
        let result = self.request(Method::GET, &path, None, None).await?;
        let items = result.get("value").and_then(Json::as_array).ok_or_else(|| DbError::Backend("missing items".into()))?;
        if items.len() > 1 { return Err(DbError::Conflict); }
        Ok(items.first().cloned())
    }
}
use chrono::Timelike;
#[async_trait]
impl Driver for SharePointDriver {
    fn set(&mut self, name: &str, value: &Value) -> SetResult {
        match (name, value) {
            ("graph_base", Value::String(url)) if url.starts_with("https://") && !self.connected => { self.graph_base = url.trim_end_matches('/').into(); SetResult::Ok },
            ("graph_base", _) => SetResult::Invalid,
            _ => SetResult::Unknown,
        }
    }
    async fn connect(&mut self) -> Result<(), DbError> {
        if self.connected { return Ok(()); }
        if self.token.is_empty() || self.site_id.is_empty() { return Err(DbError::Invalid("site_id and access token required".into())); }
        self.request(Method::GET, "?$select=id", None, None).await?;
        self.connected = true; Ok(())
    }
    fn is_connected(&self) -> bool { self.connected }
    async fn ensure_schema(&self, schema: &Schema) -> Result<(), DbError> {
        self.check_connected()?;
        // List provisioning is intentionally non-destructive; existing column types are not migrated.
        let list_path = Self::list_path(schema);
        match self.request(Method::GET, &list_path, None, None).await {
            Ok(_) => {},
            Err(DbError::NotFound) => {
                self.request(Method::POST, "lists", Some(json!({"displayName":schema.table,"list":{"template":"genericList"}})), None).await?;
            }
            Err(e) => return Err(e),
        }
        let existing = self.request(Method::GET, &format!("{list_path}/columns"), None, None).await?;
        let names = existing.get("value").and_then(Json::as_array).ok_or_else(|| DbError::Backend("missing columns".into()))?;
        for (i, c) in schema.columns.iter().enumerate() {
            let sp_name = Self::sp_column(c.name);
            if schema.columns[..i].iter().any(|o| Self::sp_column(o.name).eq_ignore_ascii_case(&sp_name)) {
                return Err(DbError::Invalid(format!("column {} collides with another column in SharePoint", c.name)));
            }
            if names.iter().any(|v| v.get("name").and_then(Json::as_str) == Some(sp_name.as_ref())) { continue; }
            let mut column = json!({"name":sp_name,"enforceUniqueValues":c.primary_key || c.unique});
            match c.kind {
                DbType::String => column["text"] = json!({"allowMultipleLines":false}),
                DbType::Boolean => column["boolean"] = json!({}),
                DbType::Integer | DbType::Float | DbType::Time => column["number"] = json!({"decimalPlaces": if c.kind == DbType::Integer || c.kind == DbType::Time {"none"} else {"automatic"}}),
                DbType::Date | DbType::DateTime => column["dateTime"] = json!({"format": if c.kind == DbType::Date {"dateOnly"} else {"dateTime"}}),
                DbType::Binary => return Err(DbError::Unsupported("binary SharePoint columns".into())),
            }
            if c.primary_key || c.unique { column["indexed"] = json!(true); }
            self.request(Method::POST, &format!("{list_path}/columns"), Some(column), None).await?;
        }
        Ok(())
    }
    async fn insert(&self, schema: &Schema, record: Record) -> Result<(), DbError> {
        self.check_connected()?;
        self.request(Method::POST, &format!("{}/items", Self::list_path(schema)), Some(json!({"fields":Self::to_fields(schema,&record)?})), None).await?;
        Ok(())
    }
    async fn get(&self, schema: &Schema, key: &Value) -> Result<Option<VersionedRecord>, DbError> {
        self.check_connected()?;
        self.find_item(schema,key).await?.as_ref().map(|v| Self::parse_item(schema,v)).transpose()
    }
    async fn find(&self, schema: &Schema, query: Query) -> Result<Vec<VersionedRecord>, DbError> {
        self.check_connected()?;
        let mut url = self.endpoint(&format!("{}/items?$expand=fields", Self::list_path(schema)));
        if let Some(Filter::Eq(name, value)) = query.filter {
            if !schema.columns.iter().any(|c| c.name == name) { return Err(DbError::Invalid("unknown filter column".into())); }
            let lit = match value { Value::String(s) => format!("'{}'", s.replace('\'', "''")), Value::Integer(i) => i.to_string(), Value::Boolean(b) => b.to_string(), _ => return Err(DbError::Unsupported("SharePoint filter type".into())) };
            url.push_str(&format!("&$filter={}",urlencoding::encode(&format!("fields/{} eq {lit}", Self::sp_column(&name)))));
        }
        // Same order as the SQL driver; the primary key column is indexed, so Graph can sort by it.
        url.push_str(&format!("&$orderby={}", urlencoding::encode(&format!("fields/{}", Self::sp_column(Self::pk(schema)?)))));
        if let Some(limit) = query.limit {
            if limit == 0 { return Ok(Vec::new()); }
            url.push_str(&format!("&$top={}", limit.min(999)));
        }
        let mut result = Vec::new();
        let max = query.limit.unwrap_or(usize::MAX);
        loop {
            let page = self.request_url(Method::GET, &url, None, None).await?;
            for item in page.get("value").and_then(Json::as_array).ok_or_else(|| DbError::Backend("missing items".into()))? {
                result.push(Self::parse_item(schema,item)?);
                if result.len() >= max { return Ok(result); }
            }
            let Some(next) = page.get("@odata.nextLink").and_then(Json::as_str) else { break; };
            // Only follow Graph-generated continuation URLs for this site.
            if !self.is_trusted_next_link(next) { return Err(DbError::Backend("untrusted pagination URL".into())); }
            url = next.to_string();
        }
        Ok(result)
    }
    async fn update(&self, schema: &Schema, key: &Value, version: &str, record: Record) -> Result<String, DbError> {
        self.check_connected()?;
        if record.get(Self::pk(schema)?) != Some(key) { return Err(DbError::Invalid("cannot change primary key".into())); }
        let item = self.find_item(schema,key).await?.ok_or(DbError::NotFound)?;
        let id = item.get("id").and_then(Json::as_str).ok_or_else(|| DbError::Backend("missing item id".into()))?;
        self.request(Method::PATCH, &format!("{}/items/{id}/fields", Self::list_path(schema)), Some(Self::to_fields(schema,&record)?), Some(version)).await?;
        let updated = self.find_item(schema,key).await?.ok_or(DbError::NotFound)?;
        Ok(Self::parse_item(schema,&updated)?.version)
    }
    async fn delete(&self, schema: &Schema, key: &Value, version: &str) -> Result<(), DbError> {
        self.check_connected()?;
        let item = self.find_item(schema,key).await?.ok_or(DbError::NotFound)?;
        let id = item.get("id").and_then(Json::as_str).ok_or_else(|| DbError::Backend("missing item id".into()))?;
        self.request(Method::DELETE, &format!("{}/items/{id}", Self::list_path(schema)), None, Some(version)).await?;
        Ok(())
    }
}
