#[cfg(not(any(
    feature = "strict-postgres",
    feature = "strict-mysql",
    feature = "strict-sqlite"
)))]
pub use sqlx::AnyPool as RullstPool;

#[cfg(feature = "strict-postgres")]
pub use sqlx::PgPool as RullstPool;

#[cfg(all(feature = "strict-mysql", not(feature = "strict-postgres")))]
pub use sqlx::MySqlPool as RullstPool;

#[cfg(all(
    feature = "strict-sqlite",
    not(feature = "strict-postgres"),
    not(feature = "strict-mysql")
))]
pub use sqlx::SqlitePool as RullstPool;

#[cfg(not(any(
    feature = "strict-postgres",
    feature = "strict-mysql",
    feature = "strict-sqlite"
)))]
use sqlx::any::install_default_drivers;

use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

// Hide underlying libraries for macro usage while keeping the public API clean
#[doc(hidden)]
pub use futures as _futures;
#[doc(hidden)]
pub use serde as _serde;
#[doc(hidden)]
pub use serde_json as _serde_json;
#[doc(hidden)]
pub use sqlx as _sqlx;

#[cfg(feature = "redis")]
#[doc(hidden)]
pub use redis as _redis;
pub mod admin;
pub mod audit;
pub mod collection;
pub mod database;
pub mod db;
pub mod error;
pub mod resource;
pub mod schema;
pub mod scout;
pub mod tenant;
pub mod types;

// Export the custom Error enum to the root
pub use error::RullstError as Error;

// Re-exports
pub use _sqlx::FromRow;
pub use admin::dashboard_html;
pub use collection::RullstCollection;
pub use database::RullstDatabase;
pub use resource::{ApiResource, JsonResource, ResourceCollection};
pub use rullst_orm_macros::Orm;
pub use scout::{SearchEngine, get_search_engine, set_search_engine};
pub use tenant::{cond_mentions_column, get_tenant_id, with_tenant};
pub use types::Json;

// Re-export async_trait so the macro can use it implicitly
pub use async_trait::async_trait;

// Re-export sqlx and FromRow for database mapping
pub use schema::{JoinClause, SubqueryBuilder};

/// The global connection pool
static DB_POOL: OnceLock<RullstPool> = OnceLock::new();

/// The driver identifier (postgres, mysql, sqlite) to help macro syntax formatting
static DB_DRIVER: OnceLock<String> = OnceLock::new();

/// The replica connection pools for read operations
static REPLICA_POOLS: OnceLock<Vec<RullstPool>> = OnceLock::new();

/// Atomic index for replica round-robin selection
static REPLICA_INDEX: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "redis")]
static REDIS_CLIENT: OnceLock<_redis::Client> = OnceLock::new();

#[cfg(feature = "redis")]
static REDIS_MANAGER: OnceLock<_redis::aio::ConnectionManager> = OnceLock::new();

/// Enum dinÃ¢mico para encapsular qualquer tipo que possa ser associado ao banco de dados pelo Macro
#[derive(Clone, Debug)]
pub enum RullstValue {
    String(String),
    Int(i32),
    Float(f64),
    Bool(bool),
}

impl From<&str> for RullstValue {
    fn from(s: &str) -> Self {
        RullstValue::String(s.to_string())
    }
}
impl From<String> for RullstValue {
    fn from(s: String) -> Self {
        RullstValue::String(s)
    }
}
impl From<i32> for RullstValue {
    fn from(i: i32) -> Self {
        RullstValue::Int(i)
    }
}
impl From<f64> for RullstValue {
    fn from(f: f64) -> Self {
        RullstValue::Float(f)
    }
}
impl From<bool> for RullstValue {
    fn from(b: bool) -> Self {
        RullstValue::Bool(b)
    }
}

