//! Connection to the panel: connect, handshake, then multiplex reports, ping
//! results and SSH events over one WebSocket, reconnecting with backoff.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio_rustls::client::TlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{self, Message};
use vespera_proto::{
    AgentMsg, BasicInfo, ErrorCode, FEATURE_PROBE, FEATURE_UPGRADE, Hello, PROTO_VERSION, PingResult, PingTask,
    ProbeResult, ServerMsg, SshFailSummary, UpgradeOrder, UpgradeResult,
};

use crate::collect::Collector;
use crate::config::Config;
use crate::net::{self, http, tls};
use crate::ping;
use crate::ssh::SshShared;
use crate::update;

const HEARTBEAT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(35);
/// Whole connect: DNS, TCP, TLS and the WebSocket upgrade. Without it a peer
/// that accepts TCP but never answers leaves the agent stuck with no log.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// A write that cannot drain (dead link, full send buffer) ends the session
/// instead of blocking the loop until the kernel gives up on retransmits.
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);
const INFO_REFRESH: Duration = Duration::from_secs(600);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

pub struct Agent {
    pub cfg: Config,
    pub collector: Collector,
    pub ips: watch::Receiver<(Option<String>, Option<String>)>,
    pub tasks_tx: watch::Sender<Vec<PingTask>>,
    pub ping_rx: mpsc::Receiver<PingResult>,
    pub ssh: Option<Arc<SshShared>>,
    pub fail_rx: mpsc::Receiver<SshFailSummary>,
    /// Finished self-update attempts: (version, result).
    pub upgrade_tx: mpsc::Sender<(String, Result<(), String>)>,
    pub upgrade_rx: mpsc::Receiver<(String, Result<(), String>)>,
    pub upgrading: bool,
    pub icmp: ping::Icmp,
    /// Finished one-off tests, on their way back to the panel.
    pub probe_tx: mpsc::Sender<ProbeResult>,
    pub probe_rx: mpsc::Receiver<ProbeResult>,
}

enum End {
    /// Server rejected us in a way that retrying soon will not fix.
    Fatal(String),
    Retry(String),
}

impl Agent {
    pub async fn run(mut self) {
        let mut backoff = Duration::from_secs(1);
        loop {
            let started = Instant::now();
            let end = match self.session().await {
                Ok(end) => end,
                Err(e) => End::Retry(format!("{e:#}")),
            };
            // A session that lived a while resets the backoff.
            if started.elapsed() > Duration::from_secs(60) {
                backoff = Duration::from_secs(1);
            }
            let wait = match end {
                End::Fatal(msg) => {
                    tracing::error!("{msg}; retrying in 60s");
                    Duration::from_secs(60)
                }
                End::Retry(msg) => {
                    tracing::warn!("connection ended: {msg}; reconnecting in {}s", backoff.as_secs());
                    let w = backoff + Duration::from_millis(fastrand::u64(0..1000));
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    w
                }
            };
            tokio::time::sleep(wait).await;
        }
    }

    fn info(&mut self) -> BasicInfo {
        let mut info = self.collector.basic_info();
        let (v4, v6) = self.ips.borrow().clone();
        info.ipv4 = self.cfg.ipv4.clone().or(v4);
        info.ipv6 = self.cfg.ipv6.clone().or(v6);
        info
    }

