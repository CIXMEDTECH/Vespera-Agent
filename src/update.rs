//! Self-update: download the release binary named in an [`UpgradeOrder`],
//! verify its SHA-256, atomically replace the running executable and re-exec.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use sha2::{Digest, Sha256};
use vespera_proto::UpgradeOrder;

use crate::net::http;

const MAX_BINARY: usize = 64 << 20;

/// Release asset name for this build, or `None` where self-update is unsupported.
pub const ASSET: Option<&str> = if !cfg!(target_os = "linux") {
    None
} else if cfg!(target_arch = "x86_64") {
    Some("vespera-agent-linux-amd64")
} else if cfg!(target_arch = "aarch64") {
    Some("vespera-agent-linux-arm64")
} else if cfg!(target_arch = "arm") {
    Some("vespera-agent-linux-armv7")
} else if cfg!(target_arch = "x86") {
    Some("vespera-agent-linux-386")
} else if cfg!(target_arch = "riscv64") {
    Some("vespera-agent-linux-riscv64")
} else {
    None
};

static EXE: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Remember the executable path at startup. After the file is replaced,
/// `/proc/self/exe` would point at a "(deleted)" path.
pub fn init() {
    EXE.get_or_init(|| std::env::current_exe().ok());
}

fn exe() -> Result<PathBuf, String> {
    EXE.get_or_init(|| std::env::current_exe().ok()).clone().ok_or_else(|| "cannot determine executable path".into())
}

/// Download, verify and install the binary for `order`. Returns once the new
/// file is in place; call [`restart`] afterwards.
pub async fn apply(order: &UpgradeOrder) -> Result<(), String> {
    let asset = ASSET.ok_or("self-update is not supported on this platform")?;
    let expected = order
        .checksums
        .iter()
        .find(|(name, _)| name == asset)
        .map(|(_, sum)| sum.clone())
        .ok_or_else(|| format!("release {} has no checksum for {asset}", order.version))?;
    let url = format!("{}/{asset}", order.base_url.trim_end_matches('/'));
    tracing::info!(version = %order.version, %url, "downloading agent update");
    let bytes =
        http::download(&url, MAX_BINARY, Duration::from_secs(180)).await.map_err(|e| format!("download: {e}"))?;
    install(&bytes, &expected, &exe()?)
}

/// Verify `bytes` against `sha256_hex` and atomically replace `exe` with them.
pub fn install(bytes: &[u8], sha256_hex: &str, exe: &Path) -> Result<(), String> {
    let actual = hex(&Sha256::digest(bytes));
    if !actual.eq_ignore_ascii_case(sha256_hex.trim()) {
        return Err(format!("checksum mismatch: expected {sha256_hex}, got {actual}"));
    }
    if !bytes.starts_with(b"\x7fELF") {
        return Err("downloaded file is not an ELF executable".into());
    }
    let dir = exe.parent().ok_or("executable has no parent directory")?;
    let tmp = dir.join(".vespera-agent.new");
    let result = (|| {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o755))?;
        }
        // Flush the data before the rename so a power loss cannot leave an
        // empty or partial binary in place of the old one.
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, exe)?;
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok::<_, std::io::Error>(())
    })();
    result.map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        if e.raw_os_error() == Some(libc::EROFS) || e.kind() == std::io::ErrorKind::PermissionDenied {
            format!("cannot write {}: {e}. Re-run install.sh once so the service may update itself", dir.display())
        } else {
            format!("install: {e}")
        }
    })
}

/// Replace the current process with the (new) executable, keeping arguments
/// and environment. Only returns on failure.
#[cfg(unix)]
pub fn restart() -> String {
    use std::os::unix::process::CommandExt;
    match exe() {
        Ok(path) => {
            // exec keeps our PID, so a child left running would become a
            // zombie the new image never reaps.
            crate::ssh::journald::kill_child();
            let err = std::process::Command::new(path).args(std::env::args_os().skip(1)).exec();
            format!("re-exec failed: {err}")
        }
        Err(e) => e,
    }
}

#[cfg(not(unix))]
pub fn restart() -> String {
    "restart not supported on this platform".into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("vespera-upd-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn verifies_and_replaces() {
        let d = tmp("ok");
        let exe = d.join("vespera-agent");
        std::fs::write(&exe, b"old").unwrap();
        let new = b"\x7fELF-new-binary".to_vec();
        let sum = hex(&Sha256::digest(&new));
        install(&new, &sum.to_uppercase(), &exe).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), new);
        assert!(!d.join(".vespera-agent.new").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777, 0o755);
        }
    }

    #[test]
    fn rejects_bad_checksum_or_format() {
        let d = tmp("bad");
        let exe = d.join("vespera-agent");
        std::fs::write(&exe, b"old").unwrap();
        let new = b"\x7fELF-new".to_vec();
        assert!(install(&new, &"0".repeat(64), &exe).unwrap_err().contains("checksum mismatch"));
        let html = b"<html>blocked</html>".to_vec();
        assert!(install(&html, &hex(&Sha256::digest(&html)), &exe).unwrap_err().contains("ELF"));
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
    }
}
