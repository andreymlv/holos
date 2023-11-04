use anyhow::Result;
use clap::Parser;
use futures::SinkExt;
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use tokio::{
    net::{TcpListener, TcpStream, UdpSocket},
    sync::{mpsc, Mutex},
};
use tokio_stream::StreamExt;
use tokio_util::{
    bytes::Bytes,
    codec::{BytesCodec, Framed, LinesCodec},
    udp::UdpFramed,
};
use tracing::info;

/// Holos server
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Port for server
    port: u16,
}

/// Shorthand for the transmit half of the message channel.
type TCPtx = mpsc::UnboundedSender<String>;
type UDPtx = mpsc::UnboundedSender<Vec<u8>>;

/// Shorthand for the receive half of the message channel.
type TCPrx = mpsc::UnboundedReceiver<String>;
type UDPrx = mpsc::UnboundedReceiver<Vec<u8>>;

/// Data that is shared between all peers in the chat server.
struct Shared {
    peers_tcp: HashMap<SocketAddr, TCPtx>,
    peers_udp: HashMap<SocketAddr, UDPtx>,
}

impl Shared {
    /// Create a new, empty, instance of `Shared`.
    fn new() -> Self {
        Shared {
            peers_tcp: HashMap::new(),
            peers_udp: HashMap::new(),
        }
    }

    async fn tcp_broadcast(&mut self, sender: SocketAddr, message: &str) {
        for peer in self.peers_tcp.iter_mut() {
            if *peer.0 != sender {
                let _ = peer.1.send(message.into());
            }
        }
    }

    async fn udp_broadcast(&mut self, sender: SocketAddr, message: &[u8]) {
        for peer in self.peers_udp.iter_mut() {
            if *peer.0 != sender {
                let _ = peer.1.send(message.into());
            }
        }
    }
}

/// The state for each connected client.
struct Peer {
    data: UdpFramed<BytesCodec, Arc<UdpSocket>>,
    control: Framed<TcpStream, LinesCodec>,

    /// Receive half of the message channel.
    ///
    /// This is used to receive messages from peers. When a message is received
    /// off of this `Rx`, it will be written to the socket.
    tcp_rx: TCPrx,
    udp_rx: UDPrx,
}

impl Peer {
    /// Create a new instance of `Peer`.
    async fn new(
        state: Arc<Mutex<Shared>>,
        data: UdpFramed<BytesCodec, Arc<UdpSocket>>,
        control: Framed<TcpStream, LinesCodec>,
        udp_addr: SocketAddr,
    ) -> Result<Peer> {
        // Get the client socket address
        let tcp_addr = control.get_ref().peer_addr()?;

        let mut state = state.lock().await;
        // Create a channel for this peer
        let (tcp_tx, tcp_rx) = mpsc::unbounded_channel();
        let (udp_tx, udp_rx) = mpsc::unbounded_channel();

        // Add an entry for this `Peer` in the shared state map.
        state.peers_tcp.insert(tcp_addr, tcp_tx);
        state.peers_udp.insert(udp_addr, udp_tx);

        Ok(Peer {
            data,
            control,
            tcp_rx,
            udp_rx,
        })
    }
}

/// Process an individual chat client
async fn process_text(
    state: Arc<Mutex<Shared>>,
    udp_sock: Arc<UdpSocket>,
    control: TcpStream,
    addr: SocketAddr,
) -> Result<()> {
    let data = UdpFramed::new(udp_sock.clone(), BytesCodec::new());
    let mut lines = Framed::new(control, LinesCodec::new());

    let udp_addr: SocketAddr = lines.next().await.unwrap()?.parse()?;
    let udp_port = udp_addr.port();
    let udp_addr = SocketAddr::new(addr.ip(), udp_port);
    println!("{:?}", udp_addr);

    // Send a prompt to the client to enter their username.
    lines.send("Please enter your username:").await?;
    // Read the first line from the `LineCodec` stream to get the username.
    let username = match lines.next().await {
        Some(Ok(line)) => line,
        // We didn't get a line so we return early here.
        _ => {
            tracing::error!("Failed to get username from {}. Client disconnected.", addr);
            return Ok(());
        }
    };

    // Register our peer with state which internally sets up some channels.
    let mut peer = Peer::new(state.clone(), data, lines, udp_addr).await?;

    // A client has connected, let's let everyone know.
    {
        let mut state = state.lock().await;
        let msg = format!("{} has joined the chat", username);
        tracing::info!("{}", msg);
        state.tcp_broadcast(addr, &msg).await;
    }

    // Process incoming messages until our stream is exhausted by a disconnect.
    loop {
        tokio::select! {
            // A message was received from a peer. Send it to the current user.
            Some(msg) = peer.tcp_rx.recv() => {
                peer.control.send(&msg).await?;
            }
            result = peer.control.next() => match result {
                // A message was received from the current user with TCP, we should
                // broadcast this message to the other users with TCP!!!
                Some(Ok(msg)) => {
                    let mut state = state.lock().await;
                    let msg = format!("{}: {}", username, msg);

                    state.tcp_broadcast(addr, &msg).await;
                }
                // An error occurred.
                Some(Err(e)) => {
                    tracing::error!(
                        "an error occurred while processing messages for {}; error = {:?}",
                        username,
                        e
                    );
                }
                // The stream has been exhausted.
                None => break,
            },
            Some(msg) = peer.udp_rx.recv() => {
                // from udp_broadcast get message that need to transmit through network
                peer.data.send((Bytes::copy_from_slice(&msg), udp_addr)).await?;
            }
            Some(data) = peer.data.next() => {
                let data = data?;
                let mut state = state.lock().await;
                state.udp_broadcast(data.1, &data.0).await;
            },
        }
    }

    // If this section is reached it means that the client was disconnected!
    // Let's let everyone still connected know about it.
    {
        let mut state = state.lock().await;
        state.peers_tcp.remove(&addr);
        state.peers_udp.remove(&addr);

        let msg = format!("{} has left the chat", username);
        tracing::info!("{}", msg);
        state.tcp_broadcast(addr, &msg).await;
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt().init();
    let addr = format!("[::]:{}", args.port);
    let listener = TcpListener::bind(&addr).await?;
    info!("TCP binded to {}", listener.local_addr()?);
    let socket = Arc::new(UdpSocket::bind(&addr).await?);
    info!("UDP binded to {}", socket.local_addr()?);
    let state = Arc::new(Mutex::new(Shared::new()));
    loop {
        let (stream, addr) = listener.accept().await?;
        info!("TCP connected {}", addr);
        let state = Arc::clone(&state);
        let socket = Arc::clone(&socket);
        tokio::spawn(async move {
            tracing::debug!("accepted connection");
            if let Err(e) = process_text(state, socket, stream, addr).await {
                tracing::info!("an error occurred; error = {:?}", e);
            }
        });
    }
}