    async fn session(&mut self) -> Result<End> {
        let mut ws = match tokio::time::timeout(CONNECT_TIMEOUT, connect(&self.cfg)).await {
            Err(_) => {
                bail!("connect timeout after {}s (TLS or WebSocket handshake stalled)", CONNECT_TIMEOUT.as_secs())
            }
            Ok(Ok(ws)) => ws,
            Ok(Err(e)) => {
                if let Some(tungstenite::Error::Http(resp)) = e.downcast_ref::<tungstenite::Error>()
                    && resp.status() == 401
                {
                    return Ok(End::Fatal("panel rejected the token (401)".into()));
                }
                return Err(e);
            }
        };
        send(&mut ws, &AgentMsg::Hello(Hello { proto: PROTO_VERSION, agent_version: crate::VERSION.into() })).await?;

        let welcome = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(msg) = ws.next().await {
                if let Message::Binary(b) = msg? {
                    return Ok::<_, anyhow::Error>(vespera_proto::decode::<ServerMsg>(&b)?);
                }
            }
            bail!("closed during handshake")
        })
        .await
        .context("handshake timeout")??;
        let welcome = match welcome {
            ServerMsg::Welcome(w) => w,
            ServerMsg::Error { code, msg } => return Ok(error_end(code, msg)),
            other => bail!("unexpected handshake frame: {other:?}"),
        };
        tracing::info!(node = %welcome.node_uuid, "connected to panel");
        if let Some(ssh) = &self.ssh {
            ssh.queue.lock().unwrap().on_welcome(welcome.ssh_acked_seq);
        }

        let mut info = self.info();
        send(&mut ws, &AgentMsg::BasicInfo(info.clone())).await?;

        let mut ssh_sent: u64 = 0;
        self.flush_ssh(&mut ws, &mut ssh_sent).await?;

        let mut report_tick = tokio::time::interval(self.cfg.report_interval());
        report_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut heartbeat = tokio::time::interval(HEARTBEAT);
        let mut info_tick = tokio::time::interval(INFO_REFRESH);
        info_tick.tick().await;
        let mut last_rx = Instant::now();
        let mut pings: Vec<PingResult> = Vec::new();

        loop {
            tokio::select! {
                _ = report_tick.tick() => {
                    let r = self.collector.report();
                    send(&mut ws, &AgentMsg::Report(r)).await?;
                }
                Some(r) = self.ping_rx.recv() => {
                    pings.push(r);
                    while let Ok(r) = self.ping_rx.try_recv() {
                        pings.push(r);
                    }
                    send(&mut ws, &AgentMsg::PingResults(std::mem::take(&mut pings))).await?;
                }
                Some(s) = self.fail_rx.recv() => {
                    send(&mut ws, &AgentMsg::SshFailSummary(s)).await?;
                }
                _ = ssh_notified(&self.ssh) => {
                    self.flush_ssh(&mut ws, &mut ssh_sent).await?;
                }
                Ok(()) = self.ips.changed() => {
                    let next = self.info();
                    if next != info {
                        info = next;
                        send(&mut ws, &AgentMsg::BasicInfo(info.clone())).await?;
                    }
                }
                _ = info_tick.tick() => {
                    let next = self.info();
                    if next != info {
                        info = next;
                        send(&mut ws, &AgentMsg::BasicInfo(info.clone())).await?;
                    }
                    // Re-send anything not yet acknowledged.
                    ssh_sent = 0;
                    self.flush_ssh(&mut ws, &mut ssh_sent).await?;
                }
                Some(r) = self.probe_rx.recv() => {
                    send(&mut ws, &AgentMsg::ProbeResult(r)).await?;
                }
                Some((version, result)) = self.upgrade_rx.recv() => {
                    self.upgrading = false;
                    match result {
                        Ok(()) => {
                            tracing::info!(%version, "update installed, restarting");
                            let r = UpgradeResult { version, ok: true, message: "installed, restarting".into() };
                            send(&mut ws, &AgentMsg::UpgradeResult(r)).await?;
                            let _ = tokio::time::timeout(WRITE_TIMEOUT, ws.close(None)).await;
                            let err = update::restart();
                            tracing::error!("{err}");
                            return Ok(End::Retry(err));
                        }
                        Err(message) => {
                            tracing::warn!(%version, "update failed: {message}");
                            send(&mut ws, &AgentMsg::UpgradeResult(UpgradeResult { version, ok: false, message })).await?;
                        }
                    }
                }
                _ = heartbeat.tick() => {
                    if last_rx.elapsed() > READ_TIMEOUT {
                        return Ok(End::Retry("read timeout".into()));
                    }
                    send_frame(&mut ws, Message::Ping(Vec::new().into())).await?;
                }
                msg = ws.next() => {
                    let Some(msg) = msg else { return Ok(End::Retry("closed by panel".into())) };
                    last_rx = Instant::now();
                    match msg? {
                        Message::Binary(b) => match vespera_proto::decode::<ServerMsg>(&b) {
                            Ok(ServerMsg::PingTasks(tasks)) => {
                                tracing::info!(count = tasks.len(), "ping tasks updated");
                                let _ = self.tasks_tx.send(tasks);
                            }
                            Ok(ServerMsg::SshAck { up_to_seq }) => {
                                if let Some(ssh) = &self.ssh {
                                    ssh.queue.lock().unwrap().ack(up_to_seq);
                                }
                            }
                            Ok(ServerMsg::Error { code, msg }) => return Ok(error_end(code, msg)),
                            Ok(ServerMsg::Features(_)) => {
                                let mut ours = vec![FEATURE_PROBE.to_string()];
                                if !self.cfg.disable_auto_update {
                                    ours.push(FEATURE_UPGRADE.to_string());
                                }
                                send(&mut ws, &AgentMsg::Features(ours)).await?;
                            }
                            Ok(ServerMsg::Upgrade(order)) => self.start_upgrade(order, &mut ws).await?,
                            Ok(ServerMsg::Probe(order)) => {
                                let (icmp, tx) = (self.icmp.clone(), self.probe_tx.clone());
                                tokio::spawn(async move {
                                    let _ = tx.send(ping::probe_once(order, &icmp).await).await;
                                });
                            }
                            Ok(ServerMsg::Welcome(_)) => {}
                            // Frames from a newer panel that this agent does not know yet.
                            Err(e) => tracing::debug!("undecodable frame: {e}"),
                        },
                        Message::Close(_) => return Ok(End::Retry("closed by panel".into())),
                        _ => {}
                    }
                }
            }
        }
    }

    async fn start_upgrade(&mut self, order: UpgradeOrder, ws: &mut Ws) -> Result<()> {
        let reply = |ok: bool, message: &str| {
            AgentMsg::UpgradeResult(UpgradeResult { version: order.version.clone(), ok, message: message.into() })
        };
        if self.cfg.disable_auto_update {
            return send(ws, &reply(false, "auto-update is disabled on this agent")).await;
        }
        if order.version == crate::VERSION {
            return send(ws, &reply(true, "already running this version")).await;
        }
        if self.upgrading {
            return Ok(());
        }
        self.upgrading = true;
        let tx = self.upgrade_tx.clone();
        tokio::spawn(async move {
            let result = update::apply(&order).await;
            let _ = tx.send((order.version, result)).await;
        });
        Ok(())
    }

    async fn flush_ssh(&mut self, ws: &mut Ws, sent: &mut u64) -> Result<()> {
        let Some(ssh) = &self.ssh else { return Ok(()) };
        loop {
            let batch = ssh.queue.lock().unwrap().pending_after(*sent, 100);
            let Some(last) = batch.last().map(|e| e.seq) else { return Ok(()) };
            send(ws, &AgentMsg::SshEvents(batch)).await?;
            *sent = last;
        }
    }
}

