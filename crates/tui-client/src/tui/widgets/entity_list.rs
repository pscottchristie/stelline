use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Row, Table};

use crate::app::ClientApp;
use super::world_map::entity_color;

pub fn render(frame: &mut Frame, area: Rect, app: &ClientApp) {
    let block = Block::default()
        .title(" Entities (AOI) ")
        .borders(Borders::ALL);

    let header = Row::new(vec!["", "Type", "HP", "Dist"])
        .style(Style::default().add_modifier(Modifier::BOLD))
        .bottom_margin(0);

    let my_pos = app.my_position;

    let rows: Vec<Row> = app
        .entities
        .iter()
        .map(|e| {
            let is_self = app.entity_id == Some(e.entity_id);
            let symbol = ClientApp::entity_symbol(e.kind, is_self).to_string();
            let label = if let Some(ref name) = e.name {
                if is_self {
                    format!("{} (You)", name)
                } else {
                    name.clone()
                }
            } else {
                ClientApp::entity_label(e.kind, is_self).to_string()
            };
            let hp = format!("{}/{}", e.health, e.max_health);
            let dist = if is_self {
                "---".to_string()
            } else {
                format!("{:.0}", my_pos.distance(e.position))
            };

            let color = entity_color(e.kind, is_self);
            let style = if is_self {
                Style::default().fg(color).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(color)
            };

            Row::new(vec![
                format!("[{symbol}]"),
                label.to_string(),
                hp,
                dist,
            ])
            .style(style)
        })
        .collect();

    let widths = [
        Constraint::Length(3),
        Constraint::Min(6),
        Constraint::Length(9),
        Constraint::Length(5),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(block);

    frame.render_widget(table, area);
}
