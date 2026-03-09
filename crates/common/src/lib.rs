//! # common
//!
//! Primitive value types shared across all Stelline crates.
//!
//! This crate contains **only** plain data types — no business logic, no async,
//! no game-engine dependencies. Every public type derives `Debug`, `Clone`,
//! `PartialEq`, and serde `Serialize`/`Deserialize` so that it can be used on
//! the wire, in the admin dashboard, and in tests without additional ceremony.

use serde::{Deserialize, Serialize};

// ─────────────────────────────────────────────────────────────────────────────
// EntityId
// ─────────────────────────────────────────────────────────────────────────────

/// A unique identifier for any entity in the game world (player, mob, NPC,
/// or game object).
///
/// Internally a `u64`. The server is the sole authority that mints new ids —
/// clients never generate them.
///
/// # Example
/// ```
/// use common::EntityId;
/// let id = EntityId::new(42);
/// assert_eq!(id.get(), 42);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntityId(u64);

impl EntityId {
    /// Wrap a raw `u64` value in an `EntityId`.
    ///
    /// The caller is responsible for supplying a value that is unique within
    /// the server. A common strategy is a monotonically-increasing atomic
    /// counter maintained by the `WorldCoordinator`.
    pub fn new(val: u64) -> Self {
        Self(val)
    }

    /// Return the underlying `u64`.
    pub fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for EntityId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "EntityId({})", self.0)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// ZoneId
// ─────────────────────────────────────────────────────────────────────────────

/// Identifies a zone (e.g. Elwynn Forest, Stormwind City).
///
/// Internally a `u32`. Zone ids are assigned statically at server startup from
/// configuration and never change during the server's lifetime.
///
/// # Example
/// ```
/// use common::ZoneId;
/// let z = ZoneId::new(1);
/// assert_eq!(z.get(), 1);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ZoneId(u32);

impl ZoneId {
    /// Wrap a raw `u32` value in a `ZoneId`.
    pub fn new(val: u32) -> Self {
        Self(val)
    }

    /// Return the underlying `u32`.
    pub fn get(self) -> u32 {
        self.0
    }
}

impl std::fmt::Display for ZoneId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ZoneId({})", self.0)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Vec2
// ─────────────────────────────────────────────────────────────────────────────

/// A 2-D vector with `f32` components.
///
/// Used for movement input direction sent by clients each tick. The server
/// treats the client-supplied direction as untrusted input and applies its
/// own speed caps before integrating position.
///
/// # Example
/// ```
/// use common::Vec2;
/// let v = Vec2::new(3.0, 4.0);
/// assert!((v.length() - 5.0).abs() < 1e-5);
/// let n = v.normalize();
/// assert!((n.length() - 1.0).abs() < 1e-5);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

impl Vec2 {
    /// The zero vector.
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };

    /// Construct a new `Vec2`.
    pub fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    /// Euclidean length (magnitude).
    pub fn length(self) -> f32 {
        (self.x * self.x + self.y * self.y).sqrt()
    }

    /// Return a unit vector in the same direction, or [`Vec2::ZERO`] if the
    /// length is (near) zero, to avoid division by zero.
    pub fn normalize(self) -> Self {
        let len = self.length();
        if len < f32::EPSILON {
            Self::ZERO
        } else {
            Self {
                x: self.x / len,
                y: self.y / len,
            }
        }
    }
}

