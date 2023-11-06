use anyhow::Result;

use clap::Parser;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use futures::{SinkExt, StreamExt};

use ringbuf::{Consumer, HeapRb, SharedRb};

use std::io::BufRead;
use std::mem::MaybeUninit;
use std::net::SocketAddr;
use std::str;
use std::sync::{mpsc, Arc};

use tokio::net::{TcpStream, UdpSocket};

use tokio_util::codec::{Framed, LinesCodec};

use tracing::{info, warn};

/// Holos client
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// An internet socket address, either IPv4 or IPv6 for server
    addr: SocketAddr,

    /// Specify the delay between input and output
    #[arg(short, long, default_value_t = 150.0)]
    latency: f32,

    /// Path to the music
    #[arg(short, long)]
    file: Option<String>,
}

fn cpal_callback(data: &[f32], tx: &mpsc::Sender<Vec<f32>>) {
    tx.send(data.to_vec()).unwrap();
}

fn cpal_out_callback(
    data: &mut [f32],
    rx: &mut Consumer<f32, Arc<SharedRb<f32, Vec<MaybeUninit<f32>>>>>,
) {
    for sample in data {
        *sample = match rx.pop() {
            Some(s) => s,
            None => 0.0,
        };
    }
}

fn encode(data: &[f32]) -> Vec<u8> {
    let mut encoded = vec![];
    for i in data {
        encoded.push(i.to_be_bytes());
    }
    encoded.into_iter().flatten().collect()
}

fn decode(data: &[u8]) -> Vec<f32> {
    let mut decoded = vec![];
    for chunk in data.chunks_exact(4) {
        decoded.push(f32::from_be_bytes(chunk.try_into().unwrap()))
    }
    decoded
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt().init();
    let socket = UdpSocket::bind("[::]:0").await?; // for UDP4/6
    info!("UDP binded to {}", socket.local_addr()?);
    let tcp = TcpStream::connect(args.addr).await?;
    info!("TCP connected to {}", tcp.peer_addr()?);
    let mut lines = Framed::new(tcp, LinesCodec::new());
    let greeting = lines.next().await.unwrap()?;
    info!("{}", greeting);
    lines
        .send(std::io::stdin().lock().lines().next().unwrap()?)
        .await?;

    // Read from stdin
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        loop {
            let mut str = String::new();
            std::io::stdin().lock().read_line(&mut str).unwrap();
            tx.send(str).await.unwrap();
        }
    });

    // Configure audio
    let err_fn = move |err| {
        eprintln!("an error occurred on stream: {}", err);
    };
    let host = cpal::default_host();
    let in_device = host.default_input_device().unwrap();
    let in_config: cpal::StreamConfig = in_device.default_input_config()?.into();
    // Create a delay in case the input and output devices aren't synced.
    let latency_frames = (args.latency / 1_000.0) * in_config.sample_rate.0 as f32;
    let latency_samples = latency_frames as usize * in_config.channels as usize;
    // The buffer to share samples
    let ring = HeapRb::<f32>::new(latency_samples * 2);
    let (mut producer, mut consumer) = ring.split();
    // Fill the samples with 0.0 equal to the length of the delay.
    for _ in 0..latency_samples {
        // The ring buffer has twice as much space as necessary to add latency here,
        // so this should never fail
        producer.push(0.0).unwrap();
    }
    let (in_tx, in_rx): (mpsc::Sender<Vec<f32>>, mpsc::Receiver<Vec<f32>>) = mpsc::channel();
    let in_stream = in_device.build_input_stream(
        &in_config,
        move |data, _: _| cpal_callback(data, &in_tx),
        err_fn,
        None,
    )?;

    let out_device = host.default_output_device().unwrap();
    let out_config = out_device.default_input_config()?;
    let out_stream = out_device.build_output_stream(
        &out_config.into(),
        move |data, _: _| cpal_out_callback(data, &mut consumer),
        err_fn,
        None,
    )?;

    // Redirect input sync channel to async input channel
    let (ain_tx, mut ain_rx): (
        tokio::sync::mpsc::UnboundedSender<Vec<f32>>,
        tokio::sync::mpsc::UnboundedReceiver<Vec<f32>>,
    ) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let data = in_rx.recv().unwrap();
            if data.iter().all(|&x| x == 0.0) {
                continue;
            }
            ain_tx.send(data.to_vec()).unwrap();
        }
    });

    in_stream.play()?;
    out_stream.play()?;

    // Main loop
    let mut buf = vec![0u8; 4096];
    loop {
        tokio::select! {
            // Listen UDP and playback
            result = socket.recv_from(&mut buf) => {
                let (amount, _) = result?;
                let mut output_fell_behind = false;
                for sample in decode(&buf[..amount]) {
                    if producer.push(sample).is_err() {
                        output_fell_behind = true;
                    }
                }
                if output_fell_behind {
                    warn!("output stream fell behind: try increasing latency");
                }
            }
            // Send recoreded audio with UDP
            Some(data) = ain_rx.recv() => {
                let encoded = encode(&data);
                for chunk in encoded.chunks(4096) {
                    socket.send_to(chunk, args.addr).await?;
                }
            }
            // Listen TCP
            Some(message) = lines.next() => {
                info!("{}", message?);
            }
            // Listen stdin
            Some(message) = rx.recv() => {
                let message = message.trim().to_string();
                if !message.is_empty() {
                    lines.send(message).await?;
                }
            }
        }
    }
}
