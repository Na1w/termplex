use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::Widget,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use vt100::Parser;

pub struct TerminalWidget {
    pub parser: Arc<Mutex<Parser>>,
    pub running: Arc<AtomicBool>,
    pub exit_code: Arc<Mutex<Option<i32>>>,
    pub scroll_offset: usize,
}

impl Widget for TerminalWidget {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let is_running = self.running.load(Ordering::SeqCst);
        let mut parser = self.parser.lock().unwrap();

        parser.screen_mut().set_scrollback(self.scroll_offset);
        let screen = parser.screen();

        for row in 0..area.height {
            for col in 0..area.width {
                let x = area.x + col;
                let y = area.y + row;
                if !buf.area.contains((x, y).into()) {
                    continue;
                }

                if let Some(cell) = screen.cell(row, col) {
                    let mut symbol = cell.contents();
                    if symbol.is_empty() {
                        symbol = " ";
                    }

                    let mut style = Style::default().bg(Color::Black).fg(Color::Gray);

                    let fg = convert_color(cell.fgcolor());
                    if fg != Color::Reset {
                        style = style.fg(fg);
                    }
                    let bg = convert_color(cell.bgcolor());
                    if bg != Color::Reset {
                        style = style.bg(bg);
                    }

                    if cell.bold() {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    if cell.italic() {
                        style = style.add_modifier(Modifier::ITALIC);
                    }
                    if cell.underline() {
                        style = style.add_modifier(Modifier::UNDERLINED);
                    }

                    buf[(x, y)].set_symbol(symbol).set_style(style);
                } else {
                    // Fill with background spaces if beyond vt100 bounds
                    buf[(x, y)]
                        .set_symbol(" ")
                        .set_style(Style::default().bg(Color::Black));
                }
            }
        }

        // Show exit status overlay
        if !is_running {
            let code = *self.exit_code.lock().unwrap();
            let msg = format!(" [ PROCESS EXITED WITH CODE {:?} ] ", code.unwrap_or(0));
            let x = area.x + (area.width.saturating_sub(msg.len() as u16) / 2);
            let y = area.y + (area.height / 2);

            if y < area.bottom() {
                for (i, c) in msg.chars().enumerate() {
                    let cx = x + i as u16;
                    if cx < area.right() {
                        buf[(cx, y)].set_char(c).set_style(
                            Style::default()
                                .fg(Color::White)
                                .bg(Color::Red)
                                .add_modifier(Modifier::BOLD),
                        );
                    }
                }
            }
        }

        // Render cursor if it's within bounds and process is still running and we are not scrolled up
        if is_running && !screen.hide_cursor() && self.scroll_offset == 0 {
            let (cursor_row, cursor_col) = screen.cursor_position();
            let x = area.x + cursor_col;
            let y = area.y + cursor_row;
            if x < area.right() && y < area.bottom() && buf.area.contains((x, y).into()) {
                let style = buf[(x, y)].style();
                buf[(x, y)].set_style(style.add_modifier(Modifier::REVERSED));
            }
        }

        parser.screen_mut().set_scrollback(0);
    }
}

fn convert_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}
