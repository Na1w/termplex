mod protocol;
mod terminal;
mod widgets;
mod window;

mod client;
mod server;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::env;
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::sleep;

use crate::protocol::*;

#[derive(Parser)]
#[command(author, version, about = "A modern terminal multiplexer with window management", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Layout file to load (.json)
    layout: Option<String>,

    /// Host to connect to or bind to
    #[arg(short = 'H', long, default_value = DEFAULT_BIND_ADDR)]
    host: String,

    /// Port to connect to or bind to
    #[arg(short = 'P', long, default_value_t = DEFAULT_PORT)]
    port: u16,

    /// Run as server
    #[arg(short, long)]
    server: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Take a screenshot of the current session or a specific window
    Capture {
        /// Specific window ID to capture (if omitted, capture full session)
        #[arg(short, long)]
        window_id: Option<usize>,

        /// Host to connect to
        #[arg(short = 'H', long, default_value = DEFAULT_BIND_ADDR)]
        host: String,

        /// Port to connect to
        #[arg(short = 'P', long, default_value_t = DEFAULT_PORT)]
        port: u16,
    },

    /// Launch a new terminal window with a command
    Launch {
        /// The command to run
        command: String,

        /// Arguments for the command
        args: Vec<String>,

        /// Host to connect to
        #[arg(short = 'H', long, default_value = DEFAULT_BIND_ADDR)]
        host: String,

        /// Port to connect to
        #[arg(short = 'P', long, default_value_t = DEFAULT_PORT)]
        port: u16,
    },
}

async fn ensure_server_running(host: &str, port: u16, layout: Option<String>) -> Result<()> {
    let addr = format!("{}:{}", host, port);

    // Try to connect
    if TcpStream::connect(&addr).await.is_ok() {
        return Ok(());
    }

    // No server running, start one
    println!("No server found, starting one...");

    // Get our own executable path
    let exe = env::current_exe()?;

    // Start server in background
    let mut cmd = Command::new(&exe);
    cmd.arg("--server")
        .arg("--host")
        .arg(host)
        .arg("--port")
        .arg(port.to_string());
    if let Some(ref path) = layout {
        cmd.arg(path);
    }

    let _child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    // Wait a bit for server to start and retry connection
    for _ in 0..10 {
        sleep(Duration::from_millis(100)).await;
        if TcpStream::connect(&addr).await.is_ok() {
            return Ok(());
        }
    }

    anyhow::bail!("Failed to start server")
}

async fn run_launch_command(
    host: &str,
    port: u16,
    command: String,
    args: Vec<String>,
) -> Result<()> {
    ensure_server_running(host, port, None).await?;

    let addr = format!("{}:{}", host, port);
    let mut stream = TcpStream::connect(&addr).await?;

    // Request new window
    let msg = ClientMessage::CreateWindow {
        x: 10,
        y: 10,
        width: 82,
        height: 26,
        command: Some(command),
        args,
    };
    let data = encode_message(&msg)?;
    stream.write_all(&data).await?;

    println!("Command sent to server at {}", addr);
    Ok(())
}

async fn run_capture_command(host: &str, port: u16, window_id: Option<usize>) -> Result<()> {
    let addr = format!("{}:{}", host, port);
    let mut stream = TcpStream::connect(&addr).await?;

    // Request capture
    let msg = if let Some(id) = window_id {
        ClientMessage::CapturePane { window_id: id }
    } else {
        ClientMessage::CaptureFull
    };
    let data = encode_message(&msg)?;
    stream.write_all(&data).await?;

    // Wait for response
    let mut buf = [0u8; 4096];
    let mut accum = Vec::new();
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        accum.extend_from_slice(&buf[..n]);

        while accum.len() >= 4 {
            let len = u32::from_be_bytes([accum[0], accum[1], accum[2], accum[3]]) as usize;
            if accum.len() < 4 + len {
                break;
            }

            let config = bincode::config::standard().with_fixed_int_encoding();
            match bincode::serde::decode_from_slice::<ServerMessage, _>(&accum[4..4 + len], config)
            {
                Ok((msg, _)) => match msg {
                    ServerMessage::FullCaptured { text } => {
                        print!("{}", text);
                        return Ok(());
                    }
                    ServerMessage::PaneCaptured { text, .. } => {
                        print!("{}", text);
                        return Ok(());
                    }
                    ServerMessage::Error { message } => {
                        anyhow::bail!("Server error: {}", message);
                    }
                    _ => {}
                },
                Err(e) => {
                    eprintln!("Bincode deserialization error in main: {}", e);
                }
            }
            accum.drain(0..4 + len);
        }
    }

    anyhow::bail!("Failed to capture session")
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Handle subcommands
    if let Some(cmd) = cli.command {
        match cmd {
            Commands::Capture {
                host,
                port,
                window_id,
            } => {
                return run_capture_command(&host, port, window_id).await;
            }
            Commands::Launch {
                host,
                port,
                command,
                args,
            } => {
                return run_launch_command(&host, port, command, args).await;
            }
        }
    }

    // Handle server mode
    if cli.server {
        return server::run_server(&cli.host, cli.port, cli.layout).await;
    }

    // Normal client operation
    let addr = format!("{}:{}", cli.host, cli.port);

    // Ensure server is running (starts one if not found)
    ensure_server_running(&cli.host, cli.port, cli.layout.clone()).await?;

    // Connect to server
    match TcpStream::connect(&addr).await {
        Ok(stream) => {
            // Server exists, run as client
            client::run_client(stream, None).await // Layout already loaded by server
        }
        Err(e) => {
            anyhow::bail!("Failed to connect to server: {}", e)
        }
    }
}
