#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

mod collect;
mod config;
mod conn;
mod net;
mod ping;
mod ssh;
mod update;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use tokio::sync::{mpsc, watch};

use crate::collect::{Collector, Filters};
use crate::config::Config;
use crate::net::Family;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

pub const VERSION: &str = env!("VESPERA_AGENT_VERSION");
const IP_REFRESH: Duration = Duration::from_secs(1800);

fn main() -> Result<()> {
    update::init();
    let cfg = Config::parse();
    let level = cfg.log.parse().unwrap_or(tracing::Level::INFO);
    tracing_subscriber::fmt().with_max_level(level).with_target(false).init();
    tracing::info!("vespera-agent {VERSION} ({})", env!("VESPERA_GIT_SHA"));

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(run(cfg))
}

fn prepare_state_dir(dir: &PathBuf) -> PathBuf {
    if std::fs::create_dir_all(dir).is_ok() {
        return dir.clone();
    }
    let fallback = std::env::temp_dir().join("vespera-agent");
    tracing::warn!("cannot create state dir {}, using {}", dir.display(), fallback.display());
    let _ = std::fs::create_dir_all(&fallback);
    fallback
}

async fn run(mut cfg: Config) -> Result<()> {
    cfg.state_dir = prepare_state_dir(&cfg.state_dir);
    let collector = Collector::new(Filters::from_config(&cfg))?;

    // Public IP detection, refreshed periodically.
    let (ip_tx, ips) = watch::channel((None, None));
    if !cfg.no_ip_lookup {
        tokio::spawn(async move {
            loop {
                let (v4, v6) = tokio::join!(collect::ip::lookup(Family::V4), collect::ip::lookup(Family::V6));
                tracing::debug!(?v4, ?v6, "public ip");
                ip_tx.send_if_modified(|cur| {
                    let changed = *cur != (v4.clone(), v6.clone());
                    *cur = (v4, v6);
                    changed
                });
                tokio::time::sleep(IP_REFRESH).await;
            }
        });
    }

    let (tasks_tx, tasks_rx) = watch::channel(Vec::new());
    let (ping_tx, ping_rx) = mpsc::channel(1024);
    let icmp = ping::Icmp::new();
    tokio::spawn(ping::run(tasks_rx, ping_tx, icmp.clone()));

    let (fail_tx, fail_rx) = mpsc::channel(64);
    let ssh = if cfg.disable_ssh { None } else { ssh::spawn(&cfg.ssh_source, &cfg.state_dir, fail_tx) };

    let (upgrade_tx, upgrade_rx) = mpsc::channel(4);
    let (probe_tx, probe_rx) = mpsc::channel(16);
    let agent = conn::Agent {
        cfg,
        collector,
        ips,
        tasks_tx,
        ping_rx,
        ssh,
        fail_rx,
        upgrade_tx,
        upgrade_rx,
        upgrading: false,
        icmp,
        probe_tx,
        probe_rx,
    };
    tokio::select! {
        _ = agent.run() => {}
        _ = shutdown_signal() => tracing::info!("shutting down"),
    }
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("signal handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
