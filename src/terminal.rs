use anyhow::Result;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use vt100::Parser;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

/// Simple event type for terminal updates
#[derive(Debug, Clone)]
pub enum TermEvent {
    Update,
    Closed,
}

pub struct Terminal {
    pub parser: Arc<Mutex<Parser>>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    pub running: Arc<AtomicBool>,
    pub exit_code: Arc<Mutex<Option<i32>>>,
    pub child_pid: u32,
}

impl Terminal {
    pub fn new(
        rows: u16,
        cols: u16,
        event_tx: Sender<TermEvent>,
        command: Option<String>,
        args: Vec<String>,
    ) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let shell = if let Some(cmd) = command {
            cmd
        } else if cfg!(target_os = "windows") {
            std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
        } else {
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string())
        };

        let mut cmd = CommandBuilder::new(&shell);

        // If it's a default shell (not a specific command), start it as a login shell
        if args.is_empty() && !cfg!(target_os = "windows") {
            cmd.arg("-l");
        }

        for arg in args {
            cmd.arg(arg);
        }

        if !cfg!(target_os = "windows") {
            cmd.env("TERM", "xterm-256color");
        }

        let mut child = pair.slave.spawn_command(cmd)?;
        let child_pid = child.process_id().unwrap_or(0);

        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let parser = Arc::new(Mutex::new(Parser::new(rows, cols, 3000)));
        let parser_clone = parser.clone();

        let running = Arc::new(AtomicBool::new(true));
        let running_clone = running.clone();

        let exit_code = Arc::new(Mutex::new(None));
        let exit_code_clone = exit_code.clone();

        let tx = event_tx.clone();
        thread::spawn(move || {
            let mut reader = reader;
            let mut byte_buf = [0u8; 4096];
            loop {
                match reader.read(&mut byte_buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        {
                            let mut p = parser_clone.lock().unwrap();
                            p.process(&byte_buf[..n]);
                        }
                        let _ = tx.send(TermEvent::Update);
                    }
                    Err(_) => break,
                }
            }
            running_clone.store(false, Ordering::SeqCst);
            let _ = tx.send(TermEvent::Update);
            let _ = tx.send(TermEvent::Closed);
        });

        let running_wait = running.clone();
        let tx_wait = event_tx.clone();
        thread::spawn(move || {
            if let Ok(status) = child.wait() {
                let code = if status.success() { 0 } else { 1 };
                *exit_code_clone.lock().unwrap() = Some(code);
            }
            running_wait.store(false, Ordering::SeqCst);
            let _ = tx_wait.send(TermEvent::Update);
        });

        Ok(Self {
            parser,
            master: pair.master,
            writer,
            running,
            exit_code,
            child_pid,
        })
    }

    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        self.writer.write_all(data)?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut p = self.parser.lock().unwrap();
        p.screen_mut().set_size(rows, cols);
        Ok(())
    }

    pub fn get_foreground_command(&self) -> Option<(String, Vec<String>)> {
        if cfg!(target_os = "windows") {
            return None;
        }

        // 1. Get the TTY name for our child process (the shell)
        let tty_output = std::process::Command::new("ps")
            .arg("-p")
            .arg(format!("{}", self.child_pid))
            .arg("-o")
            .arg("tty=")
            .output()
            .ok()?;

        let tty = String::from_utf8_lossy(&tty_output.stdout)
            .trim()
            .to_string();
        if tty.is_empty() || tty == "??" {
            return None;
        }

        // 2. Get processes on that TTY
        // We look for all processes and find the one with '+' in STAT
        let ps_output = std::process::Command::new("ps")
            .arg("-t")
            .arg(&tty)
            .arg("-o")
            .arg("stat,command")
            .output()
            .ok()?;

        let ps_str = String::from_utf8_lossy(&ps_output.stdout);
        let mut lines = ps_str.lines();
        let _header = lines.next(); // Skip header

        for line in lines {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            // Parse STAT and the rest as COMMAND_LINE
            let mut parts = line.split_whitespace();
            let stat = parts.next()?;
            let full_cmd: Vec<String> = parts.map(|s| s.to_string()).collect();

            if stat.contains('+') && !full_cmd.is_empty() {
                let cmd = full_cmd[0].clone();
                let args = full_cmd[1..].to_vec();

                // Normalize command name
                let cmd_path = std::path::Path::new(&cmd);
                let cmd_name = cmd_path
                    .file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default();
                let cmd_name = cmd_name.trim_start_matches('-');

                // Skip common shells
                if matches!(cmd_name, "bash" | "zsh" | "sh" | "fish" | "dash") {
                    continue;
                }

                return Some((cmd, args));
            }
        }

        None
    }

    pub fn shutdown(&self) {
        if self.running.load(Ordering::SeqCst) && self.child_pid != 0 {
            #[cfg(unix)]
            unsafe {
                libc::kill(self.child_pid as libc::pid_t, libc::SIGHUP);
            }
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.shutdown();
    }
}
