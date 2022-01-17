use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

use anyhow::Result;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
};

pub struct Session(ssh2::Session);

impl Session {
    pub async fn new(addr: &str) -> Result<Self> {
        let addr = match addr.split_once(':') {
            Some((addr, port)) => (addr.to_string(), port.parse::<u16>()?),
            None => (addr.to_string(), 22),
        };

        // Connect to the remote SSH server
        let tcp = TcpStream::connect(addr).await?;
        let mut session = ssh2::Session::new()?;

        session.set_tcp_stream(tcp);
        session.handshake()?;

        Ok(Self(session))
    }

    pub async fn authenticate_with_agent(&self, username: &str) -> Result<()> {
        Ok(self.0.userauth_agent(username)?)
    }

    pub async fn authenticate_with_password(&self, username: &str, password: &str) -> Result<()> {
        Ok(self.0.userauth_password(username, password)?)
    }
}

impl AsyncRead for Session {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        todo!()
    }
}

impl AsyncWrite for Session {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        todo!()
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        todo!()
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        todo!()
    }
}
