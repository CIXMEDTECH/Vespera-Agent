//! Vespera agent <-> server wire protocol.
//!
//! Frames are MessagePack-encoded (compact / array form) and sent as binary
//! WebSocket messages on `/api/agent/ws`. The agent authenticates with an
//! `Authorization: Bearer <token>` header on the upgrade request.
//!
//! Compatibility rules (structs are encoded as fixed-length arrays, and a
//! decoder rejects arrays with *more* elements than it knows about):
//! - Never change an existing struct. Not even appending a field is safe: an
//!   older peer would fail to decode the longer array.
//! - New data travels in new enum variants (encoded by name). Receivers ignore
//!   frames they cannot decode instead of dropping the connection.
//! - Only send a new variant after the peer announced support for it via
//!   [`ServerMsg::Features`] / [`AgentMsg::Features`] (see the `FEATURE_*`
//!   constants). Feature lists are plain strings, so they extend freely.
//! - Anything else is a breaking change and must bump [`PROTO_VERSION`].
//!
//! This crate is the source of truth in the `Vespera` repository; the agent
//! repository keeps a synced copy (see `scripts/sync-proto.sh` there).

use serde::{Deserialize, Serialize};

/// Protocol major version. Peers with a different value refuse to talk.
pub const PROTO_VERSION: u16 = 1;

/// Path of the agent WebSocket endpoint on the server.
pub const AGENT_WS_PATH: &str = "/api/agent/ws";

/// Self-update: server may send [`ServerMsg::Upgrade`], agent answers with
/// [`AgentMsg::UpgradeResult`]. An agent started with auto-update disabled
/// does not announce it.
pub const FEATURE_UPGRADE: &str = "upgrade";

/// One-off latency test: server may send [`ServerMsg::Probe`], agent answers
/// with [`AgentMsg::ProbeResult`].
pub const FEATURE_PROBE: &str = "probe";

// ---------------------------------------------------------------------------
// Agent -> Server
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentMsg {
    Hello(Hello),
    BasicInfo(BasicInfo),
    Report(Report),
    PingResults(Vec<PingResult>),
    SshEvents(Vec<SshEvent>),
    SshFailSummary(SshFailSummary),
    /// Features the agent supports, sent in reply to [`ServerMsg::Features`].
    Features(Vec<String>),
    /// Outcome of an [`ServerMsg::Upgrade`] order.
    UpgradeResult(UpgradeResult),
    /// Outcome of a [`ServerMsg::Probe`] order.
    ProbeResult(ProbeResult),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub proto: u16,
    pub agent_version: String,
}

/// Static-ish host information, sent on connect and whenever it changes.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BasicInfo {
    pub cpu_name: String,
    pub cpu_cores: u32,
    pub arch: String,
    pub os: String,
    pub kernel: String,
    pub virtualization: String,
    pub mem_total: u64,
    pub swap_total: u64,
    pub disk_total: u64,
    pub ipv4: Option<String>,
    pub ipv6: Option<String>,
    /// Unix seconds.
    pub boot_time: u64,
}

/// Realtime sample, sent every report interval.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct Report {
    /// Unix milliseconds on the agent.
    pub ts_ms: u64,
    /// 0..=100
    pub cpu: f32,
    pub mem_used: u64,
    pub mem_total: u64,
    pub swap_used: u64,
    pub swap_total: u64,
    pub disk_used: u64,
    pub disk_total: u64,
    /// Bytes per second.
    pub net_rx: u64,
    /// Bytes per second.
    pub net_tx: u64,
    /// Bytes since boot (counter, may reset on reboot).
    pub net_rx_total: u64,
    /// Bytes since boot (counter, may reset on reboot).
    pub net_tx_total: u64,
    pub load1: f32,
    pub load5: f32,
    pub load15: f32,
    pub tcp: u32,
    pub udp: u32,
    pub procs: u32,
    /// Seconds.
    pub uptime: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PingResult {
    pub task_id: i64,
    /// Unix seconds.
    pub ts: i64,
    /// Round trip in microseconds; `None` means lost / failed.
    pub rtt_us: Option<u32>,
}

