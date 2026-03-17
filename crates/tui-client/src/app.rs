use std::collections::VecDeque;
use std::time::Instant;

use common::{
    CharacterId, CharacterInfo, Class, EntityId, EntityKind, EntityState, Race, Vec2, Vec3, ZoneId,
};
use protocol::WorldSnapshot;

use crate::command::{Command, Direction};

/// Events received from the network task.
#[derive(Debug, Clone)]
pub enum ServerEvent {
    /// JWT accepted — now in character select phase.
    HandshakeComplete,
    /// Server sent the character list.
    CharacterList {
        characters: Vec<CharacterInfo>,
    },
    /// A new character was created.
    CharacterCreated {
        character: CharacterInfo,
    },
    /// A character was deleted.
    CharacterDeleted {
        character_id: CharacterId,
    },
    /// Character creation failed.
    CharacterCreateFailed {
        reason: String,
    },
    /// Character selected — entering the world.
    Connected {
        entity_id: EntityId,
        zone_id: ZoneId,
    },
    Rejected {
        reason: String,
    },
    Snapshot(WorldSnapshot),
    Pong {
        rtt_ms: u64,
    },
    ZoneTransfer {
        zone_id: ZoneId,
        position: Vec3,
    },
    Disconnected {
        reason: String,
    },
}

/// Actions to send to the network task.
#[derive(Debug, Clone)]
pub enum ClientAction {
    SendMove { direction: Vec2, speed: f32 },
    SendMoveStop,
    SendPing,
    SendDisconnect,
    RequestCharacterList,
    CreateCharacter { name: String, race: Race, class: Class },
    DeleteCharacter { character_id: CharacterId },
    SelectCharacter { character_id: CharacterId },
    Reconnect,
}

/// Which screen the TUI is showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    Connecting,
    CharacterSelect,
    CharacterCreate,
    Playing,
    Disconnected,
}

/// A log entry.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub timestamp: Instant,
    pub message: String,
}

const MOVE_SPEED: f32 = 5.0;
const MAX_LOG_ENTRIES: usize = 500;

const RACES: [Race; 4] = [Race::Human, Race::Orc, Race::Dwarf, Race::Elf];
const CLASSES: [Class; 4] = [Class::Warrior, Class::Mage, Class::Priest, Class::Rogue];

/// Pure state machine — no I/O, no async. Fully unit-testable.
pub struct ClientApp {
    pub screen: Screen,
    pub entity_id: Option<EntityId>,
    pub zone_id: Option<ZoneId>,
    pub my_position: Vec3,
    pub my_health: u32,
    pub my_max_health: u32,
    pub entities: Vec<EntityState>,
    pub move_direction: Option<Direction>,
    pub rtt_ms: Option<u64>,
    pub last_tick: u64,
    pub snapshot_count: u64,
    pub log: VecDeque<LogEntry>,
    pub log_scroll: usize,
    pub entity_scroll: usize,
    pub show_help: bool,
    pub should_quit: bool,
    pub default_name: Option<String>,

    // Character select state
    pub characters: Vec<CharacterInfo>,
    pub char_select_index: usize,
    pub delete_confirm: bool,

    // Character create state
    pub create_name: String,
    pub create_race_index: usize,
    pub create_class_index: usize,
    pub create_field_focus: u8,
    pub create_error: Option<String>,
}

impl ClientApp {
    pub fn new() -> Self {
        Self {
            screen: Screen::Connecting,
            entity_id: None,
            zone_id: None,
            my_position: Vec3::ZERO,
            my_health: 0,
            my_max_health: 0,
            entities: Vec::new(),
            move_direction: None,
            rtt_ms: None,
            last_tick: 0,
            snapshot_count: 0,
            log: VecDeque::new(),
            log_scroll: 0,
            entity_scroll: 0,
            show_help: false,
            should_quit: false,
            default_name: None,
            characters: Vec::new(),
            char_select_index: 0,
            delete_confirm: false,
            create_name: String::new(),
            create_race_index: 0,
            create_class_index: 0,
            create_field_focus: 0,
            create_error: None,
        }
    }

    pub fn push_log(&mut self, message: String) {
        self.log.push_back(LogEntry {
            timestamp: Instant::now(),
            message,
        });
        if self.log.len() > MAX_LOG_ENTRIES {
            self.log.pop_front();
        }
        // Auto-scroll to bottom
        self.log_scroll = 0;
    }

