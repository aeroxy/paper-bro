//! The course: what the model plans and writes, and how paper-bro assembles
//! it into one self-contained page.
//!
//! The split matters for safety. A course is served from its host's own
//! origin, next to whatever else that host runs, and the model wrote it from
//! untrusted paper text. So the page's only scripts
//! are the shell's own (shell/runtime.js); model prose goes through ammonia;
//! model text in the hero, quiz and glossary is escaped or set as
//! textContent; and every lab runs in a sandboxed iframe with an opaque
//! origin. The CSP <meta> is a second layer, not the boundary.

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::LazyLock;

use crate::arxiv::{self, Meta};
use crate::ingest::Figure;

pub const KATEX: &str = "https://cdn.jsdelivr.net/npm/katex@0.16.47/dist";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Outline {
    pub course_title: String,
    pub tagline: String,
    pub lede: String,
    pub minutes: u32,
    pub chips: Vec<String>,
    pub lessons: Vec<LessonPlan>,
    pub quiz: Vec<QuizItem>,
    pub glossary: Vec<Term>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LessonPlan {
    pub title: String,
    pub nav_title: String,
    pub eyebrow: String,
    pub minutes: u32,
    pub objectives: Vec<String>,
    pub paper_sections: Vec<String>,
    pub key_ideas: Vec<String>,
    pub figure: String,
    pub lab: LabPlan,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LabPlan {
    pub title: String,
    pub idea: String,
    pub controls: String,
    pub what_it_shows: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QuizItem {
    pub question: String,
    pub options: Vec<String>,
    pub answer: u32,
    pub explanation: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Term {
    pub term: String,
    pub definition: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Lesson {
    pub prose_html: String,
    pub figure: String,
    pub figure_note: String,
    pub lab_html: String,
    pub lab_css: String,
    pub lab_js: String,
    /// Set when QA gave up on the lab: the lesson ships without it.
    #[serde(default)]
    pub lab_dropped: bool,
}

pub fn outline_schema() -> Value {
    let s = |d: &str| json!({"type": "string", "description": d});
    let strings = |d: &str| json!({"type": "array", "items": {"type": "string"}, "description": d});
    crate::llm::strict(json!({
        "type": "object",
        "properties": {
            "course_title": s("Short course name, 2 to 5 words, e.g. 'Attention, Explained'. Not the paper's title; no colon or dash."),
            "tagline": s("The big hero headline, 4-9 words, may contain one line break as \\n."),
            "lede": s("2-3 sentences: why this paper matters and what the reader will build in their head."),
            "minutes": {"type": "integer", "description": "Total minutes for the whole course."},
            "chips": strings("3-5 short facts for hero chips, each starting with an emoji."),
            "lessons": {"type": "array", "description": "6 to 10 lessons, in teaching order.", "items": {
                "type": "object",
                "properties": {
                    "title": s("Lesson headline, ending with a full stop."),
                    "nav_title": s("2-5 word version for the sidebar."),
                    "eyebrow": s("2-3 word label, e.g. 'The big idea'."),
                    "minutes": {"type": "integer"},
                    "objectives": strings("What the reader can do or explain afterwards."),
                    "paper_sections": strings("Which parts of the paper this lesson draws on."),
                    "key_ideas": strings("The specific ideas, numbers and claims from the paper to teach."),
                    "figure": s("Key of a paper figure to show in this lesson (e.g. 'F1'), or ''."),
                    "lab": {"type": "object", "properties": {
                        "title": s("Lab title."),
                        "idea": s("What the reader plays with, concretely."),
                        "controls": s("The sliders, buttons or selects and what each changes."),
                        "what_it_shows": s("The insight the reader should get from playing."),
                    }},
                },
            }},
            "quiz": {"type": "array", "description": "8 questions over the whole course.", "items": {
                "type": "object",
                "properties": {
                    "question": s("The question."),
                    "options": strings("Exactly 4 options."),
                    "answer": {"type": "integer", "description": "Index (0-3) of the correct option."},
                    "explanation": s("One or two sentences: why the answer is right."),
                },
            }},
            "glossary": {"type": "array", "description": "8-16 terms the course uses.", "items": {
                "type": "object",
                "properties": {"term": s("Term."), "definition": s("Plain-words definition.")},
            }},
        },
    }))
}

pub fn lesson_schema() -> Value {
    let s = |d: &str| json!({"type": "string", "description": d});
    crate::llm::strict(json!({
        "type": "object",
        "properties": {
            "prose_html": s("The lesson's explanation as an HTML fragment (see the contract)."),
            "figure": s("The figure key planned for this lesson, or '' for none."),
            "figure_note": s("If figure is set: one sentence telling the reader what to look at in it. Otherwise ''."),
            "lab_html": s("The lab's markup (see the contract). Never empty."),
            "lab_css": s("Extra CSS for the lab only, or ''."),
            "lab_js": s("The lab's script (see the contract)."),
        },
    }))
}

pub fn review_schema() -> Value {
    crate::llm::strict(json!({
        "type": "object",
        "properties": {
            "lessons": {"type": "array", "items": {"type": "object", "properties": {
                "lesson": {"type": "integer", "description": "Lesson number, from 1."},
                "verdict": {"type": "string", "enum": ["pass", "fix"]},
                "findings": {"type": "array", "items": {"type": "string"},
                    "description": "Specific problems to fix. Empty when the verdict is pass."},
            }}},
            "quiz_findings": {"type": "array", "items": {"type": "string"},
                "description": "Quiz questions that are wrong or ambiguous, with the fix. Empty if none."},
        },
    }))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Review {
    pub lessons: Vec<LessonReview>,
    pub quiz_findings: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LessonReview {
    pub lesson: u32,
    pub verdict: String,
    pub findings: Vec<String>,
}

impl Outline {
    /// Reject an outline the shell can't render well.
    pub fn validate(&self) -> Result<(), String> {
        let n = self.lessons.len();
        if !(4..=12).contains(&n) {
            return Err(format!("{n} lessons; plan 6 to 10"));
        }
        for (i, q) in self.quiz.iter().enumerate() {
            if q.options.len() != 4 {
                return Err(format!("quiz question {} has {} options, not 4", i + 1, q.options.len()));
            }
            if q.answer > 3 {
                return Err(format!("quiz question {} has answer index {}", i + 1, q.answer));
            }
        }
        if self.quiz.len() < 4 {
            return Err(format!("only {} quiz questions; write 8", self.quiz.len()));
        }
        Ok(())
    }
}

pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Model prose, cut down to an allowlist: no scripts, no event handlers, no
/// styles, no images (figures are placed by the assembler), https links only.
/// Any other link (`#`, a path, `//host`) loses its address and is unwrapped to
/// plain text: the page does its own navigation.
pub fn sanitize(html: &str) -> String {
    let mut b = ammonia::Builder::empty();
    b.add_tags([
        "p", "h3", "h4", "strong", "em", "b", "i", "code", "pre", "ul", "ol", "li", "blockquote", "br",
        "hr", "span", "div", "table", "thead", "tbody", "tr", "th", "td", "sup", "sub", "small", "a",
    ])
    .add_tag_attributes("a", ["href"])
    .url_schemes(["https"].into())
    .url_relative(ammonia::UrlRelative::Deny)
    .link_rel(Some("noopener noreferrer"))
    .add_allowed_classes("div", ["card", "flat", "cols", "cols3", "callout", "deeper", "formula", "emo"])
    .add_allowed_classes("span", ["mono", "num", "chip"])
    .add_allowed_classes("p", ["lede"])
    .add_allowed_classes("ol", ["steps"])
    .add_allowed_classes("ul", ["steps"]);
    BARE_LINK.replace_all(&b.clean(html).to_string(), "$1").into_owned()
}

/// An `<a>` the sanitiser left without an address.
static BARE_LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?s)<a rel="noopener noreferrer">(.*?)</a>"#).unwrap());

static CLOSE_SCRIPT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)</(script)").unwrap());
static CLOSE_STYLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)</style").unwrap());

/// JSON safe to embed in a <script type="application/json"> block.
fn embed_json(v: &Value) -> String {
    serde_json::to_string(v).unwrap().replace('<', "\\u003c")
}

pub struct Course<'a> {
    pub meta: &'a Meta,
    pub outline: &'a Outline,
    pub lessons: &'a [Lesson],
    pub figures: &'a [Figure],
    pub model: &'a str,
    pub built: &'a str,
}

fn license_name(url: &str) -> String {
    let u = url.to_ascii_lowercase();
    if u.contains("nonexclusive-distrib") {
        "arXiv non-exclusive license".into()
    } else if u.contains("publicdomain/zero") {
        "CC0".into()
    } else if let Some(rest) = u.split("creativecommons.org/licenses/").nth(1) {
        let parts: Vec<&str> = rest.trim_end_matches('/').split('/').collect();
        format!("CC {} {}", parts.first().unwrap_or(&"").to_uppercase(), parts.get(1).unwrap_or(&""))
    } else {
        url.into()
    }
}

/// `acme/tiny-model-0425` → `Tiny Model`: the model's name
/// for readers, without the provider path or a build suffix.
fn model_name(slug: &str) -> String {
    let base = slug.rsplit('/').next().unwrap_or(slug);
    let base = match base.rsplit_once('-') {
        Some((head, tail)) if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) => head,
        _ => base,
    };
    base.split('-')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut cs = w.chars();
            cs.next().map(|f| f.to_uppercase().chain(cs).collect::<String>()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn authors_line(authors: &[String]) -> String {
    match authors.len() {
        0 => "Unknown authors".into(),
        1..=4 => authors.join(", "),
        _ => format!("{}, {} and others", authors[0], authors[1]),
    }
}

/// AERO adds its own Google Analytics tag to every page it serves, right
/// after `<head>` and so before this policy. The loader starts before the
/// policy applies, but the hits it sends later don't, so GA's hosts are
/// allowed here or every course would drop out of analytics silently.
/// Courses carry no GA tag of their own.
fn csp(origin: &str) -> String {
    let (ga_script, ga_img, ga_connect) = (
        " https://*.googletagmanager.com",
        " https://*.google-analytics.com https://*.googletagmanager.com",
        " https://*.google-analytics.com https://*.analytics.google.com https://*.googletagmanager.com",
    );
    format!(
        "default-src 'none'; script-src 'unsafe-inline' https://cdn.jsdelivr.net/npm/katex@0.16.47/{ga_script}; \
         style-src 'unsafe-inline' https://cdn.jsdelivr.net/npm/; font-src https://cdn.jsdelivr.net/npm/; \
         img-src data: blob: https://arxiv.org/html/{ga_img}; \
         connect-src {origin}/paper-bro/api/lab-errors{ga_connect}; \
         frame-src 'self'; base-uri 'none'; form-action 'none'; object-src 'none'"
    )
}

const LOGO: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" style="display:none" aria-hidden="true"><symbol id="logo" viewBox="0 0 32 32"><rect width="32" height="32" rx="8" fill="#0B0B0B"/><path d="M10 8h9l4 4v12H10z" fill="none" stroke="#fff" stroke-width="1.6" stroke-linejoin="round" opacity=".85"/><path d="M13 15h7M13 18.5h7" stroke="#fff" stroke-width="1.4" stroke-linecap="round" opacity=".55"/><circle cx="22.5" cy="22.5" r="4.2" fill="#D85A30"/></symbol></svg>"##;

const FONTS: &str = r#"<link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/@fontsource/inter@5/index.min.css"><link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/@fontsource/inter@5/700.min.css"><link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/@fontsource/inter@5/800.min.css"><link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/@fontsource/jetbrains-mono@5/index.min.css">"#;

fn head(c: &Course, origin: &str, title: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta http-equiv="Content-Security-Policy" content="{csp}">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<meta name="description" content="{desc}">
<meta name="paper-bro" content="{id}">
{FONTS}
<link rel="stylesheet" href="{KATEX}/katex.min.css">
<script defer src="{KATEX}/katex.min.js"></script>
<script defer src="{KATEX}/contrib/auto-render.min.js"></script>
<style>{css}</style>
</head>
"#,
        csp = csp(origin),
        desc = esc(&c.outline.lede),
        id = esc(&c.meta.id),
        css = include_str!("../shell/course.css"),
    )
}

/// The data block the runtime reads: labs, quiz, and where to report errors.
fn data(c: &Course, origin: &str, only: Option<usize>) -> String {
    let mut labs = serde_json::Map::new();
    for (i, l) in c.lessons.iter().enumerate() {
        if l.lab_dropped || only.is_some_and(|o| o != i) {
            continue;
        }
        let js = CLOSE_SCRIPT.replace_all(&l.lab_js, "<\\/$1").into_owned();
        let tex = js.contains("PB.tex") || l.lab_html.contains("katex");
        labs.insert(
            format!("l{}", i + 1),
            json!({"html": l.lab_html, "css": CLOSE_STYLE.replace_all(&l.lab_css, ""), "js": js, "tex": tex}),
        );
    }
    let quiz: Vec<Value> = if only.is_some() {
        Vec::new()
    } else {
        c.outline
            .quiz
            .iter()
            .map(|q| json!({"question": q.question, "options": q.options, "answer": q.answer, "explanation": q.explanation}))
            .collect()
    };
    embed_json(&json!({
        "id": c.meta.id,
        "labs": labs,
        "quiz": quiz,
        "errorUrl": format!("{origin}/paper-bro/api/lab-errors"),
        "labHead": FONTS,
        "labTex": format!(r#"<link rel="stylesheet" href="{KATEX}/katex.min.css"><script src="{KATEX}/katex.min.js"></script>"#),
        "labCss": include_str!("../shell/labkit.css"),
        "labJs": include_str!("../shell/labkit.js"),
    }))
}

fn scripts(c: &Course, origin: &str, only: Option<usize>) -> String {
    format!(
        "<script type=\"application/json\" id=\"pb-data\">{}</script>\n<script>{}</script>\n</body>\n</html>\n",
        data(c, origin, only),
        include_str!("../shell/runtime.js")
    )
}

fn lab_card(c: &Course, i: usize) -> String {
    let plan = &c.outline.lessons[i];
    format!(
        r#"<div class="card lab"><div class="lab-head"><div><div class="eyebrow">Lab · {n:02}</div><div class="lab-title">{title}</div></div></div><iframe class="lab-frame" data-lab="l{n}" sandbox="allow-scripts" title="{title}"></iframe></div>"#,
        n = i + 1,
        title = esc(&plan.lab.title)
    )
}

/// The figures lessons refer to that `figures` doesn't have, as
/// "lesson N: KEY". `figure()` leaves a missing one out without a word.
pub fn missing_figures(lessons: &[Lesson], figures: &[Figure]) -> Vec<String> {
    lessons
        .iter()
        .enumerate()
        .filter(|(_, l)| !l.figure.is_empty() && !figures.iter().any(|f| f.key == l.figure))
        .map(|(i, l)| format!("lesson {}: {}", i + 1, l.figure))
        .collect()
}

fn figure(c: &Course, lesson: &Lesson) -> String {
    let Some(f) = c.figures.iter().find(|f| !lesson.figure.is_empty() && f.key == lesson.figure) else {
        return String::new();
    };
    format!(
        r#"<figure class="paper-fig"><img src="{url}" alt="{cap}" loading="lazy"><figcaption>{note} <span>{cap} From “{title}”, shown from arXiv.</span></figcaption></figure>"#,
        url = esc(&f.url),
        cap = esc(&f.caption),
        note = esc(&lesson.figure_note),
        title = esc(&c.meta.title),
    )
}

/// The whole course, as served from `origin`.
pub fn assemble(c: &Course, origin: &str) -> String {
    let o = c.outline;
    let id = &c.meta.id;
    let abs = arxiv::abs_url(id);
    let n = o.lessons.len();
    let mut h = head(c, origin, &format!("{} — an interactive course on arXiv:{id}", esc(&o.course_title)));
    let nav: String = o
        .lessons
        .iter()
        .enumerate()
        .map(|(i, l)| format!(r##"<a href="#l{n}"><span class="n">{n:02}</span>{t}<span class="ck">✓</span></a>"##, n = i + 1, t = esc(&l.nav_title)))
        .chain(std::iter::once(format!(r##"<a href="#lq"><span class="n">{:02}</span>Final quiz<span class="ck">✓</span></a>"##, n + 1)))
        .collect();
    let chips: String = o.chips.iter().map(|c| format!(r#"<span class="chip">{}</span>"#, esc(c))).collect();
    let tagline = esc(&o.tagline).replace("\\n", "<br>").replace('\n', "<br>");
    let year = c.meta.published.map(|d| d.format("%Y").to_string()).unwrap_or_default();
    h.push_str(&format!(
        r##"<body id="top">
{LOGO}
<div class="grid-bg"></div>
<header class="top"><div class="wrap">
  <a class="brand" href="#top"><svg class="logo"><use href="#logo"/></svg><span>{course}</span></a>
  <div class="prog"><div class="bar"><i id="progBar"></i></div><span id="progTxt">0 / {total} done</span></div>
</div></header>
<main class="wrap">
  <section class="hero">
    <div>
      <div class="eyebrow">Interactive course<span class="sep">·</span>{n} lessons<span class="sep">·</span>arXiv:{id}</div>
      <h1 class="display">{tagline}</h1>
      <p class="lede">{lede}</p>
      <div class="chips">{chips}<span class="chip live"><span class="dot"></span>~{minutes} minutes</span></div>
      <div class="cta-row"><a class="btn btn-accent" href="#l1">Start lesson 1 →</a><a class="btn btn-sec" href="#lq">Jump to the quiz</a></div>
    </div>
    <div class="inverted paper-card">
      <div class="eyebrow">The paper</div>
      <div class="t">{paper}</div>
      <div class="a">{authors}{year_sep}{year}</div>
      <p style="margin:18px 0 0;position:relative"><a href="{abs}">Read it on arXiv →</a></p>
    </div>
  </section>
  <div class="layout">
    <aside class="side"><div class="eyebrow">Lessons</div><nav id="sideNav">{nav}</nav></aside>
    <div class="content">
"##,
        course = esc(&o.course_title),
        total = n + 1,
        lede = esc(&o.lede),
        minutes = o.minutes,
        paper = esc(&c.meta.title),
        authors = esc(&authors_line(&c.meta.authors)),
        year_sep = if year.is_empty() { "" } else { " · " },
    ));
    for (i, (plan, lesson)) in o.lessons.iter().zip(c.lessons).enumerate() {
        let next = match o.lessons.get(i + 1) {
            Some(nx) => format!(r##"<a class="next" href="#l{}">Next: {} →</a>"##, i + 2, esc(&nx.nav_title)),
            None => r##"<a class="next" href="#lq">Next: the final quiz →</a>"##.to_string(),
        };
        h.push_str(&format!(
            r#"<section class="lesson" id="l{n}">
  <div class="rule"><b>{n:03}</b><span class="eyebrow">{eyebrow}</span><span class="line"></span><span class="eyebrow">~{min} min</span></div>
  <h2>{title}</h2>
{prose}
{figure}
{lab}
  <div class="lesson-foot"><button class="btn btn-sec" data-done="l{n}">✓ Mark as done</button>{next}</div>
</section>
"#,
            n = i + 1,
            eyebrow = esc(&plan.eyebrow),
            min = plan.minutes,
            title = esc(&plan.title),
            prose = sanitize(&lesson.prose_html),
            figure = figure(c, lesson),
            lab = if lesson.lab_dropped { String::new() } else { lab_card(c, i) },
        ));
    }
    let glossary: String = o
        .glossary
        .iter()
        .map(|t| format!(r#"<div class="card flat"><div class="term">{}</div><p>{}</p></div>"#, esc(&t.term), esc(&t.definition)))
        .collect();
    let license = match &c.meta.license {
        Some(url) => format!(r#" The paper’s license: <a href="{}">{}</a>."#, esc(url), esc(&license_name(url))),
        None => String::new(),
    };
    h.push_str(&format!(
        r##"<section class="lesson" id="lq">
  <div class="rule"><b>{q:03}</b><span class="eyebrow">Check yourself</span><span class="line"></span><span class="eyebrow">{nq} questions</span></div>
  <h2>Final quiz.</h2>
  <p>Pick an answer and you’ll see straight away why it’s right or wrong.</p>
  <div class="quiz" id="quiz"></div>
  <div class="inverted result" id="quizResult"><div class="mono" style="font-size:11px;letter-spacing:.14em;color:rgba(255,255,255,.6);position:relative">RESULT</div><div class="score"></div><div class="msg"></div></div>
  <div class="lesson-foot"><button class="btn btn-sec" data-done="lq">✓ Mark as done</button><a class="next" href="#top">Back to top ↑</a></div>
</section>
<section id="glossary" style="padding:44px 0;border-bottom:1px solid var(--border)">
  <div class="rule"><b>ABC</b><span class="eyebrow">Word bank</span><span class="line"></span></div>
  <h2>Word bank.</h2>
  <div class="glossary">{glossary}</div>
</section>
<section class="about" id="about" style="padding:44px 0">
  <div class="rule"><b>i</b><span class="eyebrow">About this course</span><span class="line"></span></div>
  <h2>About this course.</h2>
  <p>An AI model ({model}) wrote this course from <a href="{abs}">“{paper}”</a> by {authors} (arXiv:{id}, version {v}). It explains the paper in its own words for curious beginners, so it simplifies, and it can make mistakes. When the course and the paper disagree, the paper is right.</p>
  <p>Figures are shown straight from arXiv and belong to the paper’s authors.{license}</p>
  <p>Built on {built} by <a href="{origin}/paper-bro/">paper-bro</a>. Spotted something wrong? <a href="{origin}/paper-bro/abs/{id}?report=1">Report a problem</a>.</p>
</section>
    </div>
  </div>
</main>
<footer>Made with <a href="{origin}/paper-bro/">paper-bro</a> · turn any arXiv paper into an interactive course</footer>
"##,
        q = n + 1,
        nq = o.quiz.len(),
        model = esc(&model_name(c.model)),
        paper = esc(&c.meta.title),
        authors = esc(&authors_line(&c.meta.authors)),
        v = c.meta.version,
        built = esc(c.built),
    ));
    h.push_str(&scripts(c, origin, None));
    h
}

/// A page holding one lesson's lab alone, so QA can blame errors and hangs on
/// the right lab.
pub fn lab_page(c: &Course, origin: &str, i: usize) -> String {
    let mut h = head(c, origin, "lab");
    h.push_str(&format!(
        r#"<body style="padding:24px;max-width:820px"><div class="content">{}</div>"#,
        lab_card(c, i)
    ));
    h.push_str(&scripts(c, origin, Some(i)));
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_anything_executable() {
        let dirty = r#"<p class="lede x">Hi <b>there</b><script>alert(1)</script><img src=x onerror="fetch('/api/me')">
            <a href="javascript:alert(1)">bad</a><a href="https://arxiv.org/abs/1">ok</a>
            <div class="callout" style="position:fixed" onclick="x()">c</div><iframe src="https://evil"></iframe></p>"#;
        let clean = sanitize(dirty);
        for banned in ["<script", "onerror", "<img", "javascript:", "style=", "onclick", "<iframe", "class=\"lede x\""] {
            assert!(!clean.contains(banned), "{banned} survived: {clean}");
        }
        assert!(clean.contains(r#"<a href="https://arxiv.org/abs/1" rel="noopener noreferrer">ok</a>"#), "{clean}");
        assert!(clean.contains(r#"class="callout""#));
    }

    #[test]
    fn missing_figures_names_each_lesson_whose_figure_is_gone() {
        let lesson = |figure: &str| Lesson {
            prose_html: String::new(),
            figure: figure.into(),
            figure_note: String::new(),
            lab_html: String::new(),
            lab_css: String::new(),
            lab_js: String::new(),
            lab_dropped: false,
        };
        let figures = [Figure { key: "F1".into(), url: "https://arxiv.org/html/x/f1.png".into(), caption: String::new() }];
        let lessons = [lesson("F1"), lesson(""), lesson("F2a")];
        assert_eq!(missing_figures(&lessons, &figures), ["lesson 3: F2a"]);
        assert!(missing_figures(&lessons[..2], &figures).is_empty());
    }

    #[test]
    fn sanitize_leaves_only_https_links() {
        // The page does its own navigation, and a `#` link only jumps to the top,
        // so any link without a real https address becomes plain text.
        let clean = sanitize(
            r##"<p><a href="#">Next lesson →</a> <a href="#l2">Lesson 2</a> <a href="/p/x">Back</a> <a href="//evil.example/x">cdn</a> <a>bare</a> <a href="https://arxiv.org/abs/1">the paper</a></p>"##,
        );
        assert_eq!(
            clean,
            r#"<p>Next lesson → Lesson 2 Back cdn bare <a href="https://arxiv.org/abs/1" rel="noopener noreferrer">the paper</a></p>"#
        );
    }

    #[test]
    fn embedded_json_cannot_close_its_script_tag() {
        let s = embed_json(&json!({"js": "</script><script>alert(1)</script><!--"}));
        assert!(!s.contains('<'));
        let back: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(back["js"], "</script><script>alert(1)</script><!--");
    }

    #[test]
    fn model_names_drop_the_slug() {
        assert_eq!(model_name("acme/tiny-model-0425"), "Tiny Model");
        assert_eq!(model_name("meta-models/muse-spark-open"), "Muse Spark Open");
        assert_eq!(model_name("some-model-v2"), "Some Model V2");
    }

    #[test]
    fn license_names() {
        assert_eq!(license_name("http://arxiv.org/licenses/nonexclusive-distrib/1.0/"), "arXiv non-exclusive license");
        assert_eq!(license_name("http://creativecommons.org/licenses/by/4.0/"), "CC BY 4.0");
        assert_eq!(license_name("http://creativecommons.org/publicdomain/zero/1.0/"), "CC0");
    }
}
