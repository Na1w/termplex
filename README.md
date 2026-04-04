# TermPlex

> **⚠️ TermPlex is an experiment in VIBECODING.**
> This project is based on an idea for a terminal multiplexer and was built by following the "vibe" rather than traditional development processes.

TermPlex is a modern TUI window manager and terminal multiplexer using a client-server architecture. It provides a desktop-like experience within your terminal, allowing you to manage multiple sessions with overlapping windows, z-order management, and full mouse support.

## Features

- **Client-Server Architecture**: Run sessions in the background. Multiple clients can connect to the same session simultaneously.
- **Auto-Server**: The client automatically spawns a background server instance if one isn't already running on the specified port.
- **Event-Driven Rendering**: Uses a custom screen diffing algorithm and length-prefixed binary protocol (Bincode) to minimize bandwidth and CPU usage.
- **True Window Management**:
    - **Z-Order**: Windows overlap and can be brought to the front by clicking.
    - **Smart Maximize**: Windows expand in all directions until they hit another window or the screen edge.
    - **Solo Mode**: Focus on one window while others are temporarily minimized.
    - **Tiling**: Quickly organize all visible windows into a grid.
- **Terminal Emulation**:
    - Powered by `vt100` with a **3000-line scrollback buffer** per window.
    - Supports **SGR Mouse Reporting**, allowing apps like `htop` or `vim` inside TermPlex to receive mouse events.
    - Handles `ESC [ 3 J` to clear scrollback history.
- **Persistence**: Save and restore entire layouts, including window positions and currently running foreground processes (via Unix `ps` introspection).
- **Interactive UI**:
    - **Menu Bar**: File and Window operations with keyboard mnemonics.
    - **Deskbar**: A dynamic side panel showing all managed windows and their status.
    - **Internal Clipboard**: Mouse-based text selection and right-click paste.

## UI Elements & Controls

### Window Buttons
- **`[X]`**: Close window (sends `SIGHUP` to the process).
- **`[_]`**: Minimize window to the Deskbar.
- **`[^]`**: Maximize/Restore window size.
- **`[F]`**: Toggle true fullscreen mode (hides UI chrome).
- **`[S]`**: Toggle Solo mode.
- **`[D]`**: Reset window to default 80x24 dimensions.
- **`◢`**: Drag the bottom-right corner to resize.

### Mouse Interactions
- **Titlebar Drag**: Move window.
- **Ctrl + Drag**: Move or resize windows from any point.
- **Ctrl + Right Click**: Create a new terminal at the mouse cursor.
- **Double Click Title**: Rename the active window.
- **Scroll Bar**: Interactive vertical track for navigating history.
- **Selection**: Click and drag inside the terminal to copy text to the internal clipboard.

### Keyboard Shortcuts
- **`Shift + PageUp/PageDown`**: Scroll through terminal history.
- **Menu Navigation**: When a menu is open, use arrows or highlighted letters (e.g., `Q` for Quit).

## Usage

### Installation
Requires Rust (Edition 2024).
```bash
cargo build --release
```

### Running the Client
```bash
# Start/Connect (auto-spawns server if needed)
./target/release/termplex [layout.json]

# Connect to a specific host/port
./target/release/termplex --host 127.0.0.1 --port 9876
```

### CLI Remote Control
TermPlex can be controlled from the outside via subcommands:
```bash
# Launch a specific app in a new window
termplex launch -- htop

# Capture the current session as plain text
termplex capture

# Capture a specific window only
termplex capture --window-id 1
```

### Server Mode
Run a dedicated server without a local UI:
```bash
termplex --server --port 9876
```

## Technical Details
- **Binary Framing**: Messages are prefixed with a 4-byte big-endian length.
- **Process Cleanup**: Closing a window sends a `SIGHUP` to the shell's process group.
- **Login Shells**: Standard shells are started as login shells (e.g., `bash -l`).
- **Automatic Shutdown**: The server exits automatically when the last window is closed and no clients are connected.
