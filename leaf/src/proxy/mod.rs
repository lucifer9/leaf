use std::{io, net::SocketAddr};

use async_trait::async_trait;
use futures::future::select_ok;
use futures::stream::Stream;
use futures::TryFutureExt;
use log::*;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpSocket, UdpSocket};
#[cfg(target_os = "android")]
use {
    lazy_static::lazy_static, std::os::unix::io::AsRawFd, std::os::unix::io::RawFd,
    tokio::io::AsyncReadExt, tokio::io::AsyncWriteExt, tokio::net::UnixStream, tokio::sync::Mutex,
};

use crate::{
    app::SyncDnsClient,
    common::resolver::Resolver,
    option,
    session::{DatagramSource, Session, SocksAddr},
};

pub mod datagram;
pub mod inbound;
pub mod outbound;
pub mod stream;

#[cfg(any(feature = "inbound-amux", feature = "outbound-amux"))]
pub mod amux;
#[cfg(any(feature = "inbound-chain", feature = "outbound-chain"))]
pub mod chain;
#[cfg(feature = "outbound-direct")]
pub mod direct;
#[cfg(feature = "outbound-drop")]
pub mod drop;
#[cfg(feature = "outbound-failover")]
pub mod failover;
#[cfg(feature = "outbound-h2")]
pub mod h2;
#[cfg(feature = "inbound-http")]
pub mod http;
#[cfg(any(feature = "inbound-quic", feature = "outbound-quic"))]
pub mod quic;
#[cfg(feature = "outbound-random")]
pub mod random;
#[cfg(feature = "outbound-redirect")]
pub mod redirect;
#[cfg(feature = "outbound-retry")]
pub mod retry;
#[cfg(feature = "outbound-select")]
pub mod select;
#[cfg(any(feature = "inbound-shadowsocks", feature = "outbound-shadowsocks"))]
pub mod shadowsocks;
#[cfg(any(feature = "inbound-socks", feature = "outbound-socks"))]
pub mod socks;
#[cfg(feature = "outbound-tls")]
pub mod tls;
#[cfg(any(feature = "inbound-trojan", feature = "outbound-trojan"))]
pub mod trojan;
#[cfg(feature = "outbound-tryall")]
pub mod tryall;
#[cfg(all(
    feature = "inbound-tun",
    any(
        target_os = "ios",
        target_os = "android",
        target_os = "macos",
        target_os = "linux"
    )
))]
pub mod tun;
#[cfg(feature = "outbound-vmess")]
pub mod vmess;
#[cfg(any(feature = "inbound-ws", feature = "outbound-ws"))]
pub mod ws;

pub use datagram::{
    SimpleInboundDatagram, SimpleInboundDatagramRecvHalf, SimpleInboundDatagramSendHalf,
    SimpleOutboundDatagram, SimpleOutboundDatagramRecvHalf, SimpleOutboundDatagramSendHalf,
};
pub use stream::{BufHeadProxyStream, SimpleProxyStream};

#[derive(Clone)]
pub enum ProxyHandlerType {
    Direct,
    Endpoint,
    Ensemble,
}

#[derive(Clone, PartialEq, Debug)]
pub enum UdpTransportType {
    Stream,
    Packet,
    Unknown,
}

pub trait Tag {
    fn tag(&self) -> &String;
}

pub trait Color {
    fn color(&self) -> colored::Color;
}

pub trait HandlerTyped {
    fn handler_type(&self) -> ProxyHandlerType;
}

#[cfg(target_os = "android")]
lazy_static! {
    static ref SOCKET_PROTECT_PATH: Mutex<Option<String>> = Mutex::new(None);
}

// Sets the RPC service endpoint for protecting outbound sockets on Android to
// avoid infinite loop. The `path` is treated as a Unix domain socket endpoint.
// The RPC service simply listens for incoming connections, reads an int32 on
// each connection, treats it as the file descriptor to protect, writes back 0
// on success.
#[cfg(target_os = "android")]
pub async fn set_socket_protect_path(path: String) {
    SOCKET_PROTECT_PATH.lock().await.replace(path);
}

#[cfg(target_os = "android")]
async fn protect_socket(fd: RawFd) -> io::Result<()> {
    if let Some(path) = SOCKET_PROTECT_PATH.lock().await.as_ref() {
        let mut stream = UnixStream::connect(path).await?;
        stream.write_i32(fd as i32).await?;
        if stream.read_i32().await? != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("failed to protect outbound socket {}", fd),
            ));
        }
    }
    Ok(())
}

// New UDP socket.
async fn create_udp_socket(bind_addr: &SocketAddr) -> io::Result<UdpSocket> {
    let socket = UdpSocket::bind(bind_addr).await?;
    #[cfg(target_os = "android")]
    {
        protect_socket(socket.as_raw_fd()).await?;
    }
    Ok(socket)
}

