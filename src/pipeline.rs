//! The build pipeline: one paper in, one published course out.
//!
//! ingest → outline → lessons (in parallel) → browser QA → review → repair
//! (at most two rounds) → publish. Every stage writes its result to
//! `course_artifacts` before the next starts, so a job that dies (deploy,
//! crash, lost lease) resumes from where it was without paying again.
//!
//! Prompt layout, for Baseten's automatic prefix cache: every call of a job
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
use crate::ingest::{self, Paper, Unsupported};
use crate::llm::{LlmError, Request, Usage};
use crate::qa;

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
const MAX_HTML: usize = 950_000;

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
        let budget = e.downcast_ref::<LlmError>().is_some_and(|l| matches!(l, LlmError::Budget(_)));
        let unsupported = e.downcast_ref::<Unsupported>().is_some();
        let attempts = db::attempts(&app.pool, id).await.unwrap_or(MAX_ATTEMPTS);
        let retry = budget || (!unsupported && attempts < MAX_ATTEMPTS);
        let shown = if unsupported { e.to_string() } else { "paper-bro hit a problem building this course.".to_string() };
        log::error!("[{id}] attempt {attempts} failed: {e:#}");
        let _ = db::fail(&app.pool, id, &shown, retry, &usage).await;
        let note = if retry { format!("{shown} Trying again shortly.") } else { shown };
        app.emit(id, if retry { "retrying" } else { "failed" }, &note).await;
        if budget {
            // Out of Baseten budget: stop taking jobs for a while.
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

fn paper_message(meta: &Meta, paper: &Paper) -> String {
    let figures = if paper.figures.is_empty() {
        "This paper has no figures that can be shown.".to_string()
    } else {
        let list: Vec<String> = paper.figures.iter().map(|f| format!("- {}: {}", f.key, f.caption)).collect();
        format!("Figures that can be shown in a lesson (key: caption):\n{}", list.join("\n"))
    };
    format!(
        "<<<PAPER arXiv:{id} version {v}\nTitle: {title}\nAuthors: {authors}\nSource: {kind}\n\n{figures}\n\n{text}\nEND PAPER>>>",
        id = meta.id,
        v = meta.version,
        title = meta.title,
        authors = meta.authors.join(", "),
        kind = if paper.kind == "html" { "arXiv's HTML rendering, converted to text with $math$" } else { "the LaTeX source" },
        text = paper.text,
    )
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

    fn messages(&self, paper_msg: &str, task: Value) -> Vec<Value> {
        vec![
            json!({"role": "system", "content": SYSTEM}),
            json!({"role": "user", "content": paper_msg}),
            json!({"role": "user", "content": task}),
        ]
    }

    async fn run(&self) -> Result<()> {
        let app = &self.app;
        let meta = db::get_paper(&app.pool, &self.id).await?.context("the paper row is missing")?;
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
                        Some(bytes) => ingest::from_eprint(&bytes)?,
                        None => return Err(Unsupported("arXiv has no readable version of this paper".into()).into()),
                    },
                };
                db::set_source_kind(&app.pool, &self.id, &paper.kind).await?;
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
                let o = self.outline(&paper_msg, &paper).await?;
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
        let all: Vec<usize> = (0..n).collect();
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
                self.app.emit(&self.id, "repair", &format!("Lesson {} ships without its lab", i + 1)).await;
            }
            let what = if lab_broken { "lab dropped" } else { "open after two rounds" };
            self.note(&format!("lesson {} ({what}): {}", i + 1, p.join("; "))).await;
        }

        // 7. Publish.
        self.stage("publish", "Publishing the course").await;
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
        if html.len() > MAX_HTML {
            bail!("the course came out at {} KB, over AERO's limit", html.len() / 1024);
        }
        db::put_artifact(&app.pool, &self.id, "html", &html).await?;
        let alias = arxiv::alias(&self.id);
        let (page_id, url) = if cfg.publish {
            let existing = app.course(&self.id).and_then(|c| c.aero_page_id).filter(|p| p != "preview");
            let p = app.aero.publish(existing.as_deref(), &alias, &html).await?;
            (p.id, format!("{}{}", cfg.public_origin, p.url))
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
        Ok(())
    }

    /// An internal note: kept with the course's events for the admin, never
    /// in the public progress feed (web::PRIVATE_STAGES).
    async fn note(&self, message: &str) {
        self.app.emit(&self.id, "notes", message).await;
    }

    async fn outline(&self, paper_msg: &str, paper: &Paper) -> Result<Outline> {
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
        paper_msg: &str,
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

    async fn review(&self, paper_msg: &str, outline: &Outline, lessons: &[Lesson], report: &qa::Report) -> Result<Review> {
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

    async fn fix_quiz(&self, paper_msg: &str, outline: &Outline, findings: &[String]) -> Result<Vec<QuizItem>> {
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
