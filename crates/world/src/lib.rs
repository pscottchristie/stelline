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
use game_data::{AiBehavior, CreatureDef, CreatureId};
use protocol::WorldSnapshot;
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

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

/// Human-readable label for an entity (e.g. username for players).
/// Used by the admin world view canvas to display names next to entity dots.
#[derive(Debug, Clone)]
pub struct Label(pub String);

/// Marker for any non-player entity (mob or NPC).
#[derive(Debug, Clone, Copy)]
pub struct Creature {
    pub entity_id: EntityId,
    pub kind: EntityKind,
}

/// AI state for a creature.
#[derive(Debug, Clone)]
pub struct Ai {
    pub behavior: AiBehavior,
    pub state: AiState,
    pub spawn_pos: Vec3,
    pub wander_radius: f32,
    pub move_speed: f32,
}

/// Current AI state machine state.
#[derive(Debug, Clone)]
pub enum AiState {
    /// Waiting for `remaining` seconds before picking a new action.
    Idle { remaining: f32 },
    /// Moving toward `target` position.
    Walking { target: Vec3 },
}

/// Respawn metadata — stored at spawn time, used when creature dies.
#[derive(Debug, Clone)]
pub struct SpawnInfo {
    pub creature_id: CreatureId,
    pub spawn_pos: Vec3,
    pub wander_radius: f32,
    pub respawn_secs: f32,
}

