use sqlx::postgres::{PgPool, PgPoolOptions};
use std::time::Duration;

use crate::config::Config;

/// Database connection pools.
///
/// Separates writer and reader pools to support read replica routing.
/// Writer pool: mutations (INSERT, UPDATE, DELETE) -> primary
/// Reader pool: reads (SELECT) -> read replica
#[derive(Clone)]
pub struct DbPools {
    pub writer: PgPool,
    pub reader: PgPool,
}

impl DbPools {
    /// Create writer and reader pools from config.
    pub async fn from_config(config: &Config) -> Result<Self, sqlx::Error> {
        // Disable test_before_acquire to eliminate ~20k extra SELECT 1 queries/sec
        // at 10k RPS. Stale connections fail on first use and are transparently
        // reconnected by SQLx. idle_timeout + max_lifetime provide safety nets.
        let writer = PgPoolOptions::new()
            .max_connections(config.db_max_connections)
            .min_connections(config.db_min_connections)
            .acquire_timeout(Duration::from_secs(config.db_acquire_timeout_secs))
            .test_before_acquire(false)
            .idle_timeout(Duration::from_secs(300))
            .max_lifetime(Duration::from_secs(1800))
            .after_connect(|_conn, _meta| {
                Box::pin(async move {
                    tracing::debug!("New writer DB connection established");
                    Ok(())
                })
            })
            .connect(&config.database_url)
            .await?;

        let reader = PgPoolOptions::new()
            .max_connections(config.db_max_connections)
            .min_connections(config.db_min_connections)
            .acquire_timeout(Duration::from_secs(config.db_acquire_timeout_secs))
            .test_before_acquire(false)
            .idle_timeout(Duration::from_secs(300))
            .max_lifetime(Duration::from_secs(1800))
            .after_connect(|_conn, _meta| {
                Box::pin(async move {
                    tracing::debug!("New reader DB connection established");
                    Ok(())
                })
            })
            .connect(&config.read_database_url)
            .await?;

        tracing::info!(
            writer_max = config.db_max_connections,
            reader_max = config.db_max_connections,
            "Database pools initialized"
        );

        Ok(Self { writer, reader })
    }

    /// Run embedded migrations against the writer pool.
    pub async fn run_migrations(&self) -> Result<(), sqlx::migrate::MigrateError> {
        tracing::info!("Running database migrations...");
        sqlx::migrate!("../../migrations").run(&self.writer).await?;
        tracing::info!("Database migrations complete");
        Ok(())
    }

    /// Check if BOTH writer and reader pools are reachable.
    /// A broken read replica must fail the readiness probe — reads would be broken.
    pub async fn is_healthy(&self) -> bool {
        let (writer_ok, reader_ok) = tokio::join!(
            sqlx::query("SELECT 1").execute(&self.writer),
            sqlx::query("SELECT 1").execute(&self.reader),
        );
        writer_ok.is_ok() && reader_ok.is_ok()
    }
}
