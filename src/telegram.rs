//! Each new course, announced once on the paper-bro Telegram channel
//! (t.me/arxiv_courses) through the Bot API, so that someone who asked for a
//! course can follow the channel instead of watching the build. The bot is one
//! of the channel's admins, with "Post Messages". The message's id is kept
//! with the course's artifacts (`db::ANNOUNCED`), so a rebuild, a republish or
//! a retried job never posts a course twice.

use anyhow::{Context, Result, anyhow, bail};
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

/// The announcement, in Telegram's HTML: the course's title and lede, the
/// paper it teaches (linked to arXiv) and its authors, what it holds, and the
/// link. Everything quoted is escaped: the title and the lede are the model's,
/// the rest is arXiv's.
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
        title = esc(&outline.course_title),
        lede = esc(&outline.lede),
        paper = esc(&meta.title),
        authors = esc(&authors),
        id = esc(&meta.id),
        n = lessons.len(),
        url = esc(url),
    )
}

/// Post `text` to the channel; returns the message's id.
pub async fn send(tg: &Telegram, text: &str) -> Result<i64> {
    let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
    // The token is part of the URL, and reqwest quotes the URL in its errors:
    // strip it, so that a failure never writes the token into a log.
    let resp = http
        .post(format!("https://api.telegram.org/bot{}/sendMessage", tg.token))
        .json(&json!({
            "chat_id": tg.chat_id,
            "text": text,
            "parse_mode": "HTML",
            // The course page's preview would only repeat its title and lede.
            "link_preview_options": {"is_disabled": true},
        }))
        .send()
        .await
        .map_err(|e| anyhow!("Telegram request failed: {}", e.without_url()))?;
    let body: Value = resp.json().await.map_err(|e| anyhow!("unreadable Telegram reply: {}", e.without_url()))?;
    if body["ok"].as_bool() != Some(true) {
        bail!("Telegram refused the message: {}", body["description"].as_str().unwrap_or("no reason given"));
    }
    body["result"]["message_id"].as_i64().context("Telegram reply without a message id")
}

/// True once the course has been posted.
pub async fn announced(app: &App, id: &str) -> Result<bool> {
    Ok(db::artifact(&app.pool, id, db::ANNOUNCED).await?.is_some())
}

/// Post a published course, unless it has been already, and record it.
/// Returns the message's id, or None when there was nothing to do: no
/// TELEGRAM_* settings, or the course was announced before.
pub async fn announce(app: &App, id: &str, text: &str) -> Result<Option<i64>> {
    let Some(tg) = &app.cfg.telegram else { return Ok(None) };
    if announced(app, id).await? {
        return Ok(None);
    }
    let message_id = send(tg, text).await?;
    db::put_artifact(&app.pool, id, db::ANNOUNCED, &message_id.to_string()).await?;
    Ok(Some(message_id))
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
    fn a_post_links_the_course_through_paper_bro() {
        assert_eq!(link("https://aero.oops.wtf", "2006.11239"), "https://aero.oops.wtf/paper-bro/abs/2006.11239");
        assert_eq!(link("https://aero.oops.wtf", "hep-th/9901001"), "https://aero.oops.wtf/paper-bro/abs/hep-th/9901001");
    }
}
