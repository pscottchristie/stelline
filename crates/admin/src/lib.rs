//! # admin
//!
//! Production-quality web admin dashboard for the Stelline game server.
//!
//! This crate has **no dependency on zone internals** — it only consumes
//! [`common::AdminSnapshot`] via a [`tokio::sync::watch`] channel.
//!
//! ## Entry points
//!
//! - [`admin_router`] — build an [`axum::Router`] for embedding in a larger app.
//! - [`start_admin_server`] — bind a dedicated TCP port, spawn the server, and
//!   print the dashboard URL to stdout.
//!
//! ## Endpoints
//!
//! | Method | Path        | Description                                    |
//! |--------|-------------|------------------------------------------------|
//! | GET    | `/admin`            | Serves the self-contained HTML dashboard       |
//! | GET    | `/admin/ws`         | WebSocket; pushes JSON [`AdminSnapshot`] on every change |
//! | GET    | `/admin/api/accounts` | JSON list of registered accounts (requires DB) |

use axum::{
    extract::{ws, State, WebSocketUpgrade},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use common::AdminSnapshot;
use serde::Serialize;
use tokio::sync::watch;
use tracing::{debug, error, info};

/// Combined state for admin routes.
#[derive(Clone)]
pub struct AdminState {
    pub snapshot_rx: watch::Receiver<AdminSnapshot>,
    pub db_pool: Option<sqlx::PgPool>,
}

/// Embedded self-contained dashboard HTML.
///
/// Included at compile time so the binary has no runtime file dependency.
static DASHBOARD_HTML: &str = include_str!("dashboard.html");

// ─────────────────────────────────────────────────────────────────────────────
// Router factory
// ─────────────────────────────────────────────────────────────────────────────

/// Build an [`axum::Router`] that serves the admin dashboard.
///
/// The router exposes:
/// - `GET /admin`              — the embedded HTML page.
/// - `GET /admin/ws`           — WebSocket pushing [`AdminSnapshot`] JSON on change.
/// - `GET /admin/api/accounts` — JSON list of registered accounts (if DB is configured).
///
/// The returned router can be merged into a larger application or served
/// standalone via [`start_admin_server`].
pub fn admin_router(snapshot_rx: watch::Receiver<AdminSnapshot>, db_pool: Option<sqlx::PgPool>) -> Router {
    let state = AdminState { snapshot_rx, db_pool };
    Router::new()
        .route("/admin", get(dashboard_handler))
        .route("/admin/ws", get(ws_handler))
        .route("/admin/api/accounts", get(accounts_handler))
        .with_state(state)
}

// ─────────────────────────────────────────────────────────────────────────────
// HTTP handler — serves the embedded HTML page
// ─────────────────────────────────────────────────────────────────────────────

/// Serve the embedded admin dashboard HTML.
async fn dashboard_handler() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        DASHBOARD_HTML,
    )
        .into_response()
}

// ─────────────────────────────────────────────────────────────────────────────
// WebSocket handler
// ─────────────────────────────────────────────────────────────────────────────

/// Upgrade an HTTP connection to a WebSocket and start pushing snapshots.
///
/// Each time the watch channel receives a new value the handler serialises the
/// snapshot to JSON and sends it as a Text frame. The loop exits cleanly when
/// the client disconnects or the watch sender is dropped.
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AdminState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state.snapshot_rx))
}

