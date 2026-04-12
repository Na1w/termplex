use anyhow::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
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
    text::{Line, Span},
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

const DESKBAR_WIDTH: u16 = 16;

#[derive(Debug)]
enum AppEvent {
    Terminal(Event),
    Server(ServerMessage),
}

#[derive(PartialEq, Eq, Debug, Clone, Copy)]
enum Menu {
    None,
    File,
    Window,
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
    WindowScrollbar(usize),
    DeskbarMinimizeButton,
}

struct Client {
    windows: HashMap<usize, WindowState>,
    active_window_id: Option<usize>,
    menu: Menu,
    selected_item: usize,
    clipboard: String,
    clipboard_manager: Option<arboard::Clipboard>,
    selection: Option<Selection>,
    pending_selection: Option<(usize, u16, u16)>,
    drag_state: Option<DragState>,
    scrollbar_drag: Option<usize>,
    rename_state: Option<RenameState>,
    last_screen_size: Rect,
    last_mouse_pos: (u16, u16),
    last_click: Option<(usize, std::time::Instant)>,
    server_tx: mpsc::Sender<ClientMessage>,
    hit_map: Vec<HitTarget>,
    solo_mode_active: bool,
    solo_origin_id: Option<usize>,
    temporarily_expanded_id: Option<usize>,
    deskbar_minimized: bool,
}

