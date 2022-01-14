use std::{io, os::unix::prelude::AsRawFd};

use nix::sys::{
    self,
    termios::{BaudRate, ControlFlags, FlushArg, InputFlags, LocalFlags, SetArg},
};

use crate::serial::{DataBits, FlowControl, Parity, StopBits};

#[derive(Debug, Clone)]
pub struct Termios {
    inner: sys::termios::Termios,
}

impl Termios {
    pub fn from_raw_fd<F: AsRawFd>(fd: F) -> io::Result<Self> {
        Ok(Self {
            inner: sys::termios::tcgetattr(fd.as_raw_fd())?,
        })
    }

    pub fn apply<F: AsRawFd>(&self, fd: F) -> io::Result<()> {
        let fd = fd.as_raw_fd();

        sys::termios::tcflush(fd, FlushArg::TCIFLUSH)?;
        sys::termios::tcsetattr(fd, SetArg::TCSANOW, &self.inner)?;

        Ok(())
    }

    pub fn make_raw(&mut self) {
        sys::termios::cfmakeraw(&mut self.inner);
    }

    pub fn set_baud_rate(&mut self, baud_rate: u32) -> io::Result<()> {
        let baud_rate = match baud_rate {
            0 => BaudRate::B0,
            50 => BaudRate::B50,
            75 => BaudRate::B75,
            110 => BaudRate::B110,
            134 => BaudRate::B134,
            150 => BaudRate::B150,
            200 => BaudRate::B200,
            300 => BaudRate::B300,
            600 => BaudRate::B600,
            1200 => BaudRate::B1200,
            1800 => BaudRate::B1800,
            2400 => BaudRate::B2400,
            4800 => BaudRate::B4800,
            9600 => BaudRate::B9600,
            19200 => BaudRate::B19200,
            38400 => BaudRate::B38400,
            57600 => BaudRate::B57600,
            115200 => BaudRate::B115200,
            230400 => BaudRate::B230400,
            460800 => BaudRate::B460800,
            500000 => BaudRate::B500000,
            576000 => BaudRate::B576000,
            921600 => BaudRate::B921600,
            1000000 => BaudRate::B1000000,
            1152000 => BaudRate::B1152000,
            1500000 => BaudRate::B1500000,
            2000000 => BaudRate::B2000000,
            2500000 => BaudRate::B2500000,
            3000000 => BaudRate::B3000000,
            3500000 => BaudRate::B3500000,
            4000000 => BaudRate::B4000000,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "unsupported baud rate",
                ))
            }
        };

        sys::termios::cfsetispeed(&mut self.inner, baud_rate)?;
        sys::termios::cfsetospeed(&mut self.inner, baud_rate)?;

        Ok(())
    }

    pub fn set_data_bits(&mut self, data_bits: DataBits) {
        self.inner.control_flags -= ControlFlags::CSIZE;
        self.inner.control_flags |= match data_bits {
            DataBits::Five => ControlFlags::CS5,
            DataBits::Six => ControlFlags::CS6,
            DataBits::Seven => ControlFlags::CS7,
            DataBits::Eight => ControlFlags::CS8,
        };
    }

    pub fn set_parity(&mut self, parity: Parity) {
        match parity {
            Parity::None => {
                self.inner.control_flags -= ControlFlags::PARENB | ControlFlags::PARODD;
                self.inner.input_flags |= InputFlags::IGNPAR;
            }
            Parity::Even => {
                self.inner.control_flags -= ControlFlags::PARODD;
                self.inner.control_flags |= ControlFlags::PARENB;
                self.inner.input_flags |= InputFlags::INPCK;
                self.inner.input_flags -= InputFlags::IGNPAR;
            }
            Parity::Odd => {
                self.inner.control_flags |= ControlFlags::PARENB | ControlFlags::PARODD;
                self.inner.input_flags |= InputFlags::INPCK;
                self.inner.input_flags -= InputFlags::IGNPAR;
            }
        }
    }

    pub fn set_stop_bits(&mut self, stop_bits: StopBits) {
        match stop_bits {
            StopBits::One => self.inner.control_flags &= !ControlFlags::CSTOPB,
            StopBits::Two => self.inner.control_flags |= ControlFlags::CSTOPB,
        }
    }

    pub fn set_flow_control(&mut self, flow_control: FlowControl) {
        match flow_control {
            FlowControl::None => {
                self.inner.input_flags -= InputFlags::IXON | InputFlags::IXOFF;
                self.inner.control_flags -= ControlFlags::CRTSCTS;
            }
            FlowControl::Software => {
                self.inner.input_flags |= InputFlags::IXON | InputFlags::IXOFF;
                self.inner.control_flags -= ControlFlags::CRTSCTS;
            }
            FlowControl::Hardware => {
                self.inner.input_flags -= InputFlags::IXON | InputFlags::IXOFF;
                self.inner.control_flags |= ControlFlags::CRTSCTS;
            }
        }
    }

    pub fn set_local_echo(&mut self, local_echo: bool) {
        if local_echo {
            self.inner.local_flags |= LocalFlags::ECHO;
        } else {
            self.inner.local_flags -= LocalFlags::ECHO;
        }
    }
}

impl AsRef<sys::termios::Termios> for Termios {
    fn as_ref(&self) -> &sys::termios::Termios {
        &self.inner
    }
}

impl AsMut<sys::termios::Termios> for Termios {
    fn as_mut(&mut self) -> &mut sys::termios::Termios {
        &mut self.inner
    }
}
