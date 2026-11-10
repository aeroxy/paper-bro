//! The build pipeline: one paper in, one published course out.
//!
//! ingest → outline → lessons (in parallel) → browser QA → review → repair
//! (at most two rounds) → publish, then a post on the Telegram channel if one
//! is set up (telegram.rs). Every stage writes its result to
//! `course_artifacts` before the next starts, so a job that dies (deploy,
//! crash, lost lease) resumes from where it was without paying again.
//!
//! Prompt layout, for a server's automatic prefix cache: every call of a job
//! starts with the same two messages, the shared rules (prompts/system.md)
//! and the paper, and only the last message differs. The paper is therefore
//! billed at the cached rate after the first call. `x-session-affinity`
//! keeps a job's calls on one replica so that cache is likely to be there.

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use crate::app::App;
use crate::arxiv::{self, Meta};
use crate::course::{self, Course, Lesson, Outline, QuizItem, Review};
use crate::db;
use crate::github;
use crate::ingest::{self, Paper, Unsupported};
use crate::llm::{LlmError, Request, Usage};
use crate::qa;
use crate::telegram;

const SYSTEM: &str = include_str!("../prompts/system.md");
const OUTLINE: &str = include_str!("../prompts/outline.md");
const LESSON: &str = include_str!("../prompts/lesson.md");
const REVIEW: &str = include_str!("../prompts/review.md");

/// How long a claimed job is ours without a heartbeat. Short, so a job cut
/// off by a restart is picked up again within a couple of minutes; the
/// heartbeat renews it well before then while the job is alive.
pub const LEASE: Duration = Duration::from_secs(120);
const HEARTBEAT: Duration = Duration::from_secs(30);
const MAX_ATTEMPTS: i32 = 3;
/// How a build waits for GitHub Pages to serve the page it just committed:
/// it asks every `PAGE_POLL`, `PAGE_TRIES` times (15 minutes in all), and only
/// then calls the course ready anyway. Pages usually takes seconds, but one
/// deploy took 5 minutes 40 seconds (2026-11-10), after a 5-minute wait had
/// given up and the course was announced with a link that still gave a 404.
/// The wait holds a worker, so this is as long as it is worth.
const PAGE_POLL: Duration = Duration::from_secs(5);
const PAGE_TRIES: u32 = 180;

pub async fn worker(app: Arc<App>, n: usize) {
    log::info!("worker {n} up");
    loop {
        match db::claim(&app.pool, LEASE).await {
            Ok(Some(id)) => process(&app, &id).await,
            // No polling: enqueue wakes us, so Neon can sleep while idle.
            Ok(None) => app.wake.notified().await,
            Err(e) => {
                log::error!("worker {n}: claim failed: {e:#}");
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        }
    }
}

pub async fn process(app: &Arc<App>, id: &str) {
    let _ = app.refresh(id).await;
    let hb = {
        let (app, id) = (app.clone(), id.to_string());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(HEARTBEAT).await;
                let _ = db::heartbeat(&app.pool, &id, LEASE).await;
            }
        })
    };
    let job = Job { app: app.clone(), id: id.to_string(), usage: Mutex::new(Usage::default()) };
    let result = job.run().await;
    hb.abort();
    if let Err(e) = result {
        let usage = job.usage_json();
        let budget = out_of_credit(&e);
        let unsupported = e.downcast_ref::<Unsupported>().is_some();
        let attempts = db::attempts(&app.pool, id).await.unwrap_or(MAX_ATTEMPTS);
        let retry = budget || (!unsupported && attempts < MAX_ATTEMPTS);
        let shown = visitor_message(&e);
        log::error!("[{id}] attempt {attempts} failed: {e:#}");
        let _ = db::fail(&app.pool, id, &shown, retry, &usage).await;
        // The out-of-credit text already says it retries, and "shortly" would
        // be wrong: the worker sits out the next 15 minutes.
        let note = if retry && !budget { format!("{shown} Trying again shortly.") } else { shown };
        app.emit(id, failure_stage(retry, budget), &note).await;
        if budget {
            // Out of model budget: stop taking jobs for a while.
            tokio::time::sleep(Duration::from_secs(15 * 60)).await;
        } else if retry {
            tokio::time::sleep(Duration::from_secs(20)).await;
        }
        if retry {
            app.wake.notify_one();
        }
    }
    let _ = app.refresh(id).await;
}

