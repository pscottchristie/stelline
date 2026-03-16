use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

use crate::app::ClientApp;

/// Render the character select screen.
pub fn render(frame: &mut Frame, area: Rect, app: &ClientApp) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),  // header
            Constraint::Min(4),    // character list
            Constraint::Length(2), // footer
        ])
        .split(area);

    // ── Header ──
    let header = Paragraph::new(" Stelline — Character Select")
        .style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_widget(header, outer[0]);

    // ── Character list (or empty message) ──
    if app.characters.is_empty() {
        let msg = Paragraph::new("\n  No characters yet. Press C to create one.")
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::ALL));
        frame.render_widget(msg, outer[1]);
    } else {
        let items: Vec<ListItem> = app
            .characters
            .iter()
            .enumerate()
            .map(|(i, ch)| {
                let marker = if i == app.char_select_index {
                    ">"
                } else {
                    " "
                };
                let line = format!(
                    "{marker} {:<12} Lv {:>2}  {:<6} {}",
                    ch.name, ch.level, format!("{}", ch.race), ch.class
                );
                let style = if i == app.char_select_index {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                ListItem::new(line).style(style)
            })
            .collect();

        let list = List::new(items).block(Block::default().borders(Borders::ALL));
        frame.render_widget(list, outer[1]);

        // Delete confirm overlay — render inside the list area at the bottom
        if app.delete_confirm {
            if let Some(ch) = app.characters.get(app.char_select_index) {
                let confirm_text = format!("  Delete {}? Press D again to confirm", ch.name);
                let confirm = Paragraph::new(confirm_text)
                    .style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD));
                // Position at the bottom of the list area
                let confirm_area = Rect {
                    x: outer[1].x + 1,
                    y: outer[1].y + outer[1].height.saturating_sub(2),
                    width: outer[1].width.saturating_sub(2),
                    height: 1,
                };
                frame.render_widget(confirm, confirm_area);
            }
        }
    }

    // ── Footer ──
    let footer_text = " ↑↓/JK=select  Enter=play  C=create  D=delete  X=quit";
    let footer = Paragraph::new(footer_text).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, outer[2]);
}
