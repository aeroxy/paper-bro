//! arXiv: paper ids, and a client that keeps to arXiv's terms of use.
//!
//! The terms allow one request every three seconds over a single connection,
//! counted across every machine we run. paper-bro is one process, so one
//! mutex held for the whole request is the entire rate limiter.
//!
//! We fetch metadata (CC0) and the paper itself to read it. We never store or
//! serve the PDF or source; courses link to the abstract page.

use anyhow::{Context, Result, bail};
use regex::Regex;
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

/// The AERO alias for a paper's course: `arxiv-1706-03762`.
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
    pub license: Option<String>,
    pub published: Option<chrono::DateTime<chrono::Utc>>,
}

pub struct Client {
    http: reqwest::Client,
    last: Mutex<Option<Instant>>,
}

const GAP: Duration = Duration::from_secs(3);

/// What came back from a paced GET.
pub enum Fetched {
    Ok { url: String, body: Vec<u8> },
    NotFound,
}

impl Client {
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("paper-bro/0.1 (+https://aero.oops.wtf/paper-bro)")
            .timeout(Duration::from_secs(90))
            .build()?;
        Ok(Self { http, last: Mutex::new(None) })
    }

    /// GET with arXiv's pacing. The lock is held across the request, which
    /// is what makes it one connection at a time.
    async fn get(&self, url: &str) -> Result<Fetched> {
        let mut last = self.last.lock().await;
        if let Some(t) = *last {
            let wait = GAP.saturating_sub(t.elapsed());
            tokio::time::sleep(wait).await;
        }
        let result = async {
            let resp = self.http.get(url).send().await.with_context(|| format!("GET {url}"))?;
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

    /// Metadata from the export API, plus the license from OAI-PMH. `None`
    /// when arXiv has no such paper.
    pub async fn meta(&self, id: &str) -> Result<Option<Meta>> {
        let url = format!("https://export.arxiv.org/api/query?id_list={id}&max_results=1");
        let Fetched::Ok { body, .. } = self.get(&url).await? else { return Ok(None) };
        let Some(mut meta) = parse_atom(&String::from_utf8_lossy(&body), id)? else { return Ok(None) };
        let oai = format!(
            "https://oaipmh.arxiv.org/oai?verb=GetRecord&identifier=oai:arXiv.org:{id}&metadataPrefix=arXiv"
        );
        if let Fetched::Ok { body, .. } = self.get(&oai).await? {
            meta.license = parse_license(&String::from_utf8_lossy(&body));
        }
        Ok(Some(meta))
    }

    /// The LaTeXML rendering, with the URL it was served from (figure paths
    /// resolve against it). `None` when the paper has no HTML version.
    pub async fn html(&self, id: &str, version: u32) -> Result<Option<(String, String)>> {
        match self.get(&format!("https://arxiv.org/html/{id}v{version}")).await? {
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
        match self.get(&format!("https://arxiv.org/e-print/{id}v{version}")).await? {
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

fn parse_license(xml: &str) -> Option<String> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    doc.descendants().find(|n| n.has_tag_name("license")).and_then(|n| n.text()).map(squash)
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
    fn aliases_fit_aero() {
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
}