/// What visitors see when the model provider says paper-bro's account is out
/// of credit (a 402). The address is the donation page; the course page
/// turns it into a link and its footer carries the same one
/// (frontend/src/Shell.tsx).
const OUT_OF_CREDIT: &str = "paper-bro has run out of model credit, so this course has to wait. It will keep retrying by itself. If you'd like to help top the credit up, you can donate at https://aero-sg.us.to";

fn out_of_credit(e: &anyhow::Error) -> bool {
    matches!(e.downcast_ref::<LlmError>(), Some(LlmError::Budget(_)))
}

/// The progress stage a failed attempt is reported under. The course page
/// reads "credit" as "on hold until there is model credit again" and changes
/// its headline to say so (frontend/src/PaperPage.tsx), so keep the two in step.
fn failure_stage(retry: bool, budget: bool) -> &'static str {
    match (budget, retry) {
        (true, _) => "credit",
        (false, true) => "retrying",
        (false, false) => "failed",
    }
}

/// What a visitor is told about a failed attempt. The cause stays in the log:
/// a provider's error can carry account details.
fn visitor_message(e: &anyhow::Error) -> String {
    if out_of_credit(e) {
        OUT_OF_CREDIT.to_string()
    } else if e.downcast_ref::<Unsupported>().is_some() {
        e.to_string()
    } else {
        "paper-bro hit a problem building this course.".to_string()
    }
}

struct Job {
    app: Arc<App>,
    id: String,
    usage: Mutex<Usage>,
}

static BANNED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\bfetch\s*\(|XMLHttpRequest|WebSocket|EventSource|sendBeacon|localStorage|sessionStorage|indexedDB|document\.cookie|\beval\s*\(|new\s+Function\b|importScripts|window\.open\s*\(|\btop\.|\bparent\.|\bopener\b|postMessage|\blocation\s*=|location\.(?:href|assign|replace)|\bimport\s*\(",
    )
    .unwrap()
});

/// Banned APIs in a lab. The iframe sandbox and CSP already stop them; this
/// catches a lab that tries, which is either a bug or an injection attempt.
fn banned(lesson: &Lesson) -> Vec<String> {
    let mut hits: Vec<String> = BANNED.find_iter(&lesson.lab_js).map(|m| m.as_str().trim().to_string()).collect();
    hits.dedup();
    hits.into_iter().map(|h| format!("the lab uses `{h}`, which labs may not use")).collect()
}

/// The message every call of a build starts with: the paper between the
/// markers the system prompt names. For an HTML or LaTeX paper it is text, and
/// its bytes are the cached prompt prefix, so they must not change. For a PDF
/// it is the same markers around the drawn pages (`ingest::render_pdf`), each
/// an image after a label, in order: not a PDF attachment, which the model
/// server reads only 20 pages of.
fn paper_message(meta: &Meta, paper: &Paper) -> Value {
    let figures = if paper.figures.is_empty() {
        "This paper has no figures that can be shown.".to_string()
    } else {
        let list: Vec<String> = paper.figures.iter().map(|f| format!("- {}: {}", f.key, f.caption)).collect();
        format!("Figures that can be shown in a lesson (key: caption):\n{}", list.join("\n"))
    };
    let kind = match (paper.kind.as_str(), paper.pages.len()) {
        ("html", _) => "arXiv's HTML rendering, converted to text with $math$".to_string(),
        (_, 0) => "the LaTeX source".to_string(),
        (_, n) => format!("the paper's PDF, all {n} pages, attached below as images in order, each after a label \"[page k]\""),
    };
    let head = format!(
        "<<<PAPER arXiv:{id} version {v}\nTitle: {title}\nAuthors: {authors}\nSource: {kind}\n\n{figures}\n\n",
        id = meta.id,
        v = meta.version,
        title = meta.title,
        authors = meta.authors.join(", "),
    );
    if paper.pages.is_empty() {
        return json!(format!("{head}{}\nEND PAPER>>>", paper.text));
    }
    let mut parts = vec![json!({"type": "text", "text": head})];
    for (i, page) in paper.pages.iter().enumerate() {
        parts.push(json!({"type": "text", "text": format!("[page {}]", i + 1)}));
        parts.push(json!({"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{page}")}}));
    }
    parts.push(json!({"type": "text", "text": "END PAPER>>>"}));
    Value::Array(parts)
}

