//! SSH connection and session handling.

use std::{
    io::{self, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    os::unix::prelude::AsRawFd,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use anyhow::Result;
use async_io::Async;
use futures::ready;
use ssh2::{PtyModeOpcode, PtyModes};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};

/// A reference to an established SSH session with a remote host.
pub struct Session {
    inner: AsyncSession,
}

impl Session {
    /// Attempts to establish an SSH session with the host at `addr`.
    pub async fn new<A: ToSocketAddrs>(addr: A) -> Result<Self> {
        // Connect to the remote SSH server
        let tcp = TcpStream::connect(addr)?;
        let mut session = ssh2::Session::new()?;

        session.set_tcp_stream(tcp.as_raw_fd());
        session.handshake()?;

        Ok(Self {
            inner: AsyncSession::new(session, Arc::new(Async::new(tcp)?)),
        })
    }

    /// Performs an SSH agent authentication with the remote host as `username`.
    pub async fn authenticate_with_agent(&self, username: &str) -> Result<Channel> {
        self.inner
            .run(|session| session.userauth_agent(username))
            .await?;

        self.create_channel().await
    }

    /// Performs a password authentication with the remote host as `username`.
    pub async fn authenticate_with_password(
        &self,
        username: &str,
        password: &str,
    ) -> Result<Channel> {
        self.inner
            .run(|session| session.userauth_password(username, password))
            .await?;

        self.create_channel().await
    }

    /// Create a new channel and set the session as non-blocking.
    async fn create_channel(&self) -> Result<Channel> {
        let channel = self.inner.run(|session| session.channel_session()).await?;

        // After authentication, put session in non-blocking mode
        self.inner.set_blocking(false);

        Ok(Channel {
            inner: channel,
            session: self.inner.clone(),
        })
    }
}

/// Portion of an SSH connection on which data can be read and written.
pub struct Channel {
    inner: ssh2::Channel,
    session: AsyncSession,
}

impl Channel {
    /// Requests a PTY on an established channel.
    pub async fn request_pty(&mut self, size: (u32, u32)) -> Result<()> {
        // Ensure that we get a feedback on the input
        let mut mode = PtyModes::new();
        mode.set_boolean(PtyModeOpcode::ECHO, true);

        // Allocate a terminal of the right kind
        self.session
            .run(|_| {
                self.inner
                    .request_pty("xterm", Some(mode.clone()), Some((size.0, size.1, 0, 0)))
            })
            .await?;

        Ok(())
    }

    /// Start a shell on the remote host.
    pub async fn shell(&mut self) -> Result<()> {
        self.session.run(|_| self.inner.shell()).await
    }

    /// Run a command on the remote host.
    pub async fn run<A>(&mut self, command: &str, args: A) -> Result<String>
    where
        A: IntoIterator,
        A::Item: AsRef<str>,
    {
        let mut command = command.to_string();

        // Append arguments surrounded in quotes to prevent word splitting
        for arg in args {
            command.push_str(&format!(" \"{}\"", arg.as_ref()));
        }

        self.session.run(|_| self.inner.exec(&command)).await?;

        let mut response = String::new();
        self.read_to_string(&mut response).await?;

        Ok(response)
    }
}

impl AsyncRead for Channel {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            match self.inner.read(buf.initialize_unfilled()) {
                Ok(n) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    ready!(self.session.poll_readable(cx))?;
                }
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
    }
}

impl AsyncWrite for Channel {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        loop {
            match self.inner.write(buf) {
                Ok(n) => return Poll::Ready(Ok(n)),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    ready!(self.session.poll_writable(cx))?;
                }
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(self.inner.flush())
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        // TODO: implement graceful shutdown
        Poll::Ready(Ok(()))
    }
}

/// Wrapper around a libssh2 `Session` providing an async interface to non-blocking operations.
#[derive(Clone)]
struct AsyncSession {
    inner: ssh2::Session,
    stream: Arc<Async<TcpStream>>,
}

impl AsyncSession {
    /// Wraps a session in an async interface.
    pub fn new(inner: ssh2::Session, stream: Arc<Async<TcpStream>>) -> Self {
        Self { inner, stream }
    }

    /// Sets or clears blocking mode for this session.
    pub fn set_blocking(&self, blocking: bool) {
        self.inner.set_blocking(blocking);
    }

    /// Polls the underlying session for readability.
    pub fn poll_readable(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.stream.poll_readable(cx)
    }

    /// Polls the underlying session for writeability.
    pub fn poll_writable(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.stream.poll_writable(cx)
    }

    /// Runs a libssh2 command on a non-blocking session by leveraging the blocking status reported
    /// by the library and the pollable TCP stream.
    pub async fn run<F, T>(&self, mut f: F) -> Result<T>
    where
        F: FnMut(&ssh2::Session) -> std::result::Result<T, ssh2::Error>,
    {
        if self.inner.is_blocking() {
            // In blocking mode call f() directly once
            f(&self.inner).map_err(anyhow::Error::from)
        } else {
            loop {
                match f(&self.inner) {
                    Ok(res) => break Ok(res),
                    Err(e) => match self.inner.block_directions() {
                        ssh2::BlockDirections::None => {
                            break Err(e.into());
                        }
                        ssh2::BlockDirections::Inbound => {
                            self.stream.readable().await?;
                        }
                        ssh2::BlockDirections::Outbound => {
                            self.stream.writable().await?;
                        }
                        ssh2::BlockDirections::Both => {
                            self.stream.readable().await?;
                            self.stream.writable().await?;
                        }
                    },
                }
            }
        }
    }
}
