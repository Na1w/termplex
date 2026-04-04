use serde::{Deserialize, Serialize};

/// Messages sent from Client to Server
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMessage {
    /// Initial connection with client info
    Connect {
        /// Terminal size at connection time
        term_size: (u16, u16),
    },

    /// Keyboard/mouse input for a specific window
    Input { window_id: usize, data: Vec<u8> },

    /// Request to create a new terminal window
    CreateWindow {
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        command: Option<String>,
        args: Vec<String>,
    },

    /// Request to close a window
    CloseWindow { window_id: usize },

    /// Focus a specific window
    FocusWindow { window_id: usize },

    /// Resize a window
    ResizeWindow {
        window_id: usize,
        width: u16,
        height: u16,
    },

    /// Move a window
    MoveWindow { window_id: usize, x: u16, y: u16 },

    /// Toggle minimize on a window
    MinimizeWindow { window_id: usize },

    /// Toggle maximize on a window
    MaximizeWindow { window_id: usize },

    /// Toggle true fullscreen on a window
    ToggleFullscreen { window_id: usize },

    /// Client terminal resized
    TerminalResize { width: u16, height: u16 },

    /// Scroll request
    Scroll { window_id: usize, amount: i32 },

    /// Scroll to absolute offset
    ScrollTo { window_id: usize, offset: usize },

    /// Request layout save
    SaveLayout { path: String },

    /// Request layout load
    LoadLayout { path: String },

    /// Rename a window
    RenameWindow { window_id: usize, title: String },

    /// Toggle solo mode for a window (minimize all others)
    ToggleSolo { window_id: usize },

    /// Temporarily expand a window in solo mode
    TemporaryExpand { window_id: usize },

    /// Temporarily collapse a window back in solo mode
    TemporaryCollapse { window_id: usize },

    /// Auto-tile all non-minimized windows
    TileWindows,

    /// Capture pane content
    CapturePane { window_id: usize },

    /// Capture full desktop content
    CaptureFull,

    /// Client disconnecting
    Disconnect,
}

/// Configuration for a single window in a layout
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowConfig {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub title: Option<String>,
}

/// A complete layout of windows
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Layout {
    pub windows: Vec<WindowConfig>,
}

/// A single cell in the terminal screen
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Cell {
    pub ch: char,
    pub fg: (u8, u8, u8),
    pub bg: (u8, u8, u8),
    pub attrs: u8, // bit 0: bold, 1: italic, 2: underline
}

impl Cell {
    pub fn bold(&self) -> bool {
        self.attrs & 1 != 0
    }
    pub fn italic(&self) -> bool {
        self.attrs & 2 != 0
    }
    pub fn underline(&self) -> bool {
        self.attrs & 4 != 0
    }

    pub fn new(
        ch: char,
        fg: (u8, u8, u8),
        bg: (u8, u8, u8),
        bold: bool,
        italic: bool,
        underline: bool,
    ) -> Self {
        let mut attrs = 0;
        if bold {
            attrs |= 1;
        }
        if italic {
            attrs |= 2;
        }
        if underline {
            attrs |= 4;
        }
        Self { ch, fg, bg, attrs }
    }
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: (200, 200, 200),
            bg: (0, 0, 0),
            attrs: 0,
        }
    }
}

/// Window state sent from server
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowState {
    pub id: usize,
    pub title: String,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub minimized: bool,
    pub focused: bool,
    pub running: bool,
    pub exit_code: Option<i32>,
    pub scroll_offset: usize,
    pub scrollback_size: usize,
    pub fullscreen: bool,
    /// Screen content - flat 1D grid of cells (row-major)
    pub screen: Vec<Cell>,
    /// Cursor position (row, col)
    pub cursor_pos: Option<(u16, u16)>,
    /// Cursor visible
    pub cursor_visible: bool,
    /// Mouse reporting enabled by the application
    pub mouse_reporting: bool,
    /// Z-order position (0 = back, higher = front)
    pub z_order: usize,
}

/// Messages sent from Server to Client
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMessage {
    /// Connection established, sent current state
    Welcome {
        /// Client session ID
        session_id: u64,
        /// Current windows
        windows: Vec<WindowState>,
        /// Whether solo mode is currently active
        solo_mode_active: bool,
        /// The window ID that triggered solo mode
        solo_origin_id: Option<usize>,
    },

    /// Full state sync (all windows)
    FullSync {
        windows: Vec<WindowState>,
        /// Whether solo mode is currently active
        solo_mode_active: bool,
        /// The window ID that triggered solo mode
        solo_origin_id: Option<usize>,
    },

    /// Single window updated
    WindowUpdate { window: WindowState },

    /// Window closed
    WindowClosed { window_id: usize },

    /// New window created
    WindowCreated { window: WindowState },

    /// Sparse screen update (only changed cells)
    ScreenDiff {
        window_id: usize,
        /// List of (index, cell) for changed cells
        cells: Vec<(usize, Cell)>,
        /// New cursor position
        cursor_pos: Option<(u16, u16)>,
        /// Current scrollback size
        scrollback_size: usize,
        /// Current scroll offset
        scroll_offset: usize,
    },

    /// Pane captured content
    PaneCaptured { window_id: usize, text: String },

    /// Full desktop captured content
    FullCaptured { text: String },

    /// Error message
    Error { message: String },

    /// Layout saved confirmation
    LayoutSaved,

    /// Server shutting down
    Shutdown,
}

/// Wraps a message with length prefix for TCP framing
pub fn encode_message<T: Serialize>(msg: &T) -> anyhow::Result<Vec<u8>> {
    let config = bincode::config::standard().with_fixed_int_encoding();
    let encoded = bincode::serde::encode_to_vec(msg, config)?;
    let len = encoded.len() as u32;
    let mut result = Vec::with_capacity(4 + encoded.len());
    result.extend_from_slice(&len.to_be_bytes());
    result.extend_from_slice(&encoded);
    Ok(result)
}

/// Parse length-prefixed message from buffer
#[allow(dead_code)]
pub fn decode_message<T: for<'de> Deserialize<'de>>(buf: &[u8]) -> anyhow::Result<(T, usize)> {
    if buf.len() < 4 {
        anyhow::bail!("Buffer too small for length prefix");
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if buf.len() < 4 + len {
        anyhow::bail!("Buffer too small for message");
    }
    let config = bincode::config::standard().with_fixed_int_encoding();
    let (msg, read_len) = bincode::serde::decode_from_slice::<T, _>(&buf[4..4 + len], config)?;
    Ok((msg, 4 + read_len))
}

/// Default TCP port for termplex
pub const DEFAULT_PORT: u16 = 9876;

/// Default terminal dimensions (internal area)
pub const DEFAULT_TERM_WIDTH: u16 = 80;
pub const DEFAULT_TERM_HEIGHT: u16 = 24;

/// Default bind address (localhost only)
pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1";
