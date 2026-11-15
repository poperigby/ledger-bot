use sqlx::{PgPool, postgres::PgPoolOptions};

use crate::config::{PostgresConfig, resolve_required_secret};

pub async fn connect(config: &PostgresConfig) -> anyhow::Result<PgPool> {
    let database_url =
        resolve_required_secret(config.database_url.as_ref(), "postgres.database_url")?;

    Ok(PgPoolOptions::new()
        .max_connections(config.max_connections)
        .connect(&database_url)
        .await?)
}
