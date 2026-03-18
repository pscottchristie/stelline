//! Wire protocol for Stelline.
//!
//! # Design
//! - All messages are framed with a fixed 11-byte [`PacketHeader`].
//! - Payloads are serialized with MessagePack via `rmp_serde` (named format, cross-language friendly).
//! - TCP carries control messages; UDP carries [`ServerMessage::WorldSnapshot`].
//! - The `msg_type` u16 namespace is partitioned by build phase so future
//!   additions never collide with existing constants.

use common::{CharacterId, CharacterInfo, Class, EntityId, EntityState, Race, Vec2, Vec3, ZoneId};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// JWT claims (shared between login-server and gateway)
// ---------------------------------------------------------------------------

/// JWT claims issued by the login-server and validated by the gateway.
///
/// The `jti` (JWT ID) is checked against the Redis blocklist on connect to
/// support immediate token revocation without waiting for expiry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthClaims {
    /// Account UUID (subject).
    pub sub: String,
    /// Unique token ID — stored in Redis to revoke a specific token.
    pub jti: String,
    /// Issued-at time (Unix seconds).
    pub iat: usize,
    /// Expiry time (Unix seconds).
    pub exp: usize,
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum ProtocolError {
    #[error("unknown message type: {0:#06x}")]
    UnknownMessageType(u16),

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("header too short")]
    HeaderTooShort,
}

// ---------------------------------------------------------------------------
// Message type discriminants (u16)
// ---------------------------------------------------------------------------

/// Numeric constants for the `msg_type` field of [`PacketHeader`].
///
/// Namespace layout:
/// - `0x0001–0x00FF` : Client → Server, Phase 1
/// - `0x0101–0x01FF` : Server → Client, Phase 1
/// - `0x02xx–0x0Fxx` : Reserved for Phases 2–6 (see CLAUDE.md for future types)
pub mod msg_type {
    // Client → Server (Phase 1)
    pub const HANDSHAKE: u16 = 0x0001;
    pub const PING: u16 = 0x0002;
    pub const MOVE_INPUT: u16 = 0x0003;
    pub const DISCONNECT: u16 = 0x0004;

    // Server → Client (Phase 1)
    pub const HANDSHAKE_ACCEPTED: u16 = 0x0101;
    pub const HANDSHAKE_REJECTED: u16 = 0x0102;
    pub const PONG: u16 = 0x0103;
    pub const WORLD_SNAPSHOT: u16 = 0x0104;
    pub const ZONE_TRANSFER: u16 = 0x0105;

    // Client → Server (Phase 2 — Character selection)
    pub const CHARACTER_LIST_REQUEST: u16 = 0x0200;
    pub const CREATE_CHARACTER: u16 = 0x0201;
    pub const DELETE_CHARACTER: u16 = 0x0202;
    pub const SELECT_CHARACTER: u16 = 0x0203;

    // Server → Client (Phase 2 — Character selection)
    pub const CHARACTER_LIST: u16 = 0x0210;
    pub const CHARACTER_CREATED: u16 = 0x0211;
    pub const CHARACTER_DELETED: u16 = 0x0212;
    pub const CHARACTER_CREATE_FAILED: u16 = 0x0213;
}

// ---------------------------------------------------------------------------
// Packet header
// ---------------------------------------------------------------------------

/// Fixed 11-byte framing header prepended to every message.
///
/// Wire layout (all little-endian):
/// ```text
/// Offset  Size  Field
/// 0       1     version
/// 1       2     msg_type
/// 3       4     sequence
/// 7       4     payload_len  (bytes following the header; excludes header)
/// ```
/// Total: 11 bytes (1 + 2 + 4 + 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketHeader {
    /// Protocol version. Currently `1`. Increment when the wire format changes
    /// in a backwards-incompatible way.
    pub version: u8,
    /// Message type discriminant — see [`msg_type`] constants.
    pub msg_type: u16,
    /// Monotonically increasing sequence number per sender. Used by UDP consumers
    /// to detect out-of-order delivery and discard stale snapshots.
    pub sequence: u32,
    /// Byte length of the payload that follows the header. Does not include the
    /// header itself. A value of `0` is valid for messages with no payload
    /// (e.g. [`ClientMessage::Disconnect`]).
    pub payload_len: u32,
}

/// The on-wire size of a [`PacketHeader`] in bytes.
pub const HEADER_SIZE: usize = 11; // 1 + 2 + 4 + 4

impl PacketHeader {
    /// The protocol version this build produces.
    pub const CURRENT_VERSION: u8 = 1;

    /// Encode the header into its 11-byte wire representation (little-endian).
    pub fn encode(&self) -> [u8; HEADER_SIZE] {
        let mut buf = [0u8; HEADER_SIZE];
        buf[0] = self.version;
        buf[1..3].copy_from_slice(&self.msg_type.to_le_bytes());
        buf[3..7].copy_from_slice(&self.sequence.to_le_bytes());
        buf[7..11].copy_from_slice(&self.payload_len.to_le_bytes());
        buf
    }

    /// Decode a header from its 11-byte wire representation.
    pub fn decode(bytes: &[u8; HEADER_SIZE]) -> Self {
        let version = bytes[0];
        let msg_type = u16::from_le_bytes([bytes[1], bytes[2]]);
        let sequence = u32::from_le_bytes([bytes[3], bytes[4], bytes[5], bytes[6]]);
        let payload_len = u32::from_le_bytes([bytes[7], bytes[8], bytes[9], bytes[10]]);
        Self {
            version,
            msg_type,
            sequence,
            payload_len,
        }
    }

