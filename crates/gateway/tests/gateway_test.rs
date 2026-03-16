//! Integration tests for the gateway crate.
//!
//! These tests exercise the public API end-to-end against real in-process TCP
//! and UDP sockets, matching the patterns documented in the implementation.

use bytes::Bytes;
use common::{EntityFlags, EntityId, EntityKind, EntityState, Vec2, Vec3, ZoneId};
use std::sync::Arc;
use gateway::{
    run_connection, run_udp_dispatch, ConnectionConfig, EnterZoneRequest, GatewayAuth,
    GatewayHandle, MessageClass, RoutedCommand, SessionState, UdpPacket,
    classify, read_tcp_message, write_tcp_message,
    character_store::{CharacterStore, InMemoryCharacterStore},
};
use common::{Race, Class};
use protocol::{
    ClientMessage, Handshake, MoveInput, Ping, SelectCharacter, ServerMessage, WorldSnapshot,
};
use tokio::{
    io::BufReader,
    net::{tcp::OwnedWriteHalf, TcpListener, TcpStream, UdpSocket},
    sync::{mpsc, oneshot},
};

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

async fn tcp_duplex() -> (
    OwnedWriteHalf,
    BufReader<tokio::net::tcp::OwnedReadHalf>,
    OwnedWriteHalf,
    BufReader<tokio::net::tcp::OwnedReadHalf>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let client_stream = TcpStream::connect(addr).await.unwrap();
    let (server_stream, _) = listener.accept().await.unwrap();
    let (cr, cw) = client_stream.into_split();
    let (sr, sw) = server_stream.into_split();
    (cw, BufReader::new(sr), sw, BufReader::new(cr))
}

fn make_entity(seed: u64) -> EntityState {
    let mut flags = EntityFlags::empty();
    if seed % 2 == 0 {
        flags.set(EntityFlags::MOVING);
    }
    EntityState::new(
        EntityId::new(seed),
        EntityKind::Player,
        Vec3::new(seed as f32 * 10.0, 0.0, seed as f32 * 5.0),
        seed as f32 * 0.1,
        100 - (seed % 100) as u32,
        100,
        flags,
    )
}

/// Auth config in dev mode (skip_validation=true): accepts any token.
fn dev_auth() -> GatewayAuth {
    GatewayAuth {
        jwt_secret: b"integration-test-secret".to_vec(),
        redis_client: redis::Client::open("redis://127.0.0.1:6379").unwrap(),
        skip_validation: true,
    }
}

