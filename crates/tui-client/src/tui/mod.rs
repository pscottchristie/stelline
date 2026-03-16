pub mod layout;
pub mod widgets;

use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::app::{ClientAction, ClientApp, Screen, ServerEvent};
use crate::command::{Command, Direction};

/// Run the TUI mode — interactive terminal UI.
pub async fn run_tui(
    mut event_rx: mpsc::UnboundedReceiver<ServerEvent>,
    action_tx: mpsc::UnboundedSender<ClientAction>,
    default_name: Option<String>,
) -> Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = ClientApp::new();
    app.default_name = default_name;

    let result = run_tui_loop(&mut terminal, &mut app, &mut event_rx, &action_tx).await;

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn run_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut ClientApp,
    event_rx: &mut mpsc::UnboundedReceiver<ServerEvent>,
    action_tx: &mpsc::UnboundedSender<ClientAction>,
) -> Result<()> {
    // Fixed frame interval — ~60fps
    let mut frame_tick = tokio::time::interval(Duration::from_millis(16));
    frame_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        // Wait for next frame tick
        frame_tick.tick().await;

        // Drain ALL pending server events (non-blocking)
        while let Ok(ev) = event_rx.try_recv() {
            app.apply_server_event(ev);
        }

        // Drain ALL pending keyboard events (non-blocking)
        while event::poll(Duration::ZERO)? {
            if let Event::Key(key) = event::read()? {
                if let Some(cmd) = map_key(key.code, key.modifiers, &app.screen) {
                    if let Some(action) = app.apply_command(cmd) {
                        let _ = action_tx.send(action);
                    }
                }
            }
        }

        // Draw
        terminal.draw(|f| layout::render(f, app))?;

        if app.should_quit {
            return Ok(());
        }
    }
}

fn map_key(code: KeyCode, modifiers: KeyModifiers, screen: &Screen) -> Option<Command> {
    // Ctrl+C always quits
    if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
        return Some(Command::Quit);
    }

    match screen {
        Screen::Connecting | Screen::Disconnected => match code {
            KeyCode::Char('x') => Some(Command::Quit),
            _ => None,
        },

        Screen::CharacterSelect => match code {
            KeyCode::Up | KeyCode::Char('k') => Some(Command::CharSelectUp),
            KeyCode::Down | KeyCode::Char('j') => Some(Command::CharSelectDown),
            KeyCode::Enter => Some(Command::CharSelectConfirm),
            KeyCode::Char('c') => Some(Command::CharSelectCreate),
            KeyCode::Char('d') => Some(Command::CharSelectDelete),
            KeyCode::Char('x') => Some(Command::Quit),
            _ => None,
        },

        Screen::CharacterCreate => match code {
            KeyCode::Tab => Some(Command::CreateNextField),
            KeyCode::Enter => Some(Command::CreateConfirm),
            KeyCode::Esc => Some(Command::CreateCancel),
            KeyCode::Left => Some(Command::CreateCyclePrev),
            KeyCode::Right => Some(Command::CreateCycleNext),
            KeyCode::Backspace => Some(Command::CreateBackspace),
            KeyCode::Char(ch) if ch.is_alphanumeric() => Some(Command::CreateTypeChar(ch)),
            _ => None,
        },

        Screen::Playing => match code {
            // Movement - WASD
            KeyCode::Char('w') | KeyCode::Up => Some(Command::MoveStart(Direction::North)),
            KeyCode::Char('s') | KeyCode::Down => Some(Command::MoveStart(Direction::South)),
            KeyCode::Char('d') | KeyCode::Right => Some(Command::MoveStart(Direction::East)),
            KeyCode::Char('a') | KeyCode::Left => Some(Command::MoveStart(Direction::West)),
            // Diagonals
            KeyCode::Char('e') => Some(Command::MoveStart(Direction::NorthEast)),
            KeyCode::Char('q') => Some(Command::MoveStart(Direction::NorthWest)),
            // Stop
            KeyCode::Char(' ') => Some(Command::MoveStop),
            // Ping
            KeyCode::Char('p') => Some(Command::Ping),
            // Help
            KeyCode::Char('?') => Some(Command::ToggleHelp),
            // Logout
            KeyCode::Esc => Some(Command::Logout),
            // Quit
            KeyCode::Char('x') => Some(Command::Quit),
            // Log scrolling
            KeyCode::PageUp => Some(Command::ScrollLogUp),
            KeyCode::PageDown => Some(Command::ScrollLogDown),
            // Entity scrolling
            KeyCode::Char('[') => Some(Command::ScrollEntitiesUp),
            KeyCode::Char(']') => Some(Command::ScrollEntitiesDown),
            _ => None,
        },
    }
}
