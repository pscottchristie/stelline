use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use common::Vec2;
use gateway::{read_tcp_message, write_tcp_message};
use protocol::{
    decode_server, encode_client, CharacterListRequest, ClientMessage, CreateCharacter,
    DeleteCharacter, Handshake, MoveInput, Ping, SelectCharacter, ServerMessage, HEADER_SIZE,
};
use tokio::io::BufReader;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

use crate::app::{ClientAction, ServerEvent};

const FRAME_LEN_SIZE: usize = 4;

pub struct NetworkConfig {
    pub host: String,
    pub token: String,
}

/// Run the network task with reconnect support.
/// Outer loop reconnects when the app sends `Reconnect` (logout).
pub async fn run_network(
    config: NetworkConfig,
    event_tx: mpsc::UnboundedSender<ServerEvent>,
    mut action_rx: mpsc::UnboundedReceiver<ClientAction>,
) -> Result<()> {
    loop {
        // Drain stale actions from a previous session
        while action_rx.try_recv().is_ok() {}

        match run_session(&config, &event_tx, &mut action_rx).await {
            Ok(SessionEnd::Reconnect) => {
                // Loop back and reconnect
                continue;
            }
            Ok(SessionEnd::Quit) => return Ok(()),
            Err(e) => {
                let _ = event_tx.send(ServerEvent::Disconnected {
                    reason: format!("{e}"),
                });
                return Err(e);
            }
        }
    }
}

enum SessionEnd {
    Reconnect,
    Quit,
}

