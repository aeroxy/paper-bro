//! arXiv: paper ids, and a client that keeps to arXiv's terms of use.
//!
//! The terms allow one request every three seconds over a single connection,
//! counted across every machine we run. paper-bro is one process, so one
//! mutex held for the whole request is the entire rate limiter.
//!
//! We fetch metadata (CC0) and the paper itself to read it. We never serve the
//! PDF or source, and keep what we read of a paper (its text, or the PDF or
//! pictures of its pages) only as a checkpoint for its build: it goes when
//! the course is ready or fails for good, and a PDF's within the hour once no
//! build is using it (db.rs). Courses link to the abstract page.

use anyhow::{Context, Result, bail};
use regex::Regex;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// A canonical arXiv id: no version, no prefix. `1706.03762`, `hep-th/9711200`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaperId {
    pub id: String,
    pub version: Option<u32>,
}

static NEW_STYLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d{4}\.\d{4,5})(?:v(\d+))?$").unwrap());
static OLD_STYLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([a-z][a-z\-]*(?:\.[A-Za-z]{2})?/\d{7})(?:v(\d+))?$").unwrap());

impl PaperId {
    /// Accepts bare ids, `arXiv:` ids, and arxiv.org (or paper-bro) URLs for
    /// the abstract, PDF, HTML, or source.
    pub fn parse(input: &str) -> Option<Self> {
        let mut s = input.trim();
        for scheme in ["https://", "http://"] {
            if let Some(rest) = s.strip_prefix(scheme) {
                s = rest;
            }
        }
        let lower = s.to_ascii_lowercase();
        for host in ["www.arxiv.org/", "export.arxiv.org/", "arxiv.org/", "aero.oops.wtf/paper-bro/"] {
            if lower.starts_with(host) {
                s = &s[host.len()..];
                let lower = s.to_ascii_lowercase();
                for kind in ["abs/", "pdf/", "html/", "format/", "src/", "e-print/"] {
                    if lower.starts_with(kind) {
                        s = &s[kind.len()..];
                        break;
                    }
                }
                break;
            }
        }
        if s.len() >= 6 && s[..6].eq_ignore_ascii_case("arxiv:") {
            s = &s[6..];
        }
        let s = s.split(['?', '#']).next().unwrap_or("").trim_end_matches('/');
        let s = s.strip_suffix(".pdf").unwrap_or(s);

        let caps = NEW_STYLE.captures(s).or_else(|| OLD_STYLE.captures(s))?;
        let mut id = caps[1].to_string();
        // New-style numbers had four digits through 1412 and five from 1501.
        if let Some((yymm, num)) = id.split_once('.').filter(|_| !id.contains('/')) {
            if (yymm >= "1501") != (num.len() == 5) {
                return None;
            }
        }
        // Old-style ids keep arXiv's own casing: lowercase archive, uppercase
        // subject class (`math.GT/0309136`).
        if let Some((archive, num)) = id.split_once('/') {
            let archive = match archive.split_once('.') {
                Some((a, class)) => format!("{}.{}", a.to_ascii_lowercase(), class.to_ascii_uppercase()),
                None => archive.to_ascii_lowercase(),
            };
            id = format!("{archive}/{num}");
        }
        let version = caps.get(2).and_then(|v| v.as_str().parse().ok());
        Some(Self { id, version })
    }
}

/// The name of a paper's course page: `arxiv-1706-03762`.
pub fn alias(id: &str) -> String {
    format!("arxiv-{}", id.to_ascii_lowercase().replace(['/', '.'], "-"))
}

pub fn abs_url(id: &str) -> String {
    format!("https://arxiv.org/abs/{id}")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    /// Latest version at fetch time.
    pub version: u32,
    pub title: String,
    pub authors: Vec<String>,
    #[serde(rename = "abstract")]
    pub abstract_: String,
    pub categories: Vec<String>,
    /// From `Client::license`. `Client::meta` leaves it `None`; a build
    /// fills it in.
    pub license: Option<String>,
    pub published: Option<chrono::DateTime<chrono::Utc>>,
}

pub struct Client {
    http: reqwest::Client,
    last: Mutex<Option<Instant>>,
}