// A single TCP dial.
async fn tcp_dial_task(
    dial_addr: SocketAddr,
    bind_addr: &SocketAddr,
) -> io::Result<(Box<dyn ProxyStream>, SocketAddr)> {
    let socket = TcpSocket::new_v4()?;
    #[cfg(target_os = "android")]
    {
        protect_socket(socket.as_raw_fd()).await?;
    }
    socket.bind(*bind_addr)?;
    trace!("dialing tcp {}", &dial_addr);
    let stream = socket.connect(dial_addr).await?;
    trace!("connected tcp {}", &dial_addr);
    Ok((Box::new(SimpleProxyStream(stream)), dial_addr))
}

// Dials a TCP stream.
pub async fn dial_tcp_stream(
    dns_client: SyncDnsClient,
    bind_addr: &SocketAddr,
    address: &str,
    port: &u16,
) -> io::Result<Box<dyn ProxyStream>> {
    let mut resolver = Resolver::new(dns_client.clone(), bind_addr, address, port)
        .map_err(|e| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("resolve address failed: {}", e),
            )
        })
        .await?;

    let mut last_err = None;

    let mut done = false;

    while !done {
        let mut tasks = Vec::new();
        for _ in 0..*option::OUTBOUND_DIAL_CONCURRENCY {
            let dial_addr = match resolver.next() {
                Some(a) => a,
                None => {
                    done = true; // run out
                    break; // break and execute tasks if there're any
                }
            };
            let t = tcp_dial_task(dial_addr, bind_addr);
            tasks.push(Box::pin(t));
        }
        if !tasks.is_empty() {
            match select_ok(tasks.into_iter()).await {
                Ok(v) => {
                    #[rustfmt::skip]
                    dns_client.read().await.optimize_cache(address.to_owned(), v.0.1.ip()).await;
                    #[rustfmt::skip]
                    return Ok(v.0.0);
                }
                Err(e) => {
                    last_err = Some(io::Error::new(
                        io::ErrorKind::Other,
                        format!("all attempts failed, last error: {}", e),
                    ));
                }
            }
        }
    }

    Err(last_err.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "could not resolve to any address",
        )
    }))
}

/// An interface with the ability to dial TCP connections.
#[async_trait]
pub trait TcpConnector: Send + Sync + Unpin {
    /// Dials a TCP connection.
    async fn dial_tcp_stream(
        &self,
        dns_client: SyncDnsClient,
        bind_addr: &SocketAddr,
        address: &str,
        port: &u16,
    ) -> io::Result<Box<dyn ProxyStream>> {
        dial_tcp_stream(dns_client, bind_addr, address, port).await
    }
}

/// An interface with the ability to create UDP sockets.
#[async_trait]
pub trait UdpConnector: Send + Sync + Unpin {
    /// Creates a UDP socket.
    async fn create_udp_socket(&self, bind_addr: &SocketAddr) -> io::Result<UdpSocket> {
        create_udp_socket(bind_addr).await
    }
}

/// A reliable transport for both inbound and outbound handlers.
pub trait ProxyStream: AsyncRead + AsyncWrite + Send + Sync + Unpin {}

/// An outbound handler for both UDP and TCP outgoing connections.
pub trait OutboundHandler:
    Tag + Color + HandlerTyped + TcpOutboundHandler + UdpOutboundHandler + Send + Unpin
{
    fn has_tcp(&self) -> bool;
    fn has_udp(&self) -> bool;
}

#[derive(Debug)]
pub enum OutboundConnect {
    Proxy(String, u16, SocketAddr),
    Direct(SocketAddr),
    NoConnect,
}

/// An outbound handler for outgoing TCP conections.
#[async_trait]
pub trait TcpOutboundHandler: Send + Sync + Unpin {
    /// Returns the name of the handler.
    fn name(&self) -> &str;

    /// Returns the address which the underlying transport should
    /// communicate with.
    fn tcp_connect_addr(&self) -> Option<OutboundConnect>;

    /// Handles a session with the given stream. On success, returns a
    /// stream wraps the incoming stream.
    async fn handle_tcp<'a>(
        &'a self,
        sess: &'a Session,
        stream: Option<Box<dyn ProxyStream>>,
    ) -> io::Result<Box<dyn ProxyStream>>;
}

/// An unreliable transport for outbound handlers.
pub trait OutboundDatagram: Send + Unpin {
    /// Splits the datagram.
    fn split(
        self: Box<Self>,
    ) -> (
        Box<dyn OutboundDatagramRecvHalf>,
        Box<dyn OutboundDatagramSendHalf>,
    );
}

/// The receive half.
#[async_trait]
pub trait OutboundDatagramRecvHalf: Sync + Send + Unpin {
    /// Receives a message on the socket. On success, returns the number of
    /// bytes read and the origin of the message.
    async fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<(usize, SocksAddr)>;
}

