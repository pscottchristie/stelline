//! Manual test client for Stelline game server.
//!
//! Usage:
//!   cargo run -p test-client                      # connect, handshake, move in a loop
//!   cargo run -p test-client -- --token mytoken   # custom token
//!   cargo run -p test-client -- --host 1.2.3.4    # remote server
//!
//! What it does:
//!   1. Connects TCP to the game server
//!   2. Sends a Handshake
//!   3. Prints HandshakeAccepted (entity id, zone)
//!   4. Sends a Ping every 2 seconds and prints the Pong RTT
//!   5. Sends MoveInput (walking north) every 50ms
//!   6. Binds a UDP socket and listens for WorldSnapshot packets
//!   7. Prints a summary of each snapshot received (tick, entity count)

use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use common::Vec2;
use gateway::{read_tcp_message, write_tcp_message};
use protocol::{
    decode_server, encode_client, ClientMessage, Handshake, MoveInput, Ping,
    ServerMessage, HEADER_SIZE,
};
use tokio::io::BufReader;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

const FRAME_LEN_SIZE: usize = 4;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // ── Parse simple CLI args ─────────────────────────────────────────────────
    let args: Vec<String> = std::env::args().collect();
    let mut host = "127.0.0.1".to_string();
    let mut token = "test-client-token".to_string();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--host" => { host = args.get(i + 1).cloned().unwrap_or(host); i += 2; }
            "--token" => { token = args.get(i + 1).cloned().unwrap_or(token); i += 2; }
            _ => { i += 1; }
        }
    }

    let tcp_addr = format!("{host}:7878");
    let _udp_server_addr = format!("{host}:7879");

    info!("Connecting to {tcp_addr}...");

    // ── TCP connection ────────────────────────────────────────────────────────
    let stream = TcpStream::connect(&tcp_addr)
        .await
        .with_context(|| format!("failed to connect to {tcp_addr}"))?;

    let peer_addr: SocketAddr = stream.peer_addr()?;
    let local_addr: SocketAddr = stream.local_addr()?;
    info!(peer = %peer_addr, local = %local_addr, "TCP connected");

    let (tcp_read, mut tcp_write) = stream.into_split();
    let mut tcp_reader = BufReader::new(tcp_read);

    // ── UDP socket for receiving snapshots ────────────────────────────────────
    // Bind on the same IP as the TCP local addr so the server can reach us.
    let udp_bind = format!("{}:0", local_addr.ip());
    let udp_socket = UdpSocket::bind(&udp_bind).await?;
    let udp_local = udp_socket.local_addr()?;
    info!(udp_port = udp_local.port(), "UDP recv socket bound");

    // ── Handshake ─────────────────────────────────────────────────────────────
    let mut seq: u32 = 0;
    let hs = ClientMessage::Handshake(Handshake { token: token.clone() });
    let (h, p) = encode_client(&hs, seq)?;
    write_tcp_message(&mut tcp_write, &h, &p).await?;
    info!(token = %token, "→ Handshake sent");

    let (rh, rp) = read_tcp_message(&mut tcp_reader).await?;
    let reply = decode_server(&rh, &rp)?;
    match reply {
        ServerMessage::HandshakeAccepted(ref accepted) => {
            info!(
                entity_id = accepted.entity_id.get(),
                zone_id = accepted.zone_id.get(),
                "✓ HandshakeAccepted"
            );
        }
        ServerMessage::HandshakeRejected(ref rej) => {
            error!(reason = ?rej.reason, "✗ HandshakeRejected — check your token");
            return Ok(());
        }
        other => {
            error!(msg = ?other, "unexpected message during handshake");
            return Ok(());
        }
    }

    println!();
    println!("═══════════════════════════════════════════════════");
    println!("  Connected! Running test loop. Press Ctrl+C to quit.");
    println!("  → sending MoveInput (north) every 50ms");
    println!("  → sending Ping every 2s");
    println!("  → listening for WorldSnapshot on UDP :{}", udp_local.port());
    println!("  NOTE: Phase 1 server sends UDP to TCP peer addr port,");
    println!("  not the UDP port above. Snapshots visible in server logs.");
    println!("═══════════════════════════════════════════════════");
    println!();

    // ── Spawn UDP listener task ───────────────────────────────────────────────
    let (udp_tx, mut udp_rx) = mpsc::channel::<String>(32);
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        loop {
            match udp_socket.recv_from(&mut buf).await {
                Ok((n, from)) => {
                    // Parse framed snapshot: 4-byte length + 11-byte header + payload
                    if n < FRAME_LEN_SIZE + HEADER_SIZE {
                        warn!(bytes = n, "UDP packet too short to parse");
                        continue;
                    }
                    let frame_len =
                        u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
                    if n < FRAME_LEN_SIZE + frame_len {
                        warn!(bytes = n, frame_len, "UDP packet truncated");
                        continue;
                    }
                    let header_bytes: [u8; HEADER_SIZE] = buf
                        [FRAME_LEN_SIZE..FRAME_LEN_SIZE + HEADER_SIZE]
                        .try_into()
                        .unwrap();
                    let header = protocol::PacketHeader::decode(&header_bytes);
                    let payload =
                        &buf[FRAME_LEN_SIZE + HEADER_SIZE..FRAME_LEN_SIZE + frame_len];
                    match decode_server(&header, payload) {
                        Ok(ServerMessage::WorldSnapshot(snap)) => {
                            let summary = format!(
                                "UDP WorldSnapshot  tick={:6}  entities={:3}  from={}",
                                snap.tick,
                                snap.entities.len(),
                                from
                            );
                            let _ = udp_tx.send(summary).await;
                        }
                        Ok(other) => {
                            let _ = udp_tx
                                .send(format!("UDP unexpected message: {other:?}"))
                                .await;
                        }
                        Err(e) => {
                            warn!(error = %e, "UDP decode error");
                        }
                    }
                }
                Err(e) => {
                    error!(error = %e, "UDP recv error");
                    break;
                }
            }
        }
    });

    // ── Main loop: move + ping + print ───────────────────────────────────────
    let mut move_ticker = tokio::time::interval(Duration::from_millis(50));
    let mut ping_ticker = tokio::time::interval(Duration::from_secs(2));
    let mut snapshot_count: u64 = 0;
    let mut last_stats = Instant::now();

    loop {
        tokio::select! {
            _ = move_ticker.tick() => {
                seq = seq.wrapping_add(1);
                let move_msg = ClientMessage::MoveInput(MoveInput {
                    direction: Vec2::new(0.0, 1.0),  // north
                    speed: 5.0,
                });
                let (h, p) = encode_client(&move_msg, seq)?;
                if write_tcp_message(&mut tcp_write, &h, &p).await.is_err() {
                    error!("TCP write failed — server disconnected");
                    break;
                }
            }

            _ = ping_ticker.tick() => {
                seq = seq.wrapping_add(1);
                let ts = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let ping = ClientMessage::Ping(Ping { client_timestamp: ts });
                let (h, p) = encode_client(&ping, seq)?;
                write_tcp_message(&mut tcp_write, &h, &p).await?;

                // Read the Pong reply.
                match read_tcp_message(&mut tcp_reader).await {
                    Ok((rh, rp)) => match decode_server(&rh, &rp) {
                        Ok(ServerMessage::Pong(pong)) => {
                            let now = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis() as u64;
                            let rtt_ms = now.saturating_sub(pong.client_timestamp);
                            info!(rtt_ms, "← Pong (RTT)");
                        }
                        Ok(other) => warn!(msg = ?other, "expected Pong, got something else"),
                        Err(e) => warn!(error = %e, "failed to decode Pong"),
                    },
                    Err(e) => {
                        error!(error = %e, "TCP read failed");
                        break;
                    }
                }

                // Print snapshot stats every 2s.
                let elapsed = last_stats.elapsed().as_secs_f32();
                println!(
                    "  [stats] snapshots received last {elapsed:.1}s: {snapshot_count}  \
                     (expected ~{:.0} at 20 ticks/s)",
                    elapsed * 20.0
                );
                snapshot_count = 0;
                last_stats = Instant::now();
            }

            Some(msg) = udp_rx.recv() => {
                snapshot_count += 1;
                // Only print every 10th snapshot to avoid flooding the terminal.
                if snapshot_count % 10 == 1 {
                    println!("  {msg}");
                }
            }

            _ = tokio::signal::ctrl_c() => {
                info!("Ctrl+C — sending Disconnect");
                seq = seq.wrapping_add(1);
                let bye = ClientMessage::Disconnect;
                let (h, p) = encode_client(&bye, seq)?;
                let _ = write_tcp_message(&mut tcp_write, &h, &p).await;
                break;
            }
        }
    }

    info!("Client exiting.");
    Ok(())
}
