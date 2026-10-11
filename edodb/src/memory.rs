//! Functional in-memory reference driver, useful for tests and custom drivers.
use super::*;
use std::{collections::HashMap, sync::Mutex};

#[derive(Default)]
pub struct MemoryDriver {
    connected: bool,
    tables: Mutex<HashMap<String, Table>>,
}
#[derive(Default)]
struct Table {
    rows: HashMap<String, VersionedRecord>,
    revision: u64,
}
impl MemoryDriver { pub fn new() -> Self { Self::default() } }
fn key_string(value:&Value)->Result<String,DbError> {
    match value { Value::String(v)=>Ok(format!("s:{v}")), Value::Integer(v)=>Ok(format!("i:{v}")),
        _=>Err(DbError::Invalid("key must be string or integer".into())) }
}
fn primary(schema:&Schema)->Result<&'static str,DbError> {
    schema.columns.iter().find(|c|c.primary_key).map(|c|c.name)
        .ok_or_else(||DbError::Invalid("missing primary key".into()))
}
/// Orders primary keys like SQL does (keys are always string or integer).
fn cmp_key(a:&Value, b:&Value)->std::cmp::Ordering {
    match (a,b) { (Value::Integer(x),Value::Integer(y))=>x.cmp(y), (Value::String(x),Value::String(y))=>x.cmp(y),
        _=>std::cmp::Ordering::Equal }
}
/// Enforces `#[db(unique)]` like a SQL UNIQUE constraint: NULLs never collide.
fn check_unique(schema:&Schema, table:&Table, record:&Record, own_key:Option<&str>)->Result<(),DbError> {
    for col in schema.columns.iter().filter(|c|c.unique && !c.primary_key) {
        let Some(v)=record.get(col.name).filter(|v|!matches!(v,Value::Null)) else {continue};
        if table.rows.iter().any(|(k,row)| Some(k.as_str())!=own_key && row.record.get(col.name)==Some(v)) {
            return Err(DbError::Conflict);
        }
    }
    Ok(())
}
fn validate(schema:&Schema, record:&Record)->Result<(),DbError> {
    for col in schema.columns {
        let v=record.get(col.name).ok_or_else(||DbError::Invalid(format!("missing {}",col.name)))?;
        if matches!(v,Value::Null) { if !col.nullable {return Err(DbError::Invalid(format!("{} is not nullable",col.name)));} continue; }
        let kind=match v { Value::Null=>unreachable!(), Value::Boolean(_)=>DbType::Boolean,
            Value::Integer(_)=>DbType::Integer,Value::Float(_)=>DbType::Float,
            Value::String(_)=>DbType::String,Value::Binary(_)=>DbType::Binary,
            Value::Date(_)=>DbType::Date,Value::Time(_)=>DbType::Time,
            Value::DateTime(_)=>DbType::DateTime };
        if kind!=col.kind { return Err(DbError::Invalid(format!("wrong type for {}",col.name))); }
    }
    Ok(())
}
#[async_trait]
impl Driver for MemoryDriver {
    fn set(&mut self,_name:&str,_value:&Value)->SetResult { SetResult::Unknown }
    async fn connect(&mut self)->Result<(),DbError> { self.connected=true; Ok(()) }
    fn is_connected(&self)->bool { self.connected }
    async fn ensure_schema(&self,schema:&Schema)->Result<(),DbError> {
        if !self.connected {return Err(DbError::NotConnected);}
        self.tables.lock().unwrap().entry(schema.table.into()).or_default(); Ok(())
    }
    async fn insert(&self,schema:&Schema,record:Record)->Result<(),DbError> {
        if !self.connected {return Err(DbError::NotConnected);}
        validate(schema,&record)?;
        let key=key_string(record.get(primary(schema)?).unwrap())?;
        let mut tables=self.tables.lock().unwrap();
        let table=tables.get_mut(schema.table).ok_or_else(||DbError::Invalid("schema not created".into()))?;
        if table.rows.contains_key(&key) {return Err(DbError::Conflict);}
        check_unique(schema,table,&record,None)?;
        table.revision+=1;
        table.rows.insert(key,VersionedRecord{record,version:table.revision.to_string()}); Ok(())
    }
    async fn get(&self,schema:&Schema,key:&Value)->Result<Option<VersionedRecord>,DbError> {
        if !self.connected {return Err(DbError::NotConnected);}
        let tables=self.tables.lock().unwrap();
        Ok(tables.get(schema.table).ok_or_else(||DbError::Invalid("schema not created".into()))?
            .rows.get(&key_string(key)?).cloned())
    }
    async fn find(&self,schema:&Schema,query:Query)->Result<Vec<VersionedRecord>,DbError> {
        if !self.connected {return Err(DbError::NotConnected);}
        let tables=self.tables.lock().unwrap();
        let table=tables.get(schema.table).ok_or_else(||DbError::Invalid("schema not created".into()))?;
        let mut result:Vec<_>=table.rows.values().filter(|item|match &query.filter {
            None=>true,Some(Filter::Eq(name,v))=>item.record.get(name)==Some(v)
        }).cloned().collect();
        let pk=primary(schema)?;
        result.sort_by(|a,b| cmp_key(&a.record[pk],&b.record[pk]));
        if let Some(limit)=query.limit {result.truncate(limit);}
        Ok(result)
    }
    async fn update(&self,schema:&Schema,key:&Value,version:&str,record:Record)->Result<String,DbError> {
        if !self.connected {return Err(DbError::NotConnected);}
        validate(schema,&record)?;
        let key_str=key_string(key)?;
        if key_string(record.get(primary(schema)?).unwrap())? != key_str {
            return Err(DbError::Invalid("cannot change primary key".into()));
        }
        let mut tables=self.tables.lock().unwrap();
        let table=tables.get_mut(schema.table).ok_or(DbError::NotFound)?;
        let old=table.rows.get(&key_str).ok_or(DbError::NotFound)?;
        if old.version!=version {return Err(DbError::Conflict);}
        check_unique(schema,table,&record,Some(&key_str))?;
        table.revision+=1;
        let next=table.revision.to_string();
        table.rows.insert(key_str,VersionedRecord{record,version:next.clone()}); Ok(next)
    }
    async fn delete(&self,schema:&Schema,key:&Value,version:&str)->Result<(),DbError> {
        if !self.connected {return Err(DbError::NotConnected);}
        let mut tables=self.tables.lock().unwrap();
        let table=tables.get_mut(schema.table).ok_or(DbError::NotFound)?;
        let key=key_string(key)?;
        if table.rows.get(&key).ok_or(DbError::NotFound)?.version!=version {return Err(DbError::Conflict);}
        table.rows.remove(&key); Ok(())
    }
}
