//! Draw a PDF's pages into images, in a process of its own.
//!
//! A paper that arXiv has only as a PDF reaches the model as pictures of its
//! pages (`pipeline::paper_message`), unless `PDF_INPUT=file` sends the PDF
//! itself. Pictures are the default because a model server may read only
//! part of an attached PDF: the one this was built against read at most 20
//! pages of one per request (measured: a 45-page PDF showed pages 1 to 20,
//! two PDFs in one request still showed 20 pages in all, and no request
//! option raised it), while it took 100 ordinary images in one request. A
//! Gemini model reads a whole PDF, and for fewer tokens than the pictures.
//! Never text pulled out of the PDF: math comes out scrambled, and figures
//! and the shape of tables are lost.
//!
//! The drawing is `hayro`, in safe Rust, but safe is not the same as cheap.
//! What a PDF costs to draw has little to do with the size of its file, the
//! library sets no limit of its own, and a file can simply never finish.
//! Inside the server that would be the server's memory, and a thread nobody
//! can stop. So the server never draws. It runs this same program again as
//! `paper-bro draw-pdf`, hands it the file on stdin and reads the pages from
//! its stdout, and that process gets:
//!
//! - a ceiling on its memory (`MEMORY`), which it sets on itself before it
//!   reads a byte. Past it that process dies, and only it;
//! - a time limit (`TIME`), after which the server kills it;
//! - no environment, so none of the server's settings or keys.
//!
//! A PDF sent whole is opened there too, only to count its pages
//! (`count_boxed`), so one that is damaged, protected or too long is turned
//! away before any model call, just as one to be drawn is.
//!
//! Whatever becomes of it, the paper is one we can't read (`Unsupported`),
//! never a failure worth retrying. A PDF has no figures a lesson can show.

use anyhow::{Context, Result};
use base64::Engine;
use base64::prelude::BASE64_STANDARD;
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{PixmapSettings, RenderCache, RenderSettings, render};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::ingest::Unsupported;

/// The argument that makes this program the drawing process (`child`).
pub const COMMAND: &str = "draw-pdf";
/// The largest PDF file taken. Sent whole, that is a 33 MB request with
/// every call; one of 32 MB was read.
const MAX_PDF: usize = 25 * 1024 * 1024;
/// How wide a drawn page is, in pixels: about 100 dpi for A4, and about 1,300
/// tokens a page for the model, whatever the paper size.
const PAGE_WIDE_PX: f32 = 833.0;
/// How tall a drawn page may be; a page of a stranger shape is shrunk to fit,
/// so it cannot cost, or weigh, an absurd amount.
const PAGE_HIGH_PX: f32 = 1400.0;
/// The most the drawn pages may weigh together as PNG, since they ride in
/// every call of a build and the server holds them for each. A hundred
/// ordinary pages come to about 18 MB. A hundred pages dense with figures do
/// not fit, and that paper is refused. Tried up to this, a 28 MB request.
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
/// The memory the drawing process may have (its address space). Real papers
/// needed 17 to 65 MB.
#[cfg(target_os = "linux")]
const MEMORY: u64 = 1024 * 1024 * 1024;
/// How long the drawing process may take, from its start to its last byte.
/// Real papers take a second or two.
const TIME: Duration = Duration::from_secs(120);
/// The most the drawing process may say: its pages as base64 in JSON, and
/// some room.
const MAX_ANSWER: u64 = MAX_IMAGE_BYTES as u64 * 4 / 3 + 1024 * 1024;

/// Whether a downloaded e-print is a PDF rather than a source archive.
pub fn is_pdf(bytes: &[u8]) -> bool {
    bytes.starts_with(b"%PDF")
}

fn cant(why: &str) -> Unsupported {
    Unsupported(format!("this paper was submitted as a PDF only, and {why}, which paper-bro can't read yet"))
}

