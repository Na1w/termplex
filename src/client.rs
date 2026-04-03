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
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::Line,
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

const DESKBAR_WIDTH: u16 = 20;

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

#[derive(PartialEq, Eq, Debug, Clone, Copy)]
enum Menu {
    None,
    File,
    Window,
    View,
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

#[derive(Clone, Copy, Debug)]
struct Selection {
    window_id: usize,
    start: (u16, u16),
    end: (u16, u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HitTarget {
    None,
    Deskbar(usize), // window index in deskbar
    MenuLabel(Menu),
    MenuItem(Menu, usize),
    WindowContent(usize),
    WindowTitle(usize),
    WindowBorder(usize),
    WindowResize(usize),
    CloseButton(usize),
    MinimizeButton(usize),
    MaximizeButton(usize),
    FullscreenButton(usize),
    SoloButton(usize),
    ResetButton(usize),
}

struct Client {
    windows: HashMap<usize, WindowState>,
    active_window_id: Option<usize>,
    mode: Mode,
    menu: Menu,
    selected_item: usize,
    clipboard: String,
    selection: Option<Selection>,
    pending_selection: Option<(usize, u16, u16)>,
    drag_state: Option<DragState>,
    rename_state: Option<RenameState>,
    last_screen_size: Rect,
    last_mouse_pos: (u16, u16),
    last_click: Option<(usize, std::time::Instant)>,
    server_tx: mpsc::Sender<ClientMessage>,
    hit_map: Vec<HitTarget>,
}

impl Client {
    fn new(screen_size: Rect, server_tx: mpsc::Sender<ClientMessage>) -> Self {
        let size = (screen_size.width as usize) * (screen_size.height as usize);
        Self {
            windows: HashMap::new(),
            active_window_id: None,
            mode: Mode::Terminal,
            menu: Menu::None,
            selected_item: 0,
            clipboard: String::new(),
            selection: None,
            pending_selection: None,
            drag_state: None,
            rename_state: None,
            last_screen_size: screen_size,
            last_mouse_pos: (0, 0),
            last_click: None,
            server_tx,
            hit_map: vec![HitTarget::None; size],
        }
    }

    fn update_hit_map_size(&mut self, width: u16, height: u16) {
        let size = (width as usize) * (height as usize);
        if self.hit_map.len() != size {
            self.hit_map = vec![HitTarget::None; size];
        }
    }

    fn set_hit(&mut self, x: u16, y: u16, target: HitTarget) {
        let w = self.last_screen_size.width;
        let h = self.last_screen_size.height;
        if x < w && y < h {
            let idx = (y as usize) * (w as usize) + (x as usize);
            if idx < self.hit_map.len() {
                self.hit_map[idx] = target;
            }
        }
    }

    fn get_hit(&self, x: u16, y: u16) -> HitTarget {
        let w = self.last_screen_size.width;
        let h = self.last_screen_size.height;
        if x < w && y < h {
            let idx = (y as usize) * (w as usize) + (x as usize);
            return self.hit_map.get(idx).copied().unwrap_or(HitTarget::None);
        }
        HitTarget::None
    }

    fn get_window_at(&self, x: u16, y: u16) -> Option<(usize, bool)> {
        match self.get_hit(x, y) {
            HitTarget::WindowContent(id) => Some((id, true)),
            HitTarget::WindowTitle(id)
            | HitTarget::WindowBorder(id)
            | HitTarget::WindowResize(id)
            | HitTarget::CloseButton(id)
            | HitTarget::MinimizeButton(id)
            | HitTarget::MaximizeButton(id)
            | HitTarget::FullscreenButton(id)
            | HitTarget::SoloButton(id)
            | HitTarget::ResetButton(id) => Some((id, false)),
            _ => None,
        }
    }

    fn update_hit_map(&mut self) {
        let size = self.last_screen_size;
        self.hit_map.fill(HitTarget::None);

        // --- 1. Windows (Sort by Z-order: back to front) ---
        // Collect window data first to avoid borrow checker issues with self
        let mut window_data: Vec<(usize, u16, u16, u16, u16, bool, usize)> = self
            .windows
            .values()
            .map(|w| (w.id, w.x, w.y, w.width, w.height, w.minimized, w.z_order))
            .collect();
        window_data.sort_by_key(|w| w.6); // Sort by z_order back-to-front

        for (id, win_x, win_y, win_width, win_height, minimized, _) in window_data {
            if minimized {
                // Header only
                for x in win_x..win_x + win_width {
                    self.set_hit(x, win_y, HitTarget::WindowTitle(id));
                }
                // Buttons in header
                for x in (win_x + 2)..=(win_x + 4) {
                    self.set_hit(x, win_y, HitTarget::CloseButton(id));
                }
                for x in (win_x + 6)..=(win_x + 8) {
                    self.set_hit(x, win_y, HitTarget::MinimizeButton(id));
                }
                for x in (win_x + 10)..=(win_x + 12) {
                    self.set_hit(x, win_y, HitTarget::MaximizeButton(id));
                }
            } else {
                // Full window
                for y in win_y..win_y + win_height {
                    for x in win_x..win_x + win_width {
                        let target = if x == win_x
                            || x == win_x + win_width - 1
                            || y == win_y
                            || y == win_y + win_height - 1
                        {
                            if y == win_y {
                                HitTarget::WindowTitle(id)
                            } else if x == win_x + win_width - 1 && y == win_y + win_height - 1 {
                                HitTarget::WindowResize(id)
                            } else {
                                HitTarget::WindowBorder(id)
                            }
                        } else {
                            HitTarget::WindowContent(id)
                        };
                        self.set_hit(x, y, target);
                    }
                }
                // Header Buttons
                for x in (win_x + 2)..=(win_x + 4) {
                    self.set_hit(x, win_y, HitTarget::CloseButton(id));
                }
                for x in (win_x + 6)..=(win_x + 8) {
                    self.set_hit(x, win_y, HitTarget::MinimizeButton(id));
                }
                for x in (win_x + 10)..=(win_x + 12) {
                    self.set_hit(x, win_y, HitTarget::MaximizeButton(id));
                }

                // Right-side header buttons (F, S)
                let right_start = win_x + win_width.saturating_sub(9);
                for x in right_start..right_start + 3 {
                    self.set_hit(x, win_y, HitTarget::FullscreenButton(id));
                }
                let solo_start = win_x + win_width.saturating_sub(5);
                for x in solo_start..solo_start + 4 {
                    self.set_hit(x, win_y, HitTarget::SoloButton(id));
                }

                // Reset button [D] in bottom border
                let bottom_y = win_y + win_height - 1;
                let reset_start = win_x + win_width.saturating_sub(5);
                for x in reset_start..(win_x + win_width - 1) {
                    self.set_hit(x, bottom_y, HitTarget::ResetButton(id));
                }
            }
        }

        // --- 2. Deskbar (Always on top) ---
        let deskbar_height = (self.windows.len() as u16 + 2).max(3);
        let deskbar_x_start = size.width.saturating_sub(DESKBAR_WIDTH);
        for y in 0..deskbar_height {
            for x in deskbar_x_start..size.width {
                let target = if y >= 1 && y < deskbar_height - 1 {
                    HitTarget::Deskbar((y - 1) as usize)
                } else {
                    HitTarget::None
                };
                self.set_hit(x, y, target);
            }
        }

        // --- 3. Menu Bar (Always on top) ---
        let menus = [
            (Menu::File, " File "),
            (Menu::Window, " Window "),
            (Menu::View, " View "),
        ];
        let mut x_offset = 2;
        for (m, label) in menus {
            let label_len = label.len() as u16;
            for x in x_offset..x_offset + label_len {
                self.set_hit(x, 0, HitTarget::MenuLabel(m));
            }

            // Dropdown contents
            if self.menu == m {
                let items_count = match m {
                    Menu::File => 6,
                    Menu::Window => 6,
                    Menu::View => 1,
                    _ => 0,
                };
                // Approximate dropdown width (matched with rendering)
                let dw = match m {
                    Menu::File => 18,
                    Menu::Window => 18,
                    Menu::View => 22,
                    _ => 0,
                };
                for dy in 0..items_count {
                    for dx in 0..dw {
                        self.set_hit(x_offset + 1 + dx, 2 + dy as u16, HitTarget::MenuItem(m, dy));
                    }
                }
            }
            x_offset += label_len + 2;
        }
    }

    fn execute_menu_item(&mut self, menu: Menu, idx: usize) -> Result<bool> {
        match (menu, idx) {
            (Menu::File, 0) => {
                // New Terminal
                let screen = self.last_screen_size;
                let width = DEFAULT_TERM_WIDTH + 2;
                let height = DEFAULT_TERM_HEIGHT + 2;
                let x = (screen.width.saturating_sub(width)) / 2;
                let y = (screen.height.saturating_sub(height)) / 2;
                let _ = self.server_tx.try_send(ClientMessage::CreateWindow {
                    x,
                    y,
                    width,
                    height,
                    command: None,
                    args: vec![],
                });
            }
            (Menu::File, 1) => {
                // Save Layout
                let _ = self.server_tx.try_send(ClientMessage::SaveLayout {
                    path: "layout.json".to_string(),
                });
            }
            (Menu::File, 2) => {
                // Load Layout
                let _ = self.server_tx.try_send(ClientMessage::LoadLayout {
                    path: "layout.json".to_string(),
                });
            }
            (Menu::File, 3) => {
                // Capture Pane
                if let Some(id) = self.active_window_id {
                    let _ = self
                        .server_tx
                        .try_send(ClientMessage::CapturePane { window_id: id });
                }
            }
            (Menu::File, 4) => {
                // Capture Full
                let _ = self.server_tx.try_send(ClientMessage::CaptureFull);
            }
            (Menu::File, 5) => return Ok(true), // Quit

            (Menu::Window, 0) => {
                if let Some(id) = self.active_window_id {
                    let _ = self
                        .server_tx
                        .try_send(ClientMessage::CloseWindow { window_id: id });
                }
            }
            (Menu::Window, 1) => {
                if let Some(id) = self.active_window_id {
                    let _ = self
                        .server_tx
                        .try_send(ClientMessage::MinimizeWindow { window_id: id });
                }
            }
            (Menu::Window, 2) => {
                if let Some(id) = self.active_window_id {
                    let _ = self
                        .server_tx
                        .try_send(ClientMessage::MaximizeWindow { window_id: id });
                }
            }
            (Menu::Window, 3) => {
                if let Some(id) = self.active_window_id {
                    let _ = self
                        .server_tx
                        .try_send(ClientMessage::ToggleSolo { window_id: id });
                }
            }
            (Menu::Window, 4) => {
                if let Some(id) = self.active_window_id {
                    let _ = self
                        .server_tx
                        .try_send(ClientMessage::ToggleFullscreen { window_id: id });
                }
            }
            (Menu::Window, 5) => {
                let _ = self.server_tx.try_send(ClientMessage::TileWindows);
            }

            (Menu::View, 0) => {
                self.mode = if self.mode == Mode::Desktop {
                    Mode::Terminal
                } else {
                    Mode::Desktop
                }
            }
            _ => {}
        }
        self.menu = Menu::None;
        Ok(false)
    }

    fn handle_event(&mut self, event: AppEvent) -> Result<bool> {
        match event {
            AppEvent::Terminal(ev) => match ev {
                Event::Resize(w, h) => {
                    self.last_screen_size = Rect::new(0, 0, w, h);
                    self.update_hit_map_size(w, h);
                    // Report full width now that Deskbar is dynamic and overlay
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

                    // Handle menu navigation
                    if self.menu != Menu::None {
                        match key.code {
                            KeyCode::Esc => {
                                self.menu = Menu::None;
                            }
                            KeyCode::Up => {
                                self.selected_item = self.selected_item.saturating_sub(1);
                            }
                            KeyCode::Down => {
                                let items_count: usize = match self.menu {
                                    Menu::File => 6,
                                    Menu::Window => 6,
                                    Menu::View => 1,
                                    _ => 0,
                                };
                                if self.selected_item < items_count.saturating_sub(1) {
                                    self.selected_item += 1;
                                }
                            }
                            KeyCode::Left => {
                                self.menu = match self.menu {
                                    Menu::File => Menu::View,
                                    Menu::Window => Menu::File,
                                    Menu::View => Menu::Window,
                                    _ => Menu::None,
                                };
                                self.selected_item = 0;
                            }
                            KeyCode::Right => {
                                self.menu = match self.menu {
                                    Menu::File => Menu::Window,
                                    Menu::Window => Menu::View,
                                    Menu::View => Menu::File,
                                    _ => Menu::None,
                                };
                                self.selected_item = 0;
                            }
                            KeyCode::Enter => {
                                return self.execute_menu_item(self.menu, self.selected_item);
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

                        // Auto-activate menu in desktop mode for keyboard accessibility
                        if self.mode == Mode::Desktop {
                            self.menu = Menu::File;
                            self.selected_item = 0;
                        } else {
                            self.menu = Menu::None;
                        }
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
                let width = DEFAULT_TERM_WIDTH + 2;
                let height = DEFAULT_TERM_HEIGHT + 2;
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
            (KeyModifiers::NONE, KeyCode::Char('g')) => {
                let _ = self.server_tx.try_send(ClientMessage::TileWindows);
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
        let target = self.get_hit(mouse.column, mouse.row);

        // --- 0. INTERCEPT DRAGS ---
        if let Some(ref state) = self.drag_state {
            if matches!(mouse.kind, MouseEventKind::Up(_)) {
                self.drag_state = None;
                return Ok(false);
            }

            if matches!(mouse.kind, MouseEventKind::Drag(_)) {
                if !self.windows.contains_key(&state.window_id) {
                    self.drag_state = None;
                    return Ok(false);
                }

                let now = std::time::Instant::now();
                if now.duration_since(state.last_update).as_millis() < 16 {
                    return Ok(false);
                }

                let dx = mouse.column as i32 - state.start_mouse.0 as i32;
                let dy = mouse.row as i32 - state.start_mouse.1 as i32;

                if state.is_resize {
                    let new_width = (state.start_rect.width as i32 + dx).max(10) as u16;
                    let new_height = (state.start_rect.height as i32 + dy).max(3) as u16;
                    let _ = self.server_tx.try_send(ClientMessage::ResizeWindow {
                        window_id: state.window_id,
                        width: new_width,
                        height: new_height,
                    });
                } else {
                    let nx = (state.start_rect.x as i32 + dx).max(0) as u16;
                    let ny = (state.start_rect.y as i32 + dy).max(0) as u16;
                    let _ = self.server_tx.try_send(ClientMessage::MoveWindow {
                        window_id: state.window_id,
                        x: nx,
                        y: ny,
                    });
                }

                if let Some(s) = self.drag_state.as_mut() {
                    s.last_update = now;
                }
                return Ok(false);
            }
        }

        // --- 1. HANDLE CLICKS BASED ON HIT MAP ---
        match mouse.kind {
            MouseEventKind::Down(btn) => {
                match target {
                    HitTarget::MenuLabel(m) => {
                        if btn == MouseButton::Left {
                            self.menu = if self.menu == m { Menu::None } else { m };
                            self.mode = Mode::Desktop;
                        }
                    }
                    HitTarget::MenuItem(m, idx) => {
                        if btn == MouseButton::Left {
                            return self.execute_menu_item(m, idx);
                        }
                    }
                    HitTarget::Deskbar(idx) => {
                        if btn == MouseButton::Left {
                            let mut windows_sorted: Vec<_> = self.windows.values().collect();
                            windows_sorted.sort_by_key(|w| w.id);
                            if let Some(win) = windows_sorted.get(idx) {
                                let wid = win.id;
                                self.active_window_id = Some(wid);
                                let _ = self
                                    .server_tx
                                    .try_send(ClientMessage::FocusWindow { window_id: wid });
                            }
                        }
                    }
                    HitTarget::WindowContent(id) => {
                        self.active_window_id = Some(id);
                        let _ = self
                            .server_tx
                            .try_send(ClientMessage::FocusWindow { window_id: id });

                        let win = self.windows.get(&id).unwrap();
                        let rel_x = mouse.column.saturating_sub(win.x + 1);
                        let rel_y = mouse.row.saturating_sub(win.y + 1);

                        if win.mouse_reporting && !mouse.modifiers.contains(KeyModifiers::SHIFT) {
                            let sgr_btn = match btn {
                                MouseButton::Left => 0,
                                MouseButton::Middle => 1,
                                MouseButton::Right => 2,
                            };
                            let data = format!("\x1b[<{};{};{}M", sgr_btn, rel_x + 1, rel_y + 1)
                                .into_bytes();
                            let _ = self.server_tx.try_send(ClientMessage::Input {
                                window_id: id,
                                data,
                            });
                        } else if btn == MouseButton::Left {
                            self.pending_selection = Some((id, rel_y, rel_x));
                            self.selection = None;
                        } else if btn == MouseButton::Right && !self.clipboard.is_empty() {
                            let data = self.clipboard.clone().into_bytes();
                            let _ = self.server_tx.try_send(ClientMessage::Input {
                                window_id: id,
                                data,
                            });
                            self.clipboard.clear();
                        }
                    }
                    HitTarget::WindowTitle(id)
                    | HitTarget::WindowBorder(id)
                    | HitTarget::WindowResize(id) => {
                        self.active_window_id = Some(id);
                        let _ = self
                            .server_tx
                            .try_send(ClientMessage::FocusWindow { window_id: id });

                        if btn == MouseButton::Left {
                            let win = self.windows.get(&id).unwrap();
                            let rect = if win.minimized {
                                Rect::new(win.x, win.y, win.width, 1)
                            } else {
                                Rect::new(win.x, win.y, win.width, win.height)
                            };

                            // Check for double click to rename
                            let now = std::time::Instant::now();
                            if let Some((last_id, last_time)) = self.last_click
                                && last_id == id
                                && now.duration_since(last_time).as_millis() < 500
                            {
                                self.rename_state = Some(RenameState {
                                    window_id: id,
                                    input: win.title.clone(),
                                });
                                self.last_click = None;
                            } else {
                                self.last_click = Some((id, now));

                                // Start dragging or resizing
                                let is_mgmt = self.mode == Mode::Desktop
                                    || mouse.modifiers.contains(KeyModifiers::CONTROL);
                                let is_resize = matches!(target, HitTarget::WindowResize(_))
                                    || (is_mgmt
                                        && mouse.column >= win.x + win.width - 2
                                        && mouse.row >= win.y + win.height - 1);

                                self.drag_state = Some(DragState {
                                    window_id: id,
                                    start_mouse: (mouse.column, mouse.row),
                                    start_rect: rect,
                                    is_resize,
                                    last_update: now,
                                });
                            }
                        }
                    }
                    HitTarget::CloseButton(id) => {
                        if btn == MouseButton::Left {
                            let _ = self
                                .server_tx
                                .try_send(ClientMessage::CloseWindow { window_id: id });
                        }
                    }
                    HitTarget::MinimizeButton(id) => {
                        if btn == MouseButton::Left {
                            let _ = self
                                .server_tx
                                .try_send(ClientMessage::MinimizeWindow { window_id: id });
                        }
                    }
                    HitTarget::MaximizeButton(id) => {
                        if btn == MouseButton::Left {
                            let _ = self
                                .server_tx
                                .try_send(ClientMessage::MaximizeWindow { window_id: id });
                        }
                    }
                    HitTarget::FullscreenButton(id) => {
                        if btn == MouseButton::Left {
                            let _ = self
                                .server_tx
                                .try_send(ClientMessage::ToggleFullscreen { window_id: id });
                        }
                    }
                    HitTarget::SoloButton(id) => {
                        if btn == MouseButton::Left {
                            let _ = self
                                .server_tx
                                .try_send(ClientMessage::ToggleSolo { window_id: id });
                        }
                    }
                    HitTarget::ResetButton(id) => {
                        if btn == MouseButton::Left {
                            let _ = self.server_tx.try_send(ClientMessage::ResizeWindow {
                                window_id: id,
                                width: DEFAULT_TERM_WIDTH + 2,
                                height: DEFAULT_TERM_HEIGHT + 2,
                            });
                        }
                    }
                    HitTarget::None => {
                        if btn == MouseButton::Right
                            && (self.mode == Mode::Desktop
                                || mouse.modifiers.contains(KeyModifiers::CONTROL))
                        {
                            let width = DEFAULT_TERM_WIDTH + 2;
                            let height = DEFAULT_TERM_HEIGHT + 2;
                            let _ = self.server_tx.try_send(ClientMessage::CreateWindow {
                                x: mouse.column,
                                y: mouse.row,
                                width,
                                height,
                                command: None,
                                args: vec![],
                            });
                        }
                        self.menu = Menu::None;
                    }
                }
            }
            MouseEventKind::Drag(btn) => {
                if let HitTarget::WindowContent(id) = target {
                    let win = self.windows.get(&id).unwrap();
                    if win.mouse_reporting && !mouse.modifiers.contains(KeyModifiers::SHIFT) {
                        let rel_x = mouse.column.saturating_sub(win.x + 1);
                        let rel_y = mouse.row.saturating_sub(win.y + 1);
                        let sgr_btn = match btn {
                            MouseButton::Left => 32,
                            MouseButton::Middle => 33,
                            MouseButton::Right => 34,
                        };
                        let data =
                            format!("\x1b[<{};{};{}M", sgr_btn, rel_x + 1, rel_y + 1).into_bytes();
                        let _ = self.server_tx.try_send(ClientMessage::Input {
                            window_id: id,
                            data,
                        });
                    } else if btn == MouseButton::Left {
                        if let Some((pid, start_y, start_x)) = self.pending_selection {
                            if pid == id {
                                let rel_x = mouse.column.saturating_sub(win.x + 1);
                                let rel_y = mouse.row.saturating_sub(win.y + 1);
                                if start_y != rel_y || start_x != rel_x {
                                    self.selection = Some(Selection {
                                        window_id: id,
                                        start: (start_y, start_x),
                                        end: (rel_y, rel_x),
                                    });
                                }
                            }
                        } else if let Some(ref mut sel) = self.selection
                            && sel.window_id == id
                        {
                            sel.end = (
                                mouse.row.saturating_sub(win.y + 1),
                                mouse.column.saturating_sub(win.x + 1),
                            );
                        }
                    }
                }
            }
            MouseEventKind::Up(btn) => {
                if btn == MouseButton::Left {
                    if let Some(sel) = self.selection
                        && let Some(win) = self.windows.get(&sel.window_id)
                        && sel.start != sel.end
                    {
                        let mut text = String::new();
                        let (r1, c1) = (sel.start.0.min(sel.end.0), sel.start.1.min(sel.end.1));
                        let (r2, c2) = (sel.start.0.max(sel.end.0), sel.start.1.max(sel.end.1));
                        let inner_w = win.width.saturating_sub(2) as usize;
                        for r in r1..=r2 {
                            let mut line = String::new();
                            for c in 0..inner_w {
                                let col = c as u16;
                                if (r == r1 && col < c1) || (r == r2 && col > c2) {
                                    continue;
                                }
                                let idx = r as usize * inner_w + c;
                                if let Some(cell) = win.screen.get(idx) {
                                    line.push(cell.ch);
                                }
                            }
                            text.push_str(line.trim_end());
                            if r < r2 {
                                text.push('\n');
                            }
                        }
                        self.clipboard = text;
                    }
                    self.selection = None;
                    self.pending_selection = None;
                }

                if let HitTarget::WindowContent(id) = target {
                    let win = self.windows.get(&id).unwrap();
                    if win.mouse_reporting && !mouse.modifiers.contains(KeyModifiers::SHIFT) {
                        let sgr_btn = match btn {
                            MouseButton::Left => 0,
                            MouseButton::Middle => 1,
                            MouseButton::Right => 2,
                        };
                        let data = format!(
                            "\x1b[<{};{};{}m",
                            sgr_btn,
                            mouse.column.saturating_sub(win.x + 1) + 1,
                            mouse.row.saturating_sub(win.y + 1) + 1
                        )
                        .into_bytes();
                        let _ = self.server_tx.try_send(ClientMessage::Input {
                            window_id: id,
                            data,
                        });
                    }
                }
            }
            MouseEventKind::ScrollUp => {
                if let HitTarget::WindowContent(id) = target {
                    let win = self.windows.get(&id).unwrap();
                    if win.mouse_reporting && !mouse.modifiers.contains(KeyModifiers::SHIFT) {
                        let data = format!(
                            "\x1b[<64;{};{}M",
                            mouse.column.saturating_sub(win.x + 1) + 1,
                            mouse.row.saturating_sub(win.y + 1) + 1
                        )
                        .into_bytes();
                        let _ = self.server_tx.try_send(ClientMessage::Input {
                            window_id: id,
                            data,
                        });
                    } else {
                        let _ = self.server_tx.try_send(ClientMessage::Scroll {
                            window_id: id,
                            amount: 3,
                        });
                    }
                }
            }
            MouseEventKind::ScrollDown => {
                if let HitTarget::WindowContent(id) = target {
                    let win = self.windows.get(&id).unwrap();
                    if win.mouse_reporting && !mouse.modifiers.contains(KeyModifiers::SHIFT) {
                        let data = format!(
                            "\x1b[<65;{};{}M",
                            mouse.column.saturating_sub(win.x + 1) + 1,
                            mouse.row.saturating_sub(win.y + 1) + 1
                        )
                        .into_bytes();
                        let _ = self.server_tx.try_send(ClientMessage::Input {
                            window_id: id,
                            data,
                        });
                    } else {
                        let _ = self.server_tx.try_send(ClientMessage::Scroll {
                            window_id: id,
                            amount: -3,
                        });
                    }
                }
            }
            _ => {}
        }
        Ok(false)
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
    // Full width now that Deskbar is an overlay
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
            client.update_hit_map();
            terminal.draw(|f| {
                let size = f.area();

                // Background (Full screen)
                f.render_widget(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Rgb(30, 30, 50)))
                        .style(Style::default().bg(Color::Rgb(10, 10, 20))),
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

                    // Windows use full screen area
                    let window_area = render_rect.intersection(size);
                    if window_area.is_empty() {
                        continue;
                    }

                    // Shadow
                    let shadow_area = Rect::new(
                        window_area.x + 1,
                        window_area.y + 1,
                        window_area.width,
                        window_area.height,
                    )
                    .intersection(size);
                    if !shadow_area.is_empty() && win.focused {
                        f.render_widget(
                            Block::default().style(Style::default().bg(Color::Rgb(30, 30, 30))),
                            shadow_area,
                        );
                    }

                    // Clear window area
                    f.render_widget(Clear, window_area);

                    // Border/Header
                    let border_style = if win.focused {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    };

                    let title_text = if let Some(ref rs) = client.rename_state
                        && rs.window_id == win.id
                    {
                        format!(" [X] [_] [^] > {}_ ", rs.input)
                    } else {
                        format!(" [X] [_] [^] {} ", win.title)
                    };

                    let fs_button = " [F] [S] ";
                    let title_len = title_text.chars().count();
                    let padding = win
                        .width
                        .saturating_sub(title_len as u16)
                        .saturating_sub(fs_button.chars().count() as u16)
                        .saturating_sub(2);
                    let full_title = format!(
                        "{}{}{}",
                        title_text,
                        " ".repeat(padding as usize),
                        fs_button
                    );

                    let block = if win.minimized {
                        Block::default()
                            .title(full_title)
                            .style(Style::default().bg(Color::Rgb(40, 40, 60)))
                    } else {
                        let size_text = format!(
                            " {}x{} ",
                            win.width.saturating_sub(2),
                            win.height.saturating_sub(2)
                        );
                        let reset_text = " [D] ";
                        Block::default()
                            .title(full_title)
                            .title_bottom(Line::from(size_text).alignment(Alignment::Left))
                            .title_bottom(Line::from(reset_text).alignment(Alignment::Right))
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
                        .intersection(window_area);

                        if !inner_area.is_empty() {
                            let mut widget = TerminalWidget::new(win);
                            if let Some(sel) = client.selection
                                && sel.window_id == win.id
                            {
                                widget = widget.with_selection(Some((sel.start, sel.end)));
                            }
                            f.render_widget(widget, inner_area);
                        }

                        // Resize handle
                        let handle_x = win.x + win.width - 1;
                        let handle_y = win.y + win.height - 1;
                        if window_area.contains((handle_x, handle_y).into()) {
                            let style = if win.focused {
                                Style::default().fg(Color::Cyan)
                            } else {
                                Style::default().fg(Color::Gray)
                            };
                            f.buffer_mut()[(handle_x, handle_y)]
                                .set_char('◢')
                                .set_style(style);
                        }
                    }
                }

                // --- MENU BAR ---
                let menu_rect = Rect::new(0, 0, size.width, 1);
                let menu_style = if client.mode == Mode::Desktop {
                    Style::default()
                        .bg(Color::Rgb(50, 50, 100))
                        .fg(Color::White)
                } else {
                    Style::default().bg(Color::Rgb(20, 20, 40)).fg(Color::Gray)
                };
                f.render_widget(Block::default().style(menu_style), menu_rect);

                let menus = [
                    (Menu::File, " File "),
                    (Menu::Window, " Window "),
                    (Menu::View, " View "),
                ];

                let mut x_offset = 2;
                for (m, label) in menus {
                    let style = if client.menu == m {
                        Style::default()
                            .bg(Color::White)
                            .fg(Color::Black)
                            .add_modifier(Modifier::BOLD)
                    } else if client.mode == Mode::Desktop {
                        Style::default().fg(Color::White)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    f.render_widget(
                        Paragraph::new(label).style(style),
                        Rect::new(x_offset, 0, label.len() as u16, 1),
                    );

                    // Render dropdown
                    if client.menu == m {
                        let items = match m {
                            Menu::File => vec![
                                " New Terminal (N) ",
                                " Save Layout (O)  ",
                                " Load Layout (I)  ",
                                " Capture Pane (V) ",
                                " Capture Full (P) ",
                                " Quit (Q)         ",
                            ],
                            Menu::Window => vec![
                                " Close (Z)        ",
                                " Minimize (X)     ",
                                " Maximize (C)     ",
                                " Solo (S)         ",
                                " Fullscreen (F)   ",
                                " Tile Grid (G)    ",
                            ],
                            Menu::View => vec![" Toggle Desktop (F12) "],
                            _ => vec![],
                        };

                        let dw = items.iter().map(|s| s.len()).max().unwrap_or(0) as u16;
                        let dh = items.len() as u16;
                        let dropdown_rect = Rect::new(x_offset, 1, dw + 2, dh + 2);
                        f.render_widget(Clear, dropdown_rect);
                        f.render_widget(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(Color::White))
                                .style(Style::default().bg(Color::Rgb(30, 30, 50))),
                            dropdown_rect,
                        );

                        for (i, item) in items.iter().enumerate() {
                            let style = if client.selected_item == i {
                                Style::default()
                                    .bg(Color::Cyan)
                                    .fg(Color::Black)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(Color::White)
                            };
                            f.render_widget(
                                Paragraph::new(*item).style(style),
                                Rect::new(x_offset + 1, 2 + i as u16, dw, 1),
                            );
                        }
                    }
                    x_offset += label.len() as u16 + 2;
                }

                if client.mode == Mode::Terminal {
                    let hint = " [F12] Desktop Mode ";
                    let hint_x = size
                        .width
                        .saturating_sub(hint.len() as u16 + DESKBAR_WIDTH + 2);
                    f.render_widget(
                        Paragraph::new(hint).style(Style::default().fg(Color::DarkGray)),
                        Rect::new(hint_x, 0, hint.len() as u16, 1),
                    );
                }

                // --- DESKBAR (Overlay, Dynamic Height) ---
                let deskbar_height = (client.windows.len() as u16 + 2).max(3);
                let deskbar_area = Rect::new(
                    size.width.saturating_sub(DESKBAR_WIDTH),
                    0,
                    DESKBAR_WIDTH,
                    deskbar_height,
                );

                f.render_widget(Clear, deskbar_area);
                f.render_widget(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(Color::Rgb(50, 50, 100)))
                        .title(" TP ")
                        .style(Style::default().bg(Color::Rgb(15, 15, 30))),
                    deskbar_area,
                );

                let mut windows_sorted: Vec<_> = client.windows.values().collect();
                windows_sorted.sort_by_key(|w| w.id);
                for (i, win) in windows_sorted.iter().enumerate() {
                    let style = if win.focused {
                        Style::default()
                            .bg(Color::Cyan)
                            .fg(Color::Black)
                            .add_modifier(Modifier::BOLD)
                    } else if !win.running {
                        Style::default().fg(Color::Red)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    let prefix = if win.focused { "> " } else { "  " };
                    let text = format!("{}{}", prefix, win.title);
                    f.render_widget(
                        Paragraph::new(text).style(style),
                        Rect::new(
                            deskbar_area.x + 1,
                            1 + i as u16,
                            DESKBAR_WIDTH.saturating_sub(2),
                            1,
                        ),
                    );
                }
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
