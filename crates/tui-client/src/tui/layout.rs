use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::app::{ClientApp, Screen};
use super::widgets;

/// Top-level render — dispatches by screen.
pub fn render(frame: &mut Frame, app: &ClientApp) {
    match app.screen {
        Screen::Connecting => render_connecting(frame),
        Screen::CharacterSelect => render_character_select(frame, app),
        Screen::CharacterCreate => render_character_create(frame, app),
        Screen::Playing => render_playing(frame, app),
        Screen::Disconnected => render_disconnected(frame, app),
    }
}

fn render_connecting(frame: &mut Frame) {
    let area = frame.area();
    let text = "Connecting to server...";
    let paragraph = Paragraph::new(text)
        .style(Style::default().fg(Color::Yellow))
        .alignment(Alignment::Center);
    let y = area.height / 2;
    let centered = Rect {
        x: area.x,
        y,
        width: area.width,
        height: 1,
    };
    frame.render_widget(paragraph, centered);
}

fn render_disconnected(frame: &mut Frame, app: &ClientApp) {
    let area = frame.area();
    let reason = app
        .log
        .back()
        .map(|e| e.message.as_str())
        .unwrap_or("Connection lost");
    let text = format!("{reason}\n\nPress X to quit");
    let paragraph = Paragraph::new(text)
        .style(Style::default().fg(Color::Red))
        .alignment(Alignment::Center);
    let y = area.height / 2;
    let centered = Rect {
        x: area.x,
        y: y.saturating_sub(1),
        width: area.width,
        height: 3,
    };
    frame.render_widget(paragraph, centered);
}

fn render_character_select(frame: &mut Frame, app: &ClientApp) {
    widgets::character_select::render(frame, frame.area(), app);
}

fn render_character_create(frame: &mut Frame, app: &ClientApp) {
    widgets::character_create::render(frame, frame.area(), app);
}

fn render_playing(frame: &mut Frame, app: &ClientApp) {
    let area = frame.area();

    // Header (1 line) + main area + log (6 lines) + footer (1 line)
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),  // header
            Constraint::Min(8),     // main
            Constraint::Length(8),  // log
            Constraint::Length(1),  // footer
        ])
        .split(area);

    // ── Header ──
    widgets::stats::render_header(frame, outer[0], app);

    // ── Main area: adapt based on width ──
    if area.width >= 80 {
        // Full layout: map + right panel
        let map_pct = if area.width >= 100 { 60 } else { 55 };
        let main_cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(map_pct),
                Constraint::Percentage(100 - map_pct),
            ])
            .split(outer[1]);

        // Left: world map
        widgets::world_map::render(frame, main_cols[0], app);

        // Right: entities + stats
        let right_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(60),
                Constraint::Percentage(40),
            ])
            .split(main_cols[1]);

        widgets::entity_list::render(frame, right_rows[0], app);
        widgets::stats::render_stats(frame, right_rows[1], app);
    } else {
        // Narrow: map on top, stats below, no entity list
        let main_rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(70),
                Constraint::Percentage(30),
            ])
            .split(outer[1]);

        widgets::world_map::render(frame, main_rows[0], app);
        widgets::stats::render_stats(frame, main_rows[1], app);
    }

    // ── Log ──
    widgets::log::render(frame, outer[2], app);

    // ── Footer ──
    let footer_text = "WASD=move  Space=stop  P=ping  ?=help  Esc=logout  X=quit";
    let footer = Paragraph::new(footer_text)
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, outer[3]);

    // ── Help overlay ──
    if app.show_help {
        widgets::help::render(frame, area);
    }
}
