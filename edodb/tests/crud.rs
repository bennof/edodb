use edodb::{DbModel, Database, DbError, Filter, Query, Value, memory::MemoryDriver, sql::SqlDriver};
use serde::{Serialize, Deserialize};
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, DbModel)]
#[db(table="test_items")]
struct Item { #[db(primary_key)] id: String, name: String, count: i64, active: bool }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, DbModel)]
#[db(table="test_wrapped")]
struct Wrapped<T> { #[db(primary_key)] id: i64, #[db(unique)] code: String, value: T }

async fn suite(mut db: Database) {
    db.connect().await.unwrap();
    db.connect().await.unwrap(); // idempotent
    db.ensure_schema::<Item>().await.unwrap();
    let item = Item {id:"a".into(),name:"alpha".into(),count:42,active:true};
    db.insert(&item).await.unwrap();
    assert!(matches!(db.insert(&item).await, Err(DbError::Conflict)));
    let (read, version) = db.get::<Item>("a").await.unwrap().unwrap();
    assert_eq!(read,item);
    let results=db.find::<Item>(Query{filter:Some(Filter::Eq("active".into(),Value::Boolean(true))),limit:Some(1)}).await.unwrap();
    assert_eq!(results.len(),1);
    let mut changed=item.clone();changed.count=43;
    let new_version=db.update("a",&version,&changed).await.unwrap();
    assert_ne!(new_version,version);
    assert!(matches!(db.update("a",&version,&changed).await,Err(DbError::Conflict)));
    assert!(matches!(db.delete::<Item>("a",&version).await,Err(DbError::Conflict)));
    db.delete::<Item>("a",&new_version).await.unwrap();
    assert!(db.get::<Item>("a").await.unwrap().is_none());
}
#[tokio::test]
async fn memory_crud() {suite(Database::new(Box::new(MemoryDriver::new()))).await;}
#[tokio::test]
async fn sqlite_crud() {
    let path=std::env::temp_dir().join(format!("edodb-test-{}-{}.db",std::process::id(),chrono::Utc::now().timestamp_nanos_opt().unwrap()));
    std::fs::File::create(&path).unwrap();
    let url=format!("sqlite://{}",path.display());
    suite(Database::new(Box::new(SqlDriver::new(url)))).await;
    let _=std::fs::remove_file(path);
}
#[tokio::test]
async fn memory_generic_unique_and_order() {
    let mut db=Database::new(Box::new(MemoryDriver::new()));
    db.connect().await.unwrap();
    db.ensure_schema::<Wrapped<f64>>().await.unwrap();
    for (id,code) in [(10,"x"),(9,"y"),(2,"z")] { db.insert(&Wrapped{id,code:code.into(),value:1.5}).await.unwrap(); }
    assert!(matches!(db.insert(&Wrapped{id:3,code:"x".into(),value:0.0}).await, Err(DbError::Conflict)));
    let (_, v)=db.get::<Wrapped<f64>>(9).await.unwrap().unwrap();
    assert!(matches!(db.update(9,&v,&Wrapped{id:9,code:"z".into(),value:0.0}).await, Err(DbError::Conflict)));
    let ids:Vec<i64>=db.find::<Wrapped<f64>>(Query::default()).await.unwrap().into_iter().map(|(w,_)|w.id).collect();
    assert_eq!(ids,vec![2,9,10]);
}
