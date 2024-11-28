//! Types for working with an interactive terminal.

use std::{
    io::{self, Write},
    mem::MaybeUninit,
    os::fd::{AsFd, AsRawFd},
    pin::Pin,
    task::{Context, Poll},
};

use nix::{ioctl_read_bad, sys::termios::SpecialCharacterIndices, unistd};
use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};

use crate::{cli::ESCAPE_BYTE, termios::Termios};

/// Abstraction over the PTY to which the stdin and stdout of the current process are connected.
pub struct Terminal<I: AsFd, O: AsFd> {
    input: I,
    output: O,
    saved_state: Termios,
}

impl<I, O> Terminal<I, O>
where
    I: AsFd,
    O: AsFd,
{
    /// Wraps the specified input and output streams in a `Terminal`.
    ///
    /// # Errors
    ///
    /// This function will return an error if `input` is not a PTY.
    pub fn new(input: I, output: O) -> io::Result<Self> {
        let fd = input.as_fd();

        // For the time being, do not try working with anything other than PTYs.
        if !unistd::isatty(fd.as_raw_fd())? {
            return Err(io::Error::new(io::ErrorKind::Other, "not a TTY"));
        }

        // Push the current state in order to restore it on drop
        let saved_state = Termios::from_fd(fd)?;

        Ok(Self {
            input,
            output,
            saved_state,
        })
    }

    /// Puts the terminal in raw mode.
    pub fn make_raw(&mut self) -> io::Result<()> {
        let fd = self.input.as_fd();

        let mut termios = Termios::from_fd(fd)?;

        // Put terminal in raw mode and block until a single character is detected
        termios.make_raw();
        termios.as_mut().control_chars[SpecialCharacterIndices::VMIN as usize] = 1;

        // By using our escape byte as additional EOL character, we can break out of input
        // mode even when in canonical mode
        termios.as_mut().control_chars[SpecialCharacterIndices::VEOL2 as usize] = ESCAPE_BYTE;

        termios.apply(fd)
    }

    /// Restores the terminal state as it was when `self` was first created.
    pub fn restore(&mut self) -> io::Result<()> {
        self.saved_state.apply(self.input.as_fd())
    }

    /// Retrieves the current terminal size.
    pub fn get_size(&self) -> io::Result<(u32, u32)> {
        ioctl_read_bad!(tcgwinsz, nix::libc::TIOCGWINSZ, nix::libc::winsize);

        let winsize = unsafe {
            let mut winsize = MaybeUninit::uninit();
            tcgwinsz(self.output.as_fd().as_raw_fd(), winsize.as_mut_ptr())?;
            winsize.assume_init()
        };

        Ok((winsize.ws_col.into(), winsize.ws_row.into()))
    }

    /// Enables or disables the terminal echo.
    pub fn set_local_echo(&mut self, on: bool) -> io::Result<()> {
        let fd = self.input.as_fd();
        let mut termios = Termios::from_fd(fd)?;
        termios.set_local_echo(on);
        termios.apply(fd)
    }

    /// Puts the terminal in canonical mode or raw mode.
    pub fn set_canonical_mode(&mut self, on: bool) -> io::Result<()> {
        let fd = self.input.as_fd();
        let mut termios = Termios::from_fd(fd)?;
        termios.set_canonical_mode(on);
        termios.apply(fd)
    }

    fn pin_input(self: Pin<&mut Self>) -> &mut I {
        // SAFETY: this is okay because `input` is never considered pinned.
        unsafe { &mut self.get_unchecked_mut().input }
    }
}

impl<I, O> Terminal<I, O>
where
    I: AsyncRead + AsFd + Unpin,
    O: AsFd + Unpin,
{
    /// Reads a string from the terminal's input without echo.
    pub async fn input_password(&mut self) -> io::Result<String> {
        let saved_state = {
            let fd = self.input.as_fd();

            let mut termios = Termios::from_fd(fd)?;
            let saved_state = termios.clone();

            // Do not echo the characters written
            termios.set_local_echo(false);
            termios.apply(fd)?;

            saved_state
        };

        // Read password from the terminal and convert it to UTF-8
        let mut password = [0; 256];
        let n = self.read(&mut password).await?;

        // The password read from the terminal will contain a trailing newline, so remove it
        let password = std::str::from_utf8(&password[..n])
            .map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "password is not valid UTF-8")
            })?
            .trim_end_matches(['\n', '\r']);

        // Restore the terminal's state
        saved_state.apply(self.input.as_fd())?;

        Ok(password.to_string())
    }
}

impl<I: AsFd, O: AsFd> Drop for Terminal<I, O> {
    fn drop(&mut self) {
        // Restore the origina terminal state
        let _ = self.restore();
    }
}

impl<I, O> AsyncRead for Terminal<I, O>
where
    I: AsyncRead + AsFd + Unpin,
    O: AsFd,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(self.pin_input()).poll_read(cx, buf)
    }
}

impl<I, O> Write for Terminal<I, O>
where
    I: AsFd,
    O: Write + AsFd,
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.output.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}
