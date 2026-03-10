//! # game-server — testable library surface
//!
//! `game-server` is primarily a binary crate.  This `lib.rs` re-exposes the
//! components that integration tests need:
//!
//! - `make_snapshot_bridge`  — bridge between sync zone channels and async conn.
//! - `run_coordinator`       — processes `ZoneEvent`s, updates `AdminSnapshot`.
//! - `run_tcp_listener`      — accepts TCP connections and spawns tasks.
//! - `run_immediate_router`  — forwards gateway commands to zone inboxes.

use std::collections::HashMap;
use std::sync::mpsc as std_mpsc;

use common::{AdminSnapshot, EntityId, Vec3, ZoneId};
use gateway::{ConnectionConfig, UdpPacket};
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};
use world::{Zone, ZoneCommand, ZoneEvent};

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

    // Bridge thread: blocks on the std sync receiver and pushes into the tokio
    // sender.  Named threads aid debugging in profilers and panic traces.
    std::thread::Builder::new()
        .name("snapshot-bridge".to_string())
        .spawn(move || {
            loop {
                match std_rx.recv() {
                    Ok(snapshot) => {
                        if tok_tx.blocking_send(snapshot).is_err() {
                            // Connection task has gone away.
                            break;
                        }
                    }
                    Err(_disconnected) => {
                        // Zone dropped the SyncSender → this connection is done.
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
                        };
                        let _ = admin_tx.send(snap);
                    }

                    ZoneEvent::ZoneTransfer {
                        entity_id,
                        destination,
                        position,
                    } => {
                        if let Some(inbox) = zone_inboxes.get(&destination) {
                            let (snap_tx, _snap_rx) = make_snapshot_bridge(32);
                            let cmd = ZoneCommand::PlayerEnter {
                                entity_id,
                                position,
                                snapshot_tx: snap_tx,
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
pub async fn run_tcp_listener(
    listener: tokio::net::TcpListener,
    router: gateway::ClientRouter,
    udp_tx: mpsc::Sender<UdpPacket>,
    zone_inboxes: HashMap<ZoneId, std_mpsc::SyncSender<ZoneCommand>>,
) {
    let default_zone_id = ZoneId::new(1);
    let default_entity_id = EntityId::new(1);

    loop {
        let (stream, peer_addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                error!(error = %e, "TCP accept error");
                continue;
            }
        };

        info!(addr = %peer_addr, "new TCP connection");

        let (snap_tx, snap_rx) = make_snapshot_bridge(32);

        if let Some(inbox) = zone_inboxes.get(&default_zone_id) {
            let cmd = ZoneCommand::PlayerEnter {
                entity_id: default_entity_id,
                position: Vec3::ZERO,
                snapshot_tx: snap_tx,
            };
            if inbox.send(cmd).is_err() {
                warn!(addr = %peer_addr, "zone 1 inbox closed; rejecting connection");
                continue;
            }
        } else {
            warn!(addr = %peer_addr, "no zone 1 found; cannot register player");
            continue;
        }

        let (tcp_read, tcp_write) = stream.into_split();
        let config = ConnectionConfig {
            peer_addr,
            udp_tx: udp_tx.clone(),
            router: router.clone(),
            snapshot_rx: snap_rx,
        };

        tokio::spawn(gateway::run_connection(tcp_read, tcp_write, config));
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
///
/// The `zone_event_tx` original sender is dropped before returning so the
/// channel closes when all zone threads exit.
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
        let zone = Zone::new(zone_id, &zone_config.name);
        let outbox_tx = zone_event_tx.clone();

        std::thread::Builder::new()
            .name(format!("zone-{}", zone_config.name))
            .spawn(move || world::run_zone_loop(zone, inbox_rx, outbox_tx))?;

        zone_inboxes.insert(zone_id, inbox_tx);
    }
    drop(zone_event_tx);

    Ok((zone_inboxes, zone_event_rx))
}