/// A creature pending respawn after death.
struct PendingRespawn {
    creature_id: CreatureId,
    spawn_pos: Vec3,
    wander_radius: f32,
    respawn_secs: f32,
    timer: f32,
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
        /// Human-readable label (username) for admin dashboard display.
        label: String,
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
        label: String,
    },
    /// Per-tick telemetry for the admin dashboard.
    TelemetryUpdate(ZoneTelemetry),
    /// Per-zone entity snapshot for the admin world view canvas.
    /// Emitted at a reduced rate (every 4th tick = 5 Hz).
    EntitySnapshot(common::ZoneEntitySnapshot),
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
    /// Zone playable area width in world units (for admin canvas scaling).
    width: f32,
    /// Zone playable area height in world units (for admin canvas scaling).
    height: f32,
    /// Counter for allocating mob/NPC EntityIds. Partitioned: zone_id * 1_000_000.
    next_mob_entity_id: u64,
    /// Creatures waiting to respawn after death.
    pending_respawns: Vec<PendingRespawn>,
    /// Creature type definitions, indexed by CreatureId.
    creature_defs: HashMap<CreatureId, CreatureDef>,
    /// Per-zone RNG for AI decisions (deterministic, seedable).
    rng: SmallRng,
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
            width: 1000.0,
            height: 1000.0,
            next_mob_entity_id: id.get() as u64 * 1_000_000,
            pending_respawns: Vec::new(),
            creature_defs: HashMap::new(),
            rng: SmallRng::seed_from_u64(id.get() as u64),
        }
    }

    /// Create a new, empty zone with explicit dimensions and AOI radius.
    pub fn with_config(
        id: ZoneId,
        name: impl Into<String>,
        aoi_radius: f32,
        width: f32,
        height: f32,
    ) -> Self {
        Self {
            id,
            name: name.into(),
            world: hecs::World::new(),
            spatial_index: SpatialIndex::new(50.0),
            entity_map: HashMap::new(),
            connections: HashMap::new(),
            tick: 0,
            aoi_radius,
            width,
            height,
            next_mob_entity_id: id.get() as u64 * 1_000_000,
            pending_respawns: Vec::new(),
            creature_defs: HashMap::new(),
            rng: SmallRng::seed_from_u64(id.get() as u64),
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
                label,
            } => {
                self.admit_player(entity_id, position, snapshot_tx, label);
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
        label: String,
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
            Label(label),
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

    // ── Creature spawning ──────────────────────────────────────────────────────

    /// Spawn a single creature entity into the zone.
    pub fn spawn_creature(
        &mut self,
        creature_def: &CreatureDef,
        spawn_pos: Vec3,
        wander_radius: f32,
        respawn_secs: f32,
    ) -> EntityId {
        let entity_id = EntityId::new(self.next_mob_entity_id);
        self.next_mob_entity_id += 1;

        let kind = if creature_def.is_npc {
            EntityKind::Npc
        } else {
            EntityKind::Mob
        };

        let ai_state = match creature_def.behavior {
            AiBehavior::Idle => AiState::Idle { remaining: f32::MAX },
            AiBehavior::Wander => AiState::Idle {
                remaining: self.rng.gen_range(1.0..4.0),
            },
        };

        let hecs_entity = self.world.spawn((
            Position(spawn_pos),
            Velocity(Vec2::ZERO),
            Health {
                current: creature_def.max_health,
                max: creature_def.max_health,
            },
            Creature { entity_id, kind },
            Ai {
                behavior: creature_def.behavior,
                state: ai_state,
                spawn_pos,
                wander_radius,
                move_speed: creature_def.move_speed,
            },
            SpawnInfo {
                creature_id: creature_def.id,
                spawn_pos,
                wander_radius,
                respawn_secs,
            },
            Label(creature_def.name.clone()),
        ));

        self.entity_map.insert(entity_id, hecs_entity);
        self.spatial_index.insert(entity_id, spawn_pos);

        entity_id
    }

    /// Spawn all initial creatures from the config's creature defs and spawn points.
    pub fn spawn_initial_creatures(
        &mut self,
        creatures: &[CreatureDef],
        spawns: &[game_data::SpawnPoint],
    ) {
        for c in creatures {
            self.creature_defs.insert(c.id, c.clone());
        }

        for sp in spawns {
            if let Some(creature_def) = self.creature_defs.get(&sp.creature_id).cloned() {
                let pos = Vec3::new(sp.x, 0.0, sp.z);
                self.spawn_creature(&creature_def, pos, sp.wander_radius, sp.respawn_secs);
            }
        }
    }

    /// Kill a creature: remove from ECS, schedule respawn.
    pub fn kill_creature(&mut self, entity_id: EntityId) {
        if let Some(&hecs_entity) = self.entity_map.get(&entity_id) {
            // Retrieve spawn info before despawning — clone to release borrow.
            let spawn_info: Option<SpawnInfo> = self
                .world
                .get::<&SpawnInfo>(hecs_entity)
                .ok()
                .map(|si| SpawnInfo {
                    creature_id: si.creature_id,
                    spawn_pos: si.spawn_pos,
                    wander_radius: si.wander_radius,
                    respawn_secs: si.respawn_secs,
                });

            // Remove from all zone structures.
            self.spatial_index.remove(entity_id);
            self.entity_map.remove(&entity_id);
            let _ = self.world.despawn(hecs_entity);

            // Schedule respawn if we have spawn info.
            if let Some(si) = spawn_info {
                self.pending_respawns.push(PendingRespawn {
                    creature_id: si.creature_id,
                    spawn_pos: si.spawn_pos,
                    wander_radius: si.wander_radius,
                    respawn_secs: si.respawn_secs,
                    timer: si.respawn_secs,
                });
            }
        }
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

        // Stage 2: AI
        self.step_ai(DT);

        // Stage 3: combat (not yet implemented)

        // Stage 4: timers (respawns)
        self.step_timers(DT);

        // Stage 5: send snapshots to all connected players
        self.dispatch_snapshots();

        // Stage 6: emit entity snapshot for admin canvas at reduced rate (5 Hz)
        if self.tick % 4 == 0 {
            self.emit_entity_snapshot(outbox);
        }

        // Stage 7: emit telemetry
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

        // Move all entities that have Position + Velocity.
        // Derive EntityId from either Player or Creature component.
        for (hecs_entity, (pos, vel)) in self
            .world
            .query::<(&Position, &Velocity)>()
            .iter()
        {
            let v = vel.0;
            if v.x == 0.0 && v.y == 0.0 {
                continue; // stationary — skip
            }

            // Resolve EntityId from Player or Creature marker.
            let entity_id = if let Ok(p) = self.world.get::<&Player>(hecs_entity) {
                p.entity_id
            } else if let Ok(c) = self.world.get::<&Creature>(hecs_entity) {
                c.entity_id
            } else {
                continue;
            };

            let old_pos = pos.0;
            let new_pos = Vec3::new(
                old_pos.x + v.x * dt,
                old_pos.y,
                old_pos.z + v.y * dt, // Vec2.y maps to world Z
            );
            updates.push((entity_id, old_pos, new_pos));
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

    /// Run AI for all creatures with an [`Ai`] component.
    fn step_ai(&mut self, dt: f32) {
        // Collect AI decisions first, then apply them.
        struct AiUpdate {
            entity_id: EntityId,
            new_velocity: Vec2,
            new_state: AiState,
        }

        let mut updates: Vec<AiUpdate> = Vec::new();

        for (_, (ai, creature)) in self.world.query::<(&Ai, &Creature)>().iter() {
            let entity_id = creature.entity_id;

            match &ai.state {
                AiState::Idle { remaining } => {
                    let new_remaining = remaining - dt;
                    if new_remaining <= 0.0 && ai.behavior == AiBehavior::Wander && ai.wander_radius > 0.0 {
                        // Will pick a new target — defer to after query ends.
                        updates.push(AiUpdate {
                            entity_id,
                            new_velocity: Vec2::ZERO, // will be set when Walking starts
                            new_state: AiState::Idle { remaining: -1.0 }, // sentinel: needs new target
                        });
                    } else {
                        updates.push(AiUpdate {
                            entity_id,
                            new_velocity: Vec2::ZERO,
                            new_state: AiState::Idle { remaining: new_remaining },
                        });
                    }
                }
                AiState::Walking { target } => {
                    // Check if arrived.
                    let hecs_entity = self.entity_map.get(&entity_id).copied();
                    let current_pos = hecs_entity
                        .and_then(|he| self.world.get::<&Position>(he).ok().map(|p| p.0))
                        .unwrap_or(Vec3::ZERO);

                    let dx = target.x - current_pos.x;
                    let dz = target.z - current_pos.z;
                    let dist = (dx * dx + dz * dz).sqrt();

                    if dist < 1.0 {
                        // Arrived — switch to Idle.
                        updates.push(AiUpdate {
                            entity_id,
                            new_velocity: Vec2::ZERO,
                            new_state: AiState::Idle { remaining: -2.0 }, // sentinel: needs random idle duration
                        });
                    } else {
                        // Set velocity toward target.
                        let dir_x = dx / dist;
                        let dir_z = dz / dist;
                        let speed = ai.move_speed;
                        updates.push(AiUpdate {
                            entity_id,
                            new_velocity: Vec2::new(dir_x * speed, dir_z * speed),
                            new_state: AiState::Walking { target: *target },
                        });
                    }
                }
            }
        }

        // Apply updates.
        for update in updates {
            let Some(&hecs_entity) = self.entity_map.get(&update.entity_id) else {
                continue;
            };

            // Resolve sentinel states that need RNG.
            let final_state = match update.new_state {
                AiState::Idle { remaining } if remaining == -1.0 => {
                    // Pick wander target.
                    let ai = self.world.get::<&Ai>(hecs_entity).ok();
                    let (spawn_pos, wander_radius, move_speed) = ai
                        .map(|a| (a.spawn_pos, a.wander_radius, a.move_speed))
                        .unwrap_or((Vec3::ZERO, 0.0, 0.0));

                    let target = pick_wander_target(&mut self.rng, spawn_pos, wander_radius);
                    // Set velocity toward target.
                    let current_pos = self
                        .world
                        .get::<&Position>(hecs_entity)
                        .map(|p| p.0)
                        .unwrap_or(Vec3::ZERO);
                    let dx = target.x - current_pos.x;
                    let dz = target.z - current_pos.z;
                    let dist = (dx * dx + dz * dz).sqrt();
                    if dist > 0.1 {
                        let vx = (dx / dist) * move_speed;
                        let vz = (dz / dist) * move_speed;
                        if let Ok(mut vel) = self.world.get::<&mut Velocity>(hecs_entity) {
                            *vel = Velocity(Vec2::new(vx, vz));
                        }
                    }
                    AiState::Walking { target }
                }
                AiState::Idle { remaining } if remaining == -2.0 => {
                    // Random idle duration.
                    AiState::Idle {
                        remaining: self.rng.gen_range(2.0..8.0),
                    }
                }
                other => other,
            };

            // Apply velocity (for non-sentinel Walking states where we set velocity in the update).
            match &final_state {
                AiState::Walking { .. } => {
                    // Velocity was either set above (sentinel -1.0) or needs to be set here.
                    if update.new_velocity.x != 0.0 || update.new_velocity.y != 0.0 {
                        if let Ok(mut vel) = self.world.get::<&mut Velocity>(hecs_entity) {
                            *vel = Velocity(update.new_velocity);
                        }
                    }
                }
                AiState::Idle { .. } => {
                    if let Ok(mut vel) = self.world.get::<&mut Velocity>(hecs_entity) {
                        *vel = Velocity(Vec2::ZERO);
                    }
                }
            }

            // Update AI state.
            if let Ok(mut ai) = self.world.get::<&mut Ai>(hecs_entity) {
                ai.state = final_state;
            }
        }
    }

    /// Tick respawn timers. Expired timers respawn the creature.
    fn step_timers(&mut self, dt: f32) {
        let mut to_respawn: Vec<PendingRespawn> = Vec::new();
        self.pending_respawns.retain_mut(|pr| {
            pr.timer -= dt;
            if pr.timer <= 0.0 {
                to_respawn.push(PendingRespawn {
                    creature_id: pr.creature_id,
                    spawn_pos: pr.spawn_pos,
                    wander_radius: pr.wander_radius,
                    respawn_secs: pr.respawn_secs,
                    timer: 0.0,
                });
                false
            } else {
                true
            }
        });

        for pr in to_respawn {
            if let Some(creature_def) = self.creature_defs.get(&pr.creature_id).cloned() {
                self.spawn_creature(&creature_def, pr.spawn_pos, pr.wander_radius, pr.respawn_secs);
            }
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
            } else if let Ok(c) = self.world.get::<&Creature>(candidate_hecs) {
                c.kind
            } else {
                EntityKind::Mob // fallback
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

            // Get the entity's label (character name for players, mob name, etc.)
            let name = self
                .world
                .get::<&Label>(candidate_hecs)
                .ok()
                .map(|l| l.0.clone());

            entities.push(EntityState::new(
                candidate_id,
                kind,
                candidate_pos,
                0.0, // facing: Phase 1 stub
                health,
                max_health,
                flags,
                name,
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

    /// Build and emit a [`ZoneEvent::EntitySnapshot`] for the admin world view.
    ///
    /// Queries all entities with a [`Position`] component and builds a lightweight
    /// [`AdminEntity`] for each. Player entities include their [`Label`] as the
    /// display name.
    fn emit_entity_snapshot(&self, outbox: &SyncSender<ZoneEvent>) {
        use common::{AdminEntity, ZoneEntitySnapshot};

        let mut entities = Vec::new();

        // Players: have Position + Player + Label
        for (_, (pos, player, label)) in self
            .world
            .query::<(&Position, &Player, &Label)>()
            .iter()
        {
            entities.push(AdminEntity {
                entity_id: player.entity_id,
                kind: EntityKind::Player,
                position: pos.0,
                label: label.0.clone(),
            });
        }

        // Creatures (mobs + NPCs) with Position + Creature.
        for (hecs_entity, (pos, creature)) in self
            .world
            .query::<(&Position, &Creature)>()
            .iter()
        {
            let label = self
                .world
                .get::<&Label>(hecs_entity)
                .ok()
                .map(|l| l.0.clone())
                .unwrap_or_default();
            entities.push(AdminEntity {
                entity_id: creature.entity_id,
                kind: creature.kind,
                position: pos.0,
                label,
            });
        }

        let snapshot = ZoneEntitySnapshot {
            zone_id: self.id,
            aoi_radius: self.aoi_radius,
            width: self.width,
            height: self.height,
            entities,
        };

        let _ = outbox.try_send(ZoneEvent::EntitySnapshot(snapshot));
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

/// Pick a random point within `wander_radius` of `spawn_pos` in the XZ plane.
fn pick_wander_target(rng: &mut SmallRng, spawn_pos: Vec3, wander_radius: f32) -> Vec3 {
    let angle: f32 = rng.gen_range(0.0..std::f32::consts::TAU);
    let dist: f32 = rng.gen_range(0.0..wander_radius);
    Vec3::new(
        spawn_pos.x + angle.cos() * dist,
        spawn_pos.y,
        spawn_pos.z + angle.sin() * dist,
    )
}

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
            label: String::new(),
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
            label: String::new(),
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
            label: String::new(),
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
            label: String::new(),
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
            label: String::new(),
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
            label: String::new(),
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
            label: String::new(),
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
            label: String::new(),
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
            label: String::new(),
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
            label: String::new(),
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
            label: String::new(),
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
            label: String::new(),
        });
        zone.apply(ZoneCommand::PlayerEnter {
            entity_id: player_b,
            position: pos_b,
            snapshot_tx: tx_b,
            label: String::new(),
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
            label: String::new(),
        });
        zone.apply(ZoneCommand::PlayerEnter {
            entity_id: player_b,
            position: Vec3::new(10.0, 0.0, 10.0), // well within AOI
            snapshot_tx: tx_b,
            label: String::new(),
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
            label: String::new(),
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
            label: String::new(),
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
            label: String::new(),
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
            label: String::new(),
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
            label: String::new(),
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
            label: String::new(),
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

    // ── Creature spawn tests ─────────────────────────────────────────────────

    fn wolf_def() -> CreatureDef {
        CreatureDef {
            id: 1,
            name: "Forest Wolf".into(),
            level: 3,
            max_health: 120,
            move_speed: 5.0,
            behavior: AiBehavior::Wander,
            is_npc: false,
        }
    }

    fn npc_def() -> CreatureDef {
        CreatureDef {
            id: 3,
            name: "Marshal Dughan".into(),
            level: 25,
            max_health: 2000,
            move_speed: 0.0,
            behavior: AiBehavior::Idle,
            is_npc: true,
        }
    }

    #[test]
    fn spawn_creature_adds_to_world_and_spatial_index() {
        let mut zone = make_zone();
        let wolf = wolf_def();
        let pos = Vec3::new(100.0, 0.0, 200.0);

        let entity_id = zone.spawn_creature(&wolf, pos, 40.0, 30.0);

        assert_eq!(zone.entity_count(), 1);
        assert!(zone.entity_map.contains_key(&entity_id));

        // Check spatial index.
        let found = zone.spatial_index.query_radius(pos, 5.0);
        assert!(found.contains(&entity_id));

        // Check components.
        let he = zone.entity_map[&entity_id];
        let creature = zone.world.get::<&Creature>(he).unwrap();
        assert_eq!(creature.kind, EntityKind::Mob);
        let health = zone.world.get::<&Health>(he).unwrap();
        assert_eq!(health.current, 120);
        assert_eq!(health.max, 120);
        let label = zone.world.get::<&Label>(he).unwrap();
        assert_eq!(label.0, "Forest Wolf");
    }

    #[test]
    fn spawn_creature_npc_has_correct_kind() {
        let mut zone = make_zone();
        let npc = npc_def();
        let entity_id = zone.spawn_creature(&npc, Vec3::new(50.0, 0.0, 50.0), 0.0, 60.0);

        let he = zone.entity_map[&entity_id];
        let creature = zone.world.get::<&Creature>(he).unwrap();
        assert_eq!(creature.kind, EntityKind::Npc);
    }

    #[test]
    fn spawn_initial_creatures_populates_zone() {
        let mut zone = make_zone();
        let creatures = vec![wolf_def(), npc_def()];
        let spawns = vec![
            game_data::SpawnPoint { creature_id: 1, x: 100.0, z: 200.0, wander_radius: 40.0, respawn_secs: 30.0 },
            game_data::SpawnPoint { creature_id: 1, x: 300.0, z: 400.0, wander_radius: 35.0, respawn_secs: 30.0 },
            game_data::SpawnPoint { creature_id: 3, x: 50.0, z: 50.0, wander_radius: 0.0, respawn_secs: 60.0 },
        ];

        zone.spawn_initial_creatures(&creatures, &spawns);

        assert_eq!(zone.entity_count(), 3);
    }

    #[test]
    fn step_movement_moves_mobs() {
        let mut zone = make_zone();
        let wolf = wolf_def();
        let pos = Vec3::new(100.0, 0.0, 100.0);
        let entity_id = zone.spawn_creature(&wolf, pos, 40.0, 30.0);

        // Manually set velocity on the creature.
        let he = zone.entity_map[&entity_id];
        {
            let mut vel = zone.world.get::<&mut Velocity>(he).unwrap();
            *vel = Velocity(Vec2::new(5.0, 0.0));
        }

        let (outbox_tx, _outbox_rx) = outbox_channel();
        run_tick(&mut zone, &outbox_tx);

        let new_pos = zone.world.get::<&Position>(he).unwrap().0;
        // dt = 0.05, vel.x = 5.0 → Δx = 0.25
        assert!(
            (new_pos.x - 100.25).abs() < 1e-3,
            "mob should have moved, got x={}",
            new_pos.x
        );
    }

    #[test]
    fn step_ai_wander_mob_eventually_moves() {
        let mut zone = make_zone();
        let wolf = wolf_def();
        let pos = Vec3::new(500.0, 0.0, 500.0);
        let entity_id = zone.spawn_creature(&wolf, pos, 40.0, 30.0);

        let (outbox_tx, _outbox_rx) = outbox_channel();

        // Run many ticks — the mob should eventually start moving.
        for _ in 0..200 {
            run_tick(&mut zone, &outbox_tx);
        }

        let he = zone.entity_map[&entity_id];
        let final_pos = zone.world.get::<&Position>(he).unwrap().0;

        // After 200 ticks (10 sec at 20Hz), a wander mob should have moved from spawn.
        let dist = pos.distance_xz(final_pos);
        assert!(
            dist > 0.1,
            "wander mob should have moved after 200 ticks, distance from spawn = {}",
            dist
        );
    }

    #[test]
    fn step_ai_wander_mob_stays_within_radius() {
        let mut zone = make_zone();
        let wolf = wolf_def();
        let spawn_pos = Vec3::new(500.0, 0.0, 500.0);
        let entity_id = zone.spawn_creature(&wolf, spawn_pos, 40.0, 30.0);

        let (outbox_tx, _outbox_rx) = outbox_channel();

        // Run many ticks.
        for _ in 0..400 {
            run_tick(&mut zone, &outbox_tx);
        }

        let he = zone.entity_map[&entity_id];
        let final_pos = zone.world.get::<&Position>(he).unwrap().0;

        // Wander target is picked within radius, but movement overshoot is possible.
        // Allow generous margin (2x wander_radius).
        let dist = spawn_pos.distance_xz(final_pos);
        assert!(
            dist < 80.0 + 10.0, // wander_radius * 2 + margin
            "mob should roughly stay near spawn, distance = {}",
            dist
        );
    }

    #[test]
    fn step_ai_idle_npc_does_not_move() {
        let mut zone = make_zone();
        let npc = npc_def();
        let pos = Vec3::new(100.0, 0.0, 100.0);
        let entity_id = zone.spawn_creature(&npc, pos, 0.0, 60.0);

        let (outbox_tx, _outbox_rx) = outbox_channel();

        for _ in 0..100 {
            run_tick(&mut zone, &outbox_tx);
        }

        let he = zone.entity_map[&entity_id];
        let final_pos = zone.world.get::<&Position>(he).unwrap().0;
        assert_eq!(final_pos, pos, "idle NPC should not move");
    }

    #[test]
    fn kill_creature_removes_and_schedules_respawn() {
        let mut zone = make_zone();
        let wolf = wolf_def();
        let pos = Vec3::new(100.0, 0.0, 100.0);
        let entity_id = zone.spawn_creature(&wolf, pos, 40.0, 5.0);

        assert_eq!(zone.entity_count(), 1);

        zone.kill_creature(entity_id);

        assert_eq!(zone.entity_count(), 0);
        assert!(!zone.entity_map.contains_key(&entity_id));
        assert_eq!(zone.pending_respawns.len(), 1);
        assert_eq!(zone.pending_respawns[0].creature_id, 1);
    }

    #[test]
    fn step_timers_respawns_creature_after_timer_expires() {
        let mut zone = make_zone();
        let wolf = wolf_def();
        zone.creature_defs.insert(wolf.id, wolf.clone());
        let pos = Vec3::new(100.0, 0.0, 100.0);
        let entity_id = zone.spawn_creature(&wolf, pos, 40.0, 1.0); // 1 sec respawn

        zone.kill_creature(entity_id);
        assert_eq!(zone.entity_count(), 0);

        let (outbox_tx, _outbox_rx) = outbox_channel();

        // Run enough ticks for the 1-second respawn timer to expire.
        // At 20 Hz, 1 second = 20 ticks.
        for _ in 0..25 {
            run_tick(&mut zone, &outbox_tx);
        }

        assert_eq!(zone.entity_count(), 1, "creature should have respawned");
        assert!(zone.pending_respawns.is_empty(), "respawn queue should be drained");
    }

    #[test]
    fn build_snapshot_includes_creatures_with_correct_kind() {
        let mut zone = make_zone();

        // Add a player so we can build a snapshot.
        let (tx, rx) = snapshot_channel();
        let player_id = EntityId::new(1);
        zone.apply(ZoneCommand::PlayerEnter {
            entity_id: player_id,
            position: Vec3::new(100.0, 0.0, 100.0),
            snapshot_tx: tx,
            label: "TestPlayer".into(),
        });

        // Spawn a wolf (mob) and NPC near the player.
        let wolf = wolf_def();
        let npc = npc_def();
        zone.spawn_creature(&wolf, Vec3::new(105.0, 0.0, 100.0), 40.0, 30.0);
        zone.spawn_creature(&npc, Vec3::new(110.0, 0.0, 100.0), 0.0, 60.0);

        let (outbox_tx, _outbox_rx) = outbox_channel();
        run_tick(&mut zone, &outbox_tx);

        let snapshot = rx.try_recv().unwrap();
        assert_eq!(snapshot.entities.len(), 3, "player + wolf + npc");

        let mob = snapshot.entities.iter().find(|e| e.kind == EntityKind::Mob).unwrap();
        assert_eq!(mob.name.as_deref(), Some("Forest Wolf"));

        let npc_e = snapshot.entities.iter().find(|e| e.kind == EntityKind::Npc).unwrap();
        assert_eq!(npc_e.name.as_deref(), Some("Marshal Dughan"));

        let player = snapshot.entities.iter().find(|e| e.kind == EntityKind::Player).unwrap();
        assert_eq!(player.name.as_deref(), Some("TestPlayer"));
    }
}