/// A successful SSH login observed by the agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SshEvent {
    /// Monotonic per-agent sequence number, used for ack and de-duplication.
    pub seq: u64,
    /// Unix seconds.
    pub ts: i64,
    pub user: String,
    pub ip: String,
    pub port: u16,
    /// `password`, `publickey`, `keyboard-interactive/pam`, ...
    pub method: String,
    /// Key fingerprint for publickey logins, e.g. `SHA256:...`.
    pub key_fp: Option<String>,
}

/// Aggregated failed SSH attempts over a window.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SshFailSummary {
    /// Unix seconds.
    pub window_start: i64,
    pub window_secs: u32,
    pub count: u32,
    /// Top source IPs with their failure counts, descending.
    pub top_ips: Vec<(String, u32)>,
}

// ---------------------------------------------------------------------------
// Server -> Agent
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMsg {
    Welcome(Welcome),
    PingTasks(Vec<PingTask>),
    SshAck {
        up_to_seq: u64,
    },
    Error {
        code: ErrorCode,
        msg: String,
    },
    /// Features the server supports, sent right after [`ServerMsg::Welcome`].
    Features(Vec<String>),
    /// Ask the agent to replace itself with another release.
    Upgrade(UpgradeOrder),
    /// Measure a target once, outside the scheduled ping tasks.
    Probe(ProbeOrder),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpgradeOrder {
    /// Release version without the leading `v`, e.g. `0.1.0-beta.2`.
    pub version: String,
    /// Download directory; the agent appends its asset name, e.g.
    /// `https://github.com/CIXMEDTECH/Vespera-Agent/releases/download/v0.1.0-beta.2`.
    pub base_url: String,
    /// `(asset name, sha256 hex)` for every published binary.
    pub checksums: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpgradeResult {
    pub version: String,
    pub ok: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProbeOrder {
    /// Echoed in the [`ProbeResult`].
    pub id: u64,
    pub kind: PingKind,
    /// icmp: host; tcp: host:port; http: URL.
    pub target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProbeResult {
    pub id: u64,
    /// Round trip in microseconds; `None` means lost / failed.
    pub rtt_us: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Welcome {
    pub node_uuid: String,
    /// Unix seconds on the server.
    pub server_time: i64,
    /// Highest SSH event sequence the server has already stored for this node.
    pub ssh_acked_seq: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ErrorCode {
    BadToken,
    ProtoMismatch,
    /// Another connection for the same node took over.
    Replaced,
    Internal,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum PingKind {
    Icmp,
    Tcp,
    Http,
}

impl PingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PingKind::Icmp => "icmp",
            PingKind::Tcp => "tcp",
            PingKind::Http => "http",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "icmp" => Some(PingKind::Icmp),
            "tcp" => Some(PingKind::Tcp),
            "http" => Some(PingKind::Http),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct PingTask {
    pub id: i64,
    pub kind: PingKind,
    /// icmp: host; tcp: host:port; http: URL.
    pub target: String,
    pub interval_s: u32,
}

// ---------------------------------------------------------------------------
// Codec
// ---------------------------------------------------------------------------

pub type DecodeError = rmp_serde::decode::Error;

pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    // Serializing these plain data types cannot fail.
    rmp_serde::to_vec(msg).expect("msgpack encode")
}

pub fn decode<'a, T: Deserialize<'a>>(buf: &'a [u8]) -> Result<T, DecodeError> {
    rmp_serde::from_slice(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_agent_msgs() {
        let msgs = vec![
            AgentMsg::Hello(Hello { proto: PROTO_VERSION, agent_version: "0.1.0".into() }),
            AgentMsg::BasicInfo(BasicInfo { cpu_name: "x".into(), ipv4: Some("1.2.3.4".into()), ..Default::default() }),
            AgentMsg::Report(Report { ts_ms: 1, cpu: 12.5, mem_used: 1 << 40, ..Default::default() }),
            AgentMsg::PingResults(vec![PingResult { task_id: 3, ts: 9, rtt_us: None }]),
            AgentMsg::SshEvents(vec![SshEvent {
                seq: 7,
                ts: 100,
                user: "root".into(),
                ip: "2001:db8::1".into(),
                port: 22,
                method: "publickey".into(),
                key_fp: Some("SHA256:abc".into()),
            }]),
            AgentMsg::SshFailSummary(SshFailSummary {
                window_start: 1,
                window_secs: 60,
                count: 3,
                top_ips: vec![("1.1.1.1".into(), 3)],
            }),
        ];
        for m in msgs {
            let buf = encode(&m);
            let back: AgentMsg = decode(&buf).unwrap();
            assert_eq!(format!("{m:?}"), format!("{back:?}"));
        }
    }

    #[test]
    fn report_is_compact() {
        let buf = encode(&AgentMsg::Report(Report {
            ts_ms: 1_760_000_000_000,
            cpu: 33.3,
            mem_used: 3_000_000_000,
            mem_total: 8_000_000_000,
            disk_used: 30_000_000_000,
            disk_total: 100_000_000_000,
            net_rx: 1_000_000,
            net_tx: 2_000_000,
            net_rx_total: 1 << 40,
            net_tx_total: 1 << 41,
            load1: 0.5,
            tcp: 100,
            procs: 200,
            uptime: 100_000,
            ..Default::default()
        }));
        assert!(buf.len() < 120, "report frame is {} bytes", buf.len());
    }

    #[test]
    fn roundtrip_new_variants() {
        let m = ServerMsg::Upgrade(UpgradeOrder {
            version: "0.1.0-beta.2".into(),
            base_url: "https://example.com/d".into(),
            checksums: vec![("vespera-agent-linux-amd64".into(), "ab".repeat(32))],
        });
        let back: ServerMsg = decode(&encode(&m)).unwrap();
        assert_eq!(format!("{m:?}"), format!("{back:?}"));
        let a = AgentMsg::UpgradeResult(UpgradeResult { version: "x".into(), ok: false, message: "m".into() });
        let back: AgentMsg = decode(&encode(&a)).unwrap();
        assert_eq!(format!("{a:?}"), format!("{back:?}"));
        let m = ServerMsg::Probe(ProbeOrder { id: 7, kind: PingKind::Http, target: "https://a.b".into() });
        let back: ServerMsg = decode(&encode(&m)).unwrap();
        assert_eq!(format!("{m:?}"), format!("{back:?}"));
        let a = AgentMsg::ProbeResult(ProbeResult { id: 7, rtt_us: Some(1500) });
        let back: AgentMsg = decode(&encode(&a)).unwrap();
        assert_eq!(format!("{a:?}"), format!("{back:?}"));
    }

    /// Documents why structs are frozen and new data uses new variants.
    #[test]
    fn compatibility_rules() {
        #[derive(Serialize, Deserialize, Debug)]
        enum OldServerMsg {
            Welcome(OldWelcome),
            SshAck { up_to_seq: u64 },
        }
        #[derive(Serialize, Deserialize, Debug)]
        struct OldWelcome {
            node_uuid: String,
            server_time: i64,
            ssh_acked_seq: u64,
        }
        #[derive(Serialize, Deserialize)]
        struct GrownWelcome {
            node_uuid: String,
            server_time: i64,
            ssh_acked_seq: u64,
            extra: u8,
        }

        // A new variant is an error the old peer can recognise and skip...
        assert!(decode::<OldServerMsg>(&encode(&ServerMsg::Features(vec![FEATURE_UPGRADE.into()]))).is_err());
        // ...while frames it knows still decode.
        assert!(decode::<OldServerMsg>(&encode(&ServerMsg::SshAck { up_to_seq: 3 })).is_ok());
        // Appending a field to an existing struct breaks older decoders.
        let grown = GrownWelcome { node_uuid: "u".into(), server_time: 1, ssh_acked_seq: 2, extra: 0 };
        assert!(decode::<OldWelcome>(&encode(&grown)).is_err());
    }

    #[test]
    fn roundtrip_server_msgs() {
        let m =
            ServerMsg::PingTasks(vec![PingTask { id: 1, kind: PingKind::Tcp, target: "a:1".into(), interval_s: 60 }]);
        let back: ServerMsg = decode(&encode(&m)).unwrap();
        assert_eq!(format!("{m:?}"), format!("{back:?}"));
    }
}