    /// Try to decode from a byte slice of unknown length.
    /// Returns [`ProtocolError::HeaderTooShort`] if fewer than [`HEADER_SIZE`]
    /// bytes are available.
    pub fn decode_slice(bytes: &[u8]) -> Result<Self, ProtocolError> {
        if bytes.len() < HEADER_SIZE {
            return Err(ProtocolError::HeaderTooShort);
        }
        let arr: &[u8; HEADER_SIZE] = bytes[..HEADER_SIZE]
            .try_into()
            .expect("slice length checked above");
        Ok(Self::decode(arr))
    }
}

// ---------------------------------------------------------------------------
// Client → Server messages
// ---------------------------------------------------------------------------

/// Sent immediately after the TCP connection is established.
/// The gateway validates the JWT; on success it admits the client into a zone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Handshake {
    /// JWT issued by the login-server.
    pub token: String,
}

/// Latency probe. The gateway echoes both timestamps back in a [`Pong`].
/// Never reaches the game tick loop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ping {
    /// Client's local clock at send time (milliseconds since Unix epoch, or any
    /// monotonic origin — the server treats it as an opaque u64).
    pub client_timestamp: u64,
}

/// Player movement intent. Sent over UDP every client tick (~60 Hz).
/// The server applies these inputs during the zone tick's "drain inbox" stage
/// and computes the authoritative new position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MoveInput {
    /// Desired movement direction in the XZ plane, expressed as a unit vector.
    /// `(0, 0)` means "stop". The server normalises this before applying it.
    pub direction: Vec2,
    /// Requested movement speed in world-units per second. The server clamps
    /// this to the entity's authoritative max speed to prevent speed hacks.
    pub speed: f32,
}

// ---------------------------------------------------------------------------
// Client → Server messages (Phase 2 — Character selection)
// ---------------------------------------------------------------------------

/// Request the list of characters for the authenticated account.
/// No payload — the account is identified from the validated JWT.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CharacterListRequest;

/// Create a new character on the authenticated account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateCharacter {
    pub name: String,
    pub race: Race,
    pub class: Class,
}

/// Delete a character owned by the authenticated account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeleteCharacter {
    pub character_id: CharacterId,
}

/// Select a character to enter the world with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectCharacter {
    pub character_id: CharacterId,
}

/// Top-level enum for all client-originated messages.
///
/// The gateway pattern-matches on this to route each variant to the correct
/// destination (zone inbox or async service channel).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientMessage {
    Handshake(Handshake),
    Ping(Ping),
    MoveInput(MoveInput),
    /// Client is gracefully disconnecting. No payload.
    Disconnect,
    // Phase 2 — Character selection
    CharacterListRequest(CharacterListRequest),
    CreateCharacter(CreateCharacter),
    DeleteCharacter(DeleteCharacter),
    SelectCharacter(SelectCharacter),
}

// ---------------------------------------------------------------------------
// Server → Client messages
// ---------------------------------------------------------------------------

/// Sent when the gateway accepts the client's [`Handshake`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandshakeAccepted {
    /// The ECS entity the client now controls.
    pub entity_id: EntityId,
    /// The zone the entity is currently in. The client uses this to fetch zone
    /// metadata (name, map data, etc.) before rendering begins.
    pub zone_id: ZoneId,
}

/// Reason the gateway rejected a [`Handshake`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectReason {
    /// The JWT is malformed or its signature is invalid.
    InvalidToken,
    /// The JWT is well-formed but past its expiry time.
    ExpiredToken,
    /// The realm has reached its player capacity.
    ServerFull,
    /// Another session with this account is already connected.
    AlreadyConnected,
}

/// Sent when the gateway rejects the client's [`Handshake`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandshakeRejected {
    pub reason: RejectReason,
}

/// Reply to a client [`Ping`]. Carries both timestamps so the client can
/// compute round-trip time and clock offset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pong {
    /// Echoed from the originating [`Ping`].
    pub client_timestamp: u64,
    /// Server's clock at the time it generated this reply (milliseconds since
    /// Unix epoch). Used by the client to estimate server time.
    pub server_timestamp: u64,
}

/// Full snapshot of every entity in the client's Area of Interest (AOI).
///
/// Snapshot semantics:
/// - Entity **present** → render it (update if seen before, spawn if new).
/// - Entity **absent** → it left the AOI; stop rendering (implicit despawn).
///
/// Sent over UDP every server tick (50 ms / 20 Hz target). A lost packet is
/// harmless — the next tick contains the full current state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldSnapshot {
    /// Monotonically increasing server tick counter. Clients SHOULD discard
    /// snapshots whose `tick` is older than the last rendered one (can arrive
    /// out of order on UDP).
    pub tick: u64,
    /// All entities visible to this specific client this tick.
    pub entities: Vec<EntityState>,
}

/// Tells the client it is being moved to another zone.
/// Sent over TCP (reliable) so it is guaranteed to arrive before the client's
/// new zone starts sending snapshots.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoneTransfer {
    /// The destination zone.
    pub zone_id: ZoneId,
    /// The position the entity will have upon entering the new zone.
    pub position: Vec3,
}