impl Default for Vec2 {
    fn default() -> Self {
        Self::ZERO
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Vec3
// ─────────────────────────────────────────────────────────────────────────────

/// A 3-D vector with `f32` components.
///
/// Used for authoritative world positions. The server always holds the canonical
/// position; clients render what they receive in each `WorldSnapshot`.
///
/// # Example
/// ```
/// use common::Vec3;
/// let a = Vec3::new(0.0, 0.0, 0.0);
/// let b = Vec3::new(3.0, 4.0, 0.0);
/// assert!((a.distance(b) - 5.0).abs() < 1e-5);
/// let n = Vec3::new(1.0, 2.0, 2.0).normalize(); // length = 3
/// assert!((n.length() - 1.0).abs() < 1e-5);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    /// The zero vector.
    pub const ZERO: Self = Self {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };

    /// Construct a new `Vec3`.
    pub fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    /// Euclidean length (magnitude).
    pub fn length(self) -> f32 {
        (self.x * self.x + self.y * self.y + self.z * self.z).sqrt()
    }

    /// Euclidean distance between `self` and `other`.
    pub fn distance(self, other: Vec3) -> f32 {
        let dx = self.x - other.x;
        let dy = self.y - other.y;
        let dz = self.z - other.z;
        (dx * dx + dy * dy + dz * dz).sqrt()
    }

    /// XZ-plane distance (ignores the vertical Y component).
    ///
    /// Useful for AOI radius checks and ground-plane movement, where
    /// elevation differences should not affect whether entities are "nearby".
    pub fn distance_xz(self, other: Vec3) -> f32 {
        let dx = self.x - other.x;
        let dz = self.z - other.z;
        (dx * dx + dz * dz).sqrt()
    }

    /// Return a unit vector in the same direction, or [`Vec3::ZERO`] if the
    /// length is (near) zero, to avoid division by zero.
    pub fn normalize(self) -> Self {
        let len = self.length();
        if len < f32::EPSILON {
            Self::ZERO
        } else {
            Self {
                x: self.x / len,
                y: self.y / len,
                z: self.z / len,
            }
        }
    }
}

impl Default for Vec3 {
    fn default() -> Self {
        Self::ZERO
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// EntityKind
// ─────────────────────────────────────────────────────────────────────────────

/// Discriminates the broad category of an entity in the world.
///
/// The client uses this to select the correct render path (character model,
/// creature model, interactive object, etc.) without needing access to the
/// full server-side entity data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum EntityKind {
    /// A human-controlled player character.
    Player = 0,
    /// A non-player creature (hostile or neutral wildlife, bosses, etc.).
    Mob = 1,
    /// A non-player character that may offer quests, goods, or dialogue.
    Npc = 2,
    /// An interactive world object (chest, door, resource node, etc.).
    GameObject = 3,
}

// ─────────────────────────────────────────────────────────────────────────────
// EntityFlags
// ─────────────────────────────────────────────────────────────────────────────

/// A compact bitfield of boolean entity states included in every [`EntityState`].
///
/// Using a plain `u32` newtype with named bit-constant `u32` values keeps the
/// wire format simple and avoids pulling in the `bitflags` crate.
///
/// Each bit constant is a `u32` so callers can combine them with `|`:
///
/// ```
/// use common::EntityFlags;
///
/// let mut flags = EntityFlags::empty();
/// flags.set(EntityFlags::IN_COMBAT | EntityFlags::MOVING);
/// assert!(flags.contains(EntityFlags::IN_COMBAT));
/// assert!(flags.contains(EntityFlags::MOVING));
/// assert!(!flags.contains(EntityFlags::DEAD));
///
/// flags.unset(EntityFlags::MOVING);
/// assert!(!flags.contains(EntityFlags::MOVING));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntityFlags(u32);

impl EntityFlags {
    /// Entity is currently in combat.
    pub const IN_COMBAT: u32 = 1 << 0; // 1
    /// Entity is currently moving.
    pub const MOVING: u32 = 1 << 1; // 2
    /// Entity is currently casting a spell.
    pub const CASTING: u32 = 1 << 2; // 4
    /// Entity is dead.
    pub const DEAD: u32 = 1 << 3; // 8

    /// An empty flags value — no bits set.
    pub fn empty() -> Self {
        Self(0)
    }

    /// Wrap a raw `u32` bitmask directly.
    ///
    /// Useful when deserialising stored state or reconstructing flags from a
    /// known wire value.
    pub fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// Return the underlying `u32`.
    pub fn bits(self) -> u32 {
        self.0
    }

    /// Return `true` if **all** bits in `flag` are currently set.
    pub fn contains(self, flag: u32) -> bool {
        self.0 & flag == flag
    }

    /// Set one or more bits in place (bitwise OR with `flag`).
    pub fn set(&mut self, flag: u32) {
        self.0 |= flag;
    }

