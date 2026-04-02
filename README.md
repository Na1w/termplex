# TermPlex

> **⚠️ TermPlex is an experiment in VIBECODING.**
> This project is based on an idea for a terminal multiplexer and was built by following the "vibe" rather than traditional development processes.

TermPlex is a TUI window manager and terminal multiplexer using a client-server architecture. It allows managing multiple terminal sessions within a single window.

## Features

- **Event-driven**: Uses screen diffing and flattened buffers to keep CPU usage low.
- **Client-Server**: Disconnect and reconnect to sessions. Multiple clients can share the same session.
- **Layouts**: Save and restore window arrangements and running processes.
- **Modes**: Separate Desktop and Terminal modes for workspace management.
- **Mouse Support**: SGR mouse reporting for applications, with window dragging and resizing.
- **Snapshots**: Capture individual panes or the full session as plain text via CLI or shortcuts.

## Keybindings

### Global
- **`F12`**: Toggle between **Terminal Mode** and **Desktop Mode**.

### Terminal Mode (Default)
In this mode, input is sent to the active terminal.
- **`Shift + PageUp/Down`**: Scroll through TermPlex history.
- **`Ctrl + Scroll Wheel`**: Send scroll events to the application.
- **`Ctrl + Right Click`**: Create a new terminal at the mouse cursor.
- **`Ctrl + Drag`**: Move or resize windows.

### Desktop Mode
In this mode, single-key commands manage the workspace.
- **`Tab` / `Shift + Tab`**: Cycle focus.
- **`Arrow Keys`**: Move window.
- **`W` / `S`**: Resize height.
- **`A` / `D`**: Resize width.
- **`z`**: Close window.
- **`x`**: Minimize (collapses to title bar).
- **`c`**: Maximize (expands to neighbors).
- **`f`**: Toggle Fullscreen.
- **`n`**: New window.
- **`s`**: Save layout to `layout.json`.
- **`l`**: Load layout from `layout.json`.
- **`v`**: Capture active pane.
- **`p`**: Capture full desktop.
- **`q`**: Quit.

## Mouse Controls

- **Titlebar Drag**: Move window.
- **Bottom-Right Handle (◢)**: Resize window.
- **[X] [_] [^] ...... [F]**: Window control buttons.
- **Scroll Wheel**: Scroll terminal buffers.

## Usage

### Installation
```bash
cargo build --release
```

### Running
```bash
./target/release/termplex [layout.json]
```

### CLI Capture
```bash
# Capture full desktop
termplex capture

# Capture a specific window
termplex capture --window-id 1
```

## CLI
Run with `--help` to see available options:
```bash
termplex --help
```
