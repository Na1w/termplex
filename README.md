# TermPlex

TermPlex is a lightweight TUI window manager that allows you to run and manage multiple terminal sessions within a single window. It features draggable, resizable windows, persistent layouts, and two distinct modes for seamless terminal use and workspace management.

## Keybindings

### Global
- **`F12`**: Toggle between **Terminal Mode** and **Desktop Mode**.

### Terminal Mode (Default)
In this mode, input is sent directly to the active terminal.
- **`Shift + PageUp/Down`**: Scroll through terminal history.
- **`Ctrl + Right Click`**: Spawn a new terminal at the mouse cursor.

### Desktop Mode
In this mode, keys are used for window management.
- **`Tab` / `Shift + Tab`**: Cycle through window focus.
- **`Arrow Keys`**: Move the active window.
- **`W` / `S`**: Resize height.
- **`A` / `D`**: Resize width.
- **`Z`**: Close window.
- **`X`**: Maximize/Restore.
- **`C`**: Minimize/Unminimize.
- **`P`**: Save current layout to `layout.json`.
- **`Ctrl + N`**: Spawn a new terminal.
- **`Ctrl + Q`**: Quit TermPlex.

## Mouse Controls

- **Titlebar Drag**: Move the window.
- **Bottom-Right Handle (◢)**: Resize the window.
- **[X] [^] [_] Buttons**: Close, Maximize/Restore, and Minimize.
- **Scroll Wheel**: Scroll through terminal buffers.

## Usage

### Installation
```bash
cargo build --release
```

### Running
```bash
./target/release/termplex [layout.json]
```

## Configuration

Layouts are stored in JSON format (e.g., `layout.json`):

```json
{
  "windows": [
    {
      "title": "Terminal 1",
      "rect": [1, 1, 80, 24],
      "minimized": false,
      "command": "/bin/bash",
      "args": ["-c", "ls ; exec bash"]
    }
  ]
}
```
