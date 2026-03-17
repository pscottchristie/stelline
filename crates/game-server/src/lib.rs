//! # game-server — testable library surface
//!
//! `game-server` is primarily a binary crate.  This `lib.rs` re-exposes the
//! components that integration tests need:
//!
//! - `make_snapshot_bridge`  — bridge between sync zone channels and async conn.
//! - `run_coordinator`       — processes `ZoneEvent`s, updates `AdminSnapshot`.
//! - `run_tcp_listener`      — accepts TCP connections and spawns tasks.
//! - `run_immediate_router`  — forwards gateway commands to zone inboxes.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use common::{AdminSnapshot, EntityId, ZoneEntitySnapshot, ZoneId};
use gateway::{ConnectionConfig, EnterZoneRequest, GatewayAuth, UdpPacket};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{error, info, warn};
use world::{ZoneCommand, ZoneEvent};

// ─────────────────────────────────────────────────────────────────────────────
// Connection registry — tracks which accounts are currently connected
// ─────────────────────────────────────────────────────────────────────────────

enum RegistryMsg {
    TryRegister {
        account_uuid: String,
        reply: oneshot::Sender<bool>,
    },
    Unregister {
        account_uuid: String,
    },
}

/// Spawn a task that tracks connected account UUIDs.
/// Returns the sender for communicating with it.
fn spawn_connection_registry() -> mpsc::UnboundedSender<RegistryMsg> {
    let (tx, mut rx) = mpsc::unbounded_channel::<RegistryMsg>();
    tokio::spawn(async move {
        let mut connected: HashSet<String> = HashSet::new();
        while let Some(msg) = rx.recv().await {
            match msg {
                RegistryMsg::TryRegister { account_uuid, reply } => {
                    let ok = connected.insert(account_uuid);
                    let _ = reply.send(ok);
                }
                RegistryMsg::Unregister { account_uuid } => {
                    connected.remove(&account_uuid);
                }
            }
        }
    });
    tx
}

// ─────────────────────────────────────────────────────────────────────────────
// Snapshot channel bridge
// ─────────────────────────────────────────────────────────────────────────────

/// Create a channel pair that bridges the sync zone world and the async
/// connection task.
///
/// The zone writes [`protocol::WorldSnapshot`] values through a
/// [`std::sync::mpsc::SyncSender`] (no async runtime required). The connection
/// task reads from a [`tokio::sync::mpsc::Receiver`].  A dedicated OS thread
/// sits between them, blocking on the std receiver and forwarding items into
/// the tokio sender via `blocking_send`.
///
/// Returns `(sync_tx, tokio_rx)`.
pub fn make_snapshot_bridge(
    capacity: usize,
) -> (
    std_mpsc::SyncSender<protocol::WorldSnapshot>,
    mpsc::Receiver<protocol::WorldSnapshot>,
) {
    let (std_tx, std_rx) = std_mpsc::sync_channel::<protocol::WorldSnapshot>(capacity);
    let (tok_tx, tok_rx) = mpsc::channel::<protocol::WorldSnapshot>(capacity);

    std::thread::Builder::new()
        .name("snapshot-bridge".to_string())
        .spawn(move || {
            loop {
                match std_rx.recv() {
                    Ok(snapshot) => {
                        if tok_tx.blocking_send(snapshot).is_err() {
                            break;
                        }
                    }
                    Err(_disconnected) => {
                        break;
                    }
                }
            }
        })
        .expect("failed to spawn snapshot-bridge thread");

    (std_tx, tok_rx)
}

// ─────────────────────────────────────────────────────────────────────────────
// Coordinator
// ─────────────────────────────────────────────────────────────────────────────

