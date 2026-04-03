use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
        MouseButton, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Clear, Paragraph},
};
use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::interval;

use crate::protocol::*;
use crate::widgets::TerminalWidget;

#[derive(Debug)]
enum AppEvent {
    Terminal(Event),
    Server(ServerMessage),
}

#[derive(PartialEq, Eq, Debug, Clone, Copy)]
enum Mode {
    Terminal,
    Desktop,
}

struct DragState {
    window_id: usize,
    start_mouse: (u16, u16),
    start_rect: Rect,
    is_resize: bool,
    last_update: std::time::Instant,
}

struct RenameState {
    window_id: usize,
    input: String,
}

struct Client {
    windows: HashMap<usize, WindowState>,
    active_window_id: Option<usize>,
    mode: Mode,
    drag_state: Option<DragState>,
    rename_state: Option<RenameState>,
    last_screen_size: Rect,
    last_mouse_pos: (u16, u16),
    last_click: Option<(usize, std::time::Instant)>,
    server_tx: mpsc::Sender<ClientMessage>,
}

impl Client {
    fn new(screen_size: Rect, server_tx: mpsc::Sender<ClientMessage>) -> Self {
        Self {
            windows: HashMap::new(),
            active_window_id: None,
            mode: Mode::Terminal,
            drag_state: None,
            rename_state: None,
            last_screen_size: screen_size,
            last_mouse_pos: (0, 0),
            last_click: None,
            server_tx,
        }
    }

    fn get_window_at(&self, x: u16, y: u16) -> Option<(usize, bool)> {
        // Sort by z-order (front to back)
        let mut windows_with_z: Vec<_> = self.windows.values().collect();
        windows_with_z.sort_by(|a, b| b.z_order.cmp(&a.z_order));

        for win in windows_with_z {
            let rect = if win.minimized {
                Rect::new(win.x, win.y, win.width, 1)
            } else {
                Rect::new(win.x, win.y, win.width, win.height)
            };

            if x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height {
                let is_terminal_area = !win.minimized
                    && x > win.x
                    && x < win.x + win.width - 1
                    && y > win.y
                    && y < win.y + win.height - 1;
                return Some((win.id, is_terminal_area));
            }
        }
        None
    }

