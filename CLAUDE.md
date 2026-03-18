# Stelline — MMO Game Server

## Project Goal
Build a Classic WoW-scope MMO game server in Rust. Both a production-grade engineering effort and a learning exercise in game server development.

## Scope
"Classic WoW scope" means *gameplay depth*, not WoW protocol compatibility:
- Persistent open world, zones, travel
- Classes, levels, stats, talent-style progression
- Real-time combat: spells, cooldowns, auras, threat
- Parties, guilds, dungeons/raids
- Economy, quests, NPC AI
We define our own wire protocol, auth mechanism, and data formats.

## Tech Stack
| Concern | Choice | Why |
|---|---|---|
| Language | Rust | Performance, memory safety, no GC pauses |
| Async runtime | Tokio | Industry standard, excellent ecosystem |
| ECS | hecs | Minimal entity/component storage, no scheduler — we control tick order explicitly |
| Database | PostgreSQL + sqlx | Async, compile-time SQL query verification |
| Cache | Redis | Fast ephemeral storage (token blocklist) |
| Serialization | rmp-serde (MessagePack) | Named format, cross-language (Godot/Unity/etc.) |
| Logging | tracing | Structured, async-aware |
| Pathfinding | Recastnavigation via FFI (or navmesh crate) | Battle-tested nav meshes |

### Why hecs over bevy_ecs
bevy_ecs parallelizes systems automatically via its scheduler — valuable when systems are independent.
Our tick has inherently sequential stages (apply input → move → combat → emit events), so the
scheduler fights us rather than helps. hecs gives us cache-friendly archetype storage with zero
scheduler overhead. We call processing functions in explicit order. Easy to read, easy to learn.
Migrating to bevy_ecs later is straightforward if we need intra-tick parallelism.

---

## Process Architecture

### Two binaries
- **login-server**: login, bcrypt password check, JWT issuance, realm list. No game logic.
- **game-server**: validates JWT on connect, runs ECS world. No credential management.

### Token flow
```
Client ──credentials──► Auth Server ──JWT──► Client
Client ──JWT──► Game Server ──validate (Redis blocklist)──► admit/reject
```
JWT + Redis blocklist: stateless validation normally, immediate revocation by adding token ID to
Redis. Checked once at connect, not every tick.

---

