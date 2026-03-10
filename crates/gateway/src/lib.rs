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

use std::net::SocketAddr;

use bytes::Bytes;
use common::{EntityId, ZoneId};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::tcp::{OwnedReadHalf, OwnedWriteHalf},
    sync::mpsc,
};
use tracing::{debug, warn};

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
///
/// A connection starts in [`SessionState::Handshaking`] and transitions to
/// [`SessionState::Connected`] once the gateway has validated the token and
/// admitted the client into a zone. Only `Connected` sessions may receive
/// routed zone commands.
#[derive(Debug, Clone)]
pub enum SessionState {
    /// Token not yet validated — waiting for the [`protocol::ClientMessage::Handshake`].
    Handshaking,

    /// Token accepted. The client controls `entity_id` inside `zone_id`.
    Connected { entity_id: EntityId, zone_id: ZoneId },
}

// ─────────────────────────────────────────────────────────────────────────────
// UDP dispatch
// ─────────────────────────────────────────────────────────────────────────────

/// A serialised payload destined for a specific UDP address.
///
/// Connection tasks produce these and forward them to the single
/// [`run_udp_dispatch`] task that owns the socket.
#[derive(Debug)]
pub struct UdpPacket {
    /// Pre-serialised bytes to send.
    pub payload: Bytes,
    /// Destination socket address of the client.
    pub addr: SocketAddr,
}

/// Owns the UDP socket exclusively and forwards outbound packets.
///
/// This is spawned once at server startup. Connection tasks send
/// `(payload, addr)` through the `rx` channel; this task calls
/// `socket.send_to` without any locking.
///
/// The loop ends when all senders are dropped (channel closed), allowing
/// graceful shutdown.
///
/// # Note
/// The socket is never shared — no `Arc` is needed. This is the only place
/// in the codebase that calls `send_to`.
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
///
/// The `game-server` crate reads from [`GatewayHandle::immediate_rx`] or
/// [`GatewayHandle::deferred_rx`] and forwards to the appropriate zone inbox
/// or async service.
#[derive(Debug)]
pub struct RoutedCommand {
    /// The entity that sent this message (only valid once [`SessionState::Connected`]).
    pub entity_id: EntityId,
    /// The decoded message from the client.
    pub message: protocol::ClientMessage,
}

/// How to route client messages out of the gateway.
///
/// `game-server` creates this (via [`GatewayHandle`]) and passes it into each
/// connection task. The gateway never depends on the concrete zone type — it
/// just pushes [`RoutedCommand`] values down these channels.
#[derive(Clone, Debug)]
pub struct ClientRouter {
    /// Immediate messages (movement, spell cast, interact) — forwarded to the
    /// zone inbox, processed on the current or next tick.
    pub immediate_tx: mpsc::UnboundedSender<RoutedCommand>,

    /// Deferred messages (chat, AH, mail) — forwarded to async service tasks;
    /// they never touch zone state directly.
    pub deferred_tx: mpsc::UnboundedSender<RoutedCommand>,
}

/// Classify a [`protocol::ClientMessage`] variant into immediate or deferred.
///
/// Rule:
/// - [`protocol::ClientMessage::MoveInput`] → **immediate** (zone inbox, this tick)
/// - [`protocol::ClientMessage::Handshake`] → handled internally by the connection
///   task; never forwarded.
/// - [`protocol::ClientMessage::Ping`] → handled internally (Pong reply).
/// - [`protocol::ClientMessage::Disconnect`] → handled internally.
///
/// As the protocol grows (Phase 3+), chat/AH/mail variants go to `deferred`.
/// For Phase 1 the only routed message is `MoveInput`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageClass {
    /// Route to the zone inbox via `immediate_tx`.
    Immediate,
    /// Route to an async service via `deferred_tx`.
    Deferred,
    /// Handled internally by the connection task — do not forward.
    Internal,
}

/// Classify a decoded client message.
pub fn classify(msg: &protocol::ClientMessage) -> MessageClass {
    match msg {
        protocol::ClientMessage::MoveInput(_) => MessageClass::Immediate,
        // Phase 1: Handshake, Ping, Disconnect are all handled inside the
        // connection task. No deferred messages exist yet in Phase 1.
        protocol::ClientMessage::Handshake(_)
        | protocol::ClientMessage::Ping(_)
        | protocol::ClientMessage::Disconnect => MessageClass::Internal,
    }
}

