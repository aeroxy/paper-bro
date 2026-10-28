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
#[derive(Clone)]
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

// Never print the key (or a token in the URL's query), whatever logs this.
impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("name", &self.name)
            .field("url", &self.url.split('?').next().unwrap_or_default())
            .field("key", &if self.key.is_empty() { "" } else { "<redacted>" })
            .field("model", &self.model)
            .field("fallback_model", &self.fallback_model)
            .finish()
    }
}

impl Endpoint {
    /// What a command that makes no model calls carries in place of one.
    fn unused() -> Self {
        Self { name: "(not used)".into(), url: String::new(), key: String::new(), model: String::new(), fallback_model: String::new() }
    }

    /// The settings, or for a command that needs no model, whatever is there.
    fn or_unused(settings: Result<Self>, needs_model: bool) -> Result<Self> {
        match settings {
            Err(_) if !needs_model => Ok(Self::unused()),
            other => other,
        }
    }

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
        let mut url = parsed.clone();
        url.set_fragment(None);
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("LLM_BASE_URL={base} can't carry a path"))?
            .pop_if_empty()
            .extend(["chat", "completions"]);
        Ok(Self {
            name: parsed.host_str().unwrap_or_default().to_string(),
            url: url.to_string(),
            key: get("LLM_API_KEY").unwrap_or_default(),
            fallback_model: get("LLM_FALLBACK_MODEL").unwrap_or_else(|| model.clone()),
            model,
        })
    }

    /// True when the key would cross a network unencrypted: one is set and the
    /// address is plain http to a host other than this machine.
    pub fn key_in_clear(&self) -> bool {
        let Ok(u) = reqwest::Url::parse(&self.url) else { return false };
        let local = u.host_str().is_some_and(|h| {
            h == "localhost" || h.trim_matches(['[', ']']).parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
        });
        u.scheme() == "http" && !self.key.is_empty() && !local
    }
}

// No Debug: it holds the database URL and the AERO token.
#[derive(Clone)]
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
        Self::from_env_for(true)
    }

    /// `needs_model` is false for a command that never calls the model, which
    /// then runs without the `LLM_*` settings.
    pub fn from_env_for(needs_model: bool) -> Result<Self> {
        let chrome = PathBuf::from(var("CHROME_PATH").unwrap_or_else(|| {
            ".chrome/chrome-headless-shell-mac-arm64/chrome-headless-shell".into()
        }));
        if !chrome.exists() {
            bail!("CHROME_PATH {} does not exist", chrome.display());
        }
        let endpoint = Endpoint::or_unused(Endpoint::from_vars(var), needs_model)?;
        if endpoint.key_in_clear() {
            log::warn!(
                "LLM_BASE_URL is plain http to {}: LLM_API_KEY is sent unencrypted. Use https unless that network is one you trust.",
                endpoint.name
            );
        }
        Ok(Self {
            database_url: required("NEON_DB")?,
            endpoint,
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
    fn the_path_is_added_before_any_query() {
        let url = |base: &str| Endpoint::from_vars(vars(&[("LLM_BASE_URL", base), ("LLM_MODEL", "m")])).unwrap().url;
        assert_eq!(url("https://ep.example/v1?api-version=1"), "https://ep.example/v1/chat/completions?api-version=1");
        assert_eq!(url("https://ep.example/v1/?api-version=1"), "https://ep.example/v1/chat/completions?api-version=1");
        assert_eq!(url("https://ep.example"), "https://ep.example/chat/completions");
        assert_eq!(url("https://ep.example/v1#notes"), "https://ep.example/v1/chat/completions");
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
    fn a_key_over_plain_http_to_another_machine_is_noticed() {
        let e = |url: &str, key: &str| Endpoint { name: "h".into(), url: url.into(), key: key.into(), model: "m".into(), fallback_model: "m".into() };
        assert!(e("http://llm.internal:8000/v1/chat/completions", "k").key_in_clear());
        assert!(!e("https://llm.example/v1/chat/completions", "k").key_in_clear());
        assert!(!e("http://llm.internal:8000/v1/chat/completions", "").key_in_clear());
        for local in ["http://localhost:11434/v1/chat/completions", "http://127.0.0.1:8000/v1/chat/completions", "http://[::1]:8000/v1/chat/completions"] {
            assert!(!e(local, "k").key_in_clear(), "{local}");
        }
    }

    #[test]
    fn debug_output_never_shows_the_key_or_a_token_in_the_url() {
        let e = Endpoint { name: "h".into(), url: "https://h/v1/chat/completions?token=hunter2".into(), key: "sk-secret".into(), model: "m".into(), fallback_model: "m".into() };
        let shown = format!("{e:?}");
        assert!(!shown.contains("sk-secret") && !shown.contains("hunter2"), "{shown}");
        assert!(shown.contains("<redacted>") && shown.contains("https://h/v1/chat/completions"), "{shown}");
    }

    #[test]
    fn a_command_that_needs_no_model_runs_without_model_settings() {
        let missing = || Endpoint::from_vars(vars(&[]));
        assert!(Endpoint::or_unused(missing(), false).is_ok());
        assert!(Endpoint::or_unused(missing(), true).is_err());
        let set = Endpoint::from_vars(vars(&[("LLM_BASE_URL", "https://ep.example/v1"), ("LLM_MODEL", "m")]));
        assert_eq!(Endpoint::or_unused(set, false).unwrap().model, "m");
    }

    #[test]
    fn a_base_url_that_is_not_a_url_stops_the_boot() {
        for bad in ["ep.example/v1", "ftp://ep.example/v1", "https://"] {
            let err = Endpoint::from_vars(vars(&[("LLM_BASE_URL", bad), ("LLM_MODEL", "m")])).unwrap_err().to_string();
            assert!(err.contains("LLM_BASE_URL"), "{bad}: {err}");
        }
    }
}
