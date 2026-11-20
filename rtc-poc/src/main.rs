//! Manual-configured WebRTC DataChannel route probe for Windows ↔ iPhone Safari.
//! No sync data or persistent files are transferred by this probe.

use std::{io::{BufRead, BufReader, Write}, net::UdpSocket, sync::Arc, time::Instant};

use anyhow::{bail, Context, Result};
use bytes::BytesMut;
use futures_util::{SinkExt, StreamExt};
use p2p_sync_core::channel_stream::bridge;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use webrtc::{
    data_channel::{DataChannel, DataChannelEvent},
    peer_connection::{
        MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
        RTCConfigurationBuilder, RTCIceCandidateInit, RTCIceServer, RTCSessionDescription,
        RTCPeerConnectionIceEvent, RTCPeerConnectionState, RTCStatsReportEntry, Registry,
        StatsSelector,
        register_default_interceptors,
    },
    runtime::{default_runtime, Runtime},
};

struct Handler {
    remote: String,
    candidate_tx: mpsc::UnboundedSender<Value>,
    channel_tx: mpsc::UnboundedSender<Arc<dyn DataChannel>>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        if let Ok(candidate) = event.candidate.to_json() {
            let _ = self.candidate_tx.send(json!({
                "type": "candidate", "to": self.remote, "payload": candidate,
            }));
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        println!("WebRTC connection: {state}");
    }

    async fn on_data_channel(&self, channel: Arc<dyn DataChannel>) {
        let _ = self.channel_tx.send(channel);
    }
}

fn local_ip() -> Result<std::net::IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.connect("1.1.1.1:80")?;
    Ok(socket.local_addr()?.ip())
}

async fn peer_connection(
    runtime: Arc<dyn Runtime>,
    remote: &str,
    turn: Option<(&str, &str)>,
    candidate_tx: mpsc::UnboundedSender<Value>,
    channel_tx: mpsc::UnboundedSender<Arc<dyn DataChannel>>,
) -> Result<Box<dyn PeerConnection>> {
    let mut servers = vec![RTCIceServer {
        urls: vec!["stun:stun.cloudflare.com:3478".into()],
        ..Default::default()
    }];
    if let Some((username, credential)) = turn {
        servers.push(RTCIceServer {
            urls: vec![
                "turn:turn.cloudflare.com:3478?transport=udp".into(),
                "turn:turn.cloudflare.com:443?transport=udp".into(),
                "turn:turn.cloudflare.com:3478?transport=tcp".into(),
                "turns:turn.cloudflare.com:5349?transport=tcp".into(),
            ],
            username: username.into(),
            credential: credential.into(),
        });
    }
    let mut media = MediaEngine::default();
    media.register_default_codecs()?;
    let registry = register_default_interceptors(Registry::new(), &mut media)?;
    let pc = PeerConnectionBuilder::new()
        .with_configuration(RTCConfigurationBuilder::new().with_ice_servers(servers).build())
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .with_handler(Arc::new(Handler {
            remote: remote.into(), candidate_tx, channel_tx,
        }))
        .with_runtime(runtime)
        .with_udp_addrs(vec![format!("{}:0", local_ip()?)])
        .build()
        .await?;
    Ok(Box::new(pc))
}

fn run_channel(channel: Arc<dyn DataChannel>, pong_tx: mpsc::UnboundedSender<()>) {
    tokio::spawn(async move {
        loop {
            match channel.poll().await {
                Some(DataChannelEvent::OnOpen) => break,
                Some(DataChannelEvent::OnClose) | None => return,
                _ => {}
            }
        }
        println!("DataChannel open; sending stream ping");
        let (mut stream, ingress, egress) = bridge();
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        tokio::task::spawn_blocking(move || {
            while let Some(frame) = egress.next() {
                if outbound_tx.send(frame).is_err() { break; }
            }
        });
        tokio::task::spawn_blocking(move || {
            if writeln!(stream, "ping:{}", now_ms()).is_err() { return; }
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if let Some(stamp) = line.trim_end().strip_prefix("ping:") {
                            let _ = writeln!(reader.get_mut(), "pong:{stamp}");
                        } else if let Some(stamp) = line.trim_end().strip_prefix("pong:") {
                            if let Ok(sent) = stamp.parse::<u128>() {
                                println!("DataChannel pong: {} ms", now_ms().saturating_sub(sent));
                                let _ = pong_tx.send(());
                            }
                        }
                    }
                }
            }
        });
        loop {
            tokio::select! {
                frame = outbound_rx.recv() => {
                    let Some(frame) = frame else { break };
                    if channel.send(BytesMut::from(frame.as_slice())).await.is_err() { break; }
                }
                event = channel.poll() => match event {
                    Some(DataChannelEvent::OnMessage(message)) => {
                        loop {
                            match ingress.try_receive(&message.data) {
                                Ok(()) => break,
                                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                                }
                                Err(_) => return,
                            }
                        }
                    }
                    Some(DataChannelEvent::OnClose) | None => break,
                    _ => {}
                }
            }
        }
        println!("DataChannel closed");
    });
}

