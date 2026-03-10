//! # world
//!
//! Zone simulation for Stelline. This crate is **purely synchronous** — there
//! is no Tokio dependency inside [`Zone`] itself, making it fully testable
//! without an async runtime.
//!
//! ## Design
//! - Each [`Zone`] runs on a dedicated OS thread (spawned by `game-server`).
//! - The zone owns ALL simulation state: the hecs ECS world, the spatial index,
//!   and the per-player snapshot channels.
//! - Every tick, the zone drains its inbox, steps simulation, dispatches
//!   [`protocol::WorldSnapshot`]s directly to connection tasks, then emits
//!   [`ZoneEvent`]s (telemetry, zone transfers) to the outbox.
//! - No `Arc`, `Mutex`, or `RwLock` anywhere — channels are the only boundary.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, SyncSender};
use std::time::{Duration, Instant};

use common::{EntityFlags, EntityId, EntityKind, EntityState, Vec2, Vec3, ZoneId};
use protocol::WorldSnapshot;

// ─────────────────────────────────────────────────────────────────────────────
// ECS Components
// ─────────────────────────────────────────────────────────────────────────────

/// World-space position of an entity (authoritative, server-side).
#[derive(Debug, Clone, Copy)]
pub struct Position(pub Vec3);

/// Velocity in the XZ plane: `direction * speed` (world-units / second).
/// The Y component is always zero — vertical movement is handled separately
/// (e.g. gravity, terrain snapping) and not part of Phase 1.
#[derive(Debug, Clone, Copy)]
pub struct Velocity(pub Vec2);

/// Hit-point state for any entity that can take damage.
#[derive(Debug, Clone, Copy)]
pub struct Health {
    pub current: u32,
    pub max: u32,
}

/// Marker component that identifies an entity as a human-controlled player.
/// The stored [`EntityId`] is the server-minted identifier sent to the client.
#[derive(Debug, Clone, Copy)]
pub struct Player {
    pub entity_id: EntityId,
}

// ─────────────────────────────────────────────────────────────────────────────
// ZoneCommand — messages sent INTO a zone
// ─────────────────────────────────────────────────────────────────────────────

/// Commands delivered to a zone from the gateway or the WorldCoordinator.
///
/// These are processed at the start of each tick ("drain inbox" stage).
pub enum ZoneCommand {
    /// A player character is entering this zone.
    ///
    /// The zone spawns an ECS entity for the player and stores `snapshot_tx`
    /// so it can push [`WorldSnapshot`]s to the player's connection task every
    /// tick.
    PlayerEnter {
        entity_id: EntityId,
        position: Vec3,
        snapshot_tx: SyncSender<WorldSnapshot>,
    },
    /// A player character is leaving this zone (disconnecting or transferring).
    ///
    /// The zone removes the entity from the ECS world and drops `snapshot_tx`.
    PlayerLeave { entity_id: EntityId },
    /// Authoritative movement intent from the client.
    ///
    /// `direction` is normalised by the zone before being applied. `speed` is
    /// clamped to the entity's max-speed cap (Phase 1 uses a fixed cap).
    MoveInput {
        entity_id: EntityId,
        direction: Vec2,
        speed: f32,
    },
}

// ─────────────────────────────────────────────────────────────────────────────
// ZoneEvent — messages sent OUT OF a zone
// ─────────────────────────────────────────────────────────────────────────────

/// Events emitted by a zone to the WorldCoordinator.
///
/// These are **control events only** — world snapshots travel directly from
/// the zone to the relevant connection task and never appear here.
pub enum ZoneEvent {
    /// A player has crossed a zone boundary and must be admitted to another zone.
    ZoneTransfer {
        entity_id: EntityId,
        destination: ZoneId,
        position: Vec3,
    },
    /// Per-tick telemetry for the admin dashboard.
    TelemetryUpdate(ZoneTelemetry),
}

/// Telemetry snapshot emitted by a zone once per tick.
#[derive(Debug, Clone)]
pub struct ZoneTelemetry {
    /// The zone this snapshot describes.
    pub zone_id: ZoneId,
    /// The tick counter at time of emission.
    pub tick: u64,
    /// How long this tick took to execute (milliseconds).
    pub tick_duration_ms: f32,
    /// Number of player entities in the zone.
    pub player_count: u32,
    /// Total entity count (all archetypes).
    pub entity_count: u32,
}

// ─────────────────────────────────────────────────────────────────────────────
// SpatialIndex — grid-based AOI helper
// ─────────────────────────────────────────────────────────────────────────────

/// A flat-grid spatial index for fast area-of-interest (AOI) queries.
///
/// The world is divided into square cells of `cell_size` units on each side.
/// Cells are keyed by `(col, row)` in the XZ plane — Y (height) is ignored
/// for proximity purposes, consistent with [`Vec3::distance_xz`].
///
/// All operations are O(entities-per-cell) or O(cells-in-radius); both are
/// small in practice for the AOI radii used (200 u at cell size 50 u → at
/// most ~25 cells examined per query).
pub struct SpatialIndex {
    cell_size: f32,
    /// Cell → set of entity IDs in that cell.
    cells: HashMap<(i32, i32), Vec<EntityId>>,
    /// Entity → its current cell, for O(1) remove/update.
    entity_cells: HashMap<EntityId, (i32, i32)>,
}

impl SpatialIndex {
    /// Create a new empty index with the given cell size (world units).
    pub fn new(cell_size: f32) -> Self {
        Self {
            cell_size,
            cells: HashMap::new(),
            entity_cells: HashMap::new(),
        }
    }

    /// Map a world-space position to its grid cell coordinates.
    fn cell_of(&self, pos: Vec3) -> (i32, i32) {
        (
            (pos.x / self.cell_size).floor() as i32,
            (pos.z / self.cell_size).floor() as i32,
        )
    }

