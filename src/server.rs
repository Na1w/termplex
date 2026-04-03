use anyhow::Result;
use ratatui::layout::Rect;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::protocol::*;
use crate::terminal::Terminal;

fn debug_log(msg: &str) {
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/termplex_server.log")
    {
        let _ = writeln!(file, "{}", msg);
    }
}

const MIN_WIDTH: u16 = 10;
const MIN_HEIGHT: u16 = 3;
const SCROLLBACK_SIZE: usize = 3000;

#[allow(dead_code)]
struct WindowInfo {
    id: usize,
    title: String,
    rect: Rect,
    minimized: bool,
    focused: bool,
    saved_rect: Option<Rect>,
    command: Option<String>,
    args: Vec<String>,
    scroll_offset: usize,
    terminal: Terminal,
    last_screen: Vec<Cell>,
    last_cursor_pos: Option<(u16, u16)>,
    last_scrollback_size: usize,
    fullscreen: bool,
}

impl WindowInfo {
    fn update_last_state(&mut self, ws: &WindowState) {
        self.last_screen = ws.screen.clone();
        self.last_cursor_pos = ws.cursor_pos;
        self.last_scrollback_size = ws.scrollback_size;
    }
}

struct ServerState {
    windows: HashMap<usize, WindowInfo>,
    window_order: Vec<usize>, // z-order: back to front
    next_window_id: usize,
    active_window_id: Option<usize>,
    solo_mode_origin: Option<usize>, // window_id that triggered solo mode
    solo_minimized_ids: Vec<usize>,  // IDs that were minimized by solo action
}

