use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{ClientApp, Screen};

/// Render the top header bar (1 line).
pub fn render_header(frame: &mut Frame, area: Rect, app: &ClientApp) {
    let zone_str = app
        .zone_id
        .map(|z| format!("Zone {}", z.get()))
        .unwrap_or_else(|| "---".to_string());

    let rtt_str = app
        .rtt_ms
        .map(|r| format!("RTT: {}ms", r))
        .unwrap_or_else(|| "RTT: ---".to_string());

    let conn_str = match app.screen {
        Screen::Connecting => " [Connecting...]",
        Screen::Playing => "",
        Screen::Disconnected => " [Disconnected]",
        Screen::CharacterSelect | Screen::CharacterCreate => " [Character Select]",
    };

    let header = format!(" Stelline TUI          {zone_str}{conn_str}      {rtt_str} ");

    let widget = Paragraph::new(header)
        .style(Style::default().bg(Color::Blue).fg(Color::White).add_modifier(Modifier::BOLD));
    frame.render_widget(widget, area);
}

/// Render the stats panel.
pub fn render_stats(frame: &mut Frame, area: Rect, app: &ClientApp) {
    let pos = app.my_position;
    let move_str = app.move_state_str();

    let health_str = if app.my_max_health > 0 {
        format!("{}/{}", app.my_health, app.my_max_health)
    } else {
        "---".to_string()
    };

    let text = vec![
        Line::from(format!(" Pos: ({:.1}, {:.1}, {:.1})", pos.x, pos.y, pos.z)),
        Line::from(format!(" Moving: {move_str}")),
        Line::from(format!(" Health: {health_str}")),
        Line::from(format!(" Tick: {}  Snapshots: {}", app.last_tick, app.snapshot_count)),
    ];

    let block = Block::default().title(" Stats ").borders(Borders::ALL);
    let widget = Paragraph::new(text).block(block);
    frame.render_widget(widget, area);
}
