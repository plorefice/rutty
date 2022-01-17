use std::io::Write;

use anyhow::{anyhow, bail, Context, Result};
use futures::TryFutureExt;
use nix::unistd::{Uid, User};
use structopt::StructOpt;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    select,
};
use zeroize::Zeroize;

use crate::{
    cli::{Connection, EscapeDetector, Opts, TristateOpt},
    serial::SerialPort,
    ssh::Session,
    tty::Terminal,
};

pub mod cli;
pub mod serial;
pub mod ssh;
pub mod termios;
pub mod tty;

/// Utility trait which encapsulates a read/write async stream that can be unpinned.
trait AsyncReadWrite: AsyncRead + AsyncWrite + Unpin {}

impl AsyncReadWrite for SerialPort {}
impl AsyncReadWrite for TcpStream {}
impl AsyncReadWrite for Session {}

#[tokio::main]
async fn main() -> Result<()> {
    let mut opts = Opts::from_args();

    let mut terminal = Terminal::new(tokio::io::stdin(), std::io::stdout())?;

    let mut remote: Box<dyn AsyncReadWrite> = match opts.connection {
        Connection::Serial {
            device,
            baud_rate,
            parameters,
        } => {
            writeln!(terminal, "Opening {}...", &device.display())?;

            let (bits, parity, stops) = cli::parse_serial_parameters(&parameters)
                .ok_or_else(|| anyhow!("Invalid parameter string: {}", parameters))?;

            let port = SerialPort::with_options()
                .baud_rate(baud_rate)
                .data_bits(bits)
                .parity(parity)
                .stop_bits(stops)
                .open(&device)
                .with_context(|| format!("Could not open {}", &device.display()))?;

            // Use a raw TTY with no local echo over serial port by default
            opts.canonical.prefer(TristateOpt::Off);
            opts.local_echo.prefer(TristateOpt::Off);

            Box::new(port)
        }
        Connection::Tcp { address } => {
            writeln!(terminal, "Connecting to {}...", address)?;

            let stream = TcpStream::connect(&address)
                .await
                .with_context(|| format!("Could not connect to {}", &address))?;

            Box::new(stream)
        }
        Connection::Ssh { destination } => {
            let (username, address) = match destination.split_once('@') {
                Some((username, address)) => (username.to_string(), address.to_string()),
                None => match User::from_uid(Uid::effective())? {
                    Some(user) => (user.name, destination),
                    None => bail!("Could not retrieve username"),
                },
            };

            let session = Session::new(&address)
                .await
                .with_context(|| format!("Could not connect to {}", &address))?;

            // Authenticate with the server.
            // Try using the agent first, and fallback on password authentication.
            session
                .authenticate_with_agent(&username)
                .or_else(|_| async {
                    let mut password = terminal.input_password(Some("Password: ")).await?;

                    let res = session
                        .authenticate_with_password(&username, &password)
                        .await;

                    // Securely clear password from memory
                    password.zeroize();

                    res
                })
                .await?;

            Box::new(session)
        }
    };

    // Usage instructions
    writeln!(terminal, "Connected, press ^] to quit.\n")?;

    // Start with a raw mode TTY and start build up from that
    terminal.make_raw()?;

    // Process TTY options
    terminal.set_local_echo(opts.local_echo.into())?;
    terminal.set_canonical_mode(opts.canonical.into())?;

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
                terminal.write_all(&bufout[..n?])?;
                terminal.flush()?;
            }
        }
    }

    Ok(())
}
