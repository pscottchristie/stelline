use common::Vec2;

/// A user intent — from keyboard (TUI) or stdin line (script mode).
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    // ── Playing ──
    MoveStart(Direction),
    MoveStop,
    Ping,
    Disconnect,
    Quit,
    ToggleHelp,
    ScrollLogUp,
    ScrollLogDown,
    ScrollEntitiesUp,
    ScrollEntitiesDown,
    Status,
    Help,
    Logout,

    // ── Character select ──
    CharSelectUp,
    CharSelectDown,
    CharSelectConfirm,
    CharSelectCreate,
    CharSelectDelete,

    // ── Character create ──
    CreateConfirm,
    CreateCancel,
    CreateNextField,
    CreateTypeChar(char),
    CreateBackspace,
    CreateCycleNext,
    CreateCyclePrev,
}

/// Cardinal and inter-cardinal directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    North,
    South,
    East,
    West,
    NorthEast,
    NorthWest,
    SouthEast,
    SouthWest,
}

impl Direction {
    /// Convert to a unit Vec2 in the XZ plane.
    /// +Y = north, +X = east.
    pub fn to_vec2(self) -> Vec2 {
        let d = std::f32::consts::FRAC_1_SQRT_2;
        match self {
            Direction::North => Vec2::new(0.0, 1.0),
            Direction::South => Vec2::new(0.0, -1.0),
            Direction::East => Vec2::new(1.0, 0.0),
            Direction::West => Vec2::new(-1.0, 0.0),
            Direction::NorthEast => Vec2::new(d, d),
            Direction::NorthWest => Vec2::new(-d, d),
            Direction::SouthEast => Vec2::new(d, -d),
            Direction::SouthWest => Vec2::new(-d, -d),
        }
    }
}

/// A command definition for the help registry.
pub struct CommandDef {
    pub name: &'static str,
    pub syntax: &'static str,
    pub description: &'static str,
}

/// Central registry of all commands — used by both TUI help overlay and script `help` output.
pub static COMMAND_REGISTRY: &[CommandDef] = &[
    CommandDef {
        name: "move",
        syntax: "move <north|south|east|west|ne|nw|se|sw|stop>",
        description: "Start moving in a direction, or stop",
    },
    CommandDef {
        name: "ping",
        syntax: "ping",
        description: "Send a ping, receive RTT",
    },
    CommandDef {
        name: "status",
        syntax: "status",
        description: "Print current client state",
    },
    CommandDef {
        name: "wait",
        syntax: "wait <ms>",
        description: "Sleep for N milliseconds",
    },
    CommandDef {
        name: "help",
        syntax: "help",
        description: "List all available commands",
    },
    CommandDef {
        name: "disconnect",
        syntax: "disconnect",
        description: "Disconnect from server",
    },
    CommandDef {
        name: "quit",
        syntax: "quit",
        description: "Disconnect and exit",
    },
];

/// Parse a script-mode input line into a Command (or a wait duration).
pub enum ScriptCommand {
    Cmd(Command),
    Wait(u64),
}

pub fn parse_script_line(line: &str) -> Option<ScriptCommand> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let parts: Vec<&str> = line.splitn(2, ' ').collect();
    match parts[0] {
        "move" => {
            let arg = parts.get(1).map(|s| s.trim()).unwrap_or("stop");
            match arg {
                "north" => Some(ScriptCommand::Cmd(Command::MoveStart(Direction::North))),
                "south" => Some(ScriptCommand::Cmd(Command::MoveStart(Direction::South))),
                "east" => Some(ScriptCommand::Cmd(Command::MoveStart(Direction::East))),
                "west" => Some(ScriptCommand::Cmd(Command::MoveStart(Direction::West))),
                "ne" | "northeast" => {
                    Some(ScriptCommand::Cmd(Command::MoveStart(Direction::NorthEast)))
                }
                "nw" | "northwest" => {
                    Some(ScriptCommand::Cmd(Command::MoveStart(Direction::NorthWest)))
                }
                "se" | "southeast" => {
                    Some(ScriptCommand::Cmd(Command::MoveStart(Direction::SouthEast)))
                }
                "sw" | "southwest" => {
                    Some(ScriptCommand::Cmd(Command::MoveStart(Direction::SouthWest)))
                }
                "stop" => Some(ScriptCommand::Cmd(Command::MoveStop)),
                _ => None,
            }
        }
        "ping" => Some(ScriptCommand::Cmd(Command::Ping)),
        "status" => Some(ScriptCommand::Cmd(Command::Status)),
        "disconnect" => Some(ScriptCommand::Cmd(Command::Disconnect)),
        "quit" => Some(ScriptCommand::Cmd(Command::Quit)),
        "help" => Some(ScriptCommand::Cmd(Command::Help)),
        "wait" => {
            let ms: u64 = parts.get(1)?.trim().parse().ok()?;
            Some(ScriptCommand::Wait(ms))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_to_vec2_north() {
        let v = Direction::North.to_vec2();
        assert!((v.x).abs() < 1e-5);
        assert!((v.y - 1.0).abs() < 1e-5);
    }

    #[test]
    fn direction_to_vec2_diagonal_is_unit() {
        let v = Direction::NorthEast.to_vec2();
        let len = (v.x * v.x + v.y * v.y).sqrt();
        assert!((len - 1.0).abs() < 1e-5);
    }

    #[test]
    fn parse_move_north() {
        match parse_script_line("move north") {
            Some(ScriptCommand::Cmd(Command::MoveStart(Direction::North))) => {}
            other => panic!("expected MoveStart(North), got {:?}", other.is_some()),
        }
    }

    #[test]
    fn parse_wait() {
        match parse_script_line("wait 2000") {
            Some(ScriptCommand::Wait(2000)) => {}
            _ => panic!("expected Wait(2000)"),
        }
    }

    #[test]
    fn parse_empty_and_comment() {
        assert!(parse_script_line("").is_none());
        assert!(parse_script_line("# comment").is_none());
    }
}
