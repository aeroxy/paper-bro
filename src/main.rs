//! paper-bro: paste an arXiv paper, get an interactive course on AERO.
//! See README.md for what runs where.

mod aero;
mod app;
mod arxiv;
mod config;
mod course;
mod db;
mod ingest;
mod llm;
mod pipeline;
mod qa;
mod sample;
mod web;

use anyhow::{Context, Result, bail};
use std::future::IntoFuture;
use std::net::SocketAddr;
use std::sync::Arc;

const USAGE: &str = "usage: paper-bro <command>

  serve            run the site, the admin listener and the build workers
  build ID         build one paper's course in the foreground (resumes from checkpoints)
  republish ID     rebuild a finished course's page from its stored lessons with the
                   current shell, check it in the browser, and update it (no model calls)
  ingest ID        fetch a paper and write the text the model reads to tmp/
  qa-sample [nohang]  run browser QA on a hand-written course with broken labs
  migrate          apply database migrations";

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,chromiumoxide=off")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") => serve().await,
        Some("build") => {
            let id = args.get(1).and_then(|s| arxiv::PaperId::parse(s)).context("usage: build <arxiv id>")?;
            build(&id.id).await
        }
        Some("republish") => {
            let id = args.get(1).and_then(|s| arxiv::PaperId::parse(s)).context("usage: republish <arxiv id>")?;
            republish(&id.id).await
        }
        Some("ingest") => {
            let id = args.get(1).and_then(|s| arxiv::PaperId::parse(s)).context("usage: ingest <arxiv id>")?;
            ingest_one(&id.id).await
        }
        Some("qa-sample") => qa_sample(args.get(1).map(String::as_str) == Some("nohang")).await,
        Some("migrate") => {
            let url = std::env::var("NEON_DB").context("NEON_DB is not set")?;
            db::connect(&url).await?;
            println!("migrations applied");
            Ok(())
        }
        _ => bail!("{USAGE}"),
    }
}

async fn serve() -> Result<()> {
    let cfg = config::Config::from_env()?;
    let (listen, admin_listen, publish) = (cfg.listen, cfg.admin_listen, cfg.publish);
    // A dry run builds nothing unless asked to: its workers would otherwise
    // take real visitors' requests off a shared queue and finish them as
    // local previews.
    let workers = if !publish && std::env::var("WORKERS").is_err() { 0 } else { cfg.workers };
    if !admin_listen.ip().is_loopback() {
        bail!("ADMIN_LISTEN must be a loopback address; the admin routes have no auth");
    }
    // Bind before touching the queue: if another copy already holds the
    // ports, this one exits without claiming anything.
    let public = tokio::net::TcpListener::bind(listen).await.with_context(|| format!("binding {listen}"))?;
    let admin = tokio::net::TcpListener::bind(admin_listen).await.with_context(|| format!("binding {admin_listen}"))?;
    log::info!(
        "paper-bro starting: listen={listen} admin={admin_listen} workers={workers} publish={publish} endpoint={} model={}",
        cfg.endpoint.name, cfg.endpoint.model
    );
    let app = Arc::new(app::App::new(cfg).await?);
    let stale = qa::remove_stale_profiles();
    if stale > 0 {
        log::info!("removed {stale} browser profile(s) a previous run left behind");
    }
    if publish {
        let n = db::requeue_previews(&app.pool).await?;
        if n > 0 {
            log::info!("queued {n} dry-run course(s) to be published");
            for c in db::all_courses(&app.pool).await? {
                app.courses.write().unwrap().insert(c.arxiv_id.clone(), c);
            }
        }
    }
    for n in 0..workers {
        tokio::spawn(pipeline::worker(app.clone(), n));
    }
    // Queued work waits for an enqueue to wake a worker; look now instead.
    // A job a previous process left running is picked up once its lease
    // (two minutes) runs out.
    for _ in 0..workers {
        app.wake.notify_one();
    }
    let admin_app = web::admin(app.clone());
    tokio::spawn(async move {
        if let Err(e) = axum::serve(admin, admin_app).await {
            log::error!("admin listener stopped: {e}");
        }
    });
    // Graceful shutdown waits for open connections, and a live progress feed
    // stays open until its build ends. Give them a few seconds, then exit:
    // the new process needs the port, and leaving drops (and kills) any QA
    // browser along with the half-done job, which resumes from its checkpoint.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown().await;
        let _ = stop_tx.send(true);
    });
    let mut graceful = stop_rx.clone();
    let server = axum::serve(public, web::public(app).into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async move {
            let _ = graceful.wait_for(|stop| *stop).await;
        });
    let mut deadline = stop_rx;
    tokio::select! {
        r = server.into_future() => r?,
        _ = async move {
            let _ = deadline.wait_for(|stop| *stop).await;
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        } => log::info!("connections still open after 5 s; exiting anyway"),
    }
    Ok(())
}