/// The lessons whose labs are tested and repaired. A lab dropped by an earlier
/// attempt stays dropped: its lab page has no lab, so testing it would fail and
/// send the lesson back to the model, which rewrites it and undoes the drop.
fn labs_to_test(lessons: &[Lesson]) -> Vec<usize> {
    (0..lessons.len()).filter(|&i| !lessons[i].lab_dropped).collect()
}

impl Job {
    fn add(&self, u: Usage) {
        self.usage.lock().unwrap().add(&u);
    }

    fn usage_json(&self) -> Value {
        serde_json::to_value(*self.usage.lock().unwrap()).unwrap_or_default()
    }

    async fn stage(&self, stage: &str, message: &str) {
        let _ = db::set_stage(&self.app.pool, &self.id, stage, &self.usage_json()).await;
        self.app.emit(&self.id, stage, message).await;
    }

    async fn artifact<T: serde::de::DeserializeOwned>(&self, key: &str) -> Option<T> {
        let body = db::artifact(&self.app.pool, &self.id, key).await.ok()??;
        serde_json::from_str(&body).ok()
    }

    async fn save<T: serde::Serialize>(&self, key: &str, value: &T) -> Result<()> {
        db::put_artifact(&self.app.pool, &self.id, key, &serde_json::to_string(value)?).await
    }

    fn messages(&self, paper_msg: &Value, task: Value) -> Vec<Value> {
        vec![
            json!({"role": "system", "content": SYSTEM}),
            json!({"role": "user", "content": paper_msg}),
            json!({"role": "user", "content": task}),
        ]
    }

