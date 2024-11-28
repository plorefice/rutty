//! Types for working with serial devices.

use std::{
    io::{self, Read, Write},
    os::{
        fd::{FromRawFd, OwnedFd},
        unix::prelude::{AsRawFd, RawFd},
    },
    path::Path,
    pin::Pin,
    task::{Context, Poll},
};

use futures::ready;
use nix::{
    fcntl::{self, FcntlArg, OFlag},
    sys::stat::Mode,
    sys::{self, termios::ControlFlags},
    unistd,
};
use tokio::io::{unix::AsyncFd, AsyncRead, AsyncWrite, ReadBuf};

use crate::termios::Termios;

/// A reference to an open serial port device.
pub struct SerialPort {
    inner: AsyncFd<TtyDevice>,
}

impl SerialPort {
    /// Opens the serial port corresponding to the specified device node with default options.
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        Self::with_options().open(path)
    }

    /// Returns a new `SerialPortOptions` object that can be used to open a serial port with
    /// configurable communication parameters.
    pub fn with_options() -> SerialPortOptions {
        SerialPortOptions::default()
    }

    fn open_with_options<P: AsRef<Path>>(path: P, opts: SerialPortOptions) -> io::Result<Self> {
        let tty = TtyDevice::open(path)?;

        // Make the file descriptor non-blocking
        let fd = tty.as_raw_fd();
        let flags = OFlag::from_bits_retain(fcntl::fcntl(fd, FcntlArg::F_GETFL)?);
        fcntl::fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;

        let mut port = Self {
            inner: AsyncFd::new(tty)?,
        };

        // Configure port parameters
        port.set_baud_rate(opts.baud_rate)?;
        port.set_data_bits(opts.data_bits)?;
        port.set_parity(opts.parity)?;
        port.set_stop_bits(opts.stop_bits)?;
        port.set_flow_control(FlowControl::None)?;

        Ok(port)
    }

    /// Configures the baud rate used for communication on this serial port.
    pub fn set_baud_rate(&mut self, baud_rate: u32) -> io::Result<()> {
        let fd = &self.inner;
        let mut termios = Termios::from_fd(fd)?;
        termios.set_baud_rate(baud_rate)?;
        termios.apply(fd)
    }

    /// Configures the number of data bits used for communication on this serial port.
    pub fn set_data_bits(&mut self, data_bits: DataBits) -> io::Result<()> {
        let fd = &self.inner;
        let mut termios = Termios::from_fd(fd)?;
        termios.set_data_bits(data_bits);
        termios.apply(fd)
    }

    /// Configures the number of stop bits used for communication on this serial port.
    pub fn set_stop_bits(&mut self, stop_bits: StopBits) -> io::Result<()> {
        let fd = &self.inner;
        let mut termios = Termios::from_fd(fd)?;
        termios.set_stop_bits(stop_bits);
        termios.apply(fd)
    }

    /// Configures the parity check used for communication on this serial port.
    pub fn set_parity(&mut self, parity: Parity) -> io::Result<()> {
        let fd = &self.inner;
        let mut termios = Termios::from_fd(fd)?;
        termios.set_parity(parity);
        termios.apply(fd)
    }

    /// Configures the flow control used for communication on this serial port.
    pub fn set_flow_control(&mut self, flow_control: FlowControl) -> io::Result<()> {
        let fd = &self.inner;
        let mut termios = Termios::from_fd(fd)?;
        termios.set_flow_control(flow_control);
        termios.apply(fd)
    }
}

impl AsyncRead for SerialPort {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut guard = ready!(self.inner.poll_read_ready_mut(cx))?;

            match guard.try_io(|inner| inner.get_mut().read(buf.initialize_unfilled())) {
                Ok(Ok(bytes_read)) => {
                    buf.advance(bytes_read);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(err)) => {
                    return Poll::Ready(Err(err));
                }
                Err(_would_block) => continue,
            }
        }
    }
}

impl AsyncWrite for SerialPort {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = ready!(self.inner.poll_write_ready_mut(cx))?;

            match guard.try_io(|inner| inner.get_mut().write(buf)) {
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => continue,
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.get_mut().flush()?;
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }
}

/// Options which can be used to configure how a serial port is opened.
///
/// The default options are 115200bps 8N1 with no flow control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SerialPortOptions {
    baud_rate: u32,
    data_bits: DataBits,
    stop_bits: StopBits,
    parity: Parity,
}

impl Default for SerialPortOptions {
    fn default() -> Self {
        Self {
            baud_rate: 115200,
            data_bits: DataBits::Eight,
            stop_bits: StopBits::One,
            parity: Parity::None,
        }
    }
}

impl SerialPortOptions {
    /// Creates a blank set of options with their default value.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the baud rate to be used for communication.
    pub fn baud_rate(&mut self, baud_rate: u32) -> &mut Self {
        self.baud_rate = baud_rate;
        self
    }

    /// Sets the number of data bits to be used for communication.
    pub fn data_bits(&mut self, data_bits: DataBits) -> &mut Self {
        self.data_bits = data_bits;
        self
    }

    /// Sets the number of stop bits to be used for communication.
    pub fn stop_bits(&mut self, stop_bits: StopBits) -> &mut Self {
        self.stop_bits = stop_bits;
        self
    }

    /// Sets the parity check to be performed for communication.
    pub fn parity(&mut self, parity: Parity) -> &mut Self {
        self.parity = parity;
        self
    }

    /// Opens a serial port device with the options specified by `self`.
    pub fn open<P: AsRef<Path>>(self, path: P) -> io::Result<SerialPort> {
        SerialPort::open_with_options(path, self)
    }
}

/// Number of data bits in a byte.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataBits {
    Five,
    Six,
    Seven,
    Eight,
}

/// Number of stop bits after a byte.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopBits {
    One,
    Two,
}

/// Parity check to be performed on the byte.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parity {
    None,
    Even,
    Odd,
}

/// Flow control, either hardware, software or none.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowControl {
    None,
    Software,
    Hardware,
}

#[derive(Debug)]
struct TtyDevice {
    fd: OwnedFd,
}

impl TtyDevice {
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let fd = unsafe {
            OwnedFd::from_raw_fd(fcntl::open(
                path.as_ref(),
                OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_NONBLOCK,
                Mode::empty(),
            )?)
        };

        let mut termios = Termios::from_fd(&fd)?;

        // Set control flags required for a TTY device
        termios.as_mut().control_flags |= ControlFlags::CREAD | ControlFlags::CLOCAL;

        // Configure port in raw mode: we don't want any processing going on under the hood
        termios.make_raw();

        // Apply settings
        termios.apply(&fd)?;

        // Clear O_NONBLOCK flag.
        let flags = OFlag::from_bits_retain(fcntl::fcntl(fd.as_raw_fd(), FcntlArg::F_GETFL)?);
        fcntl::fcntl(fd.as_raw_fd(), FcntlArg::F_SETFL(flags - OFlag::O_NONBLOCK))?;

        Ok(Self { fd })
    }
}

impl AsRawFd for TtyDevice {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl io::Read for TtyDevice {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        unistd::read(self.as_raw_fd(), buf).map_err(io::Error::from)
    }
}

impl io::Write for TtyDevice {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        unistd::write(&self.fd, buf).map_err(io::Error::from)
    }

    fn flush(&mut self) -> io::Result<()> {
        sys::termios::tcdrain(&self.fd).map_err(io::Error::from)
    }
}
