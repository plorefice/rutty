use std::{
    io::{self, Write},
    os::unix::prelude::MetadataExt,
    path::{Path, PathBuf},
    pin::Pin,
    task::{self, Poll},
};

use anyhow::{bail, Context, Result};
use async_recursion::async_recursion;
use futures::TryFutureExt;
use indicatif::{ProgressBar, ProgressStyle};
use nix::unistd::{Uid, User};
use tokio::{
    fs,
    io::{AsyncRead, AsyncWrite, ReadBuf},
};
use zeroize::Zeroize;

use crate::{
    cli::{
        opts::{Connection, Opts, TristateOpt},
        Terminal,
    },
    ssh::{Channel, Session, Sftp},
};

pub async fn do_ssh_connection(cli: &mut Opts, term: &mut Terminal) -> Result<Channel> {
    let ssh = match cli.connection {
        Connection::Ssh(ref ssh) => ssh,
        _ => panic!("unexpected connection type"),
    };

    let (_, mut channel) = establish_ssh_session(term, &ssh.destination, ssh.opts.port).await?;

    if let Some((command, args)) = ssh.command.split_first() {
        // Run the command and print the output.
        // Don't bother exiting early here, the SSH channel will receive a EOF anyway.
        let output = channel.run(command, args).await?;
        term.write_all(output.as_bytes())?;
    } else {
        // After authentication, create the virtual terminal and the shell
        let size = term.get_size()?;
        channel.request_pty(size).await?;
        channel.shell().await?;

        // SSH shells require a raw TTY
        cli.canonical.prefer(TristateOpt::Off);
        cli.local_echo.prefer(TristateOpt::Off);
    }

    Ok(channel)
}

pub async fn do_scp_transfer(cli: &mut Opts, term: &mut Terminal) -> Result<()> {
    let scp = match cli.connection {
        Connection::Scp(ref scp) => scp,
        _ => panic!("unexpected connection type"),
    };

    let (mut target_sftp, target_path) = match scp.target.split_once(':') {
        Some((remote, path)) => {
            let (session, _) = establish_ssh_session(term, remote, scp.opts.port).await?;
            let sftp = session.sftp().await?;
            (Some(sftp), path)
        }
        None => (None, scp.target.as_str()),
    };

    for source in &scp.sources {
        let (mut source_sftp, source_path) = match source.split_once(':') {
            Some((remote, path)) => {
                let (session, _) = establish_ssh_session(term, remote, scp.opts.port).await?;
                let sftp = session.sftp().await?;
                (Some(sftp), path)
            }
            None => (None, source.as_str()),
        };

        match (&mut source_sftp, &mut target_sftp) {
            (None, Some(ref mut sftp)) => {
                do_scp_upload(sftp, source_path, target_path).await?;
            }
            (Some(ref mut sftp), None) => {
                do_scp_download(sftp, source_path, target_path).await?;
            }
            (None, None) => {
                // A local copy is performed if neither source nor target are remote hosts
                tokio::fs::copy(source_path, target_path).await?;
            }
            (Some(_), Some(_)) => bail!("Cannot transfer files between two remote hosts"),
        }
    }

    Ok(())
}

#[async_recursion(?Send)]
async fn do_scp_upload<S, T>(sftp: &mut Sftp, source: S, target: T) -> Result<()>
where
    S: AsRef<Path>,
    T: AsRef<Path>,
{
    let local_file = fs::File::open(&source).await?;
    let meta = local_file.metadata().await?;

    // Use dedicated method to upload whole directories
    if meta.is_dir() {
        return do_scp_upload_dir(sftp, source, target).await;
    }

    let file_name = source.as_ref().file_name().expect("invalid file name");

    // Prepare the remote path to be handled correctly by the server
    let remote_path = sanitize_sftp_path(target);

    // If the remote path exists and is a directory, create a file with the same name in it.
    // If not, use the remote path as is.
    let remote_path = match sftp.stat(&remote_path).await {
        Ok(stat) if stat.is_dir() => remote_path.join(file_name),
        Ok(_) | Err(_) => remote_path,
    };

    // Do the transfer while updating the progress bar
    sftp.upload(
        remote_path,
        &mut ProgressReader::new(file_name.to_str().unwrap(), local_file, meta.size()),
    )
    .await
}

