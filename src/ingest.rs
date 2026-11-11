//! Turn a paper into the text the model reads.
//!
//! Preferred source: arXiv's LaTeXML rendering. It already has numbered
//! sections, math as LaTeX (`alttext`), and figures as PNGs with captions, so
//! it reduces to compact Markdown with `$…$` math. Attention Is All You Need
//! comes out at about 41K characters, roughly 12K tokens.
//!
//! Fallback: the LaTeX source tarball, read entirely in memory (nothing is
//! written to disk, so there is no path to traverse), with `\input`s inlined.
//! The model reads raw LaTeX fine.
//!
//! Last resort: a submission that is only a PDF is drawn page by page into
//! images (`render_pdf`), and `pipeline::paper_message` sends those to every
//! call. The images go as images, not as a PDF attachment: the model server
//! reads at most 20 pages of an attached PDF per request (measured: a 45-page
//! PDF showed pages 1 to 20, two PDFs in one request still showed 20 pages in
//! all, and no request option raised it), while it took 100 ordinary images in
//! one request. The drawing is `hayro`, in safe Rust, because a PDF is
//! untrusted input and this runs in the server that holds the keys. A PDF has
//! no figures a lesson can show.

use anyhow::{Result, bail};
use base64::Engine;
use base64::prelude::BASE64_STANDARD;
use ego_tree::NodeRef;
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{PixmapSettings, RenderCache, RenderSettings, render};
use regex::Regex;
use scraper::{Html, Node, Selector};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read;
use std::sync::LazyLock;

/// About 300K tokens. Longer papers are rejected rather than half-read.
pub const MAX_CHARS: usize = 1_000_000;
/// The largest PDF file taken. It bounds what is downloaded and held, not
/// what drawing it costs; the pipeline bounds the time that may take.
pub const MAX_PDF: usize = 25 * 1024 * 1024;
/// The most pages of a PDF taken. Like `MAX_CHARS`, a longer paper is
/// rejected rather than half-read. At about 1,300 tokens a page this is
/// about 80K tokens a call, cached after the first.
pub const MAX_PDF_PAGES: usize = 60;
/// How wide a drawn page is, in pixels: about 100 dpi for A4, and about 1,300
/// tokens a page for the model, whatever the paper size.
const PAGE_WIDE_PX: f32 = 833.0;
/// How tall a drawn page may be; a page of a stranger shape is shrunk to fit,
/// so it cannot cost, or weigh, an absurd amount.
const PAGE_HIGH_PX: f32 = 1400.0;
/// The most the drawn pages may weigh together as PNG, since they ride in
/// every call of a build. Tried up to this: 60 dense pages, a 28 MB request.
const MAX_PDF_IMAGE_BYTES: usize = 20 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Figure {
    /// `F1`, `F2`, … in document order; what prompts and lessons refer to.
    pub key: String,
    pub url: String,
    pub caption: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Paper {
    /// `html`, `latex` or `pdf`.
    pub kind: String,
    /// Empty for a PDF.
    pub text: String,
    pub figures: Vec<Figure>,
    /// The pages, in order, as base64 PNG images, when `kind` is `pdf`. A
    /// paper stored before PDFs were read has none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pages: Vec<String>,
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
    Ok(Paper { kind: "html".into(), text, figures: w.figures, pages: Vec::new() })
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
    /// Appends `node` as the text the model reads: headings as markdown,
    /// math as `$…$` LaTeX, figures as `[F1]` markers with their captions,
    /// and nothing from navigation, references or footnotes.
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
                    let tex = COLOR_MODEL.replace_all(e.attr("alttext").unwrap_or(""), " ");
                    let tex = tex.trim();
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
/// LaTeXML hands over each formula as the paper's own LaTeX, and xcolor's
/// colour with a model, `\color[rgb]{0,0.5,0.5}`, is beyond KaTeX, which then
/// shows the whole formula as an error (2610.15267's figure captions did). The
/// colour goes and the math stays; `\color{name}`, which KaTeX reads, is kept.
/// It becomes a space, not nothing: math ignores spaces, and nothing would
/// join `\sin\color[rgb]{1,0,0}h` into `\sinh`.
static COLOR_MODEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\\(?:text)?color\[[^\]]*\]\{[^}]*\}").unwrap());
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

