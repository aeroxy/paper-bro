//! Configuration, read once from the environment at boot.
//!
//! Every knob has a default except the credentials and the model API's URL and
//! name. `.env.example` lists them all with their defaults.

use anyhow::{Context, Result, bail};
use std::net::SocketAddr;
use std::path::PathBuf;

/// The path prefix paper-bro is served under. The reverse proxy routes
/// `/paper-bro*` here without stripping it, so every route and asset URL
/// carries it.
pub const BASE: &str = "/paper-bro";

/// The OpenAI-compatible chat-completions endpoint a build calls.
#[derive(Clone, Debug)]
pub struct Endpoint {
    /// The endpoint's host, for messages.
    pub name: String,
    /// The full chat-completions URL.
    pub url: String,
    /// Sent as a bearer token. Empty for a server that wants none.
    pub key: String,
    pub model: String,
    /// Used if `model` stops existing (HTTP 404).
    pub fallback_model: String,
}

impl Endpoint {
    /// Any OpenAI-compatible server, hosted or your own: `LLM_BASE_URL` (up to
    /// and including `/v1`), `LLM_MODEL`, and `LLM_API_KEY` if it wants one. It
    /// must stream usage and support strict `json_schema` output.
    fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let need = |name: &str| get(name).with_context(|| format!("{name} is not set"));
        let base = need("LLM_BASE_URL")?;
        let parsed = reqwest::Url::parse(&base)
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
            .with_context(|| format!("LLM_BASE_URL={base} isn't a URL (expected https://host/v1)"))?;
        let model = need("LLM_MODEL")?;
        Ok(Self {
            name: parsed.host_str().unwrap_or_default().to_string(),
            url: format!("{}/chat/completions", parsed.as_str().trim_end_matches('/')),
            key: get("LLM_API_KEY").unwrap_or_default(),
            fallback_model: get("LLM_FALLBACK_MODEL").unwrap_or_else(|| model.clone()),
            model,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub endpoint: Endpoint,
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
    /// Google Analytics measurement ID for paper-bro's own pages (courses on
    /// AERO carry AERO's tag). Unset: no analytics.
    pub ga_id: Option<String>,
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
        // Spliced into the app's HTML, so it must be exactly an ID.
        let ga_id = var("GA_ID");
        if let Some(id) = &ga_id {
            if !id.strip_prefix("G-").is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric())) {
                bail!("GA_ID={id} isn't a measurement ID (G-XXXXXXXXXX)");
            }
        }
        Ok(Self {
            database_url: required("NEON_DB")?,
            endpoint: Endpoint::from_vars(var)?,
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
            ga_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: Vec<(String, String)> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| map.iter().find(|(name, _)| name == k).map(|(_, v)| v.clone())
    }

    #[test]
    fn any_openai_compatible_server_by_url_model_and_key() {
        let e = Endpoint::from_vars(vars(&[("LLM_BASE_URL", "https://ep.example/v1/"), ("LLM_MODEL", "org/model"), ("LLM_API_KEY", "id.secret")])).unwrap();
        assert_eq!((e.name.as_str(), e.url.as_str(), e.key.as_str()), ("ep.example", "https://ep.example/v1/chat/completions", "id.secret"));
        assert_eq!((e.model.as_str(), e.fallback_model.as_str()), ("org/model", "org/model"));
        // A local server wants no key, and may name a fallback model.
        let e = Endpoint::from_vars(vars(&[("LLM_BASE_URL", "http://localhost:11434/v1"), ("LLM_MODEL", "a"), ("LLM_FALLBACK_MODEL", "b")])).unwrap();
        assert_eq!((e.name.as_str(), e.url.as_str(), e.key.as_str(), e.fallback_model.as_str()), ("localhost", "http://localhost:11434/v1/chat/completions", "", "b"));
    }

    #[test]
    fn the_url_and_the_model_are_required() {
        let err = Endpoint::from_vars(vars(&[("LLM_MODEL", "m")])).unwrap_err().to_string();
        assert!(err.contains("LLM_BASE_URL"), "{err}");
        let err = Endpoint::from_vars(vars(&[("LLM_BASE_URL", "https://ep.example/v1")])).unwrap_err().to_string();
        assert!(err.contains("LLM_MODEL"), "{err}");
    }

    #[test]
    fn a_key_meant_for_another_service_is_never_picked_up() {
        // Only LLM_API_KEY is read, so one left in the environment for something
        // else is not sent to this server.
        let e = Endpoint::from_vars(vars(&[("LLM_BASE_URL", "https://ep.example/v1"), ("LLM_MODEL", "m"), ("OPENAI_API_KEY", "other")])).unwrap();
        assert_eq!(e.key, "");
    }

    #[test]
    fn a_base_url_that_is_not_a_url_stops_the_boot() {
        for bad in ["ep.example/v1", "ftp://ep.example/v1", "https://"] {
            let err = Endpoint::from_vars(vars(&[("LLM_BASE_URL", bad), ("LLM_MODEL", "m")])).unwrap_err().to_string();
            assert!(err.contains("LLM_BASE_URL"), "{bad}: {err}");
        }
    }
}
