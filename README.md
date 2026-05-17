# zeroclaw-fleet

Orchestrator for running multiple isolated [ZeroClaw](https://github.com/zeroclaw-labs/zeroclaw)
instances under one host.

Each *claw* is a separate ZeroClaw container with its own identity, MCP credentials,
cost ledger, and dashboard. The orchestrator owns:

- **Manifest** — `fleet.yaml` + per-claw overlay (`claws/<name>.toml`); shared `base.toml`
  is the single source of truth for settings that apply to every claw.
- **Lifecycle** — `docker create` / `start` / `stop` / `restart` per claw via the
  Docker socket.
- **Provisioning** — tenant create automates the supporting wiring (Authentik OAuth2
  provider, secret-store entries, MCP hub identity block, LiteLLM virtual key).
- **Auth-translated reverse proxy** — user authenticates once at the edge
  (Authentik forward_auth), orchestrator translates that into a per-claw bearer
  injected into HTTP requests and WebSocket subprotocols.
- **Cost roll-up** — periodic poll of each claw's `GET /api/cost`, surfaced in a
  single fleet dashboard.
- **Web UI** — sidebar of claws + iframe-embedded native claw dashboards.

## Status

Pre-alpha. Skeleton compiles; modules are stubs.

## License

MIT OR Apache-2.0 — matches the upstream ZeroClaw license.
