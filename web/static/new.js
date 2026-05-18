// /tenants/new — provisioning form. Calls POST /api/tenants and surfaces
// the YAML snippets the operator must commit to finish wiring up.

(function () {
  "use strict";

  const form = document.getElementById("new-form");
  if (!form) return;

  form.addEventListener("submit", async (ev) => {
    ev.preventDefault();
    const fd = new FormData(form);
    const name = (fd.get("name") || "").trim();
    const display_name = (fd.get("display_name") || "").trim() || null;
    const mcp_scopes = (fd.get("mcp_scopes") || "")
      .split(",").map(s => s.trim()).filter(Boolean);
    const monthly_budget_usd = parseFloat(fd.get("monthly_budget_usd")) || 100.0;
    const uid_base = parseInt(fd.get("uid_base"), 10);
    const models = (fd.get("models") || "")
      .split(",").map(s => s.trim()).filter(Boolean);
    const default_color_theme = (fd.get("default_color_theme") || "").trim();
    const default_accent = (fd.get("default_accent") || "").trim();

    const errEl = document.getElementById("form-error");
    errEl.style.display = "none";

    if (mcp_scopes.length === 0) {
      errEl.textContent = "At least one MCP scope is required.";
      errEl.style.display = "block";
      return;
    }
    if (!Number.isFinite(uid_base)) {
      errEl.textContent = "uid_base must be a number.";
      errEl.style.display = "block";
      return;
    }

    const body = { name, mcp_scopes, monthly_budget_usd, uid_base, models };
    const btn = document.getElementById("provision-btn");
    btn.disabled = true;
    btn.textContent = "Provisioning…";

    let payload;
    try {
      const r = await fetch("/api/tenants", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(body),
      });
      payload = await r.json();
      if (!r.ok) throw new Error(payload.error || `HTTP ${r.status}`);
    } catch (e) {
      errEl.textContent = `Provisioning failed: ${e.message}`;
      errEl.style.display = "block";
      btn.disabled = false;
      btn.textContent = "Provision";
      return;
    }

    // Build the overlay + fleet.yaml snippets locally so the user can paste them.
    const overlaySnippet = buildOverlaySnippet({
      name, display_name, mcp_scopes, default_color_theme, default_accent,
    });

    document.getElementById("provision-result-title").textContent =
      `${display_name || name} provisioned`;
    document.getElementById("provision-steps").innerHTML =
      (payload.steps_completed || []).map(s => `<li>${escapeHtml(s)}</li>`).join("");
    document.getElementById("provision-hub-snippet").textContent =
      payload.hub_policy_snippet || "(no snippet returned)";
    document.getElementById("provision-overlay-snippet").textContent = overlaySnippet;
    document.getElementById("provision-result").showModal();

    btn.disabled = false;
    btn.textContent = "Provision";
  });

  document.getElementById("provision-result-close")?.addEventListener("click", () => {
    document.getElementById("provision-result").close();
  });

  function buildOverlaySnippet({ name, display_name, mcp_scopes, default_color_theme, default_accent }) {
    const dn = display_name || name;
    const dot = default_color_theme ? `default_color_theme = "${default_color_theme}"\n` : "";
    const da = default_accent ? `default_accent = "${default_accent}"\n` : "";
    const scopeList = mcp_scopes.map(s => `"${s}"`).join(", ");
    return `# 1. Add this entry to fleet.yaml under \`claws:\` :
#     - ${name}
#
# 2. Create fleet/claws/${name}.toml with:

[_fleet]
name = "${name}"
mcp_scopes = [${scopeList}]
import = false

[providers]
fallback = "custom:https://ai.pape.house/v1"

[branding]
display_name = "${dn}"
${dot}${da}
[autonomy]
# TODO: enumerate the upstream tool names for each scope above and
# paste them here, one per line, prefixed with "papehouse__<scope>_".
# Until then this tenant's auto_approve is empty — it will hit the
# medium-risk-required-approval gate on every MCP call.
auto_approve = ["tool_search"]
`;
  }

  function escapeHtml(s) {
    return String(s).replace(/[&<>"']/g, c => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
  }
})();
