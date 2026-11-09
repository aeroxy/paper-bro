//! HTTP: the public site under /paper-bro (behind a reverse proxy), and the
//! admin routes on a second listener that only takes local connections and
//! that the proxy never routes.
//!
//! No login, deepwiki-style. A GET never starts a build (crawlers follow
//! links); only the Build button's JSON POST does, and admission is where the
//! cost controls live: one course per paper, a per-IP allowance, a daily cap,
//! a queue cap, and a daily spend guard.

use axum::body::Body;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use regex::Regex;
use rust_embed::RustEmbed;
use serde::Deserialize;
use serde_json::{Value, json};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use crate::app::App;
use crate::arxiv::{self, PaperId};
use crate::config::BASE;
use crate::db;
use crate::github;

#[derive(RustEmbed)]
#[folder = "frontend/dist"]
struct Assets;

/// Event kinds kept out of the public progress feed: what visitors report,
/// lab errors from their browsers, and internal build notes (cost, QA and
/// review details). The admin listener shows them.
pub const PRIVATE_STAGES: &[&str] = &["report", "visitor-error", "notes"];

/// The message a retryable failure is stored with (see pipeline::process).
const GENERIC_FAILURE: &str = "paper-bro hit a problem building this course.";

type AppState = State<Arc<App>>;

pub fn public(app: Arc<App>) -> Router {
    let api = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/paper", get(paper))
        .route("/api/build", post(build))
        .route("/api/events", get(events))
        .route("/api/courses", get(courses))
        .route("/api/lab-errors", post(lab_error))
        .route("/api/report", post(report))
        .route("/sitemap.xml", get(sitemap))
        .route("/abs/{*id}", get(paper_page))
        .route("/pdf/{*id}", get(paper_page))
        .route("/html/{*id}", get(paper_page))
        .route("/preview/{alias}", get(preview))
        .fallback(get(static_file));
    // axum's nest doesn't hand `/paper-bro/` itself to the nested fallback.
    Router::new()
        .nest(BASE, api)
        .route(BASE, get(|| async { Redirect::permanent("/paper-bro/") }))
        .route("/paper-bro/", get(static_file))
        .with_state(app)
}

pub fn admin(app: Arc<App>) -> Router {
    Router::new()
        .route("/jobs", get(admin_jobs))
        .route("/events/{*id}", get(admin_events))
        .route("/retry/{*id}", post(admin_retry))
        .route("/rebuild/{*id}", post(admin_rebuild))
        .route("/remove/{*id}", post(admin_remove))
        .with_state(app)
}

fn err(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({"error": code, "message": message}))).into_response()
}

fn internal(e: anyhow::Error) -> Response {
    log::error!("request failed: {e:#}");
    err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "Something went wrong on paper-bro's side.")
}

/// The visitor's address. Behind a reverse proxy on the same host the peer is
/// always loopback, and the proxy sets X-Forwarded-For to the real client;
/// nobody else is trusted to.
fn client_ip(peer: SocketAddr, headers: &HeaderMap) -> String {
    if peer.ip().is_loopback() {
        if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            if let Some(last) = xff.rsplit(',').next() {
                return last.trim().to_string();
            }
        }
    }
    peer.ip().to_string()
}

#[derive(Deserialize)]
struct IdQuery {
    id: String,
}

fn parse_id(raw: &str) -> Result<PaperId, Response> {
    PaperId::parse(raw).ok_or_else(|| {
        err(StatusCode::BAD_REQUEST, "bad_id", "That doesn't look like an arXiv paper. Try an id like 1706.03762 or an arxiv.org link.")
    })
}

async fn status_json(app: &App, id: &str) -> Result<Value, Response> {
    let course = app.course(id);
    let meta = match app.preview(id).await {
        Ok(m) => m,
        Err(e) => {
            log::warn!("arXiv lookup for {id} failed: {e:#}");
            return Err(err(StatusCode::BAD_GATEWAY, "arxiv_unavailable", "arXiv didn't answer just now. Try again in a minute."));
        }
    };
    let Some(meta) = meta else {
        return Err(err(StatusCode::NOT_FOUND, "no_such_paper", "arXiv has no paper with that id."));
    };
    let position = match &course {
        Some(c) if c.status == "queued" => {
            let courses = app.courses.read().unwrap();
            Some(courses.values().filter(|o| o.status == "queued" && o.requested_at < c.requested_at).count() + 1)
        }
        _ => None,
    };
    let retry_at = course.as_ref().and_then(retry_opens);
    let retryable = retry_at.is_some_and(|t| chrono::Utc::now() >= t);
    Ok(json!({
        "id": meta.id,
        "alias": arxiv::alias(&meta.id),
        "abs_url": arxiv::abs_url(&meta.id),
        "title": meta.title,
        "authors": meta.authors,
        "abstract": meta.abstract_,
        "categories": meta.categories,
        "version": meta.version,
        "published": meta.published,
        "status": course.as_ref().map_or("none", |c| c.status.as_str()),
        "stage": course.as_ref().and_then(|c| c.stage.clone()),
        "url": course.as_ref().and_then(|c| c.aero_url.clone()),
        "error": course.as_ref().and_then(|c| c.error.clone()),
        "retryable": retryable,
        "retry_at": retry_at,
        "queue_position": position,
    }))
}

