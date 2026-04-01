mod terminal;
mod widgets;
mod window;

use crate::widgets::TerminalWidget;
use crate::window::Window;
use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
        MouseButton, MouseEvent, MouseEventKind,
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
use serde::{Deserialize, Serialize};
use std::sync::mpsc::{self, Sender};
use std::{fs, io};

/// Default terminal dimensions when creating a new terminal
const DEFAULT_TERMINAL_WIDTH: u16 = 80;
const DEFAULT_TERMINAL_HEIGHT: u16 = 24;

/// Minimum terminal dimensions
const MIN_TERMINAL_WIDTH: u16 = 10;
const MIN_TERMINAL_HEIGHT: u16 = 3;

/// Window title bar height when minimized
const MINIMIZED_HEIGHT: u16 = 3;

#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub enum Mode {
    Terminal,
    Desktop,
}

#[derive(Debug, Clone)]
pub enum Action {
    Quit,
    SpawnTerminal(u16, u16, Option<String>, Vec<String>),
    CloseWindow(usize),
    FocusWindow(usize),
    NextWindow,
    PrevWindow,
    SwitchMode(Mode),
    ResizeWindow(usize, Rect),
    MoveWindow(usize, u16, u16),
    MaximizeWindow(usize),
    MinimizeWindow(usize),
    SaveLayout,
}

#[derive(Debug)]
pub enum AppEvent {
    Crossterm(Event),
    TerminalUpdate,
}

#[derive(Serialize, Deserialize)]
struct WindowLayout {
    title: String,
    rect: (u16, u16, u16, u16),
    minimized: bool,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct FullLayout {
    windows: Vec<WindowLayout>,
}

struct DragState {
    window_idx: usize,
    start_mouse: (u16, u16),
    start_rect: Rect,
    is_resize: bool,
}

struct App {
    windows: Vec<Window>,
    active_window_index: Option<usize>,
    mode: Mode,
    drag_state: Option<DragState>,
    event_tx: Sender<AppEvent>,
    last_screen_size: Rect,
}

impl App {
    /// Create a new App instance
    fn new(event_tx: Sender<AppEvent>) -> Self {
        Self {
            windows: Vec::new(),
            active_window_index: None,
            mode: Mode::Terminal,
            drag_state: None,
            event_tx,
            last_screen_size: Rect::default(),
        }
    }

    /// Perform an action on the app
    fn perform_action(&mut self, action: Action) -> Result<bool> {
        match action {
            Action::Quit => return Ok(true),
            Action::SpawnTerminal(x, y, cmd, args) => {
                let title = format!("Terminal {}", self.windows.len() + 1);
                self.add_window(
                    title,
                    Rect::new(
                        x,
                        y,
                        DEFAULT_TERMINAL_WIDTH.max(MIN_TERMINAL_WIDTH),
                        DEFAULT_TERMINAL_HEIGHT.max(MIN_TERMINAL_HEIGHT),
                    ),
                    cmd,
                    args,
                )?;
            }
            Action::CloseWindow(idx) => self.remove_window(idx),
            Action::FocusWindow(idx) => self.focus_window(idx),
            Action::NextWindow => {
                if !self.windows.is_empty() {
                    self.focus_window(0);
                }
            }
            Action::PrevWindow => {
                if self.windows.len() > 1 {
                    let idx = self.windows.len() - 1;
                    let win = self.windows.remove(idx);
                    self.windows.insert(0, win);
                    self.focus_window(self.windows.len() - 1);
                }
            }
            Action::SwitchMode(mode) => self.mode = mode,
            Action::ResizeWindow(idx, rect) => {
                if let Some(win) = self.windows.get_mut(idx) {
                    win.resize(rect)?;
                }
            }
            Action::MoveWindow(idx, x, y) => {
                if let Some(win) = self.windows.get_mut(idx) {
                    win.rect.x = x;
                    win.rect.y = y;
                }
            }
            Action::MaximizeWindow(idx) => self.maximize_window(idx)?,
            Action::MinimizeWindow(idx) => {
                if let Some(win) = self.windows.get_mut(idx) {
                    win.minimized = !win.minimized;
                }
            }
            Action::SaveLayout => self.save_layout()?,
        }
        Ok(false)
    }

