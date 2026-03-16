use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::ClientApp;

/// Render the character creation screen.
pub fn render(frame: &mut Frame, area: Rect, app: &ClientApp) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),  // header
            Constraint::Min(6),    // form
            Constraint::Length(2), // footer
        ])
        .split(area);

    // ── Header ──
    let header = Paragraph::new(" Stelline — Create Character")
        .style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_widget(header, outer[0]);

    // ── Form ──
    let focus = app.create_field_focus;

    let name_style = if focus == 0 {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let race_style = if focus == 1 {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let class_style = if focus == 2 {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    let cursor = if focus == 0 { "_" } else { "" };
    let name_display = format!("{}{cursor}", app.create_name);

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  Name:  ["),
            Span::styled(&name_display, name_style),
            Span::raw("]"),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw("  Race:  "),
            Span::styled(
                format!("< {} >", app.create_race()),
                race_style,
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw("  Class: "),
            Span::styled(
                format!("< {} >", app.create_class()),
                class_style,
            ),
        ]),
    ];

    // Error message
    if let Some(ref err) = app.create_error {
        lines.push(Line::from(""));
        lines.push(Line::from(
            Span::styled(
                format!("  {err}"),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        ));
    }

    let form = Paragraph::new(lines).block(Block::default().borders(Borders::ALL));
    frame.render_widget(form, outer[1]);

    // ── Footer ──
    let footer_text = " Tab=next field  <-/->= cycle  Enter=create  Esc=cancel";
    let footer = Paragraph::new(footer_text).style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, outer[2]);
}