impl ClientRouter {
    /// Route a command to the correct channel based on its classification.
    ///
    /// Returns `Err(GatewayError::ChannelClosed)` if the receiving end has
    /// been dropped.
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
                // Internal messages are never passed to route(); callers should
                // handle them before calling this function.
                warn!("route() called with an internal message — dropping");
                Ok(())
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// GatewayHandle
// ─────────────────────────────────────────────────────────────────────────────

/// The public interface returned when the gateway is started.
///
/// `game-server` holds this and reads routed commands from the two receivers,
/// then forwards them to zone inboxes or service tasks.
pub struct GatewayHandle {
    /// Immediate commands (e.g. `MoveInput`) that must reach the zone inbox
    /// before or during the next tick.
    pub immediate_rx: mpsc::UnboundedReceiver<RoutedCommand>,

    /// Deferred commands (chat, AH, mail) forwarded to async service tasks.
    pub deferred_rx: mpsc::UnboundedReceiver<RoutedCommand>,

    /// Pre-wired router ready to hand to connection tasks.
    ///
    /// Clone it once per connection. The underlying unbounded channels are
    /// cheap to clone (just a sender handle increment).
    pub router: ClientRouter,
}

impl GatewayHandle {
    /// Create a new `GatewayHandle`, returning both the handle and the router
    /// that connection tasks will use.
    ///
    /// Typically called once at server startup inside `game-server`.
    ///
    /// ```rust
    /// let handle = gateway::GatewayHandle::new();
    /// // hand handle.router.clone() to each ConnectionTask
    /// // read handle.immediate_rx / handle.deferred_rx in the coordinator
    /// ```
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

/// Size of the 4-byte little-endian frame-length prefix.
const FRAME_LEN_SIZE: usize = 4;

/// Read one length-prefixed TCP frame and return the decoded header and payload.
///
/// # Frame format
/// ```text
/// ┌──────────────┬──────────────────────────┬──────────────┐
/// │  frame_len   │      PacketHeader        │   payload    │
/// │   u32 LE     │       11 bytes           │  N bytes     │
/// │   4 bytes    │  (counted in frame_len)  │              │
/// └──────────────┴──────────────────────────┴──────────────┘
/// frame_len = HEADER_SIZE + payload_len
/// ```
///
/// The function reads exactly `frame_len` bytes after the prefix and decodes
/// the header, then slices the payload from what remains.
///
/// # Errors
/// - [`GatewayError::ConnectionClosed`] if the peer closed the connection.
/// - [`GatewayError::Io`] for other I/O errors.
/// - [`GatewayError::Protocol`] if the header cannot be decoded.
pub async fn read_tcp_message(
    stream: &mut BufReader<OwnedReadHalf>,
) -> Result<(protocol::PacketHeader, Vec<u8>), GatewayError> {
    // Read 4-byte frame length prefix.
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
        // Frame is smaller than a header — definitely malformed.
        return Err(GatewayError::Protocol(protocol::ProtocolError::HeaderTooShort));
    }

    // Read the rest of the frame (header + payload).
    let mut frame = vec![0u8; frame_len];
    match stream.read_exact(&mut frame).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(GatewayError::ConnectionClosed);
        }
        Err(e) => return Err(GatewayError::Io(e)),
    }

    // Decode the 11-byte header from the front of the frame.
    let header = protocol::PacketHeader::decode_slice(&frame)?;
    let payload = frame[protocol::HEADER_SIZE..].to_vec();

    Ok((header, payload))
}