/// Auth config in real mode (skip_validation=false): a garbage token will fail
/// JWT decode without needing Redis.
fn real_auth() -> GatewayAuth {
    GatewayAuth {
        jwt_secret: b"integration-test-secret".to_vec(),
        redis_client: redis::Client::open("redis://127.0.0.1:6379").unwrap(),
        skip_validation: false,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Framing
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn framing_header_and_payload_roundtrip() {
    let msg = ClientMessage::MoveInput(MoveInput {
        direction: Vec2::new(0.0, -1.0),
        speed: 7.0,
    });
    let (header_out, payload_out) = protocol::encode_client(&msg, 99).unwrap();

    let (mut cw, mut sr, _sw, _cr) = tcp_duplex().await;
    write_tcp_message(&mut cw, &header_out, &payload_out)
        .await
        .unwrap();

    let (header_in, payload_in) = read_tcp_message(&mut sr).await.unwrap();
    assert_eq!(header_in, header_out);
    assert_eq!(payload_in, payload_out);

    let decoded = protocol::decode_client(&header_in, &payload_in).unwrap();
    assert_eq!(decoded, msg);
}

#[tokio::test]
async fn framing_empty_payload_roundtrip() {
    let msg = ClientMessage::Disconnect;
    let (header_out, payload_out) = protocol::encode_client(&msg, 1).unwrap();
    assert!(payload_out.is_empty());

    let (mut cw, mut sr, _sw, _cr) = tcp_duplex().await;
    write_tcp_message(&mut cw, &header_out, &payload_out)
        .await
        .unwrap();

    let (header_in, payload_in) = read_tcp_message(&mut sr).await.unwrap();
    assert_eq!(header_in.payload_len, 0);
    assert!(payload_in.is_empty());

    let decoded = protocol::decode_client(&header_in, &payload_in).unwrap();
    assert_eq!(decoded, ClientMessage::Disconnect);
}

#[tokio::test]
async fn framing_large_world_snapshot_100_entities() {
    let entities: Vec<EntityState> = (0u64..100).map(make_entity).collect();
    let snap = WorldSnapshot {
        tick: 12345,
        entities: entities.clone(),
    };
    let server_msg = ServerMessage::WorldSnapshot(snap);
    let (header_out, payload_out) = protocol::encode_server(&server_msg, 7).unwrap();

    let (mut cw, mut sr, _sw, _cr) = tcp_duplex().await;
    write_tcp_message(&mut cw, &header_out, &payload_out)
        .await
        .unwrap();

    let (header_in, payload_in) = read_tcp_message(&mut sr).await.unwrap();
    assert_eq!(header_in, header_out);
    assert_eq!(payload_in.len(), payload_out.len());

    let decoded = protocol::decode_server(&header_in, &payload_in).unwrap();
    if let ServerMessage::WorldSnapshot(s) = decoded {
        assert_eq!(s.tick, 12345);
        assert_eq!(s.entities.len(), 100);
        assert_eq!(s.entities[0].entity_id, EntityId::new(0));
        assert_eq!(s.entities[99].entity_id, EntityId::new(99));
    } else {
        panic!("expected WorldSnapshot");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Routing tests
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn move_input_classifies_as_immediate() {
    let msg = ClientMessage::MoveInput(MoveInput {
        direction: Vec2::new(1.0, 0.0),
        speed: 3.0,
    });
    assert_eq!(classify(&msg), MessageClass::Immediate);
}

#[test]
fn handshake_stays_in_handshaking_state_until_accepted() {
    let state = SessionState::Handshaking;
    assert!(matches!(state, SessionState::Handshaking));
}

#[test]
fn session_state_transitions_to_connected() {
    let entity_id = EntityId::new(7);
    let zone_id = ZoneId::new(2);
    let state = SessionState::Connected { entity_id, zone_id };
    match state {
        SessionState::Connected { entity_id: e, zone_id: z } => {
            assert_eq!(e, EntityId::new(7));
            assert_eq!(z, ZoneId::new(2));
        }
        _ => panic!("expected Connected"),
    }
}

#[test]
fn routed_command_contains_correct_entity_id_and_message() {
    let handle = GatewayHandle::new();
    let router = handle.router.clone();
    let GatewayHandle { mut immediate_rx, .. } = handle;

    let entity_id = EntityId::new(42);
    let msg = ClientMessage::MoveInput(MoveInput {
        direction: Vec2::new(0.0, 1.0),
        speed: 6.0,
    });
    router
        .route(RoutedCommand {
            entity_id,
            message: msg.clone(),
        })
        .unwrap();

    let received = immediate_rx.try_recv().unwrap();
    assert_eq!(received.entity_id, entity_id);
    assert_eq!(received.message, msg);
}

#[test]
fn move_input_routes_to_immediate_not_deferred() {
    let handle = GatewayHandle::new();
    let router = handle.router.clone();
    let GatewayHandle { mut immediate_rx, mut deferred_rx, .. } = handle;

    router
        .route(RoutedCommand {
            entity_id: EntityId::new(1),
            message: ClientMessage::MoveInput(MoveInput {
                direction: Vec2::new(1.0, 0.0),
                speed: 1.0,
            }),
        })
        .unwrap();

    assert!(immediate_rx.try_recv().is_ok());
    assert!(deferred_rx.try_recv().is_err());
}

// ─────────────────────────────────────────────────────────────────────────────
// Handshake integration via run_connection
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn run_connection_accepts_valid_token() {
    let (mut client_w, server_r, server_w, mut client_r) = tcp_duplex().await;
    let (udp_tx, _udp_rx) = mpsc::channel(8);
    let handle = GatewayHandle::new();
    let router = handle.router.clone();

    let (validated_tx, validated_rx) = oneshot::channel::<Option<String>>();
    let (auth_result_tx, auth_result_rx) = oneshot::channel::<Result<(), protocol::RejectReason>>();
    let (enter_zone_tx, mut enter_zone_rx) = mpsc::channel::<EnterZoneRequest>(1);
    let (snap_source_tx, snap_source_rx) = oneshot::channel::<Result<mpsc::Receiver<WorldSnapshot>, protocol::RejectReason>>();

    let char_store = Arc::new(InMemoryCharacterStore::new());
    let char_info = char_store.create("some-valid-jwt", "TestHero", Race::Human, Class::Warrior).await.unwrap();

    tokio::spawn(run_connection(
        server_r.into_inner(),
        server_w,
        ConnectionConfig {
            peer_addr: "127.0.0.1:10000".parse().unwrap(),
            udp_tx,
            router,
            auth: dev_auth(),
            entity_id: EntityId::new(1),
            character_store: char_store,
            validated_tx,
            auth_result_rx,
            enter_zone_tx,
            snapshot_rx_source: snap_source_rx,
        },
    ));

    // Client sends Handshake.
    let hs = ClientMessage::Handshake(Handshake { token: "some-valid-jwt".into() });
    let (h, p) = protocol::encode_client(&hs, 1).unwrap();
    write_tcp_message(&mut client_w, &h, &p).await.unwrap();

    // Simulate game-server: receive validation signal, send auth OK.
    let valid = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        validated_rx,
    )
    .await
    .expect("timed out waiting for validated_rx")
    .unwrap();
    assert!(valid.is_some());
    auth_result_tx.send(Ok(())).unwrap();

    // Client sends SelectCharacter.
    let select = ClientMessage::SelectCharacter(SelectCharacter { character_id: char_info.id });
    let (h, p) = protocol::encode_client(&select, 2).unwrap();
    write_tcp_message(&mut client_w, &h, &p).await.unwrap();

    // Simulate game-server: receive enter zone request, provide snapshot channel.
    let _enter_req = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        enter_zone_rx.recv(),
    )
    .await
    .expect("timed out waiting for enter_zone_rx")
    .expect("enter_zone channel closed");

    let (_snap_tx, snap_rx) = mpsc::channel::<WorldSnapshot>(8);
    snap_source_tx.send(Ok(snap_rx)).unwrap();

    // Client should receive HandshakeAccepted.
    let (rh, rp) = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        read_tcp_message(&mut client_r),
    )
    .await
    .expect("timed out waiting for HandshakeAccepted")
    .unwrap();
    let reply = protocol::decode_server(&rh, &rp).unwrap();

    assert!(
        matches!(reply, ServerMessage::HandshakeAccepted(_)),
        "expected HandshakeAccepted, got {:?}",
        reply
    );
}

#[tokio::test]
async fn run_connection_rejects_empty_token() {
    let (mut client_w, server_r, server_w, mut client_r) = tcp_duplex().await;
    let (udp_tx, _udp_rx) = mpsc::channel(8);
    let handle = GatewayHandle::new();
    let router = handle.router.clone();

    let (validated_tx, _validated_rx) = oneshot::channel::<Option<String>>();
    let (_auth_result_tx, auth_result_rx) = oneshot::channel::<Result<(), protocol::RejectReason>>();
    let (enter_zone_tx, _enter_zone_rx) = mpsc::channel::<EnterZoneRequest>(1);
    let (_snap_source_tx, snap_source_rx) = oneshot::channel::<Result<mpsc::Receiver<WorldSnapshot>, protocol::RejectReason>>();

    tokio::spawn(run_connection(
        server_r.into_inner(),
        server_w,
        ConnectionConfig {
            peer_addr: "127.0.0.1:10001".parse().unwrap(),
            udp_tx,
            router,
            auth: real_auth(),
            entity_id: EntityId::new(1),
            character_store: Arc::new(InMemoryCharacterStore::new()),
            validated_tx,
            auth_result_rx,
            enter_zone_tx,
            snapshot_rx_source: snap_source_rx,
        },
    ));

    let hs = ClientMessage::Handshake(Handshake { token: "".into() });
    let (h, p) = protocol::encode_client(&hs, 1).unwrap();
    write_tcp_message(&mut client_w, &h, &p).await.unwrap();

    let (rh, rp) = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        read_tcp_message(&mut client_r),
    )
    .await
    .expect("timed out waiting for HandshakeRejected")
    .unwrap();
    let reply = protocol::decode_server(&rh, &rp).unwrap();

    assert!(
        matches!(
            reply,
            ServerMessage::HandshakeRejected(protocol::HandshakeRejected {
                reason: protocol::RejectReason::InvalidToken
            })
        ),
        "expected HandshakeRejected/InvalidToken, got {:?}",
        reply
    );
}

