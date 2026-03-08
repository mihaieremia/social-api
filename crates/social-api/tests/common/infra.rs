use social_api::cache::{CacheManager, create_pool};
use social_api::config::Config;
use social_api::db::DbPools;

use redis::AsyncCommands;
use sqlx::Connection;
use sqlx::PgConnection;
use tokio::sync::OnceCell;

const TEST_RESET_LOCK_ID: i64 = 8_420_001;
const DEFAULT_TEST_DATABASE_URL: &str =
    "postgres://social:social_password@localhost:5432/social_api";
const DEFAULT_TEST_REDIS_URL: &str = "redis://localhost:6379";

static MIGRATIONS: OnceCell<()> = OnceCell::const_new();

pub struct TestContext {
    #[allow(dead_code)]
    pub config: Config,
    pub db: DbPools,
    pub cache: CacheManager,
    _lock_conn: PgConnection,
}

impl TestContext {
    pub async fn new() -> Self {
        let config = resolved_test_config();

        ensure_migrations(&config.database_url).await;

        let db = DbPools::from_config(&config).await.expect("db pools");
        let redis_pool = create_pool(&config).await.expect("redis pool");
        let cache = CacheManager::new(redis_pool, &config);

        let mut lock_conn = PgConnection::connect(&config.database_url)
            .await
            .expect("test lock connection");

        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(TEST_RESET_LOCK_ID)
            .execute(&mut lock_conn)
            .await
            .expect("acquire test advisory lock");

        reset_postgres(&db).await;
        reset_redis(&config.redis_url).await;

        Self {
            config,
            db,
            cache,
            _lock_conn: lock_conn,
        }
    }
}

fn resolved_test_config() -> Config {
    let mut config = Config::new_for_test();

    config.database_url =
        std::env::var("TEST_DATABASE_URL").unwrap_or_else(|_| DEFAULT_TEST_DATABASE_URL.into());
    config.read_database_url =
        std::env::var("TEST_READ_DATABASE_URL").unwrap_or_else(|_| config.database_url.clone());
    config.redis_url =
        std::env::var("TEST_REDIS_URL").unwrap_or_else(|_| DEFAULT_TEST_REDIS_URL.into());

    config
}

async fn ensure_migrations(database_url: &str) {
    MIGRATIONS
        .get_or_init(|| async move {
            let pool = sqlx::PgPool::connect(database_url)
                .await
                .expect("connect migrations postgres");
            sqlx::migrate!("../../migrations")
                .run(&pool)
                .await
                .expect("run test migrations");
            pool.close().await;
        })
        .await;
}

async fn reset_postgres(db: &DbPools) {
    let tables: Option<String> = sqlx::query_scalar(
        r#"
        SELECT string_agg(format('%I.%I', schemaname, tablename), ', ')
        FROM pg_tables
        WHERE schemaname = 'public'
          AND tablename <> '_sqlx_migrations'
        "#,
    )
    .fetch_one(&db.writer)
    .await
    .expect("list postgres test tables");

    if let Some(tables) = tables {
        let truncate = format!("TRUNCATE TABLE {tables} RESTART IDENTITY CASCADE");
        sqlx::query(&truncate)
            .execute(&db.writer)
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
