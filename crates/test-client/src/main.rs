//! Manual test client for Stelline game server.
//!
//! Usage:
//!   cargo run -p test-client                                    # dev mode (JWT_SKIP_VALIDATION)
//!   cargo run -p test-client -- --token mytoken                 # custom token (dev mode)
//!   cargo run -p test-client -- --login alice hunter2           # login via login-server
//!   cargo run -p test-client -- --login alice hunter2 --auth-host 10.0.0.5
//!   cargo run -p test-client -- --host 1.2.3.4                  # remote game server
//!   cargo run -p test-client -- --character Arthas              # select or create character by name
//!
//! What it does:
//!   1. Optionally calls login-server POST /login to get a JWT
//!   2. Connects TCP to the game server
//!   3. Sends a Handshake with the token
//!   4. Lists characters; if --character is given, selects by name (or creates it)
//!   5. Prints HandshakeAccepted (entity id, zone)
//!   6. Sends a Ping every 2 seconds and prints the Pong RTT
//!   7. Sends MoveInput (walking north) every 50ms
//!   8. Binds a UDP socket and listens for WorldSnapshot packets
//!   9. Prints a summary of each snapshot received (tick, entity count)

use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use common::Vec2;
use gateway::{read_tcp_message, write_tcp_message};
use common::{Class, Race};
use protocol::{
    decode_server, encode_client, ClientMessage, CharacterListRequest, CreateCharacter,
    Handshake, MoveInput, Ping, SelectCharacter, ServerMessage, HEADER_SIZE,
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

    // ── Parse CLI args ────────────────────────────────────────────────────────
    let args: Vec<String> = std::env::args().collect();
    let mut host = "127.0.0.1".to_string();
    let mut token: Option<String> = None;
    let mut login_username: Option<String> = None;
    let mut login_password: Option<String> = None;
    let mut auth_host = "127.0.0.1".to_string();
    let mut character_name: Option<String> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--host" => {
                host = args.get(i + 1).cloned().unwrap_or(host);
                i += 2;
            }
            "--token" => {
                token = Some(args.get(i + 1).cloned().unwrap_or_default());
                i += 2;
            }
            "--login" => {
                login_username = args.get(i + 1).cloned();
                login_password = args.get(i + 2).cloned();
                i += 3;
            }
            "--auth-host" => {
                auth_host = args.get(i + 1).cloned().unwrap_or(auth_host);
                i += 2;
            }
            "--character" => {
                character_name = args.get(i + 1).cloned();
                i += 2;
            }
            _ => {
                i += 1;
            }
        }
    }

    // ── Resolve token ─────────────────────────────────────────────────────────
    let token = if let (Some(username), Some(password)) = (login_username, login_password) {
        info!(username = %username, auth_host = %auth_host, "logging in via login-server");
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("http://{auth_host}:8080/login"))
            .json(&serde_json::json!({"username": username, "password": password}))
            .send()
            .await
            .context("failed to reach login-server")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("login-server returned {status}: {body}");
        }

        let body: serde_json::Value = resp.json().await.context("failed to parse login response")?;
        let t = body["token"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("no 'token' field in login response"))?
            .to_string();
        info!("login successful — got JWT");
        t
    } else {
        token.unwrap_or_else(|| "test-client-token".to_string())
    };

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
    let udp_bind = format!("{}:{}", local_addr.ip(), local_addr.port());
    let udp_socket = UdpSocket::bind(&udp_bind).await?;
    let udp_local = udp_socket.local_addr()?;
    info!(udp_port = udp_local.port(), "UDP recv socket bound");

    // ── Phase 1: Handshake ─────────────────────────────────────────────────────
    let mut seq: u32 = 0;
    let hs = ClientMessage::Handshake(Handshake { token: token.clone() });
    let (h, p) = encode_client(&hs, seq)?;
    write_tcp_message(&mut tcp_write, &h, &p).await?;
    info!("→ Handshake sent (waiting for auth...)");

    // ── Phase 2: Character Selection ─────────────────────────────────────────
    // After auth succeeds, the server enters character selection phase.
    // Request character list first.
    seq = seq.wrapping_add(1);
    let list_req = ClientMessage::CharacterListRequest(CharacterListRequest {});
    let (h, p) = encode_client(&list_req, seq)?;
    write_tcp_message(&mut tcp_write, &h, &p).await?;
    info!("→ CharacterListRequest sent");

    let (rh, rp) = read_tcp_message(&mut tcp_reader).await?;
    let reply = decode_server(&rh, &rp)?;
    let character_id = match reply {
        ServerMessage::CharacterList(ref cl) => {
            info!(count = cl.characters.len(), "← CharacterList");
            for c in &cl.characters {
                info!(name = %c.name, id = %c.id, level = c.level, race = ?c.race, class = ?c.class, "  character");
            }

            // Determine which character to select.
            let desired_name = character_name.as_deref().unwrap_or("TestHero");

            // Try to find an existing character by name.
            if let Some(found) = cl.characters.iter().find(|c| c.name == desired_name) {
                info!(name = %found.name, id = %found.id, "selecting existing character");
                found.id.clone()
            } else {
                // Create a new character with the desired name.
                info!(name = %desired_name, "character not found, creating...");
                seq = seq.wrapping_add(1);
                let create = ClientMessage::CreateCharacter(CreateCharacter {
                    name: desired_name.to_string(),
                    race: Race::Human,
                    class: Class::Warrior,
                });
                let (h, p) = encode_client(&create, seq)?;
                write_tcp_message(&mut tcp_write, &h, &p).await?;

                let (rh, rp) = read_tcp_message(&mut tcp_reader).await?;
                let reply = decode_server(&rh, &rp)?;
                match reply {
                    ServerMessage::CharacterCreated(ref cc) => {
                        info!(name = %cc.character.name, id = %cc.character.id, "← CharacterCreated");
                        cc.character.id.clone()
                    }
                    ServerMessage::CharacterCreateFailed(ref f) => {
                        error!(reason = %f.reason, "← CharacterCreateFailed");
                        return Ok(());
                    }
                    other => {
                        error!(msg = ?other, "unexpected reply to CreateCharacter");
                        return Ok(());
                    }
                }
            }
        }
        ServerMessage::HandshakeRejected(ref rej) => {
            error!(reason = ?rej.reason, "HandshakeRejected — check your token");
            return Ok(());
        }
        other => {
            error!(msg = ?other, "unexpected message during character selection");
            return Ok(());
        }
    };

    // Select the character.
    seq = seq.wrapping_add(1);
    let select = ClientMessage::SelectCharacter(SelectCharacter { character_id });
    let (h, p) = encode_client(&select, seq)?;
    write_tcp_message(&mut tcp_write, &h, &p).await?;
    info!("→ SelectCharacter sent");

    let (rh, rp) = read_tcp_message(&mut tcp_reader).await?;
    let reply = decode_server(&rh, &rp)?;
    match reply {
        ServerMessage::HandshakeAccepted(ref accepted) => {
            info!(
                entity_id = accepted.entity_id.get(),
                zone_id = accepted.zone_id.get(),
                "← HandshakeAccepted — entering world"
            );
        }
        ServerMessage::HandshakeRejected(ref rej) => {
            error!(reason = ?rej.reason, "HandshakeRejected after character select");
            return Ok(());
        }
        other => {
            error!(msg = ?other, "unexpected message after SelectCharacter");
            return Ok(());
        }
    }

    println!();
    println!("═══════════════════════════════════════════════════");
    println!("  Connected! Running test loop. Press Ctrl+C to quit.");
    println!("  → sending MoveInput (north) every 50ms");
    println!("  → sending Ping every 2s");
    println!("  → listening for WorldSnapshot on UDP :{}", udp_local.port());
    println!("═══════════════════════════════════════════════════");
    println!();

    // ── Spawn UDP listener task ───────────────────────────────────────────────
    let (udp_tx, mut udp_rx) = mpsc::channel::<String>(32);
    tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        loop {
            match udp_socket.recv_from(&mut buf).await {
                Ok((n, from)) => {
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

                match read_tcp_message(&mut tcp_reader).await {
                    Ok((rh, rp)) => match decode_server(&rh, &rp) {
                        Ok(ServerMessage::Pong(pong)) => {
                            let now = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis() as u64;
                            let rtt_ms = now.saturating_sub(pong.client_timestamp);
                            info!(rtt_ms, "Pong (RTT)");
                        }
                        Ok(other) => warn!(msg = ?other, "expected Pong, got something else"),
                        Err(e) => warn!(error = %e, "failed to decode Pong"),
                    },
                    Err(e) => {
                        error!(error = %e, "TCP read failed");
                        break;
                    }
                }

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
