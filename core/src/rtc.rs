//! Manually configured WebRTC route for the existing TLS sync protocol.
//! The route never handles credentials or file contents beyond opaque bytes.

use std::{fs, net::{SocketAddr, UdpSocket}, path::Path, sync::Arc, time::{Duration, Instant}};

use anyhow::{bail, Context, Result};
use bytes::BytesMut;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, net::{TcpListener, TcpStream}, sync::mpsc};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use webrtc::{
    data_channel::{DataChannel, DataChannelEvent},
    peer_connection::{
        MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
        RTCConfigurationBuilder, RTCIceCandidateInit, RTCIceServer, RTCSessionDescription,
        RTCPeerConnectionIceEvent, RTCIceTransportPolicy, RTCStatsReportEntry, Registry, StatsSelector,
        register_default_interceptors,
    },
    runtime::{default_runtime, Runtime},
};

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BridgeMode { Connect, Listen }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeConfig {
    pub signal_url: String,
    pub room: String,
    pub local_tcp: SocketAddr,
    pub mode: BridgeMode,
    pub turn_username: Option<String>,
    pub turn_password: Option<String>,
    pub relay_byte_limit: Option<u64>,
    #[serde(default)]
    test_turn_url: Option<String>,
    #[serde(default)]
    test_relay_only: bool,
}

impl BridgeConfig {
    fn validate(&self) -> Result<()> {
        let local_test = self.signal_url.starts_with("ws://127.0.0.1:")
            || self.signal_url.starts_with("ws://localhost:");
        if !(self.signal_url.starts_with("wss://") || local_test)
            || !self.signal_url.ends_with("/signal")
            || self.room.len() != 32
            || !self.room.bytes().all(|byte| byte.is_ascii_hexdigit())
            || !self.local_tcp.ip().is_loopback()
            || self.local_tcp.port() == 0
            || self.turn_username.is_some() != self.turn_password.is_some()
            || self.turn_username.is_some() && self.relay_byte_limit.unwrap_or(0) == 0
            || self.test_turn_url.as_ref().is_some_and(|url|
                !local_test || !(url.starts_with("turn:127.0.0.1:")
                    || url.starts_with("turn:localhost:")))
            || self.test_relay_only && (!local_test || self.test_turn_url.is_none()
                || self.turn_username.is_none())
        { bail!("invalid WebRTC bridge configuration"); }
        Ok(())
    }

    fn ice_servers(&self) -> Vec<RTCIceServer> {
        let mut servers = vec![RTCIceServer {
            urls: vec!["stun:stun.cloudflare.com:3478".into()],
            ..Default::default()
        }];
        if let (Some(username), Some(credential)) = (&self.turn_username, &self.turn_password) {
            servers.push(RTCIceServer {
                urls: self.test_turn_url.as_ref().map(|url| vec![url.clone()]).unwrap_or_else(|| vec![
                    "turn:turn.cloudflare.com:3478?transport=udp".into(),
                    "turn:turn.cloudflare.com:443?transport=udp".into(),
                    "turn:turn.cloudflare.com:3478?transport=tcp".into(),
                    "turn:turn.cloudflare.com:80?transport=tcp".into(),
                    "turns:turn.cloudflare.com:5349?transport=tcp".into(),
                    "turns:turn.cloudflare.com:443?transport=tcp".into(),
                ]),
                username: username.clone(), credential: credential.clone(),
            });
        }
        servers
    }
}

struct Handler {
    remote: String,
    candidates: mpsc::UnboundedSender<Value>,
    channels: mpsc::UnboundedSender<Arc<dyn DataChannel>>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        if let Ok(candidate) = event.candidate.to_json() {
            let _ = self.candidates.send(json!({
                "type": "candidate", "to": self.remote, "payload": candidate,
            }));
        }
    }

    async fn on_data_channel(&self, channel: Arc<dyn DataChannel>) {
        let _ = self.channels.send(channel);
    }
}

fn local_udp_addresses() -> Result<Vec<String>> {
    let mut addresses = Vec::new();
    for (bind, target) in [
        ("0.0.0.0:0", "1.1.1.1:80"),
        ("[::]:0", "[2606:4700:4700::1111]:80"),
    ] {
        if let Ok(socket) = UdpSocket::bind(bind) {
            if socket.connect(target).is_ok() {
                if let Ok(address) = socket.local_addr() {
                    if !address.ip().is_unspecified() {
                        addresses.push(SocketAddr::new(address.ip(), 0).to_string());
                    }
                }
            }
        }
    }
    if addresses.is_empty() { bail!("no usable local network interface"); }
    Ok(addresses)
}

