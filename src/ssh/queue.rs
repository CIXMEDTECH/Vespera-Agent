//! Durable queue of SSH login events awaiting server acknowledgement.
//!
//! Events are appended to `ssh-queue.jsonl` before being sent, and removed once
//! the server acks them, so logins are not lost across disconnects or restarts.

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use vespera_proto::SshEvent;

const MAX_PENDING: usize = 10_000;

pub struct SshQueue {
    queue_path: PathBuf,
    seq_path: PathBuf,
    next_seq: u64,
    pending: VecDeque<SshEvent>,
    /// True when no sequence state existed on disk (first run or wiped state).
    fresh: bool,
}

impl SshQueue {
    pub fn open(state_dir: &Path) -> Self {
        let queue_path = state_dir.join("ssh-queue.jsonl");
        let seq_path = state_dir.join("ssh-seq");
        let saved_seq = fs::read_to_string(&seq_path).ok().and_then(|s| s.trim().parse::<u64>().ok());
        let mut pending: VecDeque<SshEvent> = fs::read_to_string(&queue_path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let max_pending = pending.iter().map(|e| e.seq).max().unwrap_or(0);
        let next_seq = saved_seq.unwrap_or(1).max(max_pending + 1);
        let trimmed = pending.len() > MAX_PENDING;
        while pending.len() > MAX_PENDING {
            pending.pop_front();
        }
        let q = Self { queue_path, seq_path, next_seq, pending, fresh: saved_seq.is_none() };
        if trimmed {
            q.rewrite();
        }
        q
    }

    pub fn push(&mut self, mut ev: SshEvent) -> u64 {
        ev.seq = self.next_seq;
        self.next_seq += 1;
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&self.queue_path) {
            let mut line = serde_json::to_string(&ev).unwrap_or_default();
            line.push('\n');
            let _ = f.write_all(line.as_bytes());
        }
        self.save_seq();
        let seq = ev.seq;
        self.pending.push_back(ev);
        // Bound memory/disk if the server is unreachable for a long time.
        while self.pending.len() > MAX_PENDING {
            self.pending.pop_front();
        }
        seq
    }

    /// Server says it has stored everything up to `up_to`.
    pub fn ack(&mut self, up_to: u64) {
        let before = self.pending.len();
        self.pending.retain(|e| e.seq > up_to);
        if self.pending.len() != before {
            self.rewrite();
        }
    }

    /// Handle the `ssh_acked_seq` from the server's Welcome.
    pub fn on_welcome(&mut self, acked: u64) {
        if self.fresh {
            // Local state was lost but the server already has events from a
            // previous life of this agent: renumber after the server's view so
            // new events are not discarded as duplicates.
            if acked >= 1 {
                let mut seq = acked + 1;
                for e in self.pending.iter_mut() {
                    e.seq = seq;
                    seq += 1;
                }
                self.next_seq = seq;
                self.rewrite();
            }
            self.fresh = false;
            self.save_seq();
            return;
        }
        self.ack(acked);
        if self.next_seq <= acked {
            self.next_seq = acked + 1;
            self.save_seq();
        }
    }

    pub fn pending_after(&self, seq: u64, limit: usize) -> Vec<SshEvent> {
        self.pending.iter().filter(|e| e.seq > seq).take(limit).cloned().collect()
    }

    fn save_seq(&self) {
        let _ = fs::write(&self.seq_path, self.next_seq.to_string());
    }

    fn rewrite(&self) {
        let tmp = self.queue_path.with_extension("tmp");
        let mut out = String::new();
        for e in &self.pending {
            out.push_str(&serde_json::to_string(e).unwrap_or_default());
            out.push('\n');
        }
        if fs::write(&tmp, out).is_ok() {
            let _ = fs::rename(&tmp, &self.queue_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev() -> SshEvent {
        SshEvent {
            seq: 0,
            ts: 1,
            user: "root".into(),
            ip: "1.1.1.1".into(),
            port: 22,
            method: "password".into(),
            key_fp: None,
        }
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("vespera-q-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn persist_and_ack() {
        let d = tmpdir("ack");
        let mut q = SshQueue::open(&d);
        q.on_welcome(0);
        assert_eq!(q.push(ev()), 1);
        assert_eq!(q.push(ev()), 2);
        assert_eq!(q.push(ev()), 3);
        q.ack(2);
        drop(q);
        let mut q = SshQueue::open(&d);
        assert_eq!(q.pending_after(0, 10).iter().map(|e| e.seq).collect::<Vec<_>>(), vec![3]);
        assert_eq!(q.push(ev()), 4);
        q.on_welcome(4);
        assert!(q.pending_after(0, 10).is_empty());
    }

    #[test]
    fn renumber_after_wipe() {
        let d = tmpdir("wipe");
        let mut q = SshQueue::open(&d);
        q.push(ev());
        q.on_welcome(41);
        assert_eq!(q.pending_after(0, 10)[0].seq, 42);
        assert_eq!(q.push(ev()), 43);
    }
}