    /// Clear one or more bits in place (bitwise AND with NOT `flag`).
    pub fn unset(&mut self, flag: u32) {
        self.0 &= !flag;
    }
}

impl Default for EntityFlags {
    fn default() -> Self {
        Self::empty()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// EntityState
// ─────────────────────────────────────────────────────────────────────────────

/// The complete snapshot of a single entity, included in every `WorldSnapshot`
/// for entities within the receiving player's area-of-interest (AOI).
///
/// The client renders exactly what it receives — there is no client-side
/// prediction of missing state. Entities absent from a snapshot have left the
/// AOI (implicit despawn); new entities have entered it (implicit spawn). No
/// separate spawn/despawn/move messages are needed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityState {
    /// Unique identifier of this entity.
    pub entity_id: EntityId,
    /// Category of entity (player, mob, NPC, game object).
    pub kind: EntityKind,
    /// Authoritative world position in 3-D space.
    pub position: Vec3,
    /// Facing direction in radians (yaw around the vertical axis).
    pub facing: f32,
    /// Current hit points.
    pub health: u32,
    /// Maximum hit points.
    pub max_health: u32,
    /// Boolean state flags (in_combat, moving, casting, dead, …).
    pub flags: EntityFlags,
}

impl EntityState {
    /// Construct a new `EntityState` with all fields supplied explicitly.
    pub fn new(
        entity_id: EntityId,
        kind: EntityKind,
        position: Vec3,
        facing: f32,
        health: u32,
        max_health: u32,
        flags: EntityFlags,
    ) -> Self {
        Self {
            entity_id,
            kind,
            position,
            facing,
            health,
            max_health,
            flags,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// ZoneSnapshot — per-zone telemetry for the admin dashboard
// ─────────────────────────────────────────────────────────────────────────────

/// Real-time telemetry for a single zone, pushed to the admin dashboard.
///
/// Zone threads emit these as telemetry events each tick; the
/// `WorldCoordinator` aggregates them into an [`AdminSnapshot`] and writes it
/// to the `watch` channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoneSnapshot {
    /// The zone this snapshot describes.
    pub zone_id: ZoneId,
    /// Human-readable zone name (e.g. `"Elwynn Forest"`).
    pub name: String,
    /// Number of player characters currently in this zone.
    pub player_count: u32,
    /// Total entity count (players + mobs + NPCs + game objects).
    pub entity_count: u32,
    /// Measured tick rate in ticks per second over the last sample window.
    pub tick_rate: f32,
    /// Average tick duration in milliseconds over the last sample window.
    pub tick_duration_ms_avg: f32,
    /// Maximum (worst-case) tick duration in milliseconds over the last sample window.
    /// Ticks that exceed the 50 ms budget are highlighted red in the dashboard.
    pub tick_duration_ms_max: f32,
}

// ─────────────────────────────────────────────────────────────────────────────
// AdminSnapshot — server-wide telemetry snapshot for the admin dashboard
// ─────────────────────────────────────────────────────────────────────────────

/// The complete server-wide snapshot delivered to the admin dashboard via the
/// `watch` channel.
///
/// The `WorldCoordinator` is the sole writer; admin axum handlers clone the
/// `watch::Receiver` and read the latest value without any locking.
///
/// Serialises to JSON for transmission over the admin WebSocket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdminSnapshot {
    /// Seconds the game-server process has been running.
    pub uptime_secs: u64,
    /// Total number of players connected across all zones.
    pub connected_players: u32,
    /// Per-zone telemetry, one entry per loaded zone (stable startup order).
    pub zones: Vec<ZoneSnapshot>,
}

impl Default for AdminSnapshot {
    fn default() -> Self {
        Self {
            uptime_secs: 0,
            connected_players: 0,
            zones: Vec::new(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── EntityId ──────────────────────────────────────────────────────────────

    #[test]
    fn entity_id_new_and_get() {
        let id = EntityId::new(99);
        assert_eq!(id.get(), 99);
    }

    #[test]
    fn entity_id_equality() {
        assert_eq!(EntityId::new(1), EntityId::new(1));
        assert_ne!(EntityId::new(1), EntityId::new(2));
    }

    #[test]
    fn entity_id_copy_semantics() {
        let a = EntityId::new(7);
        let b = a; // copy, not move
        assert_eq!(a, b);
    }

    #[test]
    fn entity_id_usable_as_hashmap_key() {
        use std::collections::HashMap;
        let mut map: HashMap<EntityId, &str> = HashMap::new();
        map.insert(EntityId::new(1), "warrior");
        map.insert(EntityId::new(2), "mage");
        assert_eq!(map[&EntityId::new(1)], "warrior");
        assert_eq!(map[&EntityId::new(2)], "mage");
    }

    #[test]
    fn entity_id_display() {
        let s = format!("{}", EntityId::new(5));
        assert!(s.contains("5"), "Display should include the numeric value");
    }

    // ── ZoneId ────────────────────────────────────────────────────────────────

    #[test]
    fn zone_id_new_and_get() {
        let z = ZoneId::new(42);
        assert_eq!(z.get(), 42);
    }

    #[test]
    fn zone_id_equality() {
        assert_eq!(ZoneId::new(5), ZoneId::new(5));
        assert_ne!(ZoneId::new(5), ZoneId::new(6));
    }

    #[test]
    fn zone_id_copy_semantics() {
        let a = ZoneId::new(3);
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn zone_id_usable_as_hashmap_key() {
        use std::collections::HashMap;
        let mut map: HashMap<ZoneId, &str> = HashMap::new();
        map.insert(ZoneId::new(1), "Elwynn Forest");
        assert_eq!(map[&ZoneId::new(1)], "Elwynn Forest");
    }

    // ── Vec2 ──────────────────────────────────────────────────────────────────

    #[test]
    fn vec2_length_3_4_triangle() {
        // 3-4-5 right triangle
        let v = Vec2::new(3.0, 4.0);
        assert!(
            (v.length() - 5.0).abs() < 1e-5,
            "expected 5.0, got {}",
            v.length()
        );
    }

    #[test]
    fn vec2_length_zero() {
        assert_eq!(Vec2::ZERO.length(), 0.0);
    }

    #[test]
    fn vec2_normalize_gives_unit_vector() {
        let n = Vec2::new(3.0, 4.0).normalize();
        assert!(
            (n.length() - 1.0).abs() < 1e-5,
            "normalized length should be 1.0, got {}",
            n.length()
        );
        assert!((n.x - 0.6).abs() < 1e-5, "x component wrong");
        assert!((n.y - 0.8).abs() < 1e-5, "y component wrong");
    }

    #[test]
    fn vec2_normalize_zero_returns_zero() {
        assert_eq!(Vec2::ZERO.normalize(), Vec2::ZERO);
    }

    #[test]
    fn vec2_copy_semantics() {
        let a = Vec2::new(1.0, 2.0);
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn vec2_default_is_zero() {
        assert_eq!(Vec2::default(), Vec2::ZERO);
    }

    // ── Vec3 ──────────────────────────────────────────────────────────────────

    #[test]
    fn vec3_length_flat_3_4() {
        // (3, 4, 0) has length 5
        let v = Vec3::new(3.0, 4.0, 0.0);
        assert!((v.length() - 5.0).abs() < 1e-5);
    }

    #[test]
    fn vec3_length_3d() {
        // (1, 2, 2): 1+4+4 = 9, sqrt = 3
        let v = Vec3::new(1.0, 2.0, 2.0);
        assert!((v.length() - 3.0).abs() < 1e-5);
    }

    #[test]
    fn vec3_length_zero() {
        assert_eq!(Vec3::ZERO.length(), 0.0);
    }

    #[test]
    fn vec3_distance_commutative() {
        let a = Vec3::new(1.0, 0.0, 0.0);
        let b = Vec3::new(4.0, 4.0, 0.0); // dx=3, dy=4 → 5
        let d_ab = a.distance(b);
        let d_ba = b.distance(a);
        assert!((d_ab - 5.0).abs() < 1e-5);
        assert!((d_ab - d_ba).abs() < 1e-5);
    }

    #[test]
    fn vec3_distance_to_self_is_zero() {
        let a = Vec3::new(7.0, 3.0, 1.0);
        assert!(a.distance(a) < 1e-5);
    }

    #[test]
    fn vec3_distance_xz_ignores_y() {
        let a = Vec3::new(0.0, 100.0, 0.0);
        let b = Vec3::new(3.0, 999.0, 4.0);
        // XZ: sqrt(9+16) = 5, regardless of Y gap
        assert!((a.distance_xz(b) - 5.0).abs() < 1e-5);
    }

    #[test]
    fn vec3_normalize_gives_unit_vector() {
        // (1, 2, 2) has length 3
        let n = Vec3::new(1.0, 2.0, 2.0).normalize();
        assert!((n.length() - 1.0).abs() < 1e-5);
        assert!((n.x - 1.0 / 3.0).abs() < 1e-5);
        assert!((n.y - 2.0 / 3.0).abs() < 1e-5);
        assert!((n.z - 2.0 / 3.0).abs() < 1e-5);
    }

    #[test]
    fn vec3_normalize_zero_returns_zero() {
        assert_eq!(Vec3::ZERO.normalize(), Vec3::ZERO);
    }

    #[test]
    fn vec3_copy_semantics() {
        let a = Vec3::new(1.0, 2.0, 3.0);
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn vec3_default_is_zero() {
        assert_eq!(Vec3::default(), Vec3::ZERO);
    }

    // ── EntityFlags ───────────────────────────────────────────────────────────

    #[test]
    fn entity_flags_empty_has_no_bits_set() {
        let f = EntityFlags::empty();
        assert!(!f.contains(EntityFlags::IN_COMBAT));
        assert!(!f.contains(EntityFlags::MOVING));
        assert!(!f.contains(EntityFlags::CASTING));
        assert!(!f.contains(EntityFlags::DEAD));
        assert_eq!(f.bits(), 0);
    }

    #[test]
    fn entity_flags_set_single_bit() {
        let mut f = EntityFlags::empty();
        f.set(EntityFlags::IN_COMBAT);
        assert!(f.contains(EntityFlags::IN_COMBAT));
        assert!(!f.contains(EntityFlags::MOVING));
    }

    #[test]
    fn entity_flags_set_multiple_bits_at_once() {
        let mut f = EntityFlags::empty();
        f.set(EntityFlags::IN_COMBAT | EntityFlags::CASTING);
        assert!(f.contains(EntityFlags::IN_COMBAT));
        assert!(f.contains(EntityFlags::CASTING));
        assert!(!f.contains(EntityFlags::MOVING));
        assert!(!f.contains(EntityFlags::DEAD));
    }

    #[test]
    fn entity_flags_unset_clears_bit() {
        let mut f = EntityFlags::empty();
        f.set(EntityFlags::MOVING);
        f.set(EntityFlags::DEAD);
        f.unset(EntityFlags::MOVING);
        assert!(!f.contains(EntityFlags::MOVING));
        assert!(f.contains(EntityFlags::DEAD));
    }

    #[test]
    fn entity_flags_unset_non_set_bit_is_noop() {
        let mut f = EntityFlags::empty();
        f.set(EntityFlags::IN_COMBAT);
        f.unset(EntityFlags::DEAD); // DEAD was never set
        assert!(f.contains(EntityFlags::IN_COMBAT));
        assert!(!f.contains(EntityFlags::DEAD));
    }

    #[test]
    fn entity_flags_from_bits_round_trip() {
        let raw: u32 = EntityFlags::IN_COMBAT | EntityFlags::DEAD;
        let f = EntityFlags::from_bits(raw);
        assert_eq!(f.bits(), raw);
        assert!(f.contains(EntityFlags::IN_COMBAT));
        assert!(f.contains(EntityFlags::DEAD));
        assert!(!f.contains(EntityFlags::MOVING));
    }

    #[test]
    fn entity_flags_bit_values_are_distinct_powers_of_two() {
        assert_eq!(EntityFlags::IN_COMBAT, 1u32);
        assert_eq!(EntityFlags::MOVING, 2u32);
        assert_eq!(EntityFlags::CASTING, 4u32);
        assert_eq!(EntityFlags::DEAD, 8u32);
    }

    #[test]
    fn entity_flags_default_is_empty() {
        assert_eq!(EntityFlags::default(), EntityFlags::empty());
    }

    // ── EntityState ───────────────────────────────────────────────────────────

    #[test]
    fn entity_state_player_construction() {
        let mut flags = EntityFlags::empty();
        flags.set(EntityFlags::IN_COMBAT);

        let state = EntityState::new(
            EntityId::new(1),
            EntityKind::Player,
            Vec3::new(100.0, 0.0, 200.0),
            1.57,
            850,
            1000,
            flags,
        );

        assert_eq!(state.entity_id, EntityId::new(1));
        assert_eq!(state.kind, EntityKind::Player);
        assert_eq!(state.position, Vec3::new(100.0, 0.0, 200.0));
        assert!((state.facing - 1.57).abs() < 1e-5);
        assert_eq!(state.health, 850);
        assert_eq!(state.max_health, 1000);
        assert!(state.flags.contains(EntityFlags::IN_COMBAT));
        assert!(!state.flags.contains(EntityFlags::DEAD));
    }

    #[test]
    fn entity_state_mob_construction() {
        let state = EntityState::new(
            EntityId::new(999),
            EntityKind::Mob,
            Vec3::new(50.0, 1.0, 75.0),
            0.0,
            200,
            200,
            EntityFlags::empty(),
        );
        assert_eq!(state.kind, EntityKind::Mob);
        assert_eq!(state.health, 200);
        assert_eq!(state.max_health, 200);
    }

    #[test]
    fn entity_state_npc_and_game_object_kinds() {
        let npc = EntityState::new(
            EntityId::new(10),
            EntityKind::Npc,
            Vec3::ZERO,
            0.0,
            1,
            1,
            EntityFlags::empty(),
        );
        let go = EntityState::new(
            EntityId::new(11),
            EntityKind::GameObject,
            Vec3::ZERO,
            0.0,
            0,
            0,
            EntityFlags::empty(),
        );
        assert_eq!(npc.kind, EntityKind::Npc);
        assert_eq!(go.kind, EntityKind::GameObject);
    }

    #[test]
    fn entity_state_clone_equality() {
        let state = EntityState::new(
            EntityId::new(2),
            EntityKind::Npc,
            Vec3::ZERO,
            0.0,
            50,
            50,
            EntityFlags::empty(),
        );
        assert_eq!(state, state.clone());
    }

    // ── AdminSnapshot / ZoneSnapshot — construction and serde round-trips ─────

    fn sample_zone_snapshot() -> ZoneSnapshot {
        ZoneSnapshot {
            zone_id: ZoneId::new(1),
            name: "Elwynn Forest".to_string(),
            player_count: 12,
            entity_count: 340,
            tick_rate: 19.8,
            tick_duration_ms_avg: 3.2,
            tick_duration_ms_max: 7.1,
        }
    }

    fn sample_admin_snapshot() -> AdminSnapshot {
        AdminSnapshot {
            uptime_secs: 3600,
            connected_players: 12,
            zones: vec![
                sample_zone_snapshot(),
                ZoneSnapshot {
                    zone_id: ZoneId::new(2),
                    name: "Stormwind City".to_string(),
                    player_count: 0,
                    entity_count: 20,
                    tick_rate: 20.0,
                    tick_duration_ms_avg: 1.1,
                    tick_duration_ms_max: 2.0,
                },
            ],
        }
    }

    #[test]
    fn zone_snapshot_fields() {
        let zs = sample_zone_snapshot();
        assert_eq!(zs.zone_id, ZoneId::new(1));
        assert_eq!(zs.name, "Elwynn Forest");
        assert_eq!(zs.player_count, 12);
        assert_eq!(zs.entity_count, 340);
        assert!((zs.tick_rate - 19.8).abs() < 1e-4);
        assert!((zs.tick_duration_ms_avg - 3.2).abs() < 1e-4);
        assert!((zs.tick_duration_ms_max - 7.1).abs() < 1e-4);
    }

    #[test]
    fn admin_snapshot_fields() {
        let snap = sample_admin_snapshot();
        assert_eq!(snap.uptime_secs, 3600);
        assert_eq!(snap.connected_players, 12);
        assert_eq!(snap.zones.len(), 2);
        assert_eq!(snap.zones[0].name, "Elwynn Forest");
        assert_eq!(snap.zones[1].name, "Stormwind City");
    }

    #[test]
    fn admin_snapshot_default_is_empty() {
        let snap = AdminSnapshot::default();
        assert_eq!(snap.uptime_secs, 0);
        assert_eq!(snap.connected_players, 0);
        assert!(snap.zones.is_empty());
    }

    #[test]
    fn zone_snapshot_serde_round_trip() {
        let original = sample_zone_snapshot();
        let json = serde_json::to_string(&original).expect("serialization failed");
        let restored: ZoneSnapshot =
            serde_json::from_str(&json).expect("deserialization failed");
        assert_eq!(original, restored);
    }

    #[test]
    fn admin_snapshot_serde_round_trip() {
        let original = sample_admin_snapshot();
        let json = serde_json::to_string(&original).expect("serialization failed");
        let restored: AdminSnapshot =
            serde_json::from_str(&json).expect("deserialization failed");
        assert_eq!(original, restored);
    }

    #[test]
    fn admin_snapshot_json_contains_zone_names() {
        let snap = sample_admin_snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        assert!(json.contains("uptime_secs"));
        assert!(json.contains("connected_players"));
        assert!(json.contains("Elwynn Forest"));
        assert!(json.contains("Stormwind City"));
    }

    #[test]
    fn entity_state_serde_round_trip() {
        let mut flags = EntityFlags::empty();
        flags.set(EntityFlags::MOVING | EntityFlags::IN_COMBAT);

        let original = EntityState::new(
            EntityId::new(77),
            EntityKind::GameObject,
            Vec3::new(1.5, 2.5, 3.5),
            3.14,
            0,
            0,
            flags,
        );
        let json = serde_json::to_string(&original).expect("serialization failed");
        let restored: EntityState =
            serde_json::from_str(&json).expect("deserialization failed");
        assert_eq!(original, restored);
    }
}
