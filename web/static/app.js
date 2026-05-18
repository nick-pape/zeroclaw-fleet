// ZeroClaw Fleet — single-page UI.
//
// Two layouts, decided by which HTML the server returned:
//   index.html — fleet dashboard at `/`
//   claw.html  — per-claw chrome at `/claws/:name`, iframes the claw's
//                native dashboard at `https://<name>.<claw_suffix>/`
//
// Both share the sidebar (list of claws + fleet cost rollup) which polls
// /api/claws + /api/cost on a 5s cadence.

(function () {
  "use strict";

  const REFRESH_MS = 5000;
  const claw = currentClawFromPath();

  if (claw) {
    setupClawView(claw);
  } else {
    setupDashboardView();
  }

  refresh();
  setInterval(refresh, REFRESH_MS);

  document.getElementById("refresh")?.addEventListener("click", refresh);

  if (claw) {
    document.getElementById("restart")?.addEventListener("click", async () => {
      if (!confirm(`Restart ${claw}?`)) return;
      const r = await fetch(`/api/claws/${encodeURIComponent(claw)}/restart`, { method: "POST" });
      if (!r.ok) alert(`Restart failed: ${r.status}`);
    });
    setupDeleteModal(claw);
  }

  function setupDeleteModal(name) {
    const modal = document.getElementById("delete-modal");
    const result = document.getElementById("delete-result");
    if (!modal || !result) return;

    document.getElementById("delete-name").textContent = name;
    document.getElementById("delete-container").textContent = `claw-${name}`;
    document.getElementById("delete-volume").textContent = `claw-data-${name}`;
    document.getElementById("delete-authentik").textContent = `mcp-${name}`;
    document.getElementById("delete-bao").textContent = `secret/services/${name}/{litellm,papehouse,auth}`;
    document.getElementById("delete-typehint").textContent = name;

    const openBtn = document.getElementById("delete");
    const cancelBtn = document.getElementById("delete-cancel");
    const confirmInput = document.getElementById("delete-confirm");
    const confirmBtn = document.getElementById("delete-confirm-btn");
    const closeResult = document.getElementById("delete-result-close");

    openBtn.addEventListener("click", () => {
      confirmInput.value = "";
      confirmBtn.disabled = true;
      modal.showModal();
      confirmInput.focus();
    });
    cancelBtn.addEventListener("click", () => modal.close());
    closeResult.addEventListener("click", () => result.close());
    confirmInput.addEventListener("input", () => {
      confirmBtn.disabled = confirmInput.value !== name;
    });

    document.getElementById("delete-form").addEventListener("submit", async (ev) => {
      if (confirmInput.value !== name) { ev.preventDefault(); return; }
      ev.preventDefault();
      confirmBtn.disabled = true;
      confirmBtn.textContent = "Deleting…";

      // Best-effort scope list — read from the overlay so we clean up
      // bao JWT roles. If we can't get it, send empty (jwt_role_delete
      // absorbs 404s as warnings).
      let scopes = [];
      try {
        const r = await fetch(`/api/configs/claws/${encodeURIComponent(name)}`);
        if (r.ok) {
          const cfg = await r.json();
          const m = (cfg.content || "").match(/mcp_scopes\s*=\s*\[([^\]]+)\]/);
          if (m) {
            scopes = m[1].split(",").map(s => s.trim().replace(/['"]/g, "")).filter(Boolean);
          }
        }
      } catch (e) { /* tolerable */ }

      let out;
      try {
        const r = await fetch(`/api/tenants/${encodeURIComponent(name)}`, {
          method: "DELETE",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ confirm: name, purge_container: true, mcp_scopes: scopes }),
        });
        out = await r.json();
        if (!r.ok && r.status !== 206) {
          throw new Error(out.error || `HTTP ${r.status}`);
        }
      } catch (e) {
        alert(`Delete failed: ${e.message}`);
        confirmBtn.disabled = false;
        confirmBtn.textContent = "Delete";
        return;
      }

      modal.close();
      document.getElementById("delete-steps").innerHTML =
        (out.steps_completed || []).map(s => `<li>${escapeHtml(s)}</li>`).join("") || "<li><em>(none)</em></li>";
      document.getElementById("delete-warnings").innerHTML =
        (out.warnings || []).length
          ? out.warnings.map(s => `<li>${escapeHtml(s)}</li>`).join("")
          : "<li><em>(none)</em></li>";
      const snippet =
        (out.hub_policy_removal_snippet || "") +
        "\n" +
        (out.fleet_manifest_removal_snippet || "");
      document.getElementById("delete-snippet").textContent = snippet.trim() || "(no manual steps required)";
      result.showModal();
      confirmBtn.disabled = false;
      confirmBtn.textContent = "Delete";
    });
  }

  // ---------------------------------------------------------------

  function currentClawFromPath() {
    const m = window.location.pathname.match(/^\/claws\/([a-z0-9-]+)\/?$/);
    return m ? m[1] : null;
  }

  function setupDashboardView() {
    document.getElementById("page-title").textContent = "Dashboard";
  }

  function setupClawView(name) {
    document.getElementById("page-title").textContent = name;
    // Once the claw list lands, swap in the friendly display_name.
    fetch("/api/claws").then(r => r.ok ? r.json() : []).then(list => {
      const entry = (list || []).find(e => e.name === name);
      if (entry && entry.display_name && entry.display_name !== name) {
        document.getElementById("page-title").textContent = `${entry.display_name} (${name})`;
        document.title = `${entry.display_name} — ZeroClaw Fleet`;
      }
    });
    // Iframe loads the claw's native dashboard at its own subdomain so the
    // SPA's relative /api/* paths work without rewriting. Land on the
    // chat surface (/agent) directly — the dashboard's other tabs are
    // reachable from the in-SPA navigation.
    fetch("/api/config").then(r => r.ok ? r.json() : null).then(cfg => {
      const suffix = (cfg && cfg.claw_suffix) || guessSuffixFromHost();
      const url = `${window.location.protocol}//${name}.${suffix}/agent`;
      const iframe = document.getElementById("claw-iframe");
      iframe.src = url;
      document.getElementById("open-new-tab").href = url;
    });
  }

  // Fallback used if /api/config is unreachable: assume the fleet host is
  // `claws.<rest>` and the claw suffix is `claw.<rest>`.
  function guessSuffixFromHost() {
    const host = window.location.hostname;
    const m = host.match(/^claws\.(.+)$/);
    return m ? `claw.${m[1]}` : host;
  }

  async function refresh() {
    await Promise.all([refreshClawList(), refreshRollup()]);
  }

  async function refreshClawList() {
    let list;
    try {
      const r = await fetch("/api/claws");
      if (!r.ok) throw new Error(`/api/claws ${r.status}`);
      list = await r.json();
    } catch (e) {
      renderListError(e.message);
      return;
    }
    const nav = document.getElementById("claw-list");
    if (!list.length) {
      nav.innerHTML = '<div class="empty">No claws in manifest.</div>';
      return;
    }
    nav.innerHTML = list.map(entry => {
      const health = (entry.status && entry.status.health) || "missing";
      const active = entry.name === claw ? "active" : "";
      const cost = entry.status && entry.status.daily_cost_usd != null
        ? `$${entry.status.daily_cost_usd.toFixed(2)}/day`
        : "";
      // Use [branding] display_name when present (e.g. "H-E-Buddy"); fall
      // back to the kebab identifier (e.g. "grocery").
      const label = entry.display_name || entry.name;
      const slug = label !== entry.name ? `<span class="slug">${escapeHtml(entry.name)}</span>` : "";
      return `
        <a class="${active}" href="/claws/${encodeURIComponent(entry.name)}">
          <span class="health health-${health}"></span>
          <span class="name">${escapeHtml(label)}</span> ${slug}
          <span class="meta">${escapeHtml(health)}${cost ? " · " + escapeHtml(cost) : ""}</span>
        </a>`;
    }).join("");
  }

  async function refreshRollup() {
    try {
      const r = await fetch("/api/cost");
      if (!r.ok) throw new Error(`/api/cost ${r.status}`);
      const c = await r.json();
      setText("fleet-cost", `$${(c.daily_cost_usd || 0).toFixed(2)} today / $${(c.monthly_cost_usd || 0).toFixed(2)} mo`);
      setText("r-claws", c.claws ?? "—");
      setText("r-stale", c.stale ?? "—");
      setText("r-session", money(c.session_cost_usd));
      setText("r-daily", money(c.daily_cost_usd));
      setText("r-monthly", money(c.monthly_cost_usd));
      setText("r-tokens", (c.total_tokens || 0).toLocaleString());
      setText("r-requests", (c.request_count || 0).toLocaleString());
    } catch (e) {
      setText("fleet-cost", "(cost rollup unavailable)");
    }
  }

  function money(v) { return v == null ? "—" : `$${v.toFixed(2)}`; }
  function setText(id, v) { const e = document.getElementById(id); if (e) e.textContent = v; }
  function escapeHtml(s) {
    return String(s).replace(/[&<>"']/g, c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
  }

  function renderListError(msg) {
    document.getElementById("claw-list").innerHTML =
      `<div class="empty">Error loading claws: ${escapeHtml(msg)}</div>`;
  }
})();