/// Open a PDF that we take: not too large, readable, with at least one page
/// and at most `max_pages`. Only the drawing process calls this (`child`);
/// the module doc says why.
fn open(bytes: &[u8], max_pages: usize) -> Result<Pdf, Unsupported> {
    if bytes.len() > MAX_PDF {
        return Err(cant(&format!("its PDF is over {} MB", MAX_PDF / (1024 * 1024))));
    }
    let pdf = Pdf::new(bytes.to_vec()).map_err(|_| cant("its PDF is damaged or protected"))?;
    let pages = pdf.pages().len();
    if pages == 0 {
        return Err(cant("its PDF has no pages"));
    }
    if pages > max_pages {
        return Err(cant(&format!("its PDF has more than {max_pages} pages")));
    }
    Ok(pdf)
}

/// Draw every page of a PDF to a PNG, base64, in order, each `PAGE_WIDE_PX`
/// wide (narrower if it would otherwise be over `PAGE_HIGH_PX` tall).
fn draw(pdf: &Pdf) -> Result<Vec<String>, Unsupported> {
    let pages = pdf.pages();
    let cache = RenderCache::new();
    let (settings, render_settings) = (InterpreterSettings::default(), RenderSettings::default());
    let (mut out, mut weight) = (Vec::with_capacity(pages.len()), 0usize);
    for page in pages.iter() {
        let (w, h) = page.render_dimensions();
        if !(w.is_finite() && h.is_finite() && w >= 1.0 && h >= 1.0) {
            return Err(cant("a page of its PDF has no size"));
        }
        let scale = (PAGE_WIDE_PX / w).min(PAGE_HIGH_PX / h);
        let pixmap = render(page, &cache, &settings, &render_settings, &PixmapSettings { x_scale: scale, y_scale: scale, bg_color: WHITE });
        let png = pixmap.into_png().map_err(|_| cant("a page of its PDF could not be drawn"))?;
        weight += png.len();
        if weight > MAX_IMAGE_BYTES {
            return Err(cant("its pages come to too much data"));
        }
        out.push(BASE64_STANDARD.encode(png));
    }
    Ok(out)
}

/// What the server wants from the drawing process: every page drawn, or for
/// a PDF the model is sent whole, only how many pages it has.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Want {
    Pages,
    Count,
}

/// What the drawing process says on its stdout, as JSON: the pages, how many
/// there are, or why it won't take this PDF.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Drawn {
    Pages(Vec<String>),
    Count(usize),
    Refused(String),
}

/// The drawing process's answer about a PDF.
fn answer(bytes: &[u8], max_pages: usize, want: Want) -> Drawn {
    let read = open(bytes, max_pages).and_then(|pdf| match want {
        Want::Pages => draw(&pdf).map(Drawn::Pages),
        Want::Count => Ok(Drawn::Count(pdf.pages().len())),
    });
    read.unwrap_or_else(|Unsupported(why)| Drawn::Refused(why))
}

/// The whole of `paper-bro draw-pdf <max pages> [count]`: the PDF on stdin,
/// the answer on stdout. `main` runs it before anything else is set up.
pub fn child() -> Result<()> {
    limit_memory()?;
    let args: Vec<String> = std::env::args().skip(2).collect();
    let max_pages = args.first().and_then(|n| n.parse().ok()).context("usage: paper-bro draw-pdf <max pages> [count]")?;
    let want = if args.get(1).is_some_and(|a| a == "count") { Want::Count } else { Want::Pages };
    let mut pdf = Vec::new();
    std::io::stdin().lock().take(MAX_PDF as u64 + 1).read_to_end(&mut pdf).context("reading the PDF")?;
    serde_json::to_writer(std::io::stdout().lock(), &answer(&pdf, max_pages, want)).context("writing the answer")?;
    Ok(())
}

/// Put the ceiling on this process's memory. On Linux, where the server
/// runs; elsewhere (a developer's machine) there is none.
fn limit_memory() -> Result<()> {
    #[cfg(target_os = "linux")]
    rlimit::setrlimit(rlimit::Resource::AS, MEMORY, MEMORY).context("limiting the memory that drawing a PDF may use")?;
    Ok(())
}

