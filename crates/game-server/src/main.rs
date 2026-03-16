use common::AdminSnapshot;
use gateway::{GatewayAuth, GatewayHandle, UdpPacket};
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

    // Load .env file if present (silently skip if missing).
    dotenvy::dotenv().ok();

    let start_time = std::time::Instant::now();

    // 2. Load world configuration.
    let world_config = game_data::default_world_config();
    game_data::validate_world_config(&world_config)?;
    info!(zones = world_config.zones.len(), "world config loaded");

    // 3. Auth configuration from environment.
    let jwt_secret = std::env::var("JWT_SECRET").unwrap_or_else(|_| {
        tracing::warn!("JWT_SECRET not set — using insecure default");
        "dev-secret-change-me".to_string()
    });
    let redis_url =
        std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
    let redis_client = redis::Client::open(redis_url.as_str()).expect("invalid REDIS_URL");
    let skip = std::env::var("JWT_SKIP_VALIDATION").is_ok();
    if skip {
        info!("JWT_SKIP_VALIDATION is set — accepting any token without verification");
    }
    let auth = GatewayAuth {
        jwt_secret: jwt_secret.into_bytes(),
        redis_client,
        skip_validation: skip,
    };

    // 4. Admin watch channel.
    let (admin_tx, admin_rx) = watch::channel(AdminSnapshot::default());

    // 4b. Optional Postgres pool for admin dashboard (accounts viewer).
    let db_pool = match std::env::var("DATABASE_URL") {
        Ok(url) => {
            match sqlx::PgPool::connect(&url).await {
                Ok(pool) => {
                    info!("admin DB connected — accounts viewer enabled");
                    Some(pool)
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not connect to DATABASE_URL — accounts viewer disabled");
                    None
                }
            }
        }
        Err(_) => {
            info!("DATABASE_URL not set — accounts viewer disabled");
            None
        }
    };

    // 5. Start admin server on port 9000.
    admin::start_admin_server(9000, admin_rx, db_pool.clone()).await?;

    // 6. Gateway handle (channels for routing client messages).
    let handle = GatewayHandle::new();
    let router = handle.router.clone();
    let GatewayHandle {
        immediate_rx,
        deferred_rx: _deferred_rx, // Phase 1: no deferred services yet
        ..
    } = handle;

    // 7. UDP socket + dispatch task.
    let udp_socket = tokio::net::UdpSocket::bind("0.0.0.0:7879").await?;
    info!("UDP listening on 0.0.0.0:7879");
    let (udp_tx, udp_rx) = mpsc::channel::<UdpPacket>(1024);
    tokio::spawn(gateway::run_udp_dispatch(udp_socket, udp_rx));

    // 8. Zone threads — one per zone in the world config.
    let (zone_inboxes, zone_event_rx) = spawn_zone_threads(&world_config)?;
    info!(zones = zone_inboxes.len(), "zone threads spawned");

    // 9. Coordinator task.
    tokio::spawn(run_coordinator(
        zone_event_rx,
        zone_inboxes.clone(),
        admin_tx,
        start_time,
    ));

    // 10. Character store — DbCharacterStore if DATABASE_URL is set, else InMemory.
    let character_store: std::sync::Arc<dyn gateway::character_store::CharacterStore> =
        if let Some(ref pool) = db_pool {
            info!("using DbCharacterStore (Postgres-backed)");
            std::sync::Arc::new(gateway::character_store::DbCharacterStore::new(pool.clone()))
        } else {
            info!("using InMemoryCharacterStore (no DATABASE_URL)");
            std::sync::Arc::new(gateway::character_store::InMemoryCharacterStore::new())
        };

    // 11. TCP listener.
    let tcp_listener = tokio::net::TcpListener::bind("0.0.0.0:7878").await?;
    info!("TCP listening on 0.0.0.0:7878");
    tokio::spawn(run_tcp_listener(
        tcp_listener,
        router,
        udp_tx,
        zone_inboxes.clone(),
        auth,
        character_store,
    ));

    // Immediate command router: bridges gateway → zone inbox.
    tokio::spawn(run_immediate_router(immediate_rx, zone_inboxes));

    // 11. Wait for Ctrl+C.
    tokio::signal::ctrl_c().await?;
    info!("Shutting down...");

    Ok(())
}
