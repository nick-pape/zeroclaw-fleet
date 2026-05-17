//! Reverse-proxy a request addressed to a per-claw subdomain
//! (e.g. `https://grocery.claw.example.com/api/cost`) at the orchestrator's
//! single Caddy upstream into the matching claw's gateway on the internal
//! docker network (`http://claw-grocery:42617/api/cost`).
//!
//! Two paths:
//!   * **HTTP** — forwarded with `reqwest`. All headers except `Host` and
//!     hop-by-hop are passed through.
//!   * **WebSocket** — upgraded on the client side via axum's
//!     `WebSocketUpgrade`, dialed on the upstream side via
//!     `tokio_tungstenite::client_async`, then spliced bidirectionally.
//!
//! No bearer injection: the orchestrator-to-claw network is trusted by the
//! `require_pairing = false` posture chosen for this deployment. If a
//! future deployment tightens that to `require_pairing = true`, wire the
//! per-claw bearer in at [`forward_http`] (as `Authorization: Bearer ...`)
//! and at the WS dial (as `Sec-WebSocket-Protocol: bearer.<token>` per
//! `crates/zeroclaw-gateway/src/ws.rs:133-167`).
//!
//! Host parsing: a request whose Host header matches
//! `<claw>.<claw_suffix>` (e.g. `<claw>.claw.example.com`) targets that
//! claw; the orchestrator's own UI lives at `fleet_host`.

use anyhow::Result;
use axum::body::{Body, Bytes};
use axum::extract::ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio_tungstenite::tungstenite::Message as TungMessage;

/// Hop-by-hop headers (RFC 7230 §6.1) that MUST NOT be forwarded by a proxy.
/// `Host` is dropped because reqwest sets its own based on the upstream URI.
const STRIP_HEADERS: &[&str] = &[
    "host",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

/// Configuration handed to the proxy so it can map a request to a claw
/// upstream. Cheap to clone (just two `Arc<str>` and a port).
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Fully-qualified host of the fleet UI (e.g. `"claws.example.com"`).
    pub fleet_host: Arc<str>,
    /// Suffix that follows a claw's name in its per-claw host (e.g.
    /// `"claw.example.com"`). A request whose Host header ends in
    /// `".<claw_suffix>"` is treated as targeting the matching claw.
    pub claw_suffix: Arc<str>,
    /// Upstream port on the claw container (always 42617 for ZeroClaw).
    pub claw_port: u16,
}

impl ProxyConfig {
    /// Classify a `Host` header value.
    pub fn classify(&self, host_header: &str) -> HostKind {
        let host = host_header.split(':').next().unwrap_or(host_header);
        if host.eq_ignore_ascii_case(&self.fleet_host) {
            return HostKind::FleetUi;
        }
        // <claw>.<claw_suffix> → claw
        let dot_suffix = format!(".{}", self.claw_suffix);
        if let Some(stripped) = host
            .to_ascii_lowercase()
            .strip_suffix(&dot_suffix.to_ascii_lowercase())
            .map(|s| s.to_string())
            && !stripped.is_empty()
            // Single kebab-case label only — no dots (rejects
            // `evil.grocery.claw.example.com`), no underscores, no
            // upper-case.
            && stripped.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return HostKind::Claw(stripped);
        }
        HostKind::Unknown
    }

    /// Build the upstream base URL for a claw container.
    /// Container name follows [`crate::driver::container_name`].
    pub fn upstream_base(&self, claw: &str) -> String {
        format!("http://claw-{claw}:{}", self.claw_port)
    }
}

/// What the Host header points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKind {
    /// Fleet orchestrator UI / API.
    FleetUi,
    /// A per-claw subdomain — proxy to the named claw.
    Claw(String),
    /// Host did not match any known pattern; should 404.
    Unknown,
}

