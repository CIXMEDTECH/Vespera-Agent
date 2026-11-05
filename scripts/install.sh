#!/usr/bin/env bash
# Vespera agent installer / upgrader / uninstaller (Linux, systemd or OpenRC).
#
#   curl -fsSL https://raw.githubusercontent.com/CIXMEDTECH/Vespera-Agent/main/scripts/install.sh \
#     | sudo bash -s -- -e https://status.example.com -t <token>
#
# Installer options (everything else is passed to the agent):
#   --version <tag>     install a specific release, e.g. v0.1.0-beta.1 (default: newest, including pre-releases)
#   --mirror <prefix>   GitHub download proxy prefix, e.g. https://ghfast.top/
#   --uninstall         stop and remove the agent (service, binary, config, state, logs)
#
# Useful agent flags: --disable-ssh, --disable-auto-update, --interval <secs>
set -euo pipefail

REPO="CIXMEDTECH/Vespera-Agent"
NAME="vespera-agent"
INSTALL_DIR="/opt/vespera-agent"
STATE_DIR="/var/lib/vespera-agent"
ENV_FILE="/etc/vespera-agent.env"
UNIT_FILE="/etc/systemd/system/${NAME}.service"
OPENRC_FILE="/etc/init.d/${NAME}"

VERSION=""
MIRROR=""
UNINSTALL=0
ENDPOINT=""
TOKEN=""
AGENT_ARGS=()

info() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --version|--agent-version) VERSION="${2:-}"; shift 2 ;;
    --version=*|--agent-version=*) VERSION="${1#*=}"; shift ;;
    --mirror) MIRROR="${2:-}"; shift 2 ;;
    --mirror=*) MIRROR="${1#*=}"; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -e|--endpoint) ENDPOINT="${2:-}"; shift 2 ;;
    --endpoint=*) ENDPOINT="${1#*=}"; shift ;;
    -t|--token) TOKEN="${2:-}"; shift 2 ;;
    --token=*) TOKEN="${1#*=}"; shift ;;
    -h|--help) sed -n '2,13p' "$0" 2>/dev/null || true; exit 0 ;;
    *) AGENT_ARGS+=("$1"); shift ;;
  esac
done

[ "$(id -u)" -eq 0 ] || die "please run as root (e.g. with sudo)"
[ "$(uname -s)" = "Linux" ] || die "only Linux is supported"

has() { command -v "$1" >/dev/null 2>&1; }

if has systemctl && [ -d /run/systemd/system ]; then
  INIT=systemd
elif has rc-service && [ -d /etc/init.d ]; then
  INIT=openrc
else
  die "neither systemd nor OpenRC found"
fi

stop_service() {
  if [ "$INIT" = systemd ]; then
    systemctl stop "$NAME" 2>/dev/null || true
  else
    rc-service "$NAME" stop 2>/dev/null || true
  fi
}

if [ "$UNINSTALL" -eq 1 ]; then
  info "Uninstalling $NAME"
  stop_service
  if [ "$INIT" = systemd ]; then
    systemctl disable "$NAME" 2>/dev/null || true
    rm -f "$UNIT_FILE"
    systemctl daemon-reload
  else
    rc-update del "$NAME" default 2>/dev/null || true
    rm -f "$OPENRC_FILE"
  fi
  rm -rf "$INSTALL_DIR" "$STATE_DIR" "$ENV_FILE" "/var/log/${NAME}.log"
  info "Done."
  exit 0
fi

# Keep endpoint/token from a previous install when upgrading without them.
if [ -f "$ENV_FILE" ]; then
  # shellcheck disable=SC1090
  . "$ENV_FILE"
  [ -n "$ENDPOINT" ] || ENDPOINT="${VESPERA_ENDPOINT:-}"
  [ -n "$TOKEN" ] || TOKEN="${VESPERA_TOKEN:-}"
  if [ ${#AGENT_ARGS[@]} -eq 0 ] && [ -n "${VESPERA_ARGS:-}" ]; then
    read -r -a AGENT_ARGS <<<"$VESPERA_ARGS"
  fi
fi
[ -n "$ENDPOINT" ] || die "missing -e <panel URL>"
[ -n "$TOKEN" ] || die "missing -t <token>"

case "$(uname -m)" in
  x86_64|amd64) ARCH=amd64 ;;
  aarch64|arm64) ARCH=arm64 ;;
  armv7l|armv7|armv8l) ARCH=armv7 ;;
  i386|i486|i586|i686) ARCH=386 ;;
  riscv64) ARCH=riscv64 ;;
  *) die "unsupported architecture: $(uname -m)" ;;
