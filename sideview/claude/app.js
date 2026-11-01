// the flurry:claude page's script (index.html), in a file so the page's CSP can forbid inline script
window.onerror = function (msg, src, line) { console.log("\u0001error: " + msg + " @" + line); };
(function () {
  var log = document.getElementById('log'), box = document.getElementById('box'), filesEl = document.getElementById('files');
  // think: the latest non-empty line of the thinking summary; thinking: the line still arriving
  var files = [], reply = null, text = '', think = '', thinking = '', stopped = false, busy = false, queued = false, painted = 0;

  // $$..$$ / \[..\] blocks and $..$ / \(..\) inline, rendered by KaTeX; marked keeps code spans intact
  function tex(src, display) {
    try { return svgToImg(katex.renderToString(src, { displayMode: display, throwOnError: false, output: 'html' })); }
    catch (e) { return esc(src); }
  }
  // Servo paints SVG images but not inline <svg>, which KaTeX uses for radicals, wide accents and
  // stretchy arrows: turn each into an <img> of the same box, filled with the text color
  function svgToImg(html) {
    return html.replace(/<svg([^>]*)>([\s\S]*?)<\/svg>/g, function (_, attrs, body) {
      // a standalone SVG document needs its namespace
      // its size is set once laid out (sharpen): a standalone SVG of em units rasterizes at 16px/em, 1x
      var svg = '<svg xmlns="http://www.w3.org/2000/svg"' + attrs.replace(/style="[^"]*"/, '').replace(/ (width|height)="[^"]*"/g, '') + ' fill="#eceff4">' + body + '</svg>';
      // the box is katex.css's `.katex svg`; the SVG gets exactly that size in device pixels once
      // laid out, so its own preserveAspectRatio applies and it rasterizes sharp
      var h = /height="([^"]*)"/.exec(attrs);
      var style = 'display:block;position:absolute;width:100%;height:' + (h ? h[1] : 'inherit') + ';' + (/style="([^"]*)"/.exec(attrs) || ['', ''])[1];
      return '<img alt="" class="tex-svg" style="' + style + '" data-svg="' + encodeURIComponent(svg) + '">';
    });
  }
  // KaTeX images sized to their laid-out boxes in device pixels
  function sharpen(el) {
    var d = window.devicePixelRatio || 1;
    el.querySelectorAll('img.tex-svg').forEach(function (img) {
      var r = img.getBoundingClientRect();
      if (!r.width || !r.height) return;
      var size = ' width="' + Math.ceil(r.width * d) + '" height="' + Math.ceil(r.height * d) + '"';
      img.src = 'data:image/svg+xml;charset=utf-8,' + encodeURIComponent(decodeURIComponent(img.dataset.svg).replace('<svg', '<svg' + size));
    });
  }
  function esc(s) { return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;'); }
  marked.use({
    gfm: true,
    breaks: false,
    renderer: { html: function (t) { return esc(typeof t === 'string' ? t : t.text); } },
    extensions: [
      { name: 'mathBlock', level: 'block',
        start: function (s) { var i = s.search(/\$\$|\\\[/); return i < 0 ? undefined : i; },
        tokenizer: function (s) {
          var m = /^\$\$([\s\S]+?)\$\$/.exec(s) || /^\\\[([\s\S]+?)\\\]/.exec(s);
          if (m) return { type: 'mathBlock', raw: m[0], text: m[1].trim() };
        },
        renderer: function (t) { return tex(t.text, true); } },
      { name: 'mathInline', level: 'inline',
        start: function (s) { var i = s.search(/\$|\\\(/); return i < 0 ? undefined : i; },
        tokenizer: function (s) {
          var m = /^\$\$([\s\S]+?)\$\$/.exec(s);
          if (m) return { type: 'mathInline', raw: m[0], text: m[1].trim(), display: true };
          m = /^\\\(([\s\S]+?)\\\)/.exec(s) || /^\$(?!\s)((?:\\.|[^\\$\n])+?)(?<!\s)\$(?!\d)/.exec(s);
          if (m) return { type: 'mathInline', raw: m[0], text: m[1], display: false };
        },
        renderer: function (t) { return tex(t.text, t.display); } }
    ]
  });

  // follow new output only while scrolled to the bottom: scrolling up detaches, back down reattaches
  var follow = true, lastTop = 0;
  function bottom() { log.scrollTop = log.scrollHeight; lastTop = log.scrollTop; follow = true; }
  log.addEventListener('scroll', function () {
    if (log.scrollTop < lastTop - 1) follow = false;
    else if (log.scrollHeight - log.scrollTop - log.clientHeight < 4) follow = true;
    lastTop = log.scrollTop;
  });
  function add(cls) { var d = document.createElement('div'); d.className = 'msg ' + cls; log.appendChild(d); return d; }

  function markers(root) {
    root.querySelectorAll('ul, ol').forEach(function (l) {
      var n = l.tagName === 'OL' ? parseInt(l.getAttribute('start') || '1', 10) : 0;
      Array.prototype.forEach.call(l.children, function (li) {
        var m = document.createElement('span'); m.className = 'marker';
        m.textContent = n ? (n++) + '.' : '•';
        li.insertBefore(m, li.firstChild);
      });
    });
  }

  // re-render the streaming reply at most once per frame
  function paint() {
    queued = false; painted = Date.now();
    if (!busy && !reply) return;
    // until the answer starts, the latest line of the thinking summary stands in for it
    var s = text ? '' : status();
    if (!text && !s) return; // no empty bubble before there is something to show
    if (!reply) reply = add('claude');
    if (text) { reply.innerHTML = marked.parse(text); markers(reply); sharpen(reply); }
    else { reply.innerHTML = ''; var t = document.createElement('div'); t.className = 'think'; t.textContent = s; reply.appendChild(t); }
    if (follow) bottom();
  }
  // a long reply re-renders at most 10 times a second (each render parses all of it)
  function schedule() {
    if (queued) return;
    queued = true;
    var wait = text.length > 8000 ? painted + 100 - Date.now() : 0;
    if (wait > 0) setTimeout(function () { requestAnimationFrame(paint); }, wait);
    else requestAnimationFrame(paint);
  }

  function send() {
    var t = box.value.trim();
    if ((!t && !files.length) || busy) return;
    var sent = files;
    var u = add('user');
    files.forEach(function (f) { var i = document.createElement('img'); i.src = f.url; u.appendChild(i); });
    if (t) u.appendChild(document.createTextNode(t));
    box.value = ''; files = []; filesEl.innerHTML = ''; grow();
    reply = null; text = ''; think = ''; thinking = ''; stopped = false; busy = true; bottom();
    // images then text; the helper builds Claude Code's message from these
    console.log('\u0001claude:user' + sent.map(function (f) { return ' img:' + f.type + ':' + f.data; }).join('') + (t ? ' text:' + JSON.stringify(t) : ''));
  }

  function reset() {
    console.log('\u0001claude:new');
    log.innerHTML = ''; reply = null; text = ''; think = ''; thinking = ''; busy = false; files = []; filesEl.innerHTML = ''; box.value = ''; grow();
  }

  // last line of the thinking summary: "what Claude is doing now"
  function clean(l) { return l.replace(/[*#_`]/g, '').trim(); }
  function status() { return clean(thinking) || think; }
  function addThinking(s) {
    var lines = (thinking + s).split('\n');
    thinking = lines.pop();
    lines.forEach(function (l) { l = clean(l); if (l) think = l; });
  }

  // ends the reply; errors are always shown, after any partial answer (not for a Ctrl+C stop)
  function finish(err) {
    if (!busy) return;
    paint();
    busy = false;
    if (err && !stopped) {
      if (!reply) reply = add('claude');
      var e = document.createElement('p'); e.textContent = err; reply.appendChild(e); if (follow) bottom();
    }
    // a reply stopped during thinking keeps no thinking line
    if (reply && !text && !err) log.removeChild(reply);
    reply = null;
  }

  // stream-json events from `claude -p`, as the lines' text
  window.__claude = function (lines) {
    lines.forEach(function (l) {
      var ev;
      try { ev = JSON.parse(l); } catch (e) { return; }
      if (ev && typeof ev === 'object') handle(ev);
    });
  };
  function handle(ev) {
    if (!busy) return;
    var d = ev.type === 'stream_event' && ev.event && ev.event.type === 'content_block_delta' && ev.event.delta || null;
    if (d && d.type === 'text_delta') {
      text += String(d.text); schedule();
    } else if (d && d.type === 'thinking_delta') {
      addThinking(String(d.thinking)); if (!text) schedule();
    } else if (ev.type === 'result') {
      finish(ev.is_error ? String(ev.result || (ev.errors || []).join('; ') || ev.subtype || 'error') : '');
    } else if (ev.type === 'exit') {
      finish('');
    }
  }

  // centers the caret's line in the input (the scroll position clamps at the top and bottom)
  var scroller = document.getElementById('scroller'), mirror = document.getElementById('mirror');
  function reveal() {
    var at = box.selectionDirection === 'backward' ? box.selectionStart : box.selectionEnd;
    var mark = document.createElement('span'); mark.textContent = '⁠'; // no break opportunity
    mirror.textContent = box.value.slice(0, at);
    mirror.appendChild(mark);
    mirror.appendChild(document.createTextNode(box.value.slice(at)));
    var r = mark.getBoundingClientRect();
    scroller.scrollTop = r.top - mirror.getBoundingClientRect().top + r.height / 2 - scroller.clientHeight / 2;
  }
  function grow() {
    var top = scroller.scrollTop;
    box.style.height = 'auto'; box.style.height = box.scrollHeight + 'px';
    scroller.scrollTop = top; // collapsing the box clamped it
  }
  box.addEventListener('input', function () { grow(); reveal(); });
  // caret keys: moved once the key's default action has run; other keys keep a wheel-scrolled view
  box.addEventListener('keydown', function () {
    var was = box.selectionStart + ':' + box.selectionEnd;
    requestAnimationFrame(function () { if (box.selectionStart + ':' + box.selectionEnd !== was) reveal(); });
  });
  // the panel opens before its final size: keep the box fully shown and the log pinned
  window.addEventListener('resize', function () { grow(); if (follow) bottom(); });
  // Sora arrives after the first layout and an empty box keeps the fallback font's metrics (shorter
  // box, smaller caret) until its text changes: touch the text once the font is in
  if (document.fonts) document.fonts.ready.then(function () { var v = box.value; box.value = v + ' '; box.value = v; grow(); });
  box.addEventListener('keydown', function (e) {
    if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); send(); }
  });
  document.addEventListener('keydown', function (e) {
    if (e.ctrlKey && (e.key === 'n' || e.key === 'N')) { e.preventDefault(); reset(); }
    // Esc drops the last attached image
    else if (e.key === 'Escape' && files.length) { e.preventDefault(); files.pop(); filesEl.removeChild(filesEl.lastChild); }
    // Ctrl+C stops the reply mid-stream (unless copying a selection); the conversation and partial answer stay
    else if (e.ctrlKey && (e.key === 'c' || e.key === 'C') && busy && box.selectionStart === box.selectionEnd && !String(window.getSelection())) {
      e.preventDefault();
      console.log('\u0001claude:stop');
      stopped = true;
    }
    else if (document.activeElement !== box && !e.ctrlKey && !e.altKey) box.focus();
  });
  // Ctrl+V with an image on the clipboard: the helper hands over the bytes directly
  window.__flurryPaste = function (b64, type) {
    if (!/^image\/(png|jpeg|gif|webp)$/.test(type)) return;
    var url = 'data:' + type + ';base64,' + b64;
    files.push({ type: type, url: url, data: b64 });
    var img = document.createElement('img'); img.src = url; filesEl.appendChild(img);
  };
  document.addEventListener('click', function (e) {
    var a = e.target.closest && e.target.closest('a[href]');
    if (a) { e.preventDefault(); console.log('\u0001open:' + a.href); }
  });
  box.focus();
})();
