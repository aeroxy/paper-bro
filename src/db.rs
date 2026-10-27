//! Postgres (Neon) access. Raw SQL, no ORM, like AERO.
//!
//! The pool is shaped around Neon's free plan, which suspends the compute
//! after five idle minutes and allows 100 CU-hours a month. A compute that
//! never sleeps would burn through that by mid-month, so: no minimum pool
//! size, idle connections close after a minute (sqlx's default is ten), and
//! nothing in paper-bro polls. Hot reads are answered from memory
//! (`state::Courses`); this module is only for writes, the job queue, and
//! cold starts.

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{FromRow, PgPool};
use std::str::FromStr;
use std::time::Duration;

use crate::arxiv::Meta;

pub async fn connect(url: &str) -> Result<PgPool> {
    // Neon's connection strings carry `channel_binding=require`, which sqlx
    // doesn't implement and warns about on every connect. TLS still applies.
    let mut url = reqwest::Url::parse(url).context("NEON_DB is not a valid Postgres URL")?;
    let query: Vec<(String, String)> =
        url.query_pairs().filter(|(k, _)| k != "channel_binding").map(|(k, v)| (k.into(), v.into())).collect();
    url.query_pairs_mut().clear().extend_pairs(query);
    let opts = PgConnectOptions::from_str(url.as_str()).context("NEON_DB is not a valid Postgres URL")?;
    let pool = PgPoolOptions::new()
        .min_connections(0)
        .max_connections(5)
        .idle_timeout(Duration::from_secs(60))
        // A suspended Neon compute takes a few hundred ms to wake; leave room.
        .acquire_timeout(Duration::from_secs(20))
        .connect_with(opts)
        .await
        .context("postgres connect failed")?;
    sqlx::migrate!("./migrations").run(&pool).await.context("migrations failed")?;
    Ok(pool)
}

pub async fn upsert_paper(pool: &PgPool, m: &Meta) -> Result<()> {
    sqlx::query(
        "INSERT INTO papers (arxiv_id, version, title, authors, abstract, categories, license_url, published_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (arxiv_id) DO UPDATE SET
           version = EXCLUDED.version, title = EXCLUDED.title, authors = EXCLUDED.authors,
           abstract = EXCLUDED.abstract, categories = EXCLUDED.categories,
           license_url = EXCLUDED.license_url, published_at = EXCLUDED.published_at,
           fetched_at = now()",
    )
    .bind(&m.id)
    .bind(m.version as i32)
    .bind(&m.title)
    .bind(serde_json::to_value(&m.authors)?)
    .bind(&m.abstract_)
    .bind(&m.categories)
    .bind(&m.license)
    .bind(m.published)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_paper(pool: &PgPool, id: &str) -> Result<Option<Meta>> {
    #[derive(FromRow)]
    struct Row {
        arxiv_id: String,
        version: i32,
        title: String,
        authors: Value,
        #[sqlx(rename = "abstract")]
        abstract_: String,
        categories: Vec<String>,
        license_url: Option<String>,
        published_at: Option<chrono::DateTime<chrono::Utc>>,
    }
    let row: Option<Row> = sqlx::query_as(
        "SELECT arxiv_id, version, title, authors, abstract, categories, license_url, published_at
         FROM papers WHERE arxiv_id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| Meta {
        id: r.arxiv_id,
        version: r.version as u32,
        title: r.title,
        authors: serde_json::from_value(r.authors).unwrap_or_default(),
        abstract_: r.abstract_,
        categories: r.categories,
        license: r.license_url,
        published: r.published_at,
    }))
}

pub async fn set_source_kind(pool: &PgPool, id: &str, kind: &str) -> Result<()> {
    sqlx::query("UPDATE papers SET source_kind = $2 WHERE arxiv_id = $1")
        .bind(id)
        .bind(kind)
        .execute(pool)
        .await?;
    Ok(())
}