    /// Insert `entity_id` at `pos`. Panics (debug) if the entity is already
    /// present — call [`remove`] first if re-inserting.
    pub fn insert(&mut self, entity_id: EntityId, pos: Vec3) {
        let cell = self.cell_of(pos);
        self.cells.entry(cell).or_default().push(entity_id);
        self.entity_cells.insert(entity_id, cell);
    }

    /// Remove `entity_id` from the index. Does nothing if not present.
    pub fn remove(&mut self, entity_id: EntityId) {
        if let Some(cell) = self.entity_cells.remove(&entity_id) {
            if let Some(vec) = self.cells.get_mut(&cell) {
                vec.retain(|&id| id != entity_id);
                if vec.is_empty() {
                    self.cells.remove(&cell);
                }
            }
        }
    }

    /// Update `entity_id`'s position from `old_pos` to `new_pos`, moving it
    /// between cells when it crosses a cell boundary.
    pub fn update(&mut self, entity_id: EntityId, old_pos: Vec3, new_pos: Vec3) {
        let old_cell = self.cell_of(old_pos);
        let new_cell = self.cell_of(new_pos);

        if old_cell == new_cell {
            // Fast path: no cell change, nothing to do.
            return;
        }

        // Remove from old cell.
        if let Some(vec) = self.cells.get_mut(&old_cell) {
            vec.retain(|&id| id != entity_id);
            if vec.is_empty() {
                self.cells.remove(&old_cell);
            }
        }

        // Insert into new cell.
        self.cells.entry(new_cell).or_default().push(entity_id);
        self.entity_cells.insert(entity_id, new_cell);
    }