    fn handle_event(&mut self, event: AppEvent) -> Result<bool> {
        match event {
            AppEvent::Terminal(ev) => match ev {
                Event::Resize(w, h) => {
                    self.last_screen_size = Rect::new(0, 0, w, h);
                    let msg = ClientMessage::TerminalResize {
                        width: w,
                        height: h,
                    };
                    let _ = self.server_tx.try_send(msg);
                }
                Event::Key(key) => {
                    if key.kind != event::KeyEventKind::Press {
                        return Ok(false);
                    }

                    // Handle renaming mode
                    if let Some(ref mut rs) = self.rename_state {
                        match key.code {
                            KeyCode::Enter => {
                                let _ = self.server_tx.try_send(ClientMessage::RenameWindow {
                                    window_id: rs.window_id,
                                    title: rs.input.clone(),
                                });
                                self.rename_state = None;
                            }
                            KeyCode::Esc => {
                                self.rename_state = None;
                            }
                            KeyCode::Backspace => {
                                rs.input.pop();
                            }
                            KeyCode::Char(c) => {
                                rs.input.push(c);
                            }
                            _ => {}
                        }
                        return Ok(false);
                    }

                    // Hover-based redirection:
                    // If mouse is inside ANY window's terminal area (excluding borders),
                    // send ALL keys directly to that window unconditionally.
                    let (mx, my) = self.last_mouse_pos;
                    if let Some((id, true)) = self.get_window_at(mx, my) {
                        return self.send_key_to_window(id, key);
                    }

                    if key.code == KeyCode::F(12) {
                        // Toggle mode normally
                        self.mode = if self.mode == Mode::Terminal {
                            Mode::Desktop
                        } else {
                            Mode::Terminal
                        };
                    } else if self.mode == Mode::Desktop {
                        return self.handle_desktop_key(key);
                    } else {
                        // Default to active window if not hovering over anything specific
                        if let Some(id) = self.active_window_id {
                            return self.send_key_to_window(id, key);
                        }
                    }
                }
                Event::Mouse(mouse) => {
                    return self.handle_mouse(mouse);
                }
                _ => {}
            },
            AppEvent::Server(msg) => match msg {
                ServerMessage::Welcome { windows, .. } => {
                    self.windows.clear();
                    self.active_window_id = None;
                    for win in windows {
                        if win.focused {
                            self.active_window_id = Some(win.id);
                        }
                        self.windows.insert(win.id, win);
                    }
                }
                ServerMessage::FullSync { windows } => {
                    self.windows.clear();
                    self.active_window_id = None;
                    for win in windows {
                        if win.focused {
                            self.active_window_id = Some(win.id);
                        }
                        self.windows.insert(win.id, win);
                    }
                }
                ServerMessage::WindowCreated { window } => {
                    let window_id = window.id;
                    let is_focused = window.focused;
                    self.windows.insert(window_id, window);
                    if is_focused {
                        self.active_window_id = Some(window_id);
                    }
                }
                ServerMessage::WindowUpdate { window } => {
                    if window.focused {
                        self.active_window_id = Some(window.id);
                    }
                    self.windows.insert(window.id, window);
                }
                ServerMessage::WindowClosed { window_id } => {
                    self.windows.remove(&window_id);
                    // active_window_id will be updated by the following FullSync from server
                }
                ServerMessage::ScreenDiff {
                    window_id,
                    cells,
                    cursor_pos,
                } => {
                    if let Some(win) = self.windows.get_mut(&window_id) {
                        for (idx, cell) in cells {
                            if idx < win.screen.len() {
                                win.screen[idx] = cell;
                            }
                        }
                        win.cursor_pos = cursor_pos;
                    }
                }
                ServerMessage::PaneCaptured { window_id, text } => {
                    let timestamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let filename = format!("capture_window_{}_{}.txt", window_id, timestamp);
                    let _ = std::fs::write(&filename, text);
                }
                ServerMessage::FullCaptured { text } => {
                    let timestamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let filename = format!("capture_full_{}.txt", timestamp);
                    let _ = std::fs::write(filename, text);
                }
                ServerMessage::Shutdown => {
                    return Ok(true);
                }
                _ => {}
            },
        }
        Ok(false)
    }

