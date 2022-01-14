use std::{io::Write, path::PathBuf};

use anyhow::{anyhow, Context, Result};
use colored::Colorize;
use structopt::StructOpt;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    select,
};

use crate::{
    serial::{DataBits, Parity, SerialPort, StopBits},
    tty::Terminal,
};

pub mod serial;
mod termios;
pub mod tty;

#[derive(Debug, StructOpt)]
#[structopt(name = env!("CARGO_PKG_NAME"), about = env!("CARGO_PKG_DESCRIPTION"))]
struct Opts {
    /// Protocol to use to connect to the remote host.
    #[structopt(subcommand)]
    connection: Connection,

    /// Enable local echo, printing all characters typed back on the terminal.
    ///
    /// By default, local echo is disabled and any character typed in the terminal is printed
    /// back only if the remote host is configured to do so. Some remote hosts however do not
    /// provide this functionality, in which cases local echo provides a visual feedback of each
    /// character typed.
    ///
    /// If canonical mode is also enabled, special control characters such as ERASE and NL will
    /// behave as expected. If not, only the corresponding representation will be echoed back,
    /// without any effect on the previously echoed characters.
    #[structopt(short = "E", long = "echo")]
    local_echo: bool,

    /// Enable canonical input mode, processing input line by line.
    ///
    /// By default, when opening a remote connection each character typed in the terminal is
    /// immediately sent to the remote host.
    ///
    /// If canonical mode is enabled, nothing will be sent until a whole line has been input and
    /// the return character has been pressed. This allows for local line editing before sending.
    /// This option is mostly used in conjunction with the local echo option.
    ///
    /// When using this option, Return must be pressed after the escape sequence to quit.
    #[structopt(short = "C", long = "canonical")]
    canonical: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let opts = Opts::from_args();

    let mut remote = match opts.connection {
        Connection::Serial {
            device,
            baud_rate,
            parameters,
        } => {
            let (bits, parity, stops) = parse_parameter_string(&parameters)
                .ok_or_else(|| anyhow!("Invalid parameter string: {}", parameters))?;

            SerialPort::with_options()
                .baud_rate(baud_rate)
                .data_bits(bits)
                .parity(parity)
                .stop_bits(stops)
                .open(&device)
                .with_context(|| format!("Could not open {}", &device.display()))?
        }
    };

    // Usage instructions
    println!(
        "{}",
        "Connection established. Press CTRL-A three times to quit.".bold()
    );

    let mut terminal = Terminal::new(tokio::io::stdin(), std::io::stdout())?;
    let mut exiter = EscapeDetector::default();

    terminal.make_raw()?;

    // Process options
    terminal.set_local_echo(opts.local_echo)?;
    terminal.set_canonical_mode(opts.canonical)?;

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
    const ESCAPE_BYTE: u8 = b'\x01';

    pub fn feed(&mut self, byte: u8) -> bool {
        if byte == Self::ESCAPE_BYTE {
            self.0 += 1;
        } else {
            self.0 = 0;
        }
        self.0 == 3
    }
}
