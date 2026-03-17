use ratatui::prelude::*;
use ratatui::symbols::Marker;
use ratatui::widgets::{Block, Borders};
use ratatui::widgets::canvas::{Canvas, Context, Line as CanvasLine, Points};

use crate::app::ClientApp;

/// View radius in world units. Smaller = more zoomed in = smoother apparent movement.
const VIEW_RADIUS: f64 = 80.0;

/// Size of the crosshair arms at the origin (world units).
const CROSSHAIR_SIZE: f64 = 1.5;

/// How many extra braille dots to draw per entity to make them more visible.
/// Draws a small cross pattern around each entity point.
const ENTITY_DOT_SPREAD: f64 = 0.4;

/// Offset (in world units) for the player label so it doesn't overlap the dot.
const LABEL_OFFSET_X: f64 = 1.2;
const LABEL_OFFSET_Y: f64 = 0.8;

pub fn entity_color(kind: common::EntityKind, is_self: bool) -> Color {
    if is_self {
        Color::Green
    } else {
        match kind {
            common::EntityKind::Player => Color::Cyan,
            common::EntityKind::Mob => Color::Red,
            common::EntityKind::Npc => Color::Yellow,
            common::EntityKind::GameObject => Color::Gray,
        }
    }
}

pub fn render(frame: &mut Frame, area: Rect, app: &ClientApp) {
    let block = Block::default()
        .title(" World Map ")
        .borders(Borders::ALL);

    let cx = app.my_position.x as f64;
    let cy = app.my_position.z as f64; // z is the "forward" axis in xz plane

    let x_min = cx - VIEW_RADIUS;
    let x_max = cx + VIEW_RADIUS;
    let y_min = cy - VIEW_RADIUS;
    let y_max = cy + VIEW_RADIUS;

    // Pre-group entity positions by color for batched Points drawing.
    // Each entity gets a small cross pattern (5 dots) to be more visible.
    let mut green_pts: Vec<(f64, f64)> = Vec::new();
    let mut cyan_pts: Vec<(f64, f64)> = Vec::new();
    let mut red_pts: Vec<(f64, f64)> = Vec::new();
    let mut yellow_pts: Vec<(f64, f64)> = Vec::new();
    let mut gray_pts: Vec<(f64, f64)> = Vec::new();

    for entity in &app.entities {
        let is_self = app.entity_id == Some(entity.entity_id);
        let ex = entity.position.x as f64;
        let ey = entity.position.z as f64;
        let s = ENTITY_DOT_SPREAD;

        // Cross pattern: center + 4 arms
        let pts = [
            (ex, ey),
            (ex - s, ey),
            (ex + s, ey),
            (ex, ey - s),
            (ex, ey + s),
        ];

        let bucket = match entity_color(entity.kind, is_self) {
            Color::Green => &mut green_pts,
            Color::Cyan => &mut cyan_pts,
            Color::Red => &mut red_pts,
            Color::Yellow => &mut yellow_pts,
            _ => &mut gray_pts,
        };
        bucket.extend_from_slice(&pts);
    }

    // Player label — for self entity, shown on the map
    let player_label: Option<String> = app.default_name.clone();

    // Collect nearby player names + positions for map labels
    let nearby_labels: Vec<(f64, f64, String, Color)> = app
        .entities
        .iter()
        .filter_map(|entity| {
            let is_self = app.entity_id == Some(entity.entity_id);
            if is_self {
                return None; // self label handled separately
            }
            let name = entity.name.as_deref()?;
            let ex = entity.position.x as f64;
            let ey = entity.position.z as f64;
            let color = entity_color(entity.kind, false);
            Some((ex, ey, name.to_string(), color))
        })
        .collect();

    let canvas = Canvas::default()
        .block(block)
        .marker(Marker::Braille)
        .x_bounds([x_min, x_max])
        .y_bounds([y_min, y_max])
        .paint(move |ctx: &mut Context| {
            // Layer 1: Origin crosshair — all sub-cell precision, no text
            let origin_visible = x_min < CROSSHAIR_SIZE
                && -CROSSHAIR_SIZE < x_max
                && y_min < CROSSHAIR_SIZE
                && -CROSSHAIR_SIZE < y_max;
            if origin_visible {
                ctx.draw(&CanvasLine {
                    x1: -CROSSHAIR_SIZE,
                    y1: 0.0,
                    x2: CROSSHAIR_SIZE,
                    y2: 0.0,
                    color: Color::DarkGray,
                });
                ctx.draw(&CanvasLine {
                    x1: 0.0,
                    y1: -CROSSHAIR_SIZE,
                    x2: 0.0,
                    y2: CROSSHAIR_SIZE,
                    color: Color::DarkGray,
                });
            }

            // Layer 2: Entity dots — drawn via Points for sub-cell precision
            let color_groups: &[(&[(f64, f64)], Color)] = &[
                (&gray_pts, Color::Gray),
                (&cyan_pts, Color::Cyan),
                (&red_pts, Color::Red),
                (&yellow_pts, Color::Yellow),
                (&green_pts, Color::Green), // player on top
            ];
            for &(coords, color) in color_groups {
                if !coords.is_empty() {
                    ctx.draw(&Points { coords, color });
                }
            }

            // Layer 3: Player name label (always at center since map follows player)
            if let Some(ref name) = player_label {
                ctx.print(
                    cx + LABEL_OFFSET_X,
                    cy + LABEL_OFFSET_Y,
                    Span::styled(
                        name.clone(),
                        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                    ),
                );
            }

            // Layer 4: Nearby entity name labels
            for (ex, ey, ref name, color) in &nearby_labels {
                ctx.print(
                    ex + LABEL_OFFSET_X,
                    ey + LABEL_OFFSET_Y,
                    Span::styled(name.clone(), Style::default().fg(*color)),
                );
            }
        });

    frame.render_widget(canvas, area);
}
