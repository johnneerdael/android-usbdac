// usbdac documentation site: theme choice, search, copy buttons and the "On this page" highlight.
// Every page works without this script; it only adds conveniences.
(function () {
  "use strict";
  var root = document.documentElement;
  root.classList.remove("no-js");

  // ---- Theme: System -> Dark -> Light, remembered per browser --------------------------
  var KEY = "usbdac-theme";
  var button = document.querySelector(".theme-btn");
  function stored() { try { return localStorage.getItem(KEY) || "system"; } catch (e) { return "system"; } }
  function apply(mode) {
    if (mode === "dark" || mode === "light") root.setAttribute("data-theme", mode); else root.removeAttribute("data-theme");
    if (button) {
      button.setAttribute("data-mode", mode);
      var label = "Theme: " + mode.charAt(0).toUpperCase() + mode.slice(1);
      button.setAttribute("aria-label", label + " (change)");
      button.title = label;
    }
  }
  apply(stored());
  if (button) button.addEventListener("click", function () {
    var order = ["system", "dark", "light"];
    var next = order[(order.indexOf(stored()) + 1) % order.length];
    try { localStorage.setItem(KEY, next); } catch (e) { /* private mode */ }
    apply(next);
  });

  // ---- Small screens: the guide menu starts folded -----------------------------------
  var menu = document.querySelector(".sidebar details.menu");
  if (menu && window.matchMedia("(max-width: 900px)").matches) menu.open = false;

  // ---- Copy buttons on code blocks ------------------------------------------------------
  document.querySelectorAll("article pre").forEach(function (pre) {
    var copy = document.createElement("button");
    copy.type = "button"; copy.className = "copy"; copy.textContent = "Copy";
    copy.addEventListener("click", function () {
      var code = pre.querySelector("code") || pre;
      navigator.clipboard.writeText(code.innerText.replace(/\n$/, "")).then(function () {
        copy.textContent = "Copied"; setTimeout(function () { copy.textContent = "Copy"; }, 1400);
      });
    });
    pre.appendChild(copy);
  });

  // ---- "On this page": highlight the section in view -----------------------------------
  var tocLinks = Array.prototype.slice.call(document.querySelectorAll(".toc a"));
  if (tocLinks.length && "IntersectionObserver" in window) {
    var byId = {};
    tocLinks.forEach(function (a) { byId[decodeURIComponent(a.hash.slice(1))] = a; });
    var visible = {};
    var observer = new IntersectionObserver(function (entries) {
      entries.forEach(function (e) { visible[e.target.id] = e.isIntersecting; });
      var first = Object.keys(byId).filter(function (id) { return visible[id]; })[0];
      if (first) { tocLinks.forEach(function (a) { a.classList.toggle("active", a === byId[first]); }); }
    }, { rootMargin: "-72px 0px -65% 0px" });
    Object.keys(byId).forEach(function (id) { var el = document.getElementById(id); if (el) observer.observe(el); });
  }

  // ---- Search over every page, section and heading --------------------------------------
  var input = document.getElementById("search-input");
  var panel = document.getElementById("search-results");
  if (!input || !panel) return;
  // The index (every heading and the start of its section) loads on first use.
  var index = [], loading = false;
  function loadIndex() {
    if (loading) return;
    loading = true;
    var script = document.createElement("script");
    script.src = input.getAttribute("data-index");
    script.onload = function () { index = window.USBDAC_SEARCH || []; render(); };
    document.head.appendChild(script);
  }
  var base = input.getAttribute("data-base") || "";
  var selected = -1;

  function escapeHtml(s) { return s.replace(/[&<>"]/g, function (c) { return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]; }); }
  function highlight(text, terms) {
    var out = escapeHtml(text);
    terms.forEach(function (t) {
      if (t.length < 2) return;
      out = out.replace(new RegExp("(" + t.replace(/[.*+?^${}()|[\]\\]/g, "\\$&") + ")", "ig"), "<mark>$1</mark>");
    });
    return out;
  }
  function search(query) {
    var terms = query.toLowerCase().split(/\s+/).filter(Boolean);
    if (!terms.length) return [];
    var phrase = terms.length > 1 ? terms.join(" ") : null;
    var results = [];
    index.forEach(function (item) {
      var title = item.t.toLowerCase(), page = item.p.toLowerCase(), text = item.x.toLowerCase(), body = item.b || "";
      var score = 0, matched = 0;
      terms.forEach(function (t) {
        var s = 0;
        if (title.indexOf(t) === 0) s += 12; else if (title.indexOf(t) > -1) s += 8;
        if (page.indexOf(t) > -1) s += 3;
        if (text.indexOf(t) > -1) s += 3; else if (body.indexOf(t) > -1) s += 1;
        if (s) { matched++; score += s; }
      });
      if (!matched) return;
      if (matched === terms.length) score += 20 * terms.length;  // every word present ranks first
      if (phrase && (title.indexOf(phrase) > -1 || text.indexOf(phrase) > -1 || body.indexOf(phrase) > -1)) score += 15;
      if (item.l === 1) score += 2;
      results.push({ item: item, score: score, full: matched === terms.length });
    });
    if (results.some(function (r) { return r.full; })) results = results.filter(function (r) { return r.full; });
    results.sort(function (a, b) { return b.score - a.score; });
    return results.slice(0, 12).map(function (r) { return r.item; });
  }
  function render() {
    var query = input.value.trim();
    if (!query) { panel.classList.remove("open"); input.setAttribute("aria-expanded", "false"); return; }
    if (!index.length) { panel.innerHTML = '<div class="empty">Loading the search index…</div>'; panel.classList.add("open"); return; }
    var found = search(query), terms = query.toLowerCase().split(/\s+/);
    selected = found.length ? 0 : -1;
    panel.innerHTML = found.length ? found.map(function (item, i) {
      return '<a role="option" href="' + base + item.u + '" aria-selected="' + (i === 0) + '">' +
        '<div class="r-page">' + escapeHtml(item.s + " · " + item.p) + "</div>" +
        '<div class="r-title">' + highlight(item.t, terms) + "</div>" +
        (item.x ? '<div class="r-text">' + highlight(item.x, terms) + "</div>" : "") + "</a>";
    }).join("") : '<div class="empty">Nothing matches “' + escapeHtml(query) + "”. Try a class, error code or term such as <code>OutputSelector</code>, <code>DoP</code> or <code>feedback</code>.</div>";
    panel.classList.add("open"); input.setAttribute("aria-expanded", "true");
  }
  function move(step) {
    var links = panel.querySelectorAll("a");
    if (!links.length) return;
    selected = (selected + step + links.length) % links.length;
    links.forEach(function (a, i) { a.setAttribute("aria-selected", String(i === selected)); });
    links[selected].scrollIntoView({ block: "nearest" });
  }
  input.addEventListener("input", function () { loadIndex(); render(); });
  input.addEventListener("focus", function () { loadIndex(); if (input.value.trim()) render(); });
  input.addEventListener("keydown", function (e) {
    if (e.key === "ArrowDown") { e.preventDefault(); move(1); }
    else if (e.key === "ArrowUp") { e.preventDefault(); move(-1); }
    else if (e.key === "Enter") { var a = panel.querySelectorAll("a")[selected]; if (a) { e.preventDefault(); location.href = a.href; } }
    else if (e.key === "Escape") { input.value = ""; render(); input.blur(); }
  });
  document.addEventListener("click", function (e) { if (!e.target.closest(".search")) panel.classList.remove("open"); });
  document.addEventListener("keydown", function (e) {
    if ((e.key === "/" || (e.key === "k" && (e.metaKey || e.ctrlKey))) && document.activeElement !== input && !/INPUT|TEXTAREA/.test(document.activeElement.tagName)) {
      e.preventDefault(); input.focus(); input.select();
    }
  });
})();
