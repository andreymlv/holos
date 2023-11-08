use anyhow::Result;

use futures::{self, SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio_util::codec::{Framed, LinesCodec};

use clap::Parser;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;

/// Holos server
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Port for server
    port: u16,

    /// How many tasks will be spawned for udp processing
    #[arg(short, long, default_value_t = 1)]
    tasks: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Configure a `tracing` subscriber that logs traces emitted by the chat
    // server.
    tracing_subscriber::fmt().init();

    let args = Args::parse();
    let addr: SocketAddr = format!("[::]:{}", args.port).parse()?;

    // Create the shared state. This is how all the peers communicate.
    //
    // The server task will hold a handle to this. For every new client, the
    // `state` handle is cloned and passed into the task that processes the
    // client connection.
    let tcp_state = Arc::new(Mutex::new(SharedTcp::new()));
    let udp_addrs = Arc::new(Mutex::new(HashSet::new()));
    // Bind a TCP listener to the socket address.
    //
    // Note that this is the Tokio TcpListener, which is fully async.
    let listener = TcpListener::bind(&addr).await?;
    let socket = Arc::new(UdpSocket::bind(&addr).await?);
    let (tx, _) = broadcast::channel::<(Vec<u8>, SocketAddr)>(4096);
    let mut txs = Vec::new();

    tracing::info!("server running on {}", addr);

    for i in 0..args.tasks {
        txs.push(tx.clone());
        let recv = socket.clone();
        let tx = txs[i].clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            loop {
                let (len, addr) = recv.recv_from(&mut buf).await.unwrap();
                tx.send((buf[..len].to_vec(), addr)).unwrap();
            }
        });
    }

    for tx in &txs {
        let udp_addrs = udp_addrs.clone();
        let mut rx = tx.subscribe();
        let send = socket.clone();
        tokio::spawn(async move {
            loop {
                let (bytes, addr) = rx.recv().await.unwrap();
                let mut udp_addrs = udp_addrs.lock().await;
                if !udp_addrs.contains(&addr) {
                    udp_addrs.insert(addr);
                }
                for udp_addr in udp_addrs.iter() {
                    if addr != *udp_addr {
                        send.send_to(&bytes, udp_addr).await.unwrap();
                    }
                }
            }
        });
    }

    loop {
        let (stream, addr) = listener.accept().await?;

        // Clone a handle to the `Shared` state for the new connection.
        let state = tcp_state.clone();

        // Spawn our handler to be run asynchronously.
        tokio::spawn(async move {
            tracing::debug!("accepted connection from {}", addr);
            if let Err(e) = process_tcp(state, stream, addr).await {
                tracing::info!("an error occurred; error = {:?}", e);
            }
        });
    }
}

/// Shorthand for the transmit half of the message channel.
type TcpTx = mpsc::UnboundedSender<String>;

/// Shorthand for the receive half of the message channel.
type TcpRx = mpsc::UnboundedReceiver<String>;

/// Data that is shared between all peers in the chat server.
///
/// This is the set of `Tx` handles for all connected clients. Whenever a
/// message is received from a client, it is broadcasted to all peers by
/// iterating over the `peers` entries and sending a copy of the message on each
/// `Tx`.
struct SharedTcp {
    peers: HashMap<SocketAddr, TcpTx>,
}

/// The state for each connected client.
struct TcpPeer {
    /// The TCP socket wrapped with the `Lines` codec, defined below.
    ///
    /// This handles sending and receiving data on the socket. When using
    /// `Lines`, we can work at the line level instead of having to manage the
    /// raw byte operations.
    lines: Framed<TcpStream, LinesCodec>,

    /// Receive half of the message channel.
    ///
    /// This is used to receive messages from peers. When a message is received
    /// off of this `Rx`, it will be written to the socket.
    rx: TcpRx,
}

impl SharedTcp {
    /// Create a new, empty, instance of `Shared`.
    fn new() -> Self {
        SharedTcp {
            peers: HashMap::new(),
        }
    }

    /// Send a `LineCodec` encoded message to every peer, except
    /// for the sender.
    async fn broadcast(&mut self, sender: SocketAddr, message: &str) {
        for peer in self.peers.iter_mut() {
            if *peer.0 != sender {
                let _ = peer.1.send(message.into());
            }
        }
    }
}

impl TcpPeer {
    /// Create a new instance of `Peer`.
    async fn new(
        state: Arc<Mutex<SharedTcp>>,
        lines: Framed<TcpStream, LinesCodec>,
    ) -> Result<TcpPeer> {
        // Get the client socket address
        let addr = lines.get_ref().peer_addr()?;

        // Create a channel for this peer
        let (tx, rx) = mpsc::unbounded_channel();

        // Add an entry for this `Peer` in the shared state map.
        state.lock().await.peers.insert(addr, tx);

        Ok(TcpPeer { lines, rx })
    }
}

/// Process an individual chat client
async fn process_tcp(
    state: Arc<Mutex<SharedTcp>>,
    stream: TcpStream,
    addr: SocketAddr,
) -> Result<()> {
    let mut lines = Framed::new(stream, LinesCodec::new());

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
    let mut peer = TcpPeer::new(state.clone(), lines).await?;

    // A client has connected, let's let everyone know.
    {
        let mut state = state.lock().await;
        let msg = format!("{} has joined the chat", username);
        tracing::info!("{}", msg);
        state.broadcast(addr, &msg).await;
    }

    // Process incoming messages until our stream is exhausted by a disconnect.
    loop {
        tokio::select! {
            // A message was received from a peer. Send it to the current user.
            Some(msg) = peer.rx.recv() => {
                peer.lines.send(&msg).await?;
            }
            result = peer.lines.next() => match result {
                // A message was received from the current user, we should
                // broadcast this message to the other users.
                Some(Ok(msg)) => {
                    let mut state = state.lock().await;
                    let msg = format!("{}: {}", username, msg);

                    state.broadcast(addr, &msg).await;
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
        }
    }

    // If this section is reached it means that the client was disconnected!
    // Let's let everyone still connected know about it.
    {
        let mut state = state.lock().await;
        state.peers.remove(&addr);

        let msg = format!("{} has left the chat", username);
        tracing::info!("{}", msg);
        state.broadcast(addr, &msg).await;
    }

    Ok(())
}
