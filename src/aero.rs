//! Publishing to AERO through its MCP endpoint.
//!
//! `/mcp` is stateless JSON-RPC: one POST per call, no handshake, a personal
//! access token as the bearer. Tokens expire after at most 90 days. When
//! paper-bro runs on the same host as AERO, `AERO_MCP_URL` can point at
//! AERO's local port instead of its public URL.

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

pub struct Aero {
    http: reqwest::Client,
    url: String,
    token: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Page {
    pub id: String,
    /// Relative, `/p/{alias}`.
    pub url: String,
}

/// A tool error from AERO, as opposed to a transport failure.
#[derive(Debug)]
pub struct ToolError(pub String);
impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AERO said: {}", self.0)
    }
}
impl std::error::Error for ToolError {}

impl Aero {
    pub fn new(url: &str, token: &str) -> Result<Self> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(60)).build()?;
        Ok(Self { http, url: url.to_string(), token: token.to_string() })
    }

    async fn call(&self, tool: &str, args: Value) -> Result<Value> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": tool, "arguments": args}});
        let resp = self
            .http
            .post(&self.url)
            .bearer_auth(&self.token)
            .header("Accept", "application/json, text/event-stream")
            .json(&body)
            .send()
            .await
            .with_context(|| format!("AERO {tool}"))?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            bail!("AERO rejected the token (expired or revoked): mint a new one at /connect");
        }
        let v: Value = resp.json().await.with_context(|| format!("AERO {tool}: HTTP {status}"))?;
        if let Some(e) = v.get("error") {
            bail!("AERO {tool}: {}", e["message"].as_str().unwrap_or("unknown error"));
        }
        let text = v["result"]["content"][0]["text"].as_str().unwrap_or("").to_string();
        if v["result"]["isError"] == Value::Bool(true) {
            return Err(ToolError(text).into());
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    pub async fn create_page(&self, html: &str, alias: &str) -> Result<Page> {
        let v = self.call("create_page", json!({"html": html, "alias": alias})).await?;
        Ok(serde_json::from_value(v)?)
    }

    /// AERO acknowledges an update with `{"ok":true}` and nothing more: the
    /// page keeps its id and URL.
    pub async fn update_page(&self, page: &str, html: &str) -> Result<()> {
        self.call("update_page", json!({"page": page, "html": html})).await?;
        Ok(())
    }

    pub async fn delete_page(&self, page: &str) -> Result<()> {
        self.call("delete_page", json!({"page": page})).await?;
        Ok(())
    }

    /// Publish a course: update the page we already have, else create one at
    /// `alias`. If the alias is taken and the page there is ours (a rebuild
    /// after the database lost track of it), update it; if it belongs to
    /// someone else, fall back to a suffixed alias.
    pub async fn publish(&self, existing: Option<&Page>, alias: &str, html: &str) -> Result<Page> {
        if let Some(p) = existing {
            match self.update_page(&p.id, html).await {
                Ok(()) => return Ok(p.clone()),
                Err(e) if e.downcast_ref::<ToolError>().is_some() => log::warn!("updating {} failed ({e}); creating", p.id),
                Err(e) => return Err(e),
            }
        }
        match self.create_page(html, alias).await {
            Ok(p) => Ok(p),
            Err(e) if e.to_string().contains("already taken") => match self.update_page(alias, html).await {
                // AERO finds a page by its alias as well as by its id, so the
                // alias serves as the id from here on.
                Ok(()) => Ok(Page { id: alias.to_string(), url: format!("/p/{alias}") }),
                // Only a refusal means the page isn't ours: after a transport
                // error the update may have landed, and a second page would
                // be a duplicate.
                Err(e) if e.downcast_ref::<ToolError>().is_some() => {
                    let alt = format!("{alias}-{:04x}", fastrand::u16(..));
                    self.create_page(html, &alt).await.map_err(|e2| anyhow!("{alias} is taken and {alt} failed: {e2}"))
                }
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, routing::post};

    /// A stand-in for AERO's `/mcp` that answers the way AERO does:
    /// `update_page` acknowledges with `{"ok":true}` and nothing more, and
    /// `create_page` finds the alias taken.
    async fn fake_aero() -> String {
        let mcp = Router::new().route(
            "/mcp",
            post(|Json(req): Json<Value>| async move {
                let (text, is_error) = match req["params"]["name"].as_str() {
                    Some("update_page") => (r#"{"ok":true}"#, false),
                    _ => ("Alias 'arxiv-x' is already taken.", true),
                };
                Json(json!({"jsonrpc": "2.0", "id": 1, "result": {"content": [{"type": "text", "text": text}], "isError": is_error}}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, mcp).await.unwrap() });
        url
    }

    #[tokio::test]
    async fn an_update_is_acknowledged_without_a_page() {
        let aero = Aero::new(&fake_aero().await, "token").unwrap();
        aero.update_page("9f2c", "<p>course</p>").await.unwrap();
    }

    #[tokio::test]
    async fn updating_in_place_keeps_the_page_we_had() {
        let aero = Aero::new(&fake_aero().await, "token").unwrap();
        let had = Page { id: "9f2c".into(), url: "/p/arxiv-x-1a2b".into() };
        let p = aero.publish(Some(&had), "arxiv-x", "<p>course</p>").await.unwrap();
        assert_eq!((p.id.as_str(), p.url.as_str()), ("9f2c", "/p/arxiv-x-1a2b"));
    }

    #[tokio::test]
    async fn a_taken_alias_of_ours_is_updated_not_duplicated() {
        let aero = Aero::new(&fake_aero().await, "token").unwrap();
        let p = aero.publish(None, "arxiv-x", "<p>course</p>").await.unwrap();
        assert_eq!((p.id.as_str(), p.url.as_str()), ("arxiv-x", "/p/arxiv-x"));
    }
}
