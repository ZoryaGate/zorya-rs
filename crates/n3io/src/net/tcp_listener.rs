use std::future::poll_fn;
#[cfg(feature = "global_reactor")]
use std::{io::Result, net::SocketAddr};

use mio::{Interest, Token};

use crate::{net::TcpStream, reactor::Reactor};

/// An asynchronous [`TcpListener`](std::net::TcpListener) based on `mio` library.
#[derive(Debug)]
pub struct TcpListener {
    /// token
    token: Token,
    /// inner source.
    mio_tcp_listener: mio::net::TcpListener,
    /// reactor bound to this io.
    reactor: Reactor,
}

impl TcpListener {
    /// Returns the immutable reference to the inner mio socket.
    pub fn mio_socket(&self) -> &mio::net::TcpListener {
        &self.mio_tcp_listener
    }

    /// See [`bind_with`](Self::bind_with)
    #[cfg(feature = "global_reactor")]
    pub async fn bind(addr: SocketAddr) -> Result<Self> {
        use crate::reactor::global_reactor;

        Self::bind_with(addr, global_reactor().clone()).await
    }

    /// Convenience method to bind a new TCP listener to the specified address to receive new connections.
    pub async fn bind_with(addr: SocketAddr, reactor: Reactor) -> Result<Self> {
        let mut mio_tcp_listener = mio::net::TcpListener::bind(addr)?;

        let token = reactor.register(&mut mio_tcp_listener, Interest::READABLE)?;

        Ok(Self {
            token,
            mio_tcp_listener,
            reactor,
        })
    }

    /// Accepts a new TcpStream.
    ///
    /// If an accepted stream is returned, the remote address of the peer is returned along with it.
    pub async fn accept(&self) -> Result<(TcpStream, SocketAddr)> {
        let (mut mio_tcp_stream, raddr) = poll_fn(|cx| {
            self.reactor
                .poll_io(cx, self.token, Interest::READABLE, |_| {
                    self.mio_tcp_listener.accept()
                })
        })
        .await?;

        let token = self.reactor.register(
            &mut mio_tcp_stream,
            Interest::READABLE.add(Interest::WRITABLE),
        )?;

        Ok((
            TcpStream {
                token,
                mio_tcp_stream,
                reactor: self.reactor.clone(),
            },
            raddr,
        ))
    }
}

#[cfg(feature = "global_reactor")]
#[cfg(test)]
mod tests {
    use std::{io::ErrorKind, time::Duration};

    use futures::{AsyncReadExt, AsyncWriteExt, executor::ThreadPool};

    use crate::timeout::TimeoutExt;

    use super::*;

    #[futures_test::test]
    async fn test_accept_timeout() {
        // _ = pretty_env_logger::try_init_timed();
        let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();

        assert_eq!(
            listener
                .accept()
                .timeout(Duration::from_millis(100))
                .await
                .expect_err("expect timeout")
                .kind(),
            ErrorKind::TimedOut,
            "expect timeout"
        );
    }

    #[futures_test::test]
    async fn test_echo() {
        let spawner = ThreadPool::new().unwrap();

        let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();

        let laddr = listener.mio_socket().local_addr().unwrap();

        spawner.spawn_ok(async move {
            while let Ok((conn, _)) = listener.accept().await {
                futures::io::copy(&conn, &mut &conn).await.unwrap();
            }
        });

        for _ in 0..10 {
            let mut conn = TcpStream::connect(laddr).await.unwrap();

            conn.write_all(b"hello world").await.unwrap();
            let mut buf = vec![0; 100];
            let read_size = conn.read(&mut buf).await.unwrap();

            assert_eq!(&buf[..read_size], b"hello world");
        }
    }
}
