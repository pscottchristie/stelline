use common::AdminSnapshot;
use gateway::{GatewayHandle, UdpPacket};
use tokio::sync::{mpsc, watch};
use tracing::info;

use game_server::{
    run_coordinator, run_immediate_router, run_tcp_listener, spawn_zone_threads,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Initialise structured logging.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let start_time = std::time::Instant::now();

    // 2. Load world configuration.
    let world_config = game_data::default_world_config();
    game_data::validate_world_config(&world_config)?;
    info!(zones = world_config.zones.len(), "world config loaded");

    // 3. Admin watch channel.
    let (admin_tx, admin_rx) = watch::channel(AdminSnapshot::default());

    // 4. Start admin server on port 9000.
    admin::start_admin_server(9000, admin_rx).await?;

    // 5. Gateway handle (channels for routing client messages).
    let handle = GatewayHandle::new();
    let router = handle.router.clone();
    let GatewayHandle {
        immediate_rx,
        deferred_rx: _deferred_rx, // Phase 1: no deferred services yet
        ..
    } = handle;

    // 6. UDP socket + dispatch task.
    let udp_socket = tokio::net::UdpSocket::bind("0.0.0.0:7879").await?;
    info!("UDP listening on 0.0.0.0:7879");
    let (udp_tx, udp_rx) = mpsc::channel::<UdpPacket>(1024);
    tokio::spawn(gateway::run_udp_dispatch(udp_socket, udp_rx));

    // 7. Zone threads — one per zone in the world config.
    let (zone_inboxes, zone_event_rx) = spawn_zone_threads(&world_config)?;
    info!(zones = zone_inboxes.len(), "zone threads spawned");

    // 8. Coordinator task.
    tokio::spawn(run_coordinator(
        zone_event_rx,
        zone_inboxes.clone(),
        admin_tx,
        start_time,
    ));

    // 9. TCP listener.
    let tcp_listener = tokio::net::TcpListener::bind("0.0.0.0:7878").await?;
    info!("TCP listening on 0.0.0.0:7878");
    tokio::spawn(run_tcp_listener(
        tcp_listener,
        router,
        udp_tx,
        zone_inboxes.clone(),
    ));

    // Immediate command router: bridges gateway → zone inbox.
    tokio::spawn(run_immediate_router(immediate_rx, zone_inboxes));

    // 10. Wait for Ctrl+C.
    tokio::signal::ctrl_c().await?;
    info!("Shutting down...");

    Ok(())
}
