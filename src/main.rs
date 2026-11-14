//! paper-bro: paste an arXiv paper, get an interactive course on AERO.
//! See README.md for what runs where.

mod app;
mod arxiv;
mod config;
mod course;
mod db;
mod github;
mod ingest;
mod llm;
mod pdf;
mod pipeline;
mod qa;
mod sample;
mod telegram;
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
  announce [ID]    post a published course (default: every one not posted yet) to the
                   Telegram channel, once; without PUBLISH=1 it prints the messages.
                   With --again, post one whose earlier post may or may not have arrived
  ingest ID        fetch a paper and write the text the model reads to tmp/
  qa-sample [nohang]  run browser QA on a hand-written course with broken labs
  migrate          apply database migrations";

fn main() -> Result<()> {
    // The one command that runs before anything else is set up. It reads an
    // untrusted file, so it gets no settings, no logger and no runtime (pdf.rs).
    if std::env::args().nth(1).as_deref() == Some(pdf::COMMAND) {
        return pdf::child();
    }
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(run())
}

async fn run() -> Result<()> {
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
        Some("announce") => {
            let again = args[1..].iter().any(|a| a == "--again");
            let id = args[1..]
                .iter()
                .find(|a| *a != "--again")
                .map(|s| arxiv::PaperId::parse(s).context("usage: announce [arxiv id [--again]]"))
                .transpose()?;
            if again && id.is_none() {
                bail!("usage: announce <arxiv id> --again");
            }
            announce(id.as_ref().map(|i| i.id.as_str()), again).await
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
        "paper-bro starting: listen={listen} admin={admin_listen} workers={workers} publish={publish} endpoint={} api={:?} model={} pdf={:?}",
        cfg.endpoint.name, cfg.endpoint.api, cfg.endpoint.model, cfg.pdf
    );
    let app = Arc::new(app::App::new(cfg).await?);
    let stale = qa::remove_stale_profiles();
    if stale > 0 {
        log::info!("removed {stale} browser profile(s) a previous run left behind");
    }
    if publish {
        // AERO's token lapsed with nothing checking it, and every build failed
        // at publish for nine days. Say at startup whether publishing works.
        match github::check(&app.cfg.github).await {
            Ok(()) => log::info!("publishing to {} as the GitHub App", app.cfg.github.repo),
            Err(e) => log::error!("publishing will fail: {e:#}"),
        }
        let n = db::requeue_previews(&app.pool).await?;
        if n > 0 {
            log::info!("queued {n} dry-run course(s) to be published");
            for c in db::all_courses(&app.pool).await? {
                app.courses.write().unwrap().insert(c.arxiv_id.clone(), c);
            }
        }
        // Now and every hour, drop the PDFs no build is using (db::sweep_pdfs).
        // Not in a dry run: it shares the database.
        let app = app.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(std::time::Duration::from_secs(60 * 60));
            loop {
                every.tick().await;
                match db::sweep_pdfs(&app.pool).await {
                    Ok(0) => {}
                    Ok(n) => log::info!("dropped {n} stored PDF paper(s) no build is using"),
                    Err(e) => log::warn!("couldn't sweep stored PDF papers: {e:#}"),
                }
            }
        });
    }
    for n in 0..workers {
        tokio::spawn(pipeline::worker(app.clone(), n));
    }
    // Queued work waits for an enqueue to wake a worker; look now instead.
    for _ in 0..workers {
        app.wake.notify_one();
    }
    // A job a previous process left running can only be claimed once its lease
    // has run out, and a restart is quicker than that, so the look above finds
    // nothing and nothing would wake a worker again until the next request.
    // Look once more when every such lease has run out.
    {
        let app = app.clone();
        tokio::spawn(async move {
            tokio::time::sleep(pipeline::LEASE + std::time::Duration::from_secs(5)).await;
            for _ in 0..workers {
                app.wake.notify_one();
            }
        });
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
    // It makes no model calls, so it does not ask for model settings.
    let cfg = config::Config::from_env_for(false)?;
    let app = app::App::new(cfg).await?;
    let pool = &app.pool;
    let (meta, row, outline, lessons) = stored(pool, id).await?;
    let problems = course::plan_problems(&outline);
    if !problems.is_empty() {
        bail!("the stored plan is broken ({}); fix its outline first, nothing was published", problems.join("; "));
    }
    let broken: Vec<String> = lessons
        .iter()
        .enumerate()
        .flat_map(|(i, l)| course::lesson_problems(l).into_iter().map(move |p| format!("lesson {}: {p}", i + 1)))
        .collect();
    if !broken.is_empty() {
        bail!("a stored lesson ran on ({}); fix it first, nothing was published", broken.join("; "));
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

    // A page AERO would reject is refused before a browser is spent on it.
    let origin = if app.cfg.publish { app.cfg.public_origin.clone() } else { format!("http://{}", app.cfg.listen) };
    let html = course::assemble(&c, &origin);
    course::check_size(&html)?;
    // The page published last time records which image each lesson showed; if
    // arXiv's rendering has changed since, a figure key may now mean another one.
    let qa_html = course::assemble(&c, qa::ORIGIN);
    match db::artifact(pool, id, "html").await? {
        Some(published) => {
            if let Some(why) = course::figures_changed(&course::figure_urls(&published), &course::figure_urls(&qa_html)) {
                bail!("{why}: the figures no longer match the published course; nothing was published");
            }
        }
        None => println!("note: there is no stored copy of the published page, so its figures were not compared"),
    }
    let labs: Vec<usize> = (0..lessons.len()).filter(|&i| !lessons[i].lab_dropped).collect();
    let report = qa::run(qa::Input {
        course_html: qa_html,
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
    if app.cfg.publish {
        if row.aero_url.is_none() || row.aero_page_id.as_deref() == Some("preview") {
            bail!("{id} has no published page to update");
        }
        let alias = arxiv::alias(id);
        github::commit(&app.cfg.github, &[(github::page_path(&alias), Some(html.as_str()))], &format!("Republish arXiv:{id}")).await?;
        db::put_artifact(pool, id, "html", &html).await?;
        let url = github::page_url(&app.cfg.github, &alias);
        println!("republished {id}: {url} ({} KB), QA clean on {} labs", html.len() / 1024, labs.len());
    } else {
        std::fs::create_dir_all("tmp")?;
        let out = format!("tmp/{}.html", arxiv::alias(id));
        std::fs::write(&out, &html)?;
        println!("dry run: wrote {out} ({} KB), QA clean on {} labs; PUBLISH=1 updates the page", html.len() / 1024, labs.len());
    }
    Ok(())
}

/// A finished course as stored: its paper, its row, and the outline and
/// lessons that were reviewed.
async fn stored(pool: &sqlx::PgPool, id: &str) -> Result<(arxiv::Meta, db::CourseRow, course::Outline, Vec<course::Lesson>)> {
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
    Ok((meta, row, outline, lessons))
}

/// Post published courses to the Telegram channel, each once: the one named,
/// or every one not posted yet, oldest first. A new course is announced when
/// it is published; this is for the ones from before, or a post that failed.
/// A post whose reply was lost may have arrived, so it is left alone until
/// someone has looked at the channel and asks for it `again`. Without
/// `PUBLISH=1` it only prints the messages.
async fn announce(id: Option<&str>, again: bool) -> Result<()> {
    let app = app::App::new(config::Config::from_env_for(false)?).await?;
    if app.cfg.publish && app.cfg.telegram.is_none() {
        bail!("set TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID to announce");
    }
    let held_back = |id: &str| println!("{id}: an earlier post may have reached the channel; if it isn't there, `announce {id} --again` posts it");
    let (ids, held) = match id {
        Some(id) => (vec![id.to_string()], vec![]),
        // The unconfirmed ones aren't posted from here, but someone has to know.
        None => (db::unannounced(&app.pool).await?, db::unconfirmed(&app.pool).await?),
    };
    if ids.is_empty() && held.is_empty() {
        println!("every published course has been announced");
    }
    for id in &held {
        held_back(id);
    }
    let mut posted = 0;
    for id in &ids {
        match db::artifact(&app.pool, id, db::ANNOUNCED).await?.as_deref() {
            Some(db::UNCONFIRMED) if !again => {
                held_back(id);
                continue;
            }
            Some(state) if state != db::PENDING && state != db::UNCONFIRMED => {
                println!("{id} was announced already");
                continue;
            }
            _ => {}
        }
        let (meta, row, outline, lessons) = stored(&app.pool, id).await?;
        if row.aero_url.is_none() || row.aero_page_id.as_deref() == Some("preview") {
            bail!("{id} isn't published");
        }
        let text = telegram::message(&meta, &outline, &lessons, &telegram::link(&app.cfg.public_origin, id));
        if !app.cfg.publish {
            println!("--- {id} (dry run; PUBLISH=1 posts it)\n{text}\n");
            continue;
        }
        if again {
            db::release_announcement(&app.pool, id).await?;
        }
        // A channel takes about 20 posts a minute; stay well under that.
        if posted > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        }
        match telegram::announce(&app, id, &text).await? {
            Some(m) => {
                println!("announced {id} (message {m})");
                posted += 1;
            }
            None => println!("{id} was posted, or is being posted, by another process"),
        }
    }
    Ok(())
}

/// `ingest ID`: read one paper the way a build does and print what arXiv
/// returned, without the database or the model.
async fn ingest_one(id: &str) -> Result<()> {
    let client = arxiv::Client::new()?;
    let meta = client.meta(id).await?.context("no such paper")?;
    let license = client.license(id).await.map_err(|e| format!("{e:#}"));
    println!("{} v{} — {}\nlicense: {license:?}\ncategories: {:?}", meta.id, meta.version, meta.title, meta.categories);
    let paper = match client.html(id, meta.version).await? {
        Some((url, html)) => ingest::from_latexml(&url, &html)?,
        None => match client.eprint(id, meta.version).await? {
            Some(bytes) => pipeline::read_eprint(bytes, config::PdfSettings::from_env()?).await?,
            None => bail!("no source"),
        },
    };
    if let Some(pdf) = &paper.pdf {
        println!("source: pdf | {} pages, sent whole | {} KB", pdf.pages, pdf.base64.len() * 3 / 4 / 1024);
        return Ok(());
    }
    if !paper.pages.is_empty() {
        // What the model reads is the pages, so write them out to look at.
        use base64::Engine;
        let dir = format!("tmp/{}", arxiv::alias(id));
        std::fs::create_dir_all(&dir)?;
        let mut kb = 0;
        for (i, page) in paper.pages.iter().enumerate() {
            let png = base64::prelude::BASE64_STANDARD.decode(page)?;
            kb += png.len() / 1024;
            std::fs::write(format!("{dir}/p{:02}.png", i + 1), png)?;
        }
        println!("source: pdf | {} pages as images | {kb} KB\nwrote {dir}/", paper.pages.len());
        return Ok(());
    }
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
    // A figure whose image cannot load, so the page check has one to find.
    let figures = vec![ingest::Figure {
        key: "F1".into(),
        url: "https://figures.invalid/missing.png".into(),
        caption: "A figure that does not load.".into(),
    }];
    lessons[0].figure = "F1".into();
    // Money written with $, which KaTeX sets as math, for the formula check to
    // find: in a lesson, and in a quiz explanation (rendered only on an answer).
    lessons[1].prose_html.push_str("<p>One run costs $5 on a laptop and $3 on a server.</p>");
    outline.quiz[0].explanation = "Because one run costs $5 here and $3 there.".into();
    let c = course::Course {
        meta: &meta,
        outline: &outline,
        lessons: &lessons,
        figures: &figures,
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
    // The sample is only worth running if QA gives each lab the right verdict.
    let mut wrong: Vec<String> = report
        .labs
        .iter()
        .filter_map(|l| {
            let name = &outline.lessons[l.lesson - 1].nav_title;
            let should_pass = sample::GOOD.contains(&name.as_str());
            (l.ok != should_pass).then(|| format!("lab {} ({name}) should {} but {}", l.lesson, if should_pass { "pass" } else { "fail" }, if l.ok { "passed" } else { "failed" }))
        })
        .collect();
    let expected: &[(&str, &str)] = if skip_hang {
        &[
            ("the broken figure", "figure image didn't load"),
            ("the money in lesson 2", "math in lesson 2 that reads like a sentence"),
            ("the money in quiz question 1", "math in quiz question 1 that reads like a sentence"),
        ]
    } else {
        &[("the skipped page check", "skipped because a lab hangs")]
    };
    for (what, expected) in expected {
        if !report.page_problems.iter().any(|p| p.contains(expected)) {
            wrong.push(format!("{what} was not reported"));
        }
    }
    if !wrong.is_empty() {
        bail!("browser QA gave the wrong verdict:\n  {}", wrong.join("\n  "));
    }
    println!("every sample lab got the right verdict");
    Ok(())
}