async fn peer_connection(
    config: &BridgeConfig,
    runtime: Arc<dyn Runtime>,
    remote: &str,
    candidates: mpsc::UnboundedSender<Value>,
    channels: mpsc::UnboundedSender<Arc<dyn DataChannel>>,
) -> Result<Box<dyn PeerConnection>> {
    let mut media = MediaEngine::default();
    media.register_default_codecs()?;
    let registry = register_default_interceptors(Registry::new(), &mut media)?;
    let mut ice_configuration = RTCConfigurationBuilder::new()
        .with_ice_servers(config.ice_servers());
    if config.test_relay_only {
        ice_configuration = ice_configuration.with_ice_transport_policy(RTCIceTransportPolicy::Relay);
    }
    let connection = PeerConnectionBuilder::new()
        .with_configuration(ice_configuration.build())
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .with_handler(Arc::new(Handler {
            remote: remote.into(), candidates, channels,
        }))
        .with_runtime(runtime)
        .with_udp_addrs(local_udp_addresses()?)
        .with_data_channel_send_buffer_limit(1024 * 1024)
        .build().await?;
    Ok(Box::new(connection))
}

async fn tunnel(channel: Arc<dyn DataChannel>, mode: BridgeMode,
    address: SocketAddr, status: mpsc::UnboundedSender<&'static str>) -> Result<()> {
    loop {
        match channel.poll().await {
            Some(DataChannelEvent::OnOpen) => break,
            Some(DataChannelEvent::OnClose) | None => bail!("DataChannel closed before opening"),
            _ => {}
        }
    }
    let _ = status.send("datachannel");
    let tcp = match mode {
        BridgeMode::Connect => TcpStream::connect(address).await?,
        BridgeMode::Listen => {
            let listener = TcpListener::bind(address).await?;
            let _ = status.send("ready");
            listener.accept().await?.0
        }
    };
    if matches!(mode, BridgeMode::Connect) { let _ = status.send("ready"); }
    let (mut reader, mut writer) = tcp.into_split();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        tokio::select! {
            read = reader.read(&mut buffer) => match read {
                Ok(0) => break,
                Ok(size) => channel.send(BytesMut::from(&buffer[..size])).await?,
                Err(error) => return Err(error.into()),
            },
            event = channel.poll() => match event {
                Some(DataChannelEvent::OnMessage(message)) => writer.write_all(&message.data).await?,
                Some(DataChannelEvent::OnClose) | None => break,
                _ => {},
            }
        }
    }
    let _ = status.send("closed");
    Ok(())
}

