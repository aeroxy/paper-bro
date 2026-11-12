//! Chat completions from any OpenAI-compatible server, or from one that speaks
//! Google's Gemini API (`config::Endpoint` says which), by hand on reqwest.
//! Built against Muse Spark Open, and the Gemini side against Gemini 4 Argon
//! behind a proxy.
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
//! - Bound every answer (`max_tokens`). A call that loops has no other limit
//!   than the server's own, and the output is what costs. One that reaches the
//!   bound is rejected, not retried here (that would pay for the loop again);
//!   the pipeline tries again from its checkpoints.
//! - `error` is sometimes a string and sometimes `{message}`.
//! - 429/529/5xx back off (honouring Retry-After); 402 means the budget ran
//!   out and is never retried; 404 means the configured model is gone, so
//!   switch to the fallback.
//!
//! The pipeline writes every request in OpenAI's shape (messages of text,
//! `image_url` and `file` parts), and for a Gemini server it is translated
//! here (`gemini_body`). Where Gemini differs:
//!
//! - While the model thinks it sends nothing, not even the response headers,
//!   and one call thought for six and a half minutes. The idle and header
//!   limits below would end it. So every call asks for summaries of the
//!   thinking (`includeThoughts`), which stream as it goes, the way
//!   `reasoning_content` does, and are left out of the answer.
//! - The effort is not sent. The model comes with its level of thinking (a
//!   proxy's `…-high`), and a level sent with the call changed nothing there.
//! - The bound is `maxOutputTokens` and reaching it is `MAX_TOKENS`. Usage is
//!   `usageMetadata`, whole on the last event, with the thinking counted
//!   apart from the answer.

use anyhow::{Context, Result, anyhow, bail};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::Semaphore;

use crate::config::{Api, Endpoint};

const ATTEMPTS: u32 = 6;
/// No bytes for this long means the stream has died.
const IDLE: Duration = Duration::from_secs(180);
/// No response headers for this long means the request has stalled. The stream's
/// idle timeout only starts once headers arrive, and reqwest's own timeouts cover
/// connecting and nothing else.
const HEADERS: Duration = Duration::from_secs(300);
/// The most one call may write, reasoning included (`max_tokens`). Across the
/// 44 courses built so far a call writes about 5,000 tokens (nine in ten
/// courses average under 6,000), so this is six times the usual. It is there
/// for the call that never stops: that has no other bound than the server's
/// own, 262,144 tokens for the model this was built against, which is about
/// $1.10 and tens of minutes for one call. See `read_stream` for what the
/// limit being reached means.
const MAX_OUTPUT_TOKENS: u32 = 32_768;

// Prices in USD per million tokens: Muse Spark Open's list price (Oct 2026),
// used whatever the server, so cost figures are exact only for that model on
// those terms. A server that reports no cached tokens is counted at the full
// input rate.
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
    /// The answer reached the output limit and was thrown away. The tokens
    /// were written and billed all the same, so what the call cost rides with
    /// the error, for the caller to count (`spent`).
    Cut(Usage),
}
impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Budget(m) => write!(f, "model API budget exhausted: {m}"),
            Self::Rejected(m) => write!(f, "model API rejected the request: {m}"),
            Self::Cut(_) => write!(f, "the answer hit the output limit (asked for at most {MAX_OUTPUT_TOKENS} tokens)"),
        }
    }
}
impl LlmError {
    /// What a failed call cost anyway, when it is known.
    pub fn spent(&self) -> Option<Usage> {
        match self {
            Self::Cut(used) => Some(*used),
            _ => None,
        }
    }
}
impl std::error::Error for LlmError {}

pub struct Llm {
    http: reqwest::Client,
    endpoint: Endpoint,
    /// The model called now: `endpoint.model`, or its fallback once that is gone.
    model: Mutex<String>,
    /// Caps concurrent calls across every job. 8 was sized for a hosted key
    /// limited to 100 requests and 2M tokens a minute, where cached tokens
    /// count toward the latter.
    slots: Semaphore,
}

