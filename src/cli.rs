//! The tool's command line interface description.

use std::{path::PathBuf, str::FromStr};

use structopt::StructOpt;

use crate::serial::{DataBits, Parity, StopBits};

/// If this byte is detected as input, the program will quit.
pub const ESCAPE_BYTE: u8 = 0x1d;

/// A structure built from command-line options.
#[derive(Debug, StructOpt)]
#[structopt(name = env!("CARGO_PKG_NAME"), about = env!("CARGO_PKG_DESCRIPTION"))]
pub struct Opts {
    /// Protocol to use to connect to the remote host.
    #[structopt(subcommand)]
    pub connection: Connection,

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
    pub local_echo: TristateOpt<true>,

    /// Process input line by line.
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
    pub canonical: TristateOpt<true>,
}

/// A type of CLI flag that can be either forced on, off or be in a default state.
///
/// The default state is selected at compile time using a const-generic bool parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, StructOpt)]
pub enum TristateOpt<const DEFAULT: bool> {
    /// Option is enabled
    On,
    /// Option is disabled
    Off,
    /// Let the tool decide the best configuration
    Auto,
}

impl<const DEFAULT: bool> FromStr for TristateOpt<DEFAULT> {
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

impl<const DEFAULT: bool> From<TristateOpt<DEFAULT>> for bool {
    fn from(o: TristateOpt<DEFAULT>) -> Self {
        match o {
            TristateOpt::On => true,
            TristateOpt::Off => false,
            TristateOpt::Auto => DEFAULT,
        }
    }
}

impl<const DEFAULT: bool> TristateOpt<DEFAULT> {
    /// Changes the option value to the specified state only if `auto` is currently selected.
    pub fn prefer(&mut self, state: TristateOpt<DEFAULT>) {
        if *self == Self::Auto {
            *self = state;
        }
    }
}

/// Protocol to use to connect to the remote host.
#[derive(Debug, StructOpt)]
pub enum Connection {
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
    /// Open a SSH session towards the specified destination.
    Ssh {
        /// Destination of the remote host.
        ///
        /// The recognized format is [user@]host[:port].
        /// The host can be either an IP address or a hostname.
        destination: String,
    },
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

/// Parses a serial parameter string comprised of data bits, parity and stop bits in canonical
/// form (eg. `8N1').
pub fn parse_serial_parameters(s: &str) -> Option<(DataBits, Parity, StopBits)> {
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
