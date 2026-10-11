//! Single-command CLI: `edodb -d DRIVER [options] -m METHOD -b JSON`.
//! Runs exactly one CRUD method and prints one JSON result line to stdout.
use edodb::{memory::MemoryDriver, sharepoint::SharePointDriver, sql::SqlDriver, *};
use serde_json::{json, Value as Json};
use std::io::Read;

fn fail(s: impl ToString) -> Json { json!({"ok":false,"error":s.to_string()}) }
fn kind(s: &str) -> Result<DbType,String> {
    match s {"string"=>Ok(DbType::String),"integer"=>Ok(DbType::Integer),"float"=>Ok(DbType::Float),
        "boolean"=>Ok(DbType::Boolean),"binary"=>Ok(DbType::Binary),"date"=>Ok(DbType::Date),
        "time"=>Ok(DbType::Time),"datetime"=>Ok(DbType::DateTime),_=>Err(format!("unknown type {s}"))}
}
fn schema(v: &Json) -> Result<Schema,String> {
    let table = v.get("table").and_then(Json::as_str).ok_or("schema.table missing")?.to_string();
    let mut columns = Vec::new();
    for c in v.get("columns").and_then(Json::as_array).ok_or("schema.columns missing")? {
        let name = c.get("name").and_then(Json::as_str).ok_or("column.name missing")?.to_string();
        columns.push(Column { name: Box::leak(name.into_boxed_str()),
            kind:kind(c.get("type").and_then(Json::as_str).ok_or("column.type missing")?)?,
            nullable:c.get("nullable").and_then(Json::as_bool).unwrap_or(false),
            primary_key:c.get("primary_key").and_then(Json::as_bool).unwrap_or(false),
            unique:c.get("unique").and_then(Json::as_bool).unwrap_or(false) });
    }
    if columns.iter().filter(|c| c.primary_key).count()!=1 { return Err("exactly one primary key required".into()); }
    Ok(Schema { table: Box::leak(table.into_boxed_str()), columns:Box::leak(columns.into_boxed_slice()) })
}
fn decode(v:&Json, t:DbType)->Result<Value,String> {
    if v.is_null() { return Ok(Value::Null); }
    Ok(match t {
        DbType::String => Value::String(v.as_str().ok_or("expected string")?.into()),
        DbType::Integer => Value::Integer(v.as_i64().ok_or("expected integer")?),
        DbType::Float => Value::Float(v.as_f64().ok_or("expected float")?),
        DbType::Boolean => Value::Boolean(v.as_bool().ok_or("expected boolean")?),
        DbType::Binary => Value::Binary(serde_json::from_value(v.clone()).map_err(|e|format!("{e}"))?),
        DbType::Date => Value::Date(v.as_str().ok_or("expected date")?.parse().map_err(|e:chrono::ParseError|e.to_string())?),
        DbType::Time => Value::Time(v.as_str().ok_or("expected time")?.parse().map_err(|e:chrono::ParseError|e.to_string())?),
        DbType::DateTime => Value::DateTime(chrono::DateTime::parse_from_rfc3339(v.as_str().ok_or("expected datetime")?).map_err(|e|e.to_string())?.with_timezone(&chrono::Utc)),
    })
}
fn record(v:&Json, schema:&Schema)->Result<Record,String> {
    let mut result = Record::new();
    for c in schema.columns {
        let value = v.get(c.name).ok_or(format!("missing field {}",c.name))?;
        result.insert(c.name.into(),decode(value,c.kind)?);
    }
    Ok(result)
}
fn encode(v:&Value)->Json {
    match v {Value::Null=>Json::Null,Value::Boolean(v)=>json!(v),Value::Integer(v)=>json!(v),
        Value::Float(v)=>json!(v),Value::String(v)=>json!(v),Value::Binary(v)=>json!(v),
        Value::Date(v)=>json!(v.to_string()),Value::Time(v)=>json!(v.to_string()),Value::DateTime(v)=>json!(v.to_rfc3339())}
}
fn output(v:VersionedRecord)->Json {
    json!({"version":v.version,"record":v.record.iter().map(|(k,v)|(k.clone(),encode(v))).collect::<serde_json::Map<String,Json>>()})
}
const USAGE: &str = "\
usage: edodb -d DRIVER [--url URL] [--site ID] [--token TOKEN] [--set key=value]... -m METHOD -b JSON

drivers:     memory | sqlite | postgres | mariadb | mysql | sharepoint
methods:     ensure_schema | insert | get | find | update | delete
-b JSON      request body, e.g. '{\"schema\":{...},\"key\":\"p1\"}'; use -b - to read it from stdin
--token      prefer the EDODB_GRAPH_TOKEN environment variable

Prints one JSON object to stdout. Exit codes: 0 ok, 1 method failed, 2 usage error.";