pub struct Request<'a> {
    /// In OpenAI's shape, whichever server is called.
    pub messages: Vec<Value>,
    /// `reasoning_effort`. Not sent to a Gemini server (see the module doc).
    pub effort: &'a str,
    /// `(name, schema)` for strict JSON output.
    pub schema: Option<(&'a str, Value)>,
    /// Sent as `x-session-affinity`, so a job's calls land on one replica and
    /// share its prompt cache.
    pub affinity: &'a str,
}

impl Llm {
    pub fn new(endpoint: &Endpoint, slots: usize) -> Result<Self> {
        let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(20)).build()?;
        Ok(Self { http, endpoint: endpoint.clone(), model: Mutex::new(endpoint.model.clone()), slots: Semaphore::new(slots) })
    }

    pub fn model(&self) -> String {
        self.model.lock().unwrap().clone()
    }

    /// One completion: the assistant's text, and what it cost.
    pub async fn chat(&self, req: &Request<'_>) -> Result<(String, Usage)> {
        let _slot = self.slots.acquire().await?;
        let (name, api) = (&self.endpoint.name, self.endpoint.api);
        let mut last_err = anyhow!("no attempt made");
        for attempt in 0..ATTEMPTS {
            let model = self.model();
            let body = match api {
                Api::OpenAi => openai_body(req, &model),
                Api::Gemini => gemini_body(req)?,
            };
            let mut call = self.http.post(self.endpoint.url(&model)?).header("x-session-affinity", req.affinity).json(&body);
            let key = &self.endpoint.key;
            if !key.is_empty() {
                call = match api {
                    Api::OpenAi => call.bearer_auth(key),
                    Api::Gemini => call.header("x-goog-api-key", key),
                };
            }
            let resp = send_within(call, HEADERS).await;
            let wait = match resp {
                Err(e) => {
                    last_err = e.context(format!("{name} request failed"));
                    backoff(attempt, None)
                }
                Ok(resp) if resp.status().is_success() => match read_stream(resp, api, req.affinity).await {
                    Ok(done) => return Ok(done),
                    // A call that reached the output limit is not asked again
                    // here: one that loops would loop again, at the full cost
                    // each time. The pipeline retries from its checkpoints.
                    Err(e) if matches!(e.downcast_ref::<LlmError>(), Some(LlmError::Cut(_))) => return Err(e),
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
                    let fallback = &self.endpoint.fallback_model;
                    match status {
                        402 => return Err(LlmError::Budget(format!("{name}: {msg}")).into()),
                        404 if model != *fallback => {
                            log::warn!("model {model} is gone ({msg}); switching to {fallback}");
                            *self.model.lock().unwrap() = fallback.clone();
                            continue;
                        }
                        429 | 529 | 500..=599 => {
                            last_err = anyhow!("{name} HTTP {status}: {msg}");
                            backoff(attempt, retry_after)
                        }
                        _ => return Err(LlmError::Rejected(format!("{name} HTTP {status}: {msg}")).into()),
                    }
                }
            };
            log::warn!("{name} attempt {} failed: {last_err:#}; retrying in {wait:?}", attempt + 1);
            tokio::time::sleep(wait).await;
        }
        Err(last_err.context(format!("{name} failed {ATTEMPTS} times")))
    }

    /// A strict-JSON call, parsed.
    pub async fn json<T: serde::de::DeserializeOwned>(&self, req: &Request<'_>) -> Result<(T, Usage)> {
        let (text, usage) = self.chat(req).await?;
        let value = serde_json::from_str(&text)
            .with_context(|| format!("model returned invalid JSON: {}", text.chars().take(300).collect::<String>()))?;
        Ok((value, usage))
    }
}