/// Draw every page of a PDF to a PNG, base64, in order, each `PAGE_WIDE_PX`
/// wide (shorter if that would pass `PAGE_HIGH_PX` tall). A PDF that can't be
/// read, has too many pages, or draws to too much data is `Unsupported`. The
/// file is untrusted, so a panic while drawing it is caught and told the same
/// way, and the pipeline calls this off its async threads, with a time limit.
fn render_pdf(bytes: &[u8]) -> Result<Vec<String>> {
    let cant = |why: &str| Unsupported(format!("this paper was submitted as a PDF only, and {why}, which paper-bro can't read yet"));
    std::panic::catch_unwind(|| -> Result<Vec<String>> {
        let pdf = Pdf::new(bytes.to_vec()).map_err(|_| cant("its PDF is damaged or protected"))?;
        let pages = pdf.pages();
        if pages.len() == 0 {
            return Err(cant("its PDF has no pages").into());
        }
        if pages.len() > MAX_PDF_PAGES {
            return Err(cant(&format!("its PDF has more than {MAX_PDF_PAGES} pages")).into());
        }
        let cache = RenderCache::new();
        let (settings, render_settings) = (InterpreterSettings::default(), RenderSettings::default());
        let (mut out, mut weight) = (Vec::with_capacity(pages.len()), 0usize);
        for page in pages.iter() {
            let (w, h) = page.render_dimensions();
            if !(w.is_finite() && h.is_finite() && w >= 1.0 && h >= 1.0) {
                return Err(cant("a page of its PDF has no size").into());
            }
            let scale = (PAGE_WIDE_PX / w).min(PAGE_HIGH_PX / h);
            let pixmap = render(page, &cache, &settings, &render_settings, &PixmapSettings { x_scale: scale, y_scale: scale, bg_color: WHITE });
            let png = pixmap.into_png().map_err(|_| cant("a page of its PDF could not be drawn"))?;
            weight += png.len();
            if weight > MAX_PDF_IMAGE_BYTES {
                return Err(cant("its pages come to too much data").into());
            }
            out.push(BASE64_STANDARD.encode(png));
        }
        Ok(out)
    })
    .unwrap_or_else(|_| Err(cant("its PDF could not be drawn").into()))
}