impl TryFrom<RullstValue> for String {
    type Error = &'static str;
    fn try_from(val: RullstValue) -> Result<Self, Self::Error> {
        match val {
            RullstValue::String(s) => Ok(s),
            _ => Err("Not a string"),
        }
    }
}
impl TryFrom<RullstValue> for i32 {
    type Error = &'static str;
    fn try_from(val: RullstValue) -> Result<Self, Self::Error> {
        match val {
            RullstValue::Int(i) => Ok(i),
            _ => Err("Not an i32"),
        }
    }
}
impl TryFrom<RullstValue> for f64 {
    type Error = &'static str;
    fn try_from(val: RullstValue) -> Result<Self, Self::Error> {
        match val {
            RullstValue::Float(f) => Ok(f),
            _ => Err("Not an f64"),
        }
    }
}
impl TryFrom<RullstValue> for bool {
    type Error = &'static str;
    fn try_from(val: RullstValue) -> Result<Self, Self::Error> {
        match val {
            RullstValue::Bool(b) => Ok(b),
            _ => Err("Not a bool"),
        }
    }
}

/// Orm configuration structure
pub struct Orm;

impl Orm {
    /// Initialize the global database connection pool using an agnostic URI
    pub async fn init(database_url: &str) -> Result<(), crate::Error> {
        #[cfg(not(any(
            feature = "strict-postgres",
            feature = "strict-mysql",
            feature = "strict-sqlite"
        )))]
        install_default_drivers();

        let pool = RullstPool::connect(database_url).await?;

        if DB_POOL.set(pool).is_err() {
            return Err(crate::Error::Internal(
                "Orm has already been initialized".to_string(),
            ));
        }

        let driver = if database_url.starts_with("postgres") {
            "postgres"
        } else if database_url.starts_with("mysql") {
            "mysql"
        } else {
            "sqlite"
        };

        let _ = DB_DRIVER.set(driver.to_string());
        let _ = REPLICA_POOLS.set(vec![]);

        Ok(())
    }

    /// Initialize the global database connection pool and its read replicas
    pub async fn init_with_replicas(
        primary_url: &str,
        replica_urls: Vec<&str>,
    ) -> Result<(), crate::Error> {
        #[cfg(not(any(
            feature = "strict-postgres",
            feature = "strict-mysql",
            feature = "strict-sqlite"
        )))]
        install_default_drivers();

        let pool = RullstPool::connect(primary_url).await?;

        if DB_POOL.set(pool).is_err() {
            return Err(crate::Error::Internal(
                "Orm has already been initialized".to_string(),
            ));
        }

        let driver = if primary_url.starts_with("postgres") {
            "postgres"
        } else if primary_url.starts_with("mysql") {
            "mysql"
        } else {
            "sqlite"
        };

        let _ = DB_DRIVER.set(driver.to_string());

        // Initialize all replica pools concurrently — each connect() is independent I/O.
        let replica_futures: Vec<_> = replica_urls.into_iter().map(RullstPool::connect).collect();
        let replicas = futures::future::try_join_all(replica_futures).await?;
        let _ = REPLICA_POOLS.set(replicas);

        Ok(())
    }

    /// Retrieve the global database connection pool (strictly for writes)
    pub fn pool() -> &'static RullstPool {
        DB_POOL
            .get()
            .expect("Orm must be initialized before querying")
    }

    /// Retrieve the connection pool for read operations.
    /// Performs a round-robin load balancing over replicas if configured.
    pub fn read_pool() -> &'static RullstPool {
        if let Some(replicas) = REPLICA_POOLS.get()
            && !replicas.is_empty()
        {
            let idx = REPLICA_INDEX.fetch_add(1, Ordering::Relaxed) % replicas.len();
            return &replicas[idx];
        }
        Self::pool()
    }

    /// Retrieve the active driver string
    pub fn driver() -> &'static str {
        DB_DRIVER
            .get()
            .expect("Orm must be initialized before querying")
            .as_str()
    }

    pub async fn begin_transaction() -> Result<crate::db::Transaction<'static>, crate::Error> {
        let pool = Self::pool();
        pool.begin().await.map_err(Into::into)
    }

    /// Run an array of seeders sequentially
    pub async fn seed(seeders: Vec<Box<dyn Seeder>>) -> Result<(), crate::Error> {
        for seeder in seeders {
            seeder.run().await?;
        }
        Ok(())
    }

    /// Enable query logging to print all queries to the terminal
    pub fn enable_query_log() {
        crate::schema::enable_query_log();
    }

    /// Disable query logging
    pub fn disable_query_log() {
        crate::schema::disable_query_log();
    }

    /// Initialize Redis connection and connection manager for caching and events
    #[cfg(feature = "redis")]
    pub async fn init_redis(redis_url: &str) -> Result<(), crate::Error> {
        let client = _redis::Client::open(redis_url)?;
        let manager = _redis::aio::ConnectionManager::new(client.clone()).await?;
        let _ = REDIS_CLIENT.set(client);
        let _ = REDIS_MANAGER.set(manager);
        Ok(())
    }

    /// Get reference to the global Redis client
    #[cfg(feature = "redis")]
    pub fn redis_client() -> Result<&'static _redis::Client, crate::Error> {
        REDIS_CLIENT.get().ok_or_else(|| {
            crate::Error::Internal(
                "Orm::init_redis() must be called before using cache features".to_string(),
            )
        })
    }

    /// Get clone of the thread-safe connection manager for async Redis queries
    #[cfg(feature = "redis")]
    pub fn redis_manager() -> Result<_redis::aio::ConnectionManager, crate::Error> {
        REDIS_MANAGER.get().cloned().ok_or_else(|| {
            crate::Error::Internal(
                "Orm::init_redis() must be called before using cache features".to_string(),
            )
        })
    }
}

