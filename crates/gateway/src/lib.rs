//! # gateway
//!
//! Async network I/O layer for Stelline. The gateway handles all TCP and UDP
//! socket work so the game tick never touches sockets — channels are the only
//! bridge between the network and the zone simulation.
//!
//! ## Architecture overview
//!
//! ```text
//! Client TCP ──► read_tcp_message ──► route_client_message
//!                                         │
//!                              ┌──────────┴──────────┐
//!                         immediate_tx          deferred_tx
//!                         (MoveInput …)         (Chat, AH …)
//!
//! Zone ──WorldSnapshot──► ConnectionTask ──UdpPacket──► UdpDispatchTask
//!                                                             │
//!                                                       socket.send_to
//! ```
//!
//! - One [`ConnectionTask`] per connected client (async Tokio task).
//! - One [`UdpDispatchTask`] owns the [`tokio::net::UdpSocket`] exclusively —
//!   no `Arc` is needed.
//! - The gateway does **not** depend on the `world` crate to avoid coupling.
//!   Routing is done through generic [`RoutedCommand`] values; `game-server`
//!   wires the concrete zone inbox at startup.

pub mod character_store;

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use common::{EntityId, Vec3, ZoneId};
use jsonwebtoken::{decode, DecodingKey, Validation};
use jsonwebtoken::errors::ErrorKind as JwtErrorKind;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::tcp::{OwnedReadHalf, OwnedWriteHalf},
    sync::{mpsc, oneshot},
};
use tracing::{debug, warn};

use character_store::CharacterStore;

pub use protocol;

// ─────────────────────────────────────────────────────────────────────────────
// Error type
// ─────────────────────────────────────────────────────────────────────────────

/// All errors that can occur within the gateway layer.
#[derive(thiserror::Error, Debug)]
pub enum GatewayError {
    /// Underlying I/O failure (TCP read/write, UDP send, etc.).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The received bytes could not be parsed as a valid protocol message.
    #[error("protocol error: {0}")]
    Protocol(#[from] protocol::ProtocolError),

    /// The TCP stream was closed by the remote peer.
    #[error("connection closed")]
    ConnectionClosed,

    /// An internal channel was dropped — the receiving end is gone.
    #[error("channel closed")]
    ChannelClosed,
}

// ─────────────────────────────────────────────────────────────────────────────
// Session state
// ─────────────────────────────────────────────────────────────────────────────

/// Tracks where a connection currently is in the session lifecycle.
#[derive(Debug, Clone)]
pub enum SessionState {
    /// Token not yet validated.
    Handshaking,
    /// Token validated, client is selecting a character.
    CharacterSelect,
    /// Character selected — client controls `entity_id` inside `zone_id`.
    Connected { entity_id: EntityId, zone_id: ZoneId },
}

// ─────────────────────────────────────────────────────────────────────────────
// UDP dispatch
// ─────────────────────────────────────────────────────────────────────────────

/// A serialised payload destined for a specific UDP address.
#[derive(Debug)]
pub struct UdpPacket {
    pub payload: Bytes,
    pub addr: SocketAddr,
}

/// Owns the UDP socket exclusively and forwards outbound packets.
pub async fn run_udp_dispatch(socket: tokio::net::UdpSocket, mut rx: mpsc::Receiver<UdpPacket>) {
    while let Some(pkt) = rx.recv().await {
        match socket.send_to(&pkt.payload, pkt.addr).await {
            Ok(n) => {
                debug!(bytes = n, addr = %pkt.addr, "udp sent");
            }
            Err(e) => {
                warn!(error = %e, addr = %pkt.addr, "udp send failed");
            }
        }
    }
    debug!("udp dispatch task exiting — channel closed");
}

// ─────────────────────────────────────────────────────────────────────────────
// Routing
// ─────────────────────────────────────────────────────────────────────────────

/// A [`protocol::ClientMessage`] tagged with the originating entity.
#[derive(Debug)]
pub struct RoutedCommand {
    pub entity_id: EntityId,
    pub message: protocol::ClientMessage,
}

/// How to route client messages out of the gateway.
#[derive(Clone, Debug)]
pub struct ClientRouter {
    pub immediate_tx: mpsc::UnboundedSender<RoutedCommand>,
    pub deferred_tx: mpsc::UnboundedSender<RoutedCommand>,
}

/// Classification of a client message for routing purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageClass {
    Immediate,
    Deferred,
    Internal,
}

/// Classify a decoded client message.
pub fn classify(msg: &protocol::ClientMessage) -> MessageClass {
    match msg {
        protocol::ClientMessage::MoveInput(_) => MessageClass::Immediate,
        protocol::ClientMessage::Handshake(_)
        | protocol::ClientMessage::Ping(_)
        | protocol::ClientMessage::Disconnect
        | protocol::ClientMessage::CharacterListRequest(_)
        | protocol::ClientMessage::CreateCharacter(_)
        | protocol::ClientMessage::DeleteCharacter(_)
        | protocol::ClientMessage::SelectCharacter(_) => MessageClass::Internal,
    }
}