    /// Reset playing state for reconnect — preserves log.
    fn reset_playing_state(&mut self) {
        self.entity_id = None;
        self.zone_id = None;
        self.my_position = Vec3::ZERO;
        self.my_health = 0;
        self.my_max_health = 0;
        self.entities.clear();
        self.move_direction = None;
        self.rtt_ms = None;
        self.last_tick = 0;
        self.snapshot_count = 0;
        self.entity_scroll = 0;
        self.show_help = false;
        self.delete_confirm = false;
        self.create_error = None;
    }

    /// Apply a server event to the state.
    pub fn apply_server_event(&mut self, event: ServerEvent) {
        match event {
            ServerEvent::HandshakeComplete => {
                self.reset_playing_state();
                self.screen = Screen::CharacterSelect;
                self.push_log("Handshake accepted, loading characters...".to_string());
            }
            ServerEvent::CharacterList { characters } => {
                self.characters = characters;
                if self.char_select_index >= self.characters.len() && !self.characters.is_empty() {
                    self.char_select_index = self.characters.len() - 1;
                }
                self.push_log(format!(
                    "Received {} character(s)",
                    self.characters.len()
                ));
            }
            ServerEvent::CharacterCreated { character } => {
                self.push_log(format!("Created character: {}", character.name));
                self.characters.push(character);
                self.char_select_index = self.characters.len() - 1;
                self.screen = Screen::CharacterSelect;
                self.create_error = None;
            }
            ServerEvent::CharacterDeleted { character_id } => {
                self.characters.retain(|c| c.id != character_id);
                if self.char_select_index >= self.characters.len() && !self.characters.is_empty() {
                    self.char_select_index = self.characters.len() - 1;
                }
                self.delete_confirm = false;
                self.push_log("Character deleted".to_string());
            }
            ServerEvent::CharacterCreateFailed { reason } => {
                self.create_error = Some(reason.clone());
                self.push_log(format!("Create failed: {reason}"));
            }
            ServerEvent::Connected { entity_id, zone_id } => {
                self.screen = Screen::Playing;
                self.entity_id = Some(entity_id);
                self.zone_id = Some(zone_id);
                self.push_log(format!(
                    "Entered world, entity_id={}, zone_id={}",
                    entity_id.get(),
                    zone_id.get()
                ));
            }
            ServerEvent::Rejected { reason } => {
                self.screen = Screen::Disconnected;
                self.push_log(format!("Rejected: {reason}"));
            }
            ServerEvent::Snapshot(snap) => {
                self.last_tick = snap.tick;
                self.snapshot_count += 1;
                if let Some(eid) = self.entity_id {
                    if let Some(me) = snap.entities.iter().find(|e| e.entity_id == eid) {
                        self.my_position = me.position;
                        self.my_health = me.health;
                        self.my_max_health = me.max_health;
                    }
                }
                self.entities = snap.entities;
            }
            ServerEvent::Pong { rtt_ms } => {
                self.rtt_ms = Some(rtt_ms);
                self.push_log(format!("Pong RTT={rtt_ms}ms"));
            }
            ServerEvent::ZoneTransfer { zone_id, position } => {
                self.zone_id = Some(zone_id);
                self.my_position = position;
                self.push_log(format!(
                    "Zone transfer to zone_id={}, pos=({:.1}, {:.1}, {:.1})",
                    zone_id.get(),
                    position.x,
                    position.y,
                    position.z
                ));
            }
            ServerEvent::Disconnected { reason } => {
                self.screen = Screen::Disconnected;
                self.push_log(format!("Disconnected: {reason}"));
            }
        }
    }

