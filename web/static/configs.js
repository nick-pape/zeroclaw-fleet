// /configs viewer — list + read-only render of base.toml + fleet.yaml +
// per-claw overlays. Edit happens in git (when FLEET_REPO_BLOB_BASE is
// configured, every file gets a "View on git" link).

(function () {
  "use strict";

  const params = new URLSearchParams(window.location.search);
  const selected = params.get("k") || "";

  refreshList();

  if (selected) {
    loadConfig(selected);
  }

  async function refreshList() {
    let entries;
    try {
      const r = await fetch("/api/configs");
      if (!r.ok) throw new Error(`/api/configs ${r.status}`);
      entries = await r.json();
    } catch (e) {
      document.getElementById("config-list").innerHTML =
        `<div class="empty">Error: ${escapeHtml(e.message)}</div>`;
      return;
    }
    const nav = document.getElementById("config-list");
    if (!entries.length) {
      nav.innerHTML = '<div class="empty">No configs found.</div>';
      return;
    }
    nav.innerHTML = entries.map(e => {
      const active = e.key === selected ? "active" : "";
      const sizeKb = (e.size_bytes / 1024).toFixed(1);
      return `
        <a class="${active}" href="/configs?k=${encodeURIComponent(e.key)}">
          <span class="name">${escapeHtml(e.label)}</span>
          <span class="meta">${sizeKb} KB</span>
        </a>`;
    }).join("");
  }

  async function loadConfig(key) {
    let payload;
    try {
      const url = `/api/configs/${encodeURIComponent(key).replace(/%2F/gi, "/")}`;
      const r = await fetch(url);
      if (!r.ok) throw new Error(`${url} ${r.status}`);
      payload = await r.json();
    } catch (e) {
      document.getElementById("empty-hint").textContent = `Error: ${e.message}`;
      return;
    }
    document.getElementById("page-title").textContent = payload.path;
    document.getElementById("empty-hint").style.display = "none";
    const pre = document.getElementById("config-content");
    pre.style.display = "block";
    pre.textContent = payload.content;
    const link = document.getElementById("git-link");
    if (payload.git_url) {
      link.href = payload.git_url;
      link.style.display = "";
    } else {
      link.style.display = "none";
    }
    document.title = `${payload.path} — ZeroClaw Fleet`;
  }

  function escapeHtml(s) {
    return String(s).replace(/[&<>"']/g, c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
  }
})();