/// Queue a course. Returns false when the paper already has one, in any state.
pub async fn enqueue(pool: &PgPool, id: &str) -> Result<bool> {
    let done = sqlx::query(
        "INSERT INTO courses (arxiv_id, status) VALUES ($1, 'queued') ON CONFLICT (arxiv_id) DO NOTHING",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Put a failed (or, with `force`, any) course back in the queue.
pub async fn requeue(pool: &PgPool, id: &str, force: bool) -> Result<bool> {
    let done = sqlx::query(
        "UPDATE courses SET status = 'queued', stage = NULL, attempts = 0, error = NULL,
                lease_until = NULL, requested_at = now()
         WHERE arxiv_id = $1 AND ($2 OR status = 'failed')",
    )
    .bind(id)
    .bind(force)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Claim the oldest queued course, or a running one whose worker died
/// (its lease ran out). SKIP LOCKED keeps two instances from taking the
/// same job during a deploy overlap.
pub async fn claim(pool: &PgPool, lease: Duration) -> Result<Option<String>> {
    let id: Option<(String,)> = sqlx::query_as(
        "UPDATE courses SET status = 'running', attempts = attempts + 1,
                lease_until = now() + make_interval(secs => $1),
                started_at = coalesce(started_at, now())
         WHERE arxiv_id = (
             SELECT arxiv_id FROM courses
             WHERE status = 'queued' OR (status = 'running' AND lease_until < now())
             ORDER BY requested_at
             FOR UPDATE SKIP LOCKED
             LIMIT 1)
         RETURNING arxiv_id",
    )
    .bind(lease.as_secs_f64())
    .fetch_optional(pool)
    .await?;
    Ok(id.map(|r| r.0))
}

/// Claim one particular course, if it's queued (the `build` command).
pub async fn claim_id(pool: &PgPool, id: &str, lease: Duration) -> Result<bool> {
    let done = sqlx::query(
        "UPDATE courses SET status = 'running', attempts = attempts + 1,
                lease_until = now() + make_interval(secs => $2),
                started_at = coalesce(started_at, now())
         WHERE arxiv_id = $1 AND status = 'queued'",
    )
    .bind(id)
    .bind(lease.as_secs_f64())
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

pub async fn attempts(pool: &PgPool, id: &str) -> Result<i32> {
    let (n,): (i32,) = sqlx::query_as("SELECT attempts FROM courses WHERE arxiv_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await?;
    Ok(n)
}

pub async fn heartbeat(pool: &PgPool, id: &str, lease: Duration) -> Result<()> {
    sqlx::query(
        "UPDATE courses SET lease_until = now() + make_interval(secs => $2)
         WHERE arxiv_id = $1 AND status = 'running'",
    )
    .bind(id)
    .bind(lease.as_secs_f64())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_stage(pool: &PgPool, id: &str, stage: &str, usage: &Value) -> Result<()> {
    sqlx::query("UPDATE courses SET stage = $2, usage = $3 WHERE arxiv_id = $1")
        .bind(id)
        .bind(stage)
        .bind(usage)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn finish_ready(
    pool: &PgPool,
    id: &str,
    page_id: &str,
    url: &str,
    version: u32,
    model: &str,
    usage: &Value,
) -> Result<()> {
    sqlx::query(
        "UPDATE courses SET status = 'ready', stage = NULL, aero_page_id = $2, aero_url = $3,
                built_version = $4, model = $5, usage = $6, error = NULL,
                lease_until = NULL, finished_at = now()
         WHERE arxiv_id = $1",
    )
    .bind(id)
    .bind(page_id)
    .bind(url)
    .bind(version as i32)
    .bind(model)
    .bind(usage)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record a failure. `retry` puts it back in the queue for another attempt.
pub async fn fail(pool: &PgPool, id: &str, error: &str, retry: bool, usage: &Value) -> Result<()> {
    sqlx::query(
        "UPDATE courses SET status = CASE WHEN $3 THEN 'queued' ELSE 'failed' END,
                error = $2, usage = $4, lease_until = NULL,
                finished_at = CASE WHEN $3 THEN NULL ELSE now() END
         WHERE arxiv_id = $1",
    )
    .bind(id)
    .bind(error)
    .bind(retry)
    .bind(usage)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_removed(pool: &PgPool, id: &str) -> Result<()> {
    sqlx::query("UPDATE courses SET status = 'removed', aero_url = NULL WHERE arxiv_id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

#[derive(Clone, Debug, FromRow, serde::Serialize)]
pub struct CourseRow {
    pub arxiv_id: String,
    pub status: String,
    pub stage: Option<String>,
    pub aero_page_id: Option<String>,
    pub aero_url: Option<String>,
    pub error: Option<String>,
    pub title: String,
    pub requested_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
}

const COURSE_SELECT: &str = "SELECT c.arxiv_id, c.status, c.stage, c.aero_page_id, c.aero_url, c.error,
        p.title, c.requested_at, c.finished_at
 FROM courses c JOIN papers p USING (arxiv_id)";

pub async fn all_courses(pool: &PgPool) -> Result<Vec<CourseRow>> {
    Ok(sqlx::query_as(COURSE_SELECT).fetch_all(pool).await?)
}

pub async fn course(pool: &PgPool, id: &str) -> Result<Option<CourseRow>> {
    Ok(sqlx::query_as(&format!("{COURSE_SELECT} WHERE c.arxiv_id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

/// Builds requested today and money spent today, for the admission limits.
pub async fn today(pool: &PgPool) -> Result<(i64, i64, f64)> {
    let row: (i64, i64, Option<f64>) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE requested_at >= date_trunc('day', now())),
                count(*) FILTER (WHERE status = 'queued'),
                sum((usage->>'cost_usd')::float8)
                  FILTER (WHERE coalesce(finished_at, now()) >= date_trunc('day', now()))
         FROM courses",
    )
    .fetch_one(pool)
    .await?;
    Ok((row.0, row.1, row.2.unwrap_or(0.0)))
}

pub async fn artifact(pool: &PgPool, id: &str, key: &str) -> Result<Option<String>> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT body FROM course_artifacts WHERE arxiv_id = $1 AND key = $2")
            .bind(id)
            .bind(key)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|r| r.0))
}

pub async fn put_artifact(pool: &PgPool, id: &str, key: &str, body: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO course_artifacts (arxiv_id, key, body) VALUES ($1, $2, $3)
         ON CONFLICT (arxiv_id, key) DO UPDATE SET body = EXCLUDED.body, created_at = now()",
    )
    .bind(id)
    .bind(key)
    .bind(body)
    .execute(pool)
    .await?;
    Ok(())
}

/// Once a course is ready, drop the paper text (arXiv serves it again for
/// free) and the reviews. The outline and lessons stay: they are what lets a
/// course be re-assembled with a newer shell without paying the model again.
pub async fn prune_artifacts(pool: &PgPool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM course_artifacts WHERE arxiv_id = $1 AND (key = 'paper' OR key LIKE 'review:%')")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Courses finished by a dry run (`PUBLISH=0`) only exist as local previews.
/// A publishing server queues them again so they reach AERO; their lessons
/// are reused, so this costs a review, not a rebuild.
pub async fn requeue_previews(pool: &PgPool) -> Result<u64> {
    let done = sqlx::query(
        "UPDATE courses SET status = 'queued', stage = NULL, attempts = 0, aero_page_id = NULL,
                aero_url = NULL, requested_at = now()
         WHERE aero_page_id = 'preview' AND status = 'ready'",
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected())
}

/// Drop every checkpoint, so a forced rebuild starts from scratch.
pub async fn clear_artifacts(pool: &PgPool, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM course_artifacts WHERE arxiv_id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

#[derive(Clone, Debug, FromRow, serde::Serialize)]
pub struct Event {
    pub id: i64,
    pub at: chrono::DateTime<chrono::Utc>,
    pub stage: String,
    pub message: String,
}

pub async fn add_event(pool: &PgPool, id: &str, stage: &str, message: &str) -> Result<Event> {
    Ok(sqlx::query_as(
        "INSERT INTO course_events (arxiv_id, stage, message) VALUES ($1, $2, $3)
         RETURNING id, at, stage, message",
    )
    .bind(id)
    .bind(stage)
    .bind(message)
    .fetch_one(pool)
    .await?)
}

pub async fn events(pool: &PgPool, id: &str) -> Result<Vec<Event>> {
    Ok(sqlx::query_as(
        "SELECT id, at, stage, message FROM course_events WHERE arxiv_id = $1 ORDER BY id",
    )
    .bind(id)
    .fetch_all(pool)
    .await?)
}
