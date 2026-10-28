//! Configuration, read once from the environment at boot.
//!
//! Every knob has a default except the credentials. `.env.example` lists them
//! all with their defaults.

use anyhow::{Context, Result, bail};
use std::net::SocketAddr;
use std::path::PathBuf;

/// The path prefix paper-bro is served under. The reverse proxy routes
/// `/paper-bro*` here without stripping it, so every route and asset URL
/// carries it.
pub const BASE: &str = "/paper-bro";

const BASETEN_URL: &str = "https://inference.baseten.co/v1/chat/completions";
const DEFAULT_MODEL: &str = "meta-models/muse-spark-open";

/// The OpenAI-compatible chat-completions endpoint a build calls.
#[derive(Clone, Debug)]
pub struct Provider {
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

impl Provider {
    /// Any OpenAI-compatible server: `LLM_BASE_URL` (up to and including
    /// `/v1`), `LLM_MODEL`, and `LLM_API_KEY` if it wants one. It must support
    /// streaming usage and strict `json_schema` output.
    ///
    /// With no `LLM_BASE_URL` it is Baseten, under its own variable names
    /// (`BASETEN_*`). Those count only then, so a Baseten key left in the
    /// environment is never sent to another server.
    fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let need = |name: &str| get(name).with_context(|| format!("{name} is not set"));
        let Some(base) = get("LLM_BASE_URL") else {
            return Ok(Self {
                name: "inference.baseten.co".into(),
                url: BASETEN_URL.into(),
                key: need("BASETEN_API_KEY")?,
                model: get("BASETEN_MODEL").unwrap_or_else(|| DEFAULT_MODEL.into()),
                fallback_model: get("BASETEN_FALLBACK_MODEL").unwrap_or_else(|| DEFAULT_MODEL.into()),
            });
        };
        let parsed = reqwest::Url::parse(&base)
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
            .with_context(|| format!("LLM_BASE_URL={base} isn't a URL (expected https://host/v1)"))?;
        let name = parsed.host_str().unwrap_or_default().to_string();
        let url = format!("{}/chat/completions", parsed.as_str().trim_end_matches('/'));
        let model = need("LLM_MODEL")?;
        Ok(Self {
            name,
            url,
            key: get("LLM_API_KEY").unwrap_or_default(),
            fallback_model: get("LLM_FALLBACK_MODEL").unwrap_or_else(|| model.clone()),
            model,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub provider: Provider,
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
            provider: Provider::from_vars(var)?,
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
    fn baseten_is_the_default() {
        let p = Provider::from_vars(vars(&[("BASETEN_API_KEY", "k")])).unwrap();
        assert_eq!((p.name.as_str(), p.url.as_str(), p.key.as_str()), ("inference.baseten.co", BASETEN_URL, "k"));
        assert_eq!((p.model.as_str(), p.fallback_model.as_str()), (DEFAULT_MODEL, DEFAULT_MODEL));
        let err = Provider::from_vars(vars(&[])).unwrap_err().to_string();
        assert!(err.contains("BASETEN_API_KEY"), "{err}");
    }

    #[test]
    fn any_openai_compatible_server_by_url_model_and_key() {
        let p = Provider::from_vars(vars(&[("LLM_BASE_URL", "https://ep.example/v1/"), ("LLM_MODEL", "org/model"), ("LLM_API_KEY", "id.secret")])).unwrap();
        assert_eq!((p.name.as_str(), p.url.as_str(), p.key.as_str()), ("ep.example", "https://ep.example/v1/chat/completions", "id.secret"));
        assert_eq!((p.model.as_str(), p.fallback_model.as_str()), ("org/model", "org/model"));
        // A local server wants no key, and may name a fallback model.
        let p = Provider::from_vars(vars(&[("LLM_BASE_URL", "http://localhost:11434/v1"), ("LLM_MODEL", "a"), ("LLM_FALLBACK_MODEL", "b")])).unwrap();
        assert_eq!((p.name.as_str(), p.url.as_str(), p.key.as_str(), p.fallback_model.as_str()), ("localhost", "http://localhost:11434/v1/chat/completions", "", "b"));
    }

    #[test]
    fn a_baseten_key_is_never_sent_to_another_server() {
        // BASETEN_* count only for the default endpoint: with a custom one, a
        // key or model name left over from Baseten must not leak into it.
        let leftovers = [("BASETEN_API_KEY", "secret"), ("BASETEN_MODEL", "baseten/model")];
        let mut set = vec![("LLM_BASE_URL", "https://ep.example/v1")];
        set.extend(leftovers);
        let err = Provider::from_vars(vars(&set)).unwrap_err().to_string();
        assert!(err.contains("LLM_MODEL"), "{err}");
        set.push(("LLM_MODEL", "mine"));
        let p = Provider::from_vars(vars(&set)).unwrap();
        assert_eq!((p.key.as_str(), p.model.as_str()), ("", "mine"));
    }

    #[test]
    fn a_base_url_that_is_not_a_url_stops_the_boot() {
        for bad in ["ep.example/v1", "ftp://ep.example/v1", "https://"] {
            let err = Provider::from_vars(vars(&[("LLM_BASE_URL", bad), ("LLM_MODEL", "m")])).unwrap_err().to_string();
            assert!(err.contains("LLM_BASE_URL"), "{bad}: {err}");
        }
    }
}
