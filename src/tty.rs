use std::{
    io::{self, Write},
    os::unix::prelude::{AsRawFd, RawFd},
    pin::Pin,
    task::{Context, Poll},
};

use futures::ready;
use nix::{
    sys::{
        self,
        termios::{FlushArg, SetArg, SpecialCharacterIndices, Termios},
    },
    unistd,
};
use pin_project::{pin_project, pinned_drop};
use tokio::io::{AsyncRead, ReadBuf};

#[pin_project(PinnedDrop)]
pub struct Terminal<I, O>
where
    I: AsyncRead + AsRawFd,
{
    #[pin]
    input: I,
    output: O,
    saved_state: Option<Termios>,
}

impl<I, O> Terminal<I, O>
where
    I: AsyncRead + AsRawFd,
{
    pub fn new(input: I, output: O) -> io::Result<Self> {
        Ok(Self {
            input,
            output,
            saved_state: None,
        })
    }

    pub fn make_raw(&mut self) -> io::Result<()> {
        let fd = self.input.as_raw_fd();

        // Do nothing if the terminal is already in raw mode
        if self.saved_state.is_some() {
            return Ok(());
        }

        // The input must be a tty in order to be put in raw mode
        if !unistd::isatty(fd)? {
            return Err(io::Error::new(io::ErrorKind::Other, "not a TTY"));
        }

        let mut termios = sys::termios::tcgetattr(fd)?;

        // Push the current state in order to restore it on drop
        self.saved_state = Some(termios.clone());

        sys::termios::cfmakeraw(&mut termios);

        termios.control_chars[SpecialCharacterIndices::VMIN as usize] = 1;

        sys::termios::tcflush(fd, FlushArg::TCIFLUSH)?;
        sys::termios::tcsetattr(fd, SetArg::TCSANOW, &termios)?;

        Ok(())
    }

    pub fn restore(&mut self) -> io::Result<()> {
        if let Some(termios) = self.saved_state.take() {
            Self::apply_termios_to_fd(self.input.as_raw_fd(), &termios)?;
        }

        Ok(())
    }

    fn apply_termios_to_fd(fd: RawFd, termios: &Termios) -> io::Result<()> {
        sys::termios::tcflush(fd, FlushArg::TCIFLUSH)?;
        sys::termios::tcsetattr(fd, SetArg::TCSANOW, termios)?;
        Ok(())
    }
}

#[pinned_drop]
impl<I, O> PinnedDrop for Terminal<I, O>
where
    I: AsyncRead + AsRawFd,
{
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();

        // Restore the previous terminal state in case the terminal was put in raw mode
        if let Some(termios) = this.saved_state {
            Self::apply_termios_to_fd(this.input.as_raw_fd(), termios).ok();
        }
    }
}

impl<I, O> AsyncRead for Terminal<I, O>
where
    I: AsyncRead + AsRawFd,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.as_mut().project();
        let res = ready!(this.input.poll_read(cx, buf));
        Poll::Ready(res)
    }
}

impl<I, O> Write for Terminal<I, O>
where
    I: AsyncRead + AsRawFd,
    O: Write,
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.output.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}