#[async_recursion(?Send)]
async fn do_scp_upload_dir<S, T>(sftp: &mut Sftp, source: S, target: T) -> Result<()>
where
    S: AsRef<Path>,
    T: AsRef<Path>,
{
    let local_path = source.as_ref();

    let dir_name = local_path.file_name().expect("invalid directory name");

    // Prepare the remote path to be handled correctly by the server
    let remote_path = sanitize_sftp_path(target);

    // If the remote path exists and is a directory, create a file with the same name in it.
    // If not, use the remote path as is.
    let remote_path = match sftp.stat(&remote_path).await {
        Ok(stat) if stat.is_dir() => remote_path.join(dir_name),
        Ok(_) | Err(_) => remote_path,
    };

    // Create directory on the server, if it doesn't exist
    match sftp.stat(&remote_path).await {
        Ok(stat) if !stat.is_dir() => bail!("file exists and is not a directory"),
        Err(_) => sftp.mkdir(&remote_path, 0o775).await?,
        Ok(_) => (),
    };

    let mut local_dir = fs::read_dir(&local_path).await?;

    while let Some(local_file) = local_dir.next_entry().await? {
        let file_name = local_file
            .path()
            .file_name()
            .expect("could not retrieve file name")
            .to_owned();

        do_scp_upload(
            sftp,
            local_path.join(&file_name),
            remote_path.join(&file_name),
        )
        .await?;
    }

    Ok(())
}

#[async_recursion(?Send)]
async fn do_scp_download<S, T>(sftp: &mut Sftp, source: S, target: T) -> Result<()>
where
    S: AsRef<Path>,
    T: AsRef<Path>,
{
    let local_path = target.as_ref();

    // Prepare the remote path to be handled correctly by the server
    let remote_path = sanitize_sftp_path(source);

    let meta = sftp.stat(&remote_path).await?;

    // Use dedicated method to download whole directories
    if meta.is_dir() {
        return do_scp_download_dir(sftp, remote_path, local_path).await;
    }

    let file_name = remote_path
        .file_name()
        .and_then(|s| s.to_str())
        .expect("invalid file name")
        .to_owned();

    // If the local path exists and is a directory, create a file with the same name in it.
    // If not, use the local path as is.
    let local_path = match fs::metadata(&local_path).await {
        Ok(stat) if stat.is_dir() => local_path.join(&file_name),
        Ok(_) | Err(_) => local_path.into(),
    };

    let local_file = fs::File::create(&local_path).await?;

    // Do the transfer while updating the progress bar
    sftp.download(
        remote_path,
        &mut ProgressWriter::new(file_name, local_file, meta.size.unwrap()),
    )
    .await
}

#[async_recursion(?Send)]
async fn do_scp_download_dir<S, T>(sftp: &mut Sftp, source: S, target: T) -> Result<()>
where
    S: AsRef<Path>,
    T: AsRef<Path>,
{
    let local_path = target.as_ref();

    // Prepare the remote path to be handled correctly by the server
    let remote_path = sanitize_sftp_path(source);

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

    for remote_file in sftp.read_dir(&remote_path).await? {
        let file_name = remote_file
            .file_name()
            .expect("could not retrieve file name");

        do_scp_download_dir(
            sftp,
            remote_path.join(&remote_file),
            local_path.join(file_name),
        )
        .await?;
    }

    Ok(())
}

