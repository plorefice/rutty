use std::{io::Write, path::PathBuf};

use anyhow::{Context, Result};
use structopt::StructOpt;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    select,
};

use crate::{serial::SerialPort, tty::Terminal};

pub mod serial;
pub mod tty;

#[derive(Debug, StructOpt)]
#[structopt(name = env!("CARGO_PKG_NAME"), about = env!("CARGO_PKG_DESCRIPTION"))]
struct Opts {
    /// Device to connect to
    #[structopt(parse(from_os_str))]
    device: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let opts = Opts::from_args();

    let mut port = SerialPort::open(&opts.device)
        .with_context(|| format!("Could not open {}", &opts.device.display()))?;

    let mut terminal = Terminal::new(tokio::io::stdin(), std::io::stdout())?;
    let mut exiter = EscapeDetector::default();

    terminal.make_raw()?;

    'repl: loop {
        let mut bufin = [0; 1];
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
