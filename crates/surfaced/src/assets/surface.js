// The surface attachment page logic (DESIGN §10.1). Loaded same-origin on
// /s/{id}; reads the surface id from the URL, attaches over SSE (an initial
// snapshot then live updates), and renders the main view + inbox feed.
//
// Trust model (DESIGN §8): third-party navigate/pdf render inside a sandboxed
// iframe that cannot reach this origin; trusted same-origin html/script (only
// cluster-authenticated hosts can push) render/execute directly.
(function () {
  var m = location.pathname.match(/\/s\/([^\/]+)/);
  var id = m ? decodeURIComponent(m[1]) : "";
  document.getElementById("sid").textContent = id;
  var state = { items: [], view: null };
  var mainEl = document.getElementById("main");
  var listEl = document.getElementById("list");
  var statusEl = document.getElementById("status");

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
