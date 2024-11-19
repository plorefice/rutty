//! The tool's command line interface description.

use std::io::Write;

use anyhow::{anyhow, Context, Result};
use structopt::StructOpt;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    select,
};

use crate::{
    cli::opts::{Connection, Opts, TristateOpt},
    serial::SerialPort,
    ssh::Channel,
};

mod opts;
mod ssh;

/// If this byte is detected as input, enter escape mode.
pub const ESCAPE_BYTE: u8 = 0x01;

/// Utility trait which encapsulates a read/write async stream that can be unpinned.
trait AsyncReadWrite: AsyncRead + AsyncWrite + Unpin {}

impl AsyncReadWrite for SerialPort {}
impl AsyncReadWrite for TcpStream {}
impl AsyncReadWrite for Channel {}

type Terminal = crate::tty::Terminal<tokio::io::Stdin, std::io::Stdout>;

pub async fn run() -> Result<()> {
    let mut cli = Opts::from_args();

    let mut terminal = Terminal::new(tokio::io::stdin(), std::io::stdout())?;

    let mut remote: Box<dyn AsyncReadWrite> = match cli.connection {
        Connection::Serial {
            device,
            baud_rate,
            parameters,
        } => {
            let (bits, parity, stops) = opts::parse_serial_parameters(&parameters)
                .ok_or_else(|| anyhow!("Invalid parameter string: {}", parameters))?;

            let port = SerialPort::with_options()
                .baud_rate(baud_rate)
                .data_bits(bits)
                .parity(parity)
                .stop_bits(stops)
                .open(&device)
                .with_context(|| format!("Could not open {}", &device.display()))?;

            // Use a raw TTY with no local echo over serial port by default
            cli.canonical.prefer(TristateOpt::Off);
            cli.local_echo.prefer(TristateOpt::Off);

            Box::new(port)
        }
        Connection::Tcp { address } => {
            let stream = TcpStream::connect(&address)
                .await
                .with_context(|| format!("Could not connect to {}", &address))?;

            Box::new(stream)
        }
        Connection::Ssh(_) => Box::new(ssh::do_ssh_connection(&mut cli, &mut terminal).await?),
        Connection::Scp(_) => return ssh::do_scp_transfer(&mut cli, &mut terminal).await,
    };

    // Start with a raw mode TTY and start build up from that
    terminal.make_raw()?;

    // Process TTY options
    terminal.set_local_echo(cli.local_echo.into())?;
    terminal.set_canonical_mode(cli.canonical.into())?;

    let mut input_mgr = InputManager::default();

    'repl: loop {
        let mut bufin = [0; 256];
        let mut bufout = [0; 256];

        select! {
            n = terminal.read(&mut bufin) => {
                let n = n?;

                for evt in input_mgr.feed(&bufin[..n]) {
                    match evt {
                        InputEvent::Quit => break 'repl,
                        InputEvent::Key { code } => {
                            remote.write_all(&[code]).await?;
                        }
                    }
                }
            }
            n = remote.read(&mut bufout) => {
                let n = n?;

                terminal.write_all(&bufout[..n])?;
                terminal.flush()?;

                // A zero byte read means EOF
                if n == 0 {
                    break 'repl;
                }
            }
        }
    }

    Ok(())
}

/// Utility to recognize if the escape byte has been entered the specified number of times.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct InputManager {
    escape: bool,
}

impl InputManager {
    /// Converts raw input data into input events.
    pub fn feed<'b>(&'b mut self, bytes: &'b [u8]) -> impl Iterator<Item = InputEvent> + 'b {
        bytes.iter().filter_map(|&b| {
            if self.escape {
                self.escape = false;

                match b {
                    ESCAPE_BYTE => Some(InputEvent::Key { code: ESCAPE_BYTE }),
                    b'x' => Some(InputEvent::Quit),
                    _ => None,
                }
            } else if b == ESCAPE_BYTE {
                self.escape = true;
                None
            } else {
                Some(InputEvent::Key { code: b })
            }
        })
    }
}

/// Events which can be produced by the input manager
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    /// A key was pressed.
    Key { code: u8 },
    /// Exit was requested.
    Quit,
}
