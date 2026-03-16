use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::ClientApp;

pub fn render(frame: &mut Frame, area: Rect, app: &ClientApp) {
    let block = Block::default()
        .title(" Log (PgUp/PgDn to scroll) ")
        .borders(Borders::ALL);

    let inner_height = area.height.saturating_sub(2) as usize; // minus borders
    let total = app.log.len();

    // log_scroll=0 means "show latest", >0 means scroll up from bottom
    let end = total.saturating_sub(app.log_scroll);
    let start = end.saturating_sub(inner_height);

    let start_time = app
        .log
        .front()
        .map(|e| e.timestamp)
        .unwrap_or_else(std::time::Instant::now);

    let lines: Vec<Line> = app
        .log
        .iter()
        .skip(start)
        .take(end - start)
        .map(|entry| {
            let secs = entry.timestamp.duration_since(start_time).as_secs();
            let mins = secs / 60;
            let s = secs % 60;
            Line::from(format!(" [{:02}:{:02}] {}", mins, s, entry.message))
                .style(Style::default().fg(Color::Gray))
        })
        .collect();

    let widget = Paragraph::new(lines).block(block);
    frame.render_widget(widget, area);
}