/// Reads `ZoneEvent`s from the aggregated zone outbox and:
/// - On `TelemetryUpdate` → rebuilds and publishes `AdminSnapshot`.
/// - On `ZoneTransfer`    → routes `PlayerEnter` to the destination zone.
pub async fn run_coordinator(
    zone_event_rx: std_mpsc::Receiver<ZoneEvent>,
    zone_inboxes: HashMap<ZoneId, std_mpsc::SyncSender<ZoneCommand>>,
    admin_tx: watch::Sender<AdminSnapshot>,
    start_time: std::time::Instant,
) {
    let mut zone_telemetry: HashMap<ZoneId, common::ZoneSnapshot> = HashMap::new();
    let mut zone_tick_counts: HashMap<ZoneId, (u64, std::time::Instant)> = HashMap::new();
    let mut zone_entity_snapshots: HashMap<ZoneId, ZoneEntitySnapshot> = HashMap::new();
    let poll_interval = tokio::time::Duration::from_millis(5);

    loop {
        loop {
            match zone_event_rx.try_recv() {
                Ok(event) => match event {
                    ZoneEvent::TelemetryUpdate(t) => {
                        let tick_rate = {
                            let entry = zone_tick_counts
                                .entry(t.zone_id)
                                .or_insert_with(|| (t.tick, std::time::Instant::now()));
                            let elapsed = entry.1.elapsed().as_secs_f32();
                            let ticks = t.tick.saturating_sub(entry.0) as f32;
                            if elapsed > 0.5 {
                                let rate = ticks / elapsed;
                                *entry = (t.tick, std::time::Instant::now());
                                rate
                            } else {
                                zone_telemetry
                                    .get(&t.zone_id)
                                    .map(|z| z.tick_rate)
                                    .unwrap_or(20.0)
                            }
                        };

                        let zs = zone_telemetry.entry(t.zone_id).or_insert_with(|| {
                            common::ZoneSnapshot {
                                zone_id: t.zone_id,
                                name: format!("Zone {}", t.zone_id.get()),
                                player_count: 0,
                                entity_count: 0,
                                tick_rate: 20.0,
                                tick_duration_ms_avg: 0.0,
                                tick_duration_ms_max: 0.0,
                            }
                        });

                        let alpha = 0.1_f32;
                        zs.tick_duration_ms_avg =
                            alpha * t.tick_duration_ms + (1.0 - alpha) * zs.tick_duration_ms_avg;
                        zs.tick_duration_ms_max =
                            zs.tick_duration_ms_max.max(t.tick_duration_ms);
                        zs.player_count = t.player_count;
                        zs.entity_count = t.entity_count;
                        zs.tick_rate = tick_rate;

                        let connected_players: u32 =
                            zone_telemetry.values().map(|z| z.player_count).sum();
                        let snap = AdminSnapshot {
                            uptime_secs: start_time.elapsed().as_secs(),
                            connected_players,
                            zones: zone_telemetry.values().cloned().collect(),
                            entity_snapshots: zone_entity_snapshots.values().cloned().collect(),
                        };
                        let _ = admin_tx.send(snap);
                    }

                    ZoneEvent::EntitySnapshot(es) => {
                        zone_entity_snapshots.insert(es.zone_id, es);
                    }

                    ZoneEvent::ZoneTransfer {
                        entity_id,
                        destination,
                        position,
                        label,
                    } => {
                        if let Some(inbox) = zone_inboxes.get(&destination) {
                            let (snap_tx, _snap_rx) = make_snapshot_bridge(32);
                            let cmd = ZoneCommand::PlayerEnter {
                                entity_id,
                                position,
                                snapshot_tx: snap_tx,
                                label,
                            };
                            if inbox.send(cmd).is_err() {
                                warn!(
                                    entity = ?entity_id,
                                    dest = ?destination,
                                    "zone transfer failed: destination inbox closed"
                                );
                            }
                        } else {
                            warn!(
                                entity = ?entity_id,
                                dest = ?destination,
                                "zone transfer to unknown zone"
                            );
                        }
                    }
                },

                Err(std_mpsc::TryRecvError::Empty) => break,
                Err(std_mpsc::TryRecvError::Disconnected) => {
                    info!("zone event channel closed; coordinator exiting");
                    return;
                }
            }
        }

        tokio::time::sleep(poll_interval).await;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// TCP listener
// ─────────────────────────────────────────────────────────────────────────────

/// Accept TCP connections and spawn a `run_connection` task for each one.
///
/// Each connection gets a unique, monotonically increasing [`EntityId`].
/// After the connection task validates the JWT, a post-handshake wiring task:
/// 1. Checks the connection registry for duplicate logins (sends auth result).
/// 2. Waits for the connection task to select a character (receives EnterZoneRequest).
/// 3. Sends `PlayerEnter` to the correct zone inbox and delivers the snapshot
///    channel to the connection task via a oneshot.
pub async fn run_tcp_listener(
    listener: tokio::net::TcpListener,
    router: gateway::ClientRouter,
    udp_tx: mpsc::Sender<UdpPacket>,
    zone_inboxes: HashMap<ZoneId, std_mpsc::SyncSender<ZoneCommand>>,
    auth: GatewayAuth,
    character_store: std::sync::Arc<dyn gateway::character_store::CharacterStore>,
) {
    let mut next_entity_id: u64 = 1;
    let registry_tx = spawn_connection_registry();

    loop {
        let (stream, peer_addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                error!(error = %e, "TCP accept error");
                continue;
            }
        };

        info!(addr = %peer_addr, "new TCP connection");

        let entity_id = EntityId::new(next_entity_id);
        next_entity_id += 1;

        let (validated_tx, validated_rx) = oneshot::channel::<Option<String>>();
        let (auth_result_tx, auth_result_rx) =
            oneshot::channel::<Result<(), protocol::RejectReason>>();
        let (enter_zone_tx, mut enter_zone_rx) = mpsc::channel::<EnterZoneRequest>(1);
        let (snap_source_tx, snap_source_rx) =
            oneshot::channel::<Result<mpsc::Receiver<protocol::WorldSnapshot>, protocol::RejectReason>>();

        let (tcp_read, tcp_write) = stream.into_split();
        let config = ConnectionConfig {
            peer_addr,
            udp_tx: udp_tx.clone(),
            router: router.clone(),
            auth: auth.clone(),
            entity_id,
            character_store: character_store.clone(),
            validated_tx,
            auth_result_rx,
            enter_zone_tx,
            snapshot_rx_source: snap_source_rx,
        };

        let conn_handle = tokio::spawn(gateway::run_connection(tcp_read, tcp_write, config));

        // Post-handshake wiring task. Three phases:
        // 1. JWT validation + duplicate login check → auth_result_tx
        // 2. Wait for character selection → enter_zone_rx
        // 3. Wire zone snapshot channel → snap_source_tx
        let zone_inboxes = zone_inboxes.clone();
        let registry = registry_tx.clone();
        tokio::spawn(async move {
            // Phase 1: Wait for JWT validation result.
            let account_uuid = match tokio::time::timeout(Duration::from_secs(10), validated_rx).await {
                Ok(Ok(Some(uuid))) => uuid,
                _ => {
                    drop(snap_source_tx);
                    return;
                }
            };

            // Check if this account is already connected.
            let (reply_tx, reply_rx) = oneshot::channel();
            if registry.send(RegistryMsg::TryRegister {
                account_uuid: account_uuid.clone(),
                reply: reply_tx,
            }).is_err() {
                drop(snap_source_tx);
                return;
            }
            match reply_rx.await {
                Ok(true) => {
                    // Registration succeeded — tell connection task to proceed to character selection.
                    let _ = auth_result_tx.send(Ok(()));
                }
                _ => {
                    // Account already connected — reject.
                    warn!(account = %account_uuid, "duplicate login rejected");
                    let _ = auth_result_tx.send(Err(protocol::RejectReason::AlreadyConnected));
                    drop(snap_source_tx);
                    return;
                }
            }

            // Phase 2: Wait for character selection (EnterZoneRequest).
            let enter_req = match tokio::time::timeout(Duration::from_secs(60), enter_zone_rx.recv()).await {
                Ok(Some(req)) => req,
                _ => {
                    // Character selection timed out or connection dropped.
                    drop(snap_source_tx);
                    let _ = registry.send(RegistryMsg::Unregister { account_uuid });
                    return;
                }
            };

            // Phase 3: Wire up the zone snapshot channel.
            let (snap_tx, snap_rx) = make_snapshot_bridge(32);
            let cmd = ZoneCommand::PlayerEnter {
                entity_id,
                position: enter_req.position,
                snapshot_tx: snap_tx,
                label: enter_req.label,
            };
            if let Some(inbox) = zone_inboxes.get(&enter_req.zone_id) {
                let _ = inbox.send(cmd);
            } else {
                warn!(zone = ?enter_req.zone_id, "player selected unknown zone");
                let _ = snap_source_tx.send(Err(protocol::RejectReason::InvalidToken));
                let _ = registry.send(RegistryMsg::Unregister { account_uuid });
                return;
            }
            let _ = snap_source_tx.send(Ok(snap_rx));

            // Wait for connection to end, then unregister.
            let _ = conn_handle.await;
            let _ = registry.send(RegistryMsg::Unregister { account_uuid });
        });
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Immediate command router
// ─────────────────────────────────────────────────────────────────────────────

/// Read `RoutedCommand`s from the gateway's immediate channel and forward them
/// to the appropriate zone inbox as `ZoneCommand`s.
pub async fn run_immediate_router(
    mut immediate_rx: mpsc::UnboundedReceiver<gateway::RoutedCommand>,
    zone_inboxes: HashMap<ZoneId, std_mpsc::SyncSender<ZoneCommand>>,
) {
    let default_zone_id = ZoneId::new(1);

    while let Some(cmd) = immediate_rx.recv().await {
        let zone_cmd = match cmd.message {
            protocol::ClientMessage::MoveInput(mi) => ZoneCommand::MoveInput {
                entity_id: cmd.entity_id,
                direction: mi.direction,
                speed: mi.speed,
            },
            _ => continue,
        };

        if let Some(inbox) = zone_inboxes.get(&default_zone_id) {
            if inbox.send(zone_cmd).is_err() {
                warn!(entity = ?cmd.entity_id, "zone inbox closed while routing immediate command");
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers re-exported for tests
// ─────────────────────────────────────────────────────────────────────────────

/// Spawn all zone threads defined in `world_config` and return the per-zone
/// inbox senders and a single aggregated event receiver.
pub fn spawn_zone_threads(
    world_config: &game_data::WorldConfig,
) -> anyhow::Result<(
    HashMap<ZoneId, std_mpsc::SyncSender<ZoneCommand>>,
    std_mpsc::Receiver<ZoneEvent>,
)> {
    let (zone_event_tx, zone_event_rx) = std_mpsc::sync_channel::<ZoneEvent>(1024);
    let mut zone_inboxes: HashMap<ZoneId, std_mpsc::SyncSender<ZoneCommand>> = HashMap::new();

    for zone_config in &world_config.zones {
        let zone_id = ZoneId::new(zone_config.id);
        let (inbox_tx, inbox_rx) = std_mpsc::sync_channel::<ZoneCommand>(256);
        let mut zone = world::Zone::with_config(
            zone_id,
            &zone_config.name,
            zone_config.aoi_radius,
            zone_config.width,
            zone_config.height,
        );

        // Spawn initial creatures from config.
        zone.spawn_initial_creatures(&world_config.creatures, &zone_config.spawns);

        let outbox_tx = zone_event_tx.clone();

        std::thread::Builder::new()
            .name(format!("zone-{}", zone_config.name))
            .spawn(move || world::run_zone_loop(zone, inbox_rx, outbox_tx))?;

        zone_inboxes.insert(zone_id, inbox_tx);
    }
    drop(zone_event_tx);

    Ok((zone_inboxes, zone_event_rx))
}