    /// Apply a user command. Returns an optional action to send to the network task.
    pub fn apply_command(&mut self, cmd: Command) -> Option<ClientAction> {
        match cmd {
            // ── Playing commands ──
            Command::MoveStart(dir) => {
                self.move_direction = Some(dir);
                Some(ClientAction::SendMove {
                    direction: dir.to_vec2(),
                    speed: MOVE_SPEED,
                })
            }
            Command::MoveStop => {
                self.move_direction = None;
                Some(ClientAction::SendMoveStop)
            }
            Command::Ping => Some(ClientAction::SendPing),
            Command::Disconnect => {
                self.push_log("Disconnecting...".to_string());
                Some(ClientAction::SendDisconnect)
            }
            Command::Quit => {
                self.should_quit = true;
                Some(ClientAction::SendDisconnect)
            }
            Command::ToggleHelp => {
                self.show_help = !self.show_help;
                None
            }
            Command::ScrollLogUp => {
                self.log_scroll = self.log_scroll.saturating_add(3);
                None
            }
            Command::ScrollLogDown => {
                self.log_scroll = self.log_scroll.saturating_sub(3);
                None
            }
            Command::ScrollEntitiesUp => {
                self.entity_scroll = self.entity_scroll.saturating_sub(1);
                None
            }
            Command::ScrollEntitiesDown => {
                self.entity_scroll = self.entity_scroll.saturating_add(1);
                None
            }
            Command::Status | Command::Help => None,

            // ── Logout (from Playing) ──
            Command::Logout => {
                self.push_log("Logging out...".to_string());
                self.screen = Screen::Connecting;
                Some(ClientAction::Reconnect)
            }

            // ── Character select commands ──
            Command::CharSelectUp => {
                self.delete_confirm = false;
                if !self.characters.is_empty() {
                    self.char_select_index =
                        self.char_select_index.saturating_sub(1);
                }
                None
            }
            Command::CharSelectDown => {
                self.delete_confirm = false;
                if !self.characters.is_empty() {
                    self.char_select_index =
                        (self.char_select_index + 1).min(self.characters.len() - 1);
                }
                None
            }
            Command::CharSelectConfirm => {
                if let Some(ch) = self.characters.get(self.char_select_index) {
                    let id = ch.id.clone();
                    self.push_log(format!("Selecting character: {}", ch.name));
                    Some(ClientAction::SelectCharacter { character_id: id })
                } else {
                    None
                }
            }
            Command::CharSelectCreate => {
                self.screen = Screen::CharacterCreate;
                self.create_name = self.default_name.clone().unwrap_or_default();
                self.create_race_index = 0;
                self.create_class_index = 0;
                self.create_field_focus = 0;
                self.create_error = None;
                None
            }
            Command::CharSelectDelete => {
                if self.characters.is_empty() {
                    return None;
                }
                if self.delete_confirm {
                    if let Some(ch) = self.characters.get(self.char_select_index) {
                        let id = ch.id.clone();
                        self.push_log(format!("Deleting character: {}", ch.name));
                        self.delete_confirm = false;
                        Some(ClientAction::DeleteCharacter { character_id: id })
                    } else {
                        self.delete_confirm = false;
                        None
                    }
                } else {
                    self.delete_confirm = true;
                    None
                }
            }

            // ── Character create commands ──
            Command::CreateConfirm => {
                let name = self.create_name.trim().to_string();
                if name.is_empty() {
                    self.create_error = Some("Name cannot be empty".to_string());
                    return None;
                }
                let race = RACES[self.create_race_index];
                let class = CLASSES[self.create_class_index];
                self.push_log(format!("Creating character: {name} {race} {class}"));
                Some(ClientAction::CreateCharacter { name, race, class })
            }
            Command::CreateCancel => {
                self.screen = Screen::CharacterSelect;
                self.create_error = None;
                None
            }
            Command::CreateNextField => {
                self.create_field_focus = (self.create_field_focus + 1) % 3;
                None
            }
            Command::CreateTypeChar(ch) => {
                if self.create_field_focus == 0 && self.create_name.len() < 12 {
                    self.create_name.push(ch);
                    self.create_error = None;
                }
                None
            }
            Command::CreateBackspace => {
                if self.create_field_focus == 0 {
                    self.create_name.pop();
                    self.create_error = None;
                }
                None
            }
            Command::CreateCycleNext => {
                match self.create_field_focus {
                    1 => self.create_race_index = (self.create_race_index + 1) % RACES.len(),
                    2 => self.create_class_index = (self.create_class_index + 1) % CLASSES.len(),
                    _ => {}
                }
                None
            }
            Command::CreateCyclePrev => {
                match self.create_field_focus {
                    1 => {
                        self.create_race_index = if self.create_race_index == 0 {
                            RACES.len() - 1
                        } else {
                            self.create_race_index - 1
                        };
                    }
                    2 => {
                        self.create_class_index = if self.create_class_index == 0 {
                            CLASSES.len() - 1
                        } else {
                            self.create_class_index - 1
                        };
                    }
                    _ => {}
                }
                None
            }
        }
    }

    /// The currently selected race in the create form.
    pub fn create_race(&self) -> Race {
        RACES[self.create_race_index]
    }

    /// The currently selected class in the create form.
    pub fn create_class(&self) -> Class {
        CLASSES[self.create_class_index]
    }

    /// Symbol for rendering an entity kind.
    pub fn entity_symbol(kind: EntityKind, is_self: bool) -> char {
        if is_self {
            return '@';
        }
        match kind {
            EntityKind::Player => 'P',
            EntityKind::Mob => 'M',
            EntityKind::Npc => 'N',
            EntityKind::GameObject => 'O',
        }
    }