pub fn run(configuration: &Path, cancelled: &dyn Fn() -> bool,
    status: &dyn Fn(&str)) -> Result<()> {
    let bytes = fs::read(configuration)?;
    if bytes.len() > 4096 { bail!("WebRTC configuration too large"); }
    let config: BridgeConfig = serde_json::from_slice(&bytes)?;
    config.validate()?;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let mut relay_bytes_total = 0u64;
    loop {
        if cancelled() { return Ok(()); }
        if config.relay_byte_limit.is_some_and(|limit| relay_bytes_total >= limit) {
            status("relay-limit");
            return Ok(());
        }
        match runtime.block_on(run_async(&config, &mut relay_bytes_total, cancelled, status)) {
            Ok(false) => return Ok(()), // The configured relay limit is terminal.
            Ok(true) | Err(_) if cancelled() => return Ok(()),
            Ok(true) | Err(_) => status("reconnecting"),
        }
        for _ in 0..4 {
            if cancelled() { return Ok(()); }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

async fn run_async(config: &BridgeConfig, relay_bytes_total: &mut u64,
    cancelled: &dyn Fn() -> bool,
    status: &dyn Fn(&str)) -> Result<bool> {
    let mode = config.mode;
    let local_tcp = config.local_tcp;
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let me: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let url = format!("{}?room={}&peer={me}", config.signal_url, config.room.to_ascii_lowercase());
    let (websocket, _) = connect_async(&url).await.context("WebRTC signaling")?;
    let (mut outgoing, mut incoming) = websocket.split();
    status("signaling");
    let (candidate_tx, mut candidate_rx) = mpsc::unbounded_channel::<Value>();
    let (channel_tx, mut channel_rx) = mpsc::unbounded_channel::<Arc<dyn DataChannel>>();
    let (status_tx, mut status_rx) = mpsc::unbounded_channel::<&'static str>();
    let runtime = default_runtime().context("WebRTC runtime unavailable")?;
    let mut remote: Option<String> = None;
    let mut pc: Option<Box<dyn PeerConnection>> = None;
    let mut pending = Vec::<RTCIceCandidateInit>::new();
    let mut timer = tokio::time::interval(Duration::from_millis(250));
    let mut stats_ticks = 0u8;
    let mut report_ticks = 0u8;
    let mut reported_route = None;
    let mut relay_limit_reached = false;
    let mut last_relay_bytes = 0u64;
    let mut tunnels = Vec::new();
    loop {
        tokio::select! {
            _ = timer.tick() => {
                if cancelled() { break; }
                stats_ticks += 1;
                if stats_ticks < 4 { continue; }
                stats_ticks = 0;
                if let Some(connection) = pc.as_ref() {
                    if let Some((relay, sent, received)) = selected_route(connection.as_ref()).await {
                        count_relay_bytes(relay_bytes_total, &mut last_relay_bytes, relay, sent, received);
                        report_ticks += 1;
                        if reported_route != Some(relay) || report_ticks >= 4 {
                            report_route(status, relay, sent, received, *relay_bytes_total);
                            reported_route = Some(relay);
                            report_ticks = 0;
                        }
                        if relay && config.relay_byte_limit.is_some_and(|limit| *relay_bytes_total >= limit) {
                            status("relay-limit");
                            relay_limit_reached = true;
                            break;
                        }
                    }
                }
            },
            Some(text) = status_rx.recv() => {
                if text == "failed" { break; }
                status(text);
                if text == "closed" { break; }
                if text == "ready" {
                    if let Some(connection) = pc.as_ref() {
                        if let Some((relay, sent, received)) = selected_route(connection.as_ref()).await {
                            count_relay_bytes(relay_bytes_total, &mut last_relay_bytes, relay, sent, received);
                            report_route(status, relay, sent, received, *relay_bytes_total);
                            reported_route = Some(relay);
                            report_ticks = 0;
                            if relay && config.relay_byte_limit.is_some_and(|limit| *relay_bytes_total >= limit) {
                                status("relay-limit");
                                relay_limit_reached = true;
                                break;
                            }
                        }
                    }
                }
            },
            Some(channel) = channel_rx.recv() => {
                let sender = status_tx.clone();
                tunnels.push(tokio::spawn(async move {
                    if tunnel(channel, mode, local_tcp, sender.clone()).await.is_err() {
                        let _ = sender.send("failed");
                    }
                }));
            },
            Some(candidate) = candidate_rx.recv() => {
                outgoing.send(Message::Text(candidate.to_string().into())).await?;
            },
            frame = incoming.next() => {
                let Some(frame) = frame else { break };
                let frame = frame?;
                if !frame.is_text() { continue; }
                let message: Value = serde_json::from_str(frame.to_text()?)?;
                let kind = message["type"].as_str().unwrap_or_default();
                let from = message["from"].as_str()
                    .or_else(|| message["peer"].as_str()).unwrap_or_default();
                let possible = if kind == "peers" {
                    message["peers"].as_array()
                        .and_then(|list| list.iter().filter_map(Value::as_str).next())
                        .unwrap_or_default()
                } else { from };
                if kind == "left" && remote.as_deref() == Some(from) { break; }
                if remote.is_none() && !possible.is_empty() && possible != me {
                    remote = Some(possible.to_owned());
                    pc = Some(peer_connection(&config, runtime.clone(), possible,
                        candidate_tx.clone(), channel_tx.clone()).await?);
                    if me.as_str() < possible {
                        let conn = pc.as_ref().unwrap();
                        let channel = conn.create_data_channel("p2p-sync", None).await?;
                        let sender = status_tx.clone();
                        tunnels.push(tokio::spawn(async move {
                            if tunnel(channel, mode, local_tcp, sender.clone()).await.is_err() {
                                let _ = sender.send("failed");
                            }
                        }));
                        conn.set_local_description(conn.create_offer(None).await?).await?;
                        let offer = conn.local_description().await.context("missing local offer")?;
                        outgoing.send(Message::Text(json!({"type":"offer","to":possible,"payload":offer}).to_string().into())).await?;
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
                        outgoing.send(Message::Text(json!({"type":"answer","to":from,"payload":answer}).to_string().into())).await?;
                    },
                    "answer" => {
                        let answer: RTCSessionDescription = serde_json::from_value(message["payload"].clone())?;
                        conn.set_remote_description(answer).await?;
                        for candidate in pending.drain(..) { conn.add_ice_candidate(candidate).await?; }
                    },
                    "candidate" => {
                        let candidate: RTCIceCandidateInit = serde_json::from_value(message["payload"].clone())?;
                        if conn.remote_description().await.is_some() { conn.add_ice_candidate(candidate).await?; }
                        else { pending.push(candidate); }
                    },
                    _ => {},
                }
            },
        }
    }
    if let Some(connection) = pc.as_ref() {
        if let Some((relay, sent, received)) = selected_route(connection.as_ref()).await {
            count_relay_bytes(relay_bytes_total, &mut last_relay_bytes, relay, sent, received);
            report_route(status, relay, sent, received, *relay_bytes_total);
        }
    }
    if !relay_limit_reached && config.relay_byte_limit.is_some_and(|limit| *relay_bytes_total >= limit) {
        status("relay-limit");
        relay_limit_reached = true;
    }
    for tunnel in tunnels {
        tunnel.abort();
        let _ = tunnel.await;
    }
    if let Some(conn) = pc { conn.close().await?; }
    status("closed");
    Ok(!relay_limit_reached)
}

fn count_relay_bytes(total: &mut u64, last: &mut u64,
    relay: bool, sent: u64, received: u64) {
    if relay {
        let current = sent.saturating_add(received);
        *total = total.saturating_add(current.saturating_sub(*last));
        *last = current;
    }
}

fn report_route(status: &dyn Fn(&str), relay: bool, sent: u64, received: u64, relay_total: u64) {
    status(&format!("route:{} sent={sent} received={received} relay_total={relay_total}",
        if relay { "relay" } else { "direct" }));
}

async fn selected_route(connection: &dyn PeerConnection) -> Option<(bool, u64, u64)> {
    let report = connection.get_stats(Instant::now(), StatsSelector::None).await;
    for entry in report.iter() {
        let RTCStatsReportEntry::Transport(transport) = entry else { continue };
        let Some(RTCStatsReportEntry::IceCandidatePair(pair)) =
            report.get(&transport.selected_candidate_pair_id) else { continue };
        let (Some(RTCStatsReportEntry::LocalCandidate(local)),
             Some(RTCStatsReportEntry::RemoteCandidate(remote))) =
            (report.get(&format!("RTCLocalIceCandidate_{}", pair.local_candidate_id)),
             report.get(&format!("RTCRemoteIceCandidate_{}", pair.remote_candidate_id)))
        else { continue };
        let relay = format!("{:?}", local.candidate_type) == "Relay"
            || format!("{:?}", remote.candidate_type) == "Relay";
        return Some((relay, transport.bytes_sent, transport.bytes_received));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_remote_tcp_and_incomplete_turn_credentials() {
        let mut config = BridgeConfig {
            signal_url: "wss://example.com/signal".into(),
            room: "0123456789abcdef0123456789abcdef".into(),
            local_tcp: "127.0.0.1:8765".parse().unwrap(),
            mode: BridgeMode::Connect,
            turn_username: None,
            turn_password: None,
            relay_byte_limit: None,
            test_turn_url: None,
            test_relay_only: false,
        };
        assert!(config.validate().is_ok());
        config.local_tcp = "192.0.2.1:8765".parse().unwrap();
        assert!(config.validate().is_err());
        config.local_tcp = "127.0.0.1:8765".parse().unwrap();
        config.turn_username = Some("short-lived".into());
        assert!(config.validate().is_err());
        config.turn_password = Some("secret".into());
        assert!(config.validate().is_err());
        config.relay_byte_limit = Some(10_000);
        assert!(config.validate().is_ok());
        config.test_turn_url = Some("turn:127.0.0.1:3478?transport=udp".into());
        config.test_relay_only = true;
        assert!(config.validate().is_err());
        config.signal_url = "ws://127.0.0.1:8787/signal".into();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn relay_limit_accumulates_across_connections() {
        let mut total = 0;
        let mut last = 0;
        count_relay_bytes(&mut total, &mut last, true, 40, 20);
        count_relay_bytes(&mut total, &mut last, true, 70, 30);
        assert_eq!(total, 100);
        last = 0; // A new ICE transport starts its byte counters at zero.
        count_relay_bytes(&mut total, &mut last, true, 20, 10);
        assert_eq!(total, 130);
        count_relay_bytes(&mut total, &mut last, false, 1000, 1000);
        assert_eq!(total, 130);
    }
}
