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
    pub selection: Option<((u16, u16), (u16, u16))>,
}

impl<'a> TerminalWidget<'a> {
    pub fn new(state: &'a WindowState) -> Self {
        Self {
            state,
            selection: None,
        }
    }

    pub fn with_selection(mut self, selection: Option<((u16, u16), (u16, u16))>) -> Self {
        self.selection = selection;
        self
    }
}

impl<'a> Widget for TerminalWidget<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let win = &self.state;
        let visible_height = area.height as usize;
        let visible_width = area.width as usize;
        let full_width = win.width.saturating_sub(3) as usize;

        // Calculate where the visible area is relative to the terminal's top-left (1,1 inside border)
        let col_offset = area.x.saturating_sub(win.x + 1) as usize;
        let row_offset = area.y.saturating_sub(win.y + 1) as usize;

        for row in 0..visible_height {
            for col in 0..visible_width {
                let x = area.x + col as u16;
                let y = area.y + row as u16;

                let idx = (row + row_offset) * full_width + (col + col_offset);
                let cell = win.screen.get(idx).copied().unwrap_or(Cell::default());

                let mut style = Style::default()
                    .fg(Color::Rgb(cell.fg.0, cell.fg.1, cell.fg.2))
                    .bg(Color::Rgb(cell.bg.0, cell.bg.1, cell.bg.2));

                // Apply selection highlighting
                if let Some((start, end)) = self.selection {
                    let r = row as u16 + row_offset as u16;
                    let c = col as u16 + col_offset as u16;
                    let (r1, c1) = (start.0.min(end.0), start.1.min(end.1));
                    let (r2, c2) = (start.0.max(end.0), start.1.max(end.1));

                    let is_selected = if r > r1 && r < r2 {
                        true
                    } else if r == r1 && r == r2 {
                        c >= c1 && c <= c2
                    } else if r == r1 {
                        c >= c1
                    } else if r == r2 {
                        c <= c2
                    } else {
                        false
                    };

                    if is_selected {
                        style = style.add_modifier(Modifier::REVERSED);
                    }
                }

                style = if cell.bold() {
                    style.add_modifier(Modifier::BOLD)
                } else {
                    style
                };

                style = if cell.italic() {
                    style.add_modifier(Modifier::ITALIC)
                } else {
                    style
                };

                style = if cell.underline() {
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