    /// Resize all windows when the terminal is resized
    fn resize_all(&mut self, new_w: u16, new_h: u16) -> Result<()> {
        let old_w = self.last_screen_size.width;
        let old_h = self.last_screen_size.height;
        if old_w == 0 || old_h == 0 {
            self.last_screen_size = Rect::new(0, 0, new_w, new_h);
            return Ok(());
        }
        let ratio_x = new_w as f32 / old_w as f32;
        let ratio_y = new_h as f32 / old_h as f32;
        for win in &mut self.windows {
            let mut nr = win.rect;
            nr.x = (nr.x as f32 * ratio_x).round() as u16;
            nr.y = (nr.y as f32 * ratio_y).round() as u16;
            nr.width = (nr.width as f32 * ratio_x).round() as u16;
            nr.height = (nr.height as f32 * ratio_y).round() as u16;
            nr.width = nr.width.max(MIN_TERMINAL_WIDTH);
            nr.height = nr.height.max(MIN_TERMINAL_HEIGHT);
            if nr.x + nr.width > new_w {
                nr.x = new_w.saturating_sub(nr.width);
            }
            if nr.y + nr.height > new_h {
                nr.y = new_h.saturating_sub(nr.height);
            }
            win.resize(nr)?;
        }
        self.last_screen_size = Rect::new(0, 0, new_w, new_h);
        Ok(())
    }

    /// Save the current layout to layout.json
    fn save_layout(&self) -> Result<()> {
        let layout = FullLayout {
            windows: self
                .windows
                .iter()
                .map(|w| WindowLayout {
                    title: w.title.clone(),
                    rect: (w.rect.x, w.rect.y, w.rect.width, w.rect.height),
                    minimized: w.minimized,
                    command: w.command.clone(),
                    args: w.args.clone(),
                })
                .collect(),
        };
        fs::write("layout.json", serde_json::to_string_pretty(&layout)?)?;
        Ok(())
    }

    /// Load a layout from a JSON file
    fn load_layout(&mut self, path: &str) -> Result<()> {
        let layout: FullLayout = serde_json::from_str(&fs::read_to_string(path)?)?;
        for wl in layout.windows {
            let rect = Rect::new(wl.rect.0, wl.rect.1, wl.rect.2, wl.rect.3);
            self.add_window(wl.title, rect, wl.command, wl.args)?;
            if let Some(win) = self.windows.last_mut() {
                win.minimized = wl.minimized;
            }
        }
        Ok(())
    }

    /// Maximize a window to fill available space
    fn maximize_window(&mut self, idx: usize) -> Result<()> {
        if idx >= self.windows.len() {
            return Ok(());
        }
        let mut new_rect = self.windows[idx].rect;
        if let Some(saved) = self.windows[idx].saved_rect {
            self.windows[idx].saved_rect = None;
            return self.windows[idx].resize(saved);
        }
        let old_rect = self.windows[idx].rect;
        let screen = self.last_screen_size;
        let intersects = |r: Rect, windows: &[Window], cur: usize| -> bool {
            for (i, w) in windows.iter().enumerate() {
                if i != cur && !w.minimized && r.intersects(w.rect) {
                    return true;
                }
            }
            false
        };
        while new_rect.y > 1 {
            let mut t = new_rect;
            t.y -= 1;
            t.height += 1;
            if intersects(t, &self.windows, idx) {
                break;
            }
            new_rect = t;
        }
        while new_rect.y + new_rect.height < screen.height.saturating_sub(1) {
            let mut t = new_rect;
            t.height += 1;
            if intersects(t, &self.windows, idx) {
                break;
            }
            new_rect = t;
        }
        while new_rect.x > 1 {
            let mut t = new_rect;
            t.x -= 1;
            t.width += 1;
            if intersects(t, &self.windows, idx) {
                break;
            }
            new_rect = t;
        }
        while new_rect.x + new_rect.width < screen.width.saturating_sub(1) {
            let mut t = new_rect;
            t.width += 1;
            if intersects(t, &self.windows, idx) {
                break;
            }
            new_rect = t;
        }
        if new_rect != old_rect {
            self.windows[idx].saved_rect = Some(old_rect);
            self.windows[idx].resize(new_rect)?;
        }
        Ok(())
    }

