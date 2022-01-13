use std::{
    io::{self, Read, Write},
    os::unix::prelude::{AsRawFd, RawFd},
    path::Path,
    pin::Pin,
    task::{Context, Poll},
};

use futures::ready;
use nix::{
    fcntl::{self, FcntlArg, OFlag},
    sys::termios::{self, ControlFlags, FlushArg, InputFlags, SetArg},
    sys::{stat::Mode, termios::BaudRate},
    unistd,
};
use tokio::io::{unix::AsyncFd, AsyncRead, AsyncWrite, ReadBuf};

#[derive(Debug)]

struct TtyDevice {
    fd: RawFd,
}

impl TtyDevice {
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let fd = fcntl::open(
            path.as_ref(),
            OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_NONBLOCK,
            Mode::empty(),
        )?;

        let mut termios = termios::tcgetattr(fd)?;

        termios.control_flags |= ControlFlags::CREAD | ControlFlags::CLOCAL;

        termios::cfmakeraw(&mut termios);

        // Data bits: 8
        termios.control_flags &= !ControlFlags::CSIZE;
        termios.control_flags |= ControlFlags::CS8;

        // Parity: none
        termios.control_flags &= !(ControlFlags::PARENB | ControlFlags::PARODD);
        termios.input_flags &= !InputFlags::IGNPAR;

        // Stop bits: 1
        termios.control_flags &= !ControlFlags::CSTOPB;

        // Flow control: none
        termios.control_flags &= !ControlFlags::CRTSCTS;
        termios.input_flags &= !(InputFlags::IXON | InputFlags::IXOFF);

        // Baudrate: 115200
        termios::cfsetispeed(&mut termios, BaudRate::B115200)?;
        termios::cfsetospeed(&mut termios, BaudRate::B115200)?;

        // Apply settings
        termios::tcflush(fd, FlushArg::TCIFLUSH)?;
        termios::tcsetattr(fd, SetArg::TCSANOW, &termios)?;

        // Clear O_NONBLOCK flag.
        // SAFETY: the bitfield retrieved with F_GETFL is assumed to be always valid.
        let flags = unsafe { OFlag::from_bits_unchecked(fcntl::fcntl(fd, FcntlArg::F_GETFL)?) };
        fcntl::fcntl(fd, FcntlArg::F_SETFL(flags - OFlag::O_NONBLOCK))?;

        Ok(Self { fd })
    }
}

impl Drop for TtyDevice {
    fn drop(&mut self) {
        let _ = unistd::close(self.fd);
    }
}

impl AsRawFd for TtyDevice {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl io::Read for TtyDevice {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        unistd::read(self.fd, buf).map_err(io::Error::from)
    }
}

impl io::Write for TtyDevice {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        unistd::write(self.fd, buf).map_err(io::Error::from)
    }

    fn flush(&mut self) -> io::Result<()> {
        termios::tcdrain(self.fd).map_err(io::Error::from)
    }
}

pub struct SerialPort {
    inner: AsyncFd<TtyDevice>,
}

impl SerialPort {
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let tty = TtyDevice::open(path)?;

        //Make the file descriptor non-blocking
        let fd = tty.as_raw_fd();
        let flags = unsafe { OFlag::from_bits_unchecked(fcntl::fcntl(fd, FcntlArg::F_GETFL)?) };
        fcntl::fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;

        Ok(Self {
            inner: AsyncFd::new(tty)?,
        })
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

pub struct SerialPortOptions {
    _baud_rate: u32,
    _data_bits: DataBits,
    _stop_bits: StopBits,
    _parity: Parity,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DataBits {
    Five,
    Six,
    Seven,
    Eight,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StopBits {
    One,
    Two,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Parity {
    None,
    Even,
    Odd,
}