const GAP: Duration = Duration::from_secs(3);
/// Long enough for a big source tarball.
const TIMEOUT: Duration = Duration::from_secs(90);
/// The lock is held for a whole request, so a hung one stalls every other
/// arXiv call. The license is optional, so its lookup gives up much sooner.
const LICENSE_TIMEOUT: Duration = Duration::from_secs(10);
/// An abstract page for a visitor's lookup, with the export API behind it.
const PAGE_TIMEOUT: Duration = Duration::from_secs(10);

/// What came back from a paced GET.
pub enum Fetched {
    Ok { url: String, body: Vec<u8> },
    NotFound,
}

impl Client {
    /// No timeout on the client itself: each request brings its own (see
    /// `get`).
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("paper-bro/0.1 (+https://aero.oops.wtf/paper-bro)")
            .build()?;
        Ok(Self { http, last: Mutex::new(None) })
    }

    /// GET with arXiv's pacing. The lock is held across the request, which
    /// is what makes it one connection at a time.
    async fn get(&self, url: &str, timeout: Duration) -> Result<Fetched> {
        let mut last = self.last.lock().await;
        if let Some(t) = *last {
            let wait = GAP.saturating_sub(t.elapsed());
            tokio::time::sleep(wait).await;
        }
        let result = async {
            let resp = self.http.get(url).timeout(timeout).send().await.with_context(|| format!("GET {url}"))?;
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                return Ok(Fetched::NotFound);
            }
            let resp = resp.error_for_status().with_context(|| format!("GET {url}"))?;
            let final_url = resp.url().to_string();
            Ok(Fetched::Ok { url: final_url, body: resp.bytes().await?.to_vec() })
        }
        .await;
        *last = Some(Instant::now());
        result
    }

    /// Metadata, without the license (see `license`). `None` when arXiv has no
    /// such paper. A visitor waits for this, so it comes from the abstract
    /// page, which answers in a tenth of a second: the export API can take
    /// half a minute and then refuse with a 429 under arXiv's own load (seen
    /// 2026-11-14, with nothing else calling it). The API stays as the
    /// fallback for a page that fails or no longer parses.
    pub async fn meta(&self, id: &str) -> Result<Option<Meta>> {
        match self.get(&abs_url(id), PAGE_TIMEOUT).await {
            Ok(Fetched::NotFound) => return Ok(None),
            Ok(Fetched::Ok { body, .. }) => match parse_abs(&String::from_utf8_lossy(&body), id) {
                Some(meta) => return Ok(Some(meta)),
                None => log::warn!("the abstract page of {id} didn't parse; asking arXiv's export API"),
            },
            Err(e) => log::warn!("the abstract page of {id} didn't load ({e:#}); asking arXiv's export API"),
        }
        let url = format!("https://export.arxiv.org/api/query?id_list={id}&max_results=1");
        let Fetched::Ok { body, .. } = self.get(&url, TIMEOUT).await? else { return Ok(None) };
        parse_atom(&String::from_utf8_lossy(&body), id)
    }

    /// The paper's license URL, read off its abstract page. OAI-PMH has the
    /// same data but has been seen to hang. `None` when the page names no
    /// license.
    pub async fn license(&self, id: &str) -> Result<Option<String>> {
        let url = format!("https://export.arxiv.org/abs/{id}");
        let Fetched::Ok { body, .. } = self.get(&url, LICENSE_TIMEOUT).await? else { return Ok(None) };
        Ok(parse_license(&String::from_utf8_lossy(&body)))
    }

    /// The LaTeXML rendering, with the URL it was served from (figure paths
    /// resolve against it). `None` when the paper has no HTML version.
    pub async fn html(&self, id: &str, version: u32) -> Result<Option<(String, String)>> {
        match self.get(&format!("https://arxiv.org/html/{id}v{version}"), TIMEOUT).await? {
            Fetched::NotFound => Ok(None),
            Fetched::Ok { url, body } => {
                let text = String::from_utf8_lossy(&body).into_owned();
                // A missing rendering can also come back as a 200 page that
                // says so; a real one always has LaTeXML's document class.
                Ok(text.contains("ltx_document").then_some((url, text)))
            }
        }
    }

    /// The source tarball (or a single gzipped .tex, or a PDF).
    pub async fn eprint(&self, id: &str, version: u32) -> Result<Option<Vec<u8>>> {
        match self.get(&format!("https://arxiv.org/e-print/{id}v{version}"), TIMEOUT).await? {
            Fetched::NotFound => Ok(None),
            Fetched::Ok { body, .. } => Ok(Some(body)),
        }
    }
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn parse_atom(xml: &str, id: &str) -> Result<Option<Meta>> {
    let doc = roxmltree::Document::parse(xml).context("arXiv returned malformed Atom")?;
    let Some(entry) = doc.descendants().find(|n| n.has_tag_name("entry")) else { return Ok(None) };
    let child = |name: &str| {
        entry
            .children()
            .find(|n| n.tag_name().name() == name)
            .and_then(|n| n.text())
            .map(squash)
    };
    // An unknown id still yields an entry, titled "Error".
    let Some(abs) = child("id") else { return Ok(None) };
    if child("title").as_deref() == Some("Error") || !abs.contains("/abs/") {
        return Ok(None);
    }
    let version = abs.rsplit_once('v').and_then(|(_, v)| v.parse().ok()).unwrap_or(1);
    let authors = entry
        .children()
        .filter(|n| n.tag_name().name() == "author")
        .filter_map(|a| a.children().find(|n| n.tag_name().name() == "name").and_then(|n| n.text()))
        .map(squash)
        .collect();
    let mut categories: Vec<String> = Vec::new();
    for n in entry.children() {
        if matches!(n.tag_name().name(), "primary_category" | "category") {
            if let Some(t) = n.attribute("term") {
                if !categories.iter().any(|c| c == t) {
                    categories.push(t.to_string());
                }
            }
        }
    }
    let Some(title) = child("title") else { bail!("arXiv entry for {id} has no title") };
    Ok(Some(Meta {
        id: id.to_string(),
        version,
        title,
        authors,
        abstract_: child("summary").unwrap_or_default(),
        categories,
        license: None,
        published: child("published").and_then(|p| p.parse().ok()),
    }))
}

