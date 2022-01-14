use std::{io::Write, path::PathBuf, str::FromStr};

use anyhow::{anyhow, Context, Result};
use colored::Colorize;
use structopt::StructOpt;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    select,
};

use crate::{
    serial::{DataBits, Parity, SerialPort, StopBits},
    tty::Terminal,
};

pub mod serial;
mod termios;
pub mod tty;

// If this byte is detected as input, the program will quit.
const ESCAPE_BYTE: u8 = 0x1d;

#[derive(Debug, StructOpt)]
#[structopt(name = env!("CARGO_PKG_NAME"), about = env!("CARGO_PKG_DESCRIPTION"))]
struct Opts {
    /// Protocol to use to connect to the remote host.
    #[structopt(subcommand)]
    connection: Connection,

    /// Print all characters typed back on the terminal.
    ///
    /// When local echo is enabled, all characters typed in the terminal are automatically echoed
    /// back to the user. If canonical mode is also enabled, special control characters such as
    /// ERASE and NL will behave as expected. If not, only the corresponding representation will be
    /// echoed back, without any effect on the previously echoed characters.
    ///
    /// When local echo is disabled, any character typed in the terminal is printed back only if
    /// the remote host is programmed to do so. Some remote hosts however do not provide this
    /// functionality, in which cases local echo provides a visual feedback of each character
    /// typed.
    ///
    /// Local echo is enabled by default for most protocols, and disabled only for connections
    /// over serial port.
    #[structopt(short = "E", long = "echo", default_value = "auto")]
    local_echo: Tristate<true>,

    /// Proocess input line by line.
    ///
    /// When canonical mode is enabled, nothing will be sent until the return key is pressed, at
    /// which point the whole line of text is sent at once. This allows for local line editing
    /// before sending, without the remote host ever seeing the intermediate edits.
    ///
    /// When canonical mode is disabled (also called "raw" mode), each character typed in
    /// the terminal is immediately sent to the remote host, without any preprocessing.
    ///
    /// Canonical mode is enabled by default for most protocols, and disabled only for connections
    /// over serial port.
    #[structopt(short = "C", long = "canonical", default_value = "auto")]
    canonical: Tristate<true>,
}

/// A type of CLI flag that can be either forced on, off or be in a default state.
///
/// The default state is selected at compile time using a const-generic bool parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, StructOpt)]
enum Tristate<const DEFAULT: bool> {
    On,
    Off,
    Auto,
}

impl<const DEFAULT: bool> FromStr for Tristate<DEFAULT> {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "on" => Ok(Self::On),
            "off" => Ok(Self::Off),
            "auto" => Ok(Self::Auto),
            _ => Err("tristate options must be one of \"on\", \"off\" or \"auto\""),
        }
    }
}

impl<const DEFAULT: bool> From<Tristate<DEFAULT>> for bool {
    fn from(o: Tristate<DEFAULT>) -> Self {
        match o {
            Tristate::On => true,
            Tristate::Off => false,
            Tristate::Auto => DEFAULT,
        }
    }
}

impl<const DEFAULT: bool> Tristate<DEFAULT> {
    pub fn prefer(&mut self, state: Tristate<DEFAULT>) {
        if *self == Self::Auto {
            *self = state;
        }
    }
}

#[derive(Debug, StructOpt)]
enum Connection {
    /// Open a serial connection on the specified device file.
    Serial {
        /// Device to connect to.
        #[structopt(parse(from_os_str))]
        device: PathBuf,

        /// Communication speed in bits per second.
        #[structopt(default_value = "115200")]
        baud_rate: u32,

        /// Configuration string for data bits, parity and stop bits.
        #[structopt(default_value = "8N1")]
        parameters: String,
    },
    /// Open a raw TCP stream towards the specified host.
    Tcp {
        /// Address and port of the remote host.
        ///
        /// Supported formats are <ip>:<port> and <uri>:<port>.
        address: String,
    },
}

/// Utility trait which encapsulates a read/write async stream that can be unpinned.
trait AsyncReadWrite: AsyncRead + AsyncWrite + Unpin {}

impl AsyncReadWrite for SerialPort {}
impl AsyncReadWrite for TcpStream {}

#[tokio::main]
async fn main() -> Result<()> {
    let mut opts = Opts::from_args();

    let mut terminal = Terminal::new(tokio::io::stdin(), std::io::stdout())?;

    // Start with a raw mode TTY and start build up from that
    terminal.make_raw()?;

    let mut remote: Box<dyn AsyncReadWrite> = match opts.connection {
        Connection::Serial {
            device,
            baud_rate,
            parameters,
        } => {
            let (bits, parity, stops) = parse_parameter_string(&parameters)
                .ok_or_else(|| anyhow!("Invalid parameter string: {}", parameters))?;

            let port = SerialPort::with_options()
                .baud_rate(baud_rate)
                .data_bits(bits)
                .parity(parity)
                .stop_bits(stops)
                .open(&device)
                .with_context(|| format!("Could not open {}", &device.display()))?;

            // Use a raw TTY with no local echo over serial port by default
            opts.canonical.prefer(Tristate::Off);
            opts.local_echo.prefer(Tristate::Off);

            Box::new(port)
        }
        Connection::Tcp { address } => {
            let stream = TcpStream::connect(&address)
                .await
                .with_context(|| format!("Could not connect to {}", &address))?;

            Box::new(stream)
        }
    };

    // Usage instructions
    writeln!(
        terminal,
        "{}",
        "Connection established. Press ^] to quit.\r".bold()
    )?;

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

fn parse_parameter_string(s: &str) -> Option<(DataBits, Parity, StopBits)> {
    let mut chars = s.chars();

    let bits = match chars.next()? {
        '5' => DataBits::Five,
        '6' => DataBits::Six,
        '7' => DataBits::Seven,
        '8' => DataBits::Eight,
        _ => return None,
    };

    let parity = match chars.next()? {
        'N' | 'n' => Parity::None,
        'E' | 'e' => Parity::Even,
        'O' | 'o' => Parity::Odd,
        _ => return None,
    };

    let stops = match chars.next()? {
        '1' => StopBits::One,
        '2' => StopBits::Two,
        _ => return None,
    };

    Some((bits, parity, stops))
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct EscapeDetector(u32);

impl EscapeDetector {
    pub fn feed(&mut self, byte: u8) -> bool {
        if byte == ESCAPE_BYTE {
            self.0 += 1;
        } else {
            self.0 = 0;
        }
        self.0 == 1
    }
}
