//! Latency probes scheduled locally on the agent. The server only pushes the
//! task list when it changes; results are streamed back in batches.

mod icmp;

pub use icmp::Icmp;

use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use vespera_proto::{PingKind, PingResult, PingTask, ProbeOrder, ProbeResult};

use crate::net::{self, http};

const TIMEOUT: Duration = Duration::from_secs(3);

pub async fn run(mut tasks_rx: watch::Receiver<Vec<PingTask>>, out: mpsc::Sender<PingResult>, icmp: Icmp) {
    loop {
        let tasks = tasks_rx.borrow_and_update().clone();
        let mut set = JoinSet::new();
        for task in tasks {
            set.spawn(task_loop(task, out.clone(), icmp.clone()));
        }
        if tasks_rx.changed().await.is_err() {
            return;
        }
        set.abort_all();
    }
}

async fn task_loop(task: PingTask, out: mpsc::Sender<PingResult>, icmp: icmp::Icmp) {
    let interval = Duration::from_secs(task.interval_s.max(1) as u64);
    // Spread probes so tasks with the same interval don't fire together.
    tokio::time::sleep(Duration::from_millis(fastrand::u64(0..interval.as_millis() as u64))).await;
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut seq: u16 = 0;
    loop {
        tick.tick().await;
        seq = seq.wrapping_add(1);
        let rtt = probe(&task, &icmp, seq).await;
        let res = PingResult { task_id: task.id, ts: crate::ssh::now_secs(), rtt_us: rtt };
        if out.try_send(res).is_err() && out.is_closed() {
            return;
        }
    }
}

/// Run a one-off test ordered by the panel.
pub async fn probe_once(order: ProbeOrder, icmp: &Icmp) -> ProbeResult {
    let task = PingTask { id: 0, kind: order.kind, target: order.target, interval_s: 0 };
    ProbeResult { id: order.id, rtt_us: probe(&task, icmp, fastrand::u16(..)).await }
}

pub async fn probe(task: &PingTask, icmp: &icmp::Icmp, seq: u16) -> Option<u32> {
    let started = Instant::now();
    let ok = match task.kind {
        PingKind::Icmp => return icmp.ping(&task.target, seq, TIMEOUT).await,
        PingKind::Tcp => {
            let (host, port) = split_host_port(&task.target)?;
            net::connect(&host, port, None, TIMEOUT).await.is_ok()
        }
        PingKind::Http => {
            let url = if task.target.contains("://") { task.target.clone() } else { format!("http://{}", task.target) };
            let url = http::parse_url(&url)?;
            matches!(http::get(&url, None, TIMEOUT, false).await, Ok(r) if r.status < 400)
        }
    };
    ok.then(|| started.elapsed().as_micros().min(u32::MAX as u128) as u32)
}

pub fn split_host_port(s: &str) -> Option<(String, u16)> {
    if let Some(rest) = s.strip_prefix('[') {
        let (h, p) = rest.split_once("]:")?;
        return Some((h.to_string(), p.parse().ok()?));
    }
    let (h, p) = s.rsplit_once(':')?;
    if h.contains(':') {
        return None;
    }
    Some((h.to_string(), p.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_port() {
        assert_eq!(split_host_port("a.com:443"), Some(("a.com".into(), 443)));
        assert_eq!(split_host_port("[::1]:22"), Some(("::1".into(), 22)));
        assert_eq!(split_host_port("::1"), None);
        assert_eq!(split_host_port("a.com"), None);
    }
}