// ---------------------------------------------------------------------------
// Server → Client messages (Phase 2 — Character selection)
// ---------------------------------------------------------------------------

/// Response to [`CharacterListRequest`] — the list of characters on the account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CharacterList {
    pub characters: Vec<CharacterInfo>,
}

/// Confirmation that a character was created successfully.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CharacterCreated {
    pub character: CharacterInfo,
}

/// Confirmation that a character was deleted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CharacterDeleted {
    pub character_id: CharacterId,
}

/// A character creation request failed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CharacterCreateFailed {
    pub reason: String,
}

/// Top-level enum for all server-originated messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerMessage {
    HandshakeAccepted(HandshakeAccepted),
    HandshakeRejected(HandshakeRejected),
    Pong(Pong),
    WorldSnapshot(WorldSnapshot),
    ZoneTransfer(ZoneTransfer),
    // Phase 2 — Character selection
    CharacterList(CharacterList),
    CharacterCreated(CharacterCreated),
    CharacterDeleted(CharacterDeleted),
    CharacterCreateFailed(CharacterCreateFailed),
}

// ---------------------------------------------------------------------------
// Serialization helpers (MessagePack via rmp-serde)
// ---------------------------------------------------------------------------

fn serialize<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, ProtocolError> {
    rmp_serde::to_vec_named(value).map_err(|e| ProtocolError::Serialization(e.to_string()))
}

fn deserialize<'a, T: serde::Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, ProtocolError> {
    rmp_serde::from_slice(bytes).map_err(|e| ProtocolError::Serialization(e.to_string()))
}

// ---------------------------------------------------------------------------
// Encode helpers
// ---------------------------------------------------------------------------

/// Returns the `msg_type` constant for a [`ClientMessage`] variant.
fn client_msg_type(msg: &ClientMessage) -> u16 {
    match msg {
        ClientMessage::Handshake(_) => msg_type::HANDSHAKE,
        ClientMessage::Ping(_) => msg_type::PING,
        ClientMessage::MoveInput(_) => msg_type::MOVE_INPUT,
        ClientMessage::Disconnect => msg_type::DISCONNECT,
        ClientMessage::CharacterListRequest(_) => msg_type::CHARACTER_LIST_REQUEST,
        ClientMessage::CreateCharacter(_) => msg_type::CREATE_CHARACTER,
        ClientMessage::DeleteCharacter(_) => msg_type::DELETE_CHARACTER,
        ClientMessage::SelectCharacter(_) => msg_type::SELECT_CHARACTER,
    }
}

/// Returns the `msg_type` constant for a [`ServerMessage`] variant.
fn server_msg_type(msg: &ServerMessage) -> u16 {
    match msg {
        ServerMessage::HandshakeAccepted(_) => msg_type::HANDSHAKE_ACCEPTED,
        ServerMessage::HandshakeRejected(_) => msg_type::HANDSHAKE_REJECTED,
        ServerMessage::Pong(_) => msg_type::PONG,
        ServerMessage::WorldSnapshot(_) => msg_type::WORLD_SNAPSHOT,
        ServerMessage::ZoneTransfer(_) => msg_type::ZONE_TRANSFER,
        ServerMessage::CharacterList(_) => msg_type::CHARACTER_LIST,
        ServerMessage::CharacterCreated(_) => msg_type::CHARACTER_CREATED,
        ServerMessage::CharacterDeleted(_) => msg_type::CHARACTER_DELETED,
        ServerMessage::CharacterCreateFailed(_) => msg_type::CHARACTER_CREATE_FAILED,
    }
}

// ---------------------------------------------------------------------------
// Public encode/decode API
// ---------------------------------------------------------------------------

/// Encode a [`ClientMessage`] into a framed packet.
///
/// Returns `(header, payload_bytes)`. Callers write both to the wire in order.
/// The `sequence` value should be a per-connection monotonically increasing
/// counter; it is echoed into the header for the receiver to use.
///
/// # Errors
/// Returns [`ProtocolError::Serialization`] if serialization fails.
/// This should never happen for well-formed message types.
pub fn encode_client(
    msg: &ClientMessage,
    sequence: u32,
) -> Result<(PacketHeader, Vec<u8>), ProtocolError> {
    // Serialize only the inner struct, not the outer enum discriminant.
    // The msg_type field in the header carries the discriminant on the wire.
    let payload: Vec<u8> = match msg {
        ClientMessage::Handshake(inner) => serialize(inner)?,
        ClientMessage::Ping(inner) => serialize(inner)?,
        ClientMessage::MoveInput(inner) => serialize(inner)?,
        ClientMessage::Disconnect => Vec::new(),
        ClientMessage::CharacterListRequest(_) => Vec::new(),
        ClientMessage::CreateCharacter(inner) => serialize(inner)?,
        ClientMessage::DeleteCharacter(inner) => serialize(inner)?,
        ClientMessage::SelectCharacter(inner) => serialize(inner)?,
    };

    let header = PacketHeader {
        version: PacketHeader::CURRENT_VERSION,
        msg_type: client_msg_type(msg),
        sequence,
        payload_len: payload.len() as u32,
    };

    Ok((header, payload))
}

