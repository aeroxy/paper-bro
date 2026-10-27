//! Turn a paper into the text the model reads.
//!
//! Preferred source: arXiv's LaTeXML rendering. It already has numbered
//! sections, math as LaTeX (`alttext`), and figures as PNGs with captions, so
//! it reduces to compact Markdown with `$…$` math. Attention Is All You Need
//! comes out at about 41K characters, roughly 12K tokens.
//!
//! Fallback: the LaTeX source tarball, read entirely in memory (nothing is
//! written to disk, so there is no path to traverse), with `\input`s inlined.
//! The model reads raw LaTeX fine. PDF-only submissions are not supported.

use anyhow::{Result, bail};
use ego_tree::NodeRef;
use regex::Regex;
use scraper::{Html, Node, Selector};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read;
use std::sync::LazyLock;

/// About 300K tokens. Longer papers are rejected rather than half-read.
pub const MAX_CHARS: usize = 1_000_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Figure {
    /// `F1`, `F2`, … in document order; what prompts and lessons refer to.
    pub key: String,
    pub url: String,
    pub caption: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Paper {
    /// `html` or `latex`.
    pub kind: String,
    pub text: String,
    pub figures: Vec<Figure>,
}

/// A paper we can't read, as opposed to a failure worth retrying.
#[derive(Debug)]
pub struct Unsupported(pub String);
impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Unsupported {}

pub fn from_latexml(page_url: &str, html: &str) -> Result<Paper> {
    let base = reqwest::Url::parse(page_url)?;
    let doc = Html::parse_document(html);
    let root = Selector::parse("article.ltx_document").unwrap();
    let Some(article) = doc.select(&root).next() else { bail!("no LaTeXML article in {page_url}") };
    let mut w = Walker { out: String::new(), figures: Vec::new(), base };
    for child in article.children() {
        w.walk(child);
    }
    let text = tidy(&w.out);
    check_size(&text)?;
    Ok(Paper { kind: "html".into(), text, figures: w.figures })
}

struct Walker {
    out: String,
    figures: Vec<Figure>,
    base: reqwest::Url,
}

const SKIP_CLASSES: &[&str] = &[
    "ltx_bibliography",
    "ltx_page_footer",
    "ltx_TOC",
    "ltx_note", // footnotes: noise for a course, and they interrupt sentences
    "ltx_authors",
    "ltx_dates",
    "ltx_pagination",
];

impl Walker {
    fn walk(&mut self, node: NodeRef<Node>) {
        match node.value() {
            Node::Text(t) => self.out.push_str(t),
            Node::Element(e) => {
                let name = e.name();
                if matches!(name, "script" | "style" | "nav" | "header" | "footer" | "button" | "noscript")
                    || e.classes().any(|c| SKIP_CLASSES.contains(&c))
                {
                    return;
                }
                if name == "math" {
                    let tex = e.attr("alttext").unwrap_or("").trim();
                    if e.attr("display") == Some("block") {
                        self.out.push_str(&format!("\n$$ {tex} $$\n"));
                    } else {
                        self.out.push_str(&format!(" ${tex}$ "));
                    }
                    return;
                }
                if name == "figure" && e.classes().any(|c| c == "ltx_figure") {
                    self.figure(node);
                    return;
                }
                let heading = match name {
                    "h1" => Some(1),
                    "h2" => Some(2),
                    "h3" => Some(3),
                    "h4" | "h5" | "h6" => Some(4),
                    _ => None,
                };
                if let Some(level) = heading {
                    self.out.push_str(&format!("\n\n{} ", "#".repeat(level)));
                    self.children(node);
                    self.out.push_str("\n\n");
                    return;
                }
                match name {
                    "p" | "div" | "section" | "table" | "blockquote" | "figcaption" | "tr" => {
                        self.out.push('\n');
                        self.children(node);
                        self.out.push('\n');
                    }
                    "li" => {
                        self.out.push_str("\n- ");
                        self.children(node);
                    }
                    "td" | "th" => {
                        self.children(node);
                        self.out.push_str(" | ");
                    }
                    "br" => self.out.push('\n'),
                    _ => self.children(node),
                }
            }
            _ => {}
        }
    }

    fn children(&mut self, node: NodeRef<Node>) {
        for c in node.children() {
            self.walk(c);
        }
    }

    fn figure(&mut self, node: NodeRef<Node>) {
        let mut caption = Walker { out: String::new(), figures: Vec::new(), base: self.base.clone() };
        let mut imgs = Vec::new();
        for d in node.descendants() {
            if let Node::Element(e) = d.value() {
                if e.name() == "figcaption" && caption.out.is_empty() {
                    caption.children(d);
                }
                if e.name() == "img" {
                    // Only plain arXiv-hosted files can be embedded; a figure
                    // LaTeXML couldn't convert has no image and stays text.
                    if let Some(url) = e.attr("src").and_then(|s| self.base.join(s).ok()) {
                        if url.as_str().starts_with("https://arxiv.org/html/") {
                            imgs.push(url.to_string());
                        }
                    }
                }
            }
        }
        let caption = tidy(&caption.out).replace('\n', " ");
        if imgs.is_empty() {
            self.out.push_str(&format!("\n{caption}\n"));
            return;
        }
        // A figure with several panels gets F2a, F2b, …
        let n = self.figures.iter().map(|f| f.key.trim_end_matches(char::is_alphabetic)).collect::<std::collections::HashSet<_>>().len() + 1;
        let keys: Vec<String> = if imgs.len() == 1 {
            vec![format!("F{n}")]
        } else {
            (0..imgs.len()).map(|i| format!("F{n}{}", (b'a' + i as u8) as char)).collect()
        };
        self.out.push_str(&format!("\n[{}] {caption}\n", keys.join(", ")));
        for (key, url) in keys.into_iter().zip(imgs) {
            self.figures.push(Figure { key, url, caption: caption.clone() });
        }
    }
}

static SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t\u{a0}]+").unwrap());
static BLANK_LINES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n\s*\n(\s*\n)+").unwrap());