async fn paper(State(app): AppState, Query(q): Query<IdQuery>) -> Response {
    let id = match parse_id(&q.id) {
        Ok(id) => id,
        Err(r) => return r,
    };
    match status_json(&app, &id.id).await {
        Ok(v) => Json(v).into_response(),
        Err(r) => r,
    }
}

/// Every published course at its lasting address (`course::course_url`), for
/// search engines. A course page names the same address as its canonical URL,
/// and the address redirects to wherever the page is hosted, so a move to
/// another host changes no entry here.
async fn sitemap(State(app): AppState) -> Response {
    let courses: Vec<db::CourseRow> = app.courses.read().unwrap().values().cloned().collect();
    ([(header::CONTENT_TYPE, "application/xml; charset=utf-8")], sitemap_xml(&app.cfg.public_origin, &courses)).into_response()
}

fn sitemap_xml(origin: &str, courses: &[db::CourseRow]) -> String {
    let mut published: Vec<&db::CourseRow> =
        courses.iter().filter(|c| c.status == "ready" && c.aero_page_id.as_deref().is_some_and(|p| p != "preview")).collect();
    published.sort_by(|a, b| a.arxiv_id.cmp(&b.arxiv_id));
    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n<url><loc>{origin}{BASE}/</loc></url>\n"
    );
    for c in published {
        let lastmod = c.finished_at.map(|t| format!("<lastmod>{}</lastmod>", t.format("%Y-%m-%d"))).unwrap_or_default();
        xml.push_str(&format!("<url><loc>{}</loc>{lastmod}</url>\n", crate::course::course_url(origin, &c.arxiv_id)));
    }
    xml.push_str("</urlset>\n");
    xml
}

/// When a course that failed for no reason of its own may be built again: an
/// hour after its last attempt, so a build that keeps breaking isn't paid for
/// over and over. The paper API and the build endpoint both go by this, so
/// the page never offers a retry the server would ignore. `None` when the
/// course can't be retried at all.
fn retry_opens(c: &db::CourseRow) -> Option<chrono::DateTime<chrono::Utc>> {
    if c.status != "failed" || c.error.as_deref() != Some(GENERIC_FAILURE) {
        return None;
    }
    c.finished_at.map(|t| t + chrono::Duration::hours(1))
}

#[derive(Deserialize)]
struct BuildBody {
    id: String,
}