async fn shutdown() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = ctrl_c => {},
        _ = term.recv() => {},
    }
    log::info!("shutting down");
}

/// Build one course in the foreground. Reruns resume from checkpoints.
async fn build(id: &str) -> Result<()> {
    let cfg = config::Config::from_env()?;
    let app = Arc::new(app::App::new(cfg).await?);
    let meta = app.arxiv.meta(id).await?.context("arXiv has no such paper")?;
    db::upsert_paper(&app.pool, &meta).await?;
    if !db::enqueue(&app.pool, id).await? {
        db::requeue(&app.pool, id, true).await?;
    }
    let started = std::time::Instant::now();
    if !db::claim_id(&app.pool, id, pipeline::LEASE).await? {
        bail!("{id} isn't queued any more: a running server's worker took it");
    }
    pipeline::process(&app, id).await;
    let c = db::course(&app.pool, id).await?.context("course row vanished")?;
    println!(
        "{} after {:.0}s: status={} url={} error={}",
        id,
        started.elapsed().as_secs_f64(),
        c.status,
        c.aero_url.unwrap_or_default(),
        c.error.unwrap_or_default()
    );
    Ok(())
}

/// Re-assemble a finished course from its stored outline and lessons with
/// the current shell (labkit, runtime, styles), run browser QA on it, and
/// publish it in place. The lessons are the ones that were reviewed; nothing
/// is regenerated, so this costs nothing and changes only the shell.
async fn republish(id: &str) -> Result<()> {
    let cfg = config::Config::from_env()?;
    let app = app::App::new(cfg).await?;
    let pool = &app.pool;
    let meta = db::get_paper(pool, id).await?.context("no such paper in the database")?;
    let row = db::course(pool, id).await?.context("no course for that paper")?;
    if row.status != "ready" {
        bail!("{id} is {}, not ready", row.status);
    }
    let outline: course::Outline =
        serde_json::from_str(&db::artifact(pool, id, "outline").await?.context("the outline wasn't kept")?)?;
    let mut lessons = Vec::new();
    for i in 1..=outline.lessons.len() {
        let body = db::artifact(pool, id, &format!("lesson:{i:02}")).await?.with_context(|| format!("lesson {i} wasn't kept"))?;
        lessons.push(serde_json::from_str::<course::Lesson>(&body)?);
    }
    // Figures come from the paper's HTML rendering, which arXiv serves again.
    let figures = match app.arxiv.html(id, meta.version).await? {
        Some((url, html)) => ingest::from_latexml(&url, &html)?.figures,
        None => Vec::new(),
    };
    // A lesson finds its figure by key. If arXiv's rendering has changed or
    // gone, stop rather than publish the course without those figures.
    let missing = course::missing_figures(&lessons, &figures);
    if !missing.is_empty() {
        bail!("arXiv no longer serves figures the lessons use ({}); nothing was published", missing.join(", "));
    }
    let (model, finished): (Option<String>, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT model, finished_at FROM courses WHERE arxiv_id = $1").bind(id).fetch_one(pool).await?;
    let model = model.unwrap_or_default();
    let built = finished.unwrap_or_else(chrono::Utc::now).format("%-d %B %Y").to_string();
    let c = course::Course { meta: &meta, outline: &outline, lessons: &lessons, figures: &figures, model: &model, built: &built };

    let labs: Vec<usize> = (0..lessons.len()).filter(|&i| !lessons[i].lab_dropped).collect();
    let report = qa::run(qa::Input {
        course_html: course::assemble(&c, qa::ORIGIN),
        lab_pages: labs.iter().map(|&i| (i + 1, course::lab_page(&c, qa::ORIGIN, i))).collect(),
        figure_prefix: Some(format!("https://arxiv.org/html/{id}")),
        chrome: &app.cfg.chrome,
        screenshots: false,
    })
    .await?;
    let failing: Vec<_> = report.labs.iter().filter(|l| !l.ok).collect();
    for l in &failing {
        println!("lab {}: {}", l.lesson, l.problems.join("; "));
    }
    for p in &report.page_problems {
        println!("page: {p}");
    }
    if !failing.is_empty() || !report.page_problems.is_empty() {
        bail!("browser QA found problems; nothing was published");
    }
    let origin = if app.cfg.publish { app.cfg.public_origin.clone() } else { format!("http://{}", app.cfg.listen) };
    let html = course::assemble(&c, &origin);
    if app.cfg.publish {
        let page = row.aero_page_id.filter(|p| p != "preview").context("the course has no AERO page to update")?;
        app.aero.update_page(&page, &html).await?;
        db::put_artifact(pool, id, "html", &html).await?;
        println!("republished {id}: {} ({} KB), QA clean on {} labs", row.aero_url.as_deref().unwrap_or(&page), html.len() / 1024, labs.len());
    } else {
        std::fs::create_dir_all("tmp")?;
        let out = format!("tmp/{}.html", arxiv::alias(id));
        std::fs::write(&out, &html)?;
        println!("dry run: wrote {out} ({} KB), QA clean on {} labs; PUBLISH=1 updates the page", html.len() / 1024, labs.len());
    }
    Ok(())
}

async fn ingest_one(id: &str) -> Result<()> {
    let client = arxiv::Client::new()?;
    let meta = client.meta(id).await?.context("no such paper")?;
    println!("{} v{} — {}\nlicense: {:?}\ncategories: {:?}", meta.id, meta.version, meta.title, meta.license, meta.categories);
    let paper = match client.html(id, meta.version).await? {
        Some((url, html)) => ingest::from_latexml(&url, &html)?,
        None => match client.eprint(id, meta.version).await? {
            Some(bytes) => ingest::from_eprint(&bytes)?,
            None => bail!("no source"),
        },
    };
    println!(
        "source: {} | {} chars | ~{}K tokens | {} figures",
        paper.kind,
        paper.text.len(),
        paper.text.len() / 3500,
        paper.figures.len()
    );
    std::fs::create_dir_all("tmp")?;
    let out = format!("tmp/{}.md", arxiv::alias(id));
    std::fs::write(&out, &paper.text)?;
    println!("wrote {out}");
    Ok(())
}

async fn qa_sample(skip_hang: bool) -> Result<()> {
    let chrome = std::path::PathBuf::from(
        std::env::var("CHROME_PATH").unwrap_or_else(|_| ".chrome/chrome-headless-shell-mac-arm64/chrome-headless-shell".into()),
    );
    let (meta, mut outline, mut lessons) = sample::course();
    if skip_hang {
        outline.lessons.pop();
        lessons.pop();
    }
    let c = course::Course {
        meta: &meta,
        outline: &outline,
        lessons: &lessons,
        figures: &[],
        model: "none",
        built: "today",
    };
    std::fs::create_dir_all("tmp")?;
    std::fs::write("tmp/sample-course.html", course::assemble(&c, "http://localhost"))?;
    let lab_pages = (0..lessons.len()).map(|i| (i + 1, course::lab_page(&c, qa::ORIGIN, i))).collect();
    let started = std::time::Instant::now();
    let report = qa::run(qa::Input {
        course_html: course::assemble(&c, qa::ORIGIN),
        lab_pages,
        figure_prefix: None,
        chrome: &chrome,
        screenshots: true,
    })
    .await?;
    println!("QA took {:.1}s", started.elapsed().as_secs_f64());
    println!("page: {:?}", report.page_problems);
    for l in &report.labs {
        println!("lab {} ok={} problems={:?}", l.lesson, l.ok, l.problems);
    }
    Ok(())
}