fn tidy(s: &str) -> String {
    let s = SPACES.replace_all(s, " ");
    let lines: Vec<&str> = s.lines().map(str::trim).collect();
    BLANK_LINES.replace_all(&lines.join("\n"), "\n\n").trim().to_string()
}

fn check_size(text: &str) -> Result<()> {
    if text.len() > MAX_CHARS {
        return Err(Unsupported(format!(
            "this paper is too long for paper-bro ({}K characters; the limit is {}K)",
            text.len() / 1000,
            MAX_CHARS / 1000
        ))
        .into());
    }
    if text.len() < 2_000 {
        bail!("only {} characters of text came out of the paper", text.len());
    }
    Ok(())
}

const MAX_UNPACKED: u64 = 50 * 1024 * 1024;
const MAX_ENTRIES: usize = 2_000;

pub fn from_eprint(bytes: &[u8]) -> Result<Paper> {
    if bytes.starts_with(b"%PDF") {
        return Err(Unsupported("this paper was submitted as a PDF only, which paper-bro can't read yet".into()).into());
    }
    let raw = if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(bytes).take(MAX_UNPACKED + 1).read_to_end(&mut out)?;
        if out.len() as u64 > MAX_UNPACKED {
            return Err(Unsupported("the paper's source unpacks to more than 50 MB".into()).into());
        }
        out
    } else {
        bytes.to_vec()
    };

    let is_tar = raw.len() > 262 && &raw[257..262] == b"ustar";
    let mut files: HashMap<String, String> = HashMap::new();
    if is_tar {
        let mut archive = tar::Archive::new(raw.as_slice());
        for (i, entry) in archive.entries()?.enumerate() {
            if i >= MAX_ENTRIES {
                return Err(Unsupported("the paper's source has too many files".into()).into());
            }
            let mut entry = entry?;
            if !entry.header().entry_type().is_file() {
                continue; // symlinks, hard links, devices, directories
            }
            let path = entry.path()?.to_string_lossy().replace('\\', "/");
            if path.starts_with('/') || path.split('/').any(|p| p == "..") || !path.ends_with(".tex") {
                continue;
            }
            let mut text = String::new();
            entry.read_to_string(&mut text).ok();
            files.insert(path.trim_start_matches("./").to_string(), text);
        }
    } else {
        // A single-file submission, often plain TeX (harvmac) with no
        // \documentclass. Accept anything that reads like TeX.
        let text = String::from_utf8_lossy(&raw).into_owned();
        if TEX_COMMAND.find_iter(&text).take(20).count() < 20 {
            return Err(Unsupported("paper-bro couldn't find LaTeX in this paper's source".into()).into());
        }
        files.insert("main.tex".into(), text);
    }

    // The main file is the biggest one that begins a LaTeX document, or for
    // plain TeX, the biggest file.
    let largest = |latex_only: bool| {
        files
            .iter()
            .filter(|(_, t)| !latex_only || t.contains("\\begin{document}"))
            .max_by_key(|(_, t)| t.len())
            .map(|(p, _)| p.clone())
    };
    let Some(main) = largest(true).or_else(|| largest(false)) else {
        return Err(Unsupported("paper-bro couldn't find the main LaTeX file".into()).into());
    };
    let text = tidy(&inline(&files, &main, 0));
    check_size(&text)?;
    Ok(Paper { kind: "latex".into(), text, figures: Vec::new() })
}