async fn establish_ssh_session(
    term: &mut Terminal,
    remote: &str,
    port: u16,
) -> Result<(Session, Channel)> {
    let (username, address) = match remote.split_once('@') {
        Some((username, address)) => (username.to_string(), address.to_string()),
        None => match User::from_uid(Uid::effective())? {
            Some(user) => (user.name, remote.to_string()),
            None => bail!("Could not retrieve username"),
        },
    };

    let session = Session::new((address.as_str(), port))
        .await
        .with_context(|| format!("Could not connect to {}", &address))?;

    // Authenticate with the server.
    // Try using a none authentication, followed by agent, and fallback on password authentication.
    let channel = match session
        .authenticate_with_none(&username)
        .or_else(|_| session.authenticate_with_agent(&username))
        .await
    {
        Ok(channel) => channel,
        Err(_) => {
            // Show a prompt to the user
            write!(term, "{}@{}'s password: ", &username, &address)?;
            term.flush()?;

            let mut password = term.input_password().await?;
            writeln!(term)?;

            let channel = session
                .authenticate_with_password(&username, &password)
                .await;

            // Securely clear password from memory
            password.zeroize();

            channel?
        }
    };

    Ok((session, channel))
}

/// Canonicalizes a path by removing unwanted prefixes.
fn sanitize_sftp_path<P: AsRef<Path>>(path: P) -> PathBuf {
    let mut path = path.as_ref().to_path_buf();

    // Map empty path or home directory to current directory
    if path == Path::new("") || path == Path::new("~") || path == Path::new(".") {
        return PathBuf::from(".");
    }

    // Convert a tilde prefix into current directory
    if path.starts_with("~") {
        path = Path::new(".").join(path.strip_prefix("~").unwrap());
    }

    path
}

/// A `Reader` which creates and updates its own `ProgressBar` at each read operation.
struct ProgressReader<R> {
    pb: ProgressBar,
    rd: R,
}

impl<R> ProgressReader<R> {
    pub fn new<S: ToString>(msg: S, rd: R, size: u64) -> Self {
        Self {
            pb: default_progress_bar(msg, size),
            rd,
        }
    }
}

impl<R> Drop for ProgressReader<R> {
    fn drop(&mut self) {
        self.pb.finish();
    }
}

impl<R> AsyncRead for ProgressReader<R>
where
    R: AsyncRead + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> task::Poll<io::Result<()>> {
        let cap = buf.filled().len();
        let res = Pin::new(&mut self.rd).poll_read(cx, buf);

        if matches!(res, Poll::Ready(Ok(()))) {
            let delta = buf.filled().len() - cap;
            self.pb.inc(delta as u64);
        }

        res
    }
}

/// A `Writer` which creates and updates its own `ProgressBar` at each write operation.
struct ProgressWriter<W> {
    pb: ProgressBar,
    wr: W,
}

impl<W> ProgressWriter<W> {
    pub fn new<S: ToString>(msg: S, wr: W, size: u64) -> Self {
        Self {
            pb: default_progress_bar(msg, size),
            wr,
        }
    }
}

impl<W> Drop for ProgressWriter<W> {
    fn drop(&mut self) {
        self.pb.finish();
    }
}

impl<W> AsyncWrite for ProgressWriter<W>
where
    W: AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let res = Pin::new(&mut self.wr).poll_write(cx, buf);

        if let Poll::Ready(Ok(n)) = res {
            self.pb.inc(n as u64);
        }

        res
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.wr).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.wr).poll_shutdown(cx)
    }
}

/// Creates a progress bar with a uniformed style for both uploads and downloads.
fn default_progress_bar<S: ToString>(msg: S, size: u64) -> ProgressBar {
    let pb = ProgressBar::new(size);
    pb.set_style(
        ProgressStyle::default_bar()
            .template(
                "{wide_msg} {percent:>7}% {bytes:>10} {binary_bytes_per_sec:>12}    eta {eta}",
            )
            .unwrap(),
    );
    pb.set_message(msg.to_string());
    pb
}