/// Encode a [`ServerMessage`] into a framed packet.
///
/// Returns `(header, payload_bytes)`. See [`encode_client`] for usage notes.
pub fn encode_server(
    msg: &ServerMessage,
    sequence: u32,
) -> Result<(PacketHeader, Vec<u8>), ProtocolError> {
    // Serialize only the inner struct, not the outer enum discriminant.
    // The msg_type field in the header carries the discriminant on the wire.
    let payload = match msg {
        ServerMessage::HandshakeAccepted(inner) => serialize(inner)?,
        ServerMessage::HandshakeRejected(inner) => serialize(inner)?,
        ServerMessage::Pong(inner) => serialize(inner)?,
        ServerMessage::WorldSnapshot(inner) => serialize(inner)?,
        ServerMessage::ZoneTransfer(inner) => serialize(inner)?,
        ServerMessage::CharacterList(inner) => serialize(inner)?,
        ServerMessage::CharacterCreated(inner) => serialize(inner)?,
        ServerMessage::CharacterDeleted(inner) => serialize(inner)?,
        ServerMessage::CharacterCreateFailed(inner) => serialize(inner)?,
    };

    let header = PacketHeader {
        version: PacketHeader::CURRENT_VERSION,
        msg_type: server_msg_type(msg),
        sequence,
        payload_len: payload.len() as u32,
    };

    Ok((header, payload))
}

/// Decode a [`ClientMessage`] given a previously decoded header and the
/// payload bytes that followed it on the wire.
///
/// # Errors
/// - [`ProtocolError::UnknownMessageType`] if `header.msg_type` is not a
///   recognised client message type.
/// - [`ProtocolError::Serialization`] if deserialization fails.
pub fn decode_client(
    header: &PacketHeader,
    payload: &[u8],
) -> Result<ClientMessage, ProtocolError> {
    let msg = match header.msg_type {
        msg_type::HANDSHAKE => {
            let inner: Handshake = deserialize(payload)?;
            ClientMessage::Handshake(inner)
        }
        msg_type::PING => {
            let inner: Ping = deserialize(payload)?;
            ClientMessage::Ping(inner)
        }
        msg_type::MOVE_INPUT => {
            let inner: MoveInput = deserialize(payload)?;
            ClientMessage::MoveInput(inner)
        }
        msg_type::DISCONNECT => ClientMessage::Disconnect,
        msg_type::CHARACTER_LIST_REQUEST => {
            ClientMessage::CharacterListRequest(CharacterListRequest)
        }
        msg_type::CREATE_CHARACTER => {
            let inner: CreateCharacter = deserialize(payload)?;
            ClientMessage::CreateCharacter(inner)
        }
        msg_type::DELETE_CHARACTER => {
            let inner: DeleteCharacter = deserialize(payload)?;
            ClientMessage::DeleteCharacter(inner)
        }
        msg_type::SELECT_CHARACTER => {
            let inner: SelectCharacter = deserialize(payload)?;
            ClientMessage::SelectCharacter(inner)
        }
        unknown => return Err(ProtocolError::UnknownMessageType(unknown)),
    };
    Ok(msg)
}

