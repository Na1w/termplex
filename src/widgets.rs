use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::Widget,
};

use crate::protocol::{Cell, WindowState};

/// Widget that renders a terminal window from server data
pub struct TerminalWidget<'a> {
    pub state: &'a WindowState,
}

impl<'a> TerminalWidget<'a> {
    pub fn new(state: &'a WindowState) -> Self {
        Self { state }
    }
}

impl<'a> Widget for TerminalWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let win = &self.state;
        let visible_height = area.height as usize;
        let visible_width = area.width as usize;
        let full_width = win.width.saturating_sub(2) as usize;

        // Calculate where the visible area is relative to the terminal's top-left (1,1 inside border)
        let col_offset = area.x.saturating_sub(win.x + 1) as usize;
        let row_offset = area.y.saturating_sub(win.y + 1) as usize;

        for row in 0..visible_height {
            for col in 0..visible_width {
                let x = area.x + col as u16;
                let y = area.y + row as u16;

                let idx = (row + row_offset) * full_width + (col + col_offset);
                let cell = win.screen.get(idx).copied().unwrap_or(Cell::default());

                let style = Style::default()
                    .fg(Color::Rgb(cell.fg.0, cell.fg.1, cell.fg.2))
                    .bg(Color::Rgb(cell.bg.0, cell.bg.1, cell.bg.2));

                let style = if cell.bold() {
                    style.add_modifier(Modifier::BOLD)
                } else {
                    style
                };

                let style = if cell.italic() {
                    style.add_modifier(Modifier::ITALIC)
                } else {
                    style
                };

                let style = if cell.underline() {
                    style.add_modifier(Modifier::UNDERLINED)
                } else {
                    style
                };

                let mut char_buf = [0u8; 4];
                buf[(x, y)]
                    .set_symbol(cell.ch.encode_utf8(&mut char_buf))
                    .set_style(style);
            }
        }

        // Show exit status overlay
        if !win.running {
            let msg = format!(
                " [ PROCESS EXITED WITH CODE {:?} ] ",
                win.exit_code.unwrap_or(0)
            );
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

        // Render cursor if visible
        if let Some((cursor_row, cursor_col)) = win.cursor_pos
            && win.cursor_visible
        {
            let x = (win.x + 1) + cursor_col;
            let y = (win.y + 1) + cursor_row;
            if area.contains((x, y).into()) {
                let style = buf[(x, y)].style();
                buf[(x, y)].set_style(style.add_modifier(Modifier::REVERSED));
            }
        }
    }
}
