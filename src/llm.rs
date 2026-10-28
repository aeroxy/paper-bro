//! Chat completions for Muse Spark Open from any OpenAI-compatible server
//! (Baseten by default, `config::Provider` says where), by hand on reqwest.
//!
//! No agent framework: every pipeline step is one call, and what matters is
//! the parts of these APIs that differ from OpenAI's, which client
//! libraries tend to drop (`reasoning_content`, `reasoning_effort: xhigh`,
//! per-call usage, a stream that ends early).
//! What this has to get right:
//!
//! - Stream, always. Long reasoning calls would otherwise sit silent for
//!   minutes, and a stream is how truncation shows: one that ends without a
//!   `finish_reason` failed partway and is retried, never taken as a short
//!   answer. Usage comes from the final chunk (`choices: []`).
//! - `error` is sometimes a string and sometimes `{message}`.
//! - 429/529/5xx back off (honouring Retry-After); 402 means the budget ran
//!   out and is never retried; 404 means the configured model is gone, so
//!   switch to the fallback.

use anyhow::{Context, Result, anyhow, bail};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::Semaphore;

use crate::config::Provider;

const ATTEMPTS: u32 = 6;
/// No bytes for this long means the stream has died.
const IDLE: Duration = Duration::from_secs(180);

// Prices in USD per million tokens (Oct 2026). Baseten and Modal list the same.
// Modal reports no cached tokens, so there every prompt token is counted, and
// billed, at the full rate.
const PRICE_INPUT: f64 = 1.25;
const PRICE_CACHED: f64 = 0.15;
const PRICE_OUTPUT: f64 = 4.25;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt: u64,
    pub cached: u64,
    pub completion: u64,
    pub reasoning: u64,
    pub cost_usd: f64,
    pub calls: u64,
}

impl Usage {
    pub fn add(&mut self, o: &Usage) {
        self.prompt += o.prompt;
        self.cached += o.cached;
        self.completion += o.completion;
        self.reasoning += o.reasoning;
        self.cost_usd += o.cost_usd;
        self.calls += o.calls;
    }
}

/// Why a call ended, for the caller to tell "retry later" from "give up".
#[derive(Debug)]
pub enum LlmError {
    /// 402: the provider's account or budget is exhausted.
    Budget(String),
    /// A request the provider rejected outright (400 and friends).
    Rejected(String),
}
impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Budget(m) => write!(f, "model API budget exhausted: {m}"),
            Self::Rejected(m) => write!(f, "model API rejected the request: {m}"),
        }
    }
}
impl std::error::Error for LlmError {}

pub struct Llm {
    http: reqwest::Client,
    name: String,
    url: String,
    key: String,
    model: Mutex<String>,
    fallback: String,
    /// Caps concurrent calls across every job. Sized for Baseten, whose key
    /// allows 100 requests and 2M tokens a minute (cached tokens count toward
    /// the latter).
    slots: Semaphore,
}

pub struct Request<'a> {
    pub messages: Vec<Value>,
    pub effort: &'a str,
    /// `(name, schema)` for strict JSON output.
    pub schema: Option<(&'a str, Value)>,
    /// Sent as `x-session-affinity`, so a job's calls land on one replica and
    /// share its prompt cache.
    pub affinity: &'a str,
}

impl Llm {
    pub fn new(provider: &Provider, slots: usize) -> Result<Self> {
        let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(20)).build()?;
        Ok(Self {
            http,
            name: provider.name.clone(),
            url: provider.url.clone(),
            key: provider.key.clone(),
            model: Mutex::new(provider.model.clone()),
            fallback: provider.fallback_model.clone(),
            slots: Semaphore::new(slots),
        })
    }

    pub fn model(&self) -> String {
        self.model.lock().unwrap().clone()
    }

    /// One completion: the assistant's text, and what it cost.
    pub async fn chat(&self, req: &Request<'_>) -> Result<(String, Usage)> {
        let _slot = self.slots.acquire().await?;
        let mut last_err = anyhow!("no attempt made");
        for attempt in 0..ATTEMPTS {
            let model = self.model();
            let mut body = json!({
                "model": model,
                "messages": req.messages,
                "reasoning_effort": req.effort,
                "stream": true,
                "stream_options": {"include_usage": true},
            });
            if let Some((name, schema)) = &req.schema {
                body["response_format"] = json!({
                    "type": "json_schema",
                    "json_schema": {"name": name, "strict": true, "schema": schema},
                });
            }
            let mut call = self.http.post(&self.url).header("x-session-affinity", req.affinity).json(&body);
            if !self.key.is_empty() {
                call = call.bearer_auth(&self.key);
            }
            let resp = call.send().await;
            let wait = match resp {
                Err(e) => {
                    last_err = anyhow!(e).context(format!("{} request failed", self.name));
                    backoff(attempt, None)
                }
                Ok(resp) if resp.status().is_success() => match read_stream(resp).await {
                    Ok(done) => return Ok(done),
                    Err(e) => {
                        last_err = e;
                        backoff(attempt, None)
                    }
                },
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let retry_after = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok()?.parse::<u64>().ok())
                        .map(Duration::from_secs);
                    let msg = error_text(&resp.text().await.unwrap_or_default());
                    match status {
                        402 => return Err(LlmError::Budget(format!("{}: {msg}", self.name)).into()),
                        404 if model != self.fallback => {
                            log::warn!("model {model} is gone ({msg}); switching to {}", self.fallback);
                            *self.model.lock().unwrap() = self.fallback.clone();
                            continue;
                        }
                        429 | 529 | 500..=599 => {
                            last_err = anyhow!("{} HTTP {status}: {msg}", self.name);
                            backoff(attempt, retry_after)
                        }
                        _ => return Err(LlmError::Rejected(format!("{} HTTP {status}: {msg}", self.name)).into()),
                    }
                }
            };
            log::warn!("{} attempt {} failed: {last_err:#}; retrying in {wait:?}", self.name, attempt + 1);
            tokio::time::sleep(wait).await;
        }
        Err(last_err.context(format!("{} failed {ATTEMPTS} times", self.name)))
    }

    /// A strict-JSON call, parsed.
    pub async fn json<T: serde::de::DeserializeOwned>(&self, req: &Request<'_>) -> Result<(T, Usage)> {
        let (text, usage) = self.chat(req).await?;
        let value = serde_json::from_str(&text)
            .with_context(|| format!("model returned invalid JSON: {}", text.chars().take(300).collect::<String>()))?;
        Ok((value, usage))
    }
}