#[tokio::test]
async fn run_connection_routes_move_input_after_handshake() {
    let (mut client_w, server_r, server_w, mut client_r) = tcp_duplex().await;
    let (udp_tx, _udp_rx) = mpsc::channel(8);
    let handle = GatewayHandle::new();
    let router = handle.router.clone();
    let GatewayHandle { mut immediate_rx, .. } = handle;

    let (validated_tx, validated_rx) = oneshot::channel::<Option<String>>();
    let (auth_result_tx, auth_result_rx) = oneshot::channel::<Result<(), protocol::RejectReason>>();
    let (enter_zone_tx, mut enter_zone_rx) = mpsc::channel::<EnterZoneRequest>(1);
    let (snap_source_tx, snap_source_rx) = oneshot::channel::<Result<mpsc::Receiver<WorldSnapshot>, protocol::RejectReason>>();

    let assigned_entity = EntityId::new(77);
    let char_store = Arc::new(InMemoryCharacterStore::new());
    let char_info = char_store.create("tok", "Fighter", Race::Human, Class::Warrior).await.unwrap();

    tokio::spawn(run_connection(
        server_r.into_inner(),
        server_w,
        ConnectionConfig {
            peer_addr: "127.0.0.1:10002".parse().unwrap(),
            udp_tx,
            router,
            auth: dev_auth(),
            entity_id: assigned_entity,
            character_store: char_store,
            validated_tx,
            auth_result_rx,
            enter_zone_tx,
            snapshot_rx_source: snap_source_rx,
        },
    ));

    // Send Handshake.
    let hs = ClientMessage::Handshake(Handshake { token: "tok".into() });
    let (h, p) = protocol::encode_client(&hs, 1).unwrap();
    write_tcp_message(&mut client_w, &h, &p).await.unwrap();

    // Simulate game-server: auth OK.
    let valid = tokio::time::timeout(std::time::Duration::from_secs(2), validated_rx)
        .await.unwrap().unwrap();
    assert!(valid.is_some());
    auth_result_tx.send(Ok(())).unwrap();

    // Client sends SelectCharacter.
    let select = ClientMessage::SelectCharacter(SelectCharacter { character_id: char_info.id });
    let (h, p) = protocol::encode_client(&select, 2).unwrap();
    write_tcp_message(&mut client_w, &h, &p).await.unwrap();

    // Simulate game-server: enter zone + snapshot channel.
    let _enter_req = tokio::time::timeout(std::time::Duration::from_secs(2), enter_zone_rx.recv())
        .await.unwrap().unwrap();
    let (_snap_tx, snap_rx) = mpsc::channel::<WorldSnapshot>(8);
    snap_source_tx.send(Ok(snap_rx)).unwrap();

    // Read HandshakeAccepted.
    let (rh, rp) = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        read_tcp_message(&mut client_r),
    )
    .await
    .unwrap()
    .unwrap();
    let reply = protocol::decode_server(&rh, &rp).unwrap();
    assert!(matches!(reply, ServerMessage::HandshakeAccepted(_)));

    // Now send a MoveInput.
    let mi = ClientMessage::MoveInput(MoveInput {
        direction: Vec2::new(-1.0, 0.0),
        speed: 4.0,
    });
    let (h, p) = protocol::encode_client(&mi, 3).unwrap();
    write_tcp_message(&mut client_w, &h, &p).await.unwrap();

    // Routed command should arrive on immediate_rx with the pre-assigned entity_id.
    let cmd = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        immediate_rx.recv(),
    )
    .await
    .expect("timed out waiting for RoutedCommand")
    .expect("channel closed");

    assert_eq!(cmd.entity_id, assigned_entity);
    assert!(matches!(cmd.message, ClientMessage::MoveInput(_)));
}