impl ClientRouter {
    pub fn route(&self, cmd: RoutedCommand) -> Result<(), GatewayError> {
        let class = classify(&cmd.message);
        match class {
            MessageClass::Immediate => self
                .immediate_tx
                .send(cmd)
                .map_err(|_| GatewayError::ChannelClosed),
            MessageClass::Deferred => self
                .deferred_tx
                .send(cmd)
                .map_err(|_| GatewayError::ChannelClosed),
            MessageClass::Internal => {
                warn!("route() called with an internal message — dropping");
                Ok(())
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// GatewayHandle
// ─────────────────────────────────────────────────────────────────────────────

pub struct GatewayHandle {
    pub immediate_rx: mpsc::UnboundedReceiver<RoutedCommand>,
    pub deferred_rx: mpsc::UnboundedReceiver<RoutedCommand>,
    pub router: ClientRouter,
}

impl GatewayHandle {
    pub fn new() -> Self {
        let (immediate_tx, immediate_rx) = mpsc::unbounded_channel();
        let (deferred_tx, deferred_rx) = mpsc::unbounded_channel();
        let router = ClientRouter {
            immediate_tx,
            deferred_tx,
        };
        Self {
            immediate_rx,
            deferred_rx,
            router,
        }
    }
}

impl Default for GatewayHandle {
    fn default() -> Self {
        Self::new()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// TCP framing
// ─────────────────────────────────────────────────────────────────────────────

const FRAME_LEN_SIZE: usize = 4;

/// Read one length-prefixed TCP frame and return the decoded header and payload.
pub async fn read_tcp_message(
    stream: &mut BufReader<OwnedReadHalf>,
) -> Result<(protocol::PacketHeader, Vec<u8>), GatewayError> {
    let mut len_buf = [0u8; FRAME_LEN_SIZE];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(GatewayError::ConnectionClosed);
        }
        Err(e) => return Err(GatewayError::Io(e)),
    }
    let frame_len = u32::from_le_bytes(len_buf) as usize;

    if frame_len < protocol::HEADER_SIZE {
        return Err(GatewayError::Protocol(protocol::ProtocolError::HeaderTooShort));
    }

    let mut frame = vec![0u8; frame_len];
    match stream.read_exact(&mut frame).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(GatewayError::ConnectionClosed);
        }
        Err(e) => return Err(GatewayError::Io(e)),
    }

    let header = protocol::PacketHeader::decode_slice(&frame)?;
    let payload = frame[protocol::HEADER_SIZE..].to_vec();

    Ok((header, payload))
}

/// Write one length-prefixed TCP frame.
pub async fn write_tcp_message(
    stream: &mut OwnedWriteHalf,
    header: &protocol::PacketHeader,
    payload: &[u8],
) -> Result<(), GatewayError> {
    let header_bytes = header.encode();
    let frame_len = (protocol::HEADER_SIZE + payload.len()) as u32;

    let mut buf =
        Vec::with_capacity(FRAME_LEN_SIZE + protocol::HEADER_SIZE + payload.len());
    buf.extend_from_slice(&frame_len.to_le_bytes());
    buf.extend_from_slice(&header_bytes);
    buf.extend_from_slice(payload);

    stream.write_all(&buf).await?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Auth config
// ─────────────────────────────────────────────────────────────────────────────

/// Auth configuration threaded into every connection task.
///
/// - `jwt_secret`: HMAC-SHA256 key used to verify JWTs issued by login-server.
/// - `redis_client`: checked once at connect to query the token blocklist.
/// - `skip_validation`: dev/test mode — accept any token without real validation.
#[derive(Clone)]
pub struct GatewayAuth {
    pub jwt_secret: Vec<u8>,
    pub redis_client: redis::Client,
    pub skip_validation: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// ConnectionTask
// ─────────────────────────────────────────────────────────────────────────────

/// Request from the connection task to the game-server to place a player in a zone.
#[derive(Debug)]
pub struct EnterZoneRequest {
    pub zone_id: ZoneId,
    pub position: Vec3,
    pub label: String,
}

/// Configuration required to spawn a [`run_connection`] task.
pub struct ConnectionConfig {
    /// Remote address of this client (used for UDP packet addressing).
    pub peer_addr: SocketAddr,

    /// Channel to the [`run_udp_dispatch`] task for outbound UDP packets.
    pub udp_tx: mpsc::Sender<UdpPacket>,

    /// Pre-built router for forwarding decoded client messages.
    pub router: ClientRouter,

    /// Auth configuration (JWT secret + Redis blocklist + dev-mode flag).
    pub auth: GatewayAuth,

    /// Entity ID pre-assigned by game-server before this task is spawned.
    pub entity_id: EntityId,

    /// Character store for the character selection phase.
    pub character_store: Arc<dyn CharacterStore>,

    /// Signal back to game-server: `Some(account_uuid)` = JWT valid;
    /// `None` = rejected, skip everything.
    pub validated_tx: oneshot::Sender<Option<String>>,

    /// Receives auth result from game-server (duplicate login check).
    /// `Ok(())` = proceed to character selection.
    /// `Err(reason)` = rejected.
    pub auth_result_rx: oneshot::Receiver<Result<(), protocol::RejectReason>>,

    /// Send zone placement request after character selection.
    pub enter_zone_tx: mpsc::Sender<EnterZoneRequest>,

    /// Receives the async snapshot channel from game-server after PlayerEnter.
    /// `Ok(rx)` = proceed with HandshakeAccepted.
    /// `Err(reason)` = game-server rejected.
    pub snapshot_rx_source: oneshot::Receiver<Result<mpsc::Receiver<protocol::WorldSnapshot>, protocol::RejectReason>>,
}

/// Run the async task that manages one TCP connection.
///
/// ## Three-phase design
///
/// **Phase 1 — Handshake**: wait for a `Handshake` message, validate the JWT
/// (real or dev-mode), signal game-server via `validated_tx`, await auth result.
///
/// **Phase 2 — Character Selection**: handle CharacterListRequest, CreateCharacter,
/// DeleteCharacter. On SelectCharacter, load full info, signal game-server to
/// place the player in a zone, send HandshakeAccepted.
///
/// **Phase 3 — Connected**: select loop forwarding zone snapshots via UDP and
/// routing inbound client messages.
pub async fn run_connection(
    tcp_read: OwnedReadHalf,
    mut tcp_write: OwnedWriteHalf,
    config: ConnectionConfig,
) {
    let ConnectionConfig {
        peer_addr,
        udp_tx,
        router,
        auth,
        entity_id,
        character_store,
        validated_tx,
        auth_result_rx,
        enter_zone_tx,
        snapshot_rx_source,
    } = config;

    let mut reader = BufReader::new(tcp_read);
    let mut seq: u32 = 0;

    // ── Phase 1: Handshake ────────────────────────────────────────────────────
    let account_uuid = 'handshake: loop {
        match read_tcp_message(&mut reader).await {
            Err(GatewayError::ConnectionClosed) => {
                debug!(addr = %peer_addr, "disconnected before handshake");
                let _ = validated_tx.send(None);
                return;
            }
            Err(e) => {
                warn!(addr = %peer_addr, error = %e, "tcp error before handshake");
                let _ = validated_tx.send(None);
                return;
            }
            Ok((header, payload)) => {
                let msg = match protocol::decode_client(&header, &payload) {
                    Ok(m) => m,
                    Err(e) => {
                        warn!(addr = %peer_addr, error = %e, "protocol error before handshake");
                        let _ = validated_tx.send(None);
                        return;
                    }
                };

                match msg {
                    protocol::ClientMessage::Handshake(hs) => {
                        seq = seq.wrapping_add(1);
                        match validate_token(&hs.token, &auth).await {
                            Ok(claims) => {
                                let account = claims.sub.clone();
                                if validated_tx.send(Some(claims.sub)).is_err() {
                                    return;
                                }
                                // Wait for game-server auth result (duplicate login check).
                                match auth_result_rx.await {
                                    Ok(Ok(())) => {
                                        break 'handshake account;
                                    }
                                    Ok(Err(reason)) => {
                                        let reply = protocol::ServerMessage::HandshakeRejected(
                                            protocol::HandshakeRejected { reason },
                                        );
                                        if let Ok((hdr, pl)) = protocol::encode_server(&reply, seq) {
                                            let _ = write_tcp_message(&mut tcp_write, &hdr, &pl).await;
                                        }
                                        debug!(addr = %peer_addr, ?reason, "handshake rejected by game-server");
                                        return;
                                    }
                                    Err(_) => return,
                                }
                            }
                            Err(reason) => {
                                let reply = protocol::ServerMessage::HandshakeRejected(
                                    protocol::HandshakeRejected { reason },
                                );
                                if let Ok((hdr, pl)) = protocol::encode_server(&reply, seq) {
                                    let _ = write_tcp_message(&mut tcp_write, &hdr, &pl).await;
                                }
                                let _ = validated_tx.send(None);
                                debug!(addr = %peer_addr, ?reason, "handshake rejected");
                                return;
                            }
                        }
                    }

                    protocol::ClientMessage::Ping(ping) => {
                        seq = seq.wrapping_add(1);
                        send_pong(&mut tcp_write, &mut seq, ping.client_timestamp).await;
                    }

                    protocol::ClientMessage::Disconnect => {
                        debug!(addr = %peer_addr, "disconnect before handshake");
                        let _ = validated_tx.send(None);
                        return;
                    }

                    other => {
                        warn!(
                            addr = %peer_addr,
                            msg = ?other,
                            "non-Handshake before session established — dropping connection"
                        );
                        let _ = validated_tx.send(None);
                        return;
                    }
                }
            }
        }
    };

    // ── Phase 2: Character Selection ──────────────────────────────────────────
    let mut snapshot_rx = 'char_select: loop {
        match read_tcp_message(&mut reader).await {
            Err(GatewayError::ConnectionClosed) => {
                debug!(addr = %peer_addr, "disconnected during character selection");
                return;
            }
            Err(e) => {
                warn!(addr = %peer_addr, error = %e, "tcp error during character selection");
                return;
            }
            Ok((header, payload)) => {
                let msg = match protocol::decode_client(&header, &payload) {
                    Ok(m) => m,
                    Err(e) => {
                        warn!(addr = %peer_addr, error = %e, "protocol error during character selection");
                        return;
                    }
                };

                match msg {
                    protocol::ClientMessage::CharacterListRequest(_) => {
                        seq = seq.wrapping_add(1);
                        match character_store.list(&account_uuid).await {
                            Ok(characters) => {
                                let reply = protocol::ServerMessage::CharacterList(
                                    protocol::CharacterList { characters },
                                );
                                if let Ok((hdr, pl)) = protocol::encode_server(&reply, seq) {
                                    let _ = write_tcp_message(&mut tcp_write, &hdr, &pl).await;
                                }
                            }
                            Err(e) => {
                                warn!(addr = %peer_addr, error = %e, "character list failed");
                            }
                        }
                    }

                    protocol::ClientMessage::CreateCharacter(req) => {
                        seq = seq.wrapping_add(1);
                        match character_store
                            .create(&account_uuid, &req.name, req.race, req.class)
                            .await
                        {
                            Ok(character) => {
                                let reply = protocol::ServerMessage::CharacterCreated(
                                    protocol::CharacterCreated { character },
                                );
                                if let Ok((hdr, pl)) = protocol::encode_server(&reply, seq) {
                                    let _ = write_tcp_message(&mut tcp_write, &hdr, &pl).await;
                                }
                            }
                            Err(e) => {
                                let reply = protocol::ServerMessage::CharacterCreateFailed(
                                    protocol::CharacterCreateFailed {
                                        reason: e.to_string(),
                                    },
                                );
                                if let Ok((hdr, pl)) = protocol::encode_server(&reply, seq) {
                                    let _ = write_tcp_message(&mut tcp_write, &hdr, &pl).await;
                                }
                            }
                        }
                    }

                    protocol::ClientMessage::DeleteCharacter(req) => {
                        seq = seq.wrapping_add(1);
                        match character_store
                            .delete(&account_uuid, req.character_id.get())
                            .await
                        {
                            Ok(()) => {
                                let reply = protocol::ServerMessage::CharacterDeleted(
                                    protocol::CharacterDeleted {
                                        character_id: req.character_id,
                                    },
                                );
                                if let Ok((hdr, pl)) = protocol::encode_server(&reply, seq) {
                                    let _ = write_tcp_message(&mut tcp_write, &hdr, &pl).await;
                                }
                            }
                            Err(e) => {
                                warn!(addr = %peer_addr, error = %e, "character delete failed");
                            }
                        }
                    }

                    protocol::ClientMessage::SelectCharacter(req) => {
                        seq = seq.wrapping_add(1);
                        match character_store
                            .get_full(&account_uuid, req.character_id.get())
                            .await
                        {
                            Ok(full) => {
                                let zone_id = full.zone_id;
                                let position = full.position;
                                let label = full.name.clone();

                                // Tell game-server to place this player.
                                if enter_zone_tx
                                    .send(EnterZoneRequest {
                                        zone_id,
                                        position,
                                        label,
                                    })
                                    .await
                                    .is_err()
                                {
                                    return;
                                }

                                // Wait for snapshot channel.
                                match snapshot_rx_source.await {
                                    Ok(Ok(rx)) => {
                                        let reply =
                                            protocol::ServerMessage::HandshakeAccepted(
                                                protocol::HandshakeAccepted {
                                                    entity_id,
                                                    zone_id,
                                                },
                                            );
                                        let (hdr, pl) = match protocol::encode_server(&reply, seq) {
                                            Ok(v) => v,
                                            Err(e) => {
                                                warn!(addr = %peer_addr, error = %e, "encode HandshakeAccepted");
                                                return;
                                            }
                                        };
                                        if let Err(e) =
                                            write_tcp_message(&mut tcp_write, &hdr, &pl).await
                                        {
                                            warn!(addr = %peer_addr, error = %e, "write HandshakeAccepted");
                                            return;
                                        }
                                        debug!(
                                            addr = %peer_addr,
                                            entity = ?entity_id,
                                            zone = ?zone_id,
                                            "character selected, entering world"
                                        );
                                        break 'char_select rx;
                                    }
                                    Ok(Err(reason)) => {
                                        let reply = protocol::ServerMessage::HandshakeRejected(
                                            protocol::HandshakeRejected { reason },
                                        );
                                        if let Ok((hdr, pl)) = protocol::encode_server(&reply, seq) {
                                            let _ = write_tcp_message(&mut tcp_write, &hdr, &pl).await;
                                        }
                                        return;
                                    }
                                    Err(_) => return,
                                }
                            }
                            Err(e) => {
                                // Character not found — send create-failed as error feedback.
                                let reply = protocol::ServerMessage::CharacterCreateFailed(
                                    protocol::CharacterCreateFailed {
                                        reason: e.to_string(),
                                    },
                                );
                                if let Ok((hdr, pl)) = protocol::encode_server(&reply, seq) {
                                    let _ = write_tcp_message(&mut tcp_write, &hdr, &pl).await;
                                }
                            }
                        }
                    }

                    protocol::ClientMessage::Ping(ping) => {
                        seq = seq.wrapping_add(1);
                        send_pong(&mut tcp_write, &mut seq, ping.client_timestamp).await;
                    }

                    protocol::ClientMessage::Disconnect => {
                        debug!(addr = %peer_addr, "disconnect during character selection");
                        return;
                    }

                    other => {
                        warn!(
                            addr = %peer_addr,
                            msg = ?other,
                            "unexpected message during character selection"
                        );
                    }
                }
            }
        }
    };

    // ── Phase 2: Connected — select loop ──────────────────────────────────────
    loop {
        tokio::select! {
            // Inbound TCP frame from the client.
            result = read_tcp_message(&mut reader) => {
                match result {
                    Err(GatewayError::ConnectionClosed) => {
                        debug!(addr = %peer_addr, "client disconnected");
                        break;
                    }
                    Err(e) => {
                        warn!(addr = %peer_addr, error = %e, "tcp read error");
                        break;
                    }
                    Ok((header, payload)) => {
                        match protocol::decode_client(&header, &payload) {
                            Err(e) => {
                                warn!(addr = %peer_addr, error = %e, "protocol error");
                                break;
                            }
                            Ok(msg) => {
                                match handle_connected(
                                    msg,
                                    &mut seq,
                                    &mut tcp_write,
                                    &router,
                                    entity_id,
                                    peer_addr,
                                )
                                .await
                                {
                                    Ok(KeepGoing::Yes) => {}
                                    Ok(KeepGoing::No) | Err(_) => break,
                                }
                            }
                        }
                    }
                }
            }

            // Outbound WorldSnapshot from the zone — forward via UDP.
            snapshot = snapshot_rx.recv() => {
                match snapshot {
                    None => {
                        debug!(addr = %peer_addr, "snapshot channel closed");
                        break;
                    }
                    Some(snap) => {
                        if let Err(e) = forward_snapshot(snap, seq, peer_addr, &udp_tx).await {
                            warn!(addr = %peer_addr, error = %e, "snapshot forward failed");
                            // Non-fatal: UDP is best-effort.
                        }
                        seq = seq.wrapping_add(1);
                    }
                }
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Connected-phase message handler
// ─────────────────────────────────────────────────────────────────────────────

/// Handle one inbound message in the connected phase.
async fn handle_connected(
    msg: protocol::ClientMessage,
    seq: &mut u32,
    tcp_write: &mut OwnedWriteHalf,
    router: &ClientRouter,
    entity_id: EntityId,
    peer_addr: SocketAddr,
) -> Result<KeepGoing, GatewayError> {
    match msg {
        protocol::ClientMessage::Ping(ping) => {
            *seq = seq.wrapping_add(1);
            let server_ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            let reply = protocol::ServerMessage::Pong(protocol::Pong {
                client_timestamp: ping.client_timestamp,
                server_timestamp: server_ts,
            });
            let (hdr, pl) = protocol::encode_server(&reply, *seq)?;
            write_tcp_message(tcp_write, &hdr, &pl).await?;
        }

        protocol::ClientMessage::Disconnect => {
            debug!(addr = %peer_addr, "client sent Disconnect");
            return Ok(KeepGoing::No);
        }

        // A second Handshake after session is established is a protocol error.
        protocol::ClientMessage::Handshake(_) => {
            warn!(addr = %peer_addr, "duplicate Handshake after session established — dropping");
            return Ok(KeepGoing::No);
        }

        other => {
            let class = classify(&other);
            if class != MessageClass::Internal {
                let cmd = RoutedCommand {
                    entity_id,
                    message: other,
                };
                router.route(cmd)?;
            }
        }
    }

    Ok(KeepGoing::Yes)
}

// ─────────────────────────────────────────────────────────────────────────────
// Snapshot forwarding
// ─────────────────────────────────────────────────────────────────────────────

async fn forward_snapshot(
    snapshot: protocol::WorldSnapshot,
    seq: u32,
    peer_addr: SocketAddr,
    udp_tx: &mpsc::Sender<UdpPacket>,
) -> Result<(), GatewayError> {
    let msg = protocol::ServerMessage::WorldSnapshot(snapshot);
    let (header, payload) = protocol::encode_server(&msg, seq)?;

    let frame_len = (protocol::HEADER_SIZE + payload.len()) as u32;
    let mut buf = Vec::with_capacity(FRAME_LEN_SIZE + protocol::HEADER_SIZE + payload.len());
    buf.extend_from_slice(&frame_len.to_le_bytes());
    buf.extend_from_slice(&header.encode());
    buf.extend_from_slice(&payload);

    let pkt = UdpPacket {
        payload: Bytes::from(buf),
        addr: peer_addr,
    };
    udp_tx.send(pkt).await.map_err(|_| GatewayError::ChannelClosed)
}

// ─────────────────────────────────────────────────────────────────────────────
// Token validation
// ─────────────────────────────────────────────────────────────────────────────

/// Validate a JWT token string.
///
/// - In dev mode (`skip_validation = true`): return fake claims immediately.
/// - Otherwise: verify the JWT signature and expiry, then check the Redis
///   blocklist using the `jti` claim. Returns the validated claims on success.
async fn validate_token(
    token: &str,
    auth: &GatewayAuth,
) -> Result<protocol::AuthClaims, protocol::RejectReason> {
    if auth.skip_validation {
        // Dev mode: skip real validation so developers can test without a
        // running login-server. The token string is used as a fake subject.
        return Ok(protocol::AuthClaims {
            sub: token.to_string(),
            jti: token.to_string(),
            iat: 0,
            exp: usize::MAX,
        });
    }

    // 1. Decode and verify the JWT signature and expiry.
    let token_data = decode::<protocol::AuthClaims>(
        token,
        &DecodingKey::from_secret(&auth.jwt_secret),
        &Validation::default(),
    )
    .map_err(|e| {
        if e.kind() == &JwtErrorKind::ExpiredSignature {
            protocol::RejectReason::ExpiredToken
        } else {
            protocol::RejectReason::InvalidToken
        }
    })?;

    // 2. Redis blocklist check — reject if the jti has been revoked.
    let mut conn = auth
        .redis_client
        .get_async_connection()
        .await
        .map_err(|_| protocol::RejectReason::InvalidToken)?;

    let blocked: bool = redis::cmd("EXISTS")
        .arg(&token_data.claims.jti)
        .query_async(&mut conn)
        .await
        .unwrap_or(false);

    if blocked {
        return Err(protocol::RejectReason::InvalidToken);
    }

    Ok(token_data.claims)
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal helpers
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeepGoing {
    Yes,
    No,
}

/// Send a Pong reply on the TCP connection.
async fn send_pong(tcp_write: &mut OwnedWriteHalf, seq: &mut u32, client_timestamp: u64) {
    let server_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let reply = protocol::ServerMessage::Pong(protocol::Pong {
        client_timestamp,
        server_timestamp: server_ts,
    });
    if let Ok((hdr, pl)) = protocol::encode_server(&reply, *seq) {
        let _ = write_tcp_message(tcp_write, &hdr, &pl).await;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use common::{EntityFlags, EntityId, EntityKind, EntityState, Vec2, Vec3};
    use protocol::{ClientMessage, Handshake, MoveInput, PacketHeader, Ping, WorldSnapshot};
    use tokio::io::BufReader;

    // ── Helpers ──────────────────────────────────────────────────────────────

    async fn tcp_duplex() -> (
        OwnedWriteHalf,
        BufReader<OwnedReadHalf>,
        OwnedWriteHalf,
        BufReader<OwnedReadHalf>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (server_stream, _) = listener.accept().await.unwrap();
        let (cr, cw) = client_stream.into_split();
        let (sr, sw) = server_stream.into_split();
        (cw, BufReader::new(sr), sw, BufReader::new(cr))
    }

    fn make_entity(seed: u64) -> EntityState {
        EntityState::new(
            EntityId::new(seed),
            EntityKind::Player,
            Vec3::new(seed as f32, 0.0, seed as f32),
            0.0,
            100,
            100,
            EntityFlags::empty(),
        )
    }

    /// Build a GatewayAuth in skip-validation (dev) mode for tests that don't
    /// need real JWT or Redis.
    fn dev_auth() -> GatewayAuth {
        GatewayAuth {
            jwt_secret: b"test-secret".to_vec(),
            redis_client: redis::Client::open("redis://127.0.0.1:6379").unwrap(),
            skip_validation: true,
        }
    }

    /// Build a GatewayAuth in real-validation mode for rejection tests.
    /// Redis is not needed because the JWT decode fails before the blocklist
    /// check for invalid tokens.
    fn real_auth() -> GatewayAuth {
        GatewayAuth {
            jwt_secret: b"test-secret".to_vec(),
            redis_client: redis::Client::open("redis://127.0.0.1:6379").unwrap(),
            skip_validation: false,
        }
    }

    // ── Framing: write then read round-trip ──────────────────────────────────

    async fn framing_roundtrip(msg: &ClientMessage) -> (PacketHeader, Vec<u8>) {
        let (header_out, payload_out) = protocol::encode_client(msg, 7).unwrap();

        let (mut cw, mut sr, _sw, _cr) = tcp_duplex().await;
        write_tcp_message(&mut cw, &header_out, &payload_out)
            .await
            .unwrap();

        read_tcp_message(&mut sr).await.unwrap()
    }

    #[tokio::test]
    async fn framing_move_input_roundtrip() {
        let msg = ClientMessage::MoveInput(MoveInput {
            direction: Vec2::new(1.0, 0.0),
            speed: 5.5,
        });
        let (header_out, payload_out) = protocol::encode_client(&msg, 7).unwrap();
        let (header_in, payload_in) = framing_roundtrip(&msg).await;

        assert_eq!(header_in, header_out);
        assert_eq!(payload_in, payload_out);

        let decoded = protocol::decode_client(&header_in, &payload_in).unwrap();
        assert_eq!(decoded, msg);
    }

    #[tokio::test]
    async fn framing_empty_payload_disconnect() {
        let msg = ClientMessage::Disconnect;
        let (header_in, payload_in) = framing_roundtrip(&msg).await;
        assert_eq!(header_in.payload_len, 0);
        assert!(payload_in.is_empty());
        let decoded = protocol::decode_client(&header_in, &payload_in).unwrap();
        assert_eq!(decoded, ClientMessage::Disconnect);
    }

    #[tokio::test]
    async fn framing_large_world_snapshot() {
        let entities: Vec<EntityState> = (0u64..100).map(make_entity).collect();
        let snap = WorldSnapshot {
            tick: 9999,
            entities,
        };
        let server_msg = protocol::ServerMessage::WorldSnapshot(snap.clone());
        let (header_out, payload_out) = protocol::encode_server(&server_msg, 42).unwrap();

        let (mut cw, mut sr, _sw, _cr) = tcp_duplex().await;
        write_tcp_message(&mut cw, &header_out, &payload_out)
            .await
            .unwrap();

        let (header_in, payload_in) = read_tcp_message(&mut sr).await.unwrap();
        assert_eq!(header_in, header_out);
        assert_eq!(payload_in, payload_out);

        let decoded = protocol::decode_server(&header_in, &payload_in).unwrap();
        if let protocol::ServerMessage::WorldSnapshot(s) = decoded {
            assert_eq!(s.tick, 9999);
            assert_eq!(s.entities.len(), 100);
        } else {
            panic!("expected WorldSnapshot");
        }
    }

    #[tokio::test]
    async fn framing_multiple_messages_sequential() {
        let (mut cw, mut sr, _sw, _cr) = tcp_duplex().await;

        let msgs: Vec<ClientMessage> = vec![
            ClientMessage::Handshake(Handshake { token: "abc".into() }),
            ClientMessage::Ping(Ping { client_timestamp: 12345 }),
            ClientMessage::Disconnect,
        ];

        for m in &msgs {
            let (h, p) = protocol::encode_client(m, 0).unwrap();
            write_tcp_message(&mut cw, &h, &p).await.unwrap();
        }

        for expected in &msgs {
            let (h, p) = read_tcp_message(&mut sr).await.unwrap();
            let decoded = protocol::decode_client(&h, &p).unwrap();
            assert_eq!(&decoded, expected);
        }
    }

    // ── Routing tests ────────────────────────────────────────────────────────

    #[test]
    fn classify_move_input_is_immediate() {
        let msg = ClientMessage::MoveInput(MoveInput {
            direction: Vec2::new(0.0, 1.0),
            speed: 3.0,
        });
        assert_eq!(classify(&msg), MessageClass::Immediate);
    }

    #[test]
    fn classify_handshake_is_internal() {
        let msg = ClientMessage::Handshake(Handshake { token: "x".into() });
        assert_eq!(classify(&msg), MessageClass::Internal);
    }

    #[test]
    fn classify_ping_is_internal() {
        let msg = ClientMessage::Ping(Ping { client_timestamp: 0 });
        assert_eq!(classify(&msg), MessageClass::Internal);
    }

    #[test]
    fn classify_disconnect_is_internal() {
        assert_eq!(classify(&ClientMessage::Disconnect), MessageClass::Internal);
    }

    #[test]
    fn router_routes_move_input_to_immediate() {
        let handle = GatewayHandle::new();
        let router = handle.router.clone();
        let GatewayHandle { mut immediate_rx, mut deferred_rx, .. } = handle;

        let cmd = RoutedCommand {
            entity_id: EntityId::new(42),
            message: ClientMessage::MoveInput(MoveInput {
                direction: Vec2::new(1.0, 0.0),
                speed: 5.0,
            }),
        };
        router.route(cmd).unwrap();

        let received = immediate_rx.try_recv().expect("should have a command");
        assert_eq!(received.entity_id, EntityId::new(42));
        assert!(matches!(received.message, ClientMessage::MoveInput(_)));

        assert!(deferred_rx.try_recv().is_err());
    }

    #[test]
    fn routed_command_contains_correct_entity_id() {
        let handle = GatewayHandle::new();
        let router = handle.router.clone();
        let GatewayHandle { mut immediate_rx, .. } = handle;

        let entity_id = EntityId::new(999);
        let cmd = RoutedCommand {
            entity_id,
            message: ClientMessage::MoveInput(MoveInput {
                direction: Vec2::ZERO,
                speed: 0.0,
            }),
        };
        router.route(cmd).unwrap();

        let received = immediate_rx.try_recv().unwrap();
        assert_eq!(received.entity_id, EntityId::new(999));
    }

    #[test]
    fn router_channel_closed_returns_error() {
        let (immediate_tx, immediate_rx) = mpsc::unbounded_channel::<RoutedCommand>();
        let (deferred_tx, deferred_rx) = mpsc::unbounded_channel::<RoutedCommand>();
        let router = ClientRouter { immediate_tx, deferred_tx };

        drop(immediate_rx);
        drop(deferred_rx);

        let cmd = RoutedCommand {
            entity_id: EntityId::new(1),
            message: ClientMessage::MoveInput(MoveInput {
                direction: Vec2::ZERO,
                speed: 0.0,
            }),
        };
        let err = router.route(cmd);
        assert!(matches!(err, Err(GatewayError::ChannelClosed)));
    }

    // ── Handshake tests ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn handshake_accepted_for_nonempty_token() {
        use character_store::InMemoryCharacterStore;
        use common::{Race, Class};

        let (mut client_w, server_r, server_w, mut client_r) = tcp_duplex().await;
        let (udp_tx, _udp_rx) = mpsc::channel(8);
        let handle = GatewayHandle::new();
        let router = handle.router.clone();

        let (validated_tx, validated_rx) = oneshot::channel::<Option<String>>();
        let (auth_result_tx, auth_result_rx) =
            oneshot::channel::<Result<(), protocol::RejectReason>>();
        let (enter_zone_tx, mut enter_zone_rx) = mpsc::channel::<EnterZoneRequest>(1);
        let (snap_source_tx, snap_source_rx) =
            oneshot::channel::<Result<mpsc::Receiver<WorldSnapshot>, protocol::RejectReason>>();

        let char_store = Arc::new(InMemoryCharacterStore::new());
        // Pre-create a character so we can select it.
        let char_info = char_store
            .create("valid-token", "Hero", Race::Human, Class::Warrior)
            .await
            .unwrap();

        let config = ConnectionConfig {
            peer_addr: "127.0.0.1:9999".parse().unwrap(),
            udp_tx,
            router,
            auth: dev_auth(),
            entity_id: EntityId::new(1),
            character_store: char_store,
            validated_tx,
            auth_result_rx,
            enter_zone_tx,
            snapshot_rx_source: snap_source_rx,
        };

        tokio::spawn(run_connection(server_r.into_inner(), server_w, config));

        // Client sends Handshake.
        let hs = ClientMessage::Handshake(Handshake { token: "valid-token".into() });
        let (h, p) = protocol::encode_client(&hs, 1).unwrap();
        write_tcp_message(&mut client_w, &h, &p).await.unwrap();

        // Simulate game-server: read validation result, send auth OK.
        let valid = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            validated_rx,
        )
        .await
        .expect("timed out waiting for validated_rx")
        .unwrap();
        assert!(valid.is_some(), "expected validation to succeed");
        auth_result_tx.send(Ok(())).unwrap();

        // Client sends SelectCharacter.
        let select = ClientMessage::SelectCharacter(protocol::SelectCharacter {
            character_id: char_info.id.clone(),
        });
        let (h, p) = protocol::encode_client(&select, 2).unwrap();
        write_tcp_message(&mut client_w, &h, &p).await.unwrap();

        // Simulate game-server: receive EnterZoneRequest, provide snapshot channel.
        let enter_req = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            enter_zone_rx.recv(),
        )
        .await
        .expect("timed out waiting for enter_zone_rx")
        .expect("enter_zone channel closed");
        assert_eq!(enter_req.label, "Hero");

        let (_snap_tx, snap_rx) = mpsc::channel::<WorldSnapshot>(8);
        snap_source_tx.send(Ok(snap_rx)).unwrap();

        // Client should now receive HandshakeAccepted.
        let (rh, rp) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            read_tcp_message(&mut client_r),
        )
        .await
        .expect("timed out waiting for HandshakeAccepted")
        .unwrap();
        let reply = protocol::decode_server(&rh, &rp).unwrap();
        assert!(
            matches!(reply, protocol::ServerMessage::HandshakeAccepted(_)),
            "expected HandshakeAccepted, got {:?}",
            reply
        );
    }

    #[tokio::test]
    async fn handshake_rejected_for_invalid_token() {
        use character_store::InMemoryCharacterStore;

        let (mut client_w, server_r, server_w, mut client_r) = tcp_duplex().await;
        let (udp_tx, _udp_rx) = mpsc::channel(8);
        let handle = GatewayHandle::new();
        let router = handle.router.clone();

        let (validated_tx, _validated_rx) = oneshot::channel::<Option<String>>();
        let (_auth_result_tx, auth_result_rx) =
            oneshot::channel::<Result<(), protocol::RejectReason>>();
        let (enter_zone_tx, _enter_zone_rx) = mpsc::channel::<EnterZoneRequest>(1);
        let (_snap_source_tx, snap_source_rx) =
            oneshot::channel::<Result<mpsc::Receiver<WorldSnapshot>, protocol::RejectReason>>();

        let config = ConnectionConfig {
            peer_addr: "127.0.0.1:9998".parse().unwrap(),
            udp_tx,
            router,
            auth: real_auth(),
            entity_id: EntityId::new(1),
            character_store: Arc::new(InMemoryCharacterStore::new()),
            validated_tx,
            auth_result_rx,
            enter_zone_tx,
            snapshot_rx_source: snap_source_rx,
        };

        tokio::spawn(run_connection(server_r.into_inner(), server_w, config));

        // Client sends Handshake with an invalid (empty) token.
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
                protocol::ServerMessage::HandshakeRejected(protocol::HandshakeRejected {
                    reason: protocol::RejectReason::InvalidToken
                })
            ),
            "expected HandshakeRejected/InvalidToken, got {:?}",
            reply
        );
    }

    // ── UdpDispatch tests ────────────────────────────────────────────────────

    #[tokio::test]
    async fn udp_dispatch_sends_bytes_to_addr() {
        let recv_socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let recv_addr = recv_socket.local_addr().unwrap();
        let dispatch_socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let (udp_tx, udp_rx) = mpsc::channel::<UdpPacket>(8);
        tokio::spawn(run_udp_dispatch(dispatch_socket, udp_rx));

        let payload = Bytes::from_static(b"hello udp dispatch");
        udp_tx
            .send(UdpPacket { payload: payload.clone(), addr: recv_addr })
            .await
            .unwrap();

        let mut buf = [0u8; 128];
        let (n, _) = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            recv_socket.recv_from(&mut buf),
        )
        .await
        .expect("timed out waiting for UDP packet")
        .unwrap();

        assert_eq!(&buf[..n], b"hello udp dispatch");
    }

    #[tokio::test]
    async fn udp_dispatch_exits_when_channel_closed() {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (udp_tx, udp_rx) = mpsc::channel::<UdpPacket>(8);

        let task = tokio::spawn(run_udp_dispatch(socket, udp_rx));
        drop(udp_tx);

        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("dispatch task did not exit after channel close")
            .expect("task panicked");
    }
}