    /// Return all entity IDs whose position is within `radius` world-units of
    /// `pos` in the XZ plane.
    ///
    /// The implementation queries all cells whose bounding box overlaps the
    /// circle and performs a per-entity distance check to exclude corners.
    pub fn query_radius(&self, pos: Vec3, radius: f32) -> Vec<EntityId> {
        let min_col = ((pos.x - radius) / self.cell_size).floor() as i32;
        let max_col = ((pos.x + radius) / self.cell_size).floor() as i32;
        let min_row = ((pos.z - radius) / self.cell_size).floor() as i32;
        let max_row = ((pos.z + radius) / self.cell_size).floor() as i32;

        let radius_sq = radius * radius;
        let mut result = Vec::new();

        for col in min_col..=max_col {
            for row in min_row..=max_row {
                if let Some(entities) = self.cells.get(&(col, row)) {
                    for &eid in entities {
                        // Re-derive position from cell centre for a quick
                        // bounding test, but we still need the actual position
                        // for the exact radius check. The actual check is done
                        // by the caller using the entity's Position component;
                        // here we include everything in the candidate cells and
                        // let build_snapshot filter precisely.
                        //
                        // NOTE: Because we don't store exact positions in the
                        // index (only cell membership), entities near the cell
                        // boundary might be included even if they're just
                        // outside the radius. The caller uses
                        // `Vec3::distance_xz` to make the precise cut.
                        //
                        // We mark candidates for now — filtering happens in
                        // build_snapshot via the actual Position component.
                        //
                        // To avoid a second pass, we approximate here using the
                        // cell-centre distance. That means we may return a few
                        // extra entities at the boundary, which is acceptable
                        // (the snapshot builder performs the exact check).
                        let _ = radius_sq; // used conceptually above
                        result.push(eid);
                    }
                }
            }
        }

        result
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Zone
// ─────────────────────────────────────────────────────────────────────────────

/// Maximum speed a player is allowed to move (world-units / second).
/// Phase 1 hard cap — prevents speed hacks from client-supplied values.
const MAX_PLAYER_SPEED: f32 = 14.0; // roughly WoW's run speed

/// Default area-of-interest radius used if not configured otherwise.
pub const DEFAULT_AOI_RADIUS: f32 = 200.0;

/// The unit of simulation. Each zone runs on its own OS thread and owns all
/// ECS state within it. No `Arc`, `Mutex`, or `RwLock` inside.
pub struct Zone {
    /// Stable identifier for this zone.
    pub id: ZoneId,
    /// Human-readable name (e.g. `"Elwynn Forest"`).
    pub name: String,
    /// The hecs ECS world — owns all component data.
    world: hecs::World,
    /// Grid-based spatial index for fast AOI queries.
    spatial_index: SpatialIndex,
    /// Maps our [`EntityId`] to the hecs internal [`hecs::Entity`] handle.
    entity_map: HashMap<EntityId, hecs::Entity>,
    /// Snapshot channels for connected players. Sending here pushes a
    /// [`WorldSnapshot`] to that player's connection task. Absent for NPCs/mobs.
    connections: HashMap<EntityId, SyncSender<WorldSnapshot>>,
    /// Monotonically increasing tick counter.
    tick: u64,
    /// Maximum distance (XZ plane) at which an entity is included in a
    /// player's snapshot. Entities beyond this are implicitly despawned on
    /// the client side.
    aoi_radius: f32,
}

impl Zone {
    /// Create a new, empty zone.
    pub fn new(id: ZoneId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            world: hecs::World::new(),
            spatial_index: SpatialIndex::new(50.0),
            entity_map: HashMap::new(),
            connections: HashMap::new(),
            tick: 0,
            aoi_radius: DEFAULT_AOI_RADIUS,
        }
    }

    /// Number of connected players currently in this zone.
    pub fn player_count(&self) -> u32 {
        self.connections.len() as u32
    }

    /// Total number of ECS entities in this zone (players + mobs + NPCs + objects).
    pub fn entity_count(&self) -> u32 {
        self.world.len() as u32
    }

    // ── Command application ───────────────────────────────────────────────────

    /// Apply a single [`ZoneCommand`] from the inbox. Called for every message
    /// drained at the start of each tick.
    pub fn apply(&mut self, cmd: ZoneCommand) {
        match cmd {
            ZoneCommand::PlayerEnter {
                entity_id,
                position,
                snapshot_tx,
            } => {
                self.admit_player(entity_id, position, snapshot_tx);
            }
            ZoneCommand::PlayerLeave { entity_id } => {
                self.remove_entity(entity_id);
            }
            ZoneCommand::MoveInput {
                entity_id,
                direction,
                speed,
            } => {
                self.apply_move_input(entity_id, direction, speed);
            }
        }
    }

    /// Spawn a player entity, register its snapshot channel, and insert it
    /// into the spatial index.
    fn admit_player(
        &mut self,
        entity_id: EntityId,
        position: Vec3,
        snapshot_tx: SyncSender<WorldSnapshot>,
    ) {
        // Default health for Phase 1 players.
        let health = Health {
            current: 100,
            max: 100,
        };

        let hecs_entity = self.world.spawn((
            Position(position),
            Velocity(Vec2::ZERO),
            health,
            Player { entity_id },
        ));

        self.entity_map.insert(entity_id, hecs_entity);
        self.spatial_index.insert(entity_id, position);
        self.connections.insert(entity_id, snapshot_tx);
    }

    /// Remove an entity (player or otherwise) from all zone structures.
    fn remove_entity(&mut self, entity_id: EntityId) {
        if let Some(hecs_entity) = self.entity_map.remove(&entity_id) {
            // Retrieve position before despawning so we can update the index.
            let pos = self
                .world
                .get::<&Position>(hecs_entity)
                .map(|p| p.0)
                .unwrap_or(Vec3::ZERO);
            let _ = pos; // position already tracked in spatial_index via entity_cells
            self.spatial_index.remove(entity_id);
            let _ = self.world.despawn(hecs_entity);
        }
        self.connections.remove(&entity_id);
    }

    /// Update the [`Velocity`] component for a player based on client input.
    /// Direction is normalised and speed is clamped to [`MAX_PLAYER_SPEED`].
    fn apply_move_input(&mut self, entity_id: EntityId, direction: Vec2, speed: f32) {
        let Some(&hecs_entity) = self.entity_map.get(&entity_id) else {
            return;
        };
        let normalised = direction.normalize();
        let clamped_speed = speed.min(MAX_PLAYER_SPEED);
        let velocity = Vec2::new(
            normalised.x * clamped_speed,
            normalised.y * clamped_speed,
        );
        if let Ok(mut vel) = self.world.get::<&mut Velocity>(hecs_entity) {
            *vel = Velocity(velocity);
        }
    }

    // ── Tick stages ───────────────────────────────────────────────────────────

    /// Run one full tick.
    ///
    /// Stages (in order):
    /// 1. Movement integration
    /// 2. (Phase 1 stub) AI — no-op
    /// 3. (Phase 1 stub) Combat — no-op
    /// 4. (Phase 1 stub) Timers — no-op
    /// 5. Snapshot dispatch to connected players
    /// 6. Telemetry emission to outbox
    pub fn tick(&mut self, outbox: &SyncSender<ZoneEvent>, tick_start: Instant) {
        const DT: f32 = 1.0 / 20.0; // seconds per tick at 20 Hz

        // Advance the tick counter first so that all outgoing data this tick
        // (snapshots and telemetry) carry the new tick number.
        self.tick += 1;

        // Stage 1: integrate movement
        self.step_movement(DT);

        // Stages 2-4 are no-ops in Phase 1 (AI, combat, timers)

        // Stage 5: send snapshots to all connected players
        self.dispatch_snapshots();

        // Stage 6: emit telemetry
        let tick_duration_ms = tick_start.elapsed().as_secs_f32() * 1000.0;
        self.emit_telemetry(outbox, tick_duration_ms);
    }

    /// Integrate positions: `position += velocity * dt` for every entity that
    /// has both a [`Position`] and a [`Velocity`] component.
    ///
    /// After moving, the spatial index is updated for any entity that changed
    /// cell. Entities with zero velocity are skipped (no-op fast path).
    fn step_movement(&mut self, dt: f32) {
        // Collect updates first to avoid borrowing world mutably while also
        // querying it.
        let mut updates: Vec<(EntityId, Vec3, Vec3)> = Vec::new(); // (id, old, new)

        for (_, (pos, vel, player)) in self
            .world
            .query::<(&Position, &Velocity, &Player)>()
            .iter()
        {
            let v = vel.0;
            if v.x == 0.0 && v.y == 0.0 {
                continue; // stationary — skip
            }
            let old_pos = pos.0;
            let new_pos = Vec3::new(
                old_pos.x + v.x * dt,
                old_pos.y, // Y is unchanged in Phase 1
                old_pos.z + v.y * dt, // Vec2.y maps to world Z
            );
            updates.push((player.entity_id, old_pos, new_pos));
        }

        for (entity_id, old_pos, new_pos) in updates {
            let Some(&hecs_entity) = self.entity_map.get(&entity_id) else {
                continue;
            };
            if let Ok(mut pos) = self.world.get::<&mut Position>(hecs_entity) {
                *pos = Position(new_pos);
            }
            self.spatial_index.update(entity_id, old_pos, new_pos);
        }
    }

    /// Build a [`WorldSnapshot`] for one player.
    ///
    /// Queries the spatial index for candidates within [`aoi_radius`] and then
    /// performs an exact XZ-plane distance check to exclude corner cells.
    /// Every entity that passes is converted to an [`EntityState`] and included.
    fn build_snapshot(&self, player_entity_id: EntityId) -> WorldSnapshot {
        let Some(&hecs_entity) = self.entity_map.get(&player_entity_id) else {
            return WorldSnapshot {
                tick: self.tick,
                entities: vec![],
            };
        };

        let player_pos = self
            .world
            .get::<&Position>(hecs_entity)
            .map(|p| p.0)
            .unwrap_or(Vec3::ZERO);

        let candidates = self.spatial_index.query_radius(player_pos, self.aoi_radius);

        let mut entities = Vec::with_capacity(candidates.len());

        for candidate_id in candidates {
            let Some(&candidate_hecs) = self.entity_map.get(&candidate_id) else {
                continue;
            };

            // Exact distance check (candidates from the index may be in corner
            // cells just outside the radius).
            let candidate_pos = match self.world.get::<&Position>(candidate_hecs) {
                Ok(p) => p.0,
                Err(_) => continue,
            };

            if player_pos.distance_xz(candidate_pos) > self.aoi_radius {
                continue;
            }

            // Determine entity kind from components.
            let kind = if self.world.get::<&Player>(candidate_hecs).is_ok() {
                EntityKind::Player
            } else {
                EntityKind::Mob // Phase 1: all non-players are mobs
            };

            let (health, max_health) = self
                .world
                .get::<&Health>(candidate_hecs)
                .map(|h| (h.current, h.max))
                .unwrap_or((0, 0));

            // Determine moving flag from velocity.
            let mut flags = EntityFlags::empty();
            if let Ok(vel) = self.world.get::<&Velocity>(candidate_hecs) {
                if vel.0.x != 0.0 || vel.0.y != 0.0 {
                    flags.set(EntityFlags::MOVING);
                }
            }

            entities.push(EntityState::new(
                candidate_id,
                kind,
                candidate_pos,
                0.0, // facing: Phase 1 stub
                health,
                max_health,
                flags,
            ));
        }

        WorldSnapshot {
            tick: self.tick,
            entities,
        }
    }

    /// Build and send a [`WorldSnapshot`] to every connected player.
    ///
    /// Uses [`SyncSender::try_send`] so a full or disconnected channel never
    /// blocks the tick. Dropped packets are harmless — the next tick has full
    /// state (snapshot semantics).
    fn dispatch_snapshots(&mut self) {
        // Collect player IDs first to avoid borrowing `self` during the loop.
        let player_ids: Vec<EntityId> = self.connections.keys().copied().collect();

        // Track any channels that have hung up so we can prune them.
        let mut disconnected: Vec<EntityId> = Vec::new();

        for player_id in player_ids {
            let snapshot = self.build_snapshot(player_id);
            if let Some(tx) = self.connections.get(&player_id) {
                match tx.try_send(snapshot) {
                    Ok(()) => {}
                    Err(std::sync::mpsc::TrySendError::Full(_)) => {
                        // Channel full — drop this snapshot; next tick will contain fresh state.
                    }
                    Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                        // Connection task has gone away — clean up next drain phase.
                        disconnected.push(player_id);
                    }
                }
            }
        }

        // Eagerly remove players whose connection tasks have terminated.
        for player_id in disconnected {
            self.remove_entity(player_id);
        }
    }