#[tokio::test]
async fn run_connection_replies_pong_to_ping_before_handshake() {
    let (mut client_w, server_r, server_w, mut client_r) = tcp_duplex().await;
    let (udp_tx, _udp_rx) = mpsc::channel(8);
    let handle = GatewayHandle::new();
    let router = handle.router.clone();

    let (validated_tx, _validated_rx) = oneshot::channel::<Option<String>>();
    let (_auth_result_tx, auth_result_rx) = oneshot::channel::<Result<(), protocol::RejectReason>>();
    let (enter_zone_tx, _enter_zone_rx) = mpsc::channel::<EnterZoneRequest>(1);
    let (_snap_source_tx, snap_source_rx) = oneshot::channel::<Result<mpsc::Receiver<WorldSnapshot>, protocol::RejectReason>>();

    tokio::spawn(run_connection(
        server_r.into_inner(),
        server_w,
        ConnectionConfig {
            peer_addr: "127.0.0.1:10003".parse().unwrap(),
            udp_tx,
            router,
            auth: dev_auth(),
            entity_id: EntityId::new(1),
            character_store: Arc::new(InMemoryCharacterStore::new()),
            validated_tx,
            auth_result_rx,
            enter_zone_tx,
            snapshot_rx_source: snap_source_rx,
        },
    ));

    // Send Ping before handshake — should still get a Pong.
    let ping = ClientMessage::Ping(Ping { client_timestamp: 999_888_777 });
    let (h, p) = protocol::encode_client(&ping, 1).unwrap();
    write_tcp_message(&mut client_w, &h, &p).await.unwrap();

    let (rh, rp) = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        read_tcp_message(&mut client_r),
    )
    .await
    .expect("timed out waiting for Pong")
    .unwrap();
    let reply = protocol::decode_server(&rh, &rp).unwrap();

    if let ServerMessage::Pong(pong) = reply {
        assert_eq!(pong.client_timestamp, 999_888_777);
        assert!(pong.server_timestamp > 0);
    } else {
        panic!("expected Pong, got {:?}", reply);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// UdpDispatch integration tests
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn udp_dispatch_delivers_bytes_to_target_address() {
    let recv_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let recv_addr = recv_socket.local_addr().unwrap();
    let dispatch_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

    let (tx, rx) = mpsc::channel::<UdpPacket>(16);
    tokio::spawn(run_udp_dispatch(dispatch_socket, rx));

    let payload = Bytes::from(b"stelline-udp-test-payload".as_ref());
    tx.send(UdpPacket { payload: payload.clone(), addr: recv_addr })
        .await
        .unwrap();

    let mut buf = [0u8; 256];
    let (n, _sender) = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        recv_socket.recv_from(&mut buf),
    )
    .await
    .expect("timed out waiting for UDP packet")
    .unwrap();

    assert_eq!(&buf[..n], payload.as_ref());
}