    /// The whole build, from reading the paper to publishing the course. A
    /// rerun reads the paper, the outline and the lessons back from an
    /// earlier attempt's checkpoints.
    async fn run(&self) -> Result<()> {
        let app = &self.app;
        let mut meta = db::get_paper(&app.pool, &self.id).await?.context("the paper row is missing")?;
        if let Some(prev) = db::course(&app.pool, &self.id).await? {
            if let Ok(Some(u)) = sqlx::query_scalar::<_, Value>("SELECT usage FROM courses WHERE arxiv_id = $1")
                .bind(&prev.arxiv_id)
                .fetch_optional(&app.pool)
                .await
            {
                if let Ok(u) = serde_json::from_value::<Usage>(u) {
                    *self.usage.lock().unwrap() = u;
                }
            }
        }

        // 1. Read the paper.
        let paper: Paper = match self.artifact("paper").await {
            Some(p) => p,
            None => {
                self.stage("ingest", "Reading the paper from arXiv").await;
                let paper = match app.arxiv.html(&meta.id, meta.version).await? {
                    Some((url, html)) => ingest::from_latexml(&url, &html)?,
                    None => match app.arxiv.eprint(&meta.id, meta.version).await? {
                        // CPU work on an untrusted file (a PDF is drawn here): off
                        // the async threads, and a crash in it is a paper we can't read.
                        Some(bytes) => tokio::task::spawn_blocking(move || ingest::from_eprint(&bytes))
                            .await
                            .map_err(|_| Unsupported("paper-bro couldn't read this paper's source".into()))??,
                        None => return Err(Unsupported("arXiv has no readable version of this paper".into()).into()),
                    },
                };
                db::set_source_kind(&app.pool, &self.id, &paper.kind).await?;
                if !paper.pages.is_empty() {
                    self.note(&format!("the paper is a PDF only: the model reads its {} pages as images", paper.pages.len())).await;
                }
                self.save("paper", &paper).await?;
                paper
            }
        };
        let paper_msg = paper_message(&meta, &paper);

        // 2. Plan.
        let mut outline: Outline = match self.artifact("outline").await {
            Some(o) => o,
            None => {
                self.stage("outline", "Planning the course").await;
                let mut o = self.outline(&paper_msg, &paper).await?;
                // A plan whose headline ran off the rails is written once more,
                // and never saved (course::plan_problems).
                let problems = course::plan_problems(&o);
                if !problems.is_empty() {
                    self.note(&format!("the plan was written again: {}", problems.join("; "))).await;
                    o = self.outline(&paper_msg, &paper).await?;
                    let problems = course::plan_problems(&o);
                    if !problems.is_empty() {
                        bail!("the plan came out broken twice: {}", problems.join("; "));
                    }
                }
                self.save("outline", &o).await?;
                o
            }
        };
        let n = outline.lessons.len();
        self.stage("lessons", &format!("Planned {n} lessons: writing them")).await;

        // 3. Write every lesson, a few at a time.
        let done = Mutex::new(0usize);
        let results: Vec<Result<Lesson>> = futures::stream::iter(0..n)
            .map(|i| {
                let (outline, paper_msg, done) = (&outline, &paper_msg, &done);
                async move {
                    let key = format!("lesson:{:02}", i + 1);
                    if let Some(l) = self.artifact::<Lesson>(&key).await {
                        return Ok(l);
                    }
                    let l = self.write_lesson(paper_msg, outline, i, None).await?;
                    self.save(&key, &l).await?;
                    let k = {
                        let mut d = done.lock().unwrap();
                        *d += 1;
                        *d
                    };
                    self.stage("lessons", &format!("Wrote lesson {} ({k} of {n} done)", i + 1)).await;
                    Ok(l)
                }
            })
            .buffered(6)
            .collect()
            .await;
        let mut lessons: Vec<Lesson> = results.into_iter().collect::<Result<_>>()?;

        // 4-6. Test, review, repair.
        self.stage("qa", "Testing every lab in a real browser").await;
        let all = labs_to_test(&lessons);
        let report = self.qa(&meta, &outline, &lessons, &paper, &all).await?;
        self.stage("review", "A separate reviewer is checking the course against the paper").await;
        let review = match self.review(&paper_msg, &outline, &lessons, &report).await {
            Ok(r) => Some(r),
            Err(e) => {
                log::warn!("[{}] review failed, continuing on browser QA alone: {e:#}", self.id);
                None
            }
        };
        let _ = self.save("review:1", &review).await;
        if let Some(r) = review.as_ref().filter(|r| !r.quiz_findings.is_empty()) {
            if let Ok(q) = self.fix_quiz(&paper_msg, &outline, &r.quiz_findings).await {
                outline.quiz = q;
                self.save("outline", &outline).await?;
            }
        }
        let mut problems = self.problems(&lessons, &report, review.as_ref(), &all);
        let mut last = report;
        for round in 1..=2 {
            if problems.is_empty() {
                break;
            }
            let which: Vec<usize> = problems.keys().copied().collect();
            let list = which.iter().map(|i| (i + 1).to_string()).collect::<Vec<_>>().join(", ");
            self.stage("repair", &format!("Fixing lesson {list} (round {round} of 2)")).await;
            let fixed: Vec<Result<(usize, Lesson)>> = futures::stream::iter(which.clone())
                .map(|i| {
                    let (outline, paper_msg, lessons, problems) = (&outline, &paper_msg, &lessons, &problems);
                    async move {
                        let l = self.write_lesson(paper_msg, outline, i, Some((&lessons[i], &problems[&i]))).await?;
                        self.save(&format!("lesson:{:02}", i + 1), &l).await?;
                        Ok((i, l))
                    }
                })
                .buffered(6)
                .collect()
                .await;
            for r in fixed {
                let (i, l) = r?;
                lessons[i] = l;
            }
            last = self.qa(&meta, &outline, &lessons, &paper, &which).await?;
            problems = self.problems(&lessons, &last, None, &which);
        }
        // Whatever still fails in the browser ships without its lab; the
        // lesson's prose stays. Anything else left over is only logged.
        for (i, p) in &problems {
            let lab_broken = !banned(&lessons[*i]).is_empty() || last.lab(i + 1).is_some_and(|r| !r.ok);
            if lab_broken {
                lessons[*i].lab_dropped = true;
                // Saved, so a later republish leaves the lab out too.
                self.save(&format!("lesson:{:02}", i + 1), &lessons[*i]).await?;
                self.app.emit(&self.id, "repair", &format!("Lesson {} ships without its lab", i + 1)).await;
            }
            let what = if lab_broken { "lab dropped" } else { "open after two rounds" };
            self.note(&format!("lesson {} ({what}): {}", i + 1, p.join("; "))).await;
        }

        // 7. Publish.
        self.stage("publish", "Publishing the course").await;
        // The license is only for the page's footer, so it is looked up here
        // rather than while a visitor waits, and a failed lookup keeps the
        // one we have (if any) instead of failing a finished course.
        match app.arxiv.license(&meta.id).await {
            Ok(Some(license)) => {
                db::set_license(&app.pool, &self.id, &license).await?;
                meta.license = Some(license);
            }
            Ok(None) => {}
            Err(e) => {
                let kept = meta.license.as_deref().unwrap_or("none");
                self.note(&format!("the license lookup failed (stored license: {kept}): {e:#}")).await;
            }
        }
        let cfg = &app.cfg;
        let origin = if cfg.publish { cfg.public_origin.clone() } else { format!("http://{}", cfg.listen) };
        let model = app.llm.model();
        let built = chrono::Utc::now().format("%-d %B %Y").to_string();
        let c = Course {
            meta: &meta,
            outline: &outline,
            lessons: &lessons,
            figures: &paper.figures,
            model: &model,
            built: &built,
        };
        let html = course::assemble(&c, &origin);
        course::check_size(&html)?;
        db::put_artifact(&app.pool, &self.id, "html", &html).await?;
        let alias = arxiv::alias(&self.id);
        let (page_id, url) = if cfg.publish {
            let path = github::page_path(&alias);
            github::commit(&cfg.github, &[(path.clone(), Some(html.as_str()))], &format!("Publish arXiv:{}", self.id)).await?;
            // Pages deploys the commit in a minute or so. Wait, so the course
            // isn't called ready, or announced, while its link is a 404.
            let url = github::page_url(&cfg.github, &alias);
            if !github::wait_until_live(&url, PAGE_POLL, PAGE_TRIES).await {
                let minutes = PAGE_POLL.as_secs() * u64::from(PAGE_TRIES) / 60;
                self.note(&format!("GitHub Pages wasn't serving the page {minutes} minutes after the commit")).await;
            }
            (path, url)
        } else {
            ("preview".to_string(), format!("{origin}/paper-bro/preview/{alias}"))
        };
        let usage = self.usage_json();
        db::finish_ready(&app.pool, &self.id, &page_id, &url, meta.version, &model, &usage).await?;
        db::prune_artifacts(&app.pool, &self.id).await?;
        app.refresh(&self.id).await?;
        let n = |k: &str| usage[k].as_u64().unwrap_or(0);
        self.note(&format!(
            "{} KB, ${:.2}: {} calls, {} prompt tokens ({} cached), {} completion tokens",
            html.len() / 1024,
            usage["cost_usd"].as_f64().unwrap_or(0.0),
            n("calls"),
            n("prompt"),
            n("cached"),
            n("completion"),
        ))
        .await;
        app.emit(&self.id, "ready", "The course is ready").await;
        // Tell the Telegram channel, once. The course is out either way, so a
        // failure here is only noted.
        if cfg.publish {
            let text = telegram::message(&meta, &outline, &lessons, &telegram::link(&cfg.public_origin, &self.id));
            match telegram::announce(app, &self.id, &text).await {
                Ok(Some(m)) => self.note(&format!("announced on Telegram (message {m})")).await,
                Ok(None) => {}
                Err(e) => {
                    log::warn!("{}: the Telegram announcement failed: {e:#}", self.id);
                    self.note(&format!("the Telegram announcement failed: {e:#}")).await;
                }
            }
        }
        Ok(())
    }

