//! End-to-end integration tests for the `game-server` crate.
//!
//! These tests compose real server components (zone threads, TCP listener,
//! admin server) against real OS sockets, with `tokio::time::timeout` guarding
//! every async wait.

use std::collections::HashMap;
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::Duration;

use common::{AdminSnapshot, Class, Race, ZoneId};
use gateway::{
    character_store::{CharacterStore, InMemoryCharacterStore},
    GatewayAuth, GatewayHandle, UdpPacket,
};
use tokio::sync::{mpsc, watch};

use game_server::{make_snapshot_bridge, run_coordinator, run_tcp_listener, spawn_zone_threads};

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn dev_auth() -> GatewayAuth {
    GatewayAuth {
        jwt_secret: b"e2e-test-secret".to_vec(),
        redis_client: redis::Client::open("redis://127.0.0.1:6379").unwrap(),
        skip_validation: true,
    }
}

fn real_auth() -> GatewayAuth {
    GatewayAuth {
        jwt_secret: b"e2e-test-secret".to_vec(),
        redis_client: redis::Client::open("redis://127.0.0.1:6379").unwrap(),
        skip_validation: false,
    }
}

/// Write a length-prefixed TCP frame to the stream.
async fn write_frame(
    stream: &mut tokio::net::TcpStream,
    msg: &protocol::ClientMessage,
    seq: u32,
) {
    use tokio::io::AsyncWriteExt;

    let (header, payload) = protocol::encode_client(msg, seq).unwrap();
    let header_bytes = header.encode();
    let frame_len = (protocol::HEADER_SIZE + payload.len()) as u32;

    let mut buf = Vec::with_capacity(4 + protocol::HEADER_SIZE + payload.len());
    buf.extend_from_slice(&frame_len.to_le_bytes());
    buf.extend_from_slice(&header_bytes);
    buf.extend_from_slice(&payload);
    stream.write_all(&buf).await.unwrap();
}

/// Read one length-prefixed TCP frame from the stream and decode a server message.
async fn read_server_msg(stream: &mut tokio::net::TcpStream) -> protocol::ServerMessage {
    use tokio::io::AsyncReadExt;

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await.unwrap();
    let frame_len = u32::from_le_bytes(len_buf) as usize;

    let mut frame = vec![0u8; frame_len];
    stream.read_exact(&mut frame).await.unwrap();

    let header = protocol::PacketHeader::decode_slice(&frame).unwrap();
    let payload = &frame[protocol::HEADER_SIZE..];
    protocol::decode_server(&header, payload).unwrap()
}

/// Connect to a TCP server and perform the full handshake + character select flow,
/// asserting `HandshakeAccepted`.
async fn do_handshake(
    addr: std::net::SocketAddr,
    char_store: &Arc<dyn CharacterStore>,
) -> tokio::net::TcpStream {
    do_handshake_with_token(addr, "test-token", char_store).await
}

/// Connect with a specific token. Creates a character if needed and selects it.
async fn do_handshake_with_token(
    addr: std::net::SocketAddr,
    token: &str,
    char_store: &Arc<dyn CharacterStore>,
) -> tokio::net::TcpStream {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Phase 1: Handshake.
    let hs = protocol::ClientMessage::Handshake(protocol::Handshake {
        token: token.to_string(),
    });
    write_frame(&mut stream, &hs, 1).await;

    // Phase 2: Create a character and select it.
    // Use the token as account_id (dev mode uses token string as subject).
    // Generate a unique name from a hash of the token to avoid collisions.
    let hash: u32 = token.bytes().fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    let char_name = format!("H{:08x}", hash);
    let char_info = match char_store.create(token, &char_name, Race::Human, Class::Warrior).await {
        Ok(info) => info,
        Err(_) => {
            // Character may already exist from a previous test; list and use first one.
            let list = char_store.list(token).await.unwrap();
            list.into_iter().next().expect("no characters found")
        }
    };

    let select = protocol::ClientMessage::SelectCharacter(protocol::SelectCharacter {
        character_id: char_info.id,
    });
    write_frame(&mut stream, &select, 2).await;

    let reply = tokio::time::timeout(Duration::from_secs(2), read_server_msg(&mut stream))
        .await
        .expect("timed out waiting for handshake reply");

    assert!(
        matches!(reply, protocol::ServerMessage::HandshakeAccepted(_)),
        "expected HandshakeAccepted, got {:?}",
        reply
    );
    stream
}

/// Spin up a minimal server: one zone, TCP listener, UDP socket.
/// Returns (tcp_addr, zone_inboxes, character_store).
async fn start_minimal_server_with_auth(
    auth: GatewayAuth,
) -> (
    std::net::SocketAddr,
    HashMap<ZoneId, std_mpsc::SyncSender<world::ZoneCommand>>,
    Arc<dyn CharacterStore>,
) {
    let world_config = game_data::WorldConfig {
        creatures: vec![],
        zones: vec![game_data::ZoneConfig {
            id: 1,
            name: "Test Zone".to_string(),
            aoi_radius: 150.0,
            width: 1000.0,
            height: 1000.0,
            spawns: vec![],
        }],
    };

    let (zone_inboxes, _zone_event_rx) = spawn_zone_threads(&world_config).unwrap();

    let handle = GatewayHandle::new();
    let router = handle.router.clone();

    let udp_socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (udp_tx, udp_rx) = mpsc::channel::<UdpPacket>(64);
    tokio::spawn(gateway::run_udp_dispatch(udp_socket, udp_rx));

    let character_store: Arc<dyn CharacterStore> = Arc::new(InMemoryCharacterStore::new());

    let tcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tcp_addr = tcp_listener.local_addr().unwrap();

    tokio::spawn(run_tcp_listener(
        tcp_listener,
        router,
        udp_tx,
        zone_inboxes.clone(),
        auth,
        character_store.clone(),
    ));

    (tcp_addr, zone_inboxes, character_store)
}