/// Runs one method; `body` holds everything except the method itself.
async fn command(driver:&dyn Driver, op:&str, body:&Json)->Result<Json,String> {
    let schema = schema(body.get("schema").ok_or("schema missing")?)?;
    let key = || -> Result<Value,String> {
        let pk = schema.columns.iter().find(|c|c.primary_key).unwrap();
        decode(body.get("key").ok_or("key missing")?,pk.kind)
    };
    let version = || body.get("version").and_then(Json::as_str).ok_or("version missing");
    let res = match op {
        "ensure_schema" => { driver.ensure_schema(&schema).await.map_err(|e|e.to_string())?; json!({"ok":true}) },
        "insert" => { driver.insert(&schema,record(body.get("record").ok_or("record missing")?,&schema)?).await.map_err(|e|e.to_string())?;json!({"ok":true}) },
        "get" => { let v=driver.get(&schema,&key()?).await.map_err(|e|e.to_string())?;json!({"ok":true,"item":v.map(output)}) },
        "find" => {
            let filter = body.get("filter").map(|f| -> Result<Filter,String> {
                let field = f.get("field").and_then(Json::as_str).ok_or("filter.field missing")?;
                let col = schema.columns.iter().find(|c|c.name==field).ok_or("unknown filter field")?;
                Ok(Filter::Eq(field.into(),decode(f.get("eq").ok_or("filter.eq missing")?,col.kind)?))
            }).transpose()?;
            let query=Query {filter,limit:body.get("limit").and_then(Json::as_u64).map(|n|n as usize)};
            let items=driver.find(&schema,query).await.map_err(|e|e.to_string())?;
            json!({"ok":true,"items":items.into_iter().map(output).collect::<Vec<_>>()})
        },
        "update" => {
            let version=driver.update(&schema,&key()?,version()?,record(body.get("record").ok_or("record missing")?,&schema)?).await.map_err(|e|e.to_string())?;
            json!({"ok":true,"version":version}) },
        "delete" => { driver.delete(&schema,&key()?,version()?).await.map_err(|e|e.to_string())?;json!({"ok":true}) },
        _ => return Err(format!("unknown method: {op}")),
    };
    Ok(res)
}
fn usage_error(msg: impl std::fmt::Display) -> ! {
    eprintln!("edodb: {msg}\n\n{USAGE}");
    std::process::exit(2)
}
#[tokio::main]
async fn main() {
    let mut backend=None;let mut url=String::new();let mut site=String::new();let mut token=std::env::var("EDODB_GRAPH_TOKEN").unwrap_or_default();
    let mut op=None;let mut body=None;let mut opts=Vec::new();
    let mut args=std::env::args().skip(1);
    while let Some(arg)=args.next() {
        if arg=="-h"||arg=="--help" { println!("{USAGE}"); return; }
        let val=args.next().unwrap_or_else(|| usage_error(format!("{arg} needs a value")));
        match arg.as_str() {
            "-d"|"--driver"=>backend=Some(val),"--url"=>url=val,"--site"=>site=val,"--token"=>token=val,
            "-m"|"--method"=>op=Some(val),"-b"|"--body"=>body=Some(val),
            "--set"=>match val.split_once('=') { Some((k,v))=>opts.push((k.to_owned(),v.to_owned())), None=>usage_error(format!("invalid --set: {val}")) },
            _=>usage_error(format!("unknown argument: {arg}")),
        }
    }
    let backend=backend.unwrap_or_else(|| usage_error("-d DRIVER is required"));
    let op=op.unwrap_or_else(|| usage_error("-m METHOD is required"));
    let body=body.unwrap_or_else(|| usage_error("-b JSON is required"));
    let body=if body=="-" {
        let mut s=String::new();
        std::io::stdin().read_to_string(&mut s).unwrap_or_else(|e| usage_error(format!("reading stdin: {e}")));
        s
    } else { body };
    let body:Json=serde_json::from_str(&body).unwrap_or_else(|e| usage_error(format!("invalid JSON body: {e}")));
    let mut driver:Box<dyn Driver> = match backend.as_str() {
        "memory"=>Box::new(MemoryDriver::new()),
        "sqlite"|"postgres"|"mariadb"|"mysql"=>Box::new(SqlDriver::new(url)),
        "sharepoint"=>Box::new(SharePointDriver::new(site,token)),
        _=>usage_error(format!("unknown driver: {backend}")),
    };
    for (name,raw) in opts {
        let val=if let Ok(v)=raw.parse::<i64>() {Value::Integer(v)} else if raw=="true"||raw=="false" {Value::Boolean(raw=="true")} else {Value::String(raw)};
        match driver.set(&name,&val) {SetResult::Ok=>{},SetResult::Unknown=>eprintln!("warning: unknown option {name}"),SetResult::Invalid=>usage_error(format!("invalid option: {name}"))}
    }
    let mut db=Database::new(driver);
    if let Err(e)=db.connect().await { println!("{}",fail(format!("connect: {e}"))); std::process::exit(1) }
    match command(db.driver(),&op,&body).await {
        Ok(v)=>println!("{v}"),
        Err(e)=>{ println!("{}",fail(e)); std::process::exit(1) }
    }
}
