//! Configuration runtime depuis l'environnement (après `dotenvy::dotenv()`).
//!
//! Tout a un défaut sensé : le binaire tourne sans `.env`.
//!
//! | Variable        | Défaut                                | Rôle                          |
//! |-----------------|---------------------------------------|-------------------------------|
//! | `ETH_WS_URL`    | `wss://ethereum-rpc.publicnode.com`   | WS mainnet (pending full tx)  |
//! | `DATABASE_URL`  | `sqlite://mev_intel.db`               | Base SQLite locale            |
//! | `MEV_STATS_SECS`| `5`                                   | Période des logs de stats     |

use eyre::{Context, Result};
use std::time::Duration;

/// URL WebSocket Ethereum mainnet par défaut (publicnode, gratuit, supporte
/// `subscribe_full_pending_transactions`).
pub const ETH_WS_DEFAULT: &str = "wss://ethereum-rpc.publicnode.com";
/// Base SQLite locale par défaut (créée si absente).
pub const DATABASE_URL_DEFAULT: &str = "sqlite://mev_intel.db";

/// Configuration validée.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Endpoint WebSocket Ethereum (pending full tx bodies).
    pub ws_url: String,
    /// URL de connexion sqlx (SQLite pour l'instant, Postgres en scale-path).
    pub database_url: String,
    /// Période d'émission des stats cumulées.
    pub stats_interval: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ws_url: ETH_WS_DEFAULT.to_string(),
            database_url: DATABASE_URL_DEFAULT.to_string(),
            stats_interval: Duration::from_secs(5),
        }
    }
}

impl Config {
    /// Charge la config depuis l'environnement. Variables absentes -> défauts.
    pub fn from_env() -> Result<Self> {
        let d = Self::default();
        Ok(Self {
            ws_url: std::env::var("ETH_WS_URL").unwrap_or(d.ws_url),
            database_url: std::env::var("DATABASE_URL").unwrap_or(d.database_url),
            stats_interval: parse_env_secs("MEV_STATS_SECS", d.stats_interval)?,
        })
    }
}

fn parse_env_secs(key: &str, default: Duration) -> Result<Duration> {
    match std::env::var(key) {
        Ok(v) => {
            let secs = v
                .parse::<u64>()
                .with_context(|| format!("{key}=\"{v}\" invalide (entier de secondes attendu)"))?;
            if secs == 0 {
                eyre::bail!("{key} doit être > 0");
            }
            Ok(Duration::from_secs(secs))
        }
        Err(_) => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let d = Config::default();
        assert_eq!(d.ws_url, ETH_WS_DEFAULT);
        assert_eq!(d.database_url, DATABASE_URL_DEFAULT);
        assert_eq!(d.stats_interval, Duration::from_secs(5));
    }
}
