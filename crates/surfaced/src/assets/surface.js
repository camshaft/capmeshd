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
  // A token-protected surface (DESIGN §10.1) is opened at …/s/<id>?token=<t>;
  // carry that token onto the SSE + view subrequests so they authorize too.
  var token = new URLSearchParams(location.search).get("token");
  var tq = token ? ("?token=" + encodeURIComponent(token)) : "";
  var state = { items: [], view: null };
  var mainEl = document.getElementById("main");
  var listEl = document.getElementById("list");
  var statusEl = document.getElementById("status");
  var feedEl = document.getElementById("feed");
  var backdropEl = document.getElementById("backdrop");
  var menuEl = document.getElementById("menu");

  // The inbox feed is an off-canvas drawer on narrow (phone) screens; the
  // hamburger toggles it. On wide screens CSS shows it as a persistent sidebar,
  // so these class/backdrop toggles are visually inert there.
  function openFeed() {
    feedEl.classList.add("open"); feedEl.setAttribute("aria-hidden", "false");
    menuEl.setAttribute("aria-expanded", "true"); backdropEl.hidden = false;
  }
  function closeFeed() {
    feedEl.classList.remove("open"); feedEl.setAttribute("aria-hidden", "true");
    menuEl.setAttribute("aria-expanded", "false"); backdropEl.hidden = true;
  }
  menuEl.addEventListener("click", function () {
    if (feedEl.classList.contains("open")) closeFeed(); else openFeed();
  });
  backdropEl.addEventListener("click", closeFeed);

  // Clear all items on this surface (empties the inbox; SSE pushes the update).
  var clearEl = document.getElementById("clear");
  clearEl.addEventListener("click", function () {
    if (!window.confirm("Clear all items on this surface?")) return;
    fetch("s/" + encodeURIComponent(id) + "/clear" + tq, { method: "POST" });
  });

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

  // A full-bleed iframe "stage" that fills the main view. ALL rich item content
  // renders inside one of these — never injected into this page's DOM — so an
  // item can never corrupt the surface's own chrome (header/drawer/feed), leak
  // <style> into it, or break layout with unbalanced markup.
  function stage(sandbox) {
    var f = document.createElement("iframe");
    f.className = "stage";
    // `sandbox` is always set; the flags differ by item kind (see callers).
    f.setAttribute("sandbox", sandbox);
    // Referrer hygiene for third-party embeds.
    f.setAttribute("referrerpolicy", "no-referrer");
    return f;
  }

  // A small "open in new tab" anchor (an escape hatch over an embedded frame).
  function openLink(url, label) {
    var a = document.createElement("a");
    a.className = "openlink";
    a.href = url; a.target = "_blank"; a.rel = "noopener noreferrer";
    a.textContent = label;
    return a;
  }

  // A centered panel shown when content can't be embedded (e.g. mixed content):
  // an explanation plus a prominent open-in-new-tab link.
  function externalFallback(url, message) {
    var d = document.createElement("div");
    d.className = "fallback";
    var p = document.createElement("p");
    p.textContent = message;
    d.appendChild(p);
    d.appendChild(openLink(url, url));
    return d;
  }

  // Render one display item into the main view.
  function renderMain(it) {
    mainEl.innerHTML = "";
    if (!it) { mainEl.innerHTML = '<div class="empty">No item selected yet.</div>'; return; }
    switch (it.type) {
      case "navigate":
      case "pdf": {
        var url = it.url || "";
        // A browser silently refuses to load an http:// URL inside an https page
        // (mixed content). Rather than show a doomed blank frame, surface it.
        if (location.protocol === "https:" && /^http:\/\//i.test(url)) {
          mainEl.appendChild(externalFallback(url,
            "This item's URL is http:// and a secure (https) page will not load it inline (mixed content). Open it directly:"));
          break;
        }
        // A third-party URL (site or PDF). It keeps its OWN origin (allow-same-origin
        // is safe here because the content is cross-origin — it still cannot touch
        // this page), which is what lets the browser's PDF viewer render inline and
        // real sites work; allow-downloads covers a "save" from the viewer.
        var wrap = document.createElement("div");
        wrap.className = "framewrap";
        var f = stage("allow-scripts allow-same-origin allow-popups allow-forms allow-downloads");
        f.src = url;
        wrap.appendChild(f);
        wrap.appendChild(openLink(url, "open ↗")); // always an escape hatch
        mainEl.appendChild(wrap);
        break;
      }
      case "html": {
        // Trusted markup, but rendered in an ISOLATED opaque-origin iframe (no
        // allow-same-origin) so it is fully contained: its scripts/styles run only
        // inside the stage and cannot reach or corrupt the surface UI.
        var fh = stage("allow-scripts allow-popups allow-forms");
        fh.srcdoc = it.markup || "";
        mainEl.appendChild(fh);
        break;
      }
      case "script": {
        // Trusted JS, run inside the isolated stage (not this page). CDATA-style
        // guard keeps a stray "</script>" in the code from escaping the element.
        var code = String(it.code || "").replace(/<\/(script)/gi, "<\\/$1");
        var fs = stage("allow-scripts allow-popups allow-forms");
        fs.srcdoc = '<!doctype html><meta charset="utf-8"><body><script>' + code + "<\/script>";
        mainEl.appendChild(fs);
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
          // Relative URL: resolves against <base href>, so it works under a
          // reverse-proxy sub-path (e.g. /surfaced/) as well as at the root.
          fetch("s/" + encodeURIComponent(id) + "/view" + tq, {
            method: "POST", headers: { "content-type": "application/json" },
            body: JSON.stringify({ "item-id": itemId })
          });
          // Selecting an item closes the drawer so the item shows centered.
          closeFeed();
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

  // Relative URL (resolves against <base href>) — reverse-proxy sub-path safe.
  var src = new EventSource("s/" + encodeURIComponent(id) + "/events" + tq);
  src.onopen = function () { statusEl.textContent = "live"; };
  src.onerror = function () { statusEl.textContent = "reconnecting…"; };
  src.onmessage = function (e) { try { onEvent(JSON.parse(e.data)); } catch (_) {} };
})();
