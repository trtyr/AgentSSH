# 🦾 AgentSSH

[![Crates.io](https://img.shields.io/crates/v/agentssh.svg)](https://crates.io/crates/agentssh)
[![Rust](https://img.shields.io/badge/rust-1.85+-orange.svg)](https://rust-lang.org)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-macOS%20|%20Linux-lightgrey.svg)]()

**为 AI 而生的 SSH 工具箱。** 不是给人看的终端，不是 OpenSSH 的封装。是一个能说 JSON 的可编程后端——为 agent 设计，人也能用。

AgentSSH 通过 `libssh2` 直接跟 SSH 协议对话。没有 shell 包装，没有屏幕抓取，没有启发式猜测。

---

## ✨ 为什么选 AgentSSH

|  | 传统 SSH | AgentSSH |
|---|---|---|
| 🎯 **输出格式** | 带 ANSI 转义码的原始终端文本 | 带状态码的结构化 JSON |
| 🔌 **连接模型** | 连接 → 执行 → 断开 | 长连接守护进程，会话复用 |
| 📡 **端口转发** | 每个终端手动 `ssh -L` / `ssh -D` | 守护进程管理的隧道 & SOCKS5 代理 |
| 🗂️ **文件传输** | 单独的 `scp`/`sftp` 命令 | 内置 SFTP → SCP → exec 三级回退链 |
| 🔐 **认证配置** | `~/.ssh/config`（自定义语法） | JSON 配置文件，可程序化写入 |
| 🪟 **Windows** | 能用但别扭 | 自动检测并启用 PowerShell 回退 |
| 📟 **输出导航** | 终端里滚屏 | 游标分页：`--offset` / `--limit` |

---

## 🚀 快速开始

```bash
# 安装依赖
brew install libssh2        # macOS
# apt install libssh2-dev   # Linux

# 从 crates.io 安装（推荐）
cargo install agentssh

# 或从源码编译
cargo build --release
# → target/release/agentssh
```

### 🔐 保存连接配置

```bash
agentssh profile add prod \
  --host example.com \
  --username root \
  --private-key-path ~/.ssh/id_ed25519

agentssh profile list
# tencent   root@82.157.147.224:22
# prod      root@example.com:22
```

### ⚡ 执行命令（一次性）

```bash
agentssh --json exec --profile prod --retry 3 -- uptime
# {"ok":true,"data":{"exit_status":0,"stdout":"21:03:01 up 42 days\n","stderr":""}}
```

### 🔄 长连接会话

```bash
agentssh connect --profile prod
# → session_id: s1（后台守护进程自动启动）

agentssh session send --session-id s1 --input "ls -la\n"
agentssh session send --session-id s1 \
  --input "sudo systemctl restart nginx\n" \
  --expect "[sudo] password" \
  --respond "mypassword\n"

agentssh session read --session-id s1          # 读最新输出
agentssh session read --session-id s1 --follow # 实时流式读取
agentssh session spawn --from s1               # 在同一条 SSH 连接上开新的 PTY

agentssh session ping --session-id s1
agentssh session close --session-id s1
```

### 📁 文件传输（多协议自动切换）

```bash
# 默认 auto 模式：SFTP → SCP → exec（Linux、macOS、Windows 通吃！）
agentssh file upload --profile prod --local ./app --remote /opt/app
agentssh file download --profile prod --remote /var/log/syslog --local ./syslog
agentssh file ls --profile prod --remote /var/www

# 强制指定传输协议
agentssh file upload --profile windows --method scp --local ./app --remote C:/app
```

### 🌐 端口转发 & SOCKS5 代理

```bash
# 本地端口转发：localhost:9999 → 远程内网 :8080
agentssh proxy create --profile prod \
  --local 127.0.0.1:9999 \
  --remote 127.0.0.1:8080

# SOCKS5 动态代理：所有流量走远程出口
agentssh proxy create --profile prod --socks5 127.0.0.1:1080
curl --socks5 127.0.0.1:1080 http://internal-service/

agentssh proxy list
agentssh proxy ping --proxy-id p1
agentssh proxy close --proxy-id p1
agentssh proxy close --all
```

---

## 🏗️ 架构

```
┌──────────────┐     Unix Socket      ┌──────────────────┐
│  CLI 客户端  │ ◄──────────────────► │  守护进程 (serve) │
│  (一次性)    │    JSON-line IPC     │                   │
└──────────────┘                      │  ┌─────────────┐  │
                                      │  │ sessions    │  │
┌──────────────┐                      │  │ proxies     │  │
│  CLI 客户端  │◄────────────────────►│  │ connections │  │
│  (会话模式)  │                      │  └─────────────┘  │
└──────────────┘                      └───────┬──────────┘
                                              │
                                     SSH (libssh2)
                                              │
                                    ┌─────────▼─────────┐
                                    │  远程服务器         │
                                    │  Linux · macOS ·   │
                                    │  Windows           │
                                    └───────────────────┘
```

**守护进程** — 首次执行 session 或 proxy 命令时自动启动。跨 CLI 调用存活。负责心跳检测、会话健康检查、连接池管理。

**连接池** — 多个会话可以共享同一条 TCP/SSH 连接。`session spawn --from <id>` 在同一条底层连接上打开新的 PTY 通道。

**代理线程** — 每个 `proxy create` 在守护进程内启动一个监听线程。每个入站连接 spawn 独立的处理线程，使用非阻塞双向转发。

---

## 📋 命令参考

```bash
# ⚡ 一次性执行
agentssh exec                          # 执行命令 → stdout + exit code
agentssh shell                         # 交互式 PTY（人用）

# 🔄 会话管理
agentssh connect                       # 打开 PTY → session_id
agentssh session send                  # 发送输入（支持 expect/respond 自动应答）
agentssh session spawn --from s1       # 在 s1 的 SSH 连接上新建 PTY
agentssh session read                  # 从游标位置读取输出
agentssh session resize                # 调整 PTY 尺寸
agentssh session signal                # 发送信号（INT, TERM, KILL…）
agentssh session status                # 查看会话元数据
agentssh session ping                  # 检查会话是否存活
agentssh session list                  # 列出所有会话 + 连接分组
agentssh session close                 # 关闭会话

# 📁 文件操作
agentssh file upload                   # 上传（SFTP → SCP → exec）
agentssh file download                 # 下载（SFTP → SCP → exec）
agentssh file ls                       # 列出远程目录

# 🌐 代理 & 隧道
agentssh proxy create                  # -L 端口转发 或 -D SOCKS5
agentssh proxy list                    # 列出活跃代理
agentssh proxy ping                    # 检查代理健康
agentssh proxy close                   # 关闭单个或 --all 全部

# 🔐 连接配置
agentssh profile list | read | add | write | delete

# ⚙️ 守护进程
agentssh daemon serve                  # 启动守护进程（自动启动）
agentssh daemon shutdown               # 停止守护进程 + 清理
```

---

## 🎛️ 配置

| 路径 | 用途 |
|---|---|
| `~/.config/agentssh/profiles.json` | SSH 连接配置文件 |
| `$XDG_RUNTIME_DIR/agentssh-{user}.sock` | 守护进程 Unix socket |
| `/tmp/agentssh-daemon.log` | 守护进程日志（可设置 `AGENTSSH_LOG` 环境变量覆盖）|

### 连接配置格式

```json
{
  "profiles": {
    "prod": {
      "host": "example.com",
      "port": 22,
      "username": "root",
      "private_key_path": "~/.ssh/id_ed25519",
      "retry": 3,
      "retry_delay_ms": 250
    }
  }
}
```

支持字段：`host`, `port`, `username`, `password`, `password_env`, `private_key_path`, `private_key_env`, `passphrase`, `passphrase_env`, `ready_timeout_ms`, `retry`, `retry_delay_ms`。

支持重试的命令：`exec`、`connect`、`file upload`、`file download`、`file ls`、`proxy create`。重试仅针对 TCP 连接和 SSH 握手失败。

---

## 🔧 从源码编译

- **Rust** ≥ 1.85（edition 2024）
- **libssh2** 系统库（`brew install libssh2` / `apt install libssh2-dev`）
- **仅 Unix**（使用 Unix domain socket，不支持 Windows 本地编译，但可以连接 Windows 远程服务器）

```bash
cargo build --release
# → target/release/agentssh
```

---

## 📄 协议

MIT