async fn build(
    State(app): AppState,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<BuildBody>,
) -> Response {
    // Json<…> already insists on Content-Type: application/json, which a
    // cross-site form can't send without a CORS preflight we never answer.
    let id = match parse_id(&body.id) {
        Ok(id) => id.id,
        Err(r) => return r,
    };
    if let Some(c) = app.course(&id) {
        let retry_failed = retry_opens(&c).is_some_and(|t| chrono::Utc::now() >= t);
        if !retry_failed {
            return match status_json(&app, &id).await {
                Ok(v) => Json(v).into_response(),
                Err(r) => r,
            };
        }
    }
    let (built_today, queued, spent) = match db::today(&app.pool).await {
        Ok(t) => t,
        Err(e) => return internal(e),
    };
    let cfg = &app.cfg;
    if built_today >= cfg.builds_per_day as i64 || spent >= cfg.daily_budget_usd {
        return err(StatusCode::TOO_MANY_REQUESTS, "daily_limit", "paper-bro has built as many courses as it can today. Come back tomorrow.");
    }
    if queued >= cfg.max_queued {
        return err(StatusCode::TOO_MANY_REQUESTS, "queue_full", "The queue is full right now. Try again in a little while.");
    }
    let meta = match app.preview(&id).await {
        Ok(Some(m)) => m,
        Ok(None) => return err(StatusCode::NOT_FOUND, "no_such_paper", "arXiv has no paper with that id."),
        Err(e) => {
            log::warn!("arXiv lookup for {id} failed: {e:#}");
            return err(StatusCode::BAD_GATEWAY, "arxiv_unavailable", "arXiv didn't answer just now. Try again in a minute.");
        }
    };
    let ip = client_ip(peer, &headers);
    if !app.take_build(&ip) {
        return err(StatusCode::TOO_MANY_REQUESTS, "ip_limit", "You've started as many courses as one visitor can today. Come back tomorrow.");
    }
    // Two visitors asking for the same paper at the same moment both get
    // here; the primary key on courses lets exactly one insert win. The other
    // just joins the existing job, so it gets its allowance back.
    let queued = async {
        db::upsert_paper(&app.pool, &meta).await?;
        let started = db::enqueue(&app.pool, &id).await? || db::requeue(&app.pool, &id, false).await?;
        app.refresh(&id).await?;
        anyhow::Ok(started)
    }
    .await;
    match queued {
        Ok(true) => {
            app.emit(&id, "queued", "Waiting for a builder").await;
            app.wake.notify_one();
        }
        Ok(false) => app.refund_build(&ip),
        Err(e) => return internal(e),
    }
    match status_json(&app, &id).await {
        Ok(v) => (StatusCode::ACCEPTED, Json(v)).into_response(),
        Err(r) => r,
    }
}

async fn events(State(app): AppState, Query(q): Query<IdQuery>) -> Response {
    let id = match parse_id(&q.id) {
        Ok(id) => id.id,
        Err(r) => return r,
    };
    if app.course(&id).is_none() {
        return err(StatusCode::NOT_FOUND, "no_course", "Nobody has asked for this course yet.");
    }
    // Subscribe before replaying, so nothing falls between the two.
    let mut live = app.subscribe(&id);
    let past = match db::events(&app.pool, &id).await {
        Ok(p) => p,
        Err(e) => return internal(e),
    };
    // Only the current build's events: a rebuilt course also has the old
    // build's "ready" in its history, and replaying it would end the feed
    // (and send the page round a reload loop) before the new build is done.
    // A build ends with a terminal event, so while one is running it is
    // everything after the last terminal event, and once it has finished it
    // is everything after the one before.
    let terminal: Vec<usize> = past
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e.stage.as_str(), "ready" | "failed" | "removed"))
        .map(|(i, _)| i)
        .collect();
    let building = app.course(&id).is_some_and(|c| matches!(c.status.as_str(), "queued" | "running"));
    let current = match (building, terminal.as_slice()) {
        (true, [.., last]) => last + 1,
        (false, [.., before, _]) => before + 1,
        _ => 0,
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<db::Event>(64);
    tokio::spawn(async move {
        let mut last = 0;
        for ev in past.into_iter().skip(current) {
            last = ev.id;
            if PRIVATE_STAGES.contains(&ev.stage.as_str()) {
                continue;
            }
            let end = matches!(ev.stage.as_str(), "ready" | "failed");
            if tx.send(ev).await.is_err() || end {
                return;
            }
        }
        while let Ok(ev) = live.recv().await {
            if ev.id <= last || PRIVATE_STAGES.contains(&ev.stage.as_str()) {
                continue;
            }
            let end = matches!(ev.stage.as_str(), "ready" | "failed");
            if tx.send(ev).await.is_err() || end {
                return;
            }
        }
    });
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        let ev = rx.recv().await?;
        let data = serde_json::to_string(&ev).unwrap_or_default();
        Some((Ok::<_, Infallible>(SseEvent::default().event("progress").data(data)), rx))
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))).into_response()
}

async fn courses(State(app): AppState) -> Response {
    let mut list: Vec<_> = app.courses.read().unwrap().values().filter(|c| c.status == "ready").cloned().collect();
    list.sort_by(|a, b| b.finished_at.cmp(&a.finished_at));
    let list: Vec<Value> = list
        .into_iter()
        .take(60)
        .map(|c| json!({"id": c.arxiv_id, "title": c.title, "url": c.aero_url, "finished_at": c.finished_at}))
        .collect();
    Json(json!({"courses": list})).into_response()
}

