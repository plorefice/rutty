use std::{io::Write, path::PathBuf};

use anyhow::{anyhow, Context, Result};
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
pub mod tty;

#[derive(Debug, StructOpt)]
#[structopt(name = env!("CARGO_PKG_NAME"), about = env!("CARGO_PKG_DESCRIPTION"))]
struct Opts {
    /// Device to connect to.
    #[structopt(parse(from_os_str))]
    device: PathBuf,

    /// Communication speed in bits per second.
    #[structopt(default_value = "115200")]
    baud_rate: u32,

    /// Configuration string for data bits, parity and stop bits.
    #[structopt(default_value = "8N1")]
    parameters: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let opts = Opts::from_args();

    let (bits, parity, stops) = parse_parameter_string(&opts.parameters)
        .ok_or_else(|| anyhow!("Invalid parameter string: {}", opts.parameters))?;

    let mut port = SerialPort::with_options()
        .baud_rate(opts.baud_rate)
        .data_bits(bits)
        .parity(parity)
        .stop_bits(stops)
        .open(&opts.device)
        .with_context(|| format!("Could not open {}", &opts.device.display()))?;

    let mut terminal = Terminal::new(tokio::io::stdin(), std::io::stdout())?;
    let mut exiter = EscapeDetector::default();

    terminal.make_raw()?;

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

                port.write_all(&bufin[..n]).await?;
            }
            n = port.read(&mut bufout) => {
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
