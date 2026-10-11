//! A small driver-neutral CRUD foundation.
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use serde::{de::DeserializeOwned, Serialize};
use std::collections::BTreeMap;
pub use edodb_derive::DbModel;
#[doc(hidden)]
pub mod __private { pub use serde; }
pub mod memory;
pub mod sql;
pub mod sharepoint;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null, Boolean(bool), Integer(i64), Float(f64), String(String),
    Binary(Vec<u8>), Date(NaiveDate), Time(NaiveTime), DateTime(DateTime<Utc>),
}
impl From<i32> for Value { fn from(v:i32)->Self { Self::Integer(v as i64) } }
impl From<i64> for Value { fn from(v:i64)->Self { Self::Integer(v) } }
impl From<f32> for Value { fn from(v:f32)->Self { Self::Float(v as f64) } }
impl From<f64> for Value { fn from(v:f64)->Self { Self::Float(v) } }
impl From<bool> for Value { fn from(v:bool)->Self { Self::Boolean(v) } }
impl From<String> for Value { fn from(v:String)->Self { Self::String(v) } }
impl From<&str> for Value { fn from(v:&str)->Self { Self::String(v.into()) } }
impl From<Vec<u8>> for Value { fn from(v:Vec<u8>)->Self { Self::Binary(v) } }
impl From<NaiveDate> for Value { fn from(v:NaiveDate)->Self { Self::Date(v) } }
impl From<NaiveTime> for Value { fn from(v:NaiveTime)->Self { Self::Time(v) } }
impl From<DateTime<Utc>> for Value { fn from(v:DateTime<Utc>)->Self { Self::DateTime(v) } }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbType { Boolean, Integer, Float, String, Binary, Date, Time, DateTime }
#[derive(Debug, Clone, Copy)]
pub struct Column { pub name: &'static str, pub kind: DbType, pub nullable: bool, pub primary_key: bool, pub unique: bool }
#[derive(Debug, Clone, Copy)]
pub struct Schema { pub table: &'static str, pub columns: &'static [Column] }
pub trait FieldType { const KIND: DbType; const NULLABLE: bool = false; }
macro_rules! field_type { ($t:ty,$v:ident) => { impl FieldType for $t { const KIND: DbType = DbType::$v; } }; }
field_type!(bool,Boolean); field_type!(i32,Integer); field_type!(i64,Integer);
field_type!(f32,Float); field_type!(f64,Float); field_type!(String,String);
field_type!(Vec<u8>,Binary); field_type!(NaiveDate,Date);
field_type!(NaiveTime,Time); field_type!(DateTime<Utc>,DateTime);
impl<T:FieldType> FieldType for Option<T> { const KIND: DbType = T::KIND; const NULLABLE: bool = true; }
pub trait Model: Serialize + DeserializeOwned + Send + Sync { const SCHEMA: Schema; }
pub type Record = BTreeMap<String,Value>;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetResult { Ok, Unknown, Invalid }
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("not connected")] NotConnected,
    #[error("not found")] NotFound,
    #[error("conflict")] Conflict,
    #[error("unsupported: {0}")] Unsupported(String),
    #[error("invalid: {0}")] Invalid(String),
    #[error("backend: {0}")] Backend(String),
}
#[derive(Debug, Clone)]
pub enum Filter { Eq(String, Value) }
#[derive(Debug, Default, Clone)]
pub struct Query { pub filter: Option<Filter>, pub limit: Option<usize> }
#[derive(Debug, Clone)]
pub struct VersionedRecord { pub record: Record, pub version: String }

#[async_trait]
pub trait Driver: Send + Sync {
    fn set(&mut self, name:&str, value:&Value)->SetResult;
    async fn connect(&mut self)->Result<(),DbError>;
    fn is_connected(&self)->bool;
    async fn ensure_schema(&self, schema:&Schema)->Result<(),DbError>;
    async fn insert(&self, schema:&Schema, record:Record)->Result<(),DbError>;
    async fn get(&self, schema:&Schema, key:&Value)->Result<Option<VersionedRecord>,DbError>;
    async fn find(&self, schema:&Schema, query:Query)->Result<Vec<VersionedRecord>,DbError>;
    async fn update(&self, schema:&Schema, key:&Value, version:&str, record:Record)->Result<String,DbError>;
    async fn delete(&self, schema:&Schema, key:&Value, version:&str)->Result<(),DbError>;
}