/// Lab errors from visitors' browsers (sendBeacon, text/plain). The first
/// report of each distinct error is recorded; repeats cost nothing.
async fn lab_error(State(app): AppState, body: String) -> StatusCode {
    let Ok(v) = serde_json::from_str::<Value>(&body) else { return StatusCode::BAD_REQUEST };
    let (Some(id), Some(lab), Some(msg)) = (v["id"].as_str(), v["lab"].as_str(), v["message"].as_str()) else {
        return StatusCode::BAD_REQUEST;
    };
    if app.course(id).is_none_or(|c| c.status != "ready") || lab.len() > 8 {
        return StatusCode::NO_CONTENT;
    }
    let msg: String = msg.chars().take(300).collect();
    if app.first_lab_error(format!("{id}|{lab}|{msg}")) {
        app.emit(id, "visitor-error", &format!("{lab}: {msg}")).await;
    }
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct ReportBody {
    id: String,
    reason: String,
}

async fn report(
    State(app): AppState,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<ReportBody>,
) -> Response {
    let id = match parse_id(&body.id) {
        Ok(id) => id.id,
        Err(r) => return r,
    };
    if app.course(&id).is_none() {
        return err(StatusCode::NOT_FOUND, "no_course", "There's no course for that paper.");
    }
    let reason: String = body.reason.trim().chars().take(1000).collect();
    if reason.is_empty() {
        return err(StatusCode::BAD_REQUEST, "empty", "Tell us what's wrong.");
    }
    // One report per visitor per course per run of the server.
    if app.first_lab_error(format!("report|{}|{id}", client_ip(peer, &headers))) {
        app.emit(&id, "report", &reason).await;
    }
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
struct PageQuery {
    report: Option<String>,
}

/// `/paper-bro/abs/1706.03762`: straight to the course if it exists,
/// otherwise the app's paper page. `?report=1` always shows the app.
async fn paper_page(State(app): AppState, Path(raw): Path<String>, Query(q): Query<PageQuery>) -> Response {
    if let Some(id) = PaperId::parse(&raw) {
        if q.report.is_none() {
            if let Some(url) = app.course(&id.id).filter(|c| c.status == "ready").and_then(|c| c.aero_url) {
                return Redirect::to(&url).into_response();
            }
        }
    }
    index()
}

/// Dry-run only: serve a finished course from the database.
async fn preview(State(app): AppState, Path(alias): Path<String>) -> Response {
    if app.cfg.publish {
        return StatusCode::NOT_FOUND.into_response();
    }
    let id = app.courses.read().unwrap().keys().find(|id| arxiv::alias(id) == alias).cloned();
    let Some(id) = id else { return StatusCode::NOT_FOUND.into_response() };
    match db::artifact(&app.pool, &id, "html").await {
        Ok(Some(html)) => ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => internal(e),
    }
}

async fn static_file(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = path.strip_prefix("paper-bro/").unwrap_or(path);
    if !path.is_empty() && path != "index.html" && Assets::get(path).is_some() {
        return asset(path);
    }
    index()
}

/// The Google Analytics property AERO uses, built in so that the app's own pages
/// and the course pages count toward the same totals. AERO added the tag to
/// the courses it served; on GitHub Pages they carry it themselves (course.rs).
pub const GA_ID: &str = "G-91SNPT9G03";

/// GitHub Pages' copy of the front end's built files, published from `main` by
/// .github/workflows/pages.yml, so that visitors fetch them from GitHub's CDN.
const CDN: &str = "https://aeroxy.github.io/paper-bro/";

/// The app's page, with the analytics tag, loading its files from the CDN.
fn index() -> Response {
    let Some(file) = Assets::get("index.html") else {
        return asset("index.html");
    };
    let html = with_cdn(&with_analytics(&String::from_utf8_lossy(&file.data), GA_ID), CDN);
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-cache")], html).into_response()
}

/// The stock gtag.js bootstrap, the same one AERO's pages carry, right after
/// `<head>`. GA's history tracking counts the app's client-side page changes.
fn with_analytics(html: &str, id: &str) -> String {
    let tag = analytics_tag(id);
    let at = html.find("<head>").map_or(0, |i| i + "<head>".len());
    let mut out = html.to_string();
    out.insert_str(at, &tag);
    out
}

/// The stock gtag.js bootstrap for property `id`.
pub fn analytics_tag(id: &str) -> String {
    format!(
        r#"<script async src="https://www.googletagmanager.com/gtag/js?id={id}"></script><script>window.dataLayer=window.dataLayer||[];function gtag(){{dataLayer.push(arguments)}}gtag("js",new Date());gtag("config","{id}");</script>"#
    )
}

/// The page's script and stylesheet tags, as Vite writes them.
static ASSET_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r#"<(script|link)([^>]*?) (?:src|href)="{}/(assets/[^"]+)""#, regex::escape(BASE))).unwrap());

/// Points the page's script and stylesheet at the CDN, each falling back to this
/// server's own copy when the CDN doesn't have it: a deploy can run ahead of the
/// Pages workflow, Pages keeps only the latest build, and a fork's build isn't
/// there at all. A file the CDN has under the same name is the same file, since
/// the name carries a hash of its content.
fn with_cdn(html: &str, cdn: &str) -> String {
    ASSET_TAG
        .replace_all(html, |c: &regex::Captures| {
            let (tag, attrs, path) = (&c[1], &c[2], &c[3]);
            let local = format!("{BASE}/{path}");
            if tag == "script" {
                format!(r#"<script{attrs} src="{cdn}{path}" onerror="var s=document.createElement('script');s.type='module';s.src='{local}';document.head.append(s)""#)
            } else {
                format!(r#"<link{attrs} href="{cdn}{path}" onerror="this.onerror=null;this.href='{local}'""#)
            }
        })
        .into_owned()
}

fn asset(path: &str) -> Response {
    let Some(file) = Assets::get(path) else {
        return (StatusCode::NOT_FOUND, "The paper-bro front end isn't built (cd frontend && bun run build).").into_response();
    };
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let cache = if path.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
    let mut resp = Response::new(Body::from(file.data.into_owned()));
    resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_str(mime.as_ref()).unwrap());
    resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    resp
}

async fn admin_jobs(State(app): AppState) -> Response {
    let mut list: Vec<_> = app.courses.read().unwrap().values().cloned().collect();
    list.sort_by(|a, b| b.requested_at.cmp(&a.requested_at));
    Json(list).into_response()
}

async fn admin_events(State(app): AppState, Path(raw): Path<String>) -> Response {
    let Some(id) = PaperId::parse(&raw) else { return StatusCode::BAD_REQUEST.into_response() };
    match db::events(&app.pool, &id.id).await {
        Ok(e) => Json(e).into_response(),
        Err(e) => internal(e),
    }
}

async fn admin_retry(State(app): AppState, Path(raw): Path<String>) -> Response {
    let Some(id) = PaperId::parse(&raw) else { return StatusCode::BAD_REQUEST.into_response() };
    match db::requeue(&app.pool, &id.id, false).await {
        Ok(true) => {
            let _ = app.refresh(&id.id).await;
            app.emit(&id.id, "queued", "Queued again by an admin").await;
            app.wake.notify_one();
            "queued\n".into_response()
        }
        Ok(false) => (StatusCode::CONFLICT, "only a failed course can be retried\n").into_response(),
        Err(e) => internal(e),
    }
}

async fn admin_rebuild(State(app): AppState, Path(raw): Path<String>) -> Response {
    let Some(id) = PaperId::parse(&raw) else { return StatusCode::BAD_REQUEST.into_response() };
    let r = async {
        db::clear_artifacts(&app.pool, &id.id).await?;
        db::requeue(&app.pool, &id.id, true).await
    }
    .await;
    match r {
        Ok(true) => {
            let _ = app.refresh(&id.id).await;
            app.emit(&id.id, "queued", "Rebuilding from scratch (admin)").await;
            app.wake.notify_one();
            "rebuilding\n".into_response()
        }
        Ok(false) => (StatusCode::NOT_FOUND, "no such course\n").into_response(),
        Err(e) => internal(e),
    }
}

async fn admin_remove(State(app): AppState, Path(raw): Path<String>) -> Response {
    let Some(id) = PaperId::parse(&raw) else { return StatusCode::BAD_REQUEST.into_response() };
    let Some(c) = app.course(&id.id) else { return (StatusCode::NOT_FOUND, "no such course\n").into_response() };
    if c.aero_page_id.is_some_and(|p| p != "preview") {
        let page = github::page_path(&arxiv::alias(&id.id));
        if let Err(e) = github::commit(&app.cfg.github, &[(page, None)], &format!("Remove arXiv:{}", id.id)).await {
            return internal(e);
        }
    }
    if let Err(e) = db::mark_removed(&app.pool, &id.id).await {
        return internal(e);
    }
    let _ = app.refresh(&id.id).await;
    app.emit(&id.id, "removed", "Removed by an admin").await;
    "removed\n".into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sitemap_lists_published_courses_at_their_lasting_address() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-11-09T09:49:12Z").unwrap().with_timezone(&chrono::Utc);
        let row = |id: &str, status: &str, page: Option<&str>| db::CourseRow {
            arxiv_id: id.into(),
            status: status.into(),
            stage: None,
            aero_page_id: page.map(Into::into),
            aero_url: None,
            error: None,
            title: String::new(),
            requested_at: at,
            finished_at: Some(at),
        };
        let xml = sitemap_xml(
            "https://aero.example",
            &[
                row("2610.39302", "ready", Some("arxiv-2610-39302.html")),
                row("hep-th/9711200", "ready", Some("arxiv-hep-th-9711200.html")),
                row("1706.03762", "failed", None),
                row("2306.05243", "ready", Some("preview")),
            ],
        );
        assert!(xml.contains("<url><loc>https://aero.example/paper-bro/</loc></url>"), "{xml}");
        assert!(xml.contains("<url><loc>https://aero.example/paper-bro/abs/2610.39302</loc><lastmod>2026-11-09</lastmod></url>"), "{xml}");
        assert!(xml.contains("<loc>https://aero.example/paper-bro/abs/hep-th/9711200</loc>"), "{xml}");
        assert!(!xml.contains("1706.03762") && !xml.contains("2306.05243"), "only published courses: {xml}");
    }

    #[test]
    fn a_failed_course_can_be_retried_an_hour_after_it_stopped() {
        let stopped = chrono::Utc::now() - chrono::Duration::minutes(55);
        let mut c = db::CourseRow {
            arxiv_id: "2610.39302".into(),
            status: "failed".into(),
            stage: Some("publish".into()),
            aero_page_id: None,
            aero_url: None,
            error: Some(GENERIC_FAILURE.into()),
            title: String::new(),
            requested_at: stopped,
            finished_at: Some(stopped),
        };
        assert_eq!(retry_opens(&c), Some(stopped + chrono::Duration::hours(1)));
        c.error = Some("arXiv has no readable version of this paper".into());
        assert_eq!(retry_opens(&c), None, "a paper that can't be built isn't offered again");
        c.error = Some(GENERIC_FAILURE.into());
        c.status = "ready".into();
        assert_eq!(retry_opens(&c), None);
    }

    #[test]
    fn the_built_in_measurement_id_is_well_formed_and_used_twice() {
        assert!(GA_ID.strip_prefix("G-").is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric())), "{GA_ID}");
        // Once in the loader address, once in the config call.
        assert_eq!(with_analytics("<head></head>", GA_ID).matches(GA_ID).count(), 2);
    }

    #[test]
    fn the_analytics_tag_goes_right_after_head() {
        let out = with_analytics(r#"<!doctype html><html lang="en"><head><meta charset="utf-8"></head></html>"#, "G-TEST1");
        assert!(out.starts_with(r#"<!doctype html><html lang="en"><head><script async src="https://www.googletagmanager.com/gtag/js?id=G-TEST1"></script>"#), "{out}");
        assert!(out.contains(r#"gtag("config","G-TEST1");</script><meta charset="utf-8">"#), "{out}");
    }

    #[test]
    fn the_page_loads_its_files_from_the_cdn_and_falls_back_to_its_own() {
        assert!(CDN.starts_with("https://") && CDN.ends_with(&format!("{BASE}/")), "{CDN}");
        let head = r#"<link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/@fontsource/inter@5/index.min.css" />
    <script type="module" crossorigin src="/paper-bro/assets/index-AbC_1.js"></script>
    <link rel="stylesheet" crossorigin href="/paper-bro/assets/index-dEf-2.css">"#;
        let out = with_cdn(head, "https://cdn.test/paper-bro/");
        assert!(out.starts_with(r#"<link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/@fontsource/inter@5/index.min.css" />"#), "{out}");
        assert!(
            out.contains(r#"<script type="module" crossorigin src="https://cdn.test/paper-bro/assets/index-AbC_1.js" onerror="var s=document.createElement('script');s.type='module';s.src='/paper-bro/assets/index-AbC_1.js';document.head.append(s)"></script>"#),
            "{out}"
        );
        assert!(
            out.contains(r#"<link rel="stylesheet" crossorigin href="https://cdn.test/paper-bro/assets/index-dEf-2.css" onerror="this.onerror=null;this.href='/paper-bro/assets/index-dEf-2.css'">"#),
            "{out}"
        );
    }
}
