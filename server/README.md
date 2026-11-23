# RabbitControl Server（Rust + Axum + React）

RabbitControl 服务端目录，与桌面客户端（Rust `crates/`）同仓演进但构建彻底隔离（独立 Cargo workspace，根目录 `Cargo.toml` 已 `exclude = ["server"]`），分为**后端**与**Web 管理前端**两部分：

- **后端（`server/backend/`，Rust + Axum 0.8 + `mimalloc`）**：
  - **Rendezvous 信令（`:21116/tcp`，无 WebSocket）**：基于 `TCP_NODELAY` 的原生长度前缀二进制帧 `[u32 BE len (1..=65536)][UTF-8 JSON]`，负责 9 位设备 ID 注册、心跳保活、P2P 打洞协调（`punch_coord`）与中继令牌签发（`relay_ready`）；
  - **UDP STUN-lite 探测（`:21116/udp`）**：固定 22 字节 `RCSP` 请求 → `RCSA` 响应，探测客户端公网 UDP `IP:Port` 并写入临时态存储；
  - **Zero-Copy TCP Relay（`:21117/tcp`）**：极简 `RCR1` 二进制握手 + Linux `splice(2)` 内核零拷贝管道转发（非 Linux 或资源受限时自动降级为有界用户态拷贝），用于对称 NAT 打洞失败后的加密流兜底透传；
  - **Admin REST API（`:21118/tcp`，Axum 0.8）**：健康检查、设备注册、运行指标、设备与活跃中继会话管理；
  - **存储分层**：
    - 持久化层：**PostgreSQL**（`sqlx` + 内置 SQL migrations），抽象在 `DeviceStore` trait 之后（单元测试使用 `MemoryDeviceStore`）；
    - 临时态层（在线表 / 公网 UDP 映射 / 一次性中继令牌）：**可选 Redis**（通过 `--redis-url` / `REDIS_URL` 开启），未配置时默认使用零依赖的进程内 `MemoryEphemeralStore`（`DashMap` + TTL 过期清理）。
- **Web 前端（`server/web/`，React + TypeScript + Vite）**：
  - 面向管理员的 Web 控制台（仪表盘、在线设备、中继会话监控），与桌面端 `crates/rc-ui` 彻底隔离。

## 目录结构

```text
server/
├── README.md
├── docs/
│   ├── 00-architecture-draft.md   ← 架构设计与技术选型定稿
│   └── 02-protocol-and-api.md     ← 线协议（TCP 信令 / UDP 探测 / TCP Relay）与 REST API 规范
├── backend/                       ← 独立 Rust 服务端 workspace (`rc-server`)
│   ├── Cargo.toml                 ← 含独立 [workspace] 表，不向上查找父 workspace
│   ├── migrations/
│   │   └── 0001_init.sql          ← PostgreSQL 初始建表（devices / relay_sessions_log）
│   └── src/
│       ├── lib.rs                 ← 核心模块导出
│       ├── store.rs               ← DeviceStore trait + PgDeviceStore + MemoryDeviceStore
│       ├── ephemeral.rs           ← EphemeralStore trait + MemoryEphemeralStore + RedisEphemeralStore
│       ├── signal.rs              ← 原生 TCP 分帧信令 + UDP RCSP/RCSA 探测服务
│       ├── relay/                 ← Zero-Copy TCP Relay（Linux splice(2) + fallback）
│       ├── api.rs                 ← Axum 0.8 REST API
│       └── main.rs                ← CLI / 服务入口（mimalloc 全局分配器）
└── web/                           ← React Web 管理前端（独立工程）
```

## 本地验证（Phase 1）

```bash
cd server/backend
cargo test --lib
cargo clippy --lib -- -D warnings
```

详见 [docs/00-architecture-draft.md](docs/00-architecture-draft.md) 与 [docs/02-protocol-and-api.md](docs/02-protocol-and-api.md)。