/// Write one length-prefixed TCP frame.
///
/// # Frame format
/// Same as [`read_tcp_message`]: 4-byte LE length prefix, then header, then payload.
///
/// # Errors
/// - [`GatewayError::Io`] for I/O errors.
pub async fn write_tcp_message(
    stream: &mut OwnedWriteHalf,
    header: &protocol::PacketHeader,
    payload: &[u8],
) -> Result<(), GatewayError> {
    let header_bytes = header.encode();
    let frame_len = (protocol::HEADER_SIZE + payload.len()) as u32;

    // Write frame length prefix, header, and payload as a single gathered write
    // to avoid partial-frame problems.
    let mut buf =
        Vec::with_capacity(FRAME_LEN_SIZE + protocol::HEADER_SIZE + payload.len());
    buf.extend_from_slice(&frame_len.to_le_bytes());
    buf.extend_from_slice(&header_bytes);
    buf.extend_from_slice(payload);

    stream.write_all(&buf).await?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// ConnectionTask
// ─────────────────────────────────────────────────────────────────────────────

/// Configuration required to spawn a [`run_connection`] task.
pub struct ConnectionConfig {
    /// Remote address of this client (used for UDP packet addressing).
    pub peer_addr: SocketAddr,

    /// Channel to the [`run_udp_dispatch`] task for outbound UDP packets.
    pub udp_tx: mpsc::Sender<UdpPacket>,

    /// Pre-built router for forwarding decoded client messages.
    pub router: ClientRouter,

    /// Receiver for [`protocol::WorldSnapshot`] messages coming from the zone.
    ///
    /// The zone sends a snapshot every tick; the connection task serialises it
    /// and forwards it to `udp_tx` for dispatch.
    pub snapshot_rx: mpsc::Receiver<protocol::WorldSnapshot>,
}

/// Run the async task that manages one TCP connection.
///
/// This task:
/// 1. Drives the session state machine (Handshaking → Connected).
/// 2. Reads inbound TCP frames via [`read_tcp_message`] and routes them:
///    - `Ping` → immediate `Pong` reply over TCP.
///    - `MoveInput` → forwarded to `router.immediate_tx` (zone inbox).
///    - `Disconnect` → task exits.
///    - `Handshake` → validated here (stub: any non-empty token accepted).
/// 3. Forwards incoming `WorldSnapshot` values from `snapshot_rx` to the UDP
///    dispatch task.
///
/// The task exits on disconnect, I/O error, or channel closure.
///
/// # Stub auth note
/// Phase 1 does not have a real auth server. Any non-empty token string is
/// accepted and a fixed `EntityId(1)` / `ZoneId(1)` is assigned. Phase 2 will
/// replace this with JWT validation + Redis blocklist check.
pub async fn run_connection(
    tcp_read: OwnedReadHalf,
    mut tcp_write: OwnedWriteHalf,
    config: ConnectionConfig,
) {
    let ConnectionConfig {
        peer_addr,
        udp_tx,
        router,
        mut snapshot_rx,
    } = config;

    let mut reader = BufReader::new(tcp_read);
    let mut session = SessionState::Handshaking;
    // Per-connection monotonically increasing sequence counter.
    let mut seq: u32 = 0;

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
                        warn!(addr = %peer_addr, error = %e, "tcp read error, dropping connection");
                        break;
                    }
                    Ok((header, payload)) => {
                        match handle_inbound(
                            &header,
                            &payload,
                            &mut session,
                            &mut seq,
                            &mut tcp_write,
                            &router,
                            peer_addr,
                        )
                        .await
                        {
                            Ok(KeepGoing::Yes) => {}
                            Ok(KeepGoing::No) => break,
                            Err(e) => {
                                warn!(addr = %peer_addr, error = %e, "error handling message");
                                break;
                            }
                        }
                    }
                }
            }

            // Outbound WorldSnapshot from the zone — forward via UDP.
            snapshot = snapshot_rx.recv() => {
                match snapshot {
                    None => {
                        debug!(addr = %peer_addr, "snapshot channel closed, dropping connection");
                        break;
                    }
                    Some(snap) => {
                        if let Err(e) = forward_snapshot(snap, seq, peer_addr, &udp_tx).await {
                            warn!(addr = %peer_addr, error = %e, "failed to forward snapshot");
                            // Non-fatal: UDP is best-effort. Continue.
                        }
                        seq = seq.wrapping_add(1);
                    }
                }
            }
        }
    }
}

