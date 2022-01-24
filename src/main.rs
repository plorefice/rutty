//! Rutty is a terminal emulator and serial console that aims to supports the most common network
//! and file transfer protocols in a single command-line tool.

#![warn(missing_docs)]

use std::io::Write;

use anyhow::{anyhow, bail, Context, Result};
use nix::unistd::{Uid, User};
use structopt::StructOpt;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    runtime::Builder,
    select,
};
use zeroize::Zeroize;

use crate::{
    cli::{Connection, EscapeDetector, Opts, TristateOpt},
    serial::SerialPort,
    ssh::{Channel, Session},
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
impl AsyncReadWrite for Channel {}

fn main() -> Result<()> {
    let runtime = Builder::new_current_thread()
        .thread_name("rutty")
        .enable_all()
        .build()?;

    runtime.block_on(async { run().await })?;

    // Normally, this wouldn't be necessary. The problem here is that tokio::io::Stdin spawns
    // a new thread with a blocking read operation inside each time a read operation is issued,
    // causing the runtime thread to hang until the user presses enter (or EOF is reached).
    //
    // When using it to run an interactive shell however, an early termination of the shell, due
    // for example to the remote host closing the connection on its end, causes the program to hang
    // until the next enter keypress, which is kinda ugly, UX-wise.
    //
    // To avoid this, we drop the runtime in background, which immediately kills all pending tasks.
    // It shouldn't be an issue since all the other objects have already been dropped.
    runtime.shutdown_background();

    Ok(())
}

async fn run() -> Result<()> {
    let mut cli = Opts::from_args();

    let mut terminal = Terminal::new(tokio::io::stdin(), std::io::stdout())?;

    let mut remote: Box<dyn AsyncReadWrite> = match cli.connection {
        Connection::Serial {
            device,
            baud_rate,
            parameters,
        } => {
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
        Connection::Ssh {
            destination,
            opts,
            command,
        } => {
            let (username, address) = match destination.split_once('@') {
                Some((username, address)) => (username.to_string(), address.to_string()),
                None => match User::from_uid(Uid::effective())? {
                    Some(user) => (user.name, destination),
                    None => bail!("Could not retrieve username"),
                },
            };

            let session = Session::new((address.as_str(), opts.port))
                .await
                .with_context(|| format!("Could not connect to {}", &address))?;

            // Authenticate with the server.
            // Try using the agent first, and fallback on password authentication.
            let mut channel = match session.authenticate_with_agent(&username).await {
                Ok(channel) => channel,
                Err(_) => {
                    // Show a prompt to the user
                    write!(terminal, "Password: ")?;
                    terminal.flush()?;

                    let mut password = terminal.input_password().await?;
                    writeln!(terminal)?;

                    let channel = session
                        .authenticate_with_password(&username, &password)
                        .await;

                    // Securely clear password from memory
                    password.zeroize();

                    channel?
                }
            };

            if let Some((command, args)) = command.split_first() {
                // Run the command and print the output.
                // Don't bother exiting early here, the SSH channel will receive a EOF anyway.
                let output = channel.run(command, args).await?;
                terminal.write_all(output.as_bytes())?;
            } else {
                // After authentication, create the virtual terminal and the shell
                let size = terminal.get_size()?;
                channel.request_pty(size).await?;
                channel.shell().await?;
            }

            // SSH shells require a raw TTY
            cli.canonical.prefer(TristateOpt::Off);
            cli.local_echo.prefer(TristateOpt::Off);

            Box::new(channel)
        }
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