pub fn from_eprint(bytes: &[u8]) -> Result<Paper> {
    if bytes.starts_with(b"%PDF") {
        if bytes.len() > MAX_PDF {
            return Err(Unsupported(format!(
                "this paper was submitted as a PDF only, and its PDF is over {} MB, which paper-bro can't read yet",
                MAX_PDF / (1024 * 1024)
            ))
            .into());
        }
        return Ok(Paper { kind: "pdf".into(), text: String::new(), figures: Vec::new(), pages: render_pdf(bytes)? });
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
    Ok(Paper { kind: "latex".into(), text, figures: Vec::new(), pages: Vec::new() })
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

    /// 2610.15267's captions: KaTeX can't read xcolor's `\color[rgb]{…}`, and
    /// removing it must not join two control words into another one.
    #[test]
    fn a_colour_katex_cannot_read_is_dropped_and_the_math_kept() {
        let html = r#"<html><body><article class="ltx_document"><h1 class="ltx_title ltx_title_document">T</h1>
          <figure class="ltx_figure"><img src="2610.15267v1/x1.png"><figcaption>Figure 1: The optimum
          <math alttext="\omega^{\color[rgb]{0,0.5,0.5}*}" display="inline"></math> and
          <math alttext="\textcolor[RGB]{0,128,128}{\theta}^{\color{teal}*}" display="inline"></math>.</figcaption></figure>
          <p>padding </p></article></body></html>"#;
        let html = html.replace("padding ", &"filler text ".repeat(200));
        let p = from_latexml("https://arxiv.org/html/2610.15267v1", &html).unwrap();
        assert!(p.figures[0].caption.contains(r"$\omega^{ *}$"), "{}", p.figures[0].caption);
        assert!(p.figures[0].caption.contains(r"${\theta}^{\color{teal}*}$"), "{}", p.figures[0].caption);
        assert!(!p.figures[0].caption.contains("color["), "{}", p.figures[0].caption);
        let joined = html.replace(r"\omega^{\color[rgb]{0,0.5,0.5}*}", r"\sin\color[rgb]{1,0,0}h(x)");
        let p = from_latexml("https://arxiv.org/html/2610.15267v1", &joined).unwrap();
        assert!(p.figures[0].caption.contains(r"$\sin h(x)$"), "\\sin and h stay apart: {}", p.figures[0].caption);
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

    /// A valid PDF of `sizes.len()` pages, each `(width, height)` in points; the
    /// first page has a black square on it, the others are blank.
    fn pdf_of(sizes: &[(u32, u32)]) -> Vec<u8> {
        let n = sizes.len();
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut at = vec![];
        let mut obj = |out: &mut Vec<u8>, body: String| {
            at.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", at.len()).as_bytes());
        };
        obj(&mut out, "<< /Type /Catalog /Pages 2 0 R >>".into());
        let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 3 + 2 * i)).collect();
        obj(&mut out, format!("<< /Type /Pages /Kids [{}] /Count {n} >>", kids.join(" ")));
        for (i, (w, h)) in sizes.iter().enumerate() {
            obj(&mut out, format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] /Contents {} 0 R >>", 4 + 2 * i));
            let ink = if i == 0 { "0 0 0 rg 10 10 40 40 re f" } else { "" };
            obj(&mut out, format!("<< /Length {} >>\nstream\n{ink}\nendstream", ink.len() + 1));
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", at.len() + 1).as_bytes());
        for offset in &at {
            out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", at.len() + 1).as_bytes());
        out
    }

    /// Width and height of a base64 PNG, from its header.
    fn png_size(b64: &str) -> (u32, u32) {
        let png = BASE64_STANDARD.decode(b64).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "a PNG");
        (u32::from_be_bytes(png[16..20].try_into().unwrap()), u32::from_be_bytes(png[20..24].try_into().unwrap()))
    }

    #[test]
    fn a_pdf_only_paper_is_drawn_one_image_per_page() {
        let p = from_eprint(&pdf_of(&[(612, 792), (612, 792), (612, 792)])).unwrap();
        assert_eq!((p.kind.as_str(), p.text.as_str(), p.figures.len(), p.pages.len()), ("pdf", "", 0, 3));
        // Every page is the same width, whatever the paper size, so costs the same.
        for page in &p.pages {
            assert_eq!(png_size(page), (833, 1078));
        }
        assert_ne!(p.pages[0], p.pages[1], "the first page has ink on it, the second does not");
        assert_eq!(p.pages[1], p.pages[2], "blank pages draw alike");
    }

    #[test]
    fn a_page_of_another_shape_is_scaled_to_the_same_width_and_a_very_tall_one_to_fit() {
        let p = from_eprint(&pdf_of(&[(595, 842), (300, 3000)])).unwrap();
        assert_eq!(png_size(&p.pages[0]), (833, 1178), "A4");
        let (w, h) = png_size(&p.pages[1]);
        assert!(h <= 1400 && w < 833, "a tall page is shrunk to fit: {w}x{h}");
    }

    #[test]
    fn a_pdf_over_the_page_limit_is_unsupported_before_any_page_is_drawn() {
        let e = from_eprint(&pdf_of(&vec![(612, 792); MAX_PDF_PAGES + 1])).unwrap_err();
        let u = e.downcast_ref::<Unsupported>().expect("rejected, not a failure worth retrying");
        assert!(u.0.contains("more than 60 pages"), "{}", u.0);
    }

    #[test]
    fn a_pdf_over_the_size_limit_is_unsupported_and_names_the_limit() {
        let mut big = b"%PDF-1.5 ".to_vec();
        big.resize(MAX_PDF + 1, b' ');
        let e = from_eprint(&big).unwrap_err();
        let u = e.downcast_ref::<Unsupported>().expect("an unsupported paper, not a failure worth retrying");
        assert!(u.0.contains("over 25 MB"), "{}", u.0);
    }

    #[test]
    fn a_damaged_pdf_is_unsupported_and_does_not_crash_the_server() {
        for junk in [&b"%PDF-1.5 ..."[..], &b"%PDF-1.7\n1 0 obj << /Type /Catalog >>"[..], &b"%PDF"[..]] {
            let e = from_eprint(junk).unwrap_err();
            assert!(e.downcast_ref::<Unsupported>().is_some(), "{e:#}");
        }
    }

    #[test]
    fn a_paper_stored_before_pdfs_were_read_still_loads() {
        let p: Paper = serde_json::from_str(r#"{"kind":"latex","text":"x","figures":[]}"#).unwrap();
        assert!(p.pages.is_empty());
        // And a text paper doesn't gain a `pages` key when stored again.
        assert!(!serde_json::to_string(&p).unwrap().contains("pages"));
    }
}