impl Client {
    fn new(screen_size: Rect, server_tx: mpsc::Sender<ClientMessage>) -> Self {
        let size = (screen_size.width as usize) * (screen_size.height as usize);
        let clipboard_manager = arboard::Clipboard::new().ok();
        Self {
            windows: HashMap::new(),
            active_window_id: None,
            menu: Menu::None,
            selected_item: 0,
            clipboard: String::new(),
            clipboard_manager,
            selection: None,
            pending_selection: None,
            drag_state: None,
            scrollbar_drag: None,
            rename_state: None,
            last_screen_size: screen_size,
            last_mouse_pos: (0, 0),
            last_click: None,
            server_tx,
            hit_map: vec![HitTarget::None; size],
            solo_mode_active: false,
            solo_origin_id: None,
            temporarily_expanded_id: None,
            deskbar_minimized: false,
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
                        } else if x == win_x + win_width - 2 {
                            HitTarget::WindowScrollbar(id)
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

        let has_fullscreen = self.windows.values().any(|w| w.fullscreen);

        // --- 2. Deskbar (Always on top) ---
        if !has_fullscreen {
            if self.deskbar_minimized {
                // Only a small button in the top right corner of the menu bar area
                let x = size.width.saturating_sub(4);
                for dx in 0..4 {
                    self.set_hit(x + dx, 0, HitTarget::DeskbarMinimizeButton);
                }
            } else {
                let deskbar_height = (self.windows.len() as u16 + 2).max(3);
                let deskbar_x_start = size.width.saturating_sub(DESKBAR_WIDTH);
                for y in 0..deskbar_height {
                    for x in deskbar_x_start..size.width {
                        let target = if y == 0 && x >= size.width.saturating_sub(4) {
                            HitTarget::DeskbarMinimizeButton
                        } else if y >= 1 && y < deskbar_height - 1 {
                            HitTarget::Deskbar((y - 1) as usize)
                        } else {
                            HitTarget::None
                        };
                        self.set_hit(x, y, target);
                    }
                }
            }
        }

        // --- 3. Menu Bar (Always on top) ---
        if !has_fullscreen {
            let menus = [(Menu::File, " File "), (Menu::Window, " Window ")];
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
                        Menu::Window => 7,
                        _ => 0,
                    };
                    // Approximate dropdown width (matched with rendering)
                    let dw = match m {
                        Menu::File => 18,
                        Menu::Window => 18,
                        _ => 0,
                    };
                    for dy in 0..items_count {
                        for dx in 0..dw {
                            self.set_hit(
                                x_offset + 1 + dx,
                                2 + dy as u16,
                                HitTarget::MenuItem(m, dy),
                            );
                        }
                    }
                }
                x_offset += label_len + 2;
            }
        }
    }

    fn send_osc52_copy(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let b64 = BASE64.encode(text);

        // Standard OSC 52: ESC ] 52 ; c ; <base64> ST
        let osc = format!("\x1b]52;c;{}\x1b\\", b64);

        // Tmux bypass: ESC P tmux ; ESC <standard_osc> ESC \
        let tmux_osc = format!("\x1bPtmux;\x1b\x1b]52;c;{}\x07\x1b\\", b64);

        // Screen bypass: ESC P \x1b]52;c;... \x07 ESC \
        let screen_osc = format!("\x1bP\x1b]52;c;{}\x07\x1b\\", b64);

        let mut stdout = io::stdout();
        let _ = io::Write::write_all(&mut stdout, osc.as_bytes());
        let _ = io::Write::write_all(&mut stdout, tmux_osc.as_bytes());
        let _ = io::Write::write_all(&mut stdout, screen_osc.as_bytes());
        let _ = io::Write::flush(&mut stdout);
    }

    fn execute_menu_item(&mut self, menu: Menu, idx: usize) -> Result<bool> {
        match (menu, idx) {
            (Menu::File, 0) => {
                // New Terminal
                let screen = self.last_screen_size;
                let width = DEFAULT_TERM_WIDTH + 2;
                let height = DEFAULT_TERM_HEIGHT + 2;
                let x = (screen.width.saturating_sub(width)) / 2;
                let y = ((screen.height.saturating_sub(height)) / 2).max(2);
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
            (Menu::Window, 6) => {
                if let Some(id) = self.active_window_id {
                    let _ = self
                        .server_tx
                        .try_send(ClientMessage::ClearScrollback { window_id: id });
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
                                    Menu::Window => 7,
                                    _ => 0,
                                };
                                if self.selected_item < items_count.saturating_sub(1) {
                                    self.selected_item += 1;
                                }
                            }
                            KeyCode::Left => {
                                self.menu = match self.menu {
                                    Menu::File => Menu::Window,
                                    Menu::Window => Menu::File,
                                    _ => Menu::None,
                                };
                                self.selected_item = 0;
                            }
                            KeyCode::Right => {
                                self.menu = match self.menu {
                                    Menu::File => Menu::Window,
                                    Menu::Window => Menu::File,
                                    _ => Menu::None,
                                };
                                self.selected_item = 0;
                            }
                            KeyCode::Enter => {
                                return self.execute_menu_item(self.menu, self.selected_item);
                            }
                            KeyCode::Char(c) => {
                                let target_idx = match (self.menu, c.to_ascii_lowercase()) {
                                    (Menu::File, 'n') => Some(0),
                                    (Menu::File, 'o') => Some(1),
                                    (Menu::File, 'i') => Some(2),
                                    (Menu::File, 'v') => Some(3),
                                    (Menu::File, 'p') => Some(4),
                                    (Menu::File, 'q') => Some(5),
                                    (Menu::Window, 'z') => Some(0),
                                    (Menu::Window, 'x') => Some(1),
                                    (Menu::Window, 'c') => Some(2),
                                    (Menu::Window, 's') => Some(3),
                                    (Menu::Window, 'f') => Some(4),
                                    (Menu::Window, 'g') => Some(5),
                                    (Menu::Window, 'l') => Some(6),
                                    _ => None,
                                };
                                if let Some(idx) = target_idx {
                                    return self.execute_menu_item(self.menu, idx);
                                }
                            }
                            _ => {}
                        }
                        return Ok(false);
                    }

                    // Determine if the mouse is over a window or the desktop
                    let hit = self.get_hit(self.last_mouse_pos.0, self.last_mouse_pos.1);
                    let is_over_window = matches!(
                        hit,
                        HitTarget::WindowContent(_)
                            | HitTarget::WindowTitle(_)
                            | HitTarget::WindowBorder(_)
                            | HitTarget::WindowResize(_)
                            | HitTarget::CloseButton(_)
                            | HitTarget::MinimizeButton(_)
                            | HitTarget::MaximizeButton(_)
                            | HitTarget::FullscreenButton(_)
                            | HitTarget::SoloButton(_)
                            | HitTarget::ResetButton(_)
                            | HitTarget::WindowScrollbar(_)
                    );

                    // Send keys directly to the active window
                    if let Some(id) = self.active_window_id {
                        return self.send_key_to_window(id, key, is_over_window);
                    }
                }
                Event::Mouse(mouse) => {
                    return self.handle_mouse(mouse);
                }
                _ => {}
            },
            AppEvent::Server(msg) => match msg {
                ServerMessage::Welcome {
                    windows,
                    solo_mode_active,
                    solo_origin_id,
                    ..
                } => {
                    self.windows.clear();
                    self.active_window_id = None;
                    self.solo_mode_active = solo_mode_active;
                    self.solo_origin_id = solo_origin_id;
                    self.temporarily_expanded_id = None;
                    for win in windows {
                        if win.focused {
                            self.active_window_id = Some(win.id);
                        }
                        self.windows.insert(win.id, win);
                    }
                }
                ServerMessage::FullSync {
                    windows,
                    solo_mode_active,
                    solo_origin_id,
                } => {
                    self.windows.clear();
                    self.active_window_id = None;
                    self.solo_mode_active = solo_mode_active;
                    self.solo_origin_id = solo_origin_id;
                    self.temporarily_expanded_id = None;
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
                ServerMessage::WindowCreatedConfirmation { .. } => {
                    // Handled by launch command, safe to ignore here
                }
                ServerMessage::ScreenDiff {
                    window_id,
                    cells,
                    cursor_pos,
                    scrollback_size,
                    scroll_offset,
                } => {
                    if let Some(win) = self.windows.get_mut(&window_id) {
                        for (idx, cell) in cells {
                            if idx < win.screen.len() {
                                win.screen[idx] = cell;
                            }
                        }
                        win.cursor_pos = cursor_pos;
                        win.scrollback_size = scrollback_size;
                        win.scroll_offset = scroll_offset;
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
                ServerMessage::ClipboardUpdate { text_b64 } => {
                    // Forward OSC 52 sequence to the terminal
                    let osc = format!("\x1b]52;c;{}\x07", text_b64);
                    let _ = io::Write::write_all(&mut io::stdout(), osc.as_bytes());
                    let _ = io::Write::flush(&mut io::stdout());
                }
                ServerMessage::Shutdown => {
                    return Ok(true);
                }
                _ => {}
            },
        }
        Ok(false)
    }

    fn send_key_to_window(
        &mut self,
        id: usize,
        key: KeyEvent,
        is_over_window: bool,
    ) -> Result<bool> {
        let mut data = Vec::new();

        let has_alt = key.modifiers.contains(KeyModifiers::ALT);
        let has_ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let has_shift = key.modifiers.contains(KeyModifiers::SHIFT);

        // 1: none, 2: shift, 3: alt, 4: shift+alt, 5: ctrl, 6: shift+ctrl, 7: alt+ctrl, 8: shift+alt+ctrl
        let mut modifier_code = 1;
        if has_shift {
            modifier_code += 1;
        }
        if has_alt {
            modifier_code += 2;
        }
        if has_ctrl {
            modifier_code += 4;
        }

        // Handle global overrides (only if NOT over a window)
        if !is_over_window && has_shift {
            match key.code {
                KeyCode::PageUp => {
                    let _ = self.server_tx.try_send(ClientMessage::Scroll {
                        window_id: id,
                        amount: 10,
                    });
                    return Ok(false);
                }
                KeyCode::PageDown => {
                    let _ = self.server_tx.try_send(ClientMessage::Scroll {
                        window_id: id,
                        amount: -10,
                    });
                    return Ok(false);
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Char(c) => {
                if has_alt {
                    data.push(27);
                }
                if has_ctrl {
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
            KeyCode::Enter => {
                if has_alt {
                    data.push(27);
                }
                data.push(b'\r');
            }
            KeyCode::Backspace => {
                if has_alt {
                    data.push(27);
                }
                data.push(127);
            }
            KeyCode::Tab => {
                if has_alt {
                    data.push(27);
                }
                data.push(9);
            }
            KeyCode::BackTab => {
                data.extend_from_slice(b"\x1b[Z");
            }
            KeyCode::Esc => {
                data.push(27);
                if has_alt {
                    data.push(27);
                }
            }
            KeyCode::Up
            | KeyCode::Down
            | KeyCode::Right
            | KeyCode::Left
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Insert
            | KeyCode::Delete
            | KeyCode::F(_) => {
                let code = match key.code {
                    KeyCode::Up => "A",
                    KeyCode::Down => "B",
                    KeyCode::Right => "C",
                    KeyCode::Left => "D",
                    KeyCode::Home => "H",
                    KeyCode::End => "F",
                    KeyCode::PageUp => "5~",
                    KeyCode::PageDown => "6~",
                    KeyCode::Insert => "2~",
                    KeyCode::Delete => "3~",
                    KeyCode::F(n) => match n {
                        1 => "P",
                        2 => "Q",
                        3 => "R",
                        4 => "S",
                        5 => "15~",
                        6 => "17~",
                        7 => "18~",
                        8 => "19~",
                        9 => "20~",
                        10 => "21~",
                        11 => "23~",
                        12 => "24~",
                        _ => "",
                    },
                    _ => "",
                };

                if !code.is_empty() {
                    data.push(27); // ESC
                    if (1..=4).contains(&match key.code {
                        KeyCode::F(n) => n,
                        _ => 0,
                    }) && modifier_code == 1
                    {
                        data.push(b'O');
                        data.extend_from_slice(code.as_bytes());
                    } else if modifier_code > 1 {
                        data.push(b'[');
                        if let Some(base) = code.strip_suffix('~') {
                            data.extend_from_slice(
                                format!("{};{}~", base, modifier_code).as_bytes(),
                            );
                        } else {
                            data.extend_from_slice(
                                format!("1;{}{}", modifier_code, code).as_bytes(),
                            );
                        }
                    } else {
                        data.push(b'[');
                        data.extend_from_slice(code.as_bytes());
                    }
                }
            }
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

        // --- HOVER EXPAND (Solo Mode) ---
        if self.solo_mode_active && matches!(mouse.kind, MouseEventKind::Moved) {
            let mut should_collapse_current = false;
            let mut new_expand_id = None;

            match target {
                HitTarget::WindowTitle(id) if Some(id) != self.solo_origin_id => {
                    if self.temporarily_expanded_id != Some(id)
                        && self.windows.get(&id).map(|w| w.minimized).unwrap_or(false)
                    {
                        new_expand_id = Some(id);
                    }
                    // If it's already the expanded one, we stay expanded (do nothing)
                }
                _ => {
                    // Not over a non-solo title bar.
                    if let Some(tid) = self.temporarily_expanded_id {
                        let still_over = match target {
                            HitTarget::WindowContent(id)
                            | HitTarget::WindowTitle(id)
                            | HitTarget::WindowBorder(id)
                            | HitTarget::WindowResize(id)
                            | HitTarget::CloseButton(id)
                            | HitTarget::MinimizeButton(id)
                            | HitTarget::MaximizeButton(id)
                            | HitTarget::FullscreenButton(id)
                            | HitTarget::SoloButton(id)
                            | HitTarget::ResetButton(id)
                            | HitTarget::WindowScrollbar(id) => id == tid,
                            _ => false,
                        };
                        if !still_over {
                            should_collapse_current = true;
                        }
                    }
                }
            }

            if let Some(id) = new_expand_id {
                // Collapse old if any
                if let Some(tid) = self.temporarily_expanded_id {
                    let _ = self
                        .server_tx
                        .try_send(ClientMessage::TemporaryCollapse { window_id: tid });
                }
                // Expand new
                let _ = self
                    .server_tx
                    .try_send(ClientMessage::TemporaryExpand { window_id: id });
                self.temporarily_expanded_id = Some(id);
            } else if should_collapse_current && let Some(tid) = self.temporarily_expanded_id {
                let _ = self
                    .server_tx
                    .try_send(ClientMessage::TemporaryCollapse { window_id: tid });
                self.temporarily_expanded_id = None;
            }
        }

        // --- MOUSE MOVE FORWARDING (for nested TermPlex/Apps) ---
        if let MouseEventKind::Moved = mouse.kind
            && let HitTarget::WindowContent(id) = target
            && let Some(win) = self.windows.get(&id)
            && win.mouse_reporting
        {
            let rel_x = mouse.column.saturating_sub(win.x + 1);
            let rel_y = mouse.row.saturating_sub(win.y + 1);
            // Button 35 is Move with no button pressed in SGR 1006
            let data = format!("\x1b[<35;{};{}M", rel_x + 1, rel_y + 1).into_bytes();
            let _ = self.server_tx.try_send(ClientMessage::Input {
                window_id: id,
                data,
            });
        }

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
                    HitTarget::DeskbarMinimizeButton => {
                        if btn == MouseButton::Left {
                            self.deskbar_minimized = !self.deskbar_minimized;
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

                        if win.mouse_reporting {
                            let mut sgr_btn = match btn {
                                MouseButton::Left => 0,
                                MouseButton::Middle => 1,
                                MouseButton::Right => 2,
                            };
                            if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                                sgr_btn += 4;
                            }
                            if mouse.modifiers.contains(KeyModifiers::ALT) {
                                sgr_btn += 8;
                            }
                            if mouse.modifiers.contains(KeyModifiers::CONTROL) {
                                sgr_btn += 16;
                            }

                            let data = format!("\x1b[<{};{};{}M", sgr_btn, rel_x + 1, rel_y + 1)
                                .into_bytes();
                            let _ = self.server_tx.try_send(ClientMessage::Input {
                                window_id: id,
                                data,
                            });
                        } else if btn == MouseButton::Left {
                            self.pending_selection = Some((id, rel_y, rel_x));
                            self.selection = None;
                        } else if btn == MouseButton::Right {
                            // Try to paste from system clipboard first
                            let mut pasted = false;
                            if let Some(ref mut cb) = self.clipboard_manager
                                && let Ok(text) = cb.get_text()
                            {
                                let data = text.into_bytes();
                                let _ = self.server_tx.try_send(ClientMessage::Input {
                                    window_id: id,
                                    data,
                                });
                                pasted = true;
                            }

                            // Fallback to internal clipboard if system clipboard failed
                            if !pasted && !self.clipboard.is_empty() {
                                let data = self.clipboard.clone().into_bytes();
                                let _ = self.server_tx.try_send(ClientMessage::Input {
                                    window_id: id,
                                    data,
                                });
                                self.clipboard.clear();
                            }
                        }
                    }
                    HitTarget::WindowScrollbar(id) => {
                        self.active_window_id = Some(id);
                        let _ = self
                            .server_tx
                            .try_send(ClientMessage::FocusWindow { window_id: id });
                        if btn == MouseButton::Left {
                            self.scrollbar_drag = Some(id);
                            let win = self.windows.get(&id).unwrap();
                            let inner_h = win.height.saturating_sub(2);
                            let scrollback = win.scrollback_size;
                            if inner_h > 0 && scrollback > 0 {
                                // Calculate thumb height to match rendering
                                let total_h = (inner_h as usize + scrollback).max(1);
                                let thumb_h = ((inner_h as f32 * inner_h as f32) / total_h as f32)
                                    .round() as u16;
                                let thumb_h = thumb_h.clamp(1, inner_h);
                                let available_track = inner_h.saturating_sub(thumb_h);

                                if available_track > 0 {
                                    let rel_y = mouse.row.saturating_sub(win.y + 1);
                                    let rel_y_clamped = rel_y.min(available_track);
                                    let ratio =
                                        1.0 - (rel_y_clamped as f32 / available_track as f32);
                                    let offset = (ratio * scrollback as f32).round() as usize;

                                    if let Some(win_mut) = self.windows.get_mut(&id) {
                                        win_mut.scroll_offset = offset;
                                    }
                                    let _ = self.server_tx.try_send(ClientMessage::ScrollTo {
                                        window_id: id,
                                        offset,
                                    });
                                    return Ok(false);
                                }
                            }
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
                                let is_mgmt = mouse.modifiers.contains(KeyModifiers::CONTROL);
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
                            && mouse.modifiers.contains(KeyModifiers::CONTROL)
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
                if let Some(id) = self.scrollbar_drag {
                    if btn == MouseButton::Left
                        && let Some(win) = self.windows.get(&id)
                    {
                        let inner_h = win.height.saturating_sub(2);
                        let scrollback = win.scrollback_size;
                        if inner_h > 0 && scrollback > 0 {
                            let total_h = (inner_h as usize + scrollback).max(1);
                            let thumb_h =
                                ((inner_h as f32 * inner_h as f32) / total_h as f32).round() as u16;
                            let thumb_h = thumb_h.clamp(1, inner_h);
                            let available_track = inner_h.saturating_sub(thumb_h);

                            if available_track > 0 {
                                let rel_y = mouse.row.saturating_sub(win.y + 1);
                                let rel_y_clamped = rel_y.min(available_track);
                                let ratio = 1.0 - (rel_y_clamped as f32 / available_track as f32);
                                let offset = (ratio * scrollback as f32).round() as usize;

                                if offset != win.scroll_offset {
                                    // Optimistic update for immediate visual feedback
                                    if let Some(win_mut) = self.windows.get_mut(&id) {
                                        win_mut.scroll_offset = offset;
                                    }
                                    let _ = self.server_tx.try_send(ClientMessage::ScrollTo {
                                        window_id: id,
                                        offset,
                                    });
                                    return Ok(false);
                                }
                            }
                        }
                    }
                    return Ok(false);
                }

                if let Some(id) = self.active_window_id {
                    // Check if mouse is actually over the active window content
                    if matches!(target, HitTarget::WindowContent(tid) if tid == id)
                        && let Some(win) = self.windows.get(&id)
                    {
                        if win.mouse_reporting {
                            let rel_x = mouse.column.saturating_sub(win.x + 1);
                            let rel_y = mouse.row.saturating_sub(win.y + 1);
                            let mut sgr_btn = match btn {
                                MouseButton::Left => 32,
                                MouseButton::Middle => 33,
                                MouseButton::Right => 34,
                            };
                            if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                                sgr_btn += 4;
                            }
                            if mouse.modifiers.contains(KeyModifiers::ALT) {
                                sgr_btn += 8;
                            }
                            if mouse.modifiers.contains(KeyModifiers::CONTROL) {
                                sgr_btn += 16;
                            }

                            let data = format!("\x1b[<{};{};{}M", sgr_btn, rel_x + 1, rel_y + 1)
                                .into_bytes();
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
            }
            MouseEventKind::Up(btn) => {
                if btn == MouseButton::Left {
                    self.scrollbar_drag = None;
                    if let Some(sel) = self.selection
                        && let Some(win) = self.windows.get(&sel.window_id)
                        && sel.start != sel.end
                    {
                        let mut text = String::new();
                        let (start_r, start_c) = sel.start;
                        let (end_r, end_c) = sel.end;

                        // Determine actual start and end (linear order)
                        let (r1, c1, r2, c2) =
                            if start_r < end_r || (start_r == end_r && start_c <= end_c) {
                                (start_r, start_c, end_r, end_c)
                            } else {
                                (end_r, end_c, start_r, start_c)
                            };

                        let inner_w = win.width.saturating_sub(3) as usize;
                        for r in r1..=r2 {
                            let mut line = String::new();
                            for c in 0..inner_w {
                                let col = c as u16;
                                // Skip columns before the start on the first row
                                if r == r1 && col < c1 {
                                    continue;
                                }
                                // Skip columns after the end on the last row
                                if r == r2 && col > c2 {
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
                        // Copy to system clipboard
                        if let Some(ref mut cb) = self.clipboard_manager {
                            let _ = cb.set_text(text.clone());
                        }
                        // Always send OSC 52 for terminal/remote integration
                        self.send_osc52_copy(&text);
                        self.clipboard = text;
                    }
                    self.selection = None;
                    self.pending_selection = None;
                }

                if let Some(id) = self.active_window_id
                    && let Some(win) = self.windows.get(&id)
                    && win.mouse_reporting
                {
                    let mut sgr_btn = match btn {
                        MouseButton::Left => 0,
                        MouseButton::Middle => 1,
                        MouseButton::Right => 2,
                    };
                    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                        sgr_btn += 4;
                    }
                    if mouse.modifiers.contains(KeyModifiers::ALT) {
                        sgr_btn += 8;
                    }
                    if mouse.modifiers.contains(KeyModifiers::CONTROL) {
                        sgr_btn += 16;
                    }

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
            MouseEventKind::ScrollUp => {
                if let Some(id) = self.active_window_id
                    && matches!(target, HitTarget::WindowContent(tid) if tid == id)
                    && let Some(win) = self.windows.get(&id)
                    && win.mouse_reporting
                {
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
                }
            }
            MouseEventKind::ScrollDown => {
                if let Some(id) = self.active_window_id
                    && matches!(target, HitTarget::WindowContent(tid) if tid == id)
                    && let Some(win) = self.windows.get(&id)
                    && win.mouse_reporting
                {
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
                        let config = bincode::config::standard().with_fixed_int_encoding();
                        match bincode::serde::decode_from_slice::<ServerMessage, _>(
                            &accum[4..4 + len],
                            config,
                        ) {
                            Ok((msg, _)) => {
                                if tx_server.send(AppEvent::Server(msg)).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                eprintln!("Bincode deserialization error: {}", e);
                            }
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
                            win.width.saturating_sub(3),
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

                        // Scrollbar
                        let scroll_x = win.x + win.width - 2;
                        let inner_h = win.height.saturating_sub(2);
                        if inner_h > 0 && !win.minimized {
                            for i in 0..inner_h {
                                let sy = win.y + 1 + i;
                                if window_area.contains((scroll_x, sy).into()) {
                                    f.buffer_mut()[(scroll_x, sy)]
                                        .set_char('│')
                                        .set_style(Style::default().fg(Color::DarkGray));
                                }
                            }

                            // Scroll handle (thumb) - Proportional
                            let scrollback = win.scrollback_size;
                            let total_h = (inner_h as usize + scrollback).max(1);

                            // Thumb height proportional to visible area
                            let thumb_h_f = (inner_h as f32 * inner_h as f32) / total_h as f32;
                            let thumb_h = (thumb_h_f.ceil() as u16).clamp(1, inner_h);

                            // Thumb position mapping scrollback_size to available track
                            let thumb_y = if scrollback > 0 {
                                let available_track = inner_h.saturating_sub(thumb_h);
                                let scroll_ratio = win.scroll_offset as f32 / scrollback as f32;
                                // 0 offset means bottom, so ratio 0.0 -> offset = available_track
                                let thumb_offset =
                                    ((1.0 - scroll_ratio) * available_track as f32).round() as u16;
                                (win.y + 1) + thumb_offset
                            } else {
                                win.y + 1
                            };

                            for h in 0..thumb_h {
                                let sy = thumb_y + h;
                                if window_area.contains((scroll_x, sy).into()) {
                                    f.buffer_mut()[(scroll_x, sy)]
                                        .set_char('█')
                                        .set_style(Style::default().fg(Color::Cyan));
                                }
                            }
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

                let has_fullscreen = client.windows.values().any(|w| w.fullscreen);

                // --- MENU BAR ---
                if !has_fullscreen {
                    let menu_rect = Rect::new(0, 0, size.width, 1);
                    let menu_style = Style::default()
                        .bg(Color::Rgb(50, 50, 100))
                        .fg(Color::White);
                    f.render_widget(Block::default().style(menu_style), menu_rect);

                    let menus = [(Menu::File, " File "), (Menu::Window, " Window ")];

                    let mut x_offset = 2;
                    for (m, label) in menus {
                        let style = if client.menu == m {
                            Style::default()
                                .bg(Color::White)
                                .fg(Color::Black)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::White)
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
                                    " Clear History (L)",
                                ],
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
                }

                // --- DESKBAR (Overlay, Dynamic Height) ---
                if !has_fullscreen {
                    if client.deskbar_minimized {
                        // Render a tiny [+] button in the top right corner of the menu bar
                        let deskbar_area = Rect::new(size.width.saturating_sub(4), 0, 4, 1);
                        f.render_widget(Clear, deskbar_area);
                        f.render_widget(
                            Paragraph::new(" [+] ")
                                .style(Style::default().bg(Color::Rgb(15, 15, 30)).fg(Color::Cyan)),
                            deskbar_area,
                        );
                    } else {
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
                                .title(
                                    Line::from(vec![
                                        Span::raw(" TP "),
                                        Span::styled("[-] ", Style::default().fg(Color::Cyan)),
                                    ])
                                    .alignment(Alignment::Right),
                                )
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
                    }
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