    fn handle_desktop_key(&mut self, key: KeyEvent) -> Result<bool> {
        match (key.modifiers, key.code) {
            (KeyModifiers::NONE, KeyCode::Char('q')) => {
                return Ok(true);
            }
            (KeyModifiers::NONE, KeyCode::Char('n')) => {
                // Calculate window size based on screen size
                let screen = self.last_screen_size;
                let width = (screen.width.saturating_sub(4)).clamp(20, 80);
                let height = (screen.height.saturating_sub(6)).clamp(5, 24);
                let x = (screen.width.saturating_sub(width)) / 2;
                let y = (screen.height.saturating_sub(height)) / 2;
                let msg = ClientMessage::CreateWindow {
                    x,
                    y,
                    width,
                    height,
                    command: None,
                    args: vec![],
                };
                let _ = self.server_tx.try_send(msg);
            }
            (KeyModifiers::NONE, KeyCode::Char('o')) => {
                let _ = self.server_tx.try_send(ClientMessage::SaveLayout {
                    path: "layout.json".to_string(),
                });
            }
            (KeyModifiers::NONE, KeyCode::Char('i')) => {
                let _ = self.server_tx.try_send(ClientMessage::LoadLayout {
                    path: "layout.json".to_string(),
                });
            }
            (KeyModifiers::NONE, KeyCode::Char('c')) => {
                if let Some(id) = self.active_window_id {
                    let _ = self
                        .server_tx
                        .try_send(ClientMessage::MaximizeWindow { window_id: id });
                }
            }
            (KeyModifiers::NONE, KeyCode::Char('f')) => {
                if let Some(id) = self.active_window_id {
                    let _ = self
                        .server_tx
                        .try_send(ClientMessage::ToggleFullscreen { window_id: id });
                }
            }
            (KeyModifiers::NONE, KeyCode::Char('v')) => {
                if let Some(id) = self.active_window_id {
                    let _ = self
                        .server_tx
                        .try_send(ClientMessage::CapturePane { window_id: id });
                }
            }
            (KeyModifiers::NONE, KeyCode::Char('p')) => {
                let _ = self.server_tx.try_send(ClientMessage::CaptureFull);
            }

            (KeyModifiers::NONE, KeyCode::Tab) => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    // Prev window
                    let ids: Vec<_> = self.windows.keys().copied().collect();
                    if let Some(active) = self.active_window_id
                        && let Some(pos) = ids.iter().position(|&id| id == active)
                    {
                        let new_pos = if pos == 0 { ids.len() - 1 } else { pos - 1 };
                        self.active_window_id = Some(ids[new_pos]);
                        let _ = self.server_tx.try_send(ClientMessage::FocusWindow {
                            window_id: ids[new_pos],
                        });
                    }
                } else {
                    // Next window
                    let ids: Vec<_> = self.windows.keys().copied().collect();
                    if let Some(active) = self.active_window_id
                        && let Some(pos) = ids.iter().position(|&id| id == active)
                    {
                        let new_pos = (pos + 1) % ids.len();
                        self.active_window_id = Some(ids[new_pos]);
                        let _ = self.server_tx.try_send(ClientMessage::FocusWindow {
                            window_id: ids[new_pos],
                        });
                    }
                }
            }
            _ => {
                if let Some(id) = self.active_window_id {
                    match key.code {
                        KeyCode::Char('z') => {
                            let _ = self
                                .server_tx
                                .try_send(ClientMessage::CloseWindow { window_id: id });
                        }
                        KeyCode::Char('x') => {
                            let _ = self
                                .server_tx
                                .try_send(ClientMessage::MinimizeWindow { window_id: id });
                        }
                        KeyCode::Char('c') => {
                            let _ = self
                                .server_tx
                                .try_send(ClientMessage::MaximizeWindow { window_id: id });
                        }
                        KeyCode::Char('f') => {
                            let _ = self
                                .server_tx
                                .try_send(ClientMessage::ToggleFullscreen { window_id: id });
                        }
                        KeyCode::Left => {
                            if let Some(win) = self.windows.get(&id) {
                                let _ = self.server_tx.try_send(ClientMessage::MoveWindow {
                                    window_id: id,
                                    x: win.x.saturating_sub(1),
                                    y: win.y,
                                });
                            }
                        }
                        KeyCode::Right => {
                            if let Some(win) = self.windows.get(&id) {
                                let _ = self.server_tx.try_send(ClientMessage::MoveWindow {
                                    window_id: id,
                                    x: win.x + 1,
                                    y: win.y,
                                });
                            }
                        }
                        KeyCode::Up => {
                            if let Some(win) = self.windows.get(&id) {
                                let _ = self.server_tx.try_send(ClientMessage::MoveWindow {
                                    window_id: id,
                                    x: win.x,
                                    y: win.y.saturating_sub(1),
                                });
                            }
                        }
                        KeyCode::Down => {
                            if let Some(win) = self.windows.get(&id) {
                                let _ = self.server_tx.try_send(ClientMessage::MoveWindow {
                                    window_id: id,
                                    x: win.x,
                                    y: win.y + 1,
                                });
                            }
                        }
                        KeyCode::Char('w') => {
                            if let Some(win) = self.windows.get(&id) {
                                let _ = self.server_tx.try_send(ClientMessage::ResizeWindow {
                                    window_id: id,
                                    width: win.width,
                                    height: win.height.saturating_sub(1).max(3),
                                });
                            }
                        }
                        KeyCode::Char('s') => {
                            if let Some(win) = self.windows.get(&id) {
                                let _ = self.server_tx.try_send(ClientMessage::ResizeWindow {
                                    window_id: id,
                                    width: win.width,
                                    height: win.height + 1,
                                });
                            }
                        }
                        KeyCode::Char('a') => {
                            if let Some(win) = self.windows.get(&id) {
                                let _ = self.server_tx.try_send(ClientMessage::ResizeWindow {
                                    window_id: id,
                                    width: win.width.saturating_sub(1).max(10),
                                    height: win.height,
                                });
                            }
                        }
                        KeyCode::Char('d') => {
                            if let Some(win) = self.windows.get(&id) {
                                let _ = self.server_tx.try_send(ClientMessage::ResizeWindow {
                                    window_id: id,
                                    width: win.width + 1,
                                    height: win.height,
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(false)
    }

    fn send_key_to_window(&mut self, id: usize, key: KeyEvent) -> Result<bool> {
        let mut data = Vec::new();

        // Handle scroll keys
        match (key.modifiers, key.code) {
            (KeyModifiers::SHIFT, KeyCode::PageUp) => {
                let _ = self.server_tx.try_send(ClientMessage::Scroll {
                    window_id: id,
                    amount: 10,
                });
                return Ok(false);
            }
            (KeyModifiers::SHIFT, KeyCode::PageDown) => {
                let _ = self.server_tx.try_send(ClientMessage::Scroll {
                    window_id: id,
                    amount: -10,
                });
                return Ok(false);
            }
            _ => {}
        }

        // Convert key to bytes
        if key.modifiers.contains(KeyModifiers::ALT) {
            data.push(27);
        }

        match key.code {
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    if c.is_ascii_lowercase() {
                        data.push((c as u8) - b'a' + 1);
                    } else if c.is_ascii_uppercase() {
                        data.push((c as u8) - b'A' + 1);
                    } else {
                        match c {
                            '[' => data.push(27),
                            '\\' => data.push(28),
                            ']' => data.push(29),
                            '^' => data.push(30),
                            '_' => data.push(31),
                            ' ' => data.push(0),
                            _ => {
                                let mut buf = [0u8; 4];
                                data.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                            }
                        }
                    }
                } else {
                    let mut buf = [0u8; 4];
                    data.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
            }
            KeyCode::Enter => data.push(b'\r'),
            KeyCode::Backspace => data.push(127),
            KeyCode::Tab => data.push(9),
            KeyCode::Esc => data.push(27),
            KeyCode::Up => data.extend_from_slice(b"\x1b[A"),
            KeyCode::Down => data.extend_from_slice(b"\x1b[B"),
            KeyCode::Right => data.extend_from_slice(b"\x1b[C"),
            KeyCode::Left => data.extend_from_slice(b"\x1b[D"),
            KeyCode::Home => data.extend_from_slice(b"\x1b[H"),
            KeyCode::End => data.extend_from_slice(b"\x1b[F"),
            KeyCode::Insert => data.extend_from_slice(b"\x1b[2~"),
            KeyCode::Delete => data.extend_from_slice(b"\x1b[3~"),
            KeyCode::PageUp => data.extend_from_slice(b"\x1b[5~"),
            KeyCode::PageDown => data.extend_from_slice(b"\x1b[6~"),
            KeyCode::F(1) => data.extend_from_slice(b"\x1bOP"),
            KeyCode::F(2) => data.extend_from_slice(b"\x1bOQ"),
            KeyCode::F(3) => data.extend_from_slice(b"\x1bOR"),
            KeyCode::F(4) => data.extend_from_slice(b"\x1bOS"),
            KeyCode::F(5) => data.extend_from_slice(b"\x1b[15~"),
            KeyCode::F(6) => data.extend_from_slice(b"\x1b[17~"),
            KeyCode::F(7) => data.extend_from_slice(b"\x1b[18~"),
            KeyCode::F(8) => data.extend_from_slice(b"\x1b[19~"),
            KeyCode::F(9) => data.extend_from_slice(b"\x1b[20~"),
            KeyCode::F(10) => data.extend_from_slice(b"\x1b[21~"),
            KeyCode::F(11) => data.extend_from_slice(b"\x1b[23~"),
            KeyCode::F(12) => data.extend_from_slice(b"\x1b[24~"),
            _ => {}
        }

        if !data.is_empty() {
            let _ = self.server_tx.try_send(ClientMessage::Input {
                window_id: id,
                data,
            });
        }
        Ok(false)
    }

    fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) -> Result<bool> {
        self.last_mouse_pos = (mouse.column, mouse.row);

        // --- 1. Window Management (Drag/Resize) ---
        // This must always take precedence so a drag is not interrupted when passing over other windows.
        if let Some(ref state) = self.drag_state {
            if matches!(mouse.kind, MouseEventKind::Up(_)) {
                self.drag_state = None;
                return Ok(false);
            }

            if let MouseEventKind::Drag(MouseButton::Left) = mouse.kind {
                // Check if window still exists
                if !self.windows.contains_key(&state.window_id) {
                    self.drag_state = None;
                    return Ok(false);
                }

                // Rate limit: only send updates every 16ms (60fps)
                let now = std::time::Instant::now();
                if now.duration_since(state.last_update).as_millis() < 16 {
                    return Ok(false);
                }

                let dx = mouse.column as i32 - state.start_mouse.0 as i32;
                let dy = mouse.row as i32 - state.start_mouse.1 as i32;

                if state.is_resize {
                    let new_width = (state.start_rect.width as i32 + dx).max(10) as u16;
                    let new_height = (state.start_rect.height as i32 + dy).max(3) as u16;
                    if self
                        .server_tx
                        .try_send(ClientMessage::ResizeWindow {
                            window_id: state.window_id,
                            width: new_width,
                            height: new_height,
                        })
                        .is_err()
                    {
                        return Ok(false);
                    }
                } else {
                    let nx = (state.start_rect.x as i32 + dx).max(0) as u16;
                    let ny = (state.start_rect.y as i32 + dy).max(0) as u16;
                    if self
                        .server_tx
                        .try_send(ClientMessage::MoveWindow {
                            window_id: state.window_id,
                            x: nx,
                            y: ny,
                        })
                        .is_err()
                    {
                        return Ok(false);
                    }
                }

                if let Some(s) = self.drag_state.as_mut() {
                    s.last_update = now;
                }
                return Ok(false);
            }
        }

        // --- 2. HOVER PASSTHROUGH LOGIC ---
        // If mouse is inside ANY window's terminal area (excluding borders),
        // we capture the event to prevent desktop logic from running.
        if let Some((id, true)) = self.get_window_at(mouse.column, mouse.row) {
            // Any click in terminal area focuses the window
            if matches!(mouse.kind, MouseEventKind::Down(_)) {
                self.active_window_id = Some(id);
                let _ = self
                    .server_tx
                    .try_send(ClientMessage::FocusWindow { window_id: id });
            }

            let win = self.windows.get(&id).unwrap();

            // Only send mouse sequences to the server if the application requested them
            if win.mouse_reporting {
                let rel_x = mouse.column.saturating_sub(win.x + 1) + 1;
                let rel_y = mouse.row.saturating_sub(win.y + 1) + 1;

                match mouse.kind {
                    MouseEventKind::Down(btn)
                    | MouseEventKind::Up(btn)
                    | MouseEventKind::Drag(btn) => {
                        let cb = match btn {
                            MouseButton::Left => 0,
                            MouseButton::Middle => 1,
                            MouseButton::Right => 2,
                        };
                        let cb = if matches!(mouse.kind, MouseEventKind::Drag(_)) {
                            cb + 32
                        } else {
                            cb
                        };
                        let suffix = if matches!(mouse.kind, MouseEventKind::Up(_)) {
                            'm'
                        } else {
                            'M'
                        };
                        let data =
                            format!("\x1b[<{};{};{}{}", cb, rel_x, rel_y, suffix).into_bytes();
                        let _ = self.server_tx.try_send(ClientMessage::Input {
                            window_id: id,
                            data,
                        });
                    }
                    MouseEventKind::ScrollUp => {
                        let data = format!("\x1b[<64;{};{}M", rel_x, rel_y).into_bytes();
                        let _ = self.server_tx.try_send(ClientMessage::Input {
                            window_id: id,
                            data,
                        });
                    }
                    MouseEventKind::ScrollDown => {
                        let data = format!("\x1b[<65;{};{}M", rel_x, rel_y).into_bytes();
                        let _ = self.server_tx.try_send(ClientMessage::Input {
                            window_id: id,
                            data,
                        });
                    }
                    _ => {}
                }
            }
            return Ok(false); // ALWAYS return here when over a terminal area. No desktop logic allowed.
        }

        // --- 3. Standard Window Management / Desktop Logic ---
        // Only reached if NOT dragging AND NOT hovering over a terminal area.
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Right)
                if self.mode == Mode::Desktop
                    || mouse.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                // Calculate window size based on screen size
                let screen = self.last_screen_size;
                let width = (screen.width.saturating_sub(4)).clamp(20, 80);
                let height = (screen.height.saturating_sub(6)).clamp(5, 24);
                let _ = self.server_tx.try_send(ClientMessage::CreateWindow {
                    x: mouse.column,
                    y: mouse.row,
                    width,
                    height,
                    command: None,
                    args: vec![],
                });
                Ok(false)
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // Find window at this position (check front windows first)
                let mut windows_with_z: Vec<_> = self.windows.iter().collect();
                windows_with_z.sort_by(|a, b| b.1.z_order.cmp(&a.1.z_order)); // Highest z_order first
                for (&id, _win) in windows_with_z {
                    let win = match self.windows.get(&id) {
                        Some(w) => w,
                        None => continue,
                    };
                    let rect = if win.minimized {
                        Rect::new(win.x, win.y, win.width, 1)
                    } else {
                        Rect::new(win.x, win.y, win.width, win.height)
                    };

                    if mouse.column >= rect.x
                        && mouse.column < rect.x + rect.width
                        && mouse.row >= rect.y
                        && mouse.row < rect.y + rect.height
                    {
                        let is_title = mouse.row == rect.y;
                        let is_resize = !win.minimized
                            && mouse.column == rect.x + rect.width - 1
                            && mouse.row == rect.y + rect.height - 1;

                        if is_title {
                            // Check for double click to rename
                            let now = std::time::Instant::now();
                            if let Some((last_id, last_time)) = self.last_click
                                && last_id == id
                                && now.duration_since(last_time).as_millis() < 500
                            {
                                // Verify we're not clicking the buttons
                                let is_button = (mouse.column >= rect.x + 2
                                    && mouse.column <= rect.x + 12)
                                    || (mouse.column >= rect.x + rect.width.saturating_sub(5));
                                if !is_button {
                                    self.rename_state = Some(RenameState {
                                        window_id: id,
                                        input: win.title.clone(),
                                    });
                                    self.last_click = None;
                                    return Ok(false);
                                }
                            }
                            self.last_click = Some((id, now));

                            if mouse.column >= rect.x + 2 && mouse.column <= rect.x + 4 {
                                let _ = self
                                    .server_tx
                                    .try_send(ClientMessage::CloseWindow { window_id: id });
                                return Ok(false);
                            }
                            if mouse.column >= rect.x + 6 && mouse.column <= rect.x + 8 {
                                let _ = self
                                    .server_tx
                                    .try_send(ClientMessage::MinimizeWindow { window_id: id });
                                return Ok(false);
                            }
                            if mouse.column >= rect.x + 10 && mouse.column <= rect.x + 12 {
                                let _ = self
                                    .server_tx
                                    .try_send(ClientMessage::MaximizeWindow { window_id: id });
                                return Ok(false);
                            }

                            // Fullscreen [F] button
                            if mouse.column >= rect.x + rect.width.saturating_sub(9)
                                && mouse.column <= rect.x + rect.width.saturating_sub(6)
                            {
                                let _ = self
                                    .server_tx
                                    .try_send(ClientMessage::ToggleFullscreen { window_id: id });
                                return Ok(false);
                            }

                            // Solo [S] button
                            if mouse.column >= rect.x + rect.width.saturating_sub(5)
                                && mouse.column < rect.x + rect.width
                            {
                                let _ = self
                                    .server_tx
                                    .try_send(ClientMessage::ToggleSolo { window_id: id });
                                return Ok(false);
                            }
                        }

                        // Focus this window
                        self.active_window_id = Some(id);
                        let _ = self
                            .server_tx
                            .try_send(ClientMessage::FocusWindow { window_id: id });

                        let is_mgmt = self.mode == Mode::Desktop
                            || mouse.modifiers.contains(KeyModifiers::CONTROL);

                        if is_title || is_resize || is_mgmt {
                            self.drag_state = Some(DragState {
                                window_id: id,
                                start_mouse: (mouse.column, mouse.row),
                                start_rect: rect,
                                is_resize: is_resize
                                    || (!win.minimized
                                        && is_mgmt
                                        && mouse.column >= rect.x + rect.width - 2
                                        && mouse.row >= rect.y + rect.height - 1),
                                last_update: std::time::Instant::now(),
                            });
                        }
                        return Ok(false);
                    }
                }
                Ok(false)
            }
            MouseEventKind::ScrollUp => {
                if let Some(id) = self.active_window_id {
                    let _ = self.server_tx.try_send(ClientMessage::Scroll {
                        window_id: id,
                        amount: 3,
                    });
                }
                Ok(false)
            }
            MouseEventKind::ScrollDown => {
                if let Some(id) = self.active_window_id {
                    let _ = self.server_tx.try_send(ClientMessage::Scroll {
                        window_id: id,
                        amount: -3,
                    });
                }
                Ok(false)
            }
            _ => Ok(false),
        }
    }
}

#[allow(unused_assignments)]
pub async fn run_client(stream: TcpStream, initial_layout: Option<String>) -> Result<()> {
    println!("Connected!");

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let (tx, mut rx) = mpsc::channel::<AppEvent>(1000);
    let (server_tx, mut server_rx) = mpsc::channel::<ClientMessage>(1000);

    // Spawn crossterm event reader
    let tx_crossterm = tx.clone();
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let shutdown_flag_clone = shutdown_flag.clone();
    let crossterm_handle = tokio::task::spawn_blocking(move || {
        loop {
            // Check if we should shutdown
            if shutdown_flag_clone.load(Ordering::Relaxed) {
                break;
            }
            // Use poll with timeout so we can exit quickly
            if event::poll(Duration::from_millis(50)).unwrap_or(false)
                && let Ok(event) = event::read()
                && tx_crossterm
                    .blocking_send(AppEvent::Terminal(event))
                    .is_err()
            {
                break;
            }
        }
    });

    // Split stream for bidirectional communication
    let (mut read_stream, mut write_stream) = stream.into_split();

    // Spawn server message reader
    let tx_server = tx.clone();
    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        let mut accum = Vec::new();
        loop {
            match read_stream.read(&mut buf).await {
                Ok(0) => {
                    // Server closed connection
                    let _ = tx_server.try_send(AppEvent::Server(ServerMessage::Shutdown));
                    break;
                }
                Ok(n) => {
                    accum.extend_from_slice(&buf[..n]);
                    // Try to decode messages with proper framing
                    while accum.len() >= 4 {
                        let len =
                            u32::from_be_bytes([accum[0], accum[1], accum[2], accum[3]]) as usize;
                        if accum.len() < 4 + len {
                            break;
                        }
                        if let Ok(msg) = bincode::deserialize::<ServerMessage>(&accum[4..4 + len])
                            && tx_server.send(AppEvent::Server(msg)).await.is_err()
                        {
                            break;
                        }
                        accum.drain(0..4 + len);
                    }
                }
                Err(_e) => {
                    let _ = tx_server.try_send(AppEvent::Server(ServerMessage::Shutdown));
                    break;
                }
            }
        }
    });
    tokio::spawn(async move {
        while let Some(msg) = server_rx.recv().await {
            if let Ok(data) = encode_message(&msg)
                && write_stream.write_all(&data).await.is_err()
            {
                break;
            }
        }
    });

    // Send connect message
    let initial_size = terminal.size()?;
    let _ = server_tx
        .send(ClientMessage::Connect {
            term_size: (initial_size.width, initial_size.height),
        })
        .await;

    // Load initial layout if requested
    if let Some(path) = initial_layout {
        let _ = server_tx.send(ClientMessage::LoadLayout { path }).await;
    }

    let mut client = Client::new(initial_size.into(), server_tx.clone());

    // Main loop - efficient event-driven rendering
    // Only render when events come in. Use a slow ticker only for cursor blink.
    let mut blink_ticker = interval(Duration::from_millis(500));
    let mut needs_redraw = true;

    loop {
        if needs_redraw {
            terminal.draw(|f| {
                // ... rendering logic
                let size = f.area();
                // ... (rest of the drawing logic)

                    // Background
                    f.render_widget(
                        Block::default()
                            .title(" TermPlex ")
                            .borders(Borders::ALL)
                            .style(Style::default().bg(Color::Rgb(15, 15, 25)).fg(Color::DarkGray)),
                        size,
                    );
                    // Render windows - sort by z_order so front windows are rendered last (on top)
                    let mut windows_to_render: Vec<_> = client.windows.values().collect();
                    windows_to_render.sort_by(|a, b| a.z_order.cmp(&b.z_order));
                    for win in windows_to_render {
                        let render_rect = if win.minimized {
                            Rect::new(win.x, win.y, win.width, 1)
                        } else {
                            Rect::new(win.x, win.y, win.width, win.height)
                        };
                        // Shadow
                        let shadow_area = Rect::new(
                            render_rect.x + 1,
                            render_rect.y + 1,
                            render_rect.width,
                            render_rect.height,
                        )
                        .intersection(size);
                        if !shadow_area.is_empty() && win.focused {
                            f.render_widget(
                                Block::default().style(Style::default().bg(Color::Rgb(30, 30, 30))),
                                shadow_area,
                            );
                        }

                        // Window area
                        let window_area = render_rect.intersection(size);
                        if window_area.is_empty() {
                            continue;
                        }

                        // Clear window area
                        f.render_widget(Clear, window_area);

                        // Border/Header
                        let border_style = if win.focused {
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::Gray)
                        };

                        let title_text = if let Some(ref rs) = client.rename_state && rs.window_id == win.id {
                            format!(" [X] [_] [^] > {}_ ", rs.input)
                        } else {
                            format!(" [X] [_] [^] {} ", win.title)
                        };

                        let fs_button = " [F] [S] ";
                        let title_len = title_text.chars().count();
                        let padding = win.width.saturating_sub(title_len as u16).saturating_sub(fs_button.chars().count() as u16).saturating_sub(2);
                        let full_title = format!("{}{}{}", title_text, " ".repeat(padding as usize), fs_button);

                        let block = if win.minimized {
                            Block::default()
                                .title(full_title)
                                .style(Style::default().bg(Color::Rgb(40, 40, 60)))
                        } else {
                            Block::default()
                                .title(full_title)
                                .borders(Borders::ALL)
                                .border_style(border_style)
                                .style(Style::default().bg(Color::Black))
                        };
                        f.render_widget(block, window_area);

                        // Terminal content
                        if !win.minimized {
                            let inner_area = Rect::new(
                                win.x + 1,
                                win.y + 1,
                                win.width.saturating_sub(2),
                                win.height.saturating_sub(2),
                            )
                            .intersection(size);

                            if !inner_area.is_empty() {
                                f.render_widget(TerminalWidget::new(win), inner_area);
                            }

                            // Resize handle
                            let handle_x = win.x + win.width - 1;
                            let handle_y = win.y + win.height - 1;
                            if handle_x < size.width && handle_y < size.height {
                                let style = if win.focused {
                                    Style::default().fg(Color::Cyan)
                                } else {
                                    Style::default().fg(Color::Gray)
                                };
                                f.buffer_mut()[(handle_x, handle_y)].set_char('◢').set_style(style);
                            }
                        }
                    }

                    // Status bar
                    let status_rect = Rect::new(0, size.height - 1, size.width, 1);
                    let (status_text, style) = if client.mode == Mode::Desktop {
                        (
                            " [DESKTOP] | Tab: Focus | Arrows: Move | WASD: Resize | Z: Close | X: Min | C: Max | F: Full | N: New | O: Save | I: Load | V: Pane | P: Full | Q: Quit ",
                            Style::default().bg(Color::Yellow).fg(Color::Black).add_modifier(Modifier::BOLD),
                        )
                    } else {
                        (
                            " [F12: Desktop Mode] ",
                            Style::default().bg(Color::Rgb(40, 40, 80)).fg(Color::White).add_modifier(Modifier::BOLD),
                        )
                    };
                    f.render_widget(Paragraph::new(status_text).style(style), status_rect);
                })?;
            needs_redraw = false;
        }

        // Block waiting for either an event or the blink ticker
        tokio::select! {
            event = rx.recv() => {
                if let Some(event) = event {
                    if let Ok(quit) = client.handle_event(event) && quit {
                        break;
                    }
                    needs_redraw = true;
                } else {
                    break;
                }
            }
            _ = blink_ticker.tick() => {
                // Periodically redraw for cursor blink
                needs_redraw = true;
            }
        };
    }

    // Cleanup
    // Signal crossterm thread to exit
    shutdown_flag.store(true, Ordering::Relaxed);
    // Wait for crossterm thread to finish (with timeout)
    let _ = tokio::time::timeout(Duration::from_millis(200), crossterm_handle).await;

    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;

    println!("Disconnected from server");
    Ok(())
}
