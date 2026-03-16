use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::command::COMMAND_REGISTRY;

pub fn render(frame: &mut Frame, area: Rect) {
    // Center the help overlay
    let width = 60u16.min(area.width.saturating_sub(4));
    let height = (COMMAND_REGISTRY.len() as u16 + 6).min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let popup_area = Rect::new(x, y, width, height);

    // Clear the area behind the popup
    frame.render_widget(Clear, popup_area);

    let mut lines = vec![
        Line::from("Keybindings").style(Style::default().add_modifier(Modifier::BOLD)),
        Line::from(""),
        Line::from(" W/Up     Move north       E  Move NE"),
        Line::from(" S/Down   Move south       Q  Move NW"),
        Line::from(" D/Right  Move east"),
        Line::from(" A/Left   Move west"),
        Line::from(" Space    Stop moving"),
        Line::from(" P        Send ping"),
        Line::from(" ?        Toggle this help"),
        Line::from(" X/Ctrl+C Quit"),
        Line::from(" PgUp/Dn  Scroll log"),
        Line::from(" [ / ]    Scroll entities"),
        Line::from(""),
        Line::from("Script mode commands:").style(Style::default().add_modifier(Modifier::BOLD)),
        Line::from(""),
    ];

    for cmd in COMMAND_REGISTRY {
        lines.push(Line::from(format!(" {:<30} {}", cmd.syntax, cmd.description)));
    }

    let block = Block::default()
        .title(" Help ")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black));

    let widget = Paragraph::new(lines).block(block);
    frame.render_widget(widget, popup_area);
}