esac

if has curl; then
  fetch() { curl -fsSL --retry 3 --connect-timeout 15 -o "$2" "$1"; }
  fetch_stdout() { curl -fsSL --retry 3 --connect-timeout 15 "$1"; }
elif has wget; then
  fetch() { wget -q -T 15 -O "$2" "$1"; }
  fetch_stdout() { wget -q -T 15 -O - "$1"; }
else
  die "curl or wget is required"
fi

if [ -n "$MIRROR" ] && [ "${MIRROR%/}" = "$MIRROR" ]; then
  MIRROR="$MIRROR/"
fi

if [ -z "$VERSION" ]; then
  info "Looking up the latest release"
  # /releases (not /releases/latest) so pre-releases such as betas are included.
  VERSION=$(fetch_stdout "https://api.github.com/repos/${REPO}/releases?per_page=1" 2>/dev/null \
    | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/') || true
  [ -n "$VERSION" ] || die "could not query GitHub releases; pass --version <tag> (e.g. --version v0.1.0-beta.1)"
fi
case "$VERSION" in v*) ;; *) VERSION="v$VERSION" ;; esac

BASE="${MIRROR}https://github.com/${REPO}/releases/download/${VERSION}"
ASSET="${NAME}-linux-${ARCH}"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

info "Downloading ${ASSET} ${VERSION}"
fetch "${BASE}/${ASSET}" "$TMP/$ASSET" || die "download failed: ${BASE}/${ASSET}"
if fetch "${BASE}/SHA256SUMS" "$TMP/SHA256SUMS" 2>/dev/null; then
  if has sha256sum; then
    (cd "$TMP" && grep " ${ASSET}\$" SHA256SUMS | sha256sum -c - >/dev/null) || die "checksum mismatch for ${ASSET}"
    info "Checksum verified"
  else
    warn "sha256sum not found, skipping checksum verification"
  fi
else
  warn "SHA256SUMS not available, skipping checksum verification"
fi

stop_service
mkdir -p "$INSTALL_DIR" "$STATE_DIR"
install -m 0755 "$TMP/$ASSET" "$INSTALL_DIR/$NAME"

# Endpoint and token go to a root-only env file instead of the command line.
umask 077
{
  echo "VESPERA_ENDPOINT=\"${ENDPOINT}\""
  echo "VESPERA_TOKEN=\"${TOKEN}\""
  echo "VESPERA_STATE_DIR=\"${STATE_DIR}\""
  echo "VESPERA_ARGS=\"${AGENT_ARGS[*]:-}\""
} >"$ENV_FILE"
chmod 600 "$ENV_FILE"
umask 022

if [ "$INIT" = systemd ]; then
  cat >"$UNIT_FILE" <<EOF
[Unit]
Description=Vespera monitoring agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
EnvironmentFile=${ENV_FILE}
ExecStart=${INSTALL_DIR}/${NAME} \$VESPERA_ARGS
Restart=always
RestartSec=5
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=read-only
# The install dir stays writable so the agent can update itself.
ReadWritePaths=${STATE_DIR} ${INSTALL_DIR}
PrivateTmp=yes
ProtectKernelTunables=yes
ProtectControlGroups=yes

[Install]
WantedBy=multi-user.target
EOF
  systemctl daemon-reload
  systemctl enable "$NAME" >/dev/null 2>&1
  systemctl restart "$NAME"
  sleep 2
  systemctl is-active --quiet "$NAME" || { journalctl -u "$NAME" -n 20 --no-pager || true; die "service failed to start"; }
  info "Installed ${NAME} ${VERSION}. Logs: journalctl -u ${NAME} -f"
else
  cat >"$OPENRC_FILE" <<EOF
#!/sbin/openrc-run
name="${NAME}"
description="Vespera monitoring agent"
command="${INSTALL_DIR}/${NAME}"
supervisor=supervise-daemon
respawn_delay=5
output_log="/var/log/${NAME}.log"
error_log="/var/log/${NAME}.log"

depend() {
  need net
}

start_pre() {
  set -a
  . ${ENV_FILE}
  set +a
  command_args="\$VESPERA_ARGS"
}
EOF
  chmod 755 "$OPENRC_FILE"
  rc-update add "$NAME" default >/dev/null 2>&1 || true
  rc-service "$NAME" restart >/dev/null 2>&1 || rc-service "$NAME" start
  info "Installed ${NAME} ${VERSION}. Logs: /var/log/${NAME}.log"
fi
