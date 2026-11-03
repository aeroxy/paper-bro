//! Each new course, announced once on the paper-bro Telegram channel
//! (t.me/arxiv_courses) through the Bot API, so that someone who asked for a
//! course can follow the channel instead of watching the build. The bot is one
//! of the channel's admins, with "Post Messages". The message's id is kept
//! with the course's artifacts (`db::ANNOUNCED`), so a rebuild, a republish or
//! a retried job never posts a course twice.

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::time::Duration;

use crate::app::App;
use crate::arxiv::Meta;
use crate::config::{BASE, Telegram};
use crate::course::{Lesson, Outline, esc};
use crate::db;

/// The address a post links: the course's paper page on paper-bro, which
/// redirects to wherever the course is published. A post stays good if the
/// courses move.
pub fn link(origin: &str, id: &str) -> String {
    format!("{origin}{BASE}/abs/{id}")
}

/// At most `max` characters of `s`, cut at a space and marked with an
/// ellipsis.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    let head = head.rsplit_once(' ').map_or(head.as_str(), |(h, _)| h);
    format!("{}…", head.trim_end())
}

/// The announcement, in Telegram's HTML: the course's title and lede, the
/// paper it teaches (linked to arXiv) and its authors, what it holds, and the
/// link. Everything quoted is escaped: the title and the lede are the model's,
/// the rest is arXiv's. Neither has a hard limit, and Telegram refuses a
/// message over 4096 characters, so each is clipped to keep the whole well
/// under that.
pub fn message(meta: &Meta, outline: &Outline, lessons: &[Lesson], url: &str) -> String {
    let authors = match meta.authors.as_slice() {
        [a, b, c, _, ..] => format!("{a}, {b}, {c} et al."),
        all => all.join(", "),
    };
    let labs = match lessons.iter().filter(|l| !l.lab_dropped).count() {
        0 => String::new(),
        1 => ", 1 hands-on lab".into(),
        n => format!(", {n} hands-on labs"),
    };
    format!(
        "<b>{title}</b>\n\n{lede}\n\n📄 <a href=\"https://arxiv.org/abs/{id}\">{paper}</a>\n{authors}\n🧪 {n} lessons{labs}\n\n<a href=\"{url}\">Open the course →</a>",
        title = esc(&clip(&outline.course_title, 150)),
        lede = esc(&clip(&outline.lede, 1000)),
        paper = esc(&clip(&meta.title, 300)),
        authors = esc(&clip(&authors, 250)),
        id = esc(&meta.id),
        n = lessons.len(),
        url = esc(url),
    )
}

/// How a post failed, and so whether the channel may have it anyway.
#[derive(Debug)]
enum SendError {
    /// The request never reached Telegram, or Telegram refused it: nothing was
    /// posted, and posting again is safe.
    NotPosted(anyhow::Error),
    /// Telegram may have taken it, but its reply was lost: a timeout, an
    /// unreadable reply, or one without a message id.
    Unknown(anyhow::Error),
}

/// Post `text` to the channel; returns the message's id.
async fn send(tg: &Telegram, text: &str) -> Result<i64, SendError> {
    let payload = json!({
        "chat_id": tg.chat_id,
        "text": text,
        "parse_mode": "HTML",
        // The course page's preview would only repeat its title and lede.
        "link_preview_options": {"is_disabled": true},
    });
    post(&format!("https://api.telegram.org/bot{}/sendMessage", tg.token), &payload, Duration::from_secs(30)).await
}

/// One Bot API call that posts a message.
async fn post(url: &str, payload: &Value, timeout: Duration) -> Result<i64, SendError> {
    use SendError::{NotPosted, Unknown};
    let http = reqwest::Client::builder().timeout(timeout).build().map_err(|e| NotPosted(e.into()))?;
    // The token is part of the URL, and reqwest quotes the URL in its errors:
    // strip it, so that a failure never writes the token into a log.
    let resp = http.post(url).json(payload).send().await.map_err(|e| {
        // Only a failed connection proves that the request never got there.
        let connect = e.is_connect();
        let why = anyhow!("Telegram request failed: {}", e.without_url());
        if connect { NotPosted(why) } else { Unknown(why) }
    })?;
    let reply: Value = resp.json().await.map_err(|e| Unknown(anyhow!("unreadable Telegram reply: {}", e.without_url())))?;
    outcome(&reply)
}