/// The send half.
#[async_trait]
pub trait OutboundDatagramSendHalf: Sync + Send + Unpin {
    /// Sends a message on the socket to `dst_addr`. On success, returns the
    /// number of bytes sent.
    ///
    /// `dst_addr` is not the proxy server address.
    async fn send_to(&mut self, buf: &[u8], dst_addr: &SocksAddr) -> io::Result<usize>;
}

/// An outbound handler for outgoing UDP connections.
#[async_trait]
pub trait UdpOutboundHandler: Send + Sync + Unpin {
    /// Returns the name of the handler.
    fn name(&self) -> &str;

    /// Returns the address which the underlying transport should
    /// communicate with.
    fn udp_connect_addr(&self) -> Option<OutboundConnect>;

    /// Returns the transport type of this handler.
    ///
    /// For example, for a SOCKS5 handler, the UDP transport type is
    /// `UdpTransportType::Packet`, but for a trojan handler, the transport
    /// type is `UdpTransportType::Stream` because trojan transport UDP
    /// packets over TCP connections.
    fn udp_transport_type(&self) -> UdpTransportType;

    /// Handles a session with the transport. On success, returns an outbound
    /// datagram wraps the incoming transport.
    async fn handle_udp<'a>(
        &'a self,
        sess: &'a Session,
        transport: Option<OutboundTransport>,
    ) -> io::Result<Box<dyn OutboundDatagram>>;
}

/// An outbound transport represents either a reliable or unreliable transport.
pub enum OutboundTransport {
    /// The reliable transport.
    Stream(Box<dyn ProxyStream>),
    /// The unreliable transport.
    Datagram(Box<dyn OutboundDatagram>),
}

pub trait InboundHandler:
    Tag + TcpInboundHandler + UdpInboundHandler + Send + Sync + Unpin
{
    fn has_tcp(&self) -> bool;
    fn has_udp(&self) -> bool;
}

/// An inbound handler for incoming TCP connections.
#[async_trait]
pub trait TcpInboundHandler: Send + Sync + Unpin {
    async fn handle_tcp<'a>(
        &'a self,
        sess: Session,
        stream: Box<dyn ProxyStream>,
    ) -> std::io::Result<InboundTransport>;
}

/// An inbound handler for incoming UDP connections.
#[async_trait]
pub trait UdpInboundHandler: Send + Sync + Unpin {
    // TODO Returns an InboundTransport to support UDP-based reliable transports
    // such as QUIC.
    async fn handle_udp<'a>(
        &'a self,
        socket: Box<dyn InboundDatagram>,
    ) -> io::Result<InboundTransport>;
}

/// An unreliable transport for inbound handlers.
pub trait InboundDatagram: Send + Sync + Unpin {
    /// Splits the datagram.
    fn split(
        self: Box<Self>,
    ) -> (
        Box<dyn InboundDatagramRecvHalf>,
        Box<dyn InboundDatagramSendHalf>,
    );

    /// Turns the datagram into a [`std::net::UdpSocket`].
    fn into_std(self: Box<Self>) -> io::Result<std::net::UdpSocket>;
}

/// The receive half.
#[async_trait]
pub trait InboundDatagramRecvHalf: Sync + Send + Unpin {
    /// Receives a single datagram message on the socket. On success, returns
    /// the number of bytes read, the source where this message
    /// originated and the destination this message shall be sent to.
    ///
    /// This should be implemented by a proxy inbound handler, the destination
    /// address could be decoded from the raw message according to the protocol
    /// specification.
    async fn recv_from(
        &mut self,
        buf: &mut [u8],
    ) -> io::Result<(usize, DatagramSource, Option<SocksAddr>)>;
}

/// The send half.
#[async_trait]
pub trait InboundDatagramSendHalf: Sync + Send + Unpin {
    /// Sends a datagram message on the socket to `dst_addr`, the `src_addr`
    /// specifies the origin of the message. On success, returns the number
    /// of bytes sent.
    ///
    /// This should be implemented by a proxy inbound handler, and the
    /// `src_addr` should be encapsulated into the protocol header to indicate
    /// the origin of the message.
    async fn send_to(
        &mut self,
        buf: &[u8],
        src_addr: Option<&SocksAddr>,
        dst_addr: &SocketAddr,
    ) -> io::Result<usize>;
}

pub enum SingleInboundTransport {
    /// The reliable transport.
    Stream(Box<dyn ProxyStream>, Session),
    /// The unreliable transport.
    Datagram(Box<dyn InboundDatagram>),
    /// None.
    Empty,
}

pub type IncomingTransport = Box<dyn Stream<Item = SingleInboundTransport> + Send + Unpin>;

/// An inbound transport represents either a reliable or unreliable transport.
pub enum InboundTransport {
    /// The reliable transport.
    Stream(Box<dyn ProxyStream>, Session),
    /// The unreliable transport.
    Datagram(Box<dyn InboundDatagram>),
    Incoming(IncomingTransport),
    /// None.
    Empty,
}