    fn add_window(
        &mut self,
        title: String,
        rect: Rect,
        command: Option<String>,
        args: Vec<String>,
    ) -> Result<()> {
        let mut window = Window::new(
            self.windows.len(),
            title,
            rect,
            self.event_tx.clone(),
            command,
            args,
        )?;
        window.focused = true;
        for w in &mut self.windows {
            w.focused = false;
        }
        self.windows.push(window);
        self.active_window_index = Some(self.windows.len() - 1);
        Ok(())
    }

    /// Focus a window and move it to the end of the list
    fn focus_window(&mut self, idx: usize) {
        if idx >= self.windows.len() {
            return;
        }
        for (i, w) in self.windows.iter_mut().enumerate() {
            w.focused = i == idx;
        }
        let focused = self.windows.remove(idx);
        self.windows.push(focused);
        self.active_window_index = Some(self.windows.len() - 1);
    }

    /// Remove a window by index
    fn remove_window(&mut self, idx: usize) {
        if idx < self.windows.len() {
            self.windows.remove(idx);
            if self.windows.is_empty() {
                self.active_window_index = None;
            } else {
                let last = self.windows.len() - 1;
                self.active_window_index = Some(last);
                self.windows[last].focused = true;
            }
        }
    }

    /// Handle terminal scrolling for a specific window
    fn handle_scroll(&mut self, window_idx: usize, scroll_amount: i32) -> Result<()> {
        if let Some(window) = self.windows.get_mut(window_idx) {
            let max_scroll = {
                let mut p = window.terminal.parser.lock().unwrap();
                let old_offset = p.screen().scrollback();
                p.screen_mut().set_scrollback(usize::MAX);
                let max = p.screen().scrollback();
                p.screen_mut().set_scrollback(old_offset);
                max
            };
            window.scroll_offset = (window.scroll_offset as i32 + scroll_amount)
                .max(0)
                .min(max_scroll as i32) as usize;
        }
        Ok(())
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let (tx, rx) = mpsc::channel();
    let tx_ct = tx.clone();
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if tx_ct.send(AppEvent::Crossterm(ev)).is_err() {
                break;
            }
        }
    });

    let mut app = App::new(tx);
    app.last_screen_size = terminal.size()?.into();

    if args.len() > 1 {
        let _ = app.load_layout(&args[1]);
    } else {
        app.perform_action(Action::SpawnTerminal(5, 2, None, vec![]))?;
    }

    loop {
        terminal.draw(|f| {
            let size = f.area();
            f.render_widget(Block::default().title(" TermPlex v0.1 ").borders(Borders::ALL).style(Style::default().bg(Color::Rgb(15, 15, 25)).fg(Color::DarkGray)), size);
            for window in &app.windows {
                let render_rect = if window.minimized { Rect::new(window.rect.x, window.rect.y, window.rect.width, MINIMIZED_HEIGHT) } else { window.rect };
                let shadow_area = Rect::new(render_rect.x + 1, render_rect.y + 1, render_rect.width, render_rect.height).intersection(size);
                if !shadow_area.is_empty() && window.focused { f.render_widget(Block::default().style(Style::default().bg(Color::Rgb(30, 30, 30))), shadow_area); }
                let window_area = render_rect.intersection(size);
                if window_area.is_empty() { continue; }
                f.render_widget(Clear, window_area);
                f.render_widget(Block::default().title(format!(" [X] [^] [_] {} ", window.title)).borders(Borders::ALL).border_style(if window.focused { Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD) } else { Style::default().fg(Color::Gray) }).style(Style::default().bg(Color::Black)), window_area);
                if !window.minimized {
                    let inner_area = Rect::new(window.rect.x + 1, window.rect.y + 1, window.rect.width.saturating_sub(2), window.rect.height.saturating_sub(2)).intersection(size);
                    if !inner_area.is_empty() { f.render_widget(TerminalWidget { parser: window.terminal.parser.clone(), running: window.terminal.running.clone(), exit_code: window.terminal.exit_code.clone(), scroll_offset: window.scroll_offset }, inner_area); }
                    let handle_x = window.rect.x + window.rect.width - 1;
                    let handle_y = window.rect.y + window.rect.height - 1;
                    if handle_x < size.width && handle_y < size.height {
                        let style = if window.focused { Style::default().fg(Color::Cyan) } else { Style::default().fg(Color::Gray) };
                        f.buffer_mut()[(handle_x, handle_y)].set_char('◢').set_style(style);
                    }
                }
            }
            let status_rect = Rect::new(0, size.height - 1, size.width, 1);
            let (status_text, style) = if app.mode == Mode::Desktop {
                (" [DESKTOP] | p: Save | Tab: Focus | Arrows: Move | WASD: Resize | Z/X/C: Action ", Style::default().bg(Color::Yellow).fg(Color::Black).add_modifier(Modifier::BOLD))
            } else {
                (" [F12: Switch Mode] | Hint: Drag titlebar to move, ◢ to resize, Ctrl+Right: New ", Style::default().bg(Color::Rgb(40, 40, 80)).fg(Color::White).add_modifier(Modifier::BOLD))
            };
            f.render_widget(Paragraph::new(status_text).style(style), status_rect);
        })?;

        match rx.recv()? {
            AppEvent::TerminalUpdate => {}
            AppEvent::Crossterm(ev) => match ev {
                Event::Resize(w, h) => {
                    app.resize_all(w, h)?;
                }
                Event::Key(key) => {
                    if key.kind != event::KeyEventKind::Press {
                        continue;
                    }
                    if key.code == KeyCode::F(12) {
                        app.perform_action(Action::SwitchMode(if app.mode == Mode::Terminal {
                            Mode::Desktop
                        } else {
                            Mode::Terminal
                        }))?;
                    } else if app.mode == Mode::Desktop {
                        match (key.modifiers, key.code) {
                            (KeyModifiers::CONTROL, KeyCode::Char('q')) => {
                                if app.perform_action(Action::Quit)? {
                                    break;
                                }
                            }
                            (KeyModifiers::CONTROL, KeyCode::Char('n')) => {
                                app.perform_action(Action::SpawnTerminal(10, 5, None, vec![]))?;
                            }
                            _ => handle_desktop_key(&mut app, key)?,
                        }
                    } else {
                        handle_terminal_key(&mut app, key)?;
                    }
                }
                Event::Mouse(mouse) => {
                    if !handle_mouse(&mut app, mouse)? {
                        handle_terminal_mouse(&mut app, mouse)?;
                    }
                }
                _ => {}
            },
        }
        while let Ok(AppEvent::TerminalUpdate) = rx.try_recv() {}
    }
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
    Ok(())
}

