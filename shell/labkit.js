// paper-bro lab kit. Runs inside each lab's sandboxed iframe (opaque origin),
// before the lab's own code. It gives labs a few tested drawing helpers, turns
// silent failures (NaN data, zero-size canvases) into thrown errors, and talks
// to the course page only through postMessage: height, ready/failed, errors,
// and the self-test the QA browser asks for.
(function () {
  var post = function (m) { m.pb = 1; parent.postMessage(m, '*'); };
  var errors = [];
  var report = function (msg) {
    msg = String(msg).slice(0, 400);
    if (errors.length < 20) errors.push(msg);
    post({ type: 'error', message: msg });
  };
  addEventListener('error', function (e) { report(e.message || 'error'); });
  addEventListener('unhandledrejection', function (e) {
    var r = e.reason; report('Unhandled promise rejection: ' + (r && r.message ? r.message : r));
  });

  window.$ = function (s, r) { return (r || document).querySelector(s); };
  window.$$ = function (s, r) { return Array.prototype.slice.call((r || document).querySelectorAll(s)); };
  window.REDUCED = matchMedia('(prefers-reduced-motion: reduce)').matches;

  window.hiDPI = function (cv, w, h) {
    if (!cv || !cv.getContext) throw new Error('hiDPI: not a canvas');
    w = w || cv.clientWidth; h = h || cv.clientHeight;
    if (!(w > 0 && h > 0)) throw new Error('hiDPI: the canvas has zero size');
    var d = window.devicePixelRatio || 1;
    cv.width = Math.round(w * d); cv.height = Math.round(h * d);
    // clientHeight leaves out the border, but under box-sizing: border-box
    // the style height includes it. Add it back, or every redraw (a lab
    // animating, a plot following a slider) shrinks the canvas until it's gone.
    var cs = getComputedStyle(cv);
    var border = cs.boxSizing === 'border-box' ? parseFloat(cs.borderTopWidth) + parseFloat(cs.borderBottomWidth) : 0;
    cv.style.height = (h + border) + 'px';
    var c = cv.getContext('2d'); c.setTransform(d, 0, 0, d, 0, 0); return c;
  };

  var finite = function (v, what) {
    if (typeof v !== 'number' || !isFinite(v)) throw new Error(what + ' is not a finite number (' + v + ')');
    return v;
  };
  var niceTicks = function (lo, hi, n) {
    var span = hi - lo || 1, step = Math.pow(10, Math.floor(Math.log10(span / n)));
    [1, 2, 2.5, 5, 10].some(function (m) { if (span / (step * m) <= n) { step *= m; return true; } return false; });
    var out = [], t = Math.ceil(lo / step) * step;
    for (; t <= hi + step * 1e-9; t += step) out.push(+t.toFixed(10));
    return out;
  };
  var label = function (v) {
    var a = Math.abs(v);
    return a !== 0 && (a >= 1e4 || a < 1e-2) ? v.toExponential(1) : String(+v.toFixed(3));
  };

  var PB = {
    color: {
      ink: '#0B0B0B', action: '#D85A30', status: '#2E6BE6', muted: '#A1A1AA', grid: '#E4E4E7',
      soft: '#FDF3EF', danger: '#DC2626', warning: '#D97706',
      series: ['#D85A30', '#2E6BE6', '#0B0B0B', '#D97706', '#7C3AED', '#059669']
    },

    fmt: function (v, d) { d = d == null ? 2 : d; return (v < 0 ? '−' : '') + Math.abs(v).toFixed(d); },

    // Wire every range, checkbox and select in the lab: on any change, read
    // them all into {name: value} (by name, else id), copy values into
    // elements marked data-show="name" (data-digits="n" to round), and call
    // update(values). Calls update once straight away.
    bindSliders: function (update, root) {
      root = root || document;
      var inputs = $$('input[type=range],input[type=checkbox],select', root);
      var run = function () {
        var v = {};
        inputs.forEach(function (i) {
          var k = i.name || i.id; if (!k) return;
          v[k] = i.type === 'checkbox' ? i.checked : i.tagName === 'SELECT' ? i.value : parseFloat(i.value);
        });
        $$('[data-show]', root).forEach(function (el) {
          var k = el.getAttribute('data-show'); if (!(k in v)) return;
          var x = v[k], d = el.getAttribute('data-digits');
          el.textContent = typeof x === 'number' ? (d != null ? PB.fmt(x, +d) : String(x)) : String(x);
        });
        update(v);
      };
      inputs.forEach(function (i) { i.addEventListener('input', run); i.addEventListener('change', run); });
      run();
      return run;
    },

    // Line, scatter, bar and area series on shared axes.
    //   PB.plot(canvas, {series: [{points: [[x, y], ...], kind: 'line', label, color}],
    //                    x: {min, max, label}, y: {min, max, label}, height: 260})
    // Returns {x(v), y(v), ctx} so a lab can draw extra marks on top.
    plot: function (cv, o) {
      o = o || {}; var series = o.series || [];
      if (!series.length) throw new Error('PB.plot: no series');
      var W = cv.clientWidth || 600, H = o.height || cv.clientHeight || 260, ctx = hiDPI(cv, W, H);
      var xs = [], ys = [];
      series.forEach(function (s, si) {
        if (!s.points || !s.points.length) throw new Error('PB.plot: series ' + si + ' has no points');
        s.points.forEach(function (p, pi) {
          xs.push(finite(p[0], 'PB.plot series ' + si + ' point ' + pi + ' x'));
          ys.push(finite(p[1], 'PB.plot series ' + si + ' point ' + pi + ' y'));
        });
      });
      var ax = o.x || {}, ay = o.y || {};
      var x0 = ax.min != null ? ax.min : Math.min.apply(null, xs), x1 = ax.max != null ? ax.max : Math.max.apply(null, xs);
      var y0 = ay.min != null ? ay.min : Math.min(0, Math.min.apply(null, ys)), y1 = ay.max != null ? ay.max : Math.max.apply(null, ys);
      if (x1 === x0) { x0 -= 1; x1 += 1; } if (y1 === y0) { y0 -= 1; y1 += 1; }
      if (ay.max == null) y1 += (y1 - y0) * 0.08;
      var L = 46, R = 14, T = 14, B = ax.label ? 40 : 28;
      var X = function (v) { return L + (v - x0) / (x1 - x0) * (W - L - R); };
      var Y = function (v) { return H - B - (v - y0) / (y1 - y0) * (H - T - B); };
      ctx.clearRect(0, 0, W, H);
      ctx.font = '11px Inter, sans-serif'; ctx.lineWidth = 1;
      niceTicks(y0, y1, 5).forEach(function (t) {
        ctx.strokeStyle = PB.color.grid; ctx.beginPath(); ctx.moveTo(L, Y(t)); ctx.lineTo(W - R, Y(t)); ctx.stroke();
        ctx.fillStyle = '#71717A'; ctx.textAlign = 'right'; ctx.textBaseline = 'middle'; ctx.fillText(label(t), L - 6, Y(t));
      });
      niceTicks(x0, x1, 6).forEach(function (t) {
        ctx.fillStyle = '#71717A'; ctx.textAlign = 'center'; ctx.textBaseline = 'top'; ctx.fillText(label(t), X(t), H - B + 6);
      });
      ctx.strokeStyle = '#A1A1AA'; ctx.beginPath(); ctx.moveTo(L, T); ctx.lineTo(L, H - B); ctx.lineTo(W - R, H - B); ctx.stroke();
      if (ax.label) { ctx.fillStyle = '#52525B'; ctx.textAlign = 'center'; ctx.fillText(ax.label, (L + W - R) / 2, H - 16); }
      if (ay.label) { ctx.save(); ctx.translate(12, (T + H - B) / 2); ctx.rotate(-Math.PI / 2); ctx.fillStyle = '#52525B'; ctx.textAlign = 'center'; ctx.textBaseline = 'middle'; ctx.fillText(ay.label, 0, 0); ctx.restore(); }
      series.forEach(function (s, si) {
        var c = s.color || PB.color.series[si % PB.color.series.length], kind = s.kind || 'line';
        ctx.strokeStyle = c; ctx.fillStyle = c; ctx.lineWidth = 2;
        if (kind === 'bar') {
          var bw = Math.max(2, (W - L - R) / s.points.length * 0.7 / series.length);
          s.points.forEach(function (p) { var x = X(p[0]) - bw * series.length / 2 + bw * si; ctx.fillRect(x, Math.min(Y(p[1]), Y(Math.max(y0, 0))), bw - 1, Math.abs(Y(Math.max(y0, 0)) - Y(p[1]))); });
        } else if (kind === 'scatter') {
          s.points.forEach(function (p) { ctx.beginPath(); ctx.arc(X(p[0]), Y(p[1]), s.r || 3.5, 0, 7); ctx.fill(); });
        } else {
          ctx.beginPath(); s.points.forEach(function (p, i) { i ? ctx.lineTo(X(p[0]), Y(p[1])) : ctx.moveTo(X(p[0]), Y(p[1])); });
          if (kind === 'area') { ctx.lineTo(X(s.points[s.points.length - 1][0]), Y(Math.max(y0, 0))); ctx.lineTo(X(s.points[0][0]), Y(Math.max(y0, 0))); ctx.globalAlpha = 0.18; ctx.fill(); ctx.globalAlpha = 1; ctx.beginPath(); s.points.forEach(function (p, i) { i ? ctx.lineTo(X(p[0]), Y(p[1])) : ctx.moveTo(X(p[0]), Y(p[1])); }); }
          ctx.stroke();
        }
      });
      var named = series.filter(function (s) { return s.label; });
      if (named.length) {
        ctx.textAlign = 'left'; ctx.textBaseline = 'middle'; ctx.font = '11.5px Inter, sans-serif';
        var lx = L + 10;
        named.forEach(function (s) {
          var c = s.color || PB.color.series[series.indexOf(s) % PB.color.series.length];
          ctx.fillStyle = c; ctx.fillRect(lx, T + 6, 10, 10); ctx.fillStyle = '#26262A'; ctx.fillText(s.label, lx + 14, T + 11);
          lx += 26 + ctx.measureText(s.label).width;
        });
      }
      return { x: X, y: Y, ctx: ctx };
    },

    // A grid of numbers as coloured cells (white to orange), e.g. attention
    // weights. PB.heatmap(canvas, [[...], ...], {rows, cols, min, max, digits})
    heatmap: function (cv, m, o) {
      o = o || {};
      if (!m || !m.length || !m[0].length) throw new Error('PB.heatmap: empty matrix');
      var flat = [];
      m.forEach(function (r, i) { r.forEach(function (v, j) { flat.push(finite(v, 'PB.heatmap cell [' + i + '][' + j + ']')); }); });
      var lo = o.min != null ? o.min : Math.min.apply(null, flat), hi = o.max != null ? o.max : Math.max.apply(null, flat);
      var rows = o.rows || [], cols = o.cols || [], L = rows.length ? 70 : 8, T = cols.length ? 26 : 8;
      var W = cv.clientWidth || 600, cw = (W - L - 8) / m[0].length, ch = Math.min(44, cw);
      var H = o.height || (T + ch * m.length + 8), ctx = hiDPI(cv, W, H);
      ctx.clearRect(0, 0, W, H); ctx.font = '11px Inter, sans-serif';
      m.forEach(function (r, i) {
        r.forEach(function (v, j) {
          var t = hi === lo ? 0.5 : (v - lo) / (hi - lo);
          ctx.fillStyle = 'rgb(' + Math.round(255 - t * 39) + ',' + Math.round(255 - t * 165) + ',' + Math.round(255 - t * 207) + ')';
          ctx.fillRect(L + j * cw, T + i * ch, cw - 2, ch - 2);
          if (o.digits != null && cw > 28) {
            ctx.fillStyle = t > 0.6 ? '#fff' : '#26262A'; ctx.textAlign = 'center'; ctx.textBaseline = 'middle';
            ctx.fillText(v.toFixed(o.digits), L + j * cw + cw / 2 - 1, T + i * ch + ch / 2 - 1);
          }
        });
      });
      ctx.fillStyle = '#52525B';
      rows.forEach(function (s, i) { ctx.textAlign = 'right'; ctx.textBaseline = 'middle'; ctx.fillText(String(s), L - 6, T + i * ch + ch / 2); });
      cols.forEach(function (s, j) { ctx.textAlign = 'center'; ctx.textBaseline = 'bottom'; ctx.fillText(String(s), L + j * cw + cw / 2, T - 6); });
    },

    // Math in a lab: PB.tex(element, 'x^2', displayMode)
    tex: function (el, latex, display) {
      if (window.katex) window.katex.render(String(latex), el, { displayMode: !!display, throwOnError: false });
      else el.textContent = String(latex);
    },

    __run: function (fn) {
      try { fn(); post({ type: 'ready' }); }
      catch (e) {
        report(e && e.message ? e.message : e);
        document.getElementById('lab-root').innerHTML =
          '<div class="pb-fallback">This lab couldn’t load. The lesson above still explains the idea.</div>';
        post({ type: 'failed' });
      }
    }
  };
  window.PB = PB;

  // Self-test, run when the QA browser asks: work every control, then report
  // errors, blank canvases, controls that change nothing, and junk text.
  var wait = function (ms) { return new Promise(function (r) { setTimeout(r, ms); }); };
  var snap = function (cs) {
    return cs.map(function (c) {
      try {
        if (!c.width || !c.height) return 'zero';
        // On screen but collapsed to nothing: its buffer can still hold an
        // old drawing that nobody sees.
        if (c.getClientRects().length && (!c.clientWidth || !c.clientHeight)) return 'zero';
        var x = c.getContext('2d'); if (!x) return 'other';
        var d = x.getImageData(0, 0, c.width, c.height).data, h = 0, ink = 0;
        for (var i = 0; i < d.length; i += 16) { h = (h * 31 + d[i] + d[i + 1] * 3 + d[i + 2] * 7 + d[i + 3]) | 0; if (d[i + 3] > 0) ink++; }
        return ink ? String(h) : 'blank';
      } catch (e) { return 'other'; }
    });
  };
  var selftest = async function () {
    var cs = $$('canvas');
    await wait(150);
    var before = snap(cs), textBefore = document.body.innerText;
    var heights = function () { return cs.map(function (c) { return c.clientHeight; }); };
    // Canvas heights before and after a second redraw at the same control
    // setting. A control may legitimately resize a canvas; redrawing with
    // nothing changed must not.
    var pairs = [];
    var again = async function (fire) { var a = heights(); fire(); await wait(30); pairs.push([a, heights()]); };
    var input = function (el) { return function () { el.dispatchEvent(new Event('input', { bubbles: true })); el.dispatchEvent(new Event('change', { bubbles: true })); }; };
    var controls = $$('input[type=range],input[type=checkbox],input[type=radio],select,button:not([disabled])').slice(0, 40);
    for (var k = 0; k < controls.length; k++) {
      var el = controls[k];
      if (!el.isConnected) continue;
      if (el.type === 'range') {
        var lo = parseFloat(el.min || '0'), hi = parseFloat(el.max || '100');
        var vals = [lo, (lo + hi) / 2, hi];
        for (var j = 0; j < vals.length; j++) {
          el.value = vals[j];
          input(el)();
          await wait(30);
          await again(input(el));
        }
      } else if (el.tagName === 'SELECT') {
        for (var o = 0; o < el.options.length; o++) { el.selectedIndex = o; input(el)(); await wait(30); await again(input(el)); }
      } else {
        el.click(); await wait(80);
        // A checkbox or radio redraws again on its events; a second click
        // on a button would be a new action.
        if (el.tagName === 'INPUT') await again(input(el));
      }
    }
    // Then watch it idle. A button can start an animation, and a lab with only
    // buttons has no other comparisons, so take three windows: one drop is a
    // canvas settling, a drop in two or more is one that shrinks on every redraw.
    await wait(500);
    for (var w = 0; w < 3; w++) { var a0 = heights(); await wait(250); pairs.push([a0, heights()]); }
    var after = snap(cs), textAfter = document.body.innerText;
    var blank = [];
    after.forEach(function (s, i) { if (s === 'blank' || s === 'zero') blank.push(i); });
    var changed = textAfter !== textBefore || after.some(function (s, i) { return s !== before[i]; });
    var bad = textAfter.match(/\bNaN\b|\bundefined\b|\bInfinity\b|\[object Object\]/g) || [];
    // A canvas that loses height more than once while nothing changed
    // shrinks on every redraw.
    var shrunk = [];
    cs.forEach(function (c, i) {
      var drops = pairs.filter(function (p) { return p[1][i] < p[0][i]; }).length;
      if (drops >= 2) shrunk.push(i);
    });
    return {
      errors: errors.slice(), canvases: cs.length, blank: blank, controls: controls.length,
      responsive: controls.length === 0 || changed, shrunk: shrunk,
      badText: bad.filter(function (v, i) { return bad.indexOf(v) === i; })
    };
  };
  addEventListener('message', function (e) {
    if (e.source !== parent || !e.data || e.data.pb !== 1 || e.data.type !== 'selftest') return;
    selftest().then(function (r) { post({ type: 'selftest', result: r }); },
                    function (err) { post({ type: 'selftest', result: { errors: [String(err)], crashed: true } }); });
  });

  addEventListener('DOMContentLoaded', function () {
    var root = document.getElementById('lab-root');
    var send = function () { post({ type: 'height', h: Math.ceil(root.getBoundingClientRect().height) + 4 }); };
    new ResizeObserver(send).observe(root);
    send();
  });
})();
