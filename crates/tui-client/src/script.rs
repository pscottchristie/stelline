use std::time::{Duration, Instant};

use anyhow::Result;
use common::{Class, Race};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

use crate::app::{ClientAction, ClientApp, ServerEvent};
use crate::command::{parse_script_line, ScriptCommand, COMMAND_REGISTRY};

/// Run script mode — reads commands from stdin, emits JSON to stdout.
/// Automatically handles character selection for backward compatibility:
/// - On HandshakeComplete: requests character list
/// - On CharacterList: auto-selects first, or auto-creates if empty
/// - On CharacterCreated: auto-selects
pub async fn run_script(
    mut event_rx: mpsc::UnboundedReceiver<ServerEvent>,
    action_tx: mpsc::UnboundedSender<ClientAction>,
    default_name: Option<String>,
) -> Result<()> {
    let mut app = ClientApp::new();
    app.default_name = default_name;
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();

    // Throttle snapshot output to 1/sec
    let mut last_snapshot_emit = Instant::now() - Duration::from_secs(2);

    // First, wait for connection (auto-handle character selection)
    loop {
        tokio::select! {
            event = event_rx.recv() => {
                match event {
                    Some(ev) => {
                        app.apply_server_event(ev.clone());
                        emit_event_json(&app, &ev, &mut last_snapshot_emit);
                        match ev {
                            ServerEvent::HandshakeComplete => {
                                let _ = action_tx.send(ClientAction::RequestCharacterList);
                            }
                            ServerEvent::CharacterList { ref characters } => {
                                if let Some(first) = characters.first() {
                                    let _ = action_tx.send(ClientAction::SelectCharacter {
                                        character_id: first.id.clone(),
                                    });
                                } else {
                                    // No characters — create one
                                    let name = app.default_name.clone().unwrap_or_else(|| {
                                        let n: u32 = std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap_or_default()
                                            .subsec_nanos();
                                        format!("Hero{}", n % 100000)
                                    });
                                    let _ = action_tx.send(ClientAction::CreateCharacter {
                                        name,
                                        race: Race::Human,
                                        class: Class::Warrior,
                                    });
                                }
                            }
                            ServerEvent::CharacterCreated { ref character } => {
                                let _ = action_tx.send(ClientAction::SelectCharacter {
                                    character_id: character.id.clone(),
                                });
                            }
                            ServerEvent::Connected { .. } => break,
                            ServerEvent::Rejected { .. } | ServerEvent::Disconnected { .. } => {
                                return Ok(());
                            }
                            _ => {}
                        }
                    }
                    None => return Ok(()),
                }
            }
        }
    }

    // Main loop: process stdin commands and server events
    loop {
        tokio::select! {
            // Server events
            event = event_rx.recv() => {
                match event {
                    Some(ev) => {
                        app.apply_server_event(ev.clone());
                        emit_event_json(&app, &ev, &mut last_snapshot_emit);
                        if matches!(ev, ServerEvent::Disconnected { .. } | ServerEvent::Rejected { .. }) {
                            return Ok(());
                        }
                    }
                    None => return Ok(()),
                }
            }

            // Stdin commands
            line = reader.next_line() => {
                match line? {
                    Some(line) => {
                        match parse_script_line(&line) {
                            Some(ScriptCommand::Cmd(cmd)) => {
                                // Handle special script-only commands
                                match cmd {
                                    crate::command::Command::Help => {
                                        emit_help_json();
                                        continue;
                                    }
                                    crate::command::Command::Status => {
                                        emit_status_json(&app);
                                        continue;
                                    }
                                    crate::command::Command::Quit => {
                                        let _ = action_tx.send(ClientAction::SendDisconnect);
                                        // Drain remaining events briefly
                                        tokio::time::sleep(Duration::from_millis(100)).await;
                                        return Ok(());
                                    }
                                    _ => {}
                                }
                                if let Some(action) = app.apply_command(cmd) {
                                    let _ = action_tx.send(action);
                                }
                            }
                            Some(ScriptCommand::Wait(ms)) => {
                                // During wait, keep processing server events
                                let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
                                loop {
                                    tokio::select! {
                                        event = event_rx.recv() => {
                                            match event {
                                                Some(ev) => {
                                                    app.apply_server_event(ev.clone());
                                                    emit_event_json(&app, &ev, &mut last_snapshot_emit);
                                                }
                                                None => return Ok(()),
                                            }
                                        }
                                        _ = tokio::time::sleep_until(deadline) => {
                                            break;
                                        }
                                    }
                                }
                            }
                            None => {} // empty line or comment
                        }
                    }
                    None => {
                        // stdin closed — send disconnect and exit
                        let _ = action_tx.send(ClientAction::SendDisconnect);
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn emit_event_json(app: &ClientApp, event: &ServerEvent, last_snapshot: &mut Instant) {
    match event {
        ServerEvent::Connected {
            entity_id,
            zone_id,
        } => {
            println!(
                "{}",
                serde_json::json!({
                    "event": "connected",
                    "entity_id": entity_id.get(),
                    "zone_id": zone_id.get()
                })
            );
        }
        ServerEvent::Rejected { reason } => {
            println!(
                "{}",
                serde_json::json!({
                    "event": "rejected",
                    "reason": reason
                })
            );
        }
        ServerEvent::Snapshot(snap) => {
            // Throttle to 1/sec
            if last_snapshot.elapsed() >= Duration::from_secs(1) {
                *last_snapshot = Instant::now();
                println!(
                    "{}",
                    serde_json::json!({
                        "event": "snapshot",
                        "tick": snap.tick,
                        "entity_count": snap.entities.len(),
                        "my_position": [app.my_position.x, app.my_position.y, app.my_position.z]
                    })
                );
            }
        }
        ServerEvent::Pong { rtt_ms } => {
            println!(
                "{}",
                serde_json::json!({
                    "event": "pong",
                    "rtt_ms": rtt_ms
                })
            );
        }
        ServerEvent::ZoneTransfer { zone_id, position } => {
            println!(
                "{}",
                serde_json::json!({
                    "event": "zone_transfer",
                    "zone_id": zone_id.get(),
                    "position": [position.x, position.y, position.z]
                })
            );
        }
        ServerEvent::Disconnected { reason } => {
            println!(
                "{}",
                serde_json::json!({
                    "event": "disconnected",
                    "reason": reason
                })
            );
        }
        ServerEvent::HandshakeComplete => {
            println!(
                "{}",
                serde_json::json!({ "event": "handshake_complete" })
            );
        }
        ServerEvent::CharacterList { characters } => {
            println!(
                "{}",
                serde_json::json!({
                    "event": "character_list",
                    "count": characters.len()
                })
            );
        }
        ServerEvent::CharacterCreated { character } => {
            println!(
                "{}",
                serde_json::json!({
                    "event": "character_created",
                    "name": character.name
                })
            );
        }
        ServerEvent::CharacterDeleted { .. } => {
            println!(
                "{}",
                serde_json::json!({ "event": "character_deleted" })
            );
        }
        ServerEvent::CharacterCreateFailed { reason } => {
            println!(
                "{}",
                serde_json::json!({
                    "event": "character_create_failed",
                    "reason": reason
                })
            );
        }
    }
}

fn emit_status_json(app: &ClientApp) {
    let conn_str = match app.screen {
        crate::app::Screen::Connecting => "connecting",
        crate::app::Screen::Playing => "connected",
        crate::app::Screen::Disconnected => "disconnected",
        crate::app::Screen::CharacterSelect => "character_select",
        crate::app::Screen::CharacterCreate => "character_create",
    };
    println!(
        "{}",
        serde_json::json!({
            "event": "status",
            "connection": conn_str,
            "entity_id": app.entity_id.map(|e| e.get()),
            "zone_id": app.zone_id.map(|z| z.get()),
            "position": [app.my_position.x, app.my_position.y, app.my_position.z],
            "entities": app.entities.len(),
            "snapshots": app.snapshot_count,
            "rtt_ms": app.rtt_ms
        })
    );
}

fn emit_help_json() {
    let commands: Vec<serde_json::Value> = COMMAND_REGISTRY
        .iter()
        .map(|c| {
            serde_json::json!({
                "name": c.name,
                "syntax": c.syntax,
                "description": c.description
            })
        })
        .collect();
    println!(
        "{}",
        serde_json::json!({
            "event": "help",
            "commands": commands
        })
    );
}
