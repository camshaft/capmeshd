//! The surface attachment page (DESIGN §10.1).
//!
//! Served same-origin at `/s/{id}` for every surface — the page is static and
//! reads its surface id from the URL, so there is no server-side templating. It
//! renders two zones: a **main view** (the currently-selected item) and a
//! **visible inbox feed** (every item ever pushed). It attaches over SSE
//! (`/s/{id}/events`), which delivers an initial snapshot then live updates, so
//! the surface reflects pushes with no reload and survives tab close on the
//! server side.
//!
//! Trust model (DESIGN §8): `html`/`script` items are same-origin and trusted
//! (only cluster-authenticated hosts can push), so they render/execute directly;
//! `navigate`/`pdf` third-party URLs render inside a **sandboxed iframe** that
//! cannot reach this origin.

/// The static HTML/JS for a surface attachment.
pub const SURFACE_PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>surface</title>
<style>
  :root { color-scheme: light dark; }
  * { box-sizing: border-box; }
  body { margin: 0; font: 15px/1.4 system-ui, sans-serif; display: flex; flex-direction: column; height: 100vh; }
  header { padding: .5rem .75rem; border-bottom: 1px solid #8884; display: flex; align-items: center; gap: .5rem; }
  header b { font-size: 1rem; }
  header .id { opacity: .6; font-family: ui-monospace, monospace; }
  header .status { margin-left: auto; font-size: .8rem; opacity: .7; }
  main { flex: 1 1 auto; min-height: 0; display: flex; }
  #main { flex: 1 1 auto; min-width: 0; overflow: auto; padding: .5rem; }
  #main iframe { width: 100%; height: 100%; border: 0; }
  #main pre { white-space: pre-wrap; word-break: break-word; }
  #feed { width: 20rem; max-width: 40%; border-left: 1px solid #8884; overflow: auto; }
  #feed h2 { font-size: .8rem; text-transform: uppercase; letter-spacing: .04em; opacity: .6; margin: .5rem .75rem; }
  #feed ul { list-style: none; margin: 0; padding: 0; }
  #feed li { padding: .5rem .75rem; border-bottom: 1px solid #8882; cursor: pointer; }
  #feed li:hover { background: #8881; }
  #feed li.active { background: #4a90e222; border-left: 3px solid #4a90e2; }
  #feed .sum { display: block; }
  #feed .meta { font-size: .75rem; opacity: .55; }
  .empty { opacity: .5; padding: 1rem; }
</style>
</head>
<body>
<header>
  <b>surface</b><span class="id" id="sid"></span>
  <span class="status" id="status">connecting…</span>
</header>
<main>
  <div id="main"><div class="empty">No item selected yet.</div></div>
  <aside id="feed"><h2>inbox</h2><ul id="list"></ul></aside>
</main>
<script>
(function () {
  var m = location.pathname.match(/\/s\/([^\/]+)/);
  var id = m ? decodeURIComponent(m[1]) : "";
  document.getElementById("sid").textContent = id;
  var state = { items: [], view: null };
  var mainEl = document.getElementById("main");
  var listEl = document.getElementById("list");
  var statusEl = document.getElementById("status");

  function esc(s) { var d = document.createElement("div"); d.textContent = s; return d.innerHTML; }

  function summary(it) {
    switch (it.type) {
      case "navigate": return "navigate → " + it.url;
      case "pdf": return "pdf → " + it.url;
      case "text": return "text: " + (it.body || "").slice(0, 60);
      case "link": return "link: " + (it.title ? it.title + " (" + it.url + ")" : it.url);
      case "html": return "html";
      case "script": return "script";
      default: return it.type || "item";
    }
  }

  // Render one display item into the main view.
  function renderMain(it) {
    mainEl.innerHTML = "";
    if (!it) { mainEl.innerHTML = '<div class="empty">No item selected yet.</div>'; return; }
    switch (it.type) {
      case "navigate":
      case "pdf": {
        // Third-party content: sandboxed iframe, no access to this origin.
        var f = document.createElement("iframe");
        f.setAttribute("sandbox", "allow-scripts allow-popups allow-forms");
        f.src = it.url;
        mainEl.appendChild(f);
        break;
      }
      case "text": {
        var pre = document.createElement("pre");
        pre.textContent = it.body || "";
        mainEl.appendChild(pre);
        break;
      }
      case "link": {
        var a = document.createElement("a");
        a.href = it.url; a.target = "_blank"; a.rel = "noopener noreferrer";
        a.textContent = it.title || it.url;
        mainEl.appendChild(a);
        break;
      }
      case "html": {
        // Same-origin, trusted (DESIGN §8): render our own markup directly.
        mainEl.innerHTML = it.markup || "";
        break;
      }
      case "script": {
        // Trusted scripting escape hatch: run in the surface page.
        var s = document.createElement("script");
        s.textContent = it.code || "";
        mainEl.appendChild(s);
        break;
      }
    }
  }

  function renderFeed() {
    listEl.innerHTML = "";
    if (!state.items.length) { listEl.innerHTML = '<li class="empty">Nothing pushed yet.</li>'; return; }
    for (var i = state.items.length - 1; i >= 0; i--) {
      var entry = state.items[i];
      var li = document.createElement("li");
      if (entry.id === state.view) li.className = "active";
      var sum = document.createElement("span"); sum.className = "sum"; sum.textContent = summary(entry.item);
      var meta = document.createElement("span"); meta.className = "meta";
      meta.textContent = new Date(entry.ts).toLocaleTimeString();
      li.appendChild(sum); li.appendChild(meta);
      (function (itemId) {
        li.addEventListener("click", function () {
          fetch("/s/" + encodeURIComponent(id) + "/view", {
            method: "POST", headers: { "content-type": "application/json" },
            body: JSON.stringify({ "item-id": itemId })
          });
        });
      })(entry.id);
      listEl.appendChild(li);
    }
  }

  function currentItem() {
    for (var i = 0; i < state.items.length; i++) if (state.items[i].id === state.view) return state.items[i].item;
    return null;
  }

  function onEvent(ev) {
    if (ev.kind === "snapshot") {
      state.items = ev.surface.items || [];
      state.view = ev.surface["current-view"] || null;
    } else if (ev.kind === "item") {
      state.items.push(ev.item);
      if (ev.item.promote) state.view = ev.item.id;
    } else if (ev.kind === "view") {
      state.view = ev["current-view"] || null;
    }
    renderMain(currentItem());
    renderFeed();
  }

  var src = new EventSource("/s/" + encodeURIComponent(id) + "/events");
  src.onopen = function () { statusEl.textContent = "live"; };
  src.onerror = function () { statusEl.textContent = "reconnecting…"; };
  src.onmessage = function (e) { try { onEvent(JSON.parse(e.data)); } catch (_) {} };
})();
</script>
</body>
</html>
"#;