/// Forward an HTTP request to the named claw.
///
/// `incoming_uri` should be the request's path+query as it arrived; the
/// host part is replaced with the claw's docker network address.
pub async fn forward_http(
    client: &reqwest::Client,
    cfg: &ProxyConfig,
    claw: &str,
    method: Method,
    incoming_uri: &Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let path_and_query = incoming_uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let upstream = format!("{}{path_and_query}", cfg.upstream_base(claw));

    let mut builder = client.request(method, &upstream).body(body);
    for (name, value) in &headers {
        if STRIP_HEADERS.iter().any(|h| name.as_str().eq_ignore_ascii_case(h)) {
            continue;
        }
        builder = builder.header(name, value);
    }

    let upstream_resp = match builder.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(claw = claw, error = %e, "upstream proxy error");
            return (StatusCode::BAD_GATEWAY, format!("upstream error: {e}")).into_response();
        }
    };

    let status = StatusCode::from_u16(upstream_resp.status().as_u16())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    let upstream_headers = upstream_resp.headers().clone();
    let bytes = match upstream_resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(claw = claw, error = %e, "reading upstream body failed");
            return (StatusCode::BAD_GATEWAY, format!("upstream body: {e}")).into_response();
        }
    };

    let mut resp = Response::builder().status(status);
    let resp_headers = resp.headers_mut().expect("fresh builder always has headers");
    for (name, value) in upstream_headers.iter() {
        if STRIP_HEADERS.iter().any(|h| name.as_str().eq_ignore_ascii_case(h)) {
            continue;
        }
        let Ok(hn) = HeaderName::from_bytes(name.as_str().as_bytes()) else { continue };
        let Ok(hv) = HeaderValue::from_bytes(value.as_bytes()) else { continue };
        resp_headers.insert(hn, hv);
    }
    resp.body(Body::from(bytes)).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "building response failed");
        Response::builder().status(StatusCode::BAD_GATEWAY).body(Body::empty()).unwrap()
    })
}

/// Upgrade an incoming WS connection and splice it bidirectionally with
/// an upstream WS connection to the named claw.
pub async fn forward_ws(
    upgrade: WebSocketUpgrade,
    cfg: ProxyConfig,
    claw: String,
    path_and_query: String,
) -> Response<Body> {
    upgrade.on_upgrade(move |socket| async move {
        if let Err(e) = pump_ws(cfg, claw, path_and_query, socket).await {
            tracing::warn!(error = %e, "ws proxy ended with error");
        }
    })
}

async fn pump_ws(
    cfg: ProxyConfig,
    claw: String,
    path_and_query: String,
    client: WebSocket,
) -> Result<()> {
    let upstream_url = format!("ws://claw-{claw}:{}{path_and_query}", cfg.claw_port);
    let (upstream, _resp) = tokio_tungstenite::connect_async(&upstream_url).await?;

    let (mut client_tx, mut client_rx) = client.split();
    let (mut up_tx, mut up_rx) = upstream.split();

    let client_to_up = async {
        while let Some(msg) = client_rx.next().await {
            let msg = msg?;
            let translated = axum_to_tungstenite(msg);
            if let Some(m) = translated {
                up_tx.send(m).await?;
            }
        }
        anyhow::Ok(())
    };

    let up_to_client = async {
        while let Some(msg) = up_rx.next().await {
            let msg = msg?;
            let translated = tungstenite_to_axum(msg);
            if let Some(m) = translated {
                client_tx.send(m).await?;
            }
        }
        anyhow::Ok(())
    };

    tokio::select! {
        r = client_to_up => r?,
        r = up_to_client => r?,
    }
    Ok(())
}

fn axum_to_tungstenite(m: AxumMessage) -> Option<TungMessage> {
    match m {
        AxumMessage::Text(t) => Some(TungMessage::Text(t.as_str().into())),
        AxumMessage::Binary(b) => Some(TungMessage::Binary(b.into())),
        AxumMessage::Ping(b) => Some(TungMessage::Ping(b.into())),
        AxumMessage::Pong(b) => Some(TungMessage::Pong(b.into())),
        AxumMessage::Close(_) => Some(TungMessage::Close(None)),
    }
}

fn tungstenite_to_axum(m: TungMessage) -> Option<AxumMessage> {
    match m {
        TungMessage::Text(t) => Some(AxumMessage::Text(t.as_str().into())),
        TungMessage::Binary(b) => Some(AxumMessage::Binary(b.into())),
        TungMessage::Ping(b) => Some(AxumMessage::Ping(b.into())),
        TungMessage::Pong(b) => Some(AxumMessage::Pong(b.into())),
        TungMessage::Close(_) => Some(AxumMessage::Close(None)),
        TungMessage::Frame(_) => None, // never observed via the high-level API
    }
}