    /// Label for an entity kind.
    pub fn entity_label(kind: EntityKind, is_self: bool) -> &'static str {
        if is_self {
            return "You";
        }
        match kind {
            EntityKind::Player => "Player",
            EntityKind::Mob => "Mob",
            EntityKind::Npc => "NPC",
            EntityKind::GameObject => "Object",
        }
    }

    /// Direction as a human-readable string.
    pub fn move_state_str(&self) -> String {
        match self.move_direction {
            None => "Stopped".to_string(),
            Some(dir) => format!("{:?} @ {MOVE_SPEED}", dir),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::EntityFlags;

    #[test]
    fn new_app_is_connecting() {
        let app = ClientApp::new();
        assert_eq!(app.screen, Screen::Connecting);
        assert!(app.entity_id.is_none());
    }

    #[test]
    fn handshake_complete_transitions_to_char_select() {
        let mut app = ClientApp::new();
        app.apply_server_event(ServerEvent::HandshakeComplete);
        assert_eq!(app.screen, Screen::CharacterSelect);
    }

    #[test]
    fn character_list_stores_characters() {
        let mut app = ClientApp::new();
        app.apply_server_event(ServerEvent::HandshakeComplete);
        app.apply_server_event(ServerEvent::CharacterList {
            characters: vec![CharacterInfo {
                id: CharacterId::new("c1".to_string()),
                name: "Arthas".to_string(),
                race: Race::Human,
                class: Class::Warrior,
                level: 12,
                zone_id: ZoneId::new(1),
            }],
        });
        assert_eq!(app.characters.len(), 1);
        assert_eq!(app.characters[0].name, "Arthas");
    }

    #[test]
    fn char_select_up_down() {
        let mut app = ClientApp::new();
        app.characters = vec![
            CharacterInfo {
                id: CharacterId::new("c1".to_string()),
                name: "A".to_string(),
                race: Race::Human,
                class: Class::Warrior,
                level: 1,
                zone_id: ZoneId::new(1),
            },
            CharacterInfo {
                id: CharacterId::new("c2".to_string()),
                name: "B".to_string(),
                race: Race::Orc,
                class: Class::Mage,
                level: 5,
                zone_id: ZoneId::new(1),
            },
        ];
        app.char_select_index = 0;
        app.apply_command(Command::CharSelectDown);
        assert_eq!(app.char_select_index, 1);
        app.apply_command(Command::CharSelectDown); // clamp
        assert_eq!(app.char_select_index, 1);
        app.apply_command(Command::CharSelectUp);
        assert_eq!(app.char_select_index, 0);
    }

    #[test]
    fn char_select_confirm_returns_action() {
        let mut app = ClientApp::new();
        app.characters = vec![CharacterInfo {
            id: CharacterId::new("c1".to_string()),
            name: "A".to_string(),
            race: Race::Human,
            class: Class::Warrior,
            level: 1,
            zone_id: ZoneId::new(1),
        }];
        let action = app.apply_command(Command::CharSelectConfirm);
        assert!(matches!(action, Some(ClientAction::SelectCharacter { .. })));
    }

    #[test]
    fn delete_requires_double_press() {
        let mut app = ClientApp::new();
        app.characters = vec![CharacterInfo {
            id: CharacterId::new("c1".to_string()),
            name: "A".to_string(),
            race: Race::Human,
            class: Class::Warrior,
            level: 1,
            zone_id: ZoneId::new(1),
        }];
        // First press: just sets confirm
        let action = app.apply_command(Command::CharSelectDelete);
        assert!(action.is_none());
        assert!(app.delete_confirm);
        // Second press: actually deletes
        let action = app.apply_command(Command::CharSelectDelete);
        assert!(matches!(action, Some(ClientAction::DeleteCharacter { .. })));
    }

    #[test]
    fn create_screen_round_trip() {
        let mut app = ClientApp::new();
        app.screen = Screen::CharacterSelect;
        app.apply_command(Command::CharSelectCreate);
        assert_eq!(app.screen, Screen::CharacterCreate);
        app.apply_command(Command::CreateCancel);
        assert_eq!(app.screen, Screen::CharacterSelect);
    }

    #[test]
    fn create_type_and_cycle() {
        let mut app = ClientApp::new();
        app.screen = Screen::CharacterCreate;
        app.create_field_focus = 0;
        app.apply_command(Command::CreateTypeChar('A'));
        app.apply_command(Command::CreateTypeChar('b'));
        assert_eq!(app.create_name, "Ab");
        app.apply_command(Command::CreateBackspace);
        assert_eq!(app.create_name, "A");

        // Switch to race field
        app.apply_command(Command::CreateNextField);
        assert_eq!(app.create_field_focus, 1);
        app.apply_command(Command::CreateCycleNext);
        assert_eq!(app.create_race(), Race::Orc);
        app.apply_command(Command::CreateCyclePrev);
        assert_eq!(app.create_race(), Race::Human);
    }

    #[test]
    fn logout_triggers_reconnect() {
        let mut app = ClientApp::new();
        app.screen = Screen::Playing;
        let action = app.apply_command(Command::Logout);
        assert!(matches!(action, Some(ClientAction::Reconnect)));
        assert_eq!(app.screen, Screen::Connecting);
    }

    #[test]
    fn apply_connected_sets_playing() {
        let mut app = ClientApp::new();
        app.apply_server_event(ServerEvent::Connected {
            entity_id: EntityId::new(42),
            zone_id: ZoneId::new(1),
        });
        assert_eq!(app.screen, Screen::Playing);
        assert_eq!(app.entity_id, Some(EntityId::new(42)));
        assert_eq!(app.zone_id, Some(ZoneId::new(1)));
    }

    #[test]
    fn apply_snapshot_updates_position() {
        let mut app = ClientApp::new();
        app.entity_id = Some(EntityId::new(1));
        app.apply_server_event(ServerEvent::Snapshot(WorldSnapshot {
            tick: 100,
            entities: vec![EntityState::new(
                EntityId::new(1),
                EntityKind::Player,
                Vec3::new(10.0, 0.0, 20.0),
                0.0,
                100,
                100,
                EntityFlags::empty(),
                None,
            )],
        }));
        assert_eq!(app.my_position, Vec3::new(10.0, 0.0, 20.0));
        assert_eq!(app.last_tick, 100);
        assert_eq!(app.snapshot_count, 1);
    }

    #[test]
    fn move_start_returns_action() {
        let mut app = ClientApp::new();
        let action = app.apply_command(Command::MoveStart(Direction::North));
        assert!(matches!(action, Some(ClientAction::SendMove { .. })));
        assert_eq!(app.move_direction, Some(Direction::North));
    }

    #[test]
    fn move_stop_clears_direction() {
        let mut app = ClientApp::new();
        app.move_direction = Some(Direction::North);
        let action = app.apply_command(Command::MoveStop);
        assert!(matches!(action, Some(ClientAction::SendMoveStop)));
        assert!(app.move_direction.is_none());
    }

    #[test]
    fn quit_sets_should_quit() {
        let mut app = ClientApp::new();
        app.apply_command(Command::Quit);
        assert!(app.should_quit);
    }

    #[test]
    fn toggle_help() {
        let mut app = ClientApp::new();
        assert!(!app.show_help);
        app.apply_command(Command::ToggleHelp);
        assert!(app.show_help);
        app.apply_command(Command::ToggleHelp);
        assert!(!app.show_help);
    }

    #[test]
    fn log_ring_buffer_caps_at_max() {
        let mut app = ClientApp::new();
        for i in 0..600 {
            app.push_log(format!("msg {i}"));
        }
        assert_eq!(app.log.len(), MAX_LOG_ENTRIES);
    }

    #[test]
    fn character_created_adds_to_list() {
        let mut app = ClientApp::new();
        app.screen = Screen::CharacterCreate;
        app.apply_server_event(ServerEvent::CharacterCreated {
            character: CharacterInfo {
                id: CharacterId::new("c1".to_string()),
                name: "Jaina".to_string(),
                race: Race::Elf,
                class: Class::Mage,
                level: 1,
                zone_id: ZoneId::new(1),
            },
        });
        assert_eq!(app.screen, Screen::CharacterSelect);
        assert_eq!(app.characters.len(), 1);
        assert_eq!(app.char_select_index, 0);
    }

    #[test]
    fn character_deleted_removes_from_list() {
        let mut app = ClientApp::new();
        app.characters = vec![
            CharacterInfo {
                id: CharacterId::new("c1".to_string()),
                name: "A".to_string(),
                race: Race::Human,
                class: Class::Warrior,
                level: 1,
                zone_id: ZoneId::new(1),
            },
            CharacterInfo {
                id: CharacterId::new("c2".to_string()),
                name: "B".to_string(),
                race: Race::Orc,
                class: Class::Mage,
                level: 5,
                zone_id: ZoneId::new(1),
            },
        ];
        app.char_select_index = 1;
        app.apply_server_event(ServerEvent::CharacterDeleted {
            character_id: CharacterId::new("c2".to_string()),
        });
        assert_eq!(app.characters.len(), 1);
        assert_eq!(app.char_select_index, 0);
    }
}