/// Handle terminal-specific mouse events (scrolling, input)
fn handle_terminal_mouse(app: &mut App, mouse: MouseEvent) -> Result<()> {
    for i in (0..app.windows.len()).rev() {
        let window = &mut app.windows[i];
        if window.minimized {
            continue;
        }
        let rel_x = (mouse.column as i32 - window.rect.x as i32 - 1) + 1;
        let rel_y = (mouse.row as i32 - window.rect.y as i32 - 1) + 1;
        if rel_x >= 1
            && rel_x <= (window.rect.width - 2) as i32
            && rel_y >= 1
            && rel_y <= (window.rect.height - 2) as i32
        {
            match mouse.kind {
                MouseEventKind::ScrollUp if !mouse.modifiers.contains(KeyModifiers::CONTROL) => {
                    let _max_scroll = {
                        let mut p = window.terminal.parser.lock().unwrap();
                        let old_offset = p.screen().scrollback();
                        p.screen_mut().set_scrollback(usize::MAX);
                        let max = p.screen().scrollback();
                        p.screen_mut().set_scrollback(old_offset);
                        max
                    };
                    app.handle_scroll(i, 3)?;
                    return Ok(());
                }
                MouseEventKind::ScrollDown if !mouse.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.handle_scroll(i, -3)?;
                    return Ok(());
                }
                _ => {}
            }
            // Only send other mouse events to the focused window to avoid accidental clicks/typing in background windows
            if !window.focused {
                return Ok(());
            }

            let button = match mouse.kind {
                MouseEventKind::Down(MouseButton::Left)
                | MouseEventKind::Drag(MouseButton::Left) => 0,
                MouseEventKind::Down(MouseButton::Right)
                | MouseEventKind::Drag(MouseButton::Right) => 2,
                MouseEventKind::Down(MouseButton::Middle)
                | MouseEventKind::Drag(MouseButton::Middle) => 1,
                MouseEventKind::ScrollUp => 64,
                MouseEventKind::ScrollDown => 65,
                _ => return Ok(()),
            };
            let mut mods = 0;
            if mouse.modifiers.contains(KeyModifiers::SHIFT) {
                mods |= 4;
            }
            if mouse.modifiers.contains(KeyModifiers::ALT) {
                mods |= 8;
            }
            if mouse.modifiers.contains(KeyModifiers::CONTROL) {
                mods |= 16;
            }
            if let MouseEventKind::Drag(_) = mouse.kind {
                mods |= 32;
            }
            let final_char = if matches!(mouse.kind, MouseEventKind::Up(_)) {
                'm'
            } else {
                'M'
            };
            window.terminal.write(
                format!("\x1b[<{};{};{}{}", button | mods, rel_x, rel_y, final_char).as_bytes(),
            )?;
            return Ok(());
        }
    }
    Ok(())
}