/// What Telegram's reply says about a post: its message id, or how it failed.
fn outcome(reply: &Value) -> Result<i64, SendError> {
    use SendError::{NotPosted, Unknown};
    match reply["ok"].as_bool() {
        Some(true) => reply["result"]["message_id"].as_i64().ok_or_else(|| Unknown(anyhow!("Telegram took the message but sent no message id"))),
        Some(false) => {
            let why = anyhow!("Telegram refused the message: {}", reply["description"].as_str().unwrap_or("no reason given"));
            // A refusal is final, except Telegram's own failure (5xx), after
            // which the post may exist or not.
            Err(if reply["error_code"].as_i64().is_some_and(|c| c >= 500) { Unknown(why) } else { NotPosted(why) })
        }
        None => Err(Unknown(anyhow!("a reply that isn't Telegram's"))),
    }
}

/// Post a published course, unless it has been already, and record it.
/// Returns the message's id, or None when there was nothing to do: no
/// TELEGRAM_* settings, or another process posted it or is posting it.
pub async fn announce(app: &App, id: &str, text: &str) -> Result<Option<i64>> {
    let Some(tg) = &app.cfg.telegram else { return Ok(None) };
    // A publishing worker and `paper-bro announce` can reach the same course
    // at once; the claim lets only one of them post.
    if !db::claim_announcement(&app.pool, id).await? {
        return Ok(None);
    }
    // From here the channel may get the post, so the claim must not expire
    // whatever happens next. If this write fails, nothing has been sent, and
    // the pending claim expires harmlessly. If it finds the claim gone, this
    // process stalled past the takeover, and the one that took over posts.
    if !db::mark_unconfirmed(&app.pool, id).await? {
        return Ok(None);
    }
    match send(tg, text).await {
        // Should recording the id fail, the claim stays unconfirmed: the course
        // is posted, and won't be again.
        Ok(message_id) => {
            db::put_artifact(&app.pool, id, db::ANNOUNCED, &message_id.to_string())
                .await
                .with_context(|| format!("{id} was posted (message {message_id}), but that wasn't recorded; it stays held back"))?;
            Ok(Some(message_id))
        }
        // Nothing was posted: give the claim back, so that a later try can post.
        Err(SendError::NotPosted(e)) => {
            db::release_announcement(&app.pool, id).await.with_context(|| format!("{e:#}; {id} stays held back"))?;
            Err(e)
        }
        // It may be on the channel: the claim stays unconfirmed until someone looks.
        Err(SendError::Unknown(e)) => Err(e.context(format!(
            "the post may have reached the channel anyway; if it isn't there, `paper-bro announce {id} --again` posts it"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_escapes_what_it_quotes_and_links_the_paper_and_the_course() {
        let (mut meta, mut outline, mut lessons) = crate::sample::course();
        meta.authors = ["A", "B", "C", "D"].map(String::from).to_vec();
        outline.course_title = "Bits & <Pieces>".into();
        lessons.truncate(3);
        lessons[0].lab_dropped = true;
        let m = message(&meta, &outline, &lessons, "https://aero.example/p/arxiv-0000-00000");
        assert_eq!(
            m,
            "<b>Bits &amp; &lt;Pieces&gt;</b>\n\nThis checks that QA catches broken labs.\n\n📄 <a href=\"https://arxiv.org/abs/0000.00000\">A Sample Paper</a>\nA, B, C et al.\n🧪 3 lessons, 2 hands-on labs\n\n<a href=\"https://aero.example/p/arxiv-0000-00000\">Open the course →</a>"
        );
        // Up to three authors are all named.
        meta.authors.truncate(3);
        let short = message(&meta, &outline, &lessons, "u");
        assert!(short.contains("</a>\nA, B, C\n🧪"), "{short}");
    }

    #[test]
    fn a_runaway_title_or_lede_is_clipped_so_that_telegram_takes_the_message() {
        let (meta, mut outline, lessons) = crate::sample::course();
        outline.course_title = "T".repeat(500);
        outline.lede = "word & ".repeat(2000);
        let m = message(&meta, &outline, &lessons, "https://aero.example/paper-bro/abs/0000.00000");
        // The markup counts too, so this is stricter than Telegram's own limit.
        assert!(m.encode_utf16().count() < 4096, "{} UTF-16 units", m.encode_utf16().count());
        assert!(m.starts_with(&format!("<b>{}…</b>", "T".repeat(150))), "{m}");
        assert!(m.contains("word &amp; word…\n\n📄 "), "the lede ends at a word, then the rest follows: {m}");
        assert!(m.ends_with("Open the course →</a>"), "{m}");
        assert_eq!(clip("short", 10), "short");
    }

    #[test]
    fn a_refusal_is_final_but_a_reply_that_says_nothing_clear_is_not() {
        let posted = |v: Value| matches!(outcome(&v), Ok(7));
        let not_posted = |v: Value| matches!(outcome(&v), Err(SendError::NotPosted(_)));
        let unknown = |v: Value| matches!(outcome(&v), Err(SendError::Unknown(_)));
        assert!(posted(json!({"ok": true, "result": {"message_id": 7}})));
        assert!(not_posted(json!({"ok": false, "error_code": 400, "description": "Bad Request: chat not found"})));
        assert!(not_posted(json!({"ok": false, "error_code": 429, "description": "Too Many Requests: retry after 5"})));
        assert!(unknown(json!({"ok": false, "error_code": 502, "description": "Bad Gateway"})));
        assert!(unknown(json!({"ok": true, "result": {}})));
        assert!(unknown(json!({"something": "else"})));
    }

    /// A stand-in for the Bot API that answers every request with `reply`
    /// after `delay`. Its address carries a token, as the real one does.
    async fn fake_telegram(delay: Duration, reply: &'static str) -> String {
        let app = axum::Router::new().fallback(move || async move {
            tokio::time::sleep(delay).await;
            reply
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/botSECRET123/sendMessage", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url
    }

    #[tokio::test]
    async fn only_a_post_that_surely_failed_counts_as_not_posted() {
        let payload = json!({"text": "x"});
        let quick = Duration::from_millis(500);
        // Nobody listening: the request never left this machine.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        assert!(matches!(post(&format!("http://{closed}/botSECRET123/sendMessage"), &payload, quick).await, Err(SendError::NotPosted(_))));
        // Telegram refused it.
        let url = fake_telegram(Duration::ZERO, r#"{"ok":false,"error_code":400,"description":"Bad Request"}"#).await;
        assert!(matches!(post(&url, &payload, quick).await, Err(SendError::NotPosted(_))));
        // No reply in time, or one that can't be read: it may have been posted.
        let url = fake_telegram(Duration::from_secs(5), "{}").await;
        let timed_out = post(&url, &payload, quick).await;
        assert!(matches!(timed_out, Err(SendError::Unknown(_))), "{timed_out:?}");
        assert!(!format!("{timed_out:?}").contains("SECRET123"), "the token leaked: {timed_out:?}");
        let url = fake_telegram(Duration::ZERO, "<html>Bad Gateway</html>").await;
        assert!(matches!(post(&url, &payload, quick).await, Err(SendError::Unknown(_))));
        // And a post Telegram took.
        let url = fake_telegram(Duration::ZERO, r#"{"ok":true,"result":{"message_id":42}}"#).await;
        assert!(matches!(post(&url, &payload, quick).await, Ok(42)));
    }

    #[test]
    fn a_post_links_the_course_through_paper_bro() {
        assert_eq!(link("https://aero.oops.wtf", "2006.11239"), "https://aero.oops.wtf/paper-bro/abs/2006.11239");
        assert_eq!(link("https://aero.oops.wtf", "hep-th/9901001"), "https://aero.oops.wtf/paper-bro/abs/hep-th/9901001");
    }
}