/// Drives the per-message logic for one inbound TCP frame.
async fn handle_inbound(
    header: &protocol::PacketHeader,
    payload: &[u8],
    session: &mut SessionState,
    seq: &mut u32,
    tcp_write: &mut OwnedWriteHalf,
    router: &ClientRouter,
    peer_addr: SocketAddr,
) -> Result<KeepGoing, GatewayError> {
    let msg = protocol::decode_client(header, payload)?;

    match (&session, &msg) {
        // ── Handshake ────────────────────────────────────────────────────
        (SessionState::Handshaking, protocol::ClientMessage::Handshake(hs)) => {
            let (accepted, entity_id, zone_id) = validate_token(&hs.token);
            *seq = seq.wrapping_add(1);

            if accepted {
                *session = SessionState::Connected { entity_id, zone_id };
                let reply = protocol::ServerMessage::HandshakeAccepted(
                    protocol::HandshakeAccepted { entity_id, zone_id },
                );
                let (hdr, pl) = protocol::encode_server(&reply, *seq)?;
                write_tcp_message(tcp_write, &hdr, &pl).await?;
                debug!(addr = %peer_addr, entity = ?entity_id, zone = ?zone_id, "handshake accepted");
            } else {
                let reply = protocol::ServerMessage::HandshakeRejected(
                    protocol::HandshakeRejected {
                        reason: protocol::RejectReason::InvalidToken,
                    },
                );
                let (hdr, pl) = protocol::encode_server(&reply, *seq)?;
                write_tcp_message(tcp_write, &hdr, &pl).await?;
                debug!(addr = %peer_addr, "handshake rejected — invalid token");
                return Ok(KeepGoing::No);
            }
        }

        // ── Ping ─────────────────────────────────────────────────────────
        (_, protocol::ClientMessage::Ping(ping)) => {
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

        // ── Disconnect ───────────────────────────────────────────────────
        (_, protocol::ClientMessage::Disconnect) => {
            debug!(addr = %peer_addr, "client sent Disconnect");
            return Ok(KeepGoing::No);
        }

        // ── MoveInput (and future immediate messages) ─────────────────────
        (SessionState::Connected { entity_id, .. }, _) => {
            let class = classify(&msg);
            if class != MessageClass::Internal {
                let cmd = RoutedCommand {
                    entity_id: *entity_id,
                    message: msg,
                };
                router.route(cmd)?;
            }
        }

        // ── Message before handshake (non-handshake message while Handshaking)
        (SessionState::Handshaking, other) => {
            warn!(addr = %peer_addr, msg = ?other, "received non-Handshake before session established — dropping");
            return Ok(KeepGoing::No);
        }
    }

    Ok(KeepGoing::Yes)
}

/// Serialise and forward a [`protocol::WorldSnapshot`] to the UDP dispatch task.
async fn forward_snapshot(
    snapshot: protocol::WorldSnapshot,
    seq: u32,
    peer_addr: SocketAddr,
    udp_tx: &mpsc::Sender<UdpPacket>,
) -> Result<(), GatewayError> {
    let msg = protocol::ServerMessage::WorldSnapshot(snapshot);
    let (header, payload) = protocol::encode_server(&msg, seq)?;

    // Build the wire frame: 4-byte length prefix + header + payload.
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
// Token validation stub (Phase 1)
// ─────────────────────────────────────────────────────────────────────────────

/// Phase 1 stub: accept any non-empty token and assign fixed ids.
///
/// Phase 2 will replace this with JWT verification against the auth-server's
/// public key, then check the token ID against the Redis blocklist.
fn validate_token(token: &str) -> (bool, EntityId, ZoneId) {
    if token.is_empty() {
        (false, EntityId::new(0), ZoneId::new(0))
    } else {
        // In Phase 1 every client gets entity 1 in zone 1. The real
        // implementation will extract entity/zone from the JWT claims.
        (true, EntityId::new(1), ZoneId::new(1))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Signal returned from [`handle_inbound`] to tell the connection loop whether
/// to continue or exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeepGoing {
    Yes,
    No,
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use common::{EntityFlags, EntityId, EntityKind, EntityState, Vec2, Vec3, ZoneId};
    use protocol::{ClientMessage, Handshake, MoveInput, PacketHeader, Ping, WorldSnapshot};
    use tokio::io::duplex;

    // ── Helpers ──────────────────────────────────────────────────────────────

    /// Build a `BufReader<OwnedReadHalf>` + `OwnedWriteHalf` pair backed by an
    /// in-memory duplex stream.
    fn duplex_pair(max_buf: usize) -> (BufReader<OwnedReadHalf>, OwnedWriteHalf) {
        // tokio::io::duplex gives us a (write_end, read_end) for an in-memory pipe.
        let (client_side, server_side) = duplex(max_buf);
        // We need a real TcpStream split, but duplex gives DuplexStream.
        // Use the tokio::net::tcp::OwnedReadHalf path by going through a
        // loopback TCP connection instead.
        // For unit tests we wrap duplex in a helper that does the same job.
        drop((client_side, server_side));
        panic!("use tcp_duplex_pair() instead");
    }

    /// Spin up a loopback TCP connection and return split halves for both ends.
    /// Returns (writer_to_server, reader_from_server, writer_to_client, reader_from_client).
    async fn tcp_duplex() -> (
        OwnedWriteHalf,              // client writes here (goes to server read)
        BufReader<OwnedReadHalf>,    // server reads here
        OwnedWriteHalf,              // server writes here (goes to client read)
        BufReader<OwnedReadHalf>,    // client reads here
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

    // ── Framing: write then read round-trip ──────────────────────────────────

    /// Write a message with write_tcp_message, read it back with read_tcp_message.
    async fn framing_roundtrip(
        msg: &ClientMessage,
    ) -> (PacketHeader, Vec<u8>) {
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
        // Build a snapshot with 100 entities.
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

        // Write three messages back-to-back.
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
        // Use a separate GatewayHandle to hold the receivers so we can inspect them.
        let GatewayHandle { mut immediate_rx, mut deferred_rx, .. } = handle;

        let cmd = RoutedCommand {
            entity_id: EntityId::new(42),
            message: ClientMessage::MoveInput(MoveInput {
                direction: Vec2::new(1.0, 0.0),
                speed: 5.0,
            }),
        };
        router.route(cmd).unwrap();

        // immediate_rx should have it.
        let received = immediate_rx.try_recv().expect("should have a command");
        assert_eq!(received.entity_id, EntityId::new(42));
        assert!(matches!(received.message, ClientMessage::MoveInput(_)));

        // deferred_rx should be empty.
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

        // Drop the receivers so the channels close.
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

    // ── Handshake session state test ─────────────────────────────────────────

    #[tokio::test]
    async fn handshake_accepted_for_nonempty_token() {
        // Wire a full connection task and drive a handshake through it.
        let (mut client_w, server_r, server_w, mut client_r) = tcp_duplex().await;
        let (udp_tx, _udp_rx) = mpsc::channel(8);
        let handle = GatewayHandle::new();
        let router = handle.router.clone();
        let (_snap_tx, snap_rx) = mpsc::channel::<WorldSnapshot>(8);

        let config = ConnectionConfig {
            peer_addr: "127.0.0.1:9999".parse().unwrap(),
            udp_tx,
            router,
            snapshot_rx: snap_rx,
        };

        // Spawn the connection task.
        tokio::spawn(run_connection(
            // unwrap the owned halves from the BufReader
            server_r.into_inner(),
            server_w,
            config,
        ));

        // Client sends a Handshake with a valid (non-empty) token.
        let hs = ClientMessage::Handshake(Handshake { token: "valid-token".into() });
        let (h, p) = protocol::encode_client(&hs, 1).unwrap();
        write_tcp_message(&mut client_w, &h, &p).await.unwrap();

        // Client should receive HandshakeAccepted.
        let (rh, rp) = read_tcp_message(&mut client_r).await.unwrap();
        let reply = protocol::decode_server(&rh, &rp).unwrap();
        assert!(
            matches!(reply, protocol::ServerMessage::HandshakeAccepted(_)),
            "expected HandshakeAccepted, got {:?}",
            reply
        );
    }

    #[tokio::test]
    async fn handshake_rejected_for_empty_token() {
        let (mut client_w, server_r, server_w, mut client_r) = tcp_duplex().await;
        let (udp_tx, _udp_rx) = mpsc::channel(8);
        let handle = GatewayHandle::new();
        let router = handle.router.clone();
        let (_snap_tx, snap_rx) = mpsc::channel::<WorldSnapshot>(8);

        let config = ConnectionConfig {
            peer_addr: "127.0.0.1:9998".parse().unwrap(),
            udp_tx,
            router,
            snapshot_rx: snap_rx,
        };

        tokio::spawn(run_connection(server_r.into_inner(), server_w, config));

        // Client sends a Handshake with an empty token.
        let hs = ClientMessage::Handshake(Handshake { token: "".into() });
        let (h, p) = protocol::encode_client(&hs, 1).unwrap();
        write_tcp_message(&mut client_w, &h, &p).await.unwrap();

        let (rh, rp) = read_tcp_message(&mut client_r).await.unwrap();
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

    // ── UdpDispatch test ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn udp_dispatch_sends_bytes_to_addr() {
        // Bind a recv socket to a random port.
        let recv_socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let recv_addr = recv_socket.local_addr().unwrap();

        // Bind the dispatch socket.
        let dispatch_socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let (udp_tx, udp_rx) = mpsc::channel::<UdpPacket>(8);

        // Spawn the dispatch task.
        tokio::spawn(run_udp_dispatch(dispatch_socket, udp_rx));

        let payload = Bytes::from_static(b"hello udp dispatch");
        udp_tx
            .send(UdpPacket { payload: payload.clone(), addr: recv_addr })
            .await
            .unwrap();

        // Receive on the other end.
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

        // Drop the sender to close the channel.
        drop(udp_tx);

        // Task should finish cleanly within a short timeout.
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("dispatch task did not exit after channel close")
            .expect("task panicked");
    }
}