/// Decode a [`ServerMessage`] given a previously decoded header and the
/// payload bytes that followed it on the wire.
///
/// # Errors
/// - [`ProtocolError::UnknownMessageType`] if `header.msg_type` is not a
///   recognised server message type.
/// - [`ProtocolError::Serialization`] if deserialization fails.
pub fn decode_server(
    header: &PacketHeader,
    payload: &[u8],
) -> Result<ServerMessage, ProtocolError> {
    let msg = match header.msg_type {
        msg_type::HANDSHAKE_ACCEPTED => {
            let inner: HandshakeAccepted = deserialize(payload)?;
            ServerMessage::HandshakeAccepted(inner)
        }
        msg_type::HANDSHAKE_REJECTED => {
            let inner: HandshakeRejected = deserialize(payload)?;
            ServerMessage::HandshakeRejected(inner)
        }
        msg_type::PONG => {
            let inner: Pong = deserialize(payload)?;
            ServerMessage::Pong(inner)
        }
        msg_type::WORLD_SNAPSHOT => {
            let inner: WorldSnapshot = deserialize(payload)?;
            ServerMessage::WorldSnapshot(inner)
        }
        msg_type::ZONE_TRANSFER => {
            let inner: ZoneTransfer = deserialize(payload)?;
            ServerMessage::ZoneTransfer(inner)
        }
        msg_type::CHARACTER_LIST => {
            let inner: CharacterList = deserialize(payload)?;
            ServerMessage::CharacterList(inner)
        }
        msg_type::CHARACTER_CREATED => {
            let inner: CharacterCreated = deserialize(payload)?;
            ServerMessage::CharacterCreated(inner)
        }
        msg_type::CHARACTER_DELETED => {
            let inner: CharacterDeleted = deserialize(payload)?;
            ServerMessage::CharacterDeleted(inner)
        }
        msg_type::CHARACTER_CREATE_FAILED => {
            let inner: CharacterCreateFailed = deserialize(payload)?;
            ServerMessage::CharacterCreateFailed(inner)
        }
        unknown => return Err(ProtocolError::UnknownMessageType(unknown)),
    };
    Ok(msg)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use common::{EntityFlags, EntityId, EntityKind, EntityState, Vec2, Vec3, ZoneId};

    // -- helpers -------------------------------------------------------------

    fn sample_entity_state(seed: u64) -> EntityState {
        let mut flags = EntityFlags::empty();
        flags.set(EntityFlags::IN_COMBAT);
        EntityState::new(
            EntityId::new(seed),
            EntityKind::Player,
            Vec3::new(seed as f32, 0.0, seed as f32 * 2.0),
            seed as f32 * 0.1,
            100 + seed as u32,
            200,
            flags,
            None,
        )
    }

    // -- PacketHeader --------------------------------------------------------

    #[test]
    fn packet_header_encode_decode_roundtrip() {
        let original = PacketHeader {
            version: 1,
            msg_type: 0x0104,
            sequence: 42_000,
            payload_len: 256,
        };
        let encoded = original.encode();
        let decoded = PacketHeader::decode(&encoded);
        assert_eq!(original, decoded);
    }

    #[test]
    fn packet_header_encode_is_little_endian() {
        let h = PacketHeader {
            version: 1,
            msg_type: 0x0102, // bytes: 0x02 0x01 in LE
            sequence: 0x0000_0001,
            payload_len: 0x0000_0010,
        };
        let bytes = h.encode();
        assert_eq!(bytes[0], 1);    // version
        assert_eq!(bytes[1], 0x02); // msg_type low byte
        assert_eq!(bytes[2], 0x01); // msg_type high byte
        assert_eq!(bytes[3], 0x01); // sequence byte 0
        assert_eq!(bytes[4], 0x00);
        assert_eq!(bytes[5], 0x00);
        assert_eq!(bytes[6], 0x00);
        assert_eq!(bytes[7], 0x10); // payload_len byte 0
        assert_eq!(bytes[8], 0x00);
        assert_eq!(bytes[9], 0x00);
        assert_eq!(bytes[10], 0x00);
    }

    #[test]
    fn packet_header_size_is_eleven_bytes() {
        let h = PacketHeader {
            version: 1,
            msg_type: 0x0001,
            sequence: 0,
            payload_len: 0,
        };
        assert_eq!(h.encode().len(), HEADER_SIZE);
        assert_eq!(HEADER_SIZE, 11);
    }

    #[test]
    fn packet_header_decode_slice_ok() {
        let original = PacketHeader {
            version: 1,
            msg_type: msg_type::WORLD_SNAPSHOT,
            sequence: 99,
            payload_len: 512,
        };
        let bytes = original.encode();
        let decoded = PacketHeader::decode_slice(&bytes).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn packet_header_decode_slice_with_extra_bytes() {
        let original = PacketHeader {
            version: 1,
            msg_type: msg_type::PING,
            sequence: 1,
            payload_len: 8,
        };
        let mut bytes = original.encode().to_vec();
        bytes.extend_from_slice(&[0xAA, 0xBB, 0xCC]); // simulate payload following header
        let decoded = PacketHeader::decode_slice(&bytes).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn packet_header_decode_slice_too_short_returns_error() {
        let short = [0u8; 5]; // less than HEADER_SIZE
        let err = PacketHeader::decode_slice(&short).unwrap_err();
        assert!(matches!(err, ProtocolError::HeaderTooShort));
    }

    #[test]
    fn packet_header_decode_slice_empty_returns_error() {
        let err = PacketHeader::decode_slice(&[]).unwrap_err();
        assert!(matches!(err, ProtocolError::HeaderTooShort));
    }

    #[test]
    fn packet_header_decode_slice_exactly_header_size_ok() {
        let h = PacketHeader {
            version: 1,
            msg_type: msg_type::DISCONNECT,
            sequence: 7,
            payload_len: 0,
        };
        let bytes = h.encode();
        assert_eq!(bytes.len(), HEADER_SIZE);
        let decoded = PacketHeader::decode_slice(&bytes).unwrap();
        assert_eq!(h, decoded);
    }

    #[test]
    fn packet_header_decode_slice_one_byte_short_returns_error() {
        let bytes = [0u8; HEADER_SIZE - 1];
        let err = PacketHeader::decode_slice(&bytes).unwrap_err();
        assert!(matches!(err, ProtocolError::HeaderTooShort));
    }

    // -- ClientMessage encode/decode round-trips -----------------------------

    fn client_roundtrip(msg: ClientMessage) -> ClientMessage {
        let (header, payload) = encode_client(&msg, 1).unwrap();
        assert_eq!(header.version, PacketHeader::CURRENT_VERSION);
        assert_eq!(header.payload_len as usize, payload.len());
        decode_client(&header, &payload).unwrap()
    }

    #[test]
    fn client_handshake_roundtrip() {
        let msg = ClientMessage::Handshake(Handshake {
            token: "eyJhbGciOiJIUzI1NiJ9.test.sig".to_string(),
        });
        assert_eq!(client_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn client_handshake_msg_type_constant() {
        let msg = ClientMessage::Handshake(Handshake { token: "t".into() });
        let (header, _) = encode_client(&msg, 0).unwrap();
        assert_eq!(header.msg_type, msg_type::HANDSHAKE);
    }

    #[test]
    fn client_ping_roundtrip() {
        let msg = ClientMessage::Ping(Ping {
            client_timestamp: 1_700_000_000_000,
        });
        assert_eq!(client_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn client_ping_msg_type_constant() {
        let msg = ClientMessage::Ping(Ping { client_timestamp: 0 });
        let (header, _) = encode_client(&msg, 0).unwrap();
        assert_eq!(header.msg_type, msg_type::PING);
    }

    #[test]
    fn client_move_input_roundtrip() {
        let msg = ClientMessage::MoveInput(MoveInput {
            direction: Vec2::new(0.707, -0.707),
            speed: 7.0,
        });
        assert_eq!(client_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn client_move_input_msg_type_constant() {
        let msg = ClientMessage::MoveInput(MoveInput {
            direction: Vec2::ZERO,
            speed: 0.0,
        });
        let (header, _) = encode_client(&msg, 0).unwrap();
        assert_eq!(header.msg_type, msg_type::MOVE_INPUT);
    }

    #[test]
    fn client_disconnect_roundtrip() {
        let msg = ClientMessage::Disconnect;
        assert_eq!(client_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn client_disconnect_has_zero_payload() {
        let (header, payload) = encode_client(&ClientMessage::Disconnect, 0).unwrap();
        assert_eq!(header.msg_type, msg_type::DISCONNECT);
        assert_eq!(header.payload_len, 0);
        assert!(payload.is_empty());
    }

    #[test]
    fn client_sequence_number_preserved() {
        let msg = ClientMessage::Ping(Ping { client_timestamp: 1 });
        let seq = 12_345_678u32;
        let (header, _) = encode_client(&msg, seq).unwrap();
        assert_eq!(header.sequence, seq);
    }

    // -- ServerMessage encode/decode round-trips -----------------------------

    fn server_roundtrip(msg: ServerMessage) -> ServerMessage {
        let (header, payload) = encode_server(&msg, 1).unwrap();
        assert_eq!(header.version, PacketHeader::CURRENT_VERSION);
        assert_eq!(header.payload_len as usize, payload.len());
        decode_server(&header, &payload).unwrap()
    }

    #[test]
    fn server_handshake_accepted_roundtrip() {
        let msg = ServerMessage::HandshakeAccepted(HandshakeAccepted {
            entity_id: EntityId::new(1),
            zone_id: ZoneId::new(42),
        });
        assert_eq!(server_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn server_handshake_accepted_msg_type_constant() {
        let msg = ServerMessage::HandshakeAccepted(HandshakeAccepted {
            entity_id: EntityId::new(1),
            zone_id: ZoneId::new(1),
        });
        let (header, _) = encode_server(&msg, 0).unwrap();
        assert_eq!(header.msg_type, msg_type::HANDSHAKE_ACCEPTED);
    }

    #[test]
    fn server_handshake_rejected_invalid_token_roundtrip() {
        let msg = ServerMessage::HandshakeRejected(HandshakeRejected {
            reason: RejectReason::InvalidToken,
        });
        assert_eq!(server_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn server_handshake_rejected_expired_token_roundtrip() {
        let msg = ServerMessage::HandshakeRejected(HandshakeRejected {
            reason: RejectReason::ExpiredToken,
        });
        assert_eq!(server_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn server_handshake_rejected_server_full_roundtrip() {
        let msg = ServerMessage::HandshakeRejected(HandshakeRejected {
            reason: RejectReason::ServerFull,
        });
        assert_eq!(server_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn server_handshake_rejected_msg_type_constant() {
        let msg = ServerMessage::HandshakeRejected(HandshakeRejected {
            reason: RejectReason::ServerFull,
        });
        let (header, _) = encode_server(&msg, 0).unwrap();
        assert_eq!(header.msg_type, msg_type::HANDSHAKE_REJECTED);
    }

    #[test]
    fn server_pong_roundtrip() {
        let msg = ServerMessage::Pong(Pong {
            client_timestamp: 1_700_000_000_000,
            server_timestamp: 1_700_000_000_050,
        });
        assert_eq!(server_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn server_pong_msg_type_constant() {
        let msg = ServerMessage::Pong(Pong {
            client_timestamp: 0,
            server_timestamp: 0,
        });
        let (header, _) = encode_server(&msg, 0).unwrap();
        assert_eq!(header.msg_type, msg_type::PONG);
    }

    #[test]
    fn server_world_snapshot_empty_roundtrip() {
        let msg = ServerMessage::WorldSnapshot(WorldSnapshot {
            tick: 1,
            entities: vec![],
        });
        assert_eq!(server_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn server_world_snapshot_single_entity_roundtrip() {
        let msg = ServerMessage::WorldSnapshot(WorldSnapshot {
            tick: 42,
            entities: vec![sample_entity_state(1)],
        });
        assert_eq!(server_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn server_world_snapshot_multiple_entities_roundtrip() {
        let entities: Vec<EntityState> = (0u64..50).map(sample_entity_state).collect();
        let msg = ServerMessage::WorldSnapshot(WorldSnapshot {
            tick: 1_000_000,
            entities,
        });
        let decoded = server_roundtrip(msg.clone());
        assert_eq!(decoded, msg);
        if let ServerMessage::WorldSnapshot(snap) = decoded {
            assert_eq!(snap.entities.len(), 50);
            assert_eq!(snap.tick, 1_000_000);
        } else {
            panic!("expected WorldSnapshot");
        }
    }

    #[test]
    fn server_world_snapshot_entity_fields_preserved() {
        let mut flags = EntityFlags::empty();
        flags.set(EntityFlags::IN_COMBAT | EntityFlags::MOVING);

        let entity = EntityState::new(
            EntityId::new(999),
            EntityKind::Mob,
            Vec3::new(1234.5, 67.8, -9.0),
            std::f32::consts::PI,
            42,
            1000,
            flags,
            None,
        );
        let msg = ServerMessage::WorldSnapshot(WorldSnapshot {
            tick: 7,
            entities: vec![entity.clone()],
        });
        if let ServerMessage::WorldSnapshot(snap) = server_roundtrip(msg) {
            assert_eq!(snap.entities[0], entity);
        } else {
            panic!("expected WorldSnapshot");
        }
    }

    #[test]
    fn server_world_snapshot_msg_type_constant() {
        let msg = ServerMessage::WorldSnapshot(WorldSnapshot {
            tick: 0,
            entities: vec![],
        });
        let (header, _) = encode_server(&msg, 0).unwrap();
        assert_eq!(header.msg_type, msg_type::WORLD_SNAPSHOT);
    }

    #[test]
    fn server_zone_transfer_roundtrip() {
        let msg = ServerMessage::ZoneTransfer(ZoneTransfer {
            zone_id: ZoneId::new(7),
            position: Vec3::new(-500.0, 10.0, 300.5),
        });
        assert_eq!(server_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn server_zone_transfer_msg_type_constant() {
        let msg = ServerMessage::ZoneTransfer(ZoneTransfer {
            zone_id: ZoneId::new(1),
            position: Vec3::ZERO,
        });
        let (header, _) = encode_server(&msg, 0).unwrap();
        assert_eq!(header.msg_type, msg_type::ZONE_TRANSFER);
    }

    #[test]
    fn server_sequence_number_preserved() {
        let msg = ServerMessage::Pong(Pong {
            client_timestamp: 0,
            server_timestamp: 1,
        });
        let seq = 0xDEAD_BEEFu32;
        let (header, _) = encode_server(&msg, seq).unwrap();
        assert_eq!(header.sequence, seq);
    }

    // -- Error cases ---------------------------------------------------------

    #[test]
    fn decode_client_unknown_msg_type_returns_error() {
        let header = PacketHeader {
            version: 1,
            msg_type: 0xFFFF,
            sequence: 0,
            payload_len: 0,
        };
        let err = decode_client(&header, &[]).unwrap_err();
        assert!(matches!(err, ProtocolError::UnknownMessageType(0xFFFF)));
    }

    #[test]
    fn decode_server_unknown_msg_type_returns_error() {
        let header = PacketHeader {
            version: 1,
            msg_type: 0x00FF,
            sequence: 0,
            payload_len: 0,
        };
        let err = decode_server(&header, &[]).unwrap_err();
        assert!(matches!(err, ProtocolError::UnknownMessageType(0x00FF)));
    }

    #[test]
    fn decode_client_with_server_msg_type_returns_error() {
        let header = PacketHeader {
            version: 1,
            msg_type: msg_type::HANDSHAKE_ACCEPTED,
            sequence: 0,
            payload_len: 0,
        };
        let err = decode_client(&header, &[]).unwrap_err();
        assert!(matches!(
            err,
            ProtocolError::UnknownMessageType(msg_type::HANDSHAKE_ACCEPTED)
        ));
    }

    #[test]
    fn decode_server_with_client_msg_type_returns_error() {
        let header = PacketHeader {
            version: 1,
            msg_type: msg_type::HANDSHAKE,
            sequence: 0,
            payload_len: 0,
        };
        let err = decode_server(&header, &[]).unwrap_err();
        assert!(matches!(
            err,
            ProtocolError::UnknownMessageType(msg_type::HANDSHAKE)
        ));
    }

    #[test]
    fn decode_client_corrupted_payload_returns_serialization_error() {
        let header = PacketHeader {
            version: 1,
            msg_type: msg_type::HANDSHAKE,
            sequence: 0,
            payload_len: 5,
        };
        let err = decode_client(&header, &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF]).unwrap_err();
        assert!(matches!(err, ProtocolError::Serialization(_)));
    }

    // -- Edge cases ----------------------------------------------------------

    #[test]
    fn world_snapshot_tick_zero_is_valid() {
        let msg = ServerMessage::WorldSnapshot(WorldSnapshot {
            tick: 0,
            entities: vec![],
        });
        assert_eq!(server_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn world_snapshot_tick_max_u64_is_valid() {
        let msg = ServerMessage::WorldSnapshot(WorldSnapshot {
            tick: u64::MAX,
            entities: vec![sample_entity_state(0)],
        });
        assert_eq!(server_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn handshake_with_empty_token_roundtrip() {
        // Empty string is technically invalid from an auth perspective but the
        // protocol layer must not panic on it — the gateway validates the token.
        let msg = ClientMessage::Handshake(Handshake {
            token: String::new(),
        });
        assert_eq!(client_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn move_input_zero_direction_zero_speed_roundtrip() {
        let msg = ClientMessage::MoveInput(MoveInput {
            direction: Vec2::ZERO,
            speed: 0.0,
        });
        assert_eq!(client_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn entity_all_flags_set_roundtrip() {
        let mut flags = EntityFlags::empty();
        flags.set(
            EntityFlags::IN_COMBAT
                | EntityFlags::MOVING
                | EntityFlags::CASTING
                | EntityFlags::DEAD,
        );
        let entity = EntityState::new(
            EntityId::new(1),
            EntityKind::Player,
            Vec3::ZERO,
            0.0,
            0,
            100,
            flags,
            None,
        );
        let msg = ServerMessage::WorldSnapshot(WorldSnapshot {
            tick: 1,
            entities: vec![entity.clone()],
        });
        if let ServerMessage::WorldSnapshot(snap) = server_roundtrip(msg) {
            assert_eq!(snap.entities[0].flags, flags);
        } else {
            panic!("expected WorldSnapshot");
        }
    }

    #[test]
    fn entity_kind_variants_all_survive_roundtrip() {
        let kinds = [
            EntityKind::Player,
            EntityKind::Mob,
            EntityKind::Npc,
            EntityKind::GameObject,
        ];
        for kind in kinds {
            let entity = EntityState::new(
                EntityId::new(1),
                kind,
                Vec3::ZERO,
                0.0,
                1,
                1,
                EntityFlags::empty(),
                None,
            );
            let msg = ServerMessage::WorldSnapshot(WorldSnapshot {
                tick: 0,
                entities: vec![entity],
            });
            if let ServerMessage::WorldSnapshot(snap) = server_roundtrip(msg) {
                assert_eq!(snap.entities[0].kind, kind);
            } else {
                panic!("expected WorldSnapshot");
            }
        }
    }

    // -- Phase 2: Character selection message round-trips ----------------------

    #[test]
    fn client_character_list_request_roundtrip() {
        let msg = ClientMessage::CharacterListRequest(CharacterListRequest);
        assert_eq!(client_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn client_character_list_request_has_zero_payload() {
        let (header, payload) =
            encode_client(&ClientMessage::CharacterListRequest(CharacterListRequest), 0).unwrap();
        assert_eq!(header.msg_type, msg_type::CHARACTER_LIST_REQUEST);
        assert_eq!(header.payload_len, 0);
        assert!(payload.is_empty());
    }

    #[test]
    fn client_create_character_roundtrip() {
        let msg = ClientMessage::CreateCharacter(CreateCharacter {
            name: "Arthas".to_string(),
            race: Race::Human,
            class: Class::Warrior,
        });
        assert_eq!(client_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn client_create_character_msg_type_constant() {
        let msg = ClientMessage::CreateCharacter(CreateCharacter {
            name: "X".to_string(),
            race: Race::Orc,
            class: Class::Rogue,
        });
        let (header, _) = encode_client(&msg, 0).unwrap();
        assert_eq!(header.msg_type, msg_type::CREATE_CHARACTER);
    }

    #[test]
    fn client_delete_character_roundtrip() {
        let msg = ClientMessage::DeleteCharacter(DeleteCharacter {
            character_id: CharacterId::new("uuid-to-delete".to_string()),
        });
        assert_eq!(client_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn client_select_character_roundtrip() {
        let msg = ClientMessage::SelectCharacter(SelectCharacter {
            character_id: CharacterId::new("uuid-to-select".to_string()),
        });
        assert_eq!(client_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn server_character_list_empty_roundtrip() {
        let msg = ServerMessage::CharacterList(CharacterList {
            characters: vec![],
        });
        assert_eq!(server_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn server_character_list_with_entries_roundtrip() {
        let msg = ServerMessage::CharacterList(CharacterList {
            characters: vec![
                CharacterInfo {
                    id: CharacterId::new("c1".to_string()),
                    name: "Arthas".to_string(),
                    race: Race::Human,
                    class: Class::Warrior,
                    level: 60,
                    zone_id: ZoneId::new(1),
                },
                CharacterInfo {
                    id: CharacterId::new("c2".to_string()),
                    name: "Jaina".to_string(),
                    race: Race::Human,
                    class: Class::Mage,
                    level: 45,
                    zone_id: ZoneId::new(2),
                },
            ],
        });
        assert_eq!(server_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn server_character_created_roundtrip() {
        let msg = ServerMessage::CharacterCreated(CharacterCreated {
            character: CharacterInfo {
                id: CharacterId::new("new-uuid".to_string()),
                name: "Thrall".to_string(),
                race: Race::Orc,
                class: Class::Warrior,
                level: 1,
                zone_id: ZoneId::new(1),
            },
        });
        assert_eq!(server_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn server_character_deleted_roundtrip() {
        let msg = ServerMessage::CharacterDeleted(CharacterDeleted {
            character_id: CharacterId::new("deleted-uuid".to_string()),
        });
        assert_eq!(server_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn server_character_create_failed_roundtrip() {
        let msg = ServerMessage::CharacterCreateFailed(CharacterCreateFailed {
            reason: "Name already taken".to_string(),
        });
        assert_eq!(server_roundtrip(msg.clone()), msg);
    }

    #[test]
    fn all_reject_reasons_roundtrip() {
        let reasons = [
            RejectReason::InvalidToken,
            RejectReason::ExpiredToken,
            RejectReason::ServerFull,
            RejectReason::AlreadyConnected,
        ];
        for reason in reasons {
            let msg = ServerMessage::HandshakeRejected(HandshakeRejected { reason });
            if let ServerMessage::HandshakeRejected(rejected) = server_roundtrip(msg) {
                assert_eq!(rejected.reason, reason);
            } else {
                panic!("expected HandshakeRejected");
            }
        }
    }
}