async fn report_route(connection: &dyn PeerConnection) {
    let report = connection.get_stats(Instant::now(), StatsSelector::None).await;
    for entry in report.iter() {
        let RTCStatsReportEntry::Transport(transport) = entry else { continue };
        let Some(RTCStatsReportEntry::IceCandidatePair(pair)) = report.get(&transport.selected_candidate_pair_id) else { continue };
        let local = report.get(&pair.local_candidate_id);
        let remote = report.get(&pair.remote_candidate_id);
        if let (Some(RTCStatsReportEntry::LocalCandidate(local)), Some(RTCStatsReportEntry::RemoteCandidate(remote))) = (local, remote) {
            println!("Selected route: {:?} -> {:?}; bytes sent={} received={}",
                local.candidate_type, remote.candidate_type, transport.bytes_sent, transport.bytes_received);
        }
    }
}

fn now_ms() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default().as_millis()
}

fn signal_text(kind: &str, remote: &str, payload: Value) -> Value {
    json!({ "type": kind, "to": remote, "payload": payload })
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 && args.len() != 5 {
        bail!("usage: p2p-rtc-poc wss://worker.example/signal ROOM_HEX [TURN_USER TURN_PASSWORD]");
    }
    let signal = &args[1];
    let room = &args[2];
    let local_test = signal.starts_with("ws://127.0.0.1:") || signal.starts_with("ws://localhost:");
    if !(signal.starts_with("wss://") || local_test) || !signal.ends_with("/signal") ||
        room.len() != 32 || !room.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("expected a wss://.../signal URL and a 32-digit hex room");
    }
    let turn = (args.len() == 5).then(|| (args[3].as_str(), args[4].as_str()));
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let me: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let url = format!("{signal}?room={}&peer={me}", room.to_ascii_lowercase());
    let (websocket, _) = connect_async(&url).await.context("signaling connection")?;
    let (mut outgoing, mut incoming) = websocket.split();
    println!("Waiting in room; local peer {}", &me[..8]);
    let (candidate_tx, mut candidate_rx) = mpsc::unbounded_channel::<Value>();
    let (channel_tx, mut channel_rx) = mpsc::unbounded_channel::<Arc<dyn DataChannel>>();
    let (pong_tx, mut pong_rx) = mpsc::unbounded_channel::<()>();
    let runtime = default_runtime().context("WebRTC runtime unavailable")?;
    let mut remote: Option<String> = None;
    let mut pc: Option<Box<dyn PeerConnection>> = None;
    let mut pending = Vec::<RTCIceCandidateInit>::new();

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            Some(channel) = channel_rx.recv() => run_channel(channel, pong_tx.clone()),
            Some(()) = pong_rx.recv() => {
                if let Some(conn) = pc.as_ref() { report_route(conn.as_ref()).await; }
            }
            Some(candidate) = candidate_rx.recv() => {
                outgoing.send(Message::Text(candidate.to_string().into())).await?;
            }
            frame = incoming.next() => {
                let Some(frame) = frame else { break };
                let frame = frame?;
                if !frame.is_text() { continue; }
                let message: Value = serde_json::from_str(frame.to_text()?)?;
                let kind = message["type"].as_str().unwrap_or_default();
                let from = message["from"].as_str()
                    .or_else(|| message["peer"].as_str()).unwrap_or_default();
                let possible = if kind == "peers" {
                    message["peers"].as_array().and_then(|list| list.iter().filter_map(Value::as_str).next()).unwrap_or_default()
                } else { from };
                if remote.is_none() && !possible.is_empty() && possible != me {
                    remote = Some(possible.to_owned());
                    pc = Some(peer_connection(runtime.clone(), possible, turn, candidate_tx.clone(), channel_tx.clone()).await?);
                    if me.as_str() < possible {
                        let conn = pc.as_ref().unwrap();
                        let channel = conn.create_data_channel("p2p-diagnostic", None).await?;
                        run_channel(channel, pong_tx.clone());
                        conn.set_local_description(conn.create_offer(None).await?).await?;
                        let offer = conn.local_description().await.context("missing local offer")?;
                        outgoing.send(Message::Text(signal_text("offer", possible, serde_json::to_value(offer)?).to_string().into())).await?;
                    }
                }
                if remote.as_deref() != Some(from) { continue; }
                let conn = pc.as_ref().unwrap();
                match kind {
                    "offer" => {
                        let offer: RTCSessionDescription = serde_json::from_value(message["payload"].clone())?;
                        conn.set_remote_description(offer).await?;
                        for candidate in pending.drain(..) { conn.add_ice_candidate(candidate).await?; }
                        conn.set_local_description(conn.create_answer(None).await?).await?;
                        let answer = conn.local_description().await.context("missing local answer")?;
                        outgoing.send(Message::Text(signal_text("answer", from, serde_json::to_value(answer)?).to_string().into())).await?;
                    }
                    "answer" => {
                        let answer: RTCSessionDescription = serde_json::from_value(message["payload"].clone())?;
                        conn.set_remote_description(answer).await?;
                        for candidate in pending.drain(..) { conn.add_ice_candidate(candidate).await?; }
                    }
                    "candidate" => {
                        let candidate: RTCIceCandidateInit = serde_json::from_value(message["payload"].clone())?;
                        if conn.remote_description().await.is_some() { conn.add_ice_candidate(candidate).await?; }
                        else { pending.push(candidate); }
                    }
                    _ => {}
                }
            }
        }
    }
    if let Some(conn) = pc { conn.close().await?; }
    Ok(())
}