fn backoff(attempt: u32, retry_after: Option<Duration>) -> Duration {
    let base = retry_after.unwrap_or(Duration::from_secs(2u64.pow(attempt.min(5))));
    (base + Duration::from_millis(fastrand::u64(0..1000))).min(Duration::from_secs(90))
}

fn error_text(body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    match &v["error"] {
        Value::String(s) => s.clone(),
        e => e["message"].as_str().map(str::to_string).unwrap_or_else(|| body.chars().take(300).collect()),
    }
}

async fn read_stream(resp: reqwest::Response) -> Result<(String, Usage)> {
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut content = String::new();
    let mut finish: Option<String> = None;
    let mut usage = Value::Null;
    loop {
        let chunk = match tokio::time::timeout(IDLE, stream.next()).await {
            Err(_) => bail!("stream went silent for {IDLE:?}"),
            Ok(None) => break,
            Ok(Some(chunk)) => chunk.context("stream broke")?,
        };
        buf.extend_from_slice(&chunk);
        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line);
            let Some(data) = line.trim().strip_prefix("data:") else { continue };
            let data = data.trim();
            if data == "[DONE]" || data.is_empty() {
                continue;
            }
            let event: Value = serde_json::from_str(data).context("unreadable stream event")?;
            if !event["error"].is_null() {
                bail!("stream error: {}", error_text(data));
            }
            if let Some(choice) = event["choices"].get(0) {
                if let Some(t) = choice["delta"]["content"].as_str() {
                    content.push_str(t);
                }
                if let Some(f) = choice["finish_reason"].as_str() {
                    finish = Some(f.to_string());
                }
            }
            if !event["usage"].is_null() {
                usage = event["usage"].clone();
            }
        }
    }
    match finish.as_deref() {
        Some("stop") => {}
        Some("length") => return Err(LlmError::Rejected("the answer hit the output limit".into()).into()),
        Some(other) => bail!("stream finished with {other}"),
        None => bail!("stream ended before the answer finished"),
    }
    let n = |v: &Value| v.as_u64().unwrap_or(0);
    let prompt = n(&usage["prompt_tokens"]);
    let cached = n(&usage["prompt_tokens_details"]["cached_tokens"]);
    let completion = n(&usage["completion_tokens"]);
    let cost = (prompt.saturating_sub(cached) as f64 * PRICE_INPUT
        + cached as f64 * PRICE_CACHED
        + completion as f64 * PRICE_OUTPUT)
        / 1e6;
    Ok((
        content,
        Usage {
            prompt,
            cached,
            completion,
            reasoning: n(&usage["completion_tokens_details"]["reasoning_tokens"]).max(n(&usage["reasoning_tokens"])),
            cost_usd: cost,
            calls: 1,
        },
    ))
}