/// One full session: connect → handshake → character select → play → disconnect.
async fn run_session(
    config: &NetworkConfig,
    event_tx: &mpsc::UnboundedSender<ServerEvent>,
    action_rx: &mut mpsc::UnboundedReceiver<ClientAction>,
) -> Result<SessionEnd> {
    let tcp_addr = format!("{}:7878", config.host);

    let stream = TcpStream::connect(&tcp_addr)
        .await
        .with_context(|| format!("failed to connect to {tcp_addr}"))?;

    let local_addr: SocketAddr = stream.local_addr()?;
    let (tcp_read, mut tcp_write) = stream.into_split();
    let mut tcp_reader = BufReader::new(tcp_read);

    // Bind UDP on same local addr for receiving snapshots
    let udp_bind = format!("{}:{}", local_addr.ip(), local_addr.port());
    let udp_socket = UdpSocket::bind(&udp_bind).await?;

    // ── Phase 1: Handshake ──────────────────────────────────────────────────
    let mut seq: u32 = 0;
    let hs = ClientMessage::Handshake(Handshake {
        token: config.token.clone(),
    });
    let (h, p) = encode_client(&hs, seq)?;
    write_tcp_message(&mut tcp_write, &h, &p).await?;

    // Request character list immediately
    seq = seq.wrapping_add(1);
    let list_req = ClientMessage::CharacterListRequest(CharacterListRequest {});
    let (h, p) = encode_client(&list_req, seq)?;
    write_tcp_message(&mut tcp_write, &h, &p).await?;

    // Read first response — expect CharacterList or HandshakeRejected
    let (rh, rp) = read_tcp_message(&mut tcp_reader).await?;
    let reply = decode_server(&rh, &rp)?;
    match reply {
        ServerMessage::CharacterList(ref cl) => {
            let _ = event_tx.send(ServerEvent::HandshakeComplete);
            let _ = event_tx.send(ServerEvent::CharacterList {
                characters: cl.characters.clone(),
            });
        }
        ServerMessage::HandshakeRejected(ref rej) => {
            let _ = event_tx.send(ServerEvent::Rejected {
                reason: format!("{:?}", rej.reason),
            });
            return Ok(SessionEnd::Quit);
        }
        other => {
            let _ = event_tx.send(ServerEvent::Rejected {
                reason: format!("unexpected message during handshake: {other:?}"),
            });
            return Ok(SessionEnd::Quit);
        }
    }

    // ── Phase 2: Character Select Loop ──────────────────────────────────────
    loop {
        tokio::select! {
            action = action_rx.recv() => {
                match action {
                    Some(ClientAction::RequestCharacterList) => {
                        seq = seq.wrapping_add(1);
                        let msg = ClientMessage::CharacterListRequest(CharacterListRequest {});
                        let (h, p) = encode_client(&msg, seq)?;
                        write_tcp_message(&mut tcp_write, &h, &p).await?;
                    }
                    Some(ClientAction::CreateCharacter { name, race, class }) => {
                        seq = seq.wrapping_add(1);
                        let msg = ClientMessage::CreateCharacter(CreateCharacter { name, race, class });
                        let (h, p) = encode_client(&msg, seq)?;
                        write_tcp_message(&mut tcp_write, &h, &p).await?;
                    }
                    Some(ClientAction::DeleteCharacter { character_id }) => {
                        seq = seq.wrapping_add(1);
                        let msg = ClientMessage::DeleteCharacter(DeleteCharacter { character_id });
                        let (h, p) = encode_client(&msg, seq)?;
                        write_tcp_message(&mut tcp_write, &h, &p).await?;
                    }
                    Some(ClientAction::SelectCharacter { character_id }) => {
                        seq = seq.wrapping_add(1);
                        let msg = ClientMessage::SelectCharacter(SelectCharacter { character_id });
                        let (h, p) = encode_client(&msg, seq)?;
                        write_tcp_message(&mut tcp_write, &h, &p).await?;
                        // Response is read from TCP below
                    }
                    Some(ClientAction::SendDisconnect) | None => {
                        seq = seq.wrapping_add(1);
                        let msg = ClientMessage::Disconnect;
                        let (h, p) = encode_client(&msg, seq)?;
                        let _ = write_tcp_message(&mut tcp_write, &h, &p).await;
                        return Ok(SessionEnd::Quit);
                    }
                    Some(ClientAction::Reconnect) => {
                        seq = seq.wrapping_add(1);
                        let msg = ClientMessage::Disconnect;
                        let (h, p) = encode_client(&msg, seq)?;
                        let _ = write_tcp_message(&mut tcp_write, &h, &p).await;
                        return Ok(SessionEnd::Reconnect);
                    }
                    Some(_) => {
                        // Ignore playing actions during character select
                    }
                }
            }
            tcp_result = read_tcp_message(&mut tcp_reader) => {
                match tcp_result {
                    Ok((rh, rp)) => {
                        match decode_server(&rh, &rp) {
                            Ok(ServerMessage::CharacterList(cl)) => {
                                let _ = event_tx.send(ServerEvent::CharacterList {
                                    characters: cl.characters,
                                });
                            }
                            Ok(ServerMessage::CharacterCreated(cc)) => {
                                let _ = event_tx.send(ServerEvent::CharacterCreated {
                                    character: cc.character,
                                });
                            }
                            Ok(ServerMessage::CharacterDeleted(cd)) => {
                                let _ = event_tx.send(ServerEvent::CharacterDeleted {
                                    character_id: cd.character_id,
                                });
                            }
                            Ok(ServerMessage::CharacterCreateFailed(f)) => {
                                let _ = event_tx.send(ServerEvent::CharacterCreateFailed {
                                    reason: f.reason,
                                });
                            }
                            Ok(ServerMessage::HandshakeAccepted(accepted)) => {
                                let _ = event_tx.send(ServerEvent::Connected {
                                    entity_id: accepted.entity_id,
                                    zone_id: accepted.zone_id,
                                });
                                break; // Transition to phase 3 (play loop)
                            }
                            Ok(ServerMessage::HandshakeRejected(rej)) => {
                                let _ = event_tx.send(ServerEvent::Rejected {
                                    reason: format!("{:?}", rej.reason),
                                });
                                return Ok(SessionEnd::Quit);
                            }
                            Ok(ServerMessage::Pong(pong)) => {
                                let now = SystemTime::now()
                                    .duration_since(UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_millis() as u64;
                                let rtt_ms = now.saturating_sub(pong.client_timestamp);
                                let _ = event_tx.send(ServerEvent::Pong { rtt_ms });
                            }
                            Ok(other) => {
                                debug!(?other, "TCP message (unhandled in char select)");
                            }
                            Err(e) => {
                                warn!(error = %e, "TCP decode error in char select");
                            }
                        }
                    }
                    Err(e) => {
                        let _ = event_tx.send(ServerEvent::Disconnected {
                            reason: format!("{e}"),
                        });
                        return Ok(SessionEnd::Quit);
                    }
                }
            }
        }
    }

    // ── Phase 3: Play Loop ──────────────────────────────────────────────────
    let mut udp_buf = vec![0u8; 65536];
    let mut ping_interval = tokio::time::interval(Duration::from_secs(2));
    let mut move_interval = tokio::time::interval(Duration::from_millis(50));
    let mut current_move: Option<(Vec2, f32)> = None;

    loop {
        tokio::select! {
            // TCP messages from server
            tcp_result = read_tcp_message(&mut tcp_reader) => {
                match tcp_result {
                    Ok((rh, rp)) => {
                        match decode_server(&rh, &rp) {
                            Ok(ServerMessage::Pong(pong)) => {
                                let now = SystemTime::now()
                                    .duration_since(UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_millis() as u64;
                                let rtt_ms = now.saturating_sub(pong.client_timestamp);
                                let _ = event_tx.send(ServerEvent::Pong { rtt_ms });
                            }
                            Ok(ServerMessage::ZoneTransfer(zt)) => {
                                let _ = event_tx.send(ServerEvent::ZoneTransfer {
                                    zone_id: zt.zone_id,
                                    position: zt.position,
                                });
                            }
                            Ok(other) => {
                                debug!(?other, "TCP message (unhandled in play loop)");
                            }
                            Err(e) => {
                                warn!(error = %e, "TCP decode error");
                            }
                        }
                    }
                    Err(e) => {
                        let _ = event_tx.send(ServerEvent::Disconnected {
                            reason: format!("{e}"),
                        });
                        return Ok(SessionEnd::Quit);
                    }
                }
            }

            // UDP snapshots from server
            udp_result = udp_socket.recv_from(&mut udp_buf) => {
                match udp_result {
                    Ok((n, _from)) => {
                        if n < FRAME_LEN_SIZE + HEADER_SIZE {
                            continue;
                        }
                        let frame_len = u32::from_le_bytes(
                            udp_buf[..4].try_into().unwrap()
                        ) as usize;
                        if n < FRAME_LEN_SIZE + frame_len {
                            continue;
                        }
                        let header_bytes: [u8; HEADER_SIZE] = udp_buf
                            [FRAME_LEN_SIZE..FRAME_LEN_SIZE + HEADER_SIZE]
                            .try_into()
                            .unwrap();
                        let header = protocol::PacketHeader::decode(&header_bytes);
                        let payload = &udp_buf[FRAME_LEN_SIZE + HEADER_SIZE..FRAME_LEN_SIZE + frame_len];
                        match decode_server(&header, payload) {
                            Ok(ServerMessage::WorldSnapshot(snap)) => {
                                let _ = event_tx.send(ServerEvent::Snapshot(snap));
                            }
                            Ok(_) => {}
                            Err(e) => {
                                warn!(error = %e, "UDP decode error");
                            }
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "UDP recv error");
                        return Ok(SessionEnd::Quit);
                    }
                }
            }

            // Actions from the app
            action = action_rx.recv() => {
                match action {
                    Some(ClientAction::SendMove { direction, speed }) => {
                        current_move = Some((direction, speed));
                        seq = seq.wrapping_add(1);
                        let msg = ClientMessage::MoveInput(MoveInput { direction, speed });
                        let (h, p) = encode_client(&msg, seq)?;
                        if write_tcp_message(&mut tcp_write, &h, &p).await.is_err() {
                            return Ok(SessionEnd::Quit);
                        }
                    }
                    Some(ClientAction::SendMoveStop) => {
                        current_move = None;
                        seq = seq.wrapping_add(1);
                        let msg = ClientMessage::MoveInput(MoveInput {
                            direction: Vec2::ZERO,
                            speed: 0.0,
                        });
                        let (h, p) = encode_client(&msg, seq)?;
                        if write_tcp_message(&mut tcp_write, &h, &p).await.is_err() {
                            return Ok(SessionEnd::Quit);
                        }
                    }
                    Some(ClientAction::SendPing) => {
                        seq = seq.wrapping_add(1);
                        let ts = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64;
                        let msg = ClientMessage::Ping(Ping { client_timestamp: ts });
                        let (h, p) = encode_client(&msg, seq)?;
                        let _ = write_tcp_message(&mut tcp_write, &h, &p).await;
                    }
                    Some(ClientAction::SendDisconnect) => {
                        seq = seq.wrapping_add(1);
                        let msg = ClientMessage::Disconnect;
                        let (h, p) = encode_client(&msg, seq)?;
                        let _ = write_tcp_message(&mut tcp_write, &h, &p).await;
                        return Ok(SessionEnd::Quit);
                    }
                    Some(ClientAction::Reconnect) => {
                        seq = seq.wrapping_add(1);
                        let msg = ClientMessage::Disconnect;
                        let (h, p) = encode_client(&msg, seq)?;
                        let _ = write_tcp_message(&mut tcp_write, &h, &p).await;
                        return Ok(SessionEnd::Reconnect);
                    }
                    None => return Ok(SessionEnd::Quit),
                    Some(_) => {
                        // Ignore character-select actions during play
                    }
                }
            }

            // Periodic ping
            _ = ping_interval.tick() => {
                seq = seq.wrapping_add(1);
                let ts = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let msg = ClientMessage::Ping(Ping { client_timestamp: ts });
                let (h, p) = encode_client(&msg, seq)?;
                let _ = write_tcp_message(&mut tcp_write, &h, &p).await;
            }

            // Continuous movement sending
            _ = move_interval.tick() => {
                if let Some((direction, speed)) = current_move {
                    seq = seq.wrapping_add(1);
                    let msg = ClientMessage::MoveInput(MoveInput { direction, speed });
                    let (h, p) = encode_client(&msg, seq)?;
                    if write_tcp_message(&mut tcp_write, &h, &p).await.is_err() {
                        return Ok(SessionEnd::Quit);
                    }
                }
            }
        }
    }
}