impl ServerState {
    fn new() -> Self {
        Self {
            windows: HashMap::new(),
            window_order: Vec::new(),
            next_window_id: 1,
            active_window_id: None,
            solo_mode_origin: None,
            solo_minimized_ids: Vec::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn create_window(
        &mut self,
        mut x: u16,
        mut y: u16,
        mut width: u16,
        mut height: u16,
        command: Option<String>,
        args: Vec<String>,
        _tx: std_mpsc::Sender<TermEvent>, // Not used but kept for API compatibility
        screen_size: Rect,
        title: Option<String>,
    ) -> Result<usize> {
        let id = self.next_window_id;
        self.next_window_id += 1;

        // Ensure window fits on screen but be less aggressive about x,y
        width = width.clamp(
            MIN_WIDTH,
            screen_size.width.max(MIN_WIDTH + 2).saturating_sub(2),
        );
        height = height.clamp(
            MIN_HEIGHT,
            screen_size.height.max(MIN_HEIGHT + 2).saturating_sub(2),
        );

        // Only clamp x,y if they are totally outside a reasonable boundary
        // but allow them to be somewhat off-screen to avoid perfect overlapping
        x = x.min(screen_size.width.saturating_sub(10));
        y = y.min(screen_size.height.saturating_sub(5));

        let title = title.unwrap_or_else(|| format!("Terminal {}", id));
        let rect = Rect::new(x, y, width, height);

        let rows = rect.height.saturating_sub(2);
        let cols = rect.width.saturating_sub(3);

        let terminal = Terminal::new(rows, cols, _tx, command.clone(), args.clone())?;

        let window = WindowInfo {
            id,
            title,
            rect,
            minimized: false,
            focused: false,
            saved_rect: None,
            command,
            args,
            scroll_offset: 0,
            terminal,
            last_screen: Vec::new(),
            last_cursor_pos: None,
            last_scrollback_size: 0,
            fullscreen: false,
        };

        self.windows.insert(id, window);
        self.window_order.push(id); // Add to z-order
        self.focus_window(id);
        Ok(id)
    }

    fn focus_window(&mut self, id: usize) {
        for (wid, win) in &mut self.windows {
            win.focused = *wid == id;
        }
        self.active_window_id = Some(id);
        // Move to front of z-order
        if let Some(pos) = self.window_order.iter().position(|&wid| wid == id) {
            let wid = self.window_order.remove(pos);
            self.window_order.push(wid);
        }
    }

    fn remove_window(&mut self, id: usize) {
        self.windows.remove(&id);
        if let Some(pos) = self.window_order.iter().position(|&wid| wid == id) {
            self.window_order.remove(pos);
        }
        if self.active_window_id == Some(id) {
            // Pick the next window from the top of the z-order
            self.active_window_id = self.window_order.last().copied();
            if let Some(new_id) = self.active_window_id {
                for (wid, win) in &mut self.windows {
                    win.focused = *wid == new_id;
                }
            }
        }
    }

    fn get_window_state(&self, id: usize) -> Option<WindowState> {
        let win = self.windows.get(&id)?;
        let running = win.terminal.running.load(Ordering::SeqCst);
        let exit_code = *win.terminal.exit_code.lock().unwrap();

        let inner_height = win.rect.height.saturating_sub(2) as usize;
        let inner_width = win.rect.width.saturating_sub(3) as usize;

        let total_lines = win.terminal.total_lines.load(Ordering::SeqCst);
        let scrollback_size = total_lines.saturating_sub(inner_height).min(SCROLLBACK_SIZE);

        let total_cells = inner_width * inner_height;

        // Get screen content
        let mut screen = Vec::with_capacity(total_cells);

        {
            let parser = win.terminal.parser.lock().unwrap();
            let vt_screen = parser.screen();

            for row in 0..inner_height {
                for col in 0..inner_width {
                    if let Some(cell) = vt_screen.cell(row as u16, col as u16) {
                        let contents = cell.contents();
                        let ch = contents.chars().next().unwrap_or(' ');

                        let fg = match cell.fgcolor() {
                            vt100::Color::Rgb(r, g, b) => (r, g, b),
                            vt100::Color::Idx(i) => ansi_to_rgb(i),
                            _ => (200, 200, 200),
                        };

                        let bg = match cell.bgcolor() {
                            vt100::Color::Rgb(r, g, b) => (r, g, b),
                            vt100::Color::Idx(i) => ansi_to_rgb(i),
                            _ => (0, 0, 0),
                        };

                        screen.push(Cell::new(
                            ch,
                            fg,
                            bg,
                            cell.bold(),
                            cell.italic(),
                            cell.underline(),
                        ));
                    } else {
                        screen.push(Cell::default());
                    }
                }
            }
        }

        let (cursor_pos, mouse_reporting) = {
            let parser = win.terminal.parser.lock().unwrap();
            let s = parser.screen();
            let (row, col) = s.cursor_position();
            let pos = Some((row, col));
            let mouse = running && s.mouse_protocol_mode() != vt100::MouseProtocolMode::None;
            (pos, mouse)
        };

        // Look up z_order from window_order
        let z_order = self
            .window_order
            .iter()
            .position(|&wid| wid == id)
            .unwrap_or(0);

        Some(WindowState {
            id: win.id,
            title: win.title.clone(),
            x: win.rect.x,
            y: win.rect.y,
            width: win.rect.width,
            height: win.rect.height,
            z_order,
            minimized: win.minimized,
            focused: win.focused,
            running,
            exit_code,
            scroll_offset: win.scroll_offset,
            scrollback_size,
            fullscreen: win.fullscreen,
            screen,
            cursor_pos,
            cursor_visible: !win.minimized && running,
            mouse_reporting,
        })
    }

    fn get_all_window_states(&self) -> Vec<WindowState> {
        // Return windows in z-order (back to front) with z_order field set
        self.window_order
            .iter()
            .filter_map(|&id| self.get_window_state(id))
            .collect()
    }

    fn save_layout(&self, path: &str) -> Result<()> {
        let mut layout = Layout::default();
        for id in &self.window_order {
            if let Some(win) = self.windows.get(id) {
                // Try to detect the currently running process
                let (command, args) =
                    if let Some((cmd, args)) = win.terminal.get_foreground_command() {
                        debug_log(&format!(
                            "Captured foreground command for window {}: {} {:?}",
                            id, cmd, args
                        ));
                        (Some(cmd), args)
                    } else {
                        debug_log(&format!(
                            "Using original command for window {}: {:?} {:?}",
                            id, win.command, win.args
                        ));
                        (win.command.clone(), win.args.clone())
                    };

                layout.windows.push(WindowConfig {
                    x: win.rect.x,
                    y: win.rect.y,
                    width: win.rect.width,
                    height: win.rect.height,
                    command,
                    args,
                    title: Some(win.title.clone()),
                });
            }
        }
        let json = serde_json::to_string_pretty(&layout)?;
        std::fs::write(path, json)?;
        Ok(())
    }
}

/// Build WindowState from WindowInfo (for use when already holding the lock)
fn build_window_state(win: &WindowInfo, window_order: &[usize]) -> WindowState {
    let running = win.terminal.running.load(Ordering::SeqCst);
    let exit_code = *win.terminal.exit_code.lock().unwrap();

    let inner_height = win.rect.height.saturating_sub(2) as usize;
    let inner_width = win.rect.width.saturating_sub(3) as usize;

    let total_lines = win.terminal.total_lines.load(Ordering::SeqCst);
    let scrollback_size = total_lines.saturating_sub(inner_height).min(SCROLLBACK_SIZE);

    let total_cells = inner_width * inner_height;

    // Get screen content
    let mut screen = Vec::with_capacity(total_cells);

    {
        let parser = win.terminal.parser.lock().unwrap();
        let vt_screen = parser.screen();

        for row in 0..inner_height {
            for col in 0..inner_width {
                if let Some(cell) = vt_screen.cell(row as u16, col as u16) {
                    let contents = cell.contents();
                    let ch = contents.chars().next().unwrap_or(' ');

                    let fg = match cell.fgcolor() {
                        vt100::Color::Rgb(r, g, b) => (r, g, b),
                        vt100::Color::Idx(i) => ansi_to_rgb(i),
                        _ => (200, 200, 200),
                    };

                    let bg = match cell.bgcolor() {
                        vt100::Color::Rgb(r, g, b) => (r, g, b),
                        vt100::Color::Idx(i) => ansi_to_rgb(i),
                        _ => (0, 0, 0),
                    };

                    screen.push(Cell::new(
                        ch,
                        fg,
                        bg,
                        cell.bold(),
                        cell.italic(),
                        cell.underline(),
                    ));
                } else {
                    screen.push(Cell::default());
                }
            }
        }
    }

    let (cursor_pos, mouse_reporting) = {
        let parser = win.terminal.parser.lock().unwrap();
        let s = parser.screen();
        let (row, col) = s.cursor_position();
        let pos = Some((row, col));
        let mouse = running && s.mouse_protocol_mode() != vt100::MouseProtocolMode::None;
        (pos, mouse)
    };

    WindowState {
        id: win.id,
        title: win.title.clone(),
        x: win.rect.x,
        y: win.rect.y,
        width: win.rect.width,
        height: win.rect.height,
        minimized: win.minimized,
        focused: win.focused,
        running,
        exit_code,
        scroll_offset: win.scroll_offset,
        scrollback_size,
        fullscreen: win.fullscreen,
        screen,
        cursor_pos,
        cursor_visible: !win.minimized && running,
        mouse_reporting,
        z_order: window_order
            .iter()
            .position(|&id| id == win.id)
            .unwrap_or(0),
    }
}

#[derive(Debug)]
enum ServerEvent {
    ClientConnected(u64, mpsc::Sender<Vec<u8>>),
    ClientDisconnected(u64),
    ClientMessage(u64, ClientMessage),
    WindowNeedsUpdate(usize),
    WindowClosed(usize),
}

fn ansi_to_rgb(idx: u8) -> (u8, u8, u8) {
    // Basic 16 colors
    let basic = [
        (0, 0, 0),
        (205, 0, 0),
        (0, 205, 0),
        (205, 205, 0),
        (0, 0, 238),
        (205, 0, 205),
        (0, 205, 205),
        (229, 229, 229),
        (127, 127, 127),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (0, 0, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    if idx < 16 {
        return basic[idx as usize];
    }
    // 216 color cube
    if idx < 232 {
        let i = idx - 16;
        let r = (i / 36) * 51;
        let g = ((i % 36) / 6) * 51;
        let b = (i % 6) * 51;
        return (r, g, b);
    }
    // Grayscale
    let level = idx - 232;
    let v = level * 10 + 8;
    (v, v, v)
}

use crate::terminal::TermEvent;

pub async fn run_server(host: &str, port: u16, layout_path: Option<String>) -> Result<()> {
    let addr = format!("{}:{}", host, port);
    println!("TermPlex server starting on {}", addr);

    let listener = TcpListener::bind(&addr).await?;
    println!("Server listening on {}", addr);

    let (event_tx, mut event_rx) = mpsc::channel::<ServerEvent>(100);

    // Server state
    let state = Arc::new(Mutex::new(ServerState::new()));

    // Accept connections
    let accept_tx = event_tx.clone();
    tokio::spawn(async move {
        let mut client_id: u64 = 1;
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    // Create a channel for this client's writes
                    let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(100);

                    // Spawn write task for this client
                    let (mut read_half, mut write_half) = stream.into_split();
                    tokio::spawn(async move {
                        while let Some(data) = write_rx.recv().await {
                            if write_half.write_all(&data).await.is_err() {
                                break;
                            }
                        }
                    });

                    // Send connection event
                    let _ = accept_tx
                        .send(ServerEvent::ClientConnected(client_id, write_tx))
                        .await;

                    // Spawn read task
                    let read_tx = accept_tx.clone();
                    let cid = client_id;
                    tokio::spawn(async move {
                        let mut buf = [0u8; 4096];
                        let mut accum = Vec::new();
                        loop {
                            match read_half.read(&mut buf).await {
                                Ok(0) => break,
                                Ok(n) => {
                                    accum.extend_from_slice(&buf[..n]);
                                    // Try to decode messages
                                    while accum.len() >= 4 {
                                        let len = u32::from_be_bytes([
                                            accum[0], accum[1], accum[2], accum[3],
                                        ])
                                            as usize;
                                        if accum.len() < 4 + len {
                                            break;
                                        }
                                        let config = bincode::config::legacy();
                                        if let Ok((msg, _)) =
                                            bincode::serde::decode_from_slice::<ClientMessage, _>(
                                                &accum[4..4 + len],
                                                config,
                                            )
                                        {
                                            let _ = read_tx
                                                .send(ServerEvent::ClientMessage(cid, msg))
                                                .await;
                                        }
                                        accum.drain(0..4 + len);
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                        let _ = read_tx.send(ServerEvent::ClientDisconnected(cid)).await;
                    });

                    client_id += 1;
                }
                Err(e) => {
                    debug_log(&format!("Accept error: {}", e));
                }
            }
        }
    });

    let mut client_writers: HashMap<u64, mpsc::Sender<Vec<u8>>> = HashMap::new();
    let mut client_sizes: HashMap<u64, (u16, u16)> = HashMap::new();
    let mut effective_screen_size: Rect = Rect::new(0, 0, 200, 60); // Default large size

    // Helper to recalculate effective screen size
    let recalculate_effective_size = |sizes: &HashMap<u64, (u16, u16)>| -> Rect {
        if sizes.is_empty() {
            return Rect::new(0, 0, 200, 60);
        }
        let max_w = sizes.values().map(|s| s.0).max().unwrap_or(200);
        let max_h = sizes.values().map(|s| s.1).max().unwrap_or(60);
        Rect::new(0, 0, max_w, max_h)
    };

    // Try to load initial layout if path provided
    let mut layout_loaded = false;
    if let Some(path) = layout_path
        && let Ok(json) = std::fs::read_to_string(&path)
        && let Ok(layout) = serde_json::from_str::<Layout>(&json)
    {
        for config in layout.windows {
            if let Err(e) = spawn_window(
                state.clone(),
                event_tx.clone(),
                config,
                effective_screen_size,
            ) {
                eprintln!("Failed to spawn window from layout: {}", e);
            }
        }
        layout_loaded = true;
    }

    if !layout_loaded {
        // Spawn default terminal if no layout loaded
        let _ = spawn_window(
            state.clone(),
            event_tx.clone(),
            WindowConfig {
                x: 2,
                y: 4,
                width: DEFAULT_TERM_WIDTH + 2,
                height: DEFAULT_TERM_HEIGHT + 2,
                command: None,
                args: vec![],
                title: None,
            },
            effective_screen_size,
        );
    }

    while let Some(event) = event_rx.recv().await {
        match event {
            ServerEvent::ClientConnected(id, writer) => {
                client_writers.insert(id, writer);

                // Initial default size for client until they send Connect message
                client_sizes.insert(id, (80, 24));
                effective_screen_size = recalculate_effective_size(&client_sizes);

                // Send welcome with current state
                let windows = {
                    let st = state.lock().unwrap();
                    st.get_all_window_states()
                };

                let welcome = ServerMessage::Welcome {
                    session_id: id,
                    windows,
                };

                if let Some(tx) = client_writers.get(&id)
                    && let Ok(data) = encode_message(&welcome)
                {
                    let _ = tx.try_send(data);
                }
            }

            ServerEvent::ClientDisconnected(id) => {
                println!("Client {} disconnected", id);
                client_writers.remove(&id);
                client_sizes.remove(&id);
                effective_screen_size = recalculate_effective_size(&client_sizes);

                // Exit if no more windows and no clients
                let st = state.lock().unwrap();
                if st.windows.is_empty() && client_writers.is_empty() {
                    println!("No more windows and no clients, shutting down");
                    break;
                }
            }

            ServerEvent::ClientMessage(client_id, msg) => {
                match msg {
                    ClientMessage::Disconnect => {
                        let _ = event_tx
                            .send(ServerEvent::ClientDisconnected(client_id))
                            .await;
                    }

                    ClientMessage::Connect { term_size } => {
                        // Update screen size based on client's terminal size
                        client_sizes.insert(client_id, term_size);
                        effective_screen_size = recalculate_effective_size(&client_sizes);
                    }

                    ClientMessage::TerminalResize { width, height } => {
                        client_sizes.insert(client_id, (width, height));
                        effective_screen_size = recalculate_effective_size(&client_sizes);
                    }

                    ClientMessage::CreateWindow {
                        x,
                        y,
                        width,
                        height,
                        command,
                        args,
                    } => {
                        // Prioritize requested size. create_window will still ensure
                        // it's not larger than the total screen area.
                        let config = WindowConfig {
                            x,
                            y,
                            width,
                            height,
                            command,
                            args,
                            title: None,
                        };

                        if let Ok(_id) = spawn_window(
                            state.clone(),
                            event_tx.clone(),
                            config,
                            effective_screen_size,
                        ) {
                            let windows = {
                                let st = state.lock().unwrap();
                                st.get_all_window_states()
                            };
                            let msg = ServerMessage::FullSync { windows };
                            broadcast_to_all(&client_writers, &msg);
                        }
                    }

                    ClientMessage::SaveLayout { path } => {
                        let st = state.lock().unwrap();
                        if let Err(e) = st.save_layout(&path) {
                            let msg = ServerMessage::Error {
                                message: format!("Failed to save layout: {}", e),
                            };
                            broadcast_to_all(&client_writers, &msg);
                        } else {
                            broadcast_to_all(&client_writers, &ServerMessage::LayoutSaved);
                        }
                    }

                    ClientMessage::LoadLayout { path } => {
                        if let Ok(json) = std::fs::read_to_string(&path)
                            && let Ok(layout) = serde_json::from_str::<Layout>(&json)
                        {
                            // 1. Close all current windows
                            {
                                let mut st = state.lock().unwrap();
                                let ids: Vec<_> = st.windows.keys().copied().collect();
                                for id in ids {
                                    st.remove_window(id);
                                }
                            }

                            // 2. Load new windows
                            let mut errors = Vec::new();
                            for config in layout.windows {
                                if let Err(e) = spawn_window(
                                    state.clone(),
                                    event_tx.clone(),
                                    config,
                                    effective_screen_size,
                                ) {
                                    errors.push(format!("Failed to spawn window: {}", e));
                                }
                            }

                            if !errors.is_empty() {
                                let msg = ServerMessage::Error {
                                    message: errors.join("\n"),
                                };
                                broadcast_to_all(&client_writers, &msg);
                            }

                            // 3. Get final state and sync
                            let windows = {
                                let st = state.lock().unwrap();
                                st.get_all_window_states()
                            };

                            // Send a single full sync to all clients
                            broadcast_to_all(&client_writers, &ServerMessage::FullSync { windows });
                        }
                    }

                    ClientMessage::CapturePane { window_id } => {
                        let text = {
                            let st = state.lock().unwrap();
                            if let Some(win) = st.windows.get(&window_id) {
                                let parser = win.terminal.parser.lock().unwrap();
                                parser.screen().contents()
                            } else {
                                String::new()
                            }
                        };

                        if !text.is_empty() {
                            let msg = ServerMessage::PaneCaptured { window_id, text };
                            broadcast_to_all(&client_writers, &msg);
                        }
                    }

                    ClientMessage::CaptureFull => {
                        let text = {
                            let st = state.lock().unwrap();
                            let width = effective_screen_size.width as usize;
                            let height = effective_screen_size.height as usize;
                            let mut grid = vec![vec![' '; width]; height];

                            // Draw background
                            for (row, line) in grid.iter_mut().enumerate() {
                                for (col, cell) in line.iter_mut().enumerate() {
                                    if row == 0 || row == height - 1 || col == 0 || col == width - 1
                                    {
                                        *cell = '.';
                                    }
                                }
                            }

                            // Render windows in z-order
                            for &id in &st.window_order {
                                if let Some(win) = st.windows.get(&id) {
                                    let r = win.rect;
                                    let win_w = r.width as usize;
                                    let win_h = if win.minimized { 1 } else { r.height as usize };

                                    // Draw borders
                                    for row in 0..win_h {
                                        for col in 0..win_w {
                                            let gx = r.x as usize + col;
                                            let gy = r.y as usize + row;
                                            if gx < width && gy < height {
                                                if row == 0 || row == win_h - 1 {
                                                    grid[gy][gx] = '-';
                                                } else if col == 0 || col == win_w - 1 {
                                                    grid[gy][gx] = '|';
                                                }
                                            }
                                        }
                                    }

                                    // Draw title
                                    let title = format!(" {} ", win.title);
                                    for (i, c) in title.chars().enumerate() {
                                        let gx = r.x as usize + 2 + i;
                                        let gy = r.y as usize;
                                        if gx < width
                                            && gy < height
                                            && gx < r.x as usize + win_w - 1
                                        {
                                            grid[gy][gx] = c;
                                        }
                                    }

                                    // Draw content
                                    if !win.minimized {
                                        let parser = win.terminal.parser.lock().unwrap();
                                        let vt_screen = parser.screen();
                                        let inner_w = win_w.saturating_sub(2);
                                        let inner_h = win_h.saturating_sub(2);

                                        for row in 0..inner_h {
                                            for col in 0..inner_w {
                                                let gx = r.x as usize + 1 + col;
                                                let gy = r.y as usize + 1 + row;
                                                if gx < width
                                                    && gy < height
                                                    && let Some(cell) =
                                                        vt_screen.cell(row as u16, col as u16)
                                                {
                                                    grid[gy][gx] = cell
                                                        .contents()
                                                        .chars()
                                                        .next()
                                                        .unwrap_or(' ');
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            // Convert grid to string
                            grid.iter()
                                .map(|row| row.iter().collect::<String>())
                                .collect::<Vec<_>>()
                                .join("\n")
                        };

                        let msg = ServerMessage::FullCaptured { text };
                        broadcast_to_all(&client_writers, &msg);
                    }

                    ClientMessage::CloseWindow { window_id } => {
                        let windows = {
                            let mut st = state.lock().unwrap();
                            st.remove_window(window_id);
                            st.get_all_window_states()
                        };
                        broadcast_to_all(
                            &client_writers,
                            &ServerMessage::WindowClosed { window_id },
                        );
                        broadcast_to_all(&client_writers, &ServerMessage::FullSync { windows });
                    }

                    ClientMessage::FocusWindow { window_id } => {
                        let windows = {
                            let mut st = state.lock().unwrap();
                            st.focus_window(window_id);
                            st.get_all_window_states()
                        };
                        let msg = ServerMessage::FullSync { windows };
                        broadcast_to_all(&client_writers, &msg);
                    }

                    ClientMessage::Input { window_id, data } => {
                        let msg = {
                            let mut st = state.lock().unwrap();
                            let window_order = st.window_order.clone();
                            if let Some(win) = st.windows.get_mut(&window_id) {
                                let _ = win.terminal.write(&data);
                                let new_ws = build_window_state(win, &window_order);

                                if win.last_screen.len() != new_ws.screen.len()
                                    || win.last_scrollback_size != new_ws.scrollback_size
                                {
                                    win.update_last_state(&new_ws);
                                    Some(ServerMessage::WindowUpdate { window: new_ws })
                                } else {
                                    let mut diff_cells = Vec::new();
                                    for (idx, (old, new)) in
                                        win.last_screen.iter().zip(new_ws.screen.iter()).enumerate()
                                    {
                                        if old != new {
                                            diff_cells.push((idx, *new));
                                        }
                                    }
                                    let cursor_changed = win.last_cursor_pos != new_ws.cursor_pos;
                                    win.last_screen = new_ws.screen;
                                    win.last_cursor_pos = new_ws.cursor_pos;

                                    if !diff_cells.is_empty() || cursor_changed {
                                        Some(ServerMessage::ScreenDiff {
                                            window_id,
                                            cells: diff_cells,
                                            cursor_pos: new_ws.cursor_pos,
                                            scrollback_size: new_ws.scrollback_size,
                                            scroll_offset: new_ws.scroll_offset,
                                        })
                                    } else {
                                        None
                                    }
                                }
                            } else {
                                None
                            }
                        };
                        if let Some(msg) = msg {
                            broadcast_to_all(&client_writers, &msg);
                        }
                    }

                    ClientMessage::RenameWindow { window_id, title } => {
                        let ws = {
                            let mut st = state.lock().unwrap();
                            let window_order = st.window_order.clone();
                            if let Some(win) = st.windows.get_mut(&window_id) {
                                win.title = title;
                                let new_ws = build_window_state(win, &window_order);
                                win.update_last_state(&new_ws);
                                Some(new_ws)
                            } else {
                                None
                            }
                        };
                        if let Some(ws) = ws {
                            let msg = ServerMessage::WindowUpdate { window: ws };
                            broadcast_to_all(&client_writers, &msg);
                        }
                    }

                    ClientMessage::ToggleSolo { window_id } => {
                        let mut st = state.lock().unwrap();
                        let current_solo = st.solo_mode_origin;

                        if current_solo == Some(window_id) {
                            // Turn OFF solo mode
                            let ids_to_restore: Vec<usize> =
                                st.solo_minimized_ids.drain(..).collect();
                            for id in ids_to_restore {
                                if let Some(win) = st.windows.get_mut(&id) {
                                    win.minimized = false;
                                }
                            }
                            st.solo_mode_origin = None;
                        } else {
                            // Turn ON solo mode (possibly switching from another window)
                            // 1. Restore previous if necessary
                            let ids_to_restore: Vec<usize> =
                                st.solo_minimized_ids.drain(..).collect();
                            for id in ids_to_restore {
                                if let Some(win) = st.windows.get_mut(&id) {
                                    win.minimized = false;
                                }
                            }

                            // 2. Solo the new window
                            let ids: Vec<usize> = st.windows.keys().copied().collect();
                            for id in ids {
                                if id == window_id {
                                    if let Some(win) = st.windows.get_mut(&id) {
                                        win.minimized = false;
                                    }
                                } else if let Some(win) = st.windows.get_mut(&id)
                                    && !win.minimized
                                {
                                    win.minimized = true;
                                    st.solo_minimized_ids.push(id);
                                }
                            }
                            st.solo_mode_origin = Some(window_id);
                            st.focus_window(window_id);
                        }

                        // Broadcast full sync to update all clients
                        let windows = st.get_all_window_states();
                        broadcast_to_all(&client_writers, &ServerMessage::FullSync { windows });
                    }

                    ClientMessage::TileWindows => {
                        let mut st = state.lock().unwrap();
                        let window_order = st.window_order.clone();

                        // Get all visible windows
                        let visible_ids: Vec<usize> = st
                            .window_order
                            .iter()
                            .filter(|&&id| !st.windows.get(&id).unwrap().minimized)
                            .copied()
                            .collect();

                        if visible_ids.is_empty() {
                            continue;
                        }

                        let count = visible_ids.len();
                        let cols = (count as f64).sqrt().ceil() as u16;
                        let rows = ((count as f64) / cols as f64).ceil() as u16;

                        let screen_w = effective_screen_size.width;
                        let screen_h = effective_screen_size.height.saturating_sub(1); // Space for menu

                        let win_w = screen_w / cols;
                        let win_h = screen_h / rows;

                        for (i, id) in visible_ids.iter().enumerate() {
                            let r = (i as u16) / cols;
                            let c = (i as u16) % cols;

                            let x = c * win_w;
                            let y = 1 + (r * win_h);

                            let actual_w = if c == cols - 1 { screen_w - x } else { win_w };
                            let actual_h = if r == rows - 1 {
                                screen_h - (y - 1)
                            } else {
                                win_h
                            };

                            if let Some(win) = st.windows.get_mut(id) {
                                win.rect = Rect::new(x, y, actual_w, actual_h);
                                let _ = win
                                    .terminal
                                    .resize(actual_h.saturating_sub(2), actual_w.saturating_sub(2));
                                let ws = build_window_state(win, &window_order);
                                win.update_last_state(&ws);
                            }
                        }

                        let windows = st.get_all_window_states();
                        broadcast_to_all(&client_writers, &ServerMessage::FullSync { windows });
                    }

                    ClientMessage::ResizeWindow {
                        window_id,
                        width,
                        height,
                    } => {
                        let ws = {
                            let mut st = state.lock().unwrap();
                            let window_order = st.window_order.clone();
                            if let Some(win) = st.windows.get_mut(&window_id) {
                                let new_rect = Rect::new(win.rect.x, win.rect.y, width, height);
                                let _ = win
                                    .terminal
                                    .resize(height.saturating_sub(2), width.saturating_sub(2));
                                win.rect = new_rect;
                                let new_ws = build_window_state(win, &window_order);
                                win.update_last_state(&new_ws);
                                Some(new_ws)
                            } else {
                                None
                            }
                        };
                        if let Some(ws) = ws {
                            let msg = ServerMessage::WindowUpdate { window: ws };
                            broadcast_to_all(&client_writers, &msg);
                        }
                    }

                    ClientMessage::MoveWindow { window_id, x, y } => {
                        let ws = {
                            let mut st = state.lock().unwrap();
                            let window_order = st.window_order.clone();
                            if let Some(win) = st.windows.get_mut(&window_id) {
                                win.rect.x = x;
                                win.rect.y = y;
                                let new_ws = build_window_state(win, &window_order);
                                win.update_last_state(&new_ws);
                                Some(new_ws)
                            } else {
                                None
                            }
                        };
                        if let Some(ws) = ws {
                            let msg = ServerMessage::WindowUpdate { window: ws };
                            broadcast_to_all(&client_writers, &msg);
                        }
                    }

                    ClientMessage::MinimizeWindow { window_id } => {
                        let ws = {
                            let mut st = state.lock().unwrap();
                            let window_order = st.window_order.clone();
                            if let Some(win) = st.windows.get_mut(&window_id) {
                                win.minimized = !win.minimized;
                                let new_ws = build_window_state(win, &window_order);
                                win.update_last_state(&new_ws);
                                Some(new_ws)
                            } else {
                                None
                            }
                        };
                        if let Some(ws) = ws {
                            let msg = ServerMessage::WindowUpdate { window: ws };
                            broadcast_to_all(&client_writers, &msg);
                        }
                    }

                    ClientMessage::ToggleFullscreen { window_id } => {
                        let ws = {
                            let mut st = state.lock().unwrap();
                            let window_order = st.window_order.clone();
                            if let Some(win) = st.windows.get_mut(&window_id) {
                                if win.fullscreen {
                                    // Restore
                                    if let Some(saved) = win.saved_rect.take() {
                                        let _ = win.terminal.resize(
                                            saved.height.saturating_sub(2),
                                            saved.width.saturating_sub(2),
                                        );
                                        win.rect = saved;
                                    }
                                    win.fullscreen = false;
                                } else {
                                    // Go Fullscreen
                                    win.saved_rect = Some(win.rect);
                                    let screen = effective_screen_size;
                                    win.rect = Rect::new(0, 0, screen.width, screen.height);
                                    win.fullscreen = true;
                                    win.minimized = false;
                                    let _ = win.terminal.resize(
                                        win.rect.height.saturating_sub(2),
                                        win.rect.width.saturating_sub(3),
                                    );
                                }
                                let new_ws = build_window_state(win, &window_order);
                                win.update_last_state(&new_ws);
                                Some(new_ws)
                            } else {
                                None
                            }
                        };
                        if let Some(ws) = ws {
                            let msg = ServerMessage::WindowUpdate { window: ws };
                            broadcast_to_all(&client_writers, &msg);
                        }
                    }

                    ClientMessage::MaximizeWindow { window_id } => {
                        let ws = {
                            let mut st = state.lock().unwrap();
                            let window_order = st.window_order.clone();
                            // Get all window rects first to avoid borrow issues
                            let other_windows: Vec<(usize, Rect, bool)> = st
                                .windows
                                .iter()
                                .filter(|(id, _)| **id != window_id)
                                .map(|(&id, w)| (id, w.rect, w.minimized))
                                .collect();

                            if let Some(win) = st.windows.get_mut(&window_id) {
                                if let Some(saved) = win.saved_rect.take() {
                                    // Restore to saved size
                                    let _ = win.terminal.resize(
                                        saved.height.saturating_sub(2),
                                        saved.width.saturating_sub(2),
                                    );
                                    win.rect = saved;
                                } else {
                                    // Maximize: expand in all directions until hitting other windows
                                    win.saved_rect = Some(win.rect);
                                    let screen = effective_screen_size;

                                    // Helper to check if rect intersects any other window
                                    let intersects =
                                        |r: Rect, windows: &[(usize, Rect, bool)]| -> bool {
                                            for (_, rect, minimized) in windows.iter() {
                                                if !minimized && r.intersects(*rect) {
                                                    return true;
                                                }
                                            }
                                            false
                                        };

                                    // Expand up
                                    while win.rect.y > 1 {
                                        let mut t = win.rect;
                                        t.y -= 1;
                                        t.height += 1;
                                        if intersects(t, &other_windows) {
                                            break;
                                        }
                                        win.rect = t;
                                    }
                                    // Expand down
                                    while win.rect.y + win.rect.height
                                        < screen.height.saturating_sub(1)
                                    {
                                        let mut t = win.rect;
                                        t.height += 1;
                                        if intersects(t, &other_windows) {
                                            break;
                                        }
                                        win.rect = t;
                                    }
                                    // Expand left
                                    while win.rect.x > 1 {
                                        let mut t = win.rect;
                                        t.x -= 1;
                                        t.width += 1;
                                        if intersects(t, &other_windows) {
                                            break;
                                        }
                                        win.rect = t;
                                    }
                                    // Expand right
                                    while win.rect.x + win.rect.width
                                        < screen.width.saturating_sub(1)
                                    {
                                        let mut t = win.rect;
                                        t.width += 1;
                                        if intersects(t, &other_windows) {
                                            break;
                                        }
                                        win.rect = t;
                                    }

                                    // Resize terminal
                                    let _ = win.terminal.resize(
                                        win.rect.height.saturating_sub(2),
                                        win.rect.width.saturating_sub(2),
                                    );
                                }
                                let new_ws = build_window_state(win, &window_order);
                                win.update_last_state(&new_ws);
                                Some(new_ws)
                            } else {
                                None
                            }
                        };
                        if let Some(ws) = ws {
                            let msg = ServerMessage::WindowUpdate { window: ws };
                            broadcast_to_all(&client_writers, &msg);
                        }
                    }

                    ClientMessage::Scroll { window_id, amount } => {
                        let msg = {
                            let mut st = state.lock().unwrap();
                            let window_order = st.window_order.clone();
                            if let Some(win) = st.windows.get_mut(&window_id) {
                                let inner_height = win.rect.height.saturating_sub(2) as usize;
                                let total_lines = win.terminal.total_lines.load(Ordering::SeqCst);
                                let max_scroll = total_lines
                                    .saturating_sub(inner_height)
                                    .min(SCROLLBACK_SIZE);

                                win.scroll_offset = (win.scroll_offset as i32 + amount)
                                    .clamp(0, max_scroll as i32)
                                    as usize;

                                {
                                    let mut parser = win.terminal.parser.lock().unwrap();
                                    parser.screen_mut().set_scrollback(win.scroll_offset);
                                }

                                let new_ws = build_window_state(win, &window_order);

                                if win.last_screen.len() != new_ws.screen.len()
                                    || win.last_scrollback_size != new_ws.scrollback_size
                                {
                                    win.update_last_state(&new_ws);
                                    Some(ServerMessage::WindowUpdate { window: new_ws })
                                } else {
                                    let mut diff_cells = Vec::new();
                                    for (idx, (old, new)) in
                                        win.last_screen.iter().zip(new_ws.screen.iter()).enumerate()
                                    {
                                        if old != new {
                                            diff_cells.push((idx, *new));
                                        }
                                    }
                                    let cursor_changed = win.last_cursor_pos != new_ws.cursor_pos;
                                    win.last_screen = new_ws.screen;
                                    win.last_cursor_pos = new_ws.cursor_pos;

                                    if !diff_cells.is_empty() || cursor_changed {
                                        Some(ServerMessage::ScreenDiff {
                                            window_id,
                                            cells: diff_cells,
                                            cursor_pos: new_ws.cursor_pos,
                                            scrollback_size: new_ws.scrollback_size,
                                            scroll_offset: new_ws.scroll_offset,
                                        })
                                    } else {
                                        None
                                    }
                                }
                            } else {
                                None
                            }
                        };
                        if let Some(msg) = msg {
                            broadcast_to_all(&client_writers, &msg);
                        }
                    }

                    ClientMessage::ScrollTo { window_id, offset } => {
                        let msg = {
                            let mut st = state.lock().unwrap();
                            let window_order = st.window_order.clone();
                            if let Some(win) = st.windows.get_mut(&window_id) {
                                let inner_height = win.rect.height.saturating_sub(2) as usize;
                                let total_lines = win.terminal.total_lines.load(Ordering::SeqCst);
                                let max_scroll = total_lines
                                    .saturating_sub(inner_height)
                                    .min(SCROLLBACK_SIZE);

                                win.scroll_offset = offset.min(max_scroll);

                                {
                                    let mut parser = win.terminal.parser.lock().unwrap();
                                    parser.screen_mut().set_scrollback(win.scroll_offset);
                                }

                                let new_ws = build_window_state(win, &window_order);

                                if win.last_screen.len() != new_ws.screen.len()
                                    || win.last_scrollback_size != new_ws.scrollback_size
                                {
                                    win.update_last_state(&new_ws);
                                    Some(ServerMessage::WindowUpdate { window: new_ws })
                                } else {
                                    let mut diff_cells = Vec::new();
                                    for (idx, (old, new)) in
                                        win.last_screen.iter().zip(new_ws.screen.iter()).enumerate()
                                    {
                                        if old != new {
                                            diff_cells.push((idx, *new));
                                        }
                                    }
                                    let cursor_changed = win.last_cursor_pos != new_ws.cursor_pos;
                                    win.last_screen = new_ws.screen;
                                    win.last_cursor_pos = new_ws.cursor_pos;

                                    if !diff_cells.is_empty() || cursor_changed {
                                        Some(ServerMessage::ScreenDiff {
                                            window_id,
                                            cells: diff_cells,
                                            cursor_pos: new_ws.cursor_pos,
                                            scrollback_size: new_ws.scrollback_size,
                                            scroll_offset: new_ws.scroll_offset,
                                        })
                                    } else {
                                        None
                                    }
                                }
                            } else {
                                None
                            }
                        };
                        if let Some(msg) = msg {
                            broadcast_to_all(&client_writers, &msg);
                        }
                    }
                }
            }

            ServerEvent::WindowNeedsUpdate(window_id) => {
                let diff_msg = {
                    let mut st = state.lock().unwrap();
                    let window_order = st.window_order.clone();
                    if let Some(win) = st.windows.get_mut(&window_id) {
                        let new_ws = build_window_state(win, &window_order);

                        // If screens have different sizes, or scrollback changed, we must send a full update
                        if win.last_screen.len() != new_ws.screen.len()
                            || win.last_scrollback_size != new_ws.scrollback_size
                        {
                            win.update_last_state(&new_ws);
                            Some(ServerMessage::WindowUpdate { window: new_ws })
                        } else {
                            // Find differences
                            let mut diff_cells = Vec::new();
                            for (idx, (old, new)) in
                                win.last_screen.iter().zip(new_ws.screen.iter()).enumerate()
                            {
                                if old != new {
                                    diff_cells.push((idx, *new));
                                }
                            }

                            // Update last_screen
                            let cursor_changed = win.last_cursor_pos != new_ws.cursor_pos;
                            win.last_screen = new_ws.screen;
                            win.last_cursor_pos = new_ws.cursor_pos;

                            if !diff_cells.is_empty() || cursor_changed {
                                Some(ServerMessage::ScreenDiff {
                                    window_id,
                                    cells: diff_cells,
                                    cursor_pos: new_ws.cursor_pos,
                                    scrollback_size: new_ws.scrollback_size,
                                    scroll_offset: new_ws.scroll_offset,
                                })
                            } else {                                None
                            }
                        }
                    } else {
                        None
                    }
                };

                if let Some(msg) = diff_msg {
                    broadcast_to_all(&client_writers, &msg);
                }
            }

            ServerEvent::WindowClosed(id) => {
                let windows = {
                    let mut st = state.lock().unwrap();
                    st.remove_window(id);
                    st.get_all_window_states()
                };

                // Broadcast closed event
                broadcast_to_all(
                    &client_writers,
                    &ServerMessage::WindowClosed { window_id: id },
                );

                // Sync all windows to update focus and z-order
                broadcast_to_all(&client_writers, &ServerMessage::FullSync { windows });

                // Exit if no more windows and no clients
                let st = state.lock().unwrap();
                if st.windows.is_empty() && client_writers.is_empty() {
                    println!("No more windows and no clients, shutting down");
                    break;
                }
            }
        }
    }

    // Cleanup
    for tx in client_writers.values() {
        let shutdown = encode_message(&ServerMessage::Shutdown).unwrap_or_default();
        let _ = tx.try_send(shutdown);
    }

    println!("Server shutting down");
    Ok(())
}

fn broadcast_to_all(writers: &HashMap<u64, mpsc::Sender<Vec<u8>>>, msg: &ServerMessage) {
    if let Ok(data) = encode_message(msg) {
        for (_, tx) in writers.iter() {
            let _ = tx.try_send(data.clone());
        }
    }
}

fn spawn_window(
    state: Arc<Mutex<ServerState>>,
    event_tx: mpsc::Sender<ServerEvent>,
    config: WindowConfig,
    screen_size: Rect,
) -> Result<usize> {
    let (tx, rx) = std_mpsc::channel::<TermEvent>();
    let id = {
        let mut st = state.lock().unwrap();
        st.create_window(
            config.x,
            config.y,
            config.width,
            config.height,
            config.command,
            config.args,
            tx,
            screen_size,
            config.title,
        )?
    };

    // Spawn handler for this window's terminal events
    let screen_tx = event_tx.clone();
    tokio::spawn(async move {
        while let Ok(event) = rx.recv() {
            match event {
                TermEvent::Update => {
                    let _ = screen_tx.try_send(ServerEvent::WindowNeedsUpdate(id));
                }
                TermEvent::Closed => {
                    let _ = screen_tx.try_send(ServerEvent::WindowClosed(id));
                    break;
                }
            }
        }
    });
    Ok(id)
}