/// Handle mouse events for window management
fn handle_mouse(app: &mut App, mouse: MouseEvent) -> Result<bool> {
    if let Some(state) = &app.drag_state {
        if matches!(mouse.kind, MouseEventKind::Up(_)) {
            app.drag_state = None;
            return Ok(true);
        }
        if let MouseEventKind::Drag(MouseButton::Left) = mouse.kind {
            let dx = mouse.column as i32 - state.start_mouse.0 as i32;
            let dy = mouse.row as i32 - state.start_mouse.1 as i32;
            if state.is_resize {
                let mut nr = state.start_rect;
                nr.width =
                    (state.start_rect.width as i32 + dx).max(MIN_TERMINAL_WIDTH as i32) as u16;
                nr.height =
                    (state.start_rect.height as i32 + dy).max(MIN_TERMINAL_HEIGHT as i32) as u16;
                app.perform_action(Action::ResizeWindow(state.window_idx, nr))?;
            } else {
                let nx = (state.start_rect.x as i32 + dx).max(0) as u16;
                let ny = (state.start_rect.y as i32 + dy).max(0) as u16;
                app.perform_action(Action::MoveWindow(state.window_idx, nx, ny))?;
            }
            return Ok(true);
        }
    }
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Right)
            if mouse.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            app.perform_action(Action::SpawnTerminal(mouse.column, mouse.row, None, vec![]))?;
            Ok(true)
        }
        MouseEventKind::Down(MouseButton::Left) => {
            for i in (0..app.windows.len()).rev() {
                let window = &app.windows[i];
                let rr = if window.minimized {
                    Rect::new(
                        window.rect.x,
                        window.rect.y,
                        window.rect.width,
                        MINIMIZED_HEIGHT,
                    )
                } else {
                    window.rect
                };
                if mouse.column >= rr.x
                    && mouse.column < rr.x + rr.width
                    && mouse.row >= rr.y
                    && mouse.row < rr.y + rr.height
                {
                    let is_title = mouse.row == rr.y;
                    let is_resize_handle = !window.minimized
                        && mouse.column == rr.x + rr.width - 1
                        && mouse.row == rr.y + rr.height - 1;
                    let is_mgmt = app.mode == Mode::Desktop
                        || mouse
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
                    if is_title || is_resize_handle || is_mgmt {
                        if is_title {
                            if mouse.column >= rr.x + 2 && mouse.column <= rr.x + 4 {
                                app.perform_action(Action::CloseWindow(i))?;
                                return Ok(true);
                            }
                            if mouse.column >= rr.x + 6 && mouse.column <= rr.x + 8 {
                                app.perform_action(Action::MaximizeWindow(i))?;
                                return Ok(true);
                            }
                            if mouse.column >= rr.x + 10 && mouse.column <= rr.x + 12 {
                                app.perform_action(Action::MinimizeWindow(i))?;
                                return Ok(true);
                            }
                        }
                        let wr = window.rect;
                        let min = window.minimized;
                        app.perform_action(Action::FocusWindow(i))?;
                        let is_resize = is_resize_handle
                            || (!min
                                && is_mgmt
                                && mouse.column >= rr.x + rr.width - 2
                                && mouse.row >= rr.y + rr.height - 1);
                        app.drag_state = Some(DragState {
                            window_idx: app.windows.len() - 1,
                            start_mouse: (mouse.column, mouse.row),
                            start_rect: wr,
                            is_resize,
                        });
                        return Ok(true);
                    } else if !window.focused {
                        app.perform_action(Action::FocusWindow(i))?;
                        return Ok(true);
                    }
                    break;
                }
            }
            Ok(false)
        }
        _ => Ok(false),
    }
}

