//! Browser QA: load a draft course in headless Chromium and see what a
//! visitor would hit.
//!
//! - Chromium's own sandbox stays on (chromiumoxide's default). It runs
//!   model-written code, so never add --no-sandbox.
//! - Every request is intercepted. Drafts are served from memory at a fake
//!   origin, so no draft is ever served on a port; the pinned CDN paths and
//!   the paper's own arXiv figures pass; everything else (GA included, so QA
//!   isn't a visit) fails and is recorded.
//! - Each lab is tested on a page of its own, so a hang or an error is pinned
//!   on the right lab, then the full page is checked for page-level errors,
//!   KaTeX errors and overflow at phone width.
//! - Runs take turns (one browser at a time), and the browser exits after
//!   each run, so nothing stays resident.

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::prelude::BASE64_STANDARD;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
use chromiumoxide::cdp::browser_protocol::fetch::{
    ContinueRequestParams, EventRequestPaused, FailRequestParams, FulfillRequestParams, HeaderEntry,
};
use chromiumoxide::cdp::browser_protocol::network::ErrorReason;
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use chromiumoxide::cdp::js_protocol::runtime::{EvaluateParams, EventConsoleApiCalled, EventExceptionThrown};
use chromiumoxide::page::ScreenshotParams;
use chromiumoxide::Page;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;

pub const ORIGIN: &str = "https://qa.paper-bro.invalid";

/// One browser at a time, across every job.
static TURN: AsyncMutex<()> = AsyncMutex::const_new(());

/// Each run's browser profile lives in a fresh temp directory with this
/// prefix, removed when the run ends.
const PROFILE_PREFIX: &str = "paper-bro-qa-";

