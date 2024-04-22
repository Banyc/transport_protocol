use std::{net::SocketAddr, num::NonZeroUsize, sync::Arc};

use async_trait::async_trait;
use rand::Rng;
use tokio::net::UdpSocket;
use udp_listener::{AcceptedUdpRead, AcceptedUdpWrite, Packet, UdpListener};

use crate::{
    socket::{socket, ReadSocket, WriteSocket},
    transport_layer::{UnreliableRead, UnreliableWrite},
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

    /// Side-effect: This method also dispatches packets to all the accepted UDP sockets.
    ///
    /// You should keep this method in a loop.
    pub async fn accept(&self) -> std::io::Result<(ReadSocket, WriteSocket)> {
        let accepted = self.listener.accept().await?;
        let (mut read, write) = accepted.split();
        let challenge = read.recv().try_recv().unwrap();
        write.send(&challenge).await?;
        Ok(socket(Box::new(read), Box::new(write)))
    }
}

pub async fn connect_without_handshake(
    addr: impl tokio::net::ToSocketAddrs,
) -> std::io::Result<(ReadSocket, WriteSocket)> {
    let udp = UdpSocket::bind("0.0.0.0:0").await?;
    udp.connect(addr).await?;
    let udp = Arc::new(udp);
    Ok(socket(Box::new(Arc::clone(&udp)), Box::new(udp)))
}

pub async fn connect(
    addr: impl tokio::net::ToSocketAddrs,
) -> Result<(ReadSocket, WriteSocket), std::io::ErrorKind> {
    let (read, mut write) = connect_without_handshake(addr)
        .await
        .map_err(|e| e.kind())?;
    let mut challenge = [0; 1];
    let mut rng = rand::thread_rng();
    rng.fill(&mut challenge);
    let _ = write.send(&challenge, true).await?;
    let mut response = [0; 1];
    let _ = read.recv(&mut response).await?;
    if challenge != response {
        return Err(std::io::ErrorKind::ConnectionReset);
    }
    Ok((read, write))
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
        UdpSocket::send(self, buf).await.map_err(|e| e.kind())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_connect() {
        let listener = Listener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr();
        tokio::spawn(async move {
            loop {
                if listener.accept().await.is_err() {
                    break;
                }
            }
        });
        let _ = connect(addr).await.unwrap();
    }
}