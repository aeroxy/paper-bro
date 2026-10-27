//! Configuration, read once from the environment at boot.
//!
//! Every knob has a default except the three credentials. `.env.example`
//! lists them all with their defaults.

use anyhow::{Context, Result, bail};
use std::net::SocketAddr;
use std::path::PathBuf;

/// The path prefix paper-bro is served under. The reverse proxy routes
/// `/paper-bro*` here without stripping it, so every route and asset URL
/// carries it.
pub const BASE: &str = "/paper-bro";

#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub baseten_key: String,
    pub model: String,
    pub fallback_model: String,
    pub aero_token: String,
    pub aero_mcp_url: String,
    /// Origin the published courses and paper-bro itself are served from.
    pub public_origin: String,
    /// When false, a finished course is kept in the database and served from
    /// `/paper-bro/preview/{alias}` instead of being published to AERO.
    pub publish: bool,
    pub chrome: PathBuf,
    pub listen: SocketAddr,
    pub admin_listen: SocketAddr,
    pub workers: usize,
    pub builds_per_ip_per_day: u32,
    pub builds_per_day: u32,
    pub max_queued: i64,
    pub daily_budget_usd: f64,
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn required(name: &str) -> Result<String> {
    var(name).with_context(|| format!("{name} is not set"))
}

fn parsed<T: std::str::FromStr>(name: &str, default: T) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    match var(name) {
        None => Ok(default),
        Some(v) => v.parse().map_err(|e| anyhow::anyhow!("{name}={v}: {e}")),
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let chrome = PathBuf::from(var("CHROME_PATH").unwrap_or_else(|| {
            ".chrome/chrome-headless-shell-mac-arm64/chrome-headless-shell".into()
        }));
        if !chrome.exists() {
            bail!("CHROME_PATH {} does not exist", chrome.display());
        }
        Ok(Self {
            database_url: required("NEON_DB")?,
            baseten_key: required("BASETEN_API_KEY")?,
            model: var("BASETEN_MODEL").unwrap_or_else(|| "meta-models/muse-spark-open".into()),
            fallback_model: var("BASETEN_FALLBACK_MODEL")
                .unwrap_or_else(|| "meta-models/muse-spark-open".into()),
            aero_token: required("AERO_TOKEN")?,
            aero_mcp_url: var("AERO_MCP_URL").unwrap_or_else(|| "https://aero.oops.wtf/mcp".into()),
            public_origin: var("PUBLIC_ORIGIN").unwrap_or_else(|| "https://aero.oops.wtf".into()),
            publish: parsed("PUBLISH", 0u8)? == 1,
            chrome,
            listen: parsed("LISTEN", "127.0.0.1:8083".parse()?)?,
            admin_listen: parsed("ADMIN_LISTEN", "127.0.0.1:8085".parse()?)?,
            workers: parsed("WORKERS", 2)?,
            builds_per_ip_per_day: parsed("BUILDS_PER_IP_PER_DAY", 3)?,
            builds_per_day: parsed("BUILDS_PER_DAY", 40)?,
            max_queued: parsed("MAX_QUEUED", 20)?,
            daily_budget_usd: parsed("DAILY_BUDGET_USD", 50.0)?,
        })
    }
}