#[tokio::test]
async fn udp_dispatch_multiple_packets_same_address() {
    let recv_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let recv_addr = recv_socket.local_addr().unwrap();
    let dispatch_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();

    let (tx, rx) = mpsc::channel::<UdpPacket>(16);
    tokio::spawn(run_udp_dispatch(dispatch_socket, rx));

    let payloads = vec![
        Bytes::from_static(b"packet-one"),
        Bytes::from_static(b"packet-two"),
        Bytes::from_static(b"packet-three"),
    ];

    for p in &payloads {
        tx.send(UdpPacket { payload: p.clone(), addr: recv_addr })
            .await
            .unwrap();
    }

    let mut received = Vec::new();
    for _ in 0..payloads.len() {
        let mut buf = [0u8; 256];
        let (n, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            recv_socket.recv_from(&mut buf),
        )
        .await
        .expect("timed out")
        .unwrap();
        received.push(buf[..n].to_vec());
    }

    for (i, expected) in payloads.iter().enumerate() {
        assert_eq!(received[i], expected.as_ref(), "packet {} mismatch", i);
    }
}

#[tokio::test]
async fn udp_dispatch_shuts_down_cleanly_when_channel_dropped() {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (tx, rx) = mpsc::channel::<UdpPacket>(8);

    let task = tokio::spawn(run_udp_dispatch(socket, rx));
    drop(tx);

    tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .expect("dispatch task did not shut down within timeout")
        .expect("task panicked");
}