/// Make a JSON schema strict-mode clean: every object closed and every
/// property required. Writing schemas by hand that way is error-prone.
pub fn strict(mut schema: Value) -> Value {
    fn walk(v: &mut Value) {
        match v {
            Value::Object(map) => {
                if map.get("type").and_then(Value::as_str) == Some("object") {
                    let keys: Vec<Value> = map
                        .get("properties")
                        .and_then(Value::as_object)
                        .map(|p| p.keys().cloned().map(Value::String).collect())
                        .unwrap_or_default();
                    map.insert("required".into(), Value::Array(keys));
                    map.insert("additionalProperties".into(), Value::Bool(false));
                }
                for child in map.values_mut() {
                    walk(child);
                }
            }
            Value::Array(items) => items.iter_mut().for_each(walk),
            _ => {}
        }
    }
    walk(&mut schema);
    schema
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_text_reads_both_shapes() {
        assert_eq!(error_text(r#"{"error":"please check the model you provided"}"#), "please check the model you provided");
        assert_eq!(error_text(r#"{"error":{"message":"Only support one response per prompt."}}"#), "Only support one response per prompt.");
        assert_eq!(error_text("<html>gateway</html>"), "<html>gateway</html>");
    }

    #[test]
    fn strict_closes_every_object() {
        let s = strict(json!({"type":"object","properties":{"a":{"type":"string"},
            "b":{"type":"array","items":{"type":"object","properties":{"c":{"type":"integer"}}}}}}));
        assert_eq!(s["required"], json!(["a", "b"]));
        assert_eq!(s["additionalProperties"], json!(false));
        assert_eq!(s["properties"]["b"]["items"]["required"], json!(["c"]));
        assert_eq!(s["properties"]["b"]["items"]["additionalProperties"], json!(false));
    }

    use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::post};

    /// A stand-in for a provider's `/v1/chat/completions`, on a free port.
    async fn serve(handler: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/chat/completions", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, handler).await.unwrap() });
        url
    }

    fn provider(url: String) -> Provider {
        Provider { name: "stand-in".into(), url, key: "wk-id.ws-secret".into(), model: "m".into(), fallback_model: "m".into() }
    }

    fn request(messages: Vec<Value>) -> Request<'static> {
        Request { messages, effort: "low", schema: None, affinity: "job-1" }
    }

    /// The stream Modal's Shared Endpoint sent on 28 Oct 2026: reasoning in
    /// `reasoning_content`, usage on a last chunk with no choices, no cached
    /// count (`prompt_tokens_details` is null), reasoning tokens at the top.
    const MODAL_STREAM: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"hmm\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"rea\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"dy\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":151,\"total_tokens\":370,\"completion_tokens\":219,",
        "\"prompt_tokens_details\":null,\"reasoning_tokens\":212}}\n\n",
        "data: [DONE]\n\n",
    );

    #[tokio::test]
    async fn a_modal_stream_is_read_and_priced_with_no_cached_tokens() {
        // 401 unless the proxy token is sent as a bearer, so success proves it was.
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|headers: axum::http::HeaderMap| async move {
                if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer wk-id.ws-secret") {
                    return (StatusCode::UNAUTHORIZED, Json(json!({"error": "Webhook token not found"}))).into_response();
                }
                ([("content-type", "text/event-stream")], MODAL_STREAM).into_response()
            }),
        );
        let llm = Llm::new(&provider(serve(app).await), 1).unwrap();
        let (text, usage) = llm.chat(&request(vec![json!({"role": "user", "content": "hi"})])).await.unwrap();
        assert_eq!(text, "ready");
        assert_eq!((usage.prompt, usage.cached, usage.completion, usage.reasoning, usage.calls), (151, 0, 219, 212, 1));
        assert!((usage.cost_usd - (151.0 * PRICE_INPUT + 219.0 * PRICE_OUTPUT) / 1e6).abs() < 1e-12);
    }

    #[tokio::test]
    async fn an_exhausted_account_is_a_budget_error_that_names_the_provider() {
        let app = Router::new().route("/v1/chat/completions", post(|| async { (StatusCode::PAYMENT_REQUIRED, Json(json!({"error": "out of credit"}))) }));
        let llm = Llm::new(&provider(serve(app).await), 1).unwrap();
        let err = llm.chat(&request(vec![])).await.unwrap_err();
        let Some(LlmError::Budget(m)) = err.downcast_ref::<LlmError>() else { panic!("{err:#}") };
        assert_eq!(m, "stand-in: out of credit");
    }

    #[tokio::test]
    async fn a_server_that_wants_no_key_gets_no_authorization_header() {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|headers: axum::http::HeaderMap| async move {
                if headers.contains_key("authorization") {
                    return (StatusCode::BAD_REQUEST, Json(json!({"error": "unexpected Authorization header"}))).into_response();
                }
                ([("content-type", "text/event-stream")], MODAL_STREAM).into_response()
            }),
        );
        let provider = Provider { key: String::new(), ..provider(serve(app).await) };
        let llm = Llm::new(&provider, 1).unwrap();
        assert_eq!(llm.chat(&request(vec![])).await.unwrap().0, "ready");
    }

    #[tokio::test]
    async fn a_refused_request_is_not_retried() {
        let app = Router::new().route("/v1/chat/completions", post(|| async { (StatusCode::UNAUTHORIZED, Json(json!({"error": "Webhook token not found: wk-x"}))) }));
        let llm = Llm::new(&provider(serve(app).await), 1).unwrap();
        let err = llm.chat(&request(vec![])).await.unwrap_err();
        assert_eq!(err.to_string(), "model API rejected the request: stand-in HTTP 401: Webhook token not found: wk-x");
    }
}