pub struct Database { driver: Box<dyn Driver> }
impl Database {
    pub fn new(driver:Box<dyn Driver>)->Self { Self {driver} }
    pub async fn connect(&mut self)->Result<(),DbError> {
        if !self.driver.is_connected() { self.driver.connect().await?; }
        Ok(())
    }
    pub fn is_connected(&self)->bool { self.driver.is_connected() }
    /// Read-only access to the hidden driver API; configuration remains unavailable.
    pub fn driver(&self)->&dyn Driver { &*self.driver }
    pub async fn ensure_schema<M:Model>(&self)->Result<(),DbError> { self.driver.ensure_schema(&M::SCHEMA).await }
    pub async fn insert<M:Model>(&self, model:&M)->Result<(),DbError> {
        self.driver.insert(&M::SCHEMA, to_record(model)?).await
    }
    pub async fn get<M:Model>(&self, key:impl Into<Value>)->Result<Option<(M,String)>,DbError> {
        self.driver.get(&M::SCHEMA,&key.into()).await?.map(from_record::<M>).transpose()
    }
    pub async fn find<M:Model>(&self, query:Query)->Result<Vec<(M,String)>,DbError> {
        self.driver.find(&M::SCHEMA,query).await?.into_iter().map(from_record::<M>).collect()
    }
    pub async fn update<M:Model>(&self, key:impl Into<Value>, version:&str, model:&M)->Result<String,DbError> {
        self.driver.update(&M::SCHEMA,&key.into(),version,to_record(model)?).await
    }
    pub async fn delete<M:Model>(&self, key:impl Into<Value>, version:&str)->Result<(),DbError> {
        self.driver.delete(&M::SCHEMA,&key.into(),version).await
    }
}
fn to_record<M:Model>(model:&M)->Result<Record,DbError> {
    let json=serde_json::to_value(model).map_err(|e|DbError::Invalid(e.to_string()))?;
    let obj=json.as_object().ok_or_else(||DbError::Invalid("expected object".into()))?;
    M::SCHEMA.columns.iter().map(|c| {
        let v=obj.get(c.name).ok_or_else(||DbError::Invalid(format!("missing {}",c.name)))?;
        Ok((c.name.to_string(),from_json(v,c.kind)?))
    }).collect()
}
fn from_json(v:&serde_json::Value, kind:DbType)->Result<Value,DbError> {
    if v.is_null() { return Ok(Value::Null); }
    let invalid=||DbError::Invalid(format!("bad {:?}: {v}",kind));
    match kind {
        DbType::Boolean=>Ok(Value::Boolean(v.as_bool().ok_or_else(invalid)?)),
        DbType::Integer=>Ok(Value::Integer(v.as_i64().ok_or_else(invalid)?)),
        DbType::Float=>Ok(Value::Float(v.as_f64().ok_or_else(invalid)?)),
        DbType::String=>Ok(Value::String(v.as_str().ok_or_else(invalid)?.into())),
        DbType::Binary=>serde_json::from_value::<Vec<u8>>(v.clone()).map(Value::Binary).map_err(|e|DbError::Invalid(e.to_string())),
        DbType::Date=>v.as_str().ok_or_else(invalid)?.parse().map(Value::Date).map_err(|e|DbError::Invalid(e.to_string())),
        DbType::Time=>v.as_str().ok_or_else(invalid)?.parse().map(Value::Time).map_err(|e|DbError::Invalid(e.to_string())),
        DbType::DateTime=>DateTime::parse_from_rfc3339(v.as_str().ok_or_else(invalid)?)
            .map(|d|Value::DateTime(d.with_timezone(&Utc))).map_err(|e|DbError::Invalid(e.to_string())),
    }
}
fn to_json(v:&Value)->serde_json::Value {
    match v {
        Value::Null=>serde_json::Value::Null, Value::Boolean(v)=>(*v).into(),
        Value::Integer(v)=>(*v).into(), Value::Float(v)=>serde_json::json!(v),
        Value::String(v)=>v.clone().into(), Value::Binary(v)=>serde_json::json!(v),
        Value::Date(v)=>v.to_string().into(), Value::Time(v)=>v.to_string().into(),
        Value::DateTime(v)=>v.to_rfc3339().into(),
    }
}
fn from_record<M:Model>(item:VersionedRecord)->Result<(M,String),DbError> {
    let json=serde_json::Value::Object(item.record.iter().map(|(k,v)|(k.clone(),to_json(v))).collect());
    let model=serde_json::from_value(json).map_err(|e|DbError::Invalid(e.to_string()))?;
    Ok((model,item.version))
}
