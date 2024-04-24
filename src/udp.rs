use std::{net::SocketAddr, num::NonZeroUsize, path::Path, sync::Arc};

use async_trait::async_trait;
use rand::Rng;
use tokio::net::UdpSocket;
use udp_listener::{AcceptedUdpRead, AcceptedUdpWrite, Packet, UdpListener};

use crate::{
    socket::{socket, ReadSocket, WriteSocket},
    transport_layer::{self, UnreliableRead, UnreliableWrite},
};

const DISPATCHER_BUFFER_SIZE: usize = 1024;

type IdentityUdpListener = UdpListener<SocketAddr, Packet>;
type IdentityAcceptedUdpRead = AcceptedUdpRead<Packet>;

#[derive(Debug)]
pub struct Listener {
    listener: IdentityUdpListener,
    local_addr: SocketAddr,
}
impl Listener {
    pub async fn bind(addr: impl tokio::net::ToSocketAddrs) -> std::io::Result<Self> {
        let udp = UdpSocket::bind(addr).await?;
        let local_addr = udp.local_addr()?;
        let listener = UdpListener::new_identity_dispatch(
            udp,
            NonZeroUsize::new(DISPATCHER_BUFFER_SIZE).unwrap(),
        );
        Ok(Self {
            listener,
            local_addr,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Side-effect: same as [`Self::accept()`]
    pub async fn accept_without_handshake(&self) -> std::io::Result<Accepted> {
        let accepted = self.listener.accept().await?;
        let peer_addr = *accepted.dispatch_key();
        let (read, write) = accepted.split();
        let (read, write) = socket(Box::new(read), Box::new(write), None);
        Ok(Accepted {
            read,
            write,
            peer_addr,
        })
    }

    /// Side-effect: This method also dispatches packets to all the accepted UDP sockets.
    ///
    /// You should keep this method in a loop.
    pub async fn accept(&self) -> std::io::Result<Accepted> {
        let accepted = self.listener.accept().await?;
        let peer_addr = *accepted.dispatch_key();
        let (mut read, write) = accepted.split();
        let challenge = read.recv().try_recv().unwrap();
        write.send(&challenge).await?;
        let (read, write) = socket(Box::new(read), Box::new(write), None);
        Ok(Accepted {
            read,
            write,
            peer_addr,
        })
    }
}
#[derive(Debug)]
pub struct Accepted {
    pub read: ReadSocket,
    pub write: WriteSocket,
    pub peer_addr: SocketAddr,
}

pub async fn connect_without_handshake(
    addr: impl tokio::net::ToSocketAddrs,
    log_config: Option<LogConfig<'_>>,
) -> std::io::Result<Connected> {
    let udp = UdpSocket::bind("0.0.0.0:0").await?;
    udp.connect(addr).await?;
    let local_addr = udp.local_addr()?;
    let peer_addr = udp.peer_addr()?;
    let log_config = match log_config {
        Some(c) => Some(c.transport_layer_log_config(local_addr, peer_addr).await?),
        None => None,
    };
    let udp = Arc::new(udp);
    let (read, write) = socket(Box::new(Arc::clone(&udp)), Box::new(udp), log_config);
    Ok(Connected {
        read,
        write,
        local_addr,
        peer_addr,
    })
}
pub async fn connect(
    addr: impl tokio::net::ToSocketAddrs,
    log_config: Option<LogConfig<'_>>,
) -> std::io::Result<Connected> {
    let udp = UdpSocket::bind("0.0.0.0:0").await?;
    udp.connect(addr).await?;
    let local_addr = udp.local_addr()?;
    let peer_addr = udp.peer_addr()?;
    let log_config = match log_config {
        Some(c) => Some(c.transport_layer_log_config(local_addr, peer_addr).await?),
        None => None,
    };
    let mut challenge = [0; 1];
    let mut rng = rand::rngs::OsRng;
    rng.fill(&mut challenge);
    let _ = udp.send(&challenge).await?;
    let mut response = [0; 1];
    let _ = udp.recv(&mut response).await?;
    if challenge != response {
        return Err(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
    }
    let udp = Arc::new(udp);
    let (read, write) = socket(Box::new(Arc::clone(&udp)), Box::new(udp), log_config);
    Ok(Connected {
        read,
        write,
        local_addr,
        peer_addr,
    })
}
#[derive(Debug)]
pub struct Connected {
    pub read: ReadSocket,
    pub write: WriteSocket,
    pub local_addr: SocketAddr,
    pub peer_addr: SocketAddr,
}

// Accepted socket
#[async_trait]
impl UnreliableRead for IdentityAcceptedUdpRead {
    fn try_recv(&mut self, buf: &mut [u8]) -> Result<usize, std::io::ErrorKind> {
        let pkt = IdentityAcceptedUdpRead::recv(self)
            .try_recv()
            .map_err(|e| match e {
                tokio::sync::mpsc::error::TryRecvError::Empty => std::io::ErrorKind::WouldBlock,
                tokio::sync::mpsc::error::TryRecvError::Disconnected => {
                    std::io::ErrorKind::UnexpectedEof
                }
            })?;
        let min_len = buf.len().min(pkt.len());
        buf[..min_len].copy_from_slice(&pkt[..min_len]);
        Ok(min_len)
    }

    async fn recv(&mut self, buf: &mut [u8]) -> Result<usize, std::io::ErrorKind> {
        let pkt = IdentityAcceptedUdpRead::recv(self)
            .recv()
            .await
            .ok_or(std::io::ErrorKind::UnexpectedEof)?;
        let min_len = buf.len().min(pkt.len());
        buf[..min_len].copy_from_slice(&pkt[..min_len]);
        Ok(min_len)
    }
}
#[async_trait]
impl UnreliableWrite for AcceptedUdpWrite {
    async fn send(&self, buf: &[u8]) -> Result<usize, std::io::ErrorKind> {
        AcceptedUdpWrite::send(self, buf)
            .await
            .map_err(|e| e.kind())
    }
}

// Connected socket
#[async_trait]
impl UnreliableRead for Arc<UdpSocket> {
    fn try_recv(&mut self, buf: &mut [u8]) -> Result<usize, std::io::ErrorKind> {
        UdpSocket::try_recv(self, buf).map_err(|e| e.kind())
    }

    async fn recv(&mut self, buf: &mut [u8]) -> Result<usize, std::io::ErrorKind> {
        UdpSocket::recv(self, buf).await.map_err(|e| e.kind())
    }
}
#[async_trait]
impl UnreliableWrite for Arc<UdpSocket> {
    async fn send(&self, buf: &[u8]) -> Result<usize, std::io::ErrorKind> {
        let res = UdpSocket::send(self, buf).await;
        if let Err(e) = &res {
            if let Some(e) = e.raw_os_error() {
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                // No buffer space available
                if e == 55 {
                    return Ok(0);
                }
            }
        }
        res.map_err(|e| e.kind())
    }
}

#[derive(Debug, Clone)]
pub struct LogConfig<'a> {
    pub log_dir_path: &'a Path,
}
impl LogConfig<'_> {
    pub(crate) async fn transport_layer_log_config(
        &self,
        local_addr: SocketAddr,
        peer_addr: SocketAddr,
    ) -> std::io::Result<transport_layer::LogConfig> {
        tokio::fs::create_dir_all(&self.log_dir_path).await?;
        let file_name = format!("{local_addr}_{peer_addr}.csv");
        Ok(transport_layer::LogConfig {
            reliable_layer_log_path: self.log_dir_path.join(file_name),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn test_connect() {
        let listener = Listener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr();
        let msg_1 = b"hello";
        tokio::spawn(async move {
            loop {
                let mut accepted = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    accepted.write.send(msg_1, true).await.unwrap();
                    let mut buf = [0; 1];
                    accepted.read.recv(&mut buf).await.unwrap();
                });
            }
        });
        let connected = connect(
            addr,
            Some(LogConfig {
                log_dir_path: Path::new("target/tests"),
            }),
        )
        .await
        .unwrap();
        let mut buf = [0; 1024];
        let n = connected.read.recv(&mut buf).await.unwrap();
        assert_eq!(msg_1, &buf[..n]);
    }

    #[test]
    fn require_fn_to_be_send() {
        fn require_send<T: Send>(_t: T) {}
        require_send(connect("0.0.0.0:0", None));
    }
}