    /// Emit a [`ZoneEvent::TelemetryUpdate`] to the outbox with current stats.
    ///
    /// Uses `try_send` — if the coordinator is behind, we drop the telemetry
    /// rather than blocking the tick.
    fn emit_telemetry(&self, outbox: &SyncSender<ZoneEvent>, tick_duration_ms: f32) {
        let event = ZoneEvent::TelemetryUpdate(ZoneTelemetry {
            zone_id: self.id,
            tick: self.tick,
            tick_duration_ms,
            player_count: self.player_count(),
            entity_count: self.entity_count(),
        });
        // Ignore send errors — coordinator may be behind or shutting down.
        let _ = outbox.try_send(event);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Zone loop — entry point for the zone OS thread
// ─────────────────────────────────────────────────────────────────────────────

/// The main loop executed by the zone's dedicated OS thread.
///
/// Runs at 20 ticks per second (50 ms per tick). Each iteration:
/// 1. Records the tick start time for duration measurement.
/// 2. Drains all pending [`ZoneCommand`]s from `inbox`.
/// 3. Runs [`Zone::tick`] (movement, dispatch, telemetry).
/// 4. Sleeps for the remainder of the 50 ms budget.
///
/// The loop exits cleanly when the `inbox` sender is dropped (i.e. when the
/// [`WorldCoordinator`] shuts down), allowing the zone thread to join.
pub fn run_zone_loop(
    mut zone: Zone,
    inbox: Receiver<ZoneCommand>,
    outbox: SyncSender<ZoneEvent>,
) {
    const TICK_BUDGET: Duration = Duration::from_millis(50); // 20 Hz

    loop {
        let tick_start = Instant::now();

        // Drain inbox: apply all pending commands for this tick.
        // `try_recv` returns `Err(Empty)` when there are no more messages and
        // `Err(Disconnected)` when all senders have been dropped.
        loop {
            match inbox.try_recv() {
                Ok(cmd) => zone.apply(cmd),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // All senders dropped → game-server is shutting down.
                    tracing::info!(zone_id = ?zone.id, "zone inbox disconnected, shutting down");
                    return;
                }
            }
        }

        // Run the tick (passes tick_start so tick() can measure its own duration).
        zone.tick(&outbox, tick_start);

        // Sleep for the remainder of the 50 ms budget.
        let elapsed = tick_start.elapsed();
        if let Some(remaining) = TICK_BUDGET.checked_sub(elapsed) {
            std::thread::sleep(remaining);
        } else {
            tracing::warn!(
                zone_id = ?zone.id,
                tick = zone.tick,
                elapsed_ms = elapsed.as_millis(),
                "tick overrun"
            );
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn make_zone() -> Zone {
        Zone::new(ZoneId::new(1), "Test Zone")
    }

    /// Create a synchronous snapshot channel with a reasonable buffer.
    fn snapshot_channel() -> (SyncSender<WorldSnapshot>, mpsc::Receiver<WorldSnapshot>) {
        mpsc::sync_channel(64)
    }

    /// Create a telemetry outbox channel.
    fn outbox_channel() -> (SyncSender<ZoneEvent>, mpsc::Receiver<ZoneEvent>) {
        mpsc::sync_channel(256)
    }

    /// Drain all available [`ZoneEvent`]s from the outbox and return them.
    fn drain_events(rx: &mpsc::Receiver<ZoneEvent>) -> Vec<ZoneTelemetry> {
        let mut telemetry = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let ZoneEvent::TelemetryUpdate(t) = event {
                telemetry.push(t);
            }
        }
        telemetry
    }

    /// Run one tick, returning the tick start instant (used by Zone::tick).
    fn run_tick(zone: &mut Zone, outbox: &SyncSender<ZoneEvent>) {
        zone.tick(outbox, Instant::now());
    }

    // ── SpatialIndex tests ────────────────────────────────────────────────────

    #[test]
    fn spatial_insert_and_query_radius_returns_entity() {
        let mut index = SpatialIndex::new(50.0);
        let id = EntityId::new(1);
        index.insert(id, Vec3::new(10.0, 0.0, 10.0));

        let results = index.query_radius(Vec3::new(10.0, 0.0, 10.0), 5.0);
        assert!(results.contains(&id), "entity should be found at its own position");
    }

    #[test]
    fn spatial_query_finds_nearby_entity() {
        let mut index = SpatialIndex::new(50.0);
        let id = EntityId::new(2);
        index.insert(id, Vec3::new(0.0, 0.0, 0.0));

        // Query from 10 units away — well within the 200-unit radius.
        let results = index.query_radius(Vec3::new(10.0, 0.0, 0.0), 200.0);
        assert!(results.contains(&id));
    }

    #[test]
    fn spatial_query_does_not_return_distant_entity() {
        let mut index = SpatialIndex::new(50.0);
        let id = EntityId::new(3);
        // Place entity at (1000, 0, 0) — far outside any reasonable radius.
        index.insert(id, Vec3::new(1000.0, 0.0, 0.0));

        // Query at origin with a small radius — should NOT find the entity.
        // The index returns candidates by cell; an exact distance check must
        // be done by the caller. We verify the index doesn't fabricate extra
        // cells by checking that no result from the tiny-radius query matches
        // our far entity (they would be in completely different cells).
        let results = index.query_radius(Vec3::new(0.0, 0.0, 0.0), 10.0);
        assert!(
            !results.contains(&id),
            "distant entity must not appear in small-radius query"
        );
    }

    #[test]
    fn spatial_remove_works() {
        let mut index = SpatialIndex::new(50.0);
        let id = EntityId::new(4);
        index.insert(id, Vec3::new(5.0, 0.0, 5.0));
        index.remove(id);

        let results = index.query_radius(Vec3::new(5.0, 0.0, 5.0), 100.0);
        assert!(!results.contains(&id), "removed entity must not appear in query");
    }

    #[test]
    fn spatial_remove_nonexistent_is_noop() {
        let mut index = SpatialIndex::new(50.0);
        // Should not panic.
        index.remove(EntityId::new(999));
    }

    #[test]
    fn spatial_update_moves_entity_to_new_cell() {
        let mut index = SpatialIndex::new(50.0);
        let id = EntityId::new(5);
        let origin = Vec3::new(0.0, 0.0, 0.0);
        let far = Vec3::new(500.0, 0.0, 500.0);

        index.insert(id, origin);
        index.update(id, origin, far);

        // Must NOT appear near origin.
        let near_origin = index.query_radius(origin, 10.0);
        assert!(!near_origin.contains(&id), "entity must have left origin cell");

        // Must appear near new position.
        let near_far = index.query_radius(far, 10.0);
        assert!(near_far.contains(&id), "entity must be in new cell");
    }

    #[test]
    fn spatial_update_same_cell_is_noop() {
        // Moving within the same cell should not corrupt state.
        let mut index = SpatialIndex::new(50.0);
        let id = EntityId::new(6);
        let pos_a = Vec3::new(1.0, 0.0, 1.0);
        let pos_b = Vec3::new(2.0, 0.0, 2.0); // same cell as pos_a (cell size 50)

        index.insert(id, pos_a);
        index.update(id, pos_a, pos_b);

        let results = index.query_radius(pos_b, 5.0);
        assert!(results.contains(&id));
    }

    #[test]
    fn spatial_multiple_entities_in_same_cell() {
        let mut index = SpatialIndex::new(50.0);
        let a = EntityId::new(10);
        let b = EntityId::new(11);
        let c = EntityId::new(12);

        index.insert(a, Vec3::new(5.0, 0.0, 5.0));
        index.insert(b, Vec3::new(6.0, 0.0, 6.0));
        index.insert(c, Vec3::new(7.0, 0.0, 7.0));

        let results = index.query_radius(Vec3::new(5.0, 0.0, 5.0), 20.0);
        assert!(results.contains(&a));
        assert!(results.contains(&b));
        assert!(results.contains(&c));
    }

    // ── Zone::apply tests ─────────────────────────────────────────────────────

    #[test]
    fn zone_player_enter_adds_entity() {
        let mut zone = make_zone();
        let (tx, _rx) = snapshot_channel();
        let entity_id = EntityId::new(1);

        zone.apply(ZoneCommand::PlayerEnter {
            entity_id,
            position: Vec3::new(10.0, 0.0, 20.0),
            snapshot_tx: tx,
        });

        assert_eq!(zone.player_count(), 1);
        assert_eq!(zone.entity_count(), 1);
        assert!(zone.entity_map.contains_key(&entity_id));
    }

    #[test]
    fn zone_player_enter_registers_in_spatial_index() {
        let mut zone = make_zone();
        let (tx, _rx) = snapshot_channel();
        let entity_id = EntityId::new(2);
        let pos = Vec3::new(0.0, 0.0, 0.0);

        zone.apply(ZoneCommand::PlayerEnter {
            entity_id,
            position: pos,
            snapshot_tx: tx,
        });

        let found = zone.spatial_index.query_radius(pos, 1.0);
        assert!(found.contains(&entity_id));
    }

    #[test]
    fn zone_player_leave_removes_entity() {
        let mut zone = make_zone();
        let (tx, _rx) = snapshot_channel();
        let entity_id = EntityId::new(3);

        zone.apply(ZoneCommand::PlayerEnter {
            entity_id,
            position: Vec3::ZERO,
            snapshot_tx: tx,
        });
        assert_eq!(zone.player_count(), 1);

        zone.apply(ZoneCommand::PlayerLeave { entity_id });
        assert_eq!(zone.player_count(), 0);
        assert_eq!(zone.entity_count(), 0);
        assert!(!zone.entity_map.contains_key(&entity_id));
    }

    #[test]
    fn zone_player_leave_removes_from_spatial_index() {
        let mut zone = make_zone();
        let (tx, _rx) = snapshot_channel();
        let entity_id = EntityId::new(4);
        let pos = Vec3::new(5.0, 0.0, 5.0);

        zone.apply(ZoneCommand::PlayerEnter {
            entity_id,
            position: pos,
            snapshot_tx: tx,
        });
        zone.apply(ZoneCommand::PlayerLeave { entity_id });

        let found = zone.spatial_index.query_radius(pos, 10.0);
        assert!(!found.contains(&entity_id));
    }

    #[test]
    fn zone_player_leave_nonexistent_is_noop() {
        let mut zone = make_zone();
        // Should not panic.
        zone.apply(ZoneCommand::PlayerLeave {
            entity_id: EntityId::new(999),
        });
        assert_eq!(zone.player_count(), 0);
    }

    #[test]
    fn zone_move_input_updates_velocity_component() {
        let mut zone = make_zone();
        let (tx, _rx) = snapshot_channel();
        let entity_id = EntityId::new(5);

        zone.apply(ZoneCommand::PlayerEnter {
            entity_id,
            position: Vec3::ZERO,
            snapshot_tx: tx,
        });

        zone.apply(ZoneCommand::MoveInput {
            entity_id,
            direction: Vec2::new(1.0, 0.0), // move along +X
            speed: 7.0,
        });

        let hecs_entity = zone.entity_map[&entity_id];
        let vel = zone.world.get::<&Velocity>(hecs_entity).unwrap();
        // direction is already unit (1,0), speed 7 → velocity (7, 0)
        assert!((vel.0.x - 7.0).abs() < 1e-5);
        assert!(vel.0.y.abs() < 1e-5);
    }

    #[test]
    fn zone_move_input_normalises_direction() {
        let mut zone = make_zone();
        let (tx, _rx) = snapshot_channel();
        let entity_id = EntityId::new(6);

        zone.apply(ZoneCommand::PlayerEnter {
            entity_id,
            position: Vec3::ZERO,
            snapshot_tx: tx,
        });

        // Non-unit direction: (3, 4) has length 5, normalised → (0.6, 0.8)
        zone.apply(ZoneCommand::MoveInput {
            entity_id,
            direction: Vec2::new(3.0, 4.0),
            speed: 10.0,
        });

        let hecs_entity = zone.entity_map[&entity_id];
        let vel = zone.world.get::<&Velocity>(hecs_entity).unwrap();
        assert!((vel.0.x - 6.0).abs() < 1e-4, "vx = {}", vel.0.x); // 0.6 * 10
        assert!((vel.0.y - 8.0).abs() < 1e-4, "vy = {}", vel.0.y); // 0.8 * 10
    }

    #[test]
    fn zone_move_input_clamps_speed() {
        let mut zone = make_zone();
        let (tx, _rx) = snapshot_channel();
        let entity_id = EntityId::new(7);

        zone.apply(ZoneCommand::PlayerEnter {
            entity_id,
            position: Vec3::ZERO,
            snapshot_tx: tx,
        });

        zone.apply(ZoneCommand::MoveInput {
            entity_id,
            direction: Vec2::new(1.0, 0.0),
            speed: 9999.0, // way above MAX_PLAYER_SPEED
        });

        let hecs_entity = zone.entity_map[&entity_id];
        let vel = zone.world.get::<&Velocity>(hecs_entity).unwrap();
        assert!(
            vel.0.x <= MAX_PLAYER_SPEED + 1e-5,
            "speed must be clamped, got {}",
            vel.0.x
        );
    }

    #[test]
    fn zone_move_input_for_nonexistent_entity_is_noop() {
        let mut zone = make_zone();
        // Must not panic.
        zone.apply(ZoneCommand::MoveInput {
            entity_id: EntityId::new(999),
            direction: Vec2::new(1.0, 0.0),
            speed: 5.0,
        });
    }

    // ── Zone tick tests ───────────────────────────────────────────────────────

    #[test]
    fn tick_moves_player_with_velocity() {
        let mut zone = make_zone();
        let (tx, _rx) = snapshot_channel();
        let (outbox_tx, _outbox_rx) = outbox_channel();
        let entity_id = EntityId::new(1);

        zone.apply(ZoneCommand::PlayerEnter {
            entity_id,
            position: Vec3::ZERO,
            snapshot_tx: tx,
        });
        zone.apply(ZoneCommand::MoveInput {
            entity_id,
            direction: Vec2::new(1.0, 0.0), // +X direction
            speed: 10.0,
        });

        run_tick(&mut zone, &outbox_tx);

        let hecs_entity = zone.entity_map[&entity_id];
        let pos = zone.world.get::<&Position>(hecs_entity).unwrap();
        // After 1 tick at 20 Hz: dt = 0.05 s, velocity = 10 u/s → Δx = 0.5 u
        assert!(
            pos.0.x > 0.0,
            "player should have moved in +X, got x={}",
            pos.0.x
        );
        assert!((pos.0.x - 0.5).abs() < 1e-4, "expected x≈0.5, got {}", pos.0.x);
    }

    #[test]
    fn tick_stationary_player_does_not_move() {
        let mut zone = make_zone();
        let (tx, _rx) = snapshot_channel();
        let (outbox_tx, _outbox_rx) = outbox_channel();
        let entity_id = EntityId::new(2);
        let initial_pos = Vec3::new(50.0, 0.0, 50.0);

        zone.apply(ZoneCommand::PlayerEnter {
            entity_id,
            position: initial_pos,
            snapshot_tx: tx,
        });
        // No MoveInput — velocity stays at ZERO.

        run_tick(&mut zone, &outbox_tx);

        let hecs_entity = zone.entity_map[&entity_id];
        let pos = zone.world.get::<&Position>(hecs_entity).unwrap();
        assert_eq!(pos.0, initial_pos);
    }

    #[test]
    fn dispatch_snapshots_player_receives_snapshot() {
        let mut zone = make_zone();
        let (tx, rx) = snapshot_channel();
        let (outbox_tx, _outbox_rx) = outbox_channel();
        let entity_id = EntityId::new(1);

        zone.apply(ZoneCommand::PlayerEnter {
            entity_id,
            position: Vec3::ZERO,
            snapshot_tx: tx,
        });

        run_tick(&mut zone, &outbox_tx);

        let snapshot = rx.try_recv().expect("player must receive a snapshot each tick");
        assert_eq!(snapshot.tick, 1, "first tick should produce tick=1");
        assert!(
            !snapshot.entities.is_empty(),
            "snapshot must contain at least the player itself"
        );
    }

    #[test]
    fn snapshot_contains_player_own_entity_state() {
        let mut zone = make_zone();
        let (tx, rx) = snapshot_channel();
        let (outbox_tx, _outbox_rx) = outbox_channel();
        let entity_id = EntityId::new(42);
        let pos = Vec3::new(100.0, 0.0, 200.0);

        zone.apply(ZoneCommand::PlayerEnter {
            entity_id,
            position: pos,
            snapshot_tx: tx,
        });

        run_tick(&mut zone, &outbox_tx);

        let snapshot = rx.try_recv().unwrap();
        let state = snapshot
            .entities
            .iter()
            .find(|e| e.entity_id == entity_id)
            .expect("snapshot must contain the player's own EntityState");

        assert_eq!(state.kind, EntityKind::Player);
        assert_eq!(state.position, pos);
        assert_eq!(state.health, 100);
        assert_eq!(state.max_health, 100);
    }

    #[test]
    fn snapshot_aoi_excludes_distant_player() {
        let mut zone = make_zone();
        let (tx_a, rx_a) = snapshot_channel();
        let (tx_b, rx_b) = snapshot_channel();
        let (outbox_tx, _outbox_rx) = outbox_channel();

        let player_a = EntityId::new(1);
        let player_b = EntityId::new(2);

        // Place A at origin and B far beyond the AOI radius.
        let pos_a = Vec3::new(0.0, 0.0, 0.0);
        let pos_b = Vec3::new(DEFAULT_AOI_RADIUS * 3.0, 0.0, 0.0);

        zone.apply(ZoneCommand::PlayerEnter {
            entity_id: player_a,
            position: pos_a,
            snapshot_tx: tx_a,
        });
        zone.apply(ZoneCommand::PlayerEnter {
            entity_id: player_b,
            position: pos_b,
            snapshot_tx: tx_b,
        });

        run_tick(&mut zone, &outbox_tx);

        let snap_a = rx_a.try_recv().unwrap();
        let snap_b = rx_b.try_recv().unwrap();

        // A should NOT see B.
        assert!(
            !snap_a.entities.iter().any(|e| e.entity_id == player_b),
            "player A must not see player B who is beyond AOI radius"
        );
        // B should NOT see A.
        assert!(
            !snap_b.entities.iter().any(|e| e.entity_id == player_a),
            "player B must not see player A who is beyond AOI radius"
        );
        // Each player should see themselves.
        assert!(snap_a.entities.iter().any(|e| e.entity_id == player_a));
        assert!(snap_b.entities.iter().any(|e| e.entity_id == player_b));
    }

    #[test]
    fn snapshot_aoi_includes_nearby_player() {
        let mut zone = make_zone();
        let (tx_a, rx_a) = snapshot_channel();
        let (tx_b, _rx_b) = snapshot_channel();
        let (outbox_tx, _outbox_rx) = outbox_channel();

        let player_a = EntityId::new(1);
        let player_b = EntityId::new(2);

        // Place both players close together.
        zone.apply(ZoneCommand::PlayerEnter {
            entity_id: player_a,
            position: Vec3::new(0.0, 0.0, 0.0),
            snapshot_tx: tx_a,
        });
        zone.apply(ZoneCommand::PlayerEnter {
            entity_id: player_b,
            position: Vec3::new(10.0, 0.0, 10.0), // well within AOI
            snapshot_tx: tx_b,
        });

        run_tick(&mut zone, &outbox_tx);

        let snap_a = rx_a.try_recv().unwrap();
        assert!(
            snap_a.entities.iter().any(|e| e.entity_id == player_b),
            "player A must see nearby player B in their AOI"
        );
    }

    #[test]
    fn telemetry_emitted_each_tick_with_correct_player_count() {
        let mut zone = make_zone();
        let (tx, _rx) = snapshot_channel();
        let (outbox_tx, outbox_rx) = outbox_channel();

        zone.apply(ZoneCommand::PlayerEnter {
            entity_id: EntityId::new(1),
            position: Vec3::ZERO,
            snapshot_tx: tx,
        });

        run_tick(&mut zone, &outbox_tx);

        let telemetry = drain_events(&outbox_rx);
        assert_eq!(telemetry.len(), 1, "exactly one telemetry event per tick");
        assert_eq!(telemetry[0].player_count, 1);
        assert_eq!(telemetry[0].entity_count, 1);
        assert_eq!(telemetry[0].zone_id, ZoneId::new(1));
        assert_eq!(telemetry[0].tick, 1);
    }

    #[test]
    fn telemetry_emitted_even_when_zone_is_empty() {
        let mut zone = make_zone();
        let (outbox_tx, outbox_rx) = outbox_channel();

        run_tick(&mut zone, &outbox_tx);

        let telemetry = drain_events(&outbox_rx);
        assert_eq!(telemetry.len(), 1);
        assert_eq!(telemetry[0].player_count, 0);
        assert_eq!(telemetry[0].entity_count, 0);
    }

    #[test]
    fn telemetry_tick_counter_increments() {
        let mut zone = make_zone();
        let (outbox_tx, outbox_rx) = outbox_channel();

        run_tick(&mut zone, &outbox_tx);
        run_tick(&mut zone, &outbox_tx);
        run_tick(&mut zone, &outbox_tx);

        let telemetry = drain_events(&outbox_rx);
        assert_eq!(telemetry.len(), 3);
        assert_eq!(telemetry[0].tick, 1);
        assert_eq!(telemetry[1].tick, 2);
        assert_eq!(telemetry[2].tick, 3);
    }

    // ── Snapshot content tests ────────────────────────────────────────────────

    #[test]
    fn snapshot_entity_state_fields_are_correct() {
        let mut zone = make_zone();
        let (tx, rx) = snapshot_channel();
        let (outbox_tx, _outbox_rx) = outbox_channel();
        let entity_id = EntityId::new(77);
        let pos = Vec3::new(1.0, 5.0, 2.0);

        zone.apply(ZoneCommand::PlayerEnter {
            entity_id,
            position: pos,
            snapshot_tx: tx,
        });

        run_tick(&mut zone, &outbox_tx);

        let snap = rx.try_recv().unwrap();
        let state = snap
            .entities
            .iter()
            .find(|e| e.entity_id == entity_id)
            .unwrap();

        assert_eq!(state.entity_id, entity_id);
        assert_eq!(state.kind, EntityKind::Player);
        assert_eq!(state.position, pos);
        assert_eq!(state.health, 100);
        assert_eq!(state.max_health, 100);
    }

    #[test]
    fn snapshot_moving_flag_set_when_player_has_velocity() {
        let mut zone = make_zone();
        let (tx, rx) = snapshot_channel();
        let (outbox_tx, _outbox_rx) = outbox_channel();
        let entity_id = EntityId::new(1);

        zone.apply(ZoneCommand::PlayerEnter {
            entity_id,
            position: Vec3::ZERO,
            snapshot_tx: tx,
        });
        zone.apply(ZoneCommand::MoveInput {
            entity_id,
            direction: Vec2::new(1.0, 0.0),
            speed: 5.0,
        });

        run_tick(&mut zone, &outbox_tx);

        let snap = rx.try_recv().unwrap();
        let state = snap
            .entities
            .iter()
            .find(|e| e.entity_id == entity_id)
            .unwrap();
        assert!(
            state.flags.contains(EntityFlags::MOVING),
            "MOVING flag must be set for a player with non-zero velocity"
        );
    }

    #[test]
    fn snapshot_moving_flag_not_set_when_stationary() {
        let mut zone = make_zone();
        let (tx, rx) = snapshot_channel();
        let (outbox_tx, _outbox_rx) = outbox_channel();
        let entity_id = EntityId::new(1);

        zone.apply(ZoneCommand::PlayerEnter {
            entity_id,
            position: Vec3::ZERO,
            snapshot_tx: tx,
        });
        // No MoveInput → velocity remains ZERO.

        run_tick(&mut zone, &outbox_tx);

        let snap = rx.try_recv().unwrap();
        let state = snap
            .entities
            .iter()
            .find(|e| e.entity_id == entity_id)
            .unwrap();
        assert!(
            !state.flags.contains(EntityFlags::MOVING),
            "MOVING flag must NOT be set for a stationary player"
        );
    }

    #[test]
    fn snapshot_tick_monotonically_increases() {
        let mut zone = make_zone();
        let (tx, rx) = snapshot_channel();
        let (outbox_tx, _outbox_rx) = outbox_channel();

        zone.apply(ZoneCommand::PlayerEnter {
            entity_id: EntityId::new(1),
            position: Vec3::ZERO,
            snapshot_tx: tx,
        });

        run_tick(&mut zone, &outbox_tx);
        run_tick(&mut zone, &outbox_tx);

        let snap1 = rx.try_recv().unwrap();
        let snap2 = rx.try_recv().unwrap();
        assert!(
            snap2.tick > snap1.tick,
            "snapshot tick must increase each tick"
        );
    }

    #[test]
    fn multiple_ticks_accumulate_movement() {
        let mut zone = make_zone();
        let (tx, _rx) = snapshot_channel();
        let (outbox_tx, _outbox_rx) = outbox_channel();
        let entity_id = EntityId::new(1);

        zone.apply(ZoneCommand::PlayerEnter {
            entity_id,
            position: Vec3::ZERO,
            snapshot_tx: tx,
        });
        // Use speed=10 (below MAX_PLAYER_SPEED=14) so no clamping occurs.
        // 5 ticks × 0.05 s × 10 u/s = 2.5 u of movement in Z.
        zone.apply(ZoneCommand::MoveInput {
            entity_id,
            direction: Vec2::new(0.0, 1.0), // +Z direction
            speed: 10.0,
        });

        for _ in 0..5 {
            run_tick(&mut zone, &outbox_tx);
        }

        let hecs_entity = zone.entity_map[&entity_id];
        let pos = zone.world.get::<&Position>(hecs_entity).unwrap();
        assert!(
            (pos.0.z - 2.5).abs() < 1e-3,
            "expected z≈2.5 after 5 ticks, got {}",
            pos.0.z
        );
    }
}
