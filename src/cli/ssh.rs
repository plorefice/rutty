use std::io::Write;

use anyhow::{bail, Context, Result};
use nix::unistd::{Uid, User};
use zeroize::Zeroize;

use crate::{
    cli::{
        opts::{Connection, Opts, TristateOpt},
        Terminal,
    },
    ssh::{Channel, Session},
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

    let (target_sftp, target_path) = match scp.target.split_once(':') {
        Some((remote, path)) => {
            let (session, _) = establish_ssh_session(term, remote, scp.opts.port).await?;
            let sftp = session.sftp().await?;
            (Some(sftp), path)
        }
        None => (None, scp.target.as_str()),
    };

    for source in &scp.sources {
        let (source_sftp, source_path) = match source.split_once(':') {
            Some((remote, path)) => {
                let (session, _) = establish_ssh_session(term, remote, scp.opts.port).await?;
                let sftp = session.sftp().await?;
                (Some(sftp), path)
            }
            None => (None, source.as_str()),
        };

        match (source_sftp, &target_sftp) {
            (None, Some(target)) => {
                target.upload(source_path, target_path).await?;
            }
            (Some(_), None) => todo!(),
            (None, None) => {
                // A local copy is performed if neither source nor target are remote hosts
                tokio::fs::copy(source_path, target_path).await?;
            }
            (Some(_), Some(_)) => bail!("Cannot transfer files between two remote hosts"),
        }
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
    // Try using the agent first, and fallback on password authentication.
    let channel = match session.authenticate_with_agent(&username).await {
        Ok(channel) => channel,
        Err(_) => {
            // Show a prompt to the user
            write!(term, "Password: ")?;
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
