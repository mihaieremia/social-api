//! Shared test infrastructure for crate-local tests.
//!
//! When shared Postgres/Redis test services are already running locally, these
//! helpers reuse them. Otherwise they fall back to spawning testcontainers.

use redis::AsyncCommands;
use sqlx::Connection;
use sqlx::PgConnection;
use testcontainers::runners::AsyncRunner;
use tokio::sync::OnceCell;

const DEFAULT_TEST_DATABASE_URL: &str =
    "postgres://social:social_password@localhost:5432/social_api";
const DEFAULT_TEST_REDIS_URL: &str = "redis://localhost:6379";
const TEST_RESET_LOCK_ID: i64 = 8_420_001;

// ── Postgres ─────────────────────────────────────────────────────────────────

pub struct SharedPg {
    _container: Option<testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>>,
    pub url: String,
}

static PG: OnceCell<SharedPg> = OnceCell::const_new();

/// Returns a long-lived Postgres container.  Migrations are run once on first
/// call; subsequent calls return the cached reference instantly.
pub async fn shared_pg() -> &'static SharedPg {
    PG.get_or_init(|| async {
        if let Some(url) = available_postgres_url().await {
            run_migrations(&url).await;
            return SharedPg {
                _container: None,
                url,
            };
        }

        let pg = testcontainers_modules::postgres::Postgres::default()
            .start()
            .await
            .expect("postgres container");
        let port = pg.get_host_port_ipv4(5432).await.unwrap();
        let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

        run_migrations(&url).await;

        SharedPg {
            _container: Some(pg),
            url,
        }
    })
    .await
}

// ── Redis ────────────────────────────────────────────────────────────────────

pub struct SharedRedis {
    _container: Option<testcontainers::ContainerAsync<testcontainers_modules::redis::Redis>>,
    pub url: String,
}

static REDIS: OnceCell<SharedRedis> = OnceCell::const_new();

pub struct TestScope {
    pub database_url: String,
    pub redis_url: String,
    _lock_conn: PgConnection,
}

/// Returns a long-lived Redis container.  First call starts it; subsequent
/// calls return the cached reference instantly.
pub async fn shared_redis() -> &'static SharedRedis {
    REDIS
        .get_or_init(|| async {
            if let Some(url) = available_redis_url().await {
                return SharedRedis {
                    _container: None,
                    url,
                };
            }

            let redis = testcontainers_modules::redis::Redis::default()
                .start()
                .await
                .expect("redis container");
            let port = redis.get_host_port_ipv4(6379).await.unwrap();
            let url = format!("redis://127.0.0.1:{port}");

            SharedRedis {
                _container: Some(redis),
                url,
            }
        })
        .await
}

pub async fn isolated_scope() -> TestScope {
    let pg = shared_pg().await;
    let redis = shared_redis().await;

    let mut lock_conn = PgConnection::connect(&pg.url)
        .await
        .expect("test lock connection");

    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(TEST_RESET_LOCK_ID)
        .execute(&mut lock_conn)
        .await
        .expect("acquire test advisory lock");

    reset_postgres(&mut lock_conn).await;
    reset_redis(&redis.url).await;

    TestScope {
        database_url: pg.url.clone(),
        redis_url: redis.url.clone(),
        _lock_conn: lock_conn,
    }
}

async fn available_postgres_url() -> Option<String> {
    let url =
        std::env::var("TEST_DATABASE_URL").unwrap_or_else(|_| DEFAULT_TEST_DATABASE_URL.into());

    match sqlx::PgPool::connect(&url).await {
        Ok(pool) => {
            pool.close().await;
            Some(url)
        }
        Err(_) => None,
    }
}

async fn available_redis_url() -> Option<String> {
    let url = std::env::var("TEST_REDIS_URL").unwrap_or_else(|_| DEFAULT_TEST_REDIS_URL.into());

    match redis::Client::open(url.clone()) {
        Ok(client) => match client.get_multiplexed_async_connection().await {
            Ok(mut conn) => match conn.ping::<String>().await {
                Ok(_) => Some(url),
                Err(_) => None,
            },
            Err(_) => None,
        },
        Err(_) => None,
    }
}

async fn run_migrations(url: &str) {
    let pool = sqlx::PgPool::connect(url).await.expect("pg connect");
    sqlx::migrate!("../../migrations")
        .run(&pool)
        .await
        .expect("migrations");
    pool.close().await;
}

async fn reset_postgres(conn: &mut PgConnection) {
    let tables: Option<String> = sqlx::query_scalar(
        r#"
        SELECT string_agg(format('%I.%I', schemaname, tablename), ', ')
        FROM pg_tables
        WHERE schemaname = 'public'
          AND tablename <> '_sqlx_migrations'
        "#,
    )
    .fetch_one(&mut *conn)
    .await
    .expect("list postgres test tables");

    if let Some(tables) = tables {
        let truncate = format!("TRUNCATE TABLE {tables} RESTART IDENTITY CASCADE");
        sqlx::query(&truncate)
            .execute(&mut *conn)
            .await
            .expect("truncate postgres test tables");
    }
}

async fn reset_redis(redis_url: &str) {
    let client = redis::Client::open(redis_url).expect("redis client for test reset");
    let mut conn = client
        .get_multiplexed_async_connection()
        .await
        .expect("redis connection for test reset");
    conn.flushdb::<()>().await.expect("flush redis test db");
}