async fn start_minimal_server() -> (
    std::net::SocketAddr,
    HashMap<ZoneId, std_mpsc::SyncSender<world::ZoneCommand>>,
    Arc<dyn CharacterStore>,
) {
    start_minimal_server_with_auth(dev_auth()).await
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 1: TCP connect → handshake → HandshakeAccepted
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_connect_handshake_accepted() {
    let (tcp_addr, _, char_store) = start_minimal_server().await;

    tokio::time::sleep(Duration::from_millis(20)).await;

    let _ = tokio::time::timeout(Duration::from_secs(2), do_handshake(tcp_addr, &char_store))
        .await
        .expect("handshake timed out");
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 2: Admin dashboard is reachable (GET /admin → 200 with "stelline")
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_admin_dashboard_reachable() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (admin_tx, admin_rx) = watch::channel(AdminSnapshot::default());
    let _admin_tx = admin_tx;

    let admin_port = {
        let tmp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = tmp.local_addr().unwrap().port();
        drop(tmp);
        port
    };

    admin::start_admin_server(admin_port, admin_rx, None)
        .await
        .expect("admin server failed to start");

    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut stream =
        tokio::time::timeout(
            Duration::from_secs(2),
            tokio::net::TcpStream::connect(format!("127.0.0.1:{}", admin_port)),
        )
        .await
        .expect("connect timed out")
        .expect("connect failed");

    let request = format!(
        "GET /admin HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n",
        admin_port
    );
    stream.write_all(request.as_bytes()).await.unwrap();

    let mut response = String::new();
    tokio::time::timeout(
        Duration::from_secs(2),
        stream.read_to_string(&mut response),
    )
    .await
    .expect("read timed out")
    .expect("read failed");

    assert!(
        response.starts_with("HTTP/1.1 200"),
        "expected HTTP 200, got: {}",
        &response[..response.len().min(200)]
    );

    let body_lower = response.to_lowercase();
    assert!(
        body_lower.contains("stelline"),
        "response body does not contain 'stelline'"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 3: Zone telemetry flows to admin watch channel
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_zone_telemetry_reaches_admin_watch() {
    let world_config = game_data::WorldConfig {
        creatures: vec![],
        zones: vec![game_data::ZoneConfig {
            id: 1,
            name: "Telemetry Zone".to_string(),
            aoi_radius: 150.0,
            width: 1000.0,
            height: 1000.0,
            spawns: vec![],
        }],
    };

    let (zone_inboxes, zone_event_rx) = spawn_zone_threads(&world_config).unwrap();

    let (admin_tx, mut admin_rx) = watch::channel(AdminSnapshot::default());

    let start_time = std::time::Instant::now();
    tokio::spawn(run_coordinator(
        zone_event_rx,
        zone_inboxes,
        admin_tx,
        start_time,
    ));

    let updated = tokio::time::timeout(Duration::from_millis(500), async {
        loop {
            admin_rx.changed().await.unwrap();
            let snap = admin_rx.borrow().clone();
            if !snap.zones.is_empty() {
                return snap;
            }
        }
    })
    .await
    .expect("timed out waiting for telemetry to reach admin snapshot");

    assert!(!updated.zones.is_empty());

    let zone_snap = &updated.zones[0];
    assert_eq!(zone_snap.zone_id, ZoneId::new(1));
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 4: Multiple clients connect without interference
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_multiple_clients_connect_concurrently() {
    let (tcp_addr, _, char_store) = start_minimal_server().await;
    tokio::time::sleep(Duration::from_millis(30)).await;

    let tasks: Vec<_> = (0..3)
        .map(|i| {
            let token = format!("test-user-{i}");
            let cs = char_store.clone();
            tokio::spawn(async move {
                tokio::time::timeout(
                    Duration::from_secs(2),
                    do_handshake_with_token(tcp_addr, &token, &cs),
                ).await
            })
        })
        .collect();

    for task in tasks {
        task.await
            .expect("task panicked")
            .expect("handshake timed out");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 5: Snapshot bridge delivers snapshots from sync to async
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_snapshot_bridge_delivers_snapshots() {
    let (sync_tx, mut tok_rx) = make_snapshot_bridge(8);

    let snap = protocol::WorldSnapshot {
        tick: 42,
        entities: vec![],
    };

    sync_tx.send(snap.clone()).unwrap();

    let received = tokio::time::timeout(Duration::from_millis(200), tok_rx.recv())
        .await
        .expect("timed out waiting for snapshot")
        .expect("channel closed unexpectedly");

    assert_eq!(received.tick, 42);
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 6: Handshake rejected for invalid token (real validation mode)
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_handshake_rejected_invalid_token() {
    // Use real_auth (skip_validation=false) so a garbage token fails JWT decode.
    let (tcp_addr, _, _char_store) = start_minimal_server_with_auth(real_auth()).await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    let mut stream = tokio::net::TcpStream::connect(tcp_addr).await.unwrap();

    let hs = protocol::ClientMessage::Handshake(protocol::Handshake {
        token: "not-a-valid-jwt".to_string(),
    });
    write_frame(&mut stream, &hs, 1).await;

    let reply = tokio::time::timeout(Duration::from_secs(2), read_server_msg(&mut stream))
        .await
        .expect("timed out waiting for handshake reply");

    assert!(
        matches!(reply, protocol::ServerMessage::HandshakeRejected(_)),
        "expected HandshakeRejected for invalid token, got {:?}",
        reply
    );
}