    /// An internal note: kept with the course's events for the admin, never
    /// in the public progress feed (web::PRIVATE_STAGES).
    async fn note(&self, message: &str) {
        self.app.emit(&self.id, "notes", message).await;
    }

    async fn outline(&self, paper_msg: &Value, paper: &Paper) -> Result<Outline> {
        let mut rejected: Option<String> = None;
        for _ in 0..2 {
            let task = match &rejected {
                None => OUTLINE.to_string(),
                Some(why) => format!("{OUTLINE}\nYour previous plan was rejected: {why}. Return the whole plan again, fixed."),
            };
            let req = Request {
                messages: self.messages(paper_msg, json!(task)),
                effort: "high",
                schema: Some(("course_outline", course::outline_schema())),
                affinity: &self.id,
            };
            let (mut o, u): (Outline, Usage) = self.app.llm.json(&req).await?;
            self.add(u);
            for l in &mut o.lessons {
                if !paper.figures.iter().any(|f| f.key == l.figure) {
                    l.figure.clear();
                }
            }
            match o.validate() {
                Ok(()) => return Ok(o),
                Err(e) => rejected = Some(e),
            }
        }
        bail!("the model couldn't produce a valid course plan: {}", rejected.unwrap_or_default())
    }

    async fn write_lesson(
        &self,
        paper_msg: &Value,
        outline: &Outline,
        i: usize,
        fix: Option<(&Lesson, &Vec<String>)>,
    ) -> Result<Lesson> {
        let plan = &outline.lessons[i];
        let mut task = LESSON
            .replace("{n}", &(i + 1).to_string())
            .replace("{total}", &outline.lessons.len().to_string())
            .replace("{title}", &plan.title)
            .replace("{figure}", &plan.figure)
            .replace("{plan}", &serde_json::to_string_pretty(outline)?);
        if let Some((prev, problems)) = fix {
            task.push_str(&format!(
                "\n# Fix this lesson\n\nYour earlier version is below, with problems found by a separate reviewer and by a real browser. Fix every one, keep what already works, and return the whole lesson again.\n\n<<<PREVIOUS\n{}\nPREVIOUS>>>\n\nProblems:\n{}\n",
                serde_json::to_string_pretty(prev)?,
                problems.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n")
            ));
        }
        let req = Request {
            messages: self.messages(paper_msg, json!(task)),
            effort: "medium",
            schema: Some(("course_lesson", course::lesson_schema())),
            affinity: &self.id,
        };
        let (mut l, u): (Lesson, Usage) = self.app.llm.json(&req).await?;
        self.add(u);
        if l.figure != plan.figure {
            l.figure.clear();
        }
        l.lab_dropped = false;
        Ok(l)
    }