/// The request for an OpenAI-compatible server.
fn openai_body(req: &Request<'_>, model: &str) -> Value {
    let mut body = json!({
        "model": model,
        "messages": req.messages,
        "reasoning_effort": req.effort,
        "max_tokens": MAX_OUTPUT_TOKENS,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if let Some((name, schema)) = &req.schema {
        body["response_format"] = json!({
            "type": "json_schema",
            "json_schema": {"name": name, "strict": true, "schema": schema},
        });
    }
    body
}

/// The same request for a Gemini server: system messages become the system
/// instruction, the others `contents` in order (two user turns in a row are
/// fine), and attachments `inlineData`. The model is in the address, not here.
fn gemini_body(req: &Request<'_>) -> Result<Value> {
    let (mut system, mut contents) = (Vec::new(), Vec::new());
    for message in &req.messages {
        let parts = gemini_parts(&message["content"])?;
        match message["role"].as_str() {
            Some("system") => system.extend(parts),
            Some("assistant") => contents.push(json!({"role": "model", "parts": parts})),
            _ => contents.push(json!({"role": "user", "parts": parts})),
        }
    }
    let mut config = json!({
        "maxOutputTokens": MAX_OUTPUT_TOKENS,
        // Without it the stream says nothing until the thinking is done (module doc).
        "thinkingConfig": {"includeThoughts": true},
    });
    if let Some((_, schema)) = &req.schema {
        config["responseMimeType"] = json!("application/json");
        config["responseJsonSchema"] = schema.clone();
    }
    let mut body = json!({"contents": contents, "generationConfig": config});
    if !system.is_empty() {
        body["systemInstruction"] = json!({"parts": system});
    }
    Ok(body)
}

/// One message's content as Gemini parts. An image or a file goes as the data
/// URL it came in, split into its type and its base64.
fn gemini_parts(content: &Value) -> Result<Vec<Value>> {
    let Some(parts) = content.as_array() else {
        return Ok(vec![json!({"text": content.as_str().context("a message with no content")?})]);
    };
    parts
        .iter()
        .map(|part| {
            let url = match part["type"].as_str() {
                Some("text") => return Ok(json!({"text": part["text"]})),
                Some("image_url") => &part["image_url"]["url"],
                Some("file") => &part["file"]["file_data"],
                other => bail!("no Gemini form for a message part of type {other:?}"),
            };
            let (mime, data) = url
                .as_str()
                .and_then(|u| u.strip_prefix("data:")?.split_once(";base64,"))
                .context("an attachment that is not a base64 data URL")?;
            Ok(json!({"inlineData": {"mimeType": mime, "data": data}}))
        })
        .collect()
}

/// Send a request, giving up if the response headers do not arrive within
/// `limit`. Without it a server that takes the connection and then says nothing
/// would hold one of the call slots for ever.
async fn send_within(call: reqwest::RequestBuilder, limit: Duration) -> Result<reqwest::Response> {
    match tokio::time::timeout(limit, call.send()).await {
        Ok(sent) => Ok(sent?),
        Err(_) => bail!("no response headers within {limit:?}"),
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

async fn read_stream(resp: reqwest::Response, api: Api, who: &str) -> Result<(String, Usage)> {
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut said = Said::default();
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
            match api {
                Api::OpenAi => said.openai(&event),
                Api::Gemini => said.gemini(&event),
            }
        }
    }
    let used = said.usage();
    match said.finish.as_deref() {
        Some("stop") => {}
        // Thrown away, but written and billed: its cost goes with the error.
        Some("length") => {
            log::warn!("{}", cut_off_note(who, &said.content, &used));
            return Err(LlmError::Cut(used).into());
        }
        Some(other) => bail!("stream finished with {other}"),
        None => bail!("stream ended before the answer finished"),
    }
    Ok((said.content, used))
}

/// What a stream has said so far, whichever API's shape it came in.
#[derive(Default)]
struct Said {
    /// The answer, without any reasoning.
    content: String,
    /// How the answer ended, in OpenAI's words: `stop`, `length`, or another.
    finish: Option<String>,
    prompt: u64,
    /// Of `prompt`, how many were read from the server's cache.
    cached: u64,
    /// Every token written, reasoning included: what is billed as output.
    completion: u64,
    reasoning: u64,
}

fn count(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

impl Said {
    fn openai(&mut self, event: &Value) {
        if let Some(choice) = event["choices"].get(0) {
            if let Some(t) = choice["delta"]["content"].as_str() {
                self.content.push_str(t);
            }
            if let Some(f) = choice["finish_reason"].as_str() {
                self.finish = Some(f.to_string());
            }
        }
        let usage = &event["usage"];
        if !usage.is_null() {
            self.prompt = count(&usage["prompt_tokens"]);
            self.cached = count(&usage["prompt_tokens_details"]["cached_tokens"]);
            self.completion = count(&usage["completion_tokens"]);
            self.reasoning = count(&usage["completion_tokens_details"]["reasoning_tokens"]).max(count(&usage["reasoning_tokens"]));
        }
    }

    /// A Gemini event. The answer is the text of the parts that are not
    /// thoughts. Usage is empty on every event but the last.
    fn gemini(&mut self, event: &Value) {
        let candidate = &event["candidates"][0];
        for part in candidate["content"]["parts"].as_array().into_iter().flatten() {
            if part["thought"] != true {
                self.content.push_str(part["text"].as_str().unwrap_or_default());
            }
        }
        if let Some(reason) = candidate["finishReason"].as_str() {
            self.finish = Some(match reason {
                "STOP" => "stop".into(),
                "MAX_TOKENS" => "length".into(),
                other => other.into(),
            });
        }
        if let Some(reason) = event["promptFeedback"]["blockReason"].as_str() {
            self.finish = Some(format!("a blocked prompt ({reason})"));
        }
        let usage = &event["usageMetadata"];
        if usage.as_object().is_some_and(|u| !u.is_empty()) {
            let thoughts = count(&usage["thoughtsTokenCount"]);
            self.prompt = count(&usage["promptTokenCount"]);
            self.cached = count(&usage["cachedContentTokenCount"]);
            self.completion = count(&usage["candidatesTokenCount"]) + thoughts;
            self.reasoning = thoughts;
        }
    }

    fn usage(&self) -> Usage {
        let cost = (self.prompt.saturating_sub(self.cached) as f64 * PRICE_INPUT
            + self.cached as f64 * PRICE_CACHED
            + self.completion as f64 * PRICE_OUTPUT)
            / 1e6;
        Usage {
            prompt: self.prompt,
            cached: self.cached,
            completion: self.completion,
            reasoning: self.reasoning,
            cost_usd: cost,
            calls: 1,
        }
    }
}

/// What a call that reached the output limit was doing, for the log: how many
/// tokens it wrote and how many of them were reasoning, how long its answer
/// got, and how that answer ends, since a loop shows in its last words. `who`
/// is the job it belongs to (the request's affinity). An answer of no
/// characters means the loop was in the reasoning.
fn cut_off_note(who: &str, content: &str, used: &Usage) -> String {
    let tail: Vec<char> = content.chars().rev().take(160).collect();
    let tail: String = tail.into_iter().rev().collect();
    format!(
        "[{who}] answer cut off at the output limit: {} tokens written, {} of them reasoning; the answer is {} characters and ends {tail:?}",
        used.completion,
        used.reasoning,
        content.chars().count(),
    )
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

    /// A stand-in for a provider, on a free port: its base URL, `…/v1`.
    async fn serve(handler: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, handler).await.unwrap() });
        base
    }

    fn endpoint(base: String) -> Endpoint {
        Endpoint { name: "stand-in".into(), api: Api::OpenAi, base, key: "wk-id.ws-secret".into(), model: "m".into(), fallback_model: "m".into() }
    }

    fn request(messages: Vec<Value>) -> Request<'static> {
        Request { messages, effort: "low", schema: None, affinity: "job-1" }
    }

    /// A stream as one hosted endpoint sent it: reasoning in `reasoning_content`,
    /// usage on a last chunk with no choices, no cached count
    /// (`prompt_tokens_details` is null), reasoning tokens at the top of usage.
    const SAMPLE_STREAM: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"hmm\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"rea\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"dy\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":151,\"total_tokens\":370,\"completion_tokens\":219,",
        "\"prompt_tokens_details\":null,\"reasoning_tokens\":212}}\n\n",
        "data: [DONE]\n\n",
    );

    /// A stream cut off by the output limit: the server says `length`, not `stop`.
    const CUT_OFF_STREAM: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"and again\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":151,\"completion_tokens\":32768}}\n\n",
        "data: [DONE]\n\n",
    );

    #[tokio::test]
    async fn a_stream_with_no_cached_count_is_read_and_priced_in_full() {
        // 401 unless the proxy token is sent as a bearer, so success proves it was.
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|headers: axum::http::HeaderMap| async move {
                if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer wk-id.ws-secret") {
                    return (StatusCode::UNAUTHORIZED, Json(json!({"error": "Webhook token not found"}))).into_response();
                }
                ([("content-type", "text/event-stream")], SAMPLE_STREAM).into_response()
            }),
        );
        let llm = Llm::new(&endpoint(serve(app).await), 1).unwrap();
        let (text, usage) = llm.chat(&request(vec![json!({"role": "user", "content": "hi"})])).await.unwrap();
        assert_eq!(text, "ready");
        assert_eq!((usage.prompt, usage.cached, usage.completion, usage.reasoning, usage.calls), (151, 0, 219, 212, 1));
        assert!((usage.cost_usd - (151.0 * PRICE_INPUT + 219.0 * PRICE_OUTPUT) / 1e6).abs() < 1e-12);
    }

    #[tokio::test]
    async fn an_exhausted_account_is_a_budget_error_that_names_the_endpoint() {
        let app = Router::new().route("/v1/chat/completions", post(|| async { (StatusCode::PAYMENT_REQUIRED, Json(json!({"error": "out of credit"}))) }));
        let llm = Llm::new(&endpoint(serve(app).await), 1).unwrap();
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
                ([("content-type", "text/event-stream")], SAMPLE_STREAM).into_response()
            }),
        );
        let endpoint = Endpoint { key: String::new(), ..endpoint(serve(app).await) };
        let llm = Llm::new(&endpoint, 1).unwrap();
        assert_eq!(llm.chat(&request(vec![])).await.unwrap().0, "ready");
    }

    #[tokio::test]
    async fn a_refused_request_is_not_retried() {
        let app = Router::new().route("/v1/chat/completions", post(|| async { (StatusCode::UNAUTHORIZED, Json(json!({"error": "Webhook token not found: wk-x"}))) }));
        let llm = Llm::new(&endpoint(serve(app).await), 1).unwrap();
        let err = llm.chat(&request(vec![])).await.unwrap_err();
        assert_eq!(err.to_string(), "model API rejected the request: stand-in HTTP 401: Webhook token not found: wk-x");
    }

    #[test]
    fn a_cut_off_call_is_logged_with_its_tokens_and_how_its_answer_ends() {
        let used = Usage { prompt: 1000, cached: 0, completion: 32768, reasoning: 30000, cost_usd: 0.14, calls: 1 };
        let answer = format!("{{\"prose_html\": \"{}", "and so on ".repeat(40));
        let note = cut_off_note("2610.26825", &answer, &used);
        assert!(note.starts_with("[2610.26825] answer cut off at the output limit"), "{note}");
        assert!(note.contains("32768 tokens written, 30000 of them reasoning"), "{note}");
        assert!(note.contains(&format!("the answer is {} characters", answer.chars().count())), "{note}");
        assert!(note.contains("and so on and so on"), "a loop shows in how the answer ends: {note}");
        assert!(note.len() < 450, "one short line, not the whole answer: {} bytes", note.len());
        // An empty answer says so: the loop was in the reasoning.
        assert!(cut_off_note("x", "", &used).contains("the answer is 0 characters"));
    }

    #[tokio::test]
    async fn every_call_asks_for_a_bounded_answer() {
        let seen = std::sync::Arc::new(Mutex::new(Vec::<Value>::new()));
        let sink = seen.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |Json(body): Json<Value>| {
                let sink = sink.clone();
                async move {
                    sink.lock().unwrap().push(body);
                    ([("content-type", "text/event-stream")], SAMPLE_STREAM)
                }
            }),
        );
        let llm = Llm::new(&endpoint(serve(app).await), 1).unwrap();
        llm.chat(&request(vec![json!({"role": "user", "content": "hi"})])).await.unwrap();
        let bodies = seen.lock().unwrap();
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0]["max_tokens"], json!(MAX_OUTPUT_TOKENS));
    }

    #[tokio::test]
    async fn a_call_that_reaches_the_output_limit_is_rejected_and_not_asked_again() {
        let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = asked.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    ([("content-type", "text/event-stream")], CUT_OFF_STREAM)
                }
            }),
        );
        let llm = Llm::new(&endpoint(serve(app).await), 1).unwrap();
        let err = llm.chat(&request(vec![])).await.unwrap_err();
        let Some(LlmError::Cut(used)) = err.downcast_ref::<LlmError>() else { panic!("{err:#}") };
        assert!(err.to_string().contains(&MAX_OUTPUT_TOKENS.to_string()), "the message names the limit: {err}");
        // The tokens were written and billed, so the call's cost comes with the error.
        assert_eq!((used.prompt, used.completion, used.calls), (151, 32768, 1));
        assert!(used.cost_usd > 0.13, "the cut-off call is priced: {}", used.cost_usd);
        assert_eq!(err.downcast_ref::<LlmError>().and_then(LlmError::spent).map(|u| u.completion), Some(32768));
        assert!(LlmError::Budget("out of credit".into()).spent().is_none());
        // One call that loops is bad enough; asking again would pay for it again.
        assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_server_that_never_answers_is_given_up_on() {
        let app = Router::new().route("/v1/chat/completions", post(|| async { tokio::time::sleep(Duration::from_secs(3600)).await; "never" }));
        let url = format!("{}/chat/completions", serve(app).await);
        let started = std::time::Instant::now();
        let err = send_within(reqwest::Client::new().post(&url), Duration::from_millis(300)).await.unwrap_err();
        assert!(err.to_string().contains("no response headers"), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn an_answer_that_comes_in_time_is_passed_through() {
        let app = Router::new().route("/v1/chat/completions", post(|| async { "ok" }));
        let url = format!("{}/chat/completions", serve(app).await);
        let resp = send_within(reqwest::Client::new().post(&url), Duration::from_secs(5)).await.unwrap();
        assert_eq!(resp.text().await.unwrap(), "ok");
    }

    /// A Gemini stream as a proxy for Gemini 4 Argon sent it: a summary of the
    /// thinking, the answer in pieces, a part that only carries a thought
    /// signature, and usage on the last event only (empty before), with a
    /// cached count and the thinking counted apart from the answer.
    const GEMINI_STREAM: &str = concat!(
        r#"data: {"candidates": [{"content": {"parts": [{"text": "**Weighing the paper**", "thought": true}], "role": "model"}}], "usageMetadata": {}}"#,
        "\n\n",
        r#"data: {"candidates": [{"content": {"parts": [{"text": "rea"}], "role": "model"}}], "usageMetadata": {}}"#,
        "\n\n",
        r#"data: {"candidates": [{"content": {"parts": [{"text": "dy"}], "role": "model"}}], "usageMetadata": {}}"#,
        "\n\n",
        r#"data: {"candidates": [{"content": {"parts": [{"thoughtSignature": "AY89", "text": ""}], "role": "model"}, "finishReason": "STOP"}], "#,
        r#""usageMetadata": {"promptTokenCount": 38116, "candidatesTokenCount": 131, "thoughtsTokenCount": 846, "cachedContentTokenCount": 35942}}"#,
        "\n\n",
    );

    /// A Gemini stream cut off by the output limit, all of it thinking.
    const GEMINI_CUT_OFF_STREAM: &str = concat!(
        r#"data: {"candidates": [{"content": {"parts": [{"text": "and again", "thought": true}], "role": "model"}}], "usageMetadata": {}}"#,
        "\n\n",
        r#"data: {"candidates": [{"content": {"parts": [{"text": ""}], "role": "model"}, "finishReason": "MAX_TOKENS"}], "#,
        r#""usageMetadata": {"promptTokenCount": 151, "candidatesTokenCount": 3, "thoughtsTokenCount": 32765}}"#,
        "\n\n",
    );

    /// What a stand-in server was asked.
    struct Asked {
        uri: String,
        key: Option<String>,
        authorization: Option<String>,
        body: Value,
    }

    /// A stand-in for a Gemini server that answers every request with
    /// `stream`, and the endpoint that calls it, with what it was asked.
    async fn gemini_stand_in(stream: &'static str) -> (Endpoint, std::sync::Arc<Mutex<Vec<Asked>>>) {
        let asked = std::sync::Arc::new(Mutex::new(Vec::new()));
        let sink = asked.clone();
        let app = Router::new().fallback(move |uri: axum::http::Uri, headers: axum::http::HeaderMap, Json(body): Json<Value>| {
            let sink = sink.clone();
            async move {
                let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(String::from);
                sink.lock().unwrap().push(Asked { uri: uri.to_string(), key: header("x-goog-api-key"), authorization: header("authorization"), body });
                ([("content-type", "text/event-stream")], stream)
            }
        });
        let base = format!("{}beta", serve(app).await);
        let model = "provider/gemini-x-high".to_string();
        (Endpoint { name: "stand-in".into(), api: Api::Gemini, base, key: "gk".into(), model: model.clone(), fallback_model: model }, asked)
    }

    #[tokio::test]
    async fn a_gemini_server_is_asked_in_its_own_shape_and_its_answer_read_without_the_thinking() {
        let (endpoint, asked) = gemini_stand_in(GEMINI_STREAM).await;
        let llm = Llm::new(&endpoint, 1).unwrap();
        let req = Request {
            messages: vec![
                json!({"role": "system", "content": "the rules"}),
                json!({"role": "user", "content": [
                    {"type": "text", "text": "<<<PAPER"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}},
                    {"type": "file", "file": {"filename": "2610.39393.pdf", "file_data": "data:application/pdf;base64,JVBERi0x"}},
                    {"type": "text", "text": "END PAPER>>>"},
                ]}),
                json!({"role": "user", "content": "the task"}),
            ],
            effort: "high",
            schema: Some(("answer", json!({"type": "object", "properties": {"word": {"type": "string"}}}))),
            affinity: "job-1",
        };
        let (text, usage) = llm.chat(&req).await.unwrap();
        assert_eq!(text, "ready", "the summary of the thinking is not part of the answer");
        // Output is the answer and the thinking together, as it is billed.
        assert_eq!((usage.prompt, usage.cached, usage.completion, usage.reasoning, usage.calls), (38116, 35942, 131 + 846, 846, 1));
        let priced = ((38116.0 - 35942.0) * PRICE_INPUT + 35942.0 * PRICE_CACHED + 977.0 * PRICE_OUTPUT) / 1e6;
        assert!((usage.cost_usd - priced).abs() < 1e-12, "{} against {priced}", usage.cost_usd);

        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].uri, "/v1beta/models/provider/gemini-x-high:streamGenerateContent?alt=sse");
        assert_eq!((asked[0].key.as_deref(), asked[0].authorization.as_deref()), (Some("gk"), None), "the key goes the way Gemini takes it");
        let body = &asked[0].body;
        assert_eq!(body["systemInstruction"], json!({"parts": [{"text": "the rules"}]}));
        assert_eq!(
            body["contents"],
            json!([
                {"role": "user", "parts": [
                    {"text": "<<<PAPER"},
                    {"inlineData": {"mimeType": "image/png", "data": "QUJD"}},
                    {"inlineData": {"mimeType": "application/pdf", "data": "JVBERi0x"}},
                    {"text": "END PAPER>>>"},
                ]},
                {"role": "user", "parts": [{"text": "the task"}]},
            ])
        );
        let config = &body["generationConfig"];
        assert_eq!(config["maxOutputTokens"], json!(MAX_OUTPUT_TOKENS));
        assert_eq!(config["thinkingConfig"], json!({"includeThoughts": true}), "without it the stream is silent while the model thinks");
        assert_eq!((config["responseMimeType"].as_str(), &config["responseJsonSchema"]["properties"]["word"]), (Some("application/json"), &json!({"type": "string"})));
        for openai_only in ["model", "messages", "reasoning_effort", "stream"] {
            assert!(body.get(openai_only).is_none(), "{openai_only} in {body}");
        }
    }

    #[tokio::test]
    async fn a_gemini_answer_that_reaches_the_output_limit_is_rejected_and_not_asked_again() {
        let (endpoint, asked) = gemini_stand_in(GEMINI_CUT_OFF_STREAM).await;
        let llm = Llm::new(&endpoint, 1).unwrap();
        let err = llm.chat(&request(vec![json!({"role": "user", "content": "hi"})])).await.unwrap_err();
        let Some(LlmError::Cut(used)) = err.downcast_ref::<LlmError>() else { panic!("{err:#}") };
        assert_eq!((used.prompt, used.completion, used.reasoning), (151, 32768, 32765));
        assert_eq!(asked.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_message_part_gemini_has_no_form_for_is_refused_rather_than_left_out() {
        let req = request(vec![json!({"role": "user", "content": [{"type": "input_audio", "input_audio": {"data": "QUJD"}}]})]);
        let err = gemini_body(&req).unwrap_err().to_string();
        assert!(err.contains("input_audio"), "{err}");
    }
}