static TEX_COMMAND: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\\[a-zA-Z]{2,}").unwrap());
static INPUT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\\(?:input|include)\s*\{([^}]+)\}").unwrap());
static COMMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)(^|[^\\])%.*$").unwrap());

fn inline(files: &HashMap<String, String>, path: &str, depth: usize) -> String {
    let Some(src) = files.get(path) else { return String::new() };
    let src = COMMENT.replace_all(src, "$1");
    if depth >= 8 {
        return src.into_owned();
    }
    let dir = path.rsplit_once('/').map(|(d, _)| format!("{d}/")).unwrap_or_default();
    INPUT
        .replace_all(&src, |c: &regex::Captures| {
            let name = c[1].trim().trim_start_matches("./");
            let with_ext = if name.ends_with(".tex") { name.to_string() } else { format!("{name}.tex") };
            [format!("{dir}{with_ext}"), with_ext]
                .iter()
                .find(|p| files.contains_key(p.as_str()) && p.as_str() != path)
                .map(|p| inline(files, p, depth + 1))
                .unwrap_or_default()
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latexml_math_sections_and_figures() {
        let html = r#"<html><body><nav>skip</nav><article class="ltx_document">
          <h1 class="ltx_title ltx_title_document">Attention</h1>
          <section class="ltx_section"><h2 class="ltx_title">3 Model</h2>
          <div class="ltx_para"><p class="ltx_p">Keys of dimension <math alttext="d_{k}" display="inline"><mi>d</mi></math> are scaled.</p></div>
          <table class="ltx_equation"><tr><td><math alttext="\mathrm{softmax}(QK^T)" display="block"></math></td><td>(1)</td></tr></table>
          <figure class="ltx_figure"><img src="1706.03762v7/Figures/a.png"><figcaption>Figure 1: The Transformer.</figcaption></figure>
          <span class="ltx_note ltx_role_footnote">a footnote</span>
          </section><section class="ltx_bibliography">refs</section>
          <p>padding </p></article></body></html>"#;
        let html = html.replace("padding ", &"filler text ".repeat(200));
        let p = from_latexml("https://arxiv.org/html/1706.03762v7", &html).unwrap();
        assert!(p.text.contains("# Attention"));
        assert!(p.text.contains("## 3 Model"));
        assert!(p.text.contains("$d_{k}$"));
        assert!(p.text.contains("$$ \\mathrm{softmax}(QK^T) $$"));
        assert!(p.text.contains("[F1] Figure 1: The Transformer."));
        assert!(!p.text.contains("footnote") && !p.text.contains("refs") && !p.text.contains("skip"));
        assert_eq!(p.figures[0].url, "https://arxiv.org/html/1706.03762v7/Figures/a.png");
    }

    fn tarball(entries: &[(&str, &[u8])], symlink: Option<(&str, &str)>) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (path, data) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(0o644);
            h.set_cksum();
            b.append_data(&mut h, path, *data).unwrap();
        }
        if let Some((name, target)) = symlink {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_size(0);
            b.append_link(&mut h, name, target).unwrap();
        }
        let tar = b.into_inner().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, &tar).unwrap();
        gz.finish().unwrap()
    }

    #[test]
    fn tarball_inlines_inputs_and_ignores_unsafe_entries() {
        let body = "x".repeat(3000);
        let main = format!("\\documentclass{{article}}\n\\begin{{document}}\nIntro % a comment\n\\input{{sec/model}}\n{body}\n\\end{{document}}");
        let gz = tarball(
            &[("ms.tex", main.as_bytes()), ("sec/model.tex", b"Model section 50\\% better"), ("secret.txt", b"nope")],
            Some(("evil.tex", "/etc/passwd")),
        );
        let p = from_eprint(&gz).unwrap();
        assert!(p.text.contains("Intro"));
        assert!(!p.text.contains("a comment"));
        assert!(p.text.contains("Model section 50\\% better"));
        assert!(!p.text.contains("nope"));
    }

    #[test]
    fn pdf_only_is_unsupported() {
        let e = from_eprint(b"%PDF-1.5 ...").unwrap_err();
        assert!(e.downcast_ref::<Unsupported>().is_some());
    }
}