    async fn qa(&self, meta: &Meta, outline: &Outline, lessons: &[Lesson], paper: &Paper, which: &[usize]) -> Result<qa::Report> {
        let c = Course {
            meta,
            outline,
            lessons,
            figures: &paper.figures,
            model: "",
            built: "",
        };
        let lab_pages = which.iter().map(|&i| (i + 1, course::lab_page(&c, qa::ORIGIN, i))).collect();
        qa::run(qa::Input {
            course_html: course::assemble(&c, qa::ORIGIN),
            lab_pages,
            figure_prefix: Some(format!("https://arxiv.org/html/{}", meta.id)),
            chrome: &self.app.cfg.chrome,
            screenshots: true,
        })
        .await
    }

    async fn review(&self, paper_msg: &Value, outline: &Outline, lessons: &[Lesson], report: &qa::Report) -> Result<Review> {
        let mut text = String::new();
        for (i, (plan, l)) in outline.lessons.iter().zip(lessons).enumerate() {
            let qa = match report.lab(i + 1) {
                Some(r) if r.ok => "No problems found.".to_string(),
                Some(r) => r.problems.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n"),
                None => "Not tested.".into(),
            };
            text.push_str(&format!(
                "## Lesson {n}: {title}\n\nPlanned key ideas: {ideas}\n\n### Prose\n{prose}\n\n### Lab: {lab}\nlab_html:\n{html}\n\nlab_js:\n{js}\n\n### Browser QA of the lab\n{qa}\n\n",
                n = i + 1,
                title = plan.title,
                ideas = plan.key_ideas.join("; "),
                prose = l.prose_html,
                lab = plan.lab.title,
                html = l.lab_html,
                js = l.lab_js,
            ));
        }
        if !report.page_problems.is_empty() {
            text.push_str(&format!("## Problems on the whole page\n{}\n\n", report.page_problems.join("\n")));
        }
        text.push_str(&format!("## Quiz\n{}\n", serde_json::to_string_pretty(&outline.quiz)?));
        let mut parts = vec![json!({"type": "text", "text": REVIEW.replace("{course}", &text)})];
        for r in &report.labs {
            if let Some(shot) = &r.screenshot_jpeg_b64 {
                parts.push(json!({"type": "text", "text": format!("Screenshot of the lab in lesson {}:", r.lesson)}));
                parts.push(json!({"type": "image_url", "image_url": {"url": format!("data:image/jpeg;base64,{shot}")}}));
            }
        }
        let req = Request {
            messages: self.messages(paper_msg, Value::Array(parts)),
            effort: "high",
            schema: Some(("course_review", course::review_schema())),
            affinity: &self.id,
        };
        let (r, u): (Review, Usage) = self.app.llm.json(&req).await?;
        self.add(u);
        Ok(r)
    }