/// Handle keyboard events in desktop mode
fn handle_desktop_key(app: &mut App, key: KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Tab => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.perform_action(Action::PrevWindow)?;
            } else {
                app.perform_action(Action::NextWindow)?;
            }
        }
        _ => {
            if let Some(idx) = app.active_window_index {
                match key.code {
                    KeyCode::Char('p') => {
                        app.perform_action(Action::SaveLayout)?;
                    }
                    KeyCode::Char('z') => {
                        app.perform_action(Action::CloseWindow(idx))?;
                    }
                    KeyCode::Char('x') => {
                        app.perform_action(Action::MaximizeWindow(idx))?;
                    }
                    KeyCode::Char('c') => {
                        app.perform_action(Action::MinimizeWindow(idx))?;
                    }
                    KeyCode::Left => {
                        let win = &app.windows[idx];
                        app.perform_action(Action::MoveWindow(
                            idx,
                            win.rect.x.saturating_sub(1),
                            win.rect.y,
                        ))?;
                    }
                    KeyCode::Right => {
                        let win = &app.windows[idx];
                        app.perform_action(Action::MoveWindow(idx, win.rect.x + 1, win.rect.y))?;
                    }
                    KeyCode::Up => {
                        let win = &app.windows[idx];
                        app.perform_action(Action::MoveWindow(
                            idx,
                            win.rect.x,
                            win.rect.y.saturating_sub(1),
                        ))?;
                    }
                    KeyCode::Down => {
                        let win = &app.windows[idx];
                        app.perform_action(Action::MoveWindow(idx, win.rect.x, win.rect.y + 1))?;
                    }
                    KeyCode::Char('a') => {
                        let mut r = app.windows[idx].rect;
                        r.width = r.width.saturating_sub(1).max(MIN_TERMINAL_WIDTH);
                        app.perform_action(Action::ResizeWindow(idx, r))?;
                    }
                    KeyCode::Char('d') => {
                        let mut r = app.windows[idx].rect;
                        r.width += 1;
                        app.perform_action(Action::ResizeWindow(idx, r))?;
                    }
                    KeyCode::Char('w') => {
                        let mut r = app.windows[idx].rect;
                        r.height = r.height.saturating_sub(1).max(MIN_TERMINAL_HEIGHT);
                        app.perform_action(Action::ResizeWindow(idx, r))?;
                    }
                    KeyCode::Char('s') => {
                        let mut r = app.windows[idx].rect;
                        r.height += 1;
                        app.perform_action(Action::ResizeWindow(idx, r))?;
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

/// Handle keyboard events in terminal mode
fn handle_terminal_key(app: &mut App, key: KeyEvent) -> Result<()> {
    if let Some(idx) = app.active_window_index {
        let window = &mut app.windows[idx];

        // Handle scrolling keys
        match (key.modifiers, key.code) {
            (KeyModifiers::SHIFT, KeyCode::PageUp) => {
                let scroll_amount = window.rect.height as usize - 2;
                app.handle_scroll(idx, scroll_amount as i32)?;
                return Ok(());
            }
            (KeyModifiers::SHIFT, KeyCode::PageDown) => {
                let scroll_amount = (window.rect.height as usize - 2) as i32;
                app.handle_scroll(idx, scroll_amount)?;
                return Ok(());
            }
            _ => {
                // For any other key (including unshifted PageUp/PageDown), reset scroll unless it's just a modifier change
                if !matches!(key.code, KeyCode::Null)
                    && !key.modifiers.contains(KeyModifiers::SHIFT)
                {
                    window.scroll_offset = 0;
                }
            }
        }

        let mut b = Vec::new();
        if key.modifiers.contains(KeyModifiers::ALT) {
            b.push(27);
        }
        match key.code {
            KeyCode::Char(c) => {
                let mut buf = [0u8; 4];
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    if c.is_ascii_lowercase() {
                        b.push((c as u8) - b'a' + 1);
                    } else if c.is_ascii_uppercase() {
                        b.push((c as u8) - b'A' + 1);
                    } else {
                        match c {
                            '[' => b.push(27),
                            '\\' => b.push(28),
                            ']' => b.push(29),
                            '^' => b.push(30),
                            '_' => b.push(31),
                            ' ' => b.push(0),
                            _ => b.extend_from_slice(c.encode_utf8(&mut buf).as_bytes()),
                        }
                    }
                } else {
                    b.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
            }
            KeyCode::Enter => b.push(b'\r'),
            KeyCode::Backspace => b.push(127),
            KeyCode::Tab => b.push(9),
            KeyCode::Esc => b.push(27),
            KeyCode::Up => b.extend_from_slice(b"\x1b[A"),
            KeyCode::Down => b.extend_from_slice(b"\x1b[B"),
            KeyCode::Right => b.extend_from_slice(b"\x1b[C"),
            KeyCode::Left => b.extend_from_slice(b"\x1b[D"),
            KeyCode::Home => b.extend_from_slice(b"\x1b[H"),
            KeyCode::End => b.extend_from_slice(b"\x1b[F"),
            KeyCode::Insert => b.extend_from_slice(b"\x1b[2~"),
            KeyCode::Delete => b.extend_from_slice(b"\x1b[3~"),
            KeyCode::PageUp => b.extend_from_slice(b"\x1b[5~"),
            KeyCode::PageDown => b.extend_from_slice(b"\x1b[6~"),
            KeyCode::F(1) => b.extend_from_slice(b"\x1bOP"),
            KeyCode::F(2) => b.extend_from_slice(b"\x1bOQ"),
            KeyCode::F(3) => b.extend_from_slice(b"\x1bOR"),
            KeyCode::F(4) => b.extend_from_slice(b"\x1bOS"),
            KeyCode::F(5) => b.extend_from_slice(b"\x1b[15~"),
            KeyCode::F(6) => b.extend_from_slice(b"\x1b[17~"),
            KeyCode::F(7) => b.extend_from_slice(b"\x1b[18~"),
            KeyCode::F(8) => b.extend_from_slice(b"\x1b[19~"),
            KeyCode::F(9) => b.extend_from_slice(b"\x1b[20~"),
            KeyCode::F(10) => b.extend_from_slice(b"\x1b[21~"),
            KeyCode::F(11) => b.extend_from_slice(b"\x1b[23~"),
            _ => {}
        }
        if !b.is_empty() {
            window.terminal.write(&b)?;
        }
    }
    Ok(())
}