async fn handle_ws(mut socket: ws::WebSocket, mut rx: watch::Receiver<AdminSnapshot>) {
    debug!("admin WebSocket client connected");

    // Send the current value immediately so the browser doesn't have to wait
    // for the next change.
    {
        let snapshot = rx.borrow_and_update().clone();
        match serde_json::to_string(&snapshot) {
            Ok(json) => {
                if socket.send(ws::Message::Text(json.into())).await.is_err() {
                    // Client already gone before we sent the first frame.
                    return;
                }
            }
            Err(e) => {
                error!(error = %e, "failed to serialise initial AdminSnapshot");
                return;
            }
        }
    }

    loop {
        // Wait for the next change.  If the sender is dropped (server
        // shutdown), `changed()` returns an error and we exit.
        match rx.changed().await {
            Err(_) => {
                debug!("admin watch channel closed; closing WebSocket");
                break;
            }
            Ok(()) => {}
        }

        let snapshot = rx.borrow_and_update().clone();

        let json = match serde_json::to_string(&snapshot) {
            Ok(j) => j,
            Err(e) => {
                error!(error = %e, "failed to serialise AdminSnapshot; skipping frame");
                continue;
            }
        };

        if socket.send(ws::Message::Text(json.into())).await.is_err() {
            debug!("admin WebSocket client disconnected");
            break;
        }
    }

    // Best-effort graceful close.
    let _ = socket.close().await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Accounts API handler
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct AccountRow {
    id: String,
    username: String,
    created_at: String,
}

/// GET /admin/api/accounts — return JSON list of registered accounts.
///
/// Returns 503 if no database is configured (dev mode without DATABASE_URL).
async fn accounts_handler(
    State(state): State<AdminState>,
) -> Result<Json<Vec<AccountRow>>, StatusCode> {
    let pool = state.db_pool.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT id::text, username, to_char(created_at, 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') FROM accounts ORDER BY created_at DESC LIMIT 100",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "failed to query accounts");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let accounts = rows
        .into_iter()
        .map(|(id, username, created_at)| AccountRow {
            id,
            username,
            created_at,
        })
        .collect();

    Ok(Json(accounts))
}

// ─────────────────────────────────────────────────────────────────────────────
// Server launcher
// ─────────────────────────────────────────────────────────────────────────────

/// Bind to `0.0.0.0:{port}`, spawn the admin HTTP server as a Tokio task, and
/// return the [`tokio::task::JoinHandle`].
///
/// Prints `Admin dashboard: http://localhost:{port}/admin` to stdout so
/// operators know where to point their browser.
///
/// # Errors
///
/// Returns an error if the TCP listener cannot be bound (e.g. the port is
/// already in use).
pub async fn start_admin_server(
    port: u16,
    snapshot_rx: watch::Receiver<AdminSnapshot>,
    db_pool: Option<sqlx::PgPool>,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    // Announce the dashboard URL before the task starts so callers see it
    // immediately after the bind succeeds.
    println!("Admin dashboard: http://localhost:{port}/admin");
    info!("admin dashboard listening on http://0.0.0.0:{port}/admin");

    let router = admin_router(snapshot_rx, db_pool);

    let handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router).await {
            error!(error = %e, "admin server exited with error");
        }
    });

    Ok(handle)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum_test::{TestServer, TestServerConfig};
    use common::{AdminSnapshot, ZoneId, ZoneSnapshot};
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    // ── helpers ───────────────────────────────────────────────────────────────

    fn three_zone_snapshot() -> AdminSnapshot {
        AdminSnapshot {
            uptime_secs: 7200,
            connected_players: 42,
            zones: vec![
                ZoneSnapshot {
                    zone_id: ZoneId::new(1),
                    name: "Elwynn Forest".to_string(),
                    player_count: 20,
                    entity_count: 400,
                    tick_rate: 20.0,
                    tick_duration_ms_avg: 2.5,
                    tick_duration_ms_max: 8.0,
                },
                ZoneSnapshot {
                    zone_id: ZoneId::new(2),
                    name: "Stormwind City".to_string(),
                    player_count: 15,
                    entity_count: 200,
                    tick_rate: 19.5,
                    tick_duration_ms_avg: 3.1,
                    tick_duration_ms_max: 12.0,
                },
                ZoneSnapshot {
                    zone_id: ZoneId::new(3),
                    name: "Westfall".to_string(),
                    player_count: 7,
                    entity_count: 150,
                    tick_rate: 17.0, // degraded — should show red in dashboard
                    tick_duration_ms_avg: 6.0,
                    tick_duration_ms_max: 55.0,
                },
            ],
            entity_snapshots: Vec::new(),
        }
    }

    /// Build a [`TestServer`] backed by a real TCP listener on a random port.
    ///
    /// We need a real port because the WebSocket test must open an actual TCP
    /// connection via `tokio-tungstenite`. axum-test's default mock-HTTP
    /// transport does not support WebSocket upgrades.
    fn build_real_server(snapshot: AdminSnapshot) -> (TestServer, watch::Sender<AdminSnapshot>) {
        let (tx, rx) = watch::channel(snapshot);
        let router = admin_router(rx, None);
        let config = TestServerConfig::builder()
            .http_transport()
            .build();
        let server = TestServer::new_with_config(router, config)
            .expect("failed to build TestServer");
        (server, tx)
    }

    // ── Test 1: GET /admin returns 200 and the body mentions "stelline" ───────

    #[tokio::test]
    async fn get_admin_returns_200_with_dashboard_html() {
        let (server, _tx) = build_real_server(AdminSnapshot::default());
        let response = server.get("/admin").await;
        response.assert_status_ok();
        let body = response.text();
        assert!(
            body.to_lowercase().contains("stelline"),
            "response body does not contain 'stelline': {body}"
        );
    }

    // ── Test 2: WebSocket pushes a snapshot that round-trips correctly ────────

    #[tokio::test]
    async fn ws_receives_snapshot_on_change() {
        let initial = AdminSnapshot::default();
        let updated = three_zone_snapshot();

        let (server, tx) = build_real_server(initial);

        // Derive the WebSocket URL from the server address.
        let addr = server
            .server_address()
            .expect("TestServer has no address — is the transport set to HTTP?");
        let ws_url = format!(
            "ws://{}:{}/admin/ws",
            addr.host_str().unwrap_or("127.0.0.1"),
            addr.port().unwrap_or(80),
        );

        // Open a real WebSocket connection.
        let (mut ws_stream, _response) =
            tokio_tungstenite::connect_async(&ws_url)
                .await
                .expect("WebSocket handshake failed");

        // The handler sends the current snapshot immediately on connect.
        let first_msg = ws_stream
            .next()
            .await
            .expect("no initial message")
            .expect("WebSocket error on first receive");

        let first_text = match first_msg {
            Message::Text(t) => t.to_string(),
            other => panic!("expected Text frame, got {other:?}"),
        };
        let _first: AdminSnapshot =
            serde_json::from_str(&first_text)
                .expect("first message is not valid JSON AdminSnapshot");

        // Publish an update on the watch channel.
        tx.send(updated.clone()).expect("watch send failed");

        // The handler should push the new snapshot.
        let second_msg = ws_stream
            .next()
            .await
            .expect("no second message")
            .expect("WebSocket error on second receive");

        let second_text = match second_msg {
            Message::Text(t) => t.to_string(),
            other => panic!("expected Text frame, got {other:?}"),
        };
        let received: AdminSnapshot =
            serde_json::from_str(&second_text)
                .expect("second message is not valid JSON AdminSnapshot");

        assert_eq!(
            received, updated,
            "received snapshot does not match sent snapshot"
        );

        // Clean close.
        ws_stream.close(None).await.ok();
    }

    // ── Test 3: AdminSnapshot with 3 zones serialises and deserialises cleanly ─

    #[test]
    fn snapshot_serialization_round_trip() {
        let original = three_zone_snapshot();
        let json = serde_json::to_string(&original).expect("serialization failed");

        assert!(json.contains("uptime_secs"), "missing uptime_secs");
        assert!(json.contains("connected_players"), "missing connected_players");
        assert!(json.contains("Elwynn Forest"), "missing zone name");
        assert!(json.contains("Stormwind City"), "missing zone name");
        assert!(json.contains("Westfall"), "missing zone name");

        let restored: AdminSnapshot =
            serde_json::from_str(&json).expect("deserialization failed");

        assert_eq!(original, restored, "round-trip failed: data mismatch");
        assert_eq!(restored.zones.len(), 3, "expected 3 zones after round-trip");
        assert_eq!(restored.uptime_secs, 7200);
        assert_eq!(restored.connected_players, 42);
    }
}
