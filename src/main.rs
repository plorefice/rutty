use std::{
    io::{self, Write},
    path::PathBuf,
};

use anyhow::Context;
use nix::{
    libc::STDIN_FILENO,
    sys::{
        self,
        termios::{FlushArg, SetArg, SpecialCharacterIndices},
    },
};
use structopt::StructOpt;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    select,
};

use crate::serial::SerialPort;

pub mod serial;
mod termios;

#[derive(Debug, StructOpt)]
#[structopt(name = env!("CARGO_PKG_NAME"), about = env!("CARGO_PKG_DESCRIPTION"))]
struct Opts {
    /// Device to connect to
    #[structopt(parse(from_os_str))]
    device: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let opts = Opts::from_args();

    let mut port = SerialPort::open(&opts.device)
        .with_context(|| format!("Could not open {}", &opts.device.display()))?;

    let term_state = make_raw(STDIN_FILENO)?;

    let mut stdin = tokio::io::stdin();
    let mut stdout = std::io::stdout();

    let mut exiter = EscapeDetector::default();

    'repl: loop {
        let mut bufin = [0; 1];
        let mut bufout = [0; 256];

        select! {
            n = stdin.read(&mut bufin) => {
                let n = n?;

                // Break from the loop if the escape sequence is detected
                if bufin[..n].iter().any(|&b| exiter.feed(b)) {
                    break 'repl;
                }

                port.write_all(&bufin[..n]).await?;
            }
            n = port.read(&mut bufout) => {
                stdout.write_all(&bufout[..n?])?;
                stdout.flush()?;
            }
        }
    }

    sys::termios::tcsetattr(STDIN_FILENO, SetArg::TCSANOW, &term_state)?;

    Ok(())
}

fn make_raw(fd: i32) -> io::Result<sys::termios::Termios> {
    let mut termios = sys::termios::tcgetattr(fd)?;
    let old_state = termios.clone();

    sys::termios::cfmakeraw(&mut termios);

    termios.control_chars[SpecialCharacterIndices::VMIN as usize] = 1;

    sys::termios::tcflush(fd, FlushArg::TCIFLUSH)?;
    sys::termios::tcsetattr(fd, SetArg::TCSANOW, &termios)?;

    Ok(old_state)
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
