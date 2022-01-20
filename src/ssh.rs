use std::{
    io::{self, Read, Write},
    net::TcpStream,
    os::unix::prelude::AsRawFd,
    pin::Pin,
    task::{Context, Poll},
};

use anyhow::{bail, Result};
use async_io::Async;
use futures::ready;
use ssh2::{PtyModeOpcode, PtyModes};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub struct Session {
    session: ssh2::Session,
    channel: Option<ssh2::Channel>,
    stream: Option<Async<TcpStream>>,
}

impl Session {
    pub async fn new(addr: &str) -> Result<Self> {
        let addr = match addr.split_once(':') {
            Some((addr, port)) => (addr.to_string(), port.parse::<u16>()?),
            None => (addr.to_string(), 22),
        };

        // Connect to the remote SSH server
        let tcp = TcpStream::connect(addr)?;
        let mut session = ssh2::Session::new()?;

        session.set_tcp_stream(tcp.as_raw_fd());
        session.handshake()?;

        Ok(Self {
            session,
            channel: None,
            stream: Some(Async::new(tcp)?),
        })
    }

    pub fn authenticate_with_agent(&mut self, username: &str) -> Result<()> {
        if self.channel.is_some() {
            bail!("already authenticated");
        }

        self.session.userauth_agent(username)?;
        self.channel = Some(self.session.channel_session()?);

        Ok(())
    }

    pub fn authenticate_with_password(&mut self, username: &str, password: &str) -> Result<()> {
        if self.channel.is_some() {
            bail!("already authenticated");
        }

        self.session.userauth_password(username, password)?;
        self.channel = Some(self.session.channel_session()?);

        Ok(())
    }

    pub fn request_pty(&mut self, size: (u32, u32)) -> Result<()> {
        let channel = match self.channel {
            Some(ref mut channel) => channel,
            None => bail!("authentication required"),
        };

        // Ensure that we get a feedback on the input
        let mut mode = PtyModes::new();
        mode.set_boolean(PtyModeOpcode::ECHO, true);

        // Allocate a terminal of the right kind
        channel.request_pty("xterm", Some(mode), Some((size.0, size.1, 0, 0)))?;

        Ok(())
    }

    pub fn shell(&mut self) -> Result<()> {
        let channel = match self.channel {
            Some(ref mut channel) => channel,
            None => bail!("authentication required"),
        };

        // Open a new shell
        channel.shell()?;

        // Session must be non-blocking to be made async
        self.session.set_blocking(false);

        Ok(())
    }
}

impl AsyncRead for Session {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let channel = self.channel.as_mut().unwrap();

            match channel.read(buf.initialize_unfilled()) {
                Ok(n) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    ready!(self.stream.as_mut().unwrap().poll_readable(cx))?;
                }
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
    }
}

impl AsyncWrite for Session {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        loop {
            let channel = self.channel.as_mut().unwrap();

            match channel.write(buf) {
                Ok(n) => return Poll::Ready(Ok(n)),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    ready!(self.stream.as_mut().unwrap().poll_writable(cx))?;
                }
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let channel = self.channel.as_mut().unwrap();
        Poll::Ready(channel.flush())
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }
}
