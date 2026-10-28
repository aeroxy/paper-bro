// paper-bro course runtime: the trusted script on every course page.
// Model-written content never runs here. Lesson prose arrives sanitized, and
// every lab runs in its own sandboxed iframe (no allow-same-origin), built
// below from data in #pb-data. Quiz and glossary text is set with textContent.
// Runs on DOMContentLoaded: KaTeX is loaded with `defer`, and deferred scripts
// run after inline ones at the end of <body> but before this event.
document.addEventListener('DOMContentLoaded', function () {
  var D = JSON.parse(document.getElementById('pb-data').textContent);
  var $ = function (s, r) { return (r || document).querySelector(s); };
  var $$ = function (s, r) { return Array.prototype.slice.call((r || document).querySelectorAll(s)); };
  var REDUCED = matchMedia('(prefers-reduced-motion: reduce)').matches;
  var MATH = {
    delimiters: [
      { left: '$$', right: '$$', display: true }, { left: '\\[', right: '\\]', display: true },
      { left: '$', right: '$', display: false }, { left: '\\(', right: '\\)', display: false }
    ],
    throwOnError: false
  };
  var math = function (el) { if (window.renderMathInElement) window.renderMathInElement(el, MATH); };

  // --- lab error reports: the first few distinct ones per visit ---
  var sent = 0, seen = {};
  var reportError = function (lab, msg) {
    var k = lab + '|' + msg;
    if (seen[k] || sent >= 3 || !D.errorUrl) return;
    seen[k] = 1; sent++;
    try { navigator.sendBeacon(D.errorUrl, JSON.stringify({ id: D.id, lab: lab, message: String(msg).slice(0, 300) })); } catch (e) {}
  };

  // --- labs ---
  var qa = { ready: false, labs: {} };
  window.__pbQa = qa;
  var frames = [];
  var waiting = {};
  var settle = function () {
    qa.ready = frames.every(function (f) { return qa.labs[f.id].state !== 'loading'; });
  };
  var srcdoc = function (lab) {
    return '<!doctype html><html><head><meta charset="utf-8">' + D.labHead + (lab.tex ? D.labTex : '') +
      '<style>' + D.labCss + '\n' + lab.css + '</style><script>' + D.labJs + '<\/script></head><body>' +
      '<div id="lab-root">' + lab.html + '</div><script>PB.__run(function(){\n' + lab.js + '\n});<\/script></body></html>';
  };
  $$('iframe.lab-frame').forEach(function (ifr) {
    var id = ifr.getAttribute('data-lab'), lab = D.labs[id];
    if (!lab) return;
    qa.labs[id] = { state: 'loading', errors: [] };
    ifr.srcdoc = srcdoc(lab);
    frames.push({ id: id, el: ifr });
  });
  settle();
  setTimeout(function () {
    // A lab that never reported is hung or broken; stop waiting on it.
    frames.forEach(function (f) { if (qa.labs[f.id].state === 'loading') qa.labs[f.id].state = 'silent'; });
    settle();
  }, 15000);
  addEventListener('message', function (e) {
    var m = e.data, f = null;
    for (var i = 0; i < frames.length; i++) if (frames[i].el.contentWindow === e.source) f = frames[i];
    if (!f || !m || m.pb !== 1) return;
    var st = qa.labs[f.id];
    if (m.type === 'height') f.el.style.height = Math.min(Math.max(m.h | 0, 60), 2400) + 'px';
    else if (m.type === 'ready') { st.state = 'ready'; settle(); }
    else if (m.type === 'failed') { st.state = 'failed'; settle(); }
    else if (m.type === 'error') { st.errors.push(String(m.message).slice(0, 300)); reportError(f.id, m.message); }
    else if (m.type === 'selftest' && waiting[f.id]) { waiting[f.id](m.result); delete waiting[f.id]; }
  });
  qa.selftest = function (id) {
    var f = frames.filter(function (x) { return x.id === id; })[0];
    if (!f) return Promise.reject(new Error('no lab ' + id));
    return new Promise(function (resolve) {
      waiting[id] = resolve;
      f.el.contentWindow.postMessage({ pb: 1, type: 'selftest' }, '*');
    });
  };
  qa.katexErrors = function () { return $$('.katex-error').map(function (e) { return e.textContent.slice(0, 120); }); };

  // --- figures shown from arXiv: fall back to the link if one won't load ---
  $$('.paper-fig img').forEach(function (img) {
    img.addEventListener('error', function () {
      var a = document.createElement('a'); a.href = img.src; a.textContent = 'Open this figure on arXiv';
      img.replaceWith(a);
    });
  });

  // --- progress ---
  var KEY = 'paper-bro:' + D.id + ':done', done = [];
  try { done = JSON.parse(localStorage.getItem(KEY) || '[]'); } catch (e) {}
  var ids = $$('section.lesson').map(function (s) { return s.id; });
  var save = function () { try { localStorage.setItem(KEY, JSON.stringify(done)); } catch (e) {} };
  var render = function () {
    var n = ids.filter(function (i) { return done.indexOf(i) >= 0; }).length;
    var t = $('#progTxt'), b = $('#progBar');
    if (t) t.textContent = n + ' / ' + ids.length + ' done';
    if (b) b.style.width = (ids.length ? n / ids.length * 100 : 0) + '%';
    $$('#sideNav a').forEach(function (a) { a.classList.toggle('done', done.indexOf(a.getAttribute('href').slice(1)) >= 0); });
    $$('[data-done]').forEach(function (btn) {
      var d = done.indexOf(btn.getAttribute('data-done')) >= 0;
      btn.classList.toggle('is-done', d); btn.textContent = d ? '✓ Done' : '✓ Mark as done';
    });
  };
  var markDone = function (id) { if (done.indexOf(id) < 0) { done.push(id); save(); render(); } };
  document.addEventListener('click', function (e) {
    var b = e.target.closest && e.target.closest('[data-done]'); if (!b) return;
    var id = b.getAttribute('data-done'), i = done.indexOf(id);
    if (i >= 0) done.splice(i, 1); else done.push(id);
    save(); render();
  });
  render();

  // In-page links. A browser aims its smooth scroll once, at the start, but the
  // labs further down only grow as they come into view, so on a long jump (to
  // the quiz) the target moves away and the scroll stops short. Jump straight
  // there instead, and re-aim for a moment if the page shifts under it.
  var settling = null;
  var stopSettling = function () { clearInterval(settling); settling = null; };
  // A reader who starts scrolling, or goes back, must not be pulled to an earlier target.
  ['wheel', 'touchstart', 'keydown', 'mousedown', 'popstate'].forEach(function (t) { addEventListener(t, stopSettling, { passive: true }); });
  document.addEventListener('click', function (e) {
    var a = e.target.closest && e.target.closest('a[href^="#"]');
    var id = a && a.getAttribute('href').slice(1);
    var target = id && document.getElementById(id);
    if (!target || e.defaultPrevented || e.button || e.metaKey || e.ctrlKey || e.shiftKey || a.target) return;
    e.preventDefault();
    stopSettling();
    var root = document.documentElement;
    var go = function () { root.style.scrollBehavior = 'auto'; target.scrollIntoView(); root.style.scrollBehavior = ''; };
    var docTop = function () { return target.getBoundingClientRect().top + window.pageYOffset; };
    go();
    history.pushState(null, '', '#' + id);
    // Compare where the target sits in the page, not on screen, so a reader who
    // starts scrolling is not pulled back.
    var last = docTop(), ticks = 0;
    settling = setInterval(function () {
      if (Math.abs(docTop() - last) > 1) { go(); last = docTop(); }
      if (++ticks >= 15) stopSettling();
    }, 120);
  });

  var links = $$('#sideNav a');
  if ('IntersectionObserver' in window) {
    var io = new IntersectionObserver(function (es) {
      es.forEach(function (en) {
        if (en.isIntersecting) links.forEach(function (a) { a.classList.toggle('active', a.getAttribute('href') === '#' + en.target.id); });
      });
    }, { rootMargin: '-40% 0px -55% 0px' });
    $$('section.lesson').forEach(function (s) { io.observe(s); });
  }

  // --- quiz (answers shuffled per course, the same for every visitor) ---
  var quiz = $('#quiz');
  if (quiz && D.quiz.length) {
    var answered = {};
    D.quiz.forEach(function (q, i) {
      var order = q.options.map(function (_, j) { return j; });
      var r = (i * 7 + 3) % order.length; order = order.slice(r).concat(order.slice(0, r));
      var card = document.createElement('div'); card.className = 'card flat';
      var eb = document.createElement('div'); eb.className = 'eyebrow'; eb.textContent = 'Question ' + String(i + 1).padStart(2, '0');
      var qt = document.createElement('div'); qt.className = 'q'; qt.textContent = q.question;
      var opts = document.createElement('div'); opts.className = 'opts';
      var fb = document.createElement('div'); fb.className = 'fb';
      order.forEach(function (j, k) {
        var b = document.createElement('button'); b.className = 'btn btn-sec opt';
        b.textContent = String.fromCharCode(65 + k) + '. ' + q.options[j];
        b.addEventListener('click', function () {
          if (i in answered) return;
          answered[i] = j === q.answer;
          $$('.opt', opts).forEach(function (x, kk) {
            x.disabled = true;
            if (order[kk] === q.answer) x.classList.add('right'); else if (x === b) x.classList.add('wrong');
          });
          fb.style.display = 'block';
          var head = document.createElement('strong');
          head.textContent = answered[i] ? '✓ Correct! ' : '✗ Not quite. ';
          head.className = answered[i] ? 'good' : 'bad';
          fb.textContent = q.explanation; fb.prepend(head); math(fb);
          var total = Object.keys(answered).length;
          if (total === D.quiz.length) {
            var score = Object.keys(answered).filter(function (k) { return answered[k]; }).length;
            var res = $('#quizResult'); res.style.display = 'block';
            $('.score', res).textContent = score + ' / ' + D.quiz.length;
            $('.msg', res).textContent = score === D.quiz.length ? 'Perfect. You really get this paper.'
              : score >= D.quiz.length * 0.7 ? 'Great job: you have the core ideas down.'
              : 'Good start. Revisit the labs for the questions you missed.';
            markDone('lq');
            res.scrollIntoView({ behavior: REDUCED ? 'auto' : 'smooth', block: 'center' });
          }
        });
        opts.appendChild(b);
      });
      card.appendChild(eb); card.appendChild(qt); card.appendChild(opts); card.appendChild(fb);
      quiz.appendChild(card);
    });
  }

  // KaTeX is ready by now unless the CDN is down, in which case math stays as source.
  math(document.querySelector('.content') || document.body);
  math(document.querySelector('.hero') || document.body);
});