/// Top-level entry point used by the axum router's fallback.
/// Inspects the Host header and either dispatches to the fleet router
/// (caller handles) or proxies to a claw.
pub async fn maybe_proxy(
    cfg: &ProxyConfig,
    client: &reqwest::Client,
    req: Request<Body>,
) -> Result<Response<Body>, Request<Body>> {
    let host = req
        .headers()
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");

    match cfg.classify(host) {
        HostKind::FleetUi => Err(req),
        HostKind::Unknown => Ok((StatusCode::NOT_FOUND, "unknown host").into_response()),
        HostKind::Claw(claw) => {
            // WS upgrade?
            let is_upgrade = req
                .headers()
                .get("upgrade")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.eq_ignore_ascii_case("websocket"))
                .unwrap_or(false);

            if is_upgrade {
                let path_and_query = req
                    .uri()
                    .path_and_query()
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "/".into());
                let (parts, body) = req.into_parts();
                // Reassemble enough of the request for axum's extractor.
                let req = Request::from_parts(parts, body);
                let upgrade = match WebSocketUpgrade::from_request(req, &()).await {
                    Ok(u) => u,
                    Err(e) => {
                        tracing::warn!(error = %e, "WS upgrade extraction failed");
                        return Ok((StatusCode::BAD_REQUEST, "ws upgrade failed").into_response());
                    }
                };
                return Ok(forward_ws(upgrade, cfg.clone(), claw, path_and_query).await);
            }

            let (parts, body) = req.into_parts();
            let bytes = match axum::body::to_bytes(body, usize::MAX).await {
                Ok(b) => b,
                Err(e) => {
                    return Ok((StatusCode::BAD_REQUEST, format!("read body: {e}")).into_response());
                }
            };
            Ok(forward_http(client, cfg, &claw, parts.method, &parts.uri, parts.headers, bytes).await)
        }
    }
}

// axum extractor needs trait import in scope for from_request.
use axum::extract::FromRequest;

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ProxyConfig {
        ProxyConfig {
            fleet_host: Arc::from("claws.example.com"),
            claw_suffix: Arc::from("claw.example.com"),
            claw_port: 42617,
        }
    }

    #[test]
    fn classify_fleet_root_is_fleet_ui() {
        assert_eq!(cfg().classify("claws.example.com"), HostKind::FleetUi);
        assert_eq!(cfg().classify("CLAWS.EXAMPLE.COM"), HostKind::FleetUi);
        assert_eq!(cfg().classify("claws.example.com:443"), HostKind::FleetUi);
    }

    #[test]
    fn classify_claw_subdomain_extracts_name() {
        assert_eq!(
            cfg().classify("grocery.claw.example.com"),
            HostKind::Claw("grocery".to_string())
        );
        assert_eq!(
            cfg().classify("alfred.claw.example.com:443"),
            HostKind::Claw("alfred".to_string())
        );
        assert_eq!(
            cfg().classify("h-e-buddy.claw.example.com"),
            HostKind::Claw("h-e-buddy".to_string())
        );
    }

    #[test]
    fn classify_rejects_multi_level_subdomain() {
        // ONE label only — rejects deeper subdomains so we don't route a
        // crafted `evil.grocery.claw.example.com` to claw `grocery`.
        assert_eq!(
            cfg().classify("evil.grocery.claw.example.com"),
            HostKind::Unknown
        );
    }

    #[test]
    fn classify_rejects_non_kebab_names() {
        // Reject underscores and dots in the claw label. Uppercase is
        // normalized via the suffix lowercasing pass so `Foo.claw.example.com`
        // maps to claw `foo` — DNS is case-insensitive (RFC 1034 §3.1).
        assert_eq!(cfg().classify("foo_bar.claw.example.com"), HostKind::Unknown);
    }

    #[test]
    fn classify_accepts_uppercase_host_suffix() {
        // RFC: host names are case-insensitive at the suffix level. We
        // lowercase the suffix match but require the claw label to be
        // already lowercase in the request.
        assert_eq!(
            cfg().classify("grocery.CLAW.EXAMPLE.COM"),
            HostKind::Claw("grocery".to_string())
        );
    }

    #[test]
    fn classify_unrelated_host_is_unknown() {
        assert_eq!(cfg().classify("example.com"), HostKind::Unknown);
        assert_eq!(cfg().classify("claws.example.org"), HostKind::Unknown);
        assert_eq!(cfg().classify(""), HostKind::Unknown);
    }

    #[test]
    fn upstream_base_uses_container_name_convention() {
        assert_eq!(cfg().upstream_base("grocery"), "http://claw-grocery:42617");
        assert_eq!(cfg().upstream_base("alfred"), "http://claw-alfred:42617");
    }
}
