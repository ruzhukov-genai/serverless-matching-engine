//! Configuration — loaded from environment variables with sensible defaults.

use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub dragonfly_url: String,
    pub database_url: String,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            dragonfly_url: env::var("DRAGONFLY_URL")
                .unwrap_or_else(|_| "redis://localhost:6379".to_string()),
            database_url: env::var("DATABASE_URL")
                .unwrap_or_else(|_| {
                    "postgres://sme:sme_dev@localhost:5432/matching_engine".to_string()
                }),
        }
    }
}
