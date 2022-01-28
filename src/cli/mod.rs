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

/// If this byte is detected as input, the program will quit.
pub const ESCAPE_BYTE: u8 = 0x1d;

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

    let mut exiter = EscapeDetector::default();

    'repl: loop {
        let mut bufin = [0; 256];
        let mut bufout = [0; 256];

        select! {
            n = terminal.read(&mut bufin) => {
                let n = n?;

                // Break from the loop if the escape sequence is detected
                if bufin[..n].iter().any(|&b| exiter.feed(b)) {
                    break 'repl;
                }

                remote.write_all(&bufin[..n]).await?;
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
pub struct EscapeDetector(u32);

impl EscapeDetector {
    /// Feeds a new byte to the detector and returns true if the specified number of consecutive
    /// escape bytes have been detected.
    pub fn feed(&mut self, byte: u8) -> bool {
        if byte == ESCAPE_BYTE {
            self.0 += 1;
        } else {
            self.0 = 0;
        }
        self.0 == 1
    }
}
