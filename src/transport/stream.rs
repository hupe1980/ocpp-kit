//! A socket that may or may not be wrapped in TLS.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

/// Either a plain TCP stream (security profile 1) or a TLS stream (profiles 2 and 3).
#[derive(Debug)]
pub enum MaybeTls {
    /// An unencrypted connection.
    Plain(TcpStream),
    /// A TLS client connection.
    #[cfg(feature = "rustls")]
    ClientTls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
    /// A TLS server connection.
    #[cfg(feature = "rustls")]
    ServerTls(Box<tokio_rustls::server::TlsStream<TcpStream>>),
}

impl MaybeTls {
    /// The DER-encoded end-entity certificate the peer presented, if any.
    ///
    /// A CSMS uses it to bind a security-profile-3 connection to a Charging Station identity.
    #[must_use]
    pub fn peer_certificate(&self) -> Option<Vec<u8>> {
        match self {
            #[cfg(feature = "rustls")]
            MaybeTls::ServerTls(stream) => stream
                .get_ref()
                .1
                .peer_certificates()
                .and_then(<[_]>::first)
                .map(|cert| cert.as_ref().to_vec()),
            _ => None,
        }
    }
}

macro_rules! project {
    ($self:ident, $stream:ident => $body:expr) => {
        match $self.get_mut() {
            MaybeTls::Plain($stream) => $body,
            #[cfg(feature = "rustls")]
            MaybeTls::ClientTls($stream) => $body,
            #[cfg(feature = "rustls")]
            MaybeTls::ServerTls($stream) => $body,
        }
    };
}

impl AsyncRead for MaybeTls {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        project!(self, stream => Pin::new(stream).poll_read(cx, buf))
    }
}

impl AsyncWrite for MaybeTls {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        project!(self, stream => Pin::new(stream).poll_write(cx, buf))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        project!(self, stream => Pin::new(stream).poll_flush(cx))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        project!(self, stream => Pin::new(stream).poll_shutdown(cx))
    }
}
