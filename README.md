# Vespera Agent

[Vespera](https://github.com/CIXMEDTECH/Vespera) 探针的被控端，负责采集 Linux 服务器状态、执行延迟探测，并监控 SSH 登录。

- 单个 musl 静态二进制，约 3 MB，无运行时依赖。
- 直接读取 `/proc`，复用文件句柄和缓冲区，采集过程中几乎不分配内存；运行时为单线程 tokio。
- 支持的架构：`amd64`、`arm64`、`armv7`、`386`、`riscv64`。

> 当前为 **beta** 版本（`0.1.0-beta.5`）。

## 安装

在面板的 **管理 → 节点 → 安装 Agent** 中复制命令，到服务器上以 root 运行：

```bash
curl -fsSL https://raw.githubusercontent.com/CIXMEDTECH/Vespera-Agent/main/scripts/install.sh \
  | sudo bash -s -- -e https://status.example.com -t <token>
```

`install.sh` 的行为：

- 默认安装最新版本（**包括 beta 等预发布版本**）。下载后校验 SHA256，安装到 `/opt/vespera-agent`，并注册为 systemd 服务；没有 systemd 时使用 OpenRC。
- 面板地址和 Token 写入只有 root 可读的 `/etc/vespera-agent.env`，不会出现在进程命令行里。
- 重新运行同一条命令即可升级；升级时不传 `-e` 和 `-t` 也会沿用原来的配置。

| 安装脚本参数 | 说明 |
|---|---|
| `--version v0.1.0-beta.1` | 安装指定版本 |
| `--mirror https://ghfast.top/` | 通过加速前缀下载 GitHub 文件 |
| `--uninstall` | 卸载 Agent |

其余参数会原样传给 Agent。

### 卸载

用安装脚本卸载，会停止并移除服务，删除程序、配置、状态和日志：

```bash
curl -fsSL https://raw.githubusercontent.com/CIXMEDTECH/Vespera-Agent/main/scripts/install.sh | sudo bash -s -- --uninstall
```

无法访问 GitHub 时，可以直接执行下面的命令，不需要下载任何东西。面板的“安装命令”弹窗里也有这两条命令。

systemd：

```bash
sudo systemctl disable --now vespera-agent; sudo rm -f /etc/systemd/system/vespera-agent.service && sudo systemctl daemon-reload && sudo rm -rf /opt/vespera-agent /var/lib/vespera-agent /etc/vespera-agent.env
```

OpenRC：

```bash
sudo rc-service vespera-agent stop; sudo rc-update del vespera-agent default; sudo rm -f /etc/init.d/vespera-agent && sudo rm -rf /opt/vespera-agent /var/lib/vespera-agent /etc/vespera-agent.env /var/log/vespera-agent.log
```

systemd 下的日志由 journald 管理，会随 journald 自身的轮转策略自动清理。

## Agent 参数

| 参数 | 环境变量 | 默认值 | 说明 |
|---|---|---|---|
| `-e, --endpoint` | `VESPERA_ENDPOINT` | — | 面板地址 |
| `-t, --token` | `VESPERA_TOKEN` | — | 节点 Token |
| `-i, --interval` | `VESPERA_INTERVAL` | `1` | 上报间隔，单位秒 |
| `--nic-include` / `--nic-exclude` | `VESPERA_NIC_*` | — | 网卡过滤，逗号分隔，支持 `*` 和 `?` 通配 |
| `--mount-include` | `VESPERA_MOUNT_INCLUDE` | — | 只统计这些挂载点的磁盘 |
| `--ssh-source` | `VESPERA_SSH_SOURCE` | `auto` | SSH 日志来源：`auto`、`journald` 或 `file:/var/log/auth.log` |
| `--disable-ssh` | `VESPERA_DISABLE_SSH` | — | 关闭 SSH 监控 |
| `--disable-auto-update` | `VESPERA_DISABLE_AUTO_UPDATE` | — | 拒绝面板推送的自动升级 |
| `--state-dir` | `VESPERA_STATE_DIR` | `/var/lib/vespera-agent` | SSH 队列和日志游标的存放目录 |
| `--ipv4` / `--ipv6` | `VESPERA_IPV4/6` | 自动 | 手动指定上报的 IP |
| `--no-ip-lookup` | `VESPERA_NO_IP_LOOKUP` | — | 不探测公网 IP |
| `--insecure` | `VESPERA_INSECURE` | — | 跳过 TLS 证书校验 |
| `--log` | `VESPERA_LOG` | `info` | 日志级别：`error`、`warn`、`info`、`debug`、`trace` |

## SSH 登录监控

- **日志来源**：有 systemd 时，通过 `journalctl -f -o json` 跟随 `sshd`、`sshd-session` 和 `sshd-auth` 的日志，并记录游标，重启后从中断处继续。没有 systemd 时，跟随 `/var/log/auth.log`、`/var/log/secure` 或 `/var/log/messages`，能正确处理 logrotate 轮转和截断。
- **登录成功**：会带上用户名、来源 IP 和端口、认证方式，公钥登录还会带上密钥指纹。事件先写入 `ssh-queue.jsonl`，面板确认收到后才删除，因此断线和重启都不会丢失。
- **失败尝试**：每 60 秒汇总一次，上报失败次数和来源 IP 的 Top 10。

## 自动更新

面板发现新版本后，会把下载地址和全部 SHA256 下发给 Agent。Agent 的处理流程：

1. 下载与自身架构对应的二进制，支持重定向和 chunked 传输。
2. 校验 SHA256，并确认文件是 ELF 可执行文件。
3. 原子替换 `/opt/vespera-agent/vespera-agent`。
4. 用原来的参数 `exec` 重启自身。

任何一步失败，Agent 都会把原因上报给面板，并显示在节点列表里。

自我升级要求安装目录可写。beta.2 的 `install.sh` 已经在 systemd 服务里加了 `ReadWritePaths=/opt/vespera-agent`；用 beta.1 安装的机器需要重新运行一次安装命令。

## 采集口径

- **内存**：与 htop 一致，计算公式为 `MemTotal - MemFree - Buffers - Cached - SReclaimable + Shmem`。
- **磁盘**：统计 `/proc/self/mountinfo` 中的真实块设备，会排除 tmpfs、overlay、网络文件系统和容器目录；同一设备只统计一次，ZFS 按存储池合并。
- **网卡**：默认排除 `lo`、`docker*`、`veth*`、`br-*`、`virbr*`、`tun*` 等虚拟网卡。
- **连接数**：来自 `/proc/net/sockstat{,6}`，读取开销是 O(1)。
- **公网 IP**：直接用 IP 地址访问 Cloudflare 的 `/cdn-cgi/trace`，分别探测 v4 和 v6，不依赖 DNS。

## 构建

```bash
pip install ziglang cargo-zigbuild      # 或者 cargo install cargo-zigbuild
rustup target add x86_64-unknown-linux-musl
cargo zigbuild --release --target x86_64-unknown-linux-musl
```

在 macOS 上可以直接 `cargo run` 进行开发调试，此时采集器会生成模拟数据。

## 发布

1. 修改 `Cargo.toml` 里的 `[workspace.package] version`。
2. 推送对应的标签：`git tag v0.1.0-beta.1 && git push origin v0.1.0-beta.1`。
3. GitHub Actions 会校验版本号，构建 5 个架构的二进制，生成 `SHA256SUMS`，并发布 Release。带后缀的版本号会发布为预发布版本。

`crates/vespera-proto` 是从面板仓库同步过来的副本，同步方法是运行 `scripts/sync-proto.sh`。