    async fn fix_quiz(&self, paper_msg: &Value, outline: &Outline, findings: &[String]) -> Result<Vec<QuizItem>> {
        #[derive(serde::Deserialize)]
        struct Quiz {
            quiz: Vec<QuizItem>,
        }
        let schema = crate::llm::strict(json!({"type": "object", "properties": {
            "quiz": course::outline_schema()["properties"]["quiz"].clone()}}));
        let task = format!(
            "# Your task: fix the quiz\n\nA reviewer found problems in this course's quiz. Return the whole quiz, corrected, with the same number of questions.\n\n<<<QUIZ\n{}\nQUIZ>>>\n\nProblems:\n{}\n",
            serde_json::to_string_pretty(&outline.quiz)?,
            findings.iter().map(|f| format!("- {f}")).collect::<Vec<_>>().join("\n")
        );
        let req = Request { messages: self.messages(paper_msg, json!(task)), effort: "medium", schema: Some(("course_quiz", schema)), affinity: &self.id };
        let (q, u): (Quiz, Usage) = self.app.llm.json(&req).await?;
        self.add(u);
        let mut fixed = outline.clone();
        fixed.quiz = q.quiz;
        fixed.validate().map_err(anyhow::Error::msg)?;
        Ok(fixed.quiz)
    }