/// Remove profiles a previous process left behind: stopped mid-QA, it never
/// got to remove its own. A run takes a minute or two, so anything older
/// than an hour can't be a live one.
pub fn remove_stale_profiles() -> usize {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else { return 0 };
    let mut removed = 0;
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(PROFILE_PREFIX) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(3600));
        if stale && std::fs::remove_dir_all(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LabReport {
    pub lesson: usize,
    pub ok: bool,
    pub problems: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshot_jpeg_b64: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Report {
    pub page_problems: Vec<String>,
    pub labs: Vec<LabReport>,
}

impl Report {
    pub fn lab(&self, lesson: usize) -> Option<&LabReport> {
        self.labs.iter().find(|l| l.lesson == lesson)
    }
}

/// `pages` maps a path under ORIGIN to its HTML. `allowed` are URL prefixes
/// that may load from the network.
struct Served {
    pages: HashMap<String, String>,
    allowed: Vec<String>,
    blocked: Mutex<Vec<String>>,
}

pub struct Input<'a> {
    pub course_html: String,
    /// `(lesson number from 1, page html)` for each lab to test.
    pub lab_pages: Vec<(usize, String)>,
    pub figure_prefix: Option<String>,
    pub chrome: &'a Path,
    pub screenshots: bool,
}

pub async fn run(input: Input<'_>) -> Result<Report> {
    let _turn = TURN.lock().await;
    let mut pages = HashMap::new();
    pages.insert(format!("{ORIGIN}/course"), input.course_html.clone());
    for (n, html) in &input.lab_pages {
        pages.insert(format!("{ORIGIN}/lab/{n}"), html.clone());
    }
    let mut allowed = vec![
        "https://cdn.jsdelivr.net/npm/katex@0.16.47/".to_string(),
        "https://cdn.jsdelivr.net/npm/@fontsource/".to_string(),
    ];
    allowed.extend(input.figure_prefix.clone());
    let served = Arc::new(Served { pages, allowed, blocked: Mutex::new(Vec::new()) });

    let mut report = Report::default();
    let mut session = Session::launch(input.chrome).await?;
    let mut any_hung = false;
    for (n, _) in &input.lab_pages {
        let (lab, hung) = check_lab(&session.browser, &served, *n, input.screenshots).await?;
        report.labs.push(lab);
        if hung {
            // A frozen renderer can be reused by the next page, so start over
            // with a fresh browser for whatever is left.
            any_hung = true;
            session.close().await;
            session = Session::launch(input.chrome).await?;
        }
    }
    report.page_problems = if any_hung {
        vec!["the full-page check was skipped because a lab hangs".into()]
    } else {
        check_course(&session.browser, &served).await?
    };
    session.close().await;
    // Anything an earlier run still left behind is cleared here too, so
    // leftovers never pile up between restarts.
    remove_stale_profiles();
    let blocked = served.blocked.lock().unwrap().clone();
    for url in blocked {
        report.page_problems.push(format!("blocked a request to {url}"));
    }
    Ok(report)
}

struct Session {
    browser: Browser,
    pump: tokio::task::JoinHandle<()>,
    profile: std::path::PathBuf,
}

impl Session {
    async fn launch(chrome: &Path) -> Result<Self> {
        let profile = std::env::temp_dir().join(format!("{PROFILE_PREFIX}{}-{}", std::process::id(), fastrand::u64(..)));
        let config = BrowserConfig::builder()
            .chrome_executable(chrome)
            .user_data_dir(&profile)
            .enable_request_intercept()
            .disable_cache()
            .window_size(1280, 900)
            .viewport(None)
            .launch_timeout(Duration::from_secs(30))
            .request_timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| anyhow!("browser config: {e}"))?;
        let (browser, mut handler) = Browser::launch(config).await.context("launching chrome-headless-shell")?;
        let pump = tokio::spawn(async move { while handler.next().await.is_some() {} });
        Ok(Self { browser, pump, profile })
    }

    async fn close(mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(5), self.browser.close()).await;
        let _ = self.browser.kill().await;
        let _ = tokio::time::timeout(Duration::from_secs(5), self.browser.wait()).await;
        self.pump.abort();
        // Chrome's helper processes can outlive the main one by a moment and
        // recreate files in the profile, so try again after a pause.
        for _ in 0..3 {
            let _ = std::fs::remove_dir_all(&self.profile);
            if !self.profile.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

/// A fresh page wired to the interception rules, recording page errors.
async fn open(browser: &Browser, served: &Arc<Served>) -> Result<(Page, Arc<Mutex<Vec<String>>>)> {
    let page = browser.new_page("about:blank").await?;
    let errors = Arc::new(Mutex::new(Vec::new()));

    let mut paused = page.event_listener::<EventRequestPaused>().await?;
    let (p, s) = (page.clone(), served.clone());
    tokio::spawn(async move {
        while let Some(ev) = paused.next().await {
            let url = ev.request.url.clone();
            let id = ev.request_id.clone();
            log::debug!("qa request {url}");
            let outcome = if let Some(html) = s.pages.get(&url) {
                let headers = vec![HeaderEntry::new("Content-Type", "text/html; charset=utf-8")];
                p.execute(
                    FulfillRequestParams::builder()
                        .request_id(id)
                        .response_code(200)
                        .response_headers(headers)
                        .body(BASE64_STANDARD.encode(html))
                        .build()
                        .unwrap(),
                )
                .await
                .map(|_| ())
            } else if url == format!("{ORIGIN}/paper-bro/api/lab-errors") {
                p.execute(FulfillRequestParams::builder().request_id(id).response_code(204).build().unwrap())
                    .await
                    .map(|_| ())
            } else if s.allowed.iter().any(|a| url.starts_with(a)) {
                p.execute(ContinueRequestParams::new(id)).await.map(|_| ())
            } else {
                let quiet = url.contains("googletagmanager.com") || url.contains("google-analytics.com");
                if !quiet {
                    s.blocked.lock().unwrap().push(url.chars().take(160).collect());
                }
                p.execute(FailRequestParams::new(id, ErrorReason::BlockedByClient)).await.map(|_| ())
            };
            if let Err(e) = outcome {
                log::debug!("interception reply failed for {url}: {e}");
            }
        }
    });

    let mut exceptions = page.event_listener::<EventExceptionThrown>().await?;
    let e1 = errors.clone();
    tokio::spawn(async move {
        while let Some(ev) = exceptions.next().await {
            let d = &ev.exception_details;
            let msg = d.exception.as_ref().and_then(|x| x.description.clone()).unwrap_or_else(|| d.text.clone());
            e1.lock().unwrap().push(format!("uncaught: {}", msg.lines().next().unwrap_or("")));
        }
    });
    let mut console = page.event_listener::<EventConsoleApiCalled>().await?;
    let e2 = errors.clone();
    tokio::spawn(async move {
        while let Some(ev) = console.next().await {
            let kind = format!("{:?}", ev.r#type).to_ascii_lowercase();
            if kind == "error" || kind == "assert" {
                let text: Vec<String> = ev
                    .args
                    .iter()
                    .map(|a| a.value.as_ref().map(|v| v.to_string()).or(a.description.clone()).unwrap_or_default())
                    .collect();
                e2.lock().unwrap().push(format!("console.error: {}", text.join(" ")));
            }
        }
    });
    Ok((page, errors))
}

async fn eval(page: &Page, js: &str, wait: Duration) -> Result<Value> {
    let params = EvaluateParams::builder()
        .expression(js)
        .await_promise(true)
        .return_by_value(true)
        .build()
        .map_err(|e| anyhow!("{e}"))?;
    let res = tokio::time::timeout(wait, page.evaluate_expression(params))
        .await
        .map_err(|_| anyhow!("timed out after {wait:?}"))??;
    Ok(res.into_value::<Value>().unwrap_or(Value::Null))
}

/// Poll until the runtime says every lab has reported in.
async fn wait_ready(page: &Page, limit: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < limit {
        if let Ok(Value::Bool(true)) = eval(page, "!!(window.__pbQa && window.__pbQa.ready)", Duration::from_secs(3)).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

/// The lab's report, and whether it froze the renderer.
async fn check_lab(browser: &Browser, served: &Arc<Served>, n: usize, screenshots: bool) -> Result<(LabReport, bool)> {
    log::debug!("qa lab {n}: open");
    let (page, errors) = open(browser, served).await?;
    log::debug!("qa lab {n}: goto");
    let mut r = LabReport { lesson: n, ok: false, ..Default::default() };
    let nav = tokio::time::timeout(Duration::from_secs(20), page.goto(format!("{ORIGIN}/lab/{n}"))).await;
    if !matches!(nav, Ok(Ok(_))) {
        r.problems.push("the lab page didn't finish loading (it may hang on load)".into());
        return Ok((r, true));
    }
    log::debug!("qa lab {n}: loaded");
    let mut hung = false;
    if !wait_ready(&page, Duration::from_secs(18)).await {
        // The runtime gives up on a silent lab after 15 s by itself, so no
        // answer at all means the page's own thread is frozen.
        r.problems.push("the lab froze the page while starting (an endless loop?)".into());
        hung = true;
    }
    let state = eval(&page, &format!("JSON.stringify(window.__pbQa.labs.l{n} || null)"), Duration::from_secs(5))
        .await
        .ok()
        .and_then(|v| v.as_str().and_then(|s| serde_json::from_str::<Value>(s).ok()))
        .unwrap_or(Value::Null);
    match state["state"].as_str() {
        Some("ready") => {}
        Some("failed") => r.problems.push("the lab threw an error while starting and showed its fallback".into()),
        Some("silent") | Some("loading") => r.problems.push("the lab never started (hung, or a syntax error)".into()),
        _ => r.problems.push("the lab didn't load at all".into()),
    }
    for e in state["errors"].as_array().into_iter().flatten() {
        r.problems.push(format!("error: {}", e.as_str().unwrap_or("")));
    }
    if state["state"].as_str() == Some("ready") {
        match eval(&page, &format!("window.__pbQa.selftest('l{n}').then(r => JSON.stringify(r))"), Duration::from_secs(25)).await {
            Err(_) => {
                r.problems.push("the lab froze while its controls were being used (an endless loop?)".into());
                hung = true;
            }
            Ok(v) => {
                let t: Value = v.as_str().and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Null);
                let known: Vec<String> = state["errors"].as_array().into_iter().flatten().filter_map(|e| e.as_str().map(String::from)).collect();
                for e in t["errors"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                    if !known.iter().any(|k| k == e) {
                        r.problems.push(format!("error while using the controls: {e}"));
                    }
                }
                if let Some(b) = t["blank"].as_array().filter(|b| !b.is_empty()) {
                    r.problems.push(format!("{} of {} canvases stayed blank", b.len(), t["canvases"]));
                }
                if t["responsive"] == Value::Bool(false) {
                    r.problems.push(format!("moving the {} controls changed nothing on screen", t["controls"]));
                }
                if t["controls"] == Value::from(0) {
                    r.problems.push("the lab has no controls to interact with".into());
                }
                for b in t["badText"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                    r.problems.push(format!("shows '{b}' on screen"));
                }
            }
        }
        if screenshots && !hung {
            let params = ScreenshotParams::builder().format(CaptureScreenshotFormat::Jpeg).quality(70).full_page(true).build();
            if let Ok(Ok(bytes)) = tokio::time::timeout(Duration::from_secs(15), page.screenshot(params)).await {
                r.screenshot_jpeg_b64 = Some(BASE64_STANDARD.encode(bytes));
            }
        }
    }
    for e in errors.lock().unwrap().iter() {
        r.problems.push(e.clone());
    }
    r.ok = r.problems.is_empty();
    if !hung {
        let _ = tokio::time::timeout(Duration::from_secs(5), page.close()).await;
    }
    Ok((r, hung))
}

async fn check_course(browser: &Browser, served: &Arc<Served>) -> Result<Vec<String>> {
    let (page, errors) = open(browser, served).await?;
    let mut problems = Vec::new();
    let nav = tokio::time::timeout(Duration::from_secs(30), page.goto(format!("{ORIGIN}/course"))).await;
    if !matches!(nav, Ok(Ok(_))) {
        problems.push("the course page didn't finish loading".into());
        return Ok(problems);
    }
    wait_ready(&page, Duration::from_secs(20)).await;
    if let Ok(Value::Array(k)) = eval(&page, "window.__pbQa.katexErrors()", Duration::from_secs(5)).await {
        for e in k {
            problems.push(format!("math that KaTeX couldn't render: {}", e.as_str().unwrap_or("")));
        }
    }
    let phone = SetDeviceMetricsOverrideParams::new(390, 844, 2.0, true);
    if page.execute(phone).await.is_ok() {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Ok(v) = eval(&page, "document.documentElement.scrollWidth - window.innerWidth", Duration::from_secs(5)).await {
            if v.as_f64().unwrap_or(0.0) > 1.0 {
                problems.push(format!("the page scrolls sideways at phone width ({v} px too wide)"));
            }
        }
    }
    for e in errors.lock().unwrap().iter() {
        problems.push(e.clone());
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), page.close()).await;
    Ok(problems)
}
