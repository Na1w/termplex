use anyhow::Result;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use vt100::Parser;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

use crate::AppEvent;

pub struct Terminal {
    pub parser: Arc<Mutex<Parser>>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    pub running: Arc<AtomicBool>,
    pub exit_code: Arc<Mutex<Option<i32>>>,
}

impl Terminal {
    pub fn new(
        rows: u16,
        cols: u16,
        event_tx: Sender<AppEvent>,
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
            "cmd.exe".to_string()
        } else {
            "/bin/bash".to_string()
        };

        let mut cmd = CommandBuilder::new(shell);
        for arg in args {
            cmd.arg(arg);
        }

        if !cfg!(target_os = "windows") {
            cmd.env("TERM", "xterm-256color");
        }

        let mut child = pair.slave.spawn_command(cmd)?;

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
                        let _ = tx.send(AppEvent::TerminalUpdate);
                    }
                    Err(_) => break,
                }
            }
            running_clone.store(false, Ordering::SeqCst);
            let _ = tx.send(AppEvent::TerminalUpdate);
        });

        let running_wait = running.clone();
        let tx_wait = event_tx.clone();
        thread::spawn(move || {
            if let Ok(status) = child.wait() {
                let code = if status.success() { 0 } else { 1 };
                *exit_code_clone.lock().unwrap() = Some(code);
            }
            running_wait.store(false, Ordering::SeqCst);
            let _ = tx_wait.send(AppEvent::TerminalUpdate);
        });

        Ok(Self {
            parser,
            master: pair.master,
            writer,
            running,
            exit_code,
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
}
