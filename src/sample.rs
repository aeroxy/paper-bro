//! A hand-written course with six good labs and seven broken ones, for
//! `paper-bro qa-sample`: proof that browser QA catches each failure kind
//! without spending anything on the model.

use crate::arxiv::Meta;
use crate::course::{LabPlan, Lesson, LessonPlan, Outline, QuizItem, Term};

pub fn course() -> (Meta, Outline, Vec<Lesson>) {
    let meta = Meta {
        id: "0000.00000".into(),
        version: 1,
        title: "A Sample Paper".into(),
        authors: vec!["Ada Lovelace".into()],
        abstract_: String::new(),
        categories: vec![],
        license: Some("http://creativecommons.org/licenses/by/4.0/".into()),
        published: None,
    };
    let labs: [(&str, &str, &str); 13] = [
        (
            "redraws every frame",
            r#"<div class="srow"><label for="k">Speed</label><input type="range" id="k" min="1" max="10" value="3"></div><canvas id="c"></canvas>"#,
            r#"let t = 0, speed = 3;
function frame() { const ctx = hiDPI($('#c')); const w = $('#c').clientWidth; ctx.fillStyle = '#D85A30'; ctx.fillRect((t * speed) % w, 20, 30, 30); t++; if (t < 400) requestAnimationFrame(frame); }
PB.bindSliders(v => { speed = v.k; });
requestAnimationFrame(frame);"#,
        ),
        (
            "shrinks its canvas (the old hiDPI bug)",
            r#"<div class="srow"><label for="k">Speed</label><input type="range" id="k" min="1" max="10" value="3"></div><canvas id="c"></canvas>"#,
            r#"const c = $('#c'); let t = 0;
function frame() { c.style.height = (c.clientHeight - 2) + 'px'; const ctx = c.getContext('2d'); ctx.fillStyle = '#2E6BE6'; ctx.fillRect(t % 200, 5, 20, 10); t++; if (t < 400) requestAnimationFrame(frame); }
PB.bindSliders(v => { t = 0; });
requestAnimationFrame(frame);"#,
        ),
        (
            "shrinks after a button press",
            r#"<button class="btn" id="b">Run</button><canvas id="c"></canvas>"#,
            r#"const c = $('#c'); let t = 0;
$('#b').addEventListener('click', () => { (function frame() { c.style.height = (c.clientHeight - 1) + 'px'; const ctx = c.getContext('2d'); ctx.fillStyle = '#2E6BE6'; ctx.fillRect(t % 200, 5, 20, 10); if (++t < 120) requestAnimationFrame(frame); })(); });"#,
        ),
        (
            "animates its height with a CSS transition",
            r#"<button class="btn" id="b">Collapse</button><canvas id="c" style="height:260px;transition:height 1.2s"></canvas>"#,
            r#"const c = $('#c'); const draw = () => { const ctx = hiDPI(c); ctx.fillStyle = '#2E6BE6'; ctx.fillRect(10, 10, 80, 30); };
draw(); $('#b').addEventListener('click', () => { c.style.height = '120px'; setTimeout(draw, 1300); });"#,
        ),
        (
            "has a select with 500 options",
            r#"<div class="srow"><label for="s">Item</label><select id="s"></select></div><canvas id="c"></canvas>"#,
            r#"const s = $('#s'); for (let i = 1; i <= 500; i++) s.add(new Option('Item ' + i, i));
PB.bindSliders(v => { const ctx = hiDPI($('#c')); ctx.clearRect(0, 0, 400, 100); ctx.fillStyle = '#2E6BE6'; ctx.fillRect(0, 0, 10 + (v.s % 400), 40); });"#,
        ),
        (
            "collapses to nothing",
            r#"<div class="srow"><label for="k">Width</label><input type="range" id="k" min="1" max="10" value="3"></div><canvas id="c"></canvas>"#,
            r#"const c = $('#c'), ctx = c.getContext('2d');
PB.bindSliders(v => { ctx.clearRect(0, 0, c.width, c.height); ctx.fillStyle = '#D85A30'; ctx.fillRect(0, 0, 20 * v.k, 40); });
c.style.height = '0px';"#,
        ),
        (
            "probes its sandbox",
            r#"<button class="btn" id="b">Probe</button><p id="o">not probed</p>"#,
            r#"$('#b').addEventListener('click', () => {
  let storage = false, cookie = true;
  try { localStorage.getItem('x'); storage = true; } catch (e) {}
  try { cookie = document.cookie !== undefined && document.cookie.length > 0; } catch (e) { cookie = false; }
  if (storage) throw new Error('the lab can reach localStorage');
  if (cookie) throw new Error('the lab can read cookies');
  fetch('https://example.com/steal').then(() => { $('#o').textContent = 'NETWORK REACHABLE'; throw new Error('the lab reached the network'); }, () => {});
  $('#o').textContent = 'sandboxed: no storage, no cookies';
});"#,
        ),
        (
            "works",
            r#"<div class="ctrl"><div class="srow"><label for="k">Steepness</label><input type="range" id="k" min="0.5" max="4" step="0.5" value="1"><span class="val" data-show="k" data-digits="1"></span></div></div><canvas id="c"></canvas>"#,
            r#"PB.bindSliders(v => { const pts = []; for (let x = -5; x <= 5; x += 0.25) pts.push([x, 1 / (1 + Math.exp(-v.k * x))]); PB.plot($('#c'), {series: [{points: pts, label: 'sigmoid'}], x: {label: 'input'}, y: {min: 0, max: 1}}); });"#,
        ),
        (
            "resizes with its controls",
            r#"<div class="srow"><label for="a">Rows</label><input type="range" id="a" min="1" max="8" step="1" value="4"></div><div class="srow"><label for="b">Zoom</label><input type="range" id="b" min="1" max="4" step="1" value="2"></div><canvas id="c"></canvas>"#,
            r#"PB.bindSliders(v => { const c = $('#c'), ctx = hiDPI(c, c.clientWidth, 260 - v.a * 15 - v.b * 10); ctx.fillStyle = '#2E6BE6'; ctx.fillRect(10, 10, 40 * v.b, 20); });"#,
        ),
        ("throws", r#"<div id="out"></div>"#, r#"$('#missing').textContent = 'x';"#),
        (
            "blank canvas",
            r#"<button class="btn" id="b">Draw</button><canvas id="c"></canvas>"#,
            r#"$('#b').addEventListener('click', () => { const s = $('#b').textContent; $('#b').textContent = s; });"#,
        ),
        (
            "unresponsive",
            r#"<div class="srow"><label for="s">Speed</label><input type="range" id="s" min="0" max="10" value="5"></div><p>Speed is fast.</p>"#,
            r#"$('#s').addEventListener('input', () => {});"#,
        ),
        ("hangs", r#"<button class="btn" id="b">Go</button><p id="o">ready</p>"#, r#"$('#b').addEventListener('click', () => { while (true) {} });"#),
    ];
    let lessons_plan = labs
        .iter()
        .enumerate()
        .map(|(i, (name, _, _))| LessonPlan {
            title: format!("Lesson {}: {name}.", i + 1),
            nav_title: name.to_string(),
            eyebrow: "Sample".into(),
            minutes: 3,
            objectives: vec![],
            paper_sections: vec![],
            key_ideas: vec![],
            figure: String::new(),
            lab: LabPlan { title: format!("A lab that {name}"), idea: String::new(), controls: String::new(), what_it_shows: String::new() },
        })
        .collect();
    let lessons = labs
        .iter()
        .map(|(_, html, js)| Lesson {
            prose_html: r#"<p>The logistic function is $\sigma(x) = \frac{1}{1+e^{-x}}$.</p><div class="callout"><strong>Key idea:</strong> it squashes.</div>"#.into(),
            figure: String::new(),
            figure_note: String::new(),
            lab_html: html.to_string(),
            lab_css: String::new(),
            lab_js: js.to_string(),
            lab_dropped: false,
        })
        .collect();
    let outline = Outline {
        course_title: "Sample Course".into(),
        tagline: "A sample,\\nfor testing.".into(),
        lede: "This checks that QA catches broken labs.".into(),
        minutes: 15,
        chips: vec!["🧪 13 labs".into()],
        lessons: lessons_plan,
        quiz: (0..4)
            .map(|i| QuizItem {
                question: format!("Question {i} about $x^2$?"),
                options: vec!["a".into(), "b".into(), "c".into(), "d".into()],
                answer: 1,
                explanation: "Because.".into(),
            })
            .collect(),
        glossary: vec![Term { term: "Sigmoid".into(), definition: "An S-shaped curve.".into() }],
    };
    (meta, outline, lessons)
}