async fn ssh_notified(ssh: &Option<Arc<SshShared>>) {
    match ssh {
        Some(s) => s.notify.notified().await,
        None => std::future::pending().await,
    }
}

fn error_end(code: ErrorCode, msg: String) -> End {
    match code {
        ErrorCode::BadToken => End::Fatal(format!("panel rejected the token: {msg}")),
        ErrorCode::ProtoMismatch => End::Fatal(format!("protocol version mismatch, upgrade the agent: {msg}")),
        ErrorCode::Replaced => End::Retry(format!("replaced by another connection: {msg}")),
        ErrorCode::Internal => End::Retry(format!("panel error: {msg}")),
    }
}

type Ws = WebSocketStream<Stream>;

async fn send(ws: &mut Ws, msg: &AgentMsg) -> Result<()> {
    send_frame(ws, Message::Binary(vespera_proto::encode(msg).into())).await
}

async fn send_frame(ws: &mut Ws, msg: Message) -> Result<()> {
    tokio::time::timeout(WRITE_TIMEOUT, ws.send(msg)).await.map_err(|_| anyhow!("write timeout"))??;
    Ok(())
}

async fn connect(cfg: &Config) -> Result<Ws> {
    let url = cfg.ws_url();
    tracing::debug!(%url, "connecting to panel");
    let secure = url.starts_with("wss://");
    let parsed = http::parse_url(&url.replacen("wss://", "https://", 1).replacen("ws://", "http://", 1))
        .ok_or_else(|| anyhow!("invalid endpoint {url}"))?;
    let tcp = net::connect(&parsed.host, parsed.port, None, Duration::from_secs(10))
        .await
        .with_context(|| format!("connect {}:{}", parsed.host, parsed.port))?;
    let stream = if secure {
        let name = tls::server_name(&parsed.host)?;
        Stream::Tls(Box::new(tls::connector(cfg.insecure).connect(name, tcp).await.context("tls handshake")?))
    } else {
        Stream::Plain(tcp)
    };
    let mut req = url.as_str().into_client_request()?;
    let h = req.headers_mut();
    h.insert("Authorization", format!("Bearer {}", cfg.token.trim()).parse()?);
    h.insert("User-Agent", format!("vespera-agent/{}", crate::VERSION).parse()?);
    let mut ws_cfg = WebSocketConfig::default();
    ws_cfg.max_message_size = Some(4 << 20);
    let (ws, _) = tokio_tungstenite::client_async_with_config(req, stream, Some(ws_cfg)).await?;
    Ok(ws)
}

pub enum Stream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl AsyncRead for Stream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_flush(cx),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}
