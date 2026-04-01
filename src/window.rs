use crate::AppEvent;
use crate::terminal::Terminal;
use ratatui::layout::Rect;
use std::sync::mpsc::Sender;

pub struct Window {
    pub title: String,
    pub rect: Rect,
    pub terminal: Terminal,
    pub focused: bool,
    pub minimized: bool,
    pub saved_rect: Option<Rect>,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub scroll_offset: usize,
}

impl Window {
    pub fn new(
        _id: usize,
        title: String,
        rect: Rect,
        event_tx: Sender<AppEvent>,
        command: Option<String>,
        args: Vec<String>,
    ) -> anyhow::Result<Self> {
        let rows = rect.height.saturating_sub(2);
        let cols = rect.width.saturating_sub(2);
        let terminal = Terminal::new(rows, cols, event_tx, command.clone(), args.clone())?;

        Ok(Self {
            title,
            rect,
            terminal,
            focused: false,
            minimized: false,
            saved_rect: None,
            command,
            args,
            scroll_offset: 0,
        })
    }

    pub fn resize(&mut self, rect: Rect) -> anyhow::Result<()> {
        self.rect = rect;
        let rows = rect.height.saturating_sub(2);
        let cols = rect.width.saturating_sub(2);
        self.terminal.resize(rows, cols)?;
        Ok(())
    }
}