/// The metadata on an abstract page: the `citation_*` tags for title, authors
/// and abstract, the subjects cell for categories (primary first), and the
/// submission history for the latest version and when v1 was submitted, which
/// is what the export API calls `published`. `None` if any of the title, the
/// abstract or the version is missing, so the API is asked instead.
fn parse_abs(html: &str, id: &str) -> Option<Meta> {
    let doc = Html::parse_document(html);
    let tag = |name: &str| -> Vec<String> {
        let sel = Selector::parse(&format!(r#"meta[name="{name}"]"#)).unwrap();
        doc.select(&sel).filter_map(|m| m.value().attr("content")).map(squash).collect()
    };
    let title = tag("citation_title").into_iter().next().filter(|t| !t.is_empty())?;
    let abstract_ = tag("citation_abstract").into_iter().next().filter(|a| !a.is_empty())?;
    // "Vaswani, Ashish" is "Ashish Vaswani" in the API; a group name has no comma.
    let authors = tag("citation_author")
        .into_iter()
        .map(|a| match a.split_once(", ") {
            Some((last, first)) => format!("{first} {last}"),
            None => a,
        })
        .collect();
    let text = |sel: &str| {
        let sel = Selector::parse(sel).unwrap();
        doc.select(&sel).next().map(|e| squash(&e.text().collect::<String>())).unwrap_or_default()
    };
    let history = text("div.submission-history");
    let version = VERSION.captures_iter(&history).filter_map(|c| c[1].parse::<u32>().ok()).max()?;
    let published = SUBMITTED
        .captures(&history)
        .and_then(|c| chrono::NaiveDateTime::parse_from_str(&c[1], "%a, %d %b %Y %H:%M:%S").ok())
        .map(|t| t.and_utc());
    let mut categories: Vec<String> = Vec::new();
    for c in CATEGORY.captures_iter(&text("td.tablecell.subjects")) {
        if !categories.iter().any(|k| k == &c[1]) {
            categories.push(c[1].to_string());
        }
    }
    Some(Meta { id: id.to_string(), version, title, authors, abstract_, categories, license: None, published })
}

static VERSION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[v(\d+)\]").unwrap());
static SUBMITTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[v1\] (\w{3}, \d{1,2} \w{3} \d{4} \d{2}:\d{2}:\d{2}) UTC").unwrap());
/// A subject's code, in brackets after its name: "Machine Learning (cs.LG)".
static CATEGORY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\(([a-z-]+(?:\.[A-Za-z-]+)?)\)").unwrap());

/// The license link on an abstract page: `<div class="abs-license"><a href=…>`.
fn parse_license(html: &str) -> Option<String> {
    let doc = Html::parse_document(html);
    let link = Selector::parse("div.abs-license a[href]").unwrap();
    doc.select(&link).next()?.value().attr("href").map(|h| h.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Option<(String, Option<u32>)> {
        PaperId::parse(s).map(|p| (p.id, p.version))
    }

    #[test]
    fn new_style_ids_in_every_form() {
        let want = Some(("1706.03762".to_string(), None));
        for s in [
            "1706.03762",
            " 1706.03762 ",
            "arXiv:1706.03762",
            "arxiv:1706.03762",
            "https://arxiv.org/abs/1706.03762",
            "http://www.arxiv.org/abs/1706.03762/",
            "arxiv.org/pdf/1706.03762.pdf",
            "https://arxiv.org/pdf/1706.03762",
            "https://export.arxiv.org/abs/1706.03762?context=cs",
            "https://aero.oops.wtf/paper-bro/abs/1706.03762",
        ] {
            assert_eq!(p(s), want, "{s}");
        }
        assert_eq!(p("https://arxiv.org/html/1706.03762v7"), Some(("1706.03762".into(), Some(7))));
        assert_eq!(p("2410.05258v2"), Some(("2410.05258".into(), Some(2))));
        assert_eq!(p("0704.0001"), Some(("0704.0001".into(), None)));
    }

    #[test]
    fn old_style_ids_keep_arxiv_casing() {
        assert_eq!(p("hep-th/9711200"), Some(("hep-th/9711200".into(), None)));
        assert_eq!(p("https://arxiv.org/abs/hep-th/9711200v3"), Some(("hep-th/9711200".into(), Some(3))));
        assert_eq!(p("math.gt/0309136"), Some(("math.GT/0309136".into(), None)));
        assert_eq!(p("arXiv:cond-mat/0101001"), Some(("cond-mat/0101001".into(), None)));
    }

    #[test]
    fn rejects_things_that_are_not_ids() {
        for s in ["", "hello", "1706.0376", "17060.3762", "https://example.com/abs/1706.03762",
                  "1706.03762; drop table", "../../etc/passwd", "hep-th/971120"] {
            assert_eq!(p(s), None, "{s}");
        }
    }

    #[test]
    fn aliases_are_safe_page_names() {
        let re = Regex::new(r"^[a-z0-9][a-z0-9-]{1,62}$").unwrap();
        for id in ["1706.03762", "hep-th/9711200", "math.GT/0309136"] {
            let a = alias(id);
            assert!(re.is_match(&a), "{a}");
        }
        assert_eq!(alias("1706.03762"), "arxiv-1706-03762");
        assert_eq!(alias("math.GT/0309136"), "arxiv-math-gt-0309136");
    }

    #[test]
    fn atom_parsing() {
        let xml = r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom" xmlns:arxiv="http://arxiv.org/schemas/atom">
          <entry><id>http://arxiv.org/abs/1706.03762v7</id><published>2017-06-12T17:57:34Z</published>
          <title>Attention Is All
            You Need</title><summary> The dominant sequence transduction models...</summary>
          <author><name>Ashish Vaswani</name></author><author><name>Noam Shazeer</name></author>
          <arxiv:primary_category term="cs.CL"/><category term="cs.CL"/><category term="cs.LG"/></entry></feed>"#;
        let m = parse_atom(xml, "1706.03762").unwrap().unwrap();
        assert_eq!(m.version, 7);
        assert_eq!(m.title, "Attention Is All You Need");
        assert_eq!(m.authors, vec!["Ashish Vaswani", "Noam Shazeer"]);
        assert_eq!(m.categories, vec!["cs.CL", "cs.LG"]);
        assert!(m.published.is_some());
    }

    #[test]
    fn abstract_page_parsing() {
        // Trimmed from arxiv.org/abs/1706.03762, plus a group author.
        let page = r#"<html><head>
<meta name="citation_title" content="Attention Is   All You Need" />
<meta name="citation_author" content="Vaswani, Ashish" />
<meta name="citation_author" content="Gomez, Aidan N." />
<meta name="citation_author" content="The ATLAS Collaboration" />
<meta name="citation_date" content="2017/06/12" />
<meta name="citation_abstract" content="The dominant sequence transduction models &amp; more." />
</head><body>
<div class="dateline">[Submitted on 12 Jun 2017 (<a href="https://arxiv.org/abs/1706.03762v1">v1</a>), last revised 2 Aug 2023 (this version, v7)]</div>
<table><tr><td class="tablecell label">Subjects:</td><td class="tablecell subjects"><span class="primary-subject">Computation and Language (cs.CL)</span>; Machine Learning (cs.LG)</td></tr></table>
<div class="submission-history"><h2>Submission history</h2> From: Llion Jones [<a href="/show-email/x/1706.03762">view email</a>]<br/>
<strong><a href="/abs/1706.03762v1" rel="nofollow">[v1]</a></strong> Mon, 12 Jun 2017 17:57:34 UTC (1,102 KB)<br/>
<strong><a href="/abs/1706.03762v2" rel="nofollow">[v2]</a></strong> Mon, 19 Jun 2017 16:49:45 UTC (1,125 KB)<br/>
<strong>[v7]</strong> Wed, 2 Aug 2023 00:41:18 UTC (1,124 KB)<br/></div>
</body></html>"#;
        let m = parse_abs(page, "1706.03762").unwrap();
        assert_eq!(m.title, "Attention Is All You Need");
        assert_eq!(m.authors, ["Ashish Vaswani", "Aidan N. Gomez", "The ATLAS Collaboration"]);
        assert_eq!(m.abstract_, "The dominant sequence transduction models & more.");
        assert_eq!(m.categories, ["cs.CL", "cs.LG"]);
        assert_eq!(m.version, 7);
        assert_eq!(m.published.unwrap().to_rfc3339(), "2017-06-12T17:57:34+00:00");
        // Without a title, the export API is asked instead.
        assert!(parse_abs(&page.replace("citation_title", "x"), "1706.03762").is_none());
    }

    #[test]
    fn license_parsing() {
        // Trimmed from real abstract pages. The PDF link comes first.
        let cc = r#"<ul><li><a href="/pdf/2610.39302">View PDF</a></li></ul>
          <div class="abs-license"><a href="http://creativecommons.org/licenses/by/4.0/" title="Rights to this article" class="has_license">
            <img alt="license icon" role="presentation" src="https://arxiv.org/icons/licenses/by-4.0.png"/>
            <span>view license</span>
          </a></div>"#;
        assert_eq!(parse_license(cc).as_deref(), Some("http://creativecommons.org/licenses/by/4.0/"));
        let default = r#"<div class="abs-license"><a href="http://arxiv.org/licenses/nonexclusive-distrib/1.0/" title="Rights to this article">view license</a></div>"#;
        assert_eq!(parse_license(default).as_deref(), Some("http://arxiv.org/licenses/nonexclusive-distrib/1.0/"));
        assert_eq!(parse_license(r#"<ul><li><a href="/pdf/2610.39302">View PDF</a></li></ul>"#), None);
    }

    /// A service that takes the connection and never answers (OAI-PMH did)
    /// holds the arXiv lock until the request's own timeout, and no longer.
    #[tokio::test]
    async fn a_request_that_never_gets_an_answer_gives_up_at_its_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let mut open = Vec::new();
            while let Ok((conn, _)) = listener.accept().await {
                open.push(conn);
            }
        });
        let client = Client::new().unwrap();
        let started = Instant::now();
        let r = tokio::time::timeout(Duration::from_secs(5), client.get(&url, Duration::from_millis(300))).await;
        assert!(matches!(r, Ok(Err(_))), "it should fail on its own, at its timeout");
        assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
    }
}