/// A database seeder trait for populating tables
#[async_trait]
pub trait Seeder: Send + Sync {
    async fn run(&self) -> Result<(), crate::Error>;
}

/// The core trait that all Orm models will implement via #[derive(Orm)]
#[async_trait]
pub trait RullstModel {
    fn table_name() -> &'static str;
}

/// Represents a paginated result set
#[derive(Debug, Clone)]
pub struct PaginationResult<T> {
    pub data: Vec<T>,
    pub total: i64,
    pub per_page: usize,
    pub current_page: usize,
    pub last_page: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pagination_result() {
        let mut pr = PaginationResult {
            data: vec![1, 2, 3],
            total: 3,
            per_page: 10,
            current_page: 1,
            last_page: 1,
        };
        assert_eq!(pr.data.len(), 3);
        assert_eq!(pr.total, 3);
        pr.data.push(4);
        assert_eq!(pr.data.len(), 4);
    }

    #[test]
    fn test_rullst_value_conversions() {
        let v: RullstValue = "test".into();
        assert!(matches!(v, RullstValue::String(_)));
        let v_int: RullstValue = 100.into();
        assert!(matches!(v_int, RullstValue::Int(100)));
        let v_bool: RullstValue = false.into();
        assert!(matches!(v_bool, RullstValue::Bool(false)));
    }

    #[test]
    fn test_enable_query_log_wrapper() {
        // Orm::enable/disable_query_log delegate to schema — verify the delegation works.
        Orm::disable_query_log();
        assert!(!crate::schema::is_query_log_enabled());
        Orm::enable_query_log();
        assert!(crate::schema::is_query_log_enabled());
        Orm::disable_query_log();
        assert!(!crate::schema::is_query_log_enabled());
    }

    #[test]
    fn test_disable_query_log_wrapper() {
        Orm::enable_query_log();
        Orm::disable_query_log();
        assert!(!crate::schema::is_query_log_enabled());
    }

    #[cfg(feature = "redis")]
    #[test]
    fn test_redis_client_uninitialized() {
        let err = Orm::redis_client().unwrap_err();
        assert!(matches!(err, crate::Error::Internal(_)));
    }

    #[cfg(feature = "redis")]
    #[test]
    fn test_redis_manager_uninitialized() {
        let err = Orm::redis_manager().unwrap_err();
        assert!(matches!(err, crate::Error::Internal(_)));
    }
}