/// Draw the pages of a PDF of at most `max_pages` pages, in a process of its
/// own (see the module doc).
pub async fn draw_boxed(pdf: &[u8], max_pages: usize) -> Result<Vec<String>> {
    match ask(pdf, max_pages, Want::Pages).await? {
        Drawn::Pages(pages) => Ok(pages),
        _ => Err(cant("its PDF could not be drawn").into()),
    }
}

/// Open a PDF in a process of its own only to count its pages, for a PDF
/// the model is sent whole: one that would not be drawn is not sent either.
pub async fn count_boxed(pdf: &[u8], max_pages: usize) -> Result<usize> {
    match ask(pdf, max_pages, Want::Count).await? {
        Drawn::Count(pages) => Ok(pages),
        _ => Err(cant("its PDF could not be opened").into()),
    }
}

async fn ask(pdf: &[u8], max_pages: usize, want: Want) -> Result<Drawn> {
    // On Linux this is the running program itself, even when a deploy has
    // since replaced the file it was started from.
    let this_program: PathBuf =
        if cfg!(target_os = "linux") { "/proc/self/exe".into() } else { std::env::current_exe().context("finding this program's own file")? };
    let mut drawer = Command::new(this_program);
    drawer.arg(COMMAND).arg(max_pages.to_string());
    if want == Want::Count {
        drawer.arg("count");
    }
    drawn_by(drawer, pdf, TIME, want).await
}

