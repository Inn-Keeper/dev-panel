//! All drawing. State lives in app.rs.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph, Row as TableRow, Sparkline, Table};

use crate::app::{App, Pane, RowKind};

const PANE_HEIGHT: u16 = 12;

pub fn draw(f: &mut Frame, app: &mut App) {
    let constraints = if app.pane.is_some() {
        vec![
            Constraint::Min(8),
            Constraint::Length(PANE_HEIGHT),
            Constraint::Length(1),
        ]
    } else {
        vec![Constraint::Min(8), Constraint::Length(1)]
    };
    let areas = Layout::vertical(constraints).split(f.area());
    draw_table(f, app, areas[0]);
    if app.pane.is_some() {
        draw_pane(f, app, areas[1]);
    }
    draw_footer(f, app, *areas.last().expect("footer area"));
    draw_modal(f, app);
}

fn status_style(status: &str) -> Style {
    let s = status.to_lowercase();
    if s == "up" || s == "listen" || s.starts_with("up ") {
        Style::new().fg(Color::Green)
    } else if s == "starting" || s == "restarting" {
        Style::new().fg(Color::Yellow)
    } else if s == "down" || s == "crashed" || s == "conflict" {
        Style::new().fg(Color::Red)
    } else {
        Style::new().dim()
    }
}

/// Width of the trailing restart-strategy column, click target for the mouse handler.
pub const RESTART_COL_WIDTH: u16 = 10;

fn draw_table(f: &mut Frame, app: &mut App, area: Rect) {
    let rows = app.rows.iter().map(|r| {
        TableRow::new(vec![
            Line::from(r.port.map(|p| p.to_string()).unwrap_or_else(|| "-".into())),
            Line::from(r.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into())),
            Line::from(r.display_name.clone()),
            Line::from(r.status.clone()).style(status_style(&r.status)),
            Line::from(r.user.clone()),
            Line::from(if r.cmd.is_empty() {
                "-".into()
            } else {
                r.cmd.clone()
            }),
            Line::from(format!("↻ {}", r.restart_label)).style(restart_style(&r.restart_label)),
        ])
    });
    let title = if app.filter.is_empty() {
        " dev-panel ".to_string()
    } else {
        format!(" dev-panel — filter: {} ", app.filter)
    };
    let table = Table::new(
        rows,
        [
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Length(20),
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Min(20),
            Constraint::Length(RESTART_COL_WIDTH),
        ],
    )
    .header(
        TableRow::new([
            "PORT", "PID", "NAME", "STATUS", "USER", "COMMAND", "RESTART",
        ])
        .bold(),
    )
    .row_highlight_style(Style::new().reversed())
    .block(Block::bordered().title(title));
    f.render_stateful_widget(table, area, &mut app.table);
    app.table_area = area;
}

fn restart_style(label: &str) -> Style {
    match label {
        "managed" | "configured" | "systemd" | "launchd" | "docker" => Style::new().fg(Color::Green),
        "shell" | "naive" => Style::new().fg(Color::Yellow),
        "unknown" => Style::new().fg(Color::Red),
        _ => Style::new().cyan(),
    }
}

fn draw_pane(f: &mut Frame, app: &mut App, area: Rect) {
    match app.pane {
        Some(Pane::Logs) => draw_logs(f, app, area),
        Some(Pane::Usage) => draw_usage(f, app, area),
        Some(Pane::EnvDiff) => {
            let text = app.env_diff_lines().join("\n");
            f.render_widget(
                Paragraph::new(text).block(Block::bordered().title(" env diff (v proc vs shell) ")),
                area,
            );
        }
        Some(Pane::Details) => {
            let title = format!(
                " details for selected row: {} (pane below table, not inline) ",
                app.selected_row()
                    .map(|r| r.display_name.as_str())
                    .unwrap_or("-")
            );
            let text = app.details_lines().join("\n");
            f.render_widget(
                Paragraph::new(text).block(Block::bordered().title(title)),
                area,
            );
        }
        Some(Pane::Events) => {
            let lines = app.event_lines();
            let text = if lines.is_empty() {
                "no events yet".to_string()
            } else {
                lines.join("\n")
            };
            f.render_widget(
                Paragraph::new(text).block(Block::bordered().title(" events (recent actions) ")),
                area,
            );
        }
        None => {}
    }
}

fn draw_logs(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::bordered().title(" logs (PgUp/PgDn scroll) ");
    let Some(RowKind::Service(i)) = app.selected_row().map(|r| r.kind.clone()) else {
        f.render_widget(
            Paragraph::new("logs are captured for managed services only").block(block),
            area,
        );
        return;
    };
    let svc = &app.manager.services[i];
    let visible = area.height.saturating_sub(2) as usize;
    let total = svc.logs.len();
    app.log_scroll = app.log_scroll.min(total.saturating_sub(visible));
    let from_top = total.saturating_sub(visible + app.log_scroll);
    // ponytail: rebuilds the tail every frame; cap is 1000 lines, fine.
    let text: Vec<Line> = svc
        .logs
        .iter()
        .skip(from_top)
        .take(visible)
        .map(|l| Line::from(l.as_str()))
        .collect();
    let title = format!(" logs: {} ({} lines) ", svc.cfg.name, total);
    f.render_widget(
        Paragraph::new(text).block(Block::bordered().title(title)),
        area,
    );
}

fn draw_usage(f: &mut Frame, app: &mut App, area: Rect) {
    let Some((hist, title)) = app.usage_data() else {
        f.render_widget(
            Paragraph::new("select a row with a pid").block(Block::bordered().title(" usage ")),
            area,
        );
        return;
    };
    let spark = Sparkline::default()
        .data(&hist)
        .style(Style::new().fg(Color::Cyan))
        .block(Block::bordered().title(format!(" {title} — cpu% last {}s ", hist.len() * 2)));
    f.render_widget(spark, area);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let line = if app.filter_editing {
        Line::from(format!(" /{}▌  Enter apply · Esc clear", app.filter)).bold()
    } else {
        Line::from(format!(
            " {}  |  k kill  r/click↻ restart  R stack-restart  K stack-stop  a adopt  s/S start-stack  l logs  m events  d details  u cpu  v env  o open  c/C copy  e edit  / filter  q quit",
            app.message
        ))
    };
    f.render_widget(line, area);
}

fn draw_modal(f: &mut Frame, app: &App) {
    let Some(lines) = app.pending_lines() else {
        return;
    };
    let height = (lines.len() as u16 + 3).min(f.area().height.saturating_sub(4));
    let width = 72u16.min(f.area().width.saturating_sub(4));
    let area = centered(f.area(), width, height);
    f.render_widget(Clear, area);
    let mut text: Vec<Line> = lines.into_iter().map(Line::from).collect();
    text.push(Line::from(""));
    text.push(Line::from("y = confirm · a = adopt (restart) · Esc = cancel").dim());
    f.render_widget(
        Paragraph::new(text).block(Block::bordered().title(" Confirm ")),
        area,
    );
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w.min(area.width),
        height: h.min(area.height),
    }
}