## Game Server Internal Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│                      game-server process                          │
│                                                                   │
│  ┌────────────────────────────────────────────────────────────┐  │
│  │                   Tokio Runtime (async)                    │  │
│  │                                                            │  │
│  │  Connection Task (one per client)                          │  │
│  │    TCP read  → parse → route to zone inbox or service      │  │
│  │    TCP write ← control messages (Handshake, ZoneTransfer)  │  │
│  │    snapshot_rx ← WorldSnapshot from zone (sent each tick)  │  │
│  │         │ (Bytes, SocketAddr)                              │  │
│  │         ▼                                                  │  │
│  │  UDP Dispatch Task  (owns UdpSocket, no Arc)               │  │
│  │    recv (payload, addr) → socket.send_to(addr)             │  │
│  │                                                            │  │
│  │  WorldCoordinator (async task — routing only)              │  │
│  │    - zone transfers (coord Zone A → Zone B handoff)        │  │
│  │    - route service feedback to correct zone inbox          │  │
│  │    - aggregate telemetry → watch::Sender<AdminSnapshot>    │  │
│  │                                                            │  │
│  │  ChatService / AuctionService / MailService (async, DB)    │  │
│  └──────────────────────────┬─────────────────────────────── ┘  │
│                             │ aggregated ZoneEvents channel      │
│                             │ (control events only, not snapshots│
│  ┌──────────────────────────▼─────────────────────────────────┐  │
│  │              Zone Threads (one per zone, fixed at startup)  │  │
│  │                                                             │  │
│  │  Zone: Elwynn Forest          Zone: Stormwind City          │  │
│  │  ┌──────────────────┐        ┌──────────────────────────┐  │  │
│  │  │ hecs::World      │        │ hecs::World              │  │  │
│  │  │ SpatialIndex     │        │ SpatialIndex             │  │  │
│  │  │ connections:     │        │ connections:             │  │  │
│  │  │  id→Sender<WS>   │        │  id→Sender<WS>           │  │  │
│  │  │                  │        │                          │  │  │
│  │  │ tick():          │        │ tick():                  │  │  │
│  │  │  simulate        │        │  simulate                │  │  │
│  │  │  build AOI/player│        │  build AOI/player        │  │  │
│  │  │  send snapshots  │        │  send snapshots ─────────┼──┼──► Connection Tasks
│  │  └──────────────────┘        └──────────────────────────┘  │  │
│  │                                                             │  │
│  │  Zone: Westfall               Zone: The Barrens             │  │
│  │  ... (all zones fixed, loaded from config at startup) ...   │  │
│  └─────────────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────────────┘
```

---

## World / Zone Model

**1 server process = 1 world.** Multiple realms = multiple processes.

### No instanced zones
All zones are open world. Dungeons and raids are persistent open zones — multiple groups can
be present simultaneously, old-school style (Ultima Online, early EverQuest). No private
instances, no dynamic zone spawning. The set of zones is fixed at server startup.

### Hierarchy
```
World (coordinator, not a simulator)
└── Zone (unit of simulation — owns ALL state within it)
    ├── hecs::World  (entities + components)
    ├── SpatialIndex (grid for AOI queries)
    ├── SpawnTable
    ├── connections: HashMap<EntityId, Sender<WorldSnapshot>>
    └── PendingTransfers
```

### Zone threads
Each zone runs on a **dedicated OS thread** with its own tick loop. Zones are fixed at startup —
no dynamic spawning. Total thread count is known and bounded: one thread per zone.
Zones never share memory. They communicate exclusively via channels.

```rust
fn zone_loop(mut zone: Zone, inbox: Receiver<ZoneCommand>, outbox: Sender<ZoneEvent>) {
    let tick_rate = Duration::from_millis(50); // 20 ticks/sec
    loop {
        let tick_start = Instant::now();
        while let Ok(cmd) = inbox.try_recv() { zone.apply(cmd); }
        zone.tick();  // builds + dispatches snapshots directly to connection tasks
        // emit non-snapshot ZoneEvents (transfers, telemetry) to outbox
        thread::sleep(tick_rate.saturating_sub(tick_start.elapsed()));
    }
}
```

### Tick stages (explicit, sequential — no scheduler)
```
1. Drain inbox        — apply client movement, spell casts, interactions
2. Run AI             — mob decisions, pathfinding steps
3. Resolve combat     — damage, healing, auras, threat
4. Tick timers        — cooldowns, regen, aura durations, respawns
5. Dispatch snapshots — build WorldSnapshot per player (AOI), send direct to connection task
6. Emit ZoneEvents    — zone transfers, telemetry, deferred commands → outbox
```

### Zone has NO shared state
The zone struct is the single owner of all simulation data. No Arc, no Mutex, no RwLock inside
the zone. The only synchronization is at the channel boundary.

### AOI and snapshot dispatch
The zone owns the spatial index and knows every player's position. It builds each player's
WorldSnapshot and sends it directly to that player's connection task via a stored channel sender.
No coordinator or gateway is in this hot path.

When a player enters a zone:
```
ZoneCommand::PlayerEnter { entity_id, state, snapshot_tx: mpsc::Sender<WorldSnapshot> }
```
Zone stores `snapshot_tx` in `connections`. Sends to it every tick. Removes it on
`ZoneCommand::PlayerLeave`.

### Cross-zone communication (zone transfers)
Zones never talk to each other directly. The WorldCoordinator mediates:

```
Zone A tick → emits ZoneTransfer { entity, state, dest: ZoneB }
WorldCoordinator receives it → sends ZoneCommand::PlayerEnter to Zone B inbox
Zone B picks it up next tick
```

---

## Tick Inbox Classification

The Gateway classifies packets at receipt and routes them — the game tick never sees deferred packets:

```
Packet received by Gateway
    │
    ├── IMMEDIATE (this tick) ──► zone inbox (ZoneCommand)
    │     Movement, spell cast, attack, interact
    │
    └── DEFERRED (async service) ──► service channel
          Chat, mail, AH bids/posts, guild ops
```

Async services (Chat, AH, Mail) never touch zone state directly. If a service needs to affect
the game world (e.g. AH auction won → deliver item), it writes a ZoneCommand back to the
appropriate zone inbox for the next tick.

---

## Gateway Pattern

The game tick never touches sockets. Channels are the only bridge:

```
[Zone threads] ──ZoneEvents──► [aggregated channel] ──► [WorldCoordinator]
                                                               │
                                                        ──► [Gateway]
                                                               │
                                                        AOI filter
                                                        Serialize packets
                                                        Send to clients
[Clients] ──packets──► [Gateway] ──ZoneCommands──► [zone inboxes]
```

Gateway owns: connection state, session→entity mapping, AOI sets per player.
Gateway does NOT own: any game world state.

The Gateway is a **single Tokio task** that owns its connection map exclusively — no locking
needed. New connections and disconnects arrive via channels, so the map is never accessed
from outside the task.

---

## Wire Protocol

### Philosophy
- **Snapshot-based, not delta-based.** Every tick the server sends each client a full
  `WorldSnapshot` containing the complete state of every entity in their AOI. The client
  renders exactly what it receives. No client-side state tracking, no desync on packet loss.
- **Input-based movement.** Clients send direction + speed. Server computes authoritative
  positions. Harder to cheat, correct from day one.
- **TCP for control, UDP for world state.** Reliable ordered delivery for auth and control
  messages. Fire-and-forget for WorldSnapshot — a dropped packet is fine, the next one
  has everything.

### Packet Header (all messages)
```
version:     u8    — protocol version for future evolution
msg_type:    u16   — discriminant identifying the message
sequence:    u32   — for ordering/dedup, especially UDP
payload_len: u32   — byte length of the payload that follows
```

### Client → Server (Phase 1)
| Message | Fields | Transport | Routes to |
|---|---|---|---|
| `Handshake` | `token: String` | TCP | Gateway (validates, then zone inbox) |
| `Ping` | `client_timestamp: u64` | TCP | Gateway (never hits game loop) |
| `MoveInput` | `direction: Vec2, speed: f32` | UDP | Zone inbox (immediate) |
| `Disconnect` | — | TCP | Gateway |

### Server → Client (Phase 1)
| Message | Fields | Transport | Trigger |
|---|---|---|---|
| `HandshakeAccepted` | `entity_id: EntityId, zone_id: ZoneId` | TCP | Valid token |
| `HandshakeRejected` | `reason: RejectReason` | TCP | Bad/expired token |
| `Pong` | `client_ts: u64, server_ts: u64` | TCP | Reply to Ping |
| `WorldSnapshot` | `tick: u64, entities: Vec<EntityState>` | UDP | Every tick, per client |
| `ZoneTransfer` | `zone_id: ZoneId, position: Vec3` | TCP | Player crosses zone boundary |

### EntityState (everything the client needs to render an entity)
```rust
pub struct EntityState {
    pub entity_id: EntityId,
    pub kind:      EntityKind,   // Player, Mob, NPC, GameObject
    pub position:  Vec3,
    pub facing:    f32,
    pub health:    u32,
    pub max_health: u32,
    pub flags:     EntityFlags,  // in_combat, moving, casting, dead, etc.
}
```

### Snapshot semantics
- Entities present in snapshot → render them
- Entities absent from snapshot → they left AOI, stop rendering (implicit despawn)
- Entities new in snapshot → they entered AOI (implicit spawn)
- No separate spawn/despawn/move messages for world state

### Future message types (later phases)
```
Phase 2: CharacterList, SelectCharacter, CreateCharacter, DeleteCharacter
Phase 3: ChatMessage (zone, guild, whisper, say)
Phase 4: SpellCast, SpellInterrupt, AuraApplied, AuraRemoved,
         DamageEvent, HealEvent, EntityDied, ThreatUpdate
Phase 5: InventoryUpdate, ItemLooted, QuestAccepted, QuestCompleted,
         AHListing, AHBid, MailReceived
```
New messages add a variant to `ClientMessage` or `ServerMessage` enums and a routing rule
in the Gateway. The u16 msg_type space is partitioned by phase to avoid collisions.

### Top-level enums in `protocol` crate
```rust
pub enum ClientMessage {
    Handshake(Handshake),
    Ping(Ping),
    MoveInput(MoveInput),
    Disconnect,
}

pub enum ServerMessage {
    HandshakeAccepted(HandshakeAccepted),
    HandshakeRejected(HandshakeRejected),
    Pong(Pong),
    WorldSnapshot(WorldSnapshot),
    ZoneTransfer(ZoneTransfer),
}
```

---

## Async Services

| Service | Handles | Game world interaction |
|---|---|---|
| ChatService | Routing by channel type (local, zone, guild, whisper) | None — gateway AOI handles local |
| AuctionService | Listings, bids, expiry | Writes DeliverItem to zone inbox on expiry |
| MailService | Send, receive, attachments | Writes DeliverItem to zone inbox on receipt |

---

## Workspace Structure

```
stelline/
├── Cargo.toml              # workspace root
└── crates/
    ├── login-server/        # binary: login, JWT, realm list
    ├── game-server/        # binary: composition root — wires Tokio runtime,
    │                       #   spawns zone threads, owns CancellationToken
    │
    ├── protocol/           # lib: wire message types, packet classification,
    │                       #   versioned header. No domain logic. Minimal deps.
    ├── common/             # lib: ONLY primitive value types — EntityId, Vec3,
    │                       #   ZoneId, Timestamp. No business logic.
    │
    ├── gateway/            # lib: async — TCP/UDP connections, packet routing,
    │                       #   AOI filtering, session management
    ├── services/           # lib: async — ChatService, AuctionService, MailService
    │                       #   All DB-backed via sqlx. No zone state access.
    ├── admin/              # lib: async — axum HTTP + WebSocket server, dashboard UI
    │                       #   Reads AdminSnapshot via watch::Receiver. No game deps.
    │
    ├── world/              # lib: sync — Zone (hecs simulation), WorldCoordinator
    │                       #   (async routing). Defines port traits for persistence.
    │                       #   Zero Tokio dependency in Zone itself.
    └── game-data/          # lib: sync — loads spell/item/creature data from files.
                            #   Implements data port traits defined in world/.
```

### Dependency graph (no circular deps)
```
login-server   ──► common, protocol
game-server   ──► world, gateway, services, admin, game-data, common, protocol
gateway       ──► protocol, common
services      ──► common, protocol  (+ sqlx, redis)
admin         ──► common  (AdminSnapshot only; axum + tokio)
world         ──► common, game-data (Zone is sync, no Tokio)
              WorldCoordinator ──► common (+ Tokio for async routing)
game-data     ──► common
protocol      ──► (serde, rmp-serde only)
common        ──► (minimal — no game deps)
```

### Key dependency rules
- `world::Zone` has NO Tokio dependency — pure sync, fully testable without a runtime
- `world::WorldCoordinator` is async (Tokio) — routing only, no simulation
- Domain crates (`world`, `game-data`) define port **traits** for persistence
- Only `game-server` (composition root) wires concrete adapters to those traits

---

## Async Patterns

### Graceful shutdown
`CancellationToken` from `tokio_util` wired in from day one. Propagates to:
- Gateway (drain connections)
- WorldCoordinator (signal zone threads)
- Zone threads (finish current tick, persist state)
- DB pools (flush pending writes)

### Lock discipline
- Zone owns all state — no locks needed inside tick loop
- Gateway is a single Tokio task that owns its connection map — no locks needed
- Prefer channels over shared state everywhere
- See Shared Memory Policy — any lock requires approval

### Async traits (port definitions)
```rust
#[async_trait]
pub trait CharacterRepository: Send + Sync {
    async fn load(&self, id: CharacterId) -> Result<Character>;
    async fn save(&self, character: &Character) -> Result<()>;
}
```

---

## Build Phases

### Phase 1 — Foundation
- [x] Cargo workspace scaffold (all crates, dependency graph)
- [x] `common`: EntityId, Vec3, ZoneId, AdminSnapshot primitives
- [x] `protocol`: packet header (versioned), core message types, MessagePack serialization
- [x] `world::Zone`: hecs world, tick loop, inbox drain, basic movement, telemetry events
- [x] `gateway`: TCP listener, session state machine, channel bridge
- [x] `admin`: axum server, WebSocket push, embedded dashboard HTML/JS
- [x] `game-server`: spawn zone thread, wire Gateway ↔ Zone ↔ Admin, CancellationToken
- [x] On startup: print admin URL to stdout
- [x] End-to-end: connect, move, see position update + live stats in dashboard

### Phase 2 — Auth & Characters
- [x] `login-server`: account DB schema, bcrypt, JWT issuance, realm list
- [x] Game server JWT validation + Redis blocklist on connect
- [x] Character create/select/delete (DB-backed + in-memory for dev)
- [x] Three-phase connection protocol: Handshake → Character Selection → Connected
- [x] Character CRUD HTTP endpoints on login-server
- [x] `dotenvy` for `.env` file loading (no manual env var exports needed)

### Phase 3 — World
- [x] Creature definitions (CreatureDef) + spawn tables (SpawnPoint) in `game-data`
- [x] Creature/Ai/SpawnInfo ECS components in `world`
- [x] Creature spawning: spawn_creature, spawn_initial_creatures
- [x] AI tick stage: wander behavior (random walk within radius), idle NPCs
- [x] Respawn infrastructure: kill_creature → PendingRespawn → step_timers → respawn
- [x] Movement for all entities (not just players)
- [x] build_snapshot: correct EntityKind for Mob vs Npc, names in snapshots
- [x] Zone configuration with creature data wired in game-server
- [x] TOML config file (`data/world.toml`) with creatures and spawn points
- [x] Validation: duplicate creature IDs, unknown refs, invalid values
- Note: single-zone for now, multi-zone with cross-zone protocol deferred

### Phase 4 — Combat & AI
- [ ] Spell system: data, casting pipeline, interrupts, cooldowns
- [ ] Aura system: buffs/debuffs, stacking, duration
- [ ] Combat formulas: hit/miss/crit/dodge/parry
- [ ] Threat system: per-mob aggro tables
- [ ] NPC AI: advanced state machines (patrol, combat)
- [ ] Pathfinding: navmesh integration

### Phase 5 — Game Systems
- [ ] `services`: ChatService, AuctionService, MailService
- [ ] Inventory & items: bags, bank, equipment, stat recalc
- [ ] Loot tables & distribution
- [ ] Quest system: objectives, completion, rewards
- [ ] Party/guild/social systems

### Phase 6 — Production
- [ ] Structured logging (tracing) with zone/entity context
- [ ] Metrics (Prometheus-compatible)
- [ ] GM/admin commands
- [ ] Load testing & performance tuning

---

## Shared Memory Policy
`Arc`, `Mutex`, `RwLock`, and any other shared-memory primitives are treated as **code smells**.
Before introducing any of these, Claude must stop, explain exactly why it is needed, and get
explicit approval. The default answer is: redesign to use channels instead.

There are currently no approved exceptions. The admin server uses `watch::Receiver<AdminSnapshot>`
as axum `State` — `watch::Receiver` is `Clone + Send + Sync` so no `Arc<Mutex<_>>` is needed.
Any future exception must be explained, approved, and documented here.

---

## Admin Dashboard

When the game server starts, it also starts a lightweight HTTP server on a configurable port
and prints the URL to stdout:

```
Admin dashboard: http://localhost:9000/admin
```

### Tech
- **axum** (Tokio-native) for the HTTP + WebSocket server
- **watch channel** for stats: WorldCoordinator writes a snapshot, admin handlers read it
- Static HTML/JS served from memory (embedded via `include_str!` or `rust-embed`)
- Browser connects via WebSocket for real-time push — no polling

### Stats collected per zone (emitted as telemetry events alongside game events)
- Actual tick rate (ticks/sec) vs target
- Tick duration (min/avg/max over last N ticks)
- Entity count (players, mobs, objects)
- Commands processed per tick
- Outbound events per tick
- Zone transfer count

### Dashboard panels
| Panel | Content |
|---|---|
| Server | Uptime, connected players total, world event log |
| Zones | Per-zone table: tick rate, tick duration, player count, entity count |
| Command feed | Rolling log of recent incoming commands (type, zone, timestamp) |
| Throughput | Commands/sec and events/sec charts (last 60s) |
| Services | Chat messages/sec, AH operations, mail deliveries |
| Tick health | Tick duration sparkline per zone — highlights overrun ticks in red |

### Data flow (no shared state)
```
Zone threads ──telemetry events──► aggregated channel ──► WorldCoordinator
                                                               │
                                                    ──► watch::Sender<AdminSnapshot>
                                                               │
Admin axum handlers ──watch::Receiver<AdminSnapshot>──► serve snapshot via WS
```

The `AdminSnapshot` is a plain struct (Clone, Serialize). The watch channel holds the latest
snapshot. Axum handlers clone the receiver — no Arc, no Mutex.

### Crate
Lives in `crates/admin/` — a lib crate with an `axum::Router` factory function.
`game-server` composes it in alongside the Gateway. Has no dependency on zone internals,
only on `AdminSnapshot` (defined in `common` or `admin` itself).

---

## Key Design Principles
- Server is authoritative — clients are untrusted
- Auth and game are separate processes
- Zone is the unit of simulation — one thread per zone, no shared state
- Game tick and network I/O are fully decoupled via channels (Gateway pattern)
- WorldCoordinator is a router, not a simulator
- Deferred actions (chat, AH, mail) are async services — never touch zone state directly
- Async services feed results back to zone inboxes for next-tick application
- All port traits defined in domain crates — adapters wired only at composition root
- Arc/Mutex/RwLock are code smells — use channels; get approval before introducing any