/// Run a drawing process on a PDF and read its answer, for at most `limit`.
async fn drawn_by(mut drawer: Command, pdf: &[u8], limit: Duration, want: Want) -> Result<Drawn> {
    let (doing, done) = match want {
        Want::Pages => ("draw", "drawn"),
        Want::Count => ("open", "opened"),
    };
    let mut child = drawer
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("starting the process that draws a PDF")?;
    let (mut to_child, mut from_child) = (child.stdin.take().context("no pipe to the drawing process")?, child.stdout.take().context("no pipe from it")?);
    // The process is killed when `child` is dropped, which is on every way out
    // of this conversation that has not seen it exit: the time running out,
    // an answer that is too long, a pipe that breaks.
    let conversation = async move {
        // It reads the whole PDF before it writes a byte, so sending first
        // and reading after cannot leave both sides waiting. One that died
        // early breaks the pipe; how it ended tells that story.
        let _ = to_child.write_all(pdf).await;
        drop(to_child);
        let mut said = Vec::new();
        (&mut from_child).take(MAX_ANSWER + 1).read_to_end(&mut said).await?;
        if said.len() as u64 > MAX_ANSWER {
            return anyhow::Ok(None);
        }
        Ok(Some((child.wait().await?, said)))
    };
    match tokio::time::timeout(limit, conversation).await {
        Err(_) => Err(cant(&format!("its PDF took too long to {doing}")).into()),
        Ok(Err(e)) => Err(e.context("talking to the process that draws a PDF")),
        Ok(Ok(None)) => Err(cant("its pages come to too much data").into()),
        Ok(Ok(Some((ended, said)))) => match serde_json::from_slice(&said) {
            Ok(Drawn::Refused(why)) if ended.success() => Err(Unsupported(why).into()),
            Ok(answer) if ended.success() => Ok(answer),
            // It crashed, ran out of memory, or said something else.
            _ => Err(cant(&format!("its PDF could not be {done}")).into()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

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

    /// Open and draw, as the drawing process does, with room for 100 pages.
    fn drawn(bytes: &[u8]) -> Result<Vec<String>, Unsupported> {
        draw(&open(bytes, 100)?)
    }

    #[test]
    fn a_pdf_is_drawn_one_image_per_page() {
        let pages = drawn(&pdf_of(&[(612, 792), (612, 792), (612, 792)])).unwrap();
        assert_eq!(pages.len(), 3);
        // Every page is the same width, whatever the paper size, so costs the same.
        for page in &pages {
            assert_eq!(png_size(page), (833, 1078));
        }
        assert_ne!(pages[0], pages[1], "the first page has ink on it, the second does not");
        assert_eq!(pages[1], pages[2], "blank pages draw alike");
    }

    #[test]
    fn a_page_of_another_shape_is_scaled_to_the_same_width_and_a_very_tall_one_to_fit() {
        let pages = drawn(&pdf_of(&[(595, 842), (300, 3000)])).unwrap();
        assert_eq!(png_size(&pages[0]), (833, 1178), "A4");
        let (w, h) = png_size(&pages[1]);
        assert!(h <= 1400 && w < 833, "a tall page is shrunk to fit: {w}x{h}");
    }

    #[test]
    fn a_pdf_over_the_page_limit_is_refused_before_any_page_is_drawn() {
        let refused = open(&pdf_of(&vec![(612, 792); 4]), 3).err().unwrap();
        assert!(refused.0.contains("more than 3 pages"), "{}", refused.0);
        assert!(drawn(&pdf_of(&vec![(612, 792); 2])).is_ok());
        let Drawn::Pages(pages) = answer(&pdf_of(&vec![(612, 792); 3]), 3, Want::Pages) else { panic!("not drawn") };
        assert_eq!(pages.len(), 3, "a PDF of exactly the limit is taken");
    }

    #[test]
    fn a_pdf_sent_whole_is_only_counted_and_refused_over_the_same_limit() {
        assert_eq!(answer(&pdf_of(&vec![(612, 792); 3]), 3, Want::Count), Drawn::Count(3));
        let Drawn::Refused(why) = answer(&pdf_of(&vec![(612, 792); 4]), 3, Want::Count) else { panic!("not refused") };
        assert!(why.contains("more than 3 pages"), "{why}");
        let Drawn::Refused(why) = answer(b"%PDF-1.5 ...", 3, Want::Count) else { panic!("not refused") };
        assert!(why.contains("damaged or protected"), "{why}");
    }

    #[test]
    fn a_pdf_over_the_size_limit_is_refused_and_the_limit_named() {
        let mut big = b"%PDF-1.5 ".to_vec();
        big.resize(MAX_PDF + 1, b' ');
        assert!(open(&big, 100).err().unwrap().0.contains("over 25 MB"));
    }

    #[test]
    fn a_damaged_pdf_is_refused() {
        for junk in [&b"%PDF-1.5 ..."[..], &b"%PDF-1.7\n1 0 obj << /Type /Catalog >>"[..], &b"%PDF"[..]] {
            assert!(open(junk, 100).err().unwrap().0.contains("damaged or protected"));
        }
    }

    #[test]
    fn the_two_sides_agree_on_what_an_answer_looks_like() {
        // The drawing process writes this; `drawn_by` reads it.
        assert_eq!(serde_json::to_string(&Drawn::Pages(vec!["QUJD".into(), "REVG".into()])).unwrap(), r#"{"pages":["QUJD","REVG"]}"#);
        assert_eq!(serde_json::to_string(&Drawn::Count(38)).unwrap(), r#"{"count":38}"#);
        assert_eq!(serde_json::to_string(&Drawn::Refused("too many pages".into())).unwrap(), r#"{"refused":"too many pages"}"#);
        assert_eq!(serde_json::from_str::<Drawn>(r#"{"refused":"x"}"#).unwrap(), Drawn::Refused("x".into()));
    }

    /// A stand-in for the drawing process: a shell script.
    fn sh(script: &str) -> Command {
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg(script);
        c
    }

    /// The reason a visitor would be given: an unsupported paper, so not retried.
    fn refusal(e: anyhow::Error) -> String {
        e.downcast_ref::<Unsupported>().unwrap_or_else(|| panic!("not a refusal: {e:#}")).0.clone()
    }

    const WAIT: Duration = Duration::from_secs(20);
    /// More than a pipe holds, so a process that never reads it is noticed.
    const A_PDF: &[u8] = &[7; 300_000];
    const NOT_DRAWN: &str = "this paper was submitted as a PDF only, and its PDF could not be drawn, which paper-bro can't read yet";

    #[tokio::test]
    async fn the_pages_a_drawing_process_answers_with_come_back_in_order() {
        // Like the real one, it reads all it is sent and then answers.
        let pages = drawn_by(sh(r#"/bin/cat > /dev/null; printf '{"pages":["QUJD","REVG"]}'"#), A_PDF, WAIT, Want::Pages).await.unwrap();
        assert_eq!(pages, Drawn::Pages(vec!["QUJD".into(), "REVG".into()]));
        let count = drawn_by(sh(r#"/bin/cat > /dev/null; printf '{"count":38}'"#), A_PDF, WAIT, Want::Count).await.unwrap();
        assert_eq!(count, Drawn::Count(38));
    }

    #[tokio::test]
    async fn a_refusal_from_the_drawing_process_is_the_reason_given() {
        let e = drawn_by(sh(r#"/bin/cat > /dev/null; printf '{"refused":"too many pages"}'"#), A_PDF, WAIT, Want::Pages).await.unwrap_err();
        assert_eq!(refusal(e), "too many pages");
    }

    #[tokio::test]
    async fn a_drawing_process_that_dies_is_a_paper_we_cannot_read() {
        let ways = [
            "exit 3",                                                           // gives up without reading a byte
            "kill -9 $$",                                                       // is killed, as when it runs out of memory
            r#"/bin/cat > /dev/null; printf '{"pages":["QU'"#,                  // stops in the middle of its answer
            r#"/bin/cat > /dev/null; printf '{"pages":["QUJD"]}'; exit 1"#,     // answers, then fails
            r#"/bin/cat > /dev/null; printf 'hello'"#,                          // says something else
        ];
        for way in ways {
            let e = drawn_by(sh(way), A_PDF, WAIT, Want::Pages).await.unwrap_err();
            assert_eq!(refusal(e), NOT_DRAWN, "{way}");
        }
        // One that only had to count says so.
        let e = drawn_by(sh("kill -9 $$"), A_PDF, WAIT, Want::Count).await.unwrap_err();
        assert_eq!(refusal(e), NOT_DRAWN.replace("drawn", "opened"));
    }

    #[tokio::test]
    async fn a_drawing_process_that_takes_too_long_is_killed() {
        let mark = std::env::temp_dir().join(format!("paper-bro-test-{}-still-alive", std::process::id()));
        let _ = std::fs::remove_file(&mark);
        let started = Instant::now();
        let e = drawn_by(sh(&format!("/bin/sleep 1; echo yes > '{}'", mark.display())), A_PDF, Duration::from_millis(100), Want::Pages).await.unwrap_err();
        assert!(refusal(e).contains("its PDF took too long to draw"));
        assert!(started.elapsed() < Duration::from_millis(900), "the caller is not kept waiting: {:?}", started.elapsed());
        // Had it lived, it would have left its mark by now.
        tokio::time::sleep(Duration::from_millis(1600)).await;
        assert!(!mark.exists(), "the process was left running");
    }

    #[tokio::test]
    async fn a_drawing_process_that_says_too_much_is_not_heard_out() {
        let e = drawn_by(sh(&format!("/bin/cat > /dev/null; /usr/bin/yes | /usr/bin/head -c {}", MAX_ANSWER + 5_000_000)), A_PDF, WAIT, Want::Pages).await.unwrap_err();
        assert!(refusal(e).contains("its pages come to too much data"));
    }

    #[tokio::test]
    async fn the_drawing_process_gets_none_of_the_servers_environment() {
        assert!(std::env::var("HOME").is_ok(), "this test needs HOME set, to see it withheld");
        let pages = drawn_by(sh(r#"/bin/cat > /dev/null; printf '{"pages":["%s"]}' "${HOME:-withheld}""#), A_PDF, WAIT, Want::Pages).await.unwrap();
        assert_eq!(pages, Drawn::Pages(vec!["withheld".into()]));
    }
}
