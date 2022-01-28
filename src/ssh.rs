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

use anyhow::{anyhow, bail, Result};
use async_io::Async;
use async_recursion::async_recursion;
use futures::ready;
use nix::NixPath;
use ssh2::{ErrorCode, PtyModeOpcode, PtyModes};
use tokio::{
    fs,
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
};

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
    /// Uploads a local file to the remote host using the SFTP protocol.
    #[async_recursion(?Send)]
    pub async fn upload<L, R>(&mut self, local_path: L, remote_path: R) -> Result<()>
    where
        L: AsRef<Path>,
        R: AsRef<Path>,
    {
        let mut local_file = fs::File::open(&local_path).await?;

        // Use dedicated method to upload whole directories
        if local_file.metadata().await?.is_dir() {
            return self.upload_dir(local_path, remote_path).await;
        }

        let file_name = local_path
            .as_ref()
            .file_name()
            .expect("could not retrieve file name");

        // Prepare the remote path to be handled correctly by the server
        let remote_path = Self::sanitize_remove_path(remote_path);

        // If the remote path exists and is a directory, create a file with the same name in it.
        // If not, use the remote path as is.
        let remote_path = match self.stat(&remote_path).await {
            Ok(stat) if stat.is_dir() => remote_path.join(file_name),
            Ok(_) | Err(_) => remote_path,
        };

        let mut remote_file = self.create(remote_path).await?;

        let mut buf = ReadBuf::new(&mut self.netbuf);

        loop {
            // The goal here is to try to maximize the amount of bytes read from the local
            // filesystem before sending them over the SSH connection.
            // Since the bottleneck for this operation is either the link speed or the RTT between
            // an SFTP chunk and its ACK, we want to send as much data as possible in a transfer.
            while buf.filled().len() < 1024 * 1024 {
                match local_file.read(buf.initialize_unfilled()).await {
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

    /// Special handling for directory uploads.
    #[async_recursion(?Send)]
    async fn upload_dir<L, R>(&mut self, local_path: L, remote_path: R) -> Result<()>
    where
        L: AsRef<Path>,
        R: AsRef<Path>,
    {
        let local_path = local_path.as_ref();

        let dir_name = local_path
            .file_name()
            .expect("could not retrieve file name");

        // Prepare the remote path to be handled correctly by the server
        let remote_path = Self::sanitize_remove_path(remote_path);

        // If the remote path exists and is a directory, create a file with the same name in it.
        // If not, use the remote path as is.
        let remote_path = match self.stat(&remote_path).await {
            Ok(stat) if stat.is_dir() => remote_path.join(dir_name),
            Ok(_) | Err(_) => remote_path,
        };

        // Create directory on the server, if it doesn't exist
        match self.stat(&remote_path).await {
            Ok(stat) if !stat.is_dir() => bail!("file exists and is not a directory"),
            Err(_) => self.mkdir(&remote_path, 0o775).await?,
            Ok(_) => (),
        };

        let mut local_dir = fs::read_dir(&local_path).await?;

        while let Some(local_file) = local_dir.next_entry().await? {
            let file_name = local_file
                .path()
                .file_name()
                .expect("could not retrieve file name")
                .to_owned();

            self.upload(local_path.join(&file_name), remote_path.join(&file_name))
                .await?;
        }

        Ok(())
    }

    /// Uploads a local file to the remote host using the SFTP protocol.
    #[async_recursion(?Send)]
    pub async fn download<L, R>(&mut self, remote_path: R, local_path: L) -> Result<()>
    where
        L: AsRef<Path>,
        R: AsRef<Path>,
    {
        // Prepare the remote path to be handled correctly by the server
        let remote_path = Self::sanitize_remove_path(remote_path);

        // Use dedicated method to download whole directories
        if self.stat(&remote_path).await?.is_dir() {
            return self.download_dir(remote_path, local_path).await;
        }

        let file_name = remote_path
            .file_name()
            .ok_or(anyhow!("invalid file name"))?;

        // If the local path exists and is a directory, create a file with the same name in it.
        // If not, use the local path as is.
        let local_path = match fs::metadata(&local_path).await {
            Ok(stat) if stat.is_dir() => local_path.as_ref().join(file_name),
            Ok(_) | Err(_) => local_path.as_ref().into(),
        };

        let mut local_file = fs::File::create(&local_path).await?;
        let mut remote_file = self.open(remote_path).await?;

        loop {
            // Same as for the upload, tokio::io::copy is abysmally slow due to very small default
            // buffer size. Doing the read-write loop by hand is one order of magnitude faster.
            match remote_file.read(&mut self.netbuf).await {
                Ok(0) => return Ok(()),
                Ok(n) => local_file.write_all(&self.netbuf[..n]).await?,
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Special handling for directory uploads.
    #[async_recursion(?Send)]
    async fn download_dir<L, R>(&mut self, remote_path: R, local_path: L) -> Result<()>
    where
        L: AsRef<Path>,
        R: AsRef<Path>,
    {
        let local_path = local_path.as_ref();

        // Prepare the remote path to be handled correctly by the server
        let remote_path = Self::sanitize_remove_path(remote_path);

        // Always create a new directory, do not copy files in the current directory
        let local_path = match fs::metadata(local_path).await {
            Ok(stat) if !stat.is_dir() => bail!("file exists and is not a directory"),
            Err(_) => local_path.to_path_buf(),
            Ok(_) => local_path.join(
                remote_path
                    .file_name()
                    .expect("could not retrieve file name"),
            ),
        };

        // Create local directory if it doesn't exist
        if fs::metadata(&local_path).await.is_err() {
            fs::create_dir(&local_path).await?;
        }

        for remote_file in self.read_dir(&remote_path).await? {
            let file_name = remote_file
                .file_name()
                .expect("could not retrieve file name");

            self.download(remote_path.join(&remote_file), local_path.join(&file_name))
                .await?;
        }

        Ok(())
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

    /// Canonicalizes a path by removing unwanted prefixes.
    fn sanitize_remove_path<P: AsRef<Path>>(path: P) -> PathBuf {
        let mut path = path.as_ref().to_path_buf();

        // Map empty path or home directory to current directory
        if path.is_empty() || path == Path::new("~") || path == Path::new(".") {
            return PathBuf::from(".");
        }

        // Convert a tilde prefix into current directory
        if path.starts_with("~") {
            path = Path::new(".").join(path.strip_prefix("~").unwrap());
        }

        path
    }
}

/// Handle to a remote file obtained via SFTP.
pub struct File {
    inner: ssh2::File,
    session: AsyncSession,
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