    /// What still needs fixing, by lesson index: browser QA failures, banned
    /// APIs, reviewer findings, and KaTeX errors traced back to a lesson.
    fn problems(&self, lessons: &[Lesson], report: &qa::Report, review: Option<&Review>, which: &[usize]) -> BTreeMap<usize, Vec<String>> {
        let mut out: BTreeMap<usize, Vec<String>> = BTreeMap::new();
        for &i in which {
            let mut p = banned(&lessons[i]);
            if !p.is_empty() {
                log::warn!("[{}] lesson {} lab uses banned APIs (bug or injection): {p:?}", self.id, i + 1);
            }
            if let Some(r) = report.lab(i + 1).filter(|r| !r.ok) {
                p.extend(r.problems.iter().cloned());
            }
            if let Some(rv) = review.and_then(|rv| rv.lessons.iter().find(|l| l.lesson as usize == i + 1)) {
                if rv.verdict == "fix" {
                    p.extend(rv.findings.iter().map(|f| format!("reviewer: {f}")));
                }
            }
            for k in &report.page_problems {
                if let Some(snippet) = k.strip_prefix("math that KaTeX couldn't render: ") {
                    let probe: String = snippet.chars().take(24).collect();
                    if !probe.is_empty() && lessons[i].prose_html.contains(&probe) {
                        p.push(format!("this math doesn't render in KaTeX: {snippet}"));
                    }
                }
            }
            if !p.is_empty() {
                out.insert(i, p);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lesson(lab_dropped: bool) -> Lesson {
        Lesson { prose_html: String::new(), figure: String::new(), figure_note: String::new(), lab_html: String::new(), lab_css: String::new(), lab_js: String::new(), lab_dropped }
    }

    #[test]
    fn a_lab_dropped_by_an_earlier_attempt_is_not_tested_again() {
        assert_eq!(labs_to_test(&[lesson(false), lesson(true), lesson(false)]), vec![0, 2]);
        assert!(labs_to_test(&[lesson(true)]).is_empty());
    }

    fn meta() -> Meta {
        Meta {
            id: "2610.39393".into(),
            version: 1,
            title: "A Title".into(),
            authors: vec!["A. Author".into(), "B. Author".into()],
            abstract_: String::new(),
            categories: vec![],
            license: None,
            published: None,
        }
    }

    #[test]
    fn a_text_papers_message_keeps_its_exact_text() {
        // These bytes are the cached prompt prefix of every call: changing them costs money.
        let paper = Paper { kind: "latex".into(), text: "\\section{Intro}".into(), figures: vec![], pages: vec![] };
        assert_eq!(
            paper_message(&meta(), &paper),
            json!("<<<PAPER arXiv:2610.39393 version 1\nTitle: A Title\nAuthors: A. Author, B. Author\nSource: the LaTeX source\n\nThis paper has no figures that can be shown.\n\n\\section{Intro}\nEND PAPER>>>")
        );
    }

    #[test]
    fn a_pdf_paper_goes_in_as_every_page_image_in_order_between_the_markers() {
        let paper = Paper { kind: "pdf".into(), text: String::new(), figures: vec![], pages: vec!["QUJD".into(), "REVG".into()] };
        let msg = paper_message(&meta(), &paper);
        let parts = msg.as_array().expect("a PDF goes in as message parts, not one string");
        // head, then a label and an image per page, then the end marker
        assert_eq!(parts.len(), 1 + 2 * 2 + 1);
        let head = parts[0]["text"].as_str().unwrap();
        assert!(head.starts_with("<<<PAPER arXiv:2610.39393 version 1\n"), "{head}");
        assert!(head.contains("all 2 pages"), "{head}");
        assert!(head.contains("This paper has no figures that can be shown."), "{head}");
        assert!(!head.contains("first 20"), "no page of the paper is withheld from the model: {head}");
        // The shape the model server was shown to read: ordinary images, as data URLs.
        assert_eq!(parts[1], json!({"type": "text", "text": "[page 1]"}));
        assert_eq!(parts[2], json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}}));
        assert_eq!(parts[3], json!({"type": "text", "text": "[page 2]"}));
        assert_eq!(parts[4], json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,REVG"}}));
        assert_eq!(parts[5], json!({"type": "text", "text": "END PAPER>>>"}));
    }

    #[test]
    fn running_out_of_credit_tells_visitors_so_and_where_to_donate() {
        // The error reaches process() wrapped in the stage's context.
        let out = anyhow::Error::from(LlmError::Budget("provider: out of credit".into())).context("outline");
        assert!(out_of_credit(&out));
        let shown = visitor_message(&out);
        assert!(shown.contains("run out of model credit"), "{shown}");
        assert!(shown.contains("https://aero-sg.us.to"), "{shown}");
        // The provider's own words stay in the log.
        assert!(!shown.contains("provider: out of credit"), "{shown}");
    }

    #[test]
    fn only_the_credit_hold_has_its_own_stage_and_the_others_keep_theirs() {
        // The page's headline keys on "credit" (frontend/src/PaperPage.tsx).
        assert_eq!(failure_stage(true, true), "credit");
        assert_eq!(failure_stage(true, false), "retrying");
        assert_eq!(failure_stage(false, false), "failed");
    }

    #[test]
    fn other_failures_keep_their_old_messages() {
        let rejected = anyhow::Error::from(LlmError::Rejected("HTTP 400: model deactivated".into()));
        assert!(!out_of_credit(&rejected));
        assert_eq!(visitor_message(&rejected), "paper-bro hit a problem building this course.");
        let unsupported = anyhow::Error::from(Unsupported("the paper's source has too many files".into()));
        assert_eq!(visitor_message(&unsupported), "the paper's source has too many files");
    }
}
