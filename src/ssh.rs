//! SSH connection and session handling.

use std::{
    io::{self, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    os::unix::prelude::AsRawFd,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use anyhow::{anyhow, Result};
use async_io::Async;
use async_recursion::async_recursion;
use futures::ready;
use ssh2::{ErrorCode, FileStat, PtyModeOpcode, PtyModes};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

const DEFAULT_BUF_SIZE: usize = 1024 * 1024;

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

    /// Performs an SSH none authentication with the remote host as `username`.
    pub async fn authenticate_with_none(&self, username: &str) -> Result<Channel> {
        let _ = self
            .inner
            .run(|session| session.auth_methods(username).map(|_| ()))
            .await;

        if self
            .inner
            .run(|session| Ok(session.authenticated()))
            .await?
        {
            self.create_channel().await
        } else {
            Err(anyhow!("unsupported authentication method"))
        }
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

    /// Opens a SFTP channel on this session.
    pub async fn sftp(&self) -> Result<Sftp> {
        Ok(Sftp {
            inner: self.inner.run(|session| session.sftp()).await?,
            session: self.inner.clone(),
            netbuf: vec![0; DEFAULT_BUF_SIZE],
        })
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

/// Handle to a remote filesystem over SFTP.
pub struct Sftp {
    inner: ssh2::Sftp,
    session: AsyncSession,
    netbuf: Vec<u8>,
}

impl Sftp {
    /// Creates a file on the remote host from a stream of bytes using the SFTP protocol.
    pub async fn upload<P, R>(&mut self, remote_path: P, source: &mut R) -> Result<()>
    where
        R: AsyncRead + Unpin,
        P: AsRef<Path>,
    {
        let mut remote_file = self.create(remote_path).await?;

        let mut buf = ReadBuf::new(&mut self.netbuf);

        loop {
            // The goal here is to try to maximize the amount of bytes read from the local
            // filesystem before sending them over the SSH connection.
            // Since the bottleneck for this operation is either the link speed or the RTT between
            // an SFTP chunk and its ACK, we want to send as much data as possible in a transfer.
            while buf.filled().len() < 1024 * 1024 {
                match source.read(buf.initialize_unfilled()).await {
                    Ok(0) => break,
                    Ok(n) => buf.advance(n),
                    Err(e) => return Err(e.into()),
                }
            }

            // EOF from local file
            if buf.filled().is_empty() {
                return Ok(());
            }

            remote_file.write_all(buf.filled()).await?;

            buf.clear();
        }
    }

    /// Downloads the contents of a file to the remote host using the SFTP protocol.
    #[async_recursion(?Send)]
    pub async fn download<P, W>(&mut self, remote_path: P, dest: &mut W) -> Result<()>
    where
        W: AsyncWrite + Unpin,
        P: AsRef<Path>,
    {
        let mut remote_file = self.open(remote_path).await?;

        loop {
            // Same as for the upload, tokio::io::copy is abysmally slow due to very small default
            // buffer size. Doing the read-write loop by hand is one order of magnitude faster.
            match remote_file.read(&mut self.netbuf).await {
                Ok(0) => return Ok(()),
                Ok(n) => dest.write_all(&self.netbuf[..n]).await?,
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Opens a file in read-only mode.
    pub async fn open<P: AsRef<Path>>(&self, path: P) -> Result<File> {
        Ok(File {
            inner: self.session.run(|_| self.inner.open(path.as_ref())).await?,
            session: self.session.clone(),
        })
    }

    /// Creates a file in write-only mode with truncation.
    pub async fn create<P: AsRef<Path>>(&self, path: P) -> Result<File> {
        Ok(File {
            inner: self
                .session
                .run(|_| self.inner.create(path.as_ref()))
                .await?,
            session: self.session.clone(),
        })
    }

    /// Creates a directory on the remote file system.
    pub async fn mkdir<P: AsRef<Path>>(&self, path: P, mode: i32) -> Result<()> {
        self.session
            .run(|_| self.inner.mkdir(path.as_ref(), mode))
            .await
    }

    /// Gets the metadata for a file, performed by stat(2).
    pub async fn stat<P: AsRef<Path>>(&self, path: P) -> Result<ssh2::FileStat> {
        self.session.run(|_| self.inner.stat(path.as_ref())).await
    }

    /// Opens a file in read-only mode.
    pub async fn read_dir<P: AsRef<Path>>(&self, path: P) -> Result<Vec<PathBuf>> {
        let mut dir = self
            .session
            .run(|_| self.inner.opendir(path.as_ref()))
            .await?;

        let mut ret = Vec::new();

        while let Ok((path, _)) = self.session.run(|_| dir.readdir()).await {
            if path != Path::new(".") && path != Path::new("..") {
                ret.push(path);
            }
        }

        Ok(ret)
    }
}

/// Handle to a remote file obtained via SFTP.
pub struct File {
    inner: ssh2::File,
    session: AsyncSession,
}

impl File {
    /// Retrieves the metadata associated to this file.
    pub async fn stat(&mut self) -> Result<FileStat> {
        self.session.run(|_| self.inner.stat()).await
    }
}

impl AsyncRead for File {
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

impl AsyncWrite for File {
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
                    // The hard-coded number is ugly as hell, but ssh2 does not re-export the
                    // error codes from the C library, so... *shrug*
                    Err(e) if e.code() == ErrorCode::Session(-37) => {
                        match self.inner.block_directions() {
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
                            ssh2::BlockDirections::None => {
                                panic!("EAGAIN but should not block")
                            }
                        }
                    }
                    Err(e) => break Err(e.into()),
                }
            }
        }
    }
}
