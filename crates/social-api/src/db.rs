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
        let writer = PgPoolOptions::new()
            .max_connections(config.db_max_connections)
            .min_connections(config.db_min_connections)
            .acquire_timeout(Duration::from_secs(config.db_acquire_timeout_secs))
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

    /// Check if the database is reachable (for health checks).
    pub async fn is_healthy(&self) -> bool {
        sqlx::query("SELECT 1").execute(&self.writer).await.is_ok()
    }
}
