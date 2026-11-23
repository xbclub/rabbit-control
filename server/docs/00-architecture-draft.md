# RabbitControl Rust 服务端架构设计（已定稿）

> 状态：**架构已定稿（Phase 1 实现中）**
> 客户端侧现状：Rust（`rc-net` QUIC + `rc-session` + 一体端 `rc-ui`），当前支持局域网 mDNS/组播自动发现与 `IP:端口` 直连，安全面已落地 T19 PBKDF2 密码挑战与 T22 应用内审批弹窗。

---

## 1. 最终技术栈与架构总览

服务端拆为**后端（`server/backend/`）**与**Web 管理前端（`server/web/`）**两部分，与桌面客户端 `crates/` 构建彻底隔离（独立 Cargo workspace）：

- **后端（`server/backend/`，Rust `rc-server`）**：
  - **运行时与内存分配**：`tokio` 异步运行时 + **`mimalloc`** 全局分配器，低尾延迟、极低内存碎片。
  - **持久化存储**：**PostgreSQL**（`sqlx` + 内置 `migrations/`），抽象在 `DeviceStore` trait 之后（生产用 `PgDeviceStore`，离线单测用 `MemoryDeviceStore`）。
  - **临时态存储**：抽象在 `EphemeralStore` trait 之后——**Redis 可选**（通过 `--redis-url` / `REDIS_URL` 启用 `RedisEphemeralStore`），**未配置时默认使用零依赖 `MemoryEphemeralStore`（`DashMap` + TTL 定时清理）**。
  - **信令控制面（无 WebSocket）**：摒弃 HTTP/WebSocket 握手与掩码开销，使用**原生 TCP 长度前缀帧（`:21116/tcp`，`TCP_NODELAY`）** + **UDP STUN-lite 公网端点探测（`:21116/udp`）**。
  - **数据面中继（Zero-Copy TCP Relay）**：监听 `:21117/tcp`，极简 `RCR1` 二进制握手配对后，在 Linux 上使用 **`splice(2)` 内核零拷贝管道转发**（256 KiB pipe、`SPLICE_F_MOVE | SPLICE_F_NONBLOCK`、每 4 MiB / 64 次操作主动协作让出、优雅半关闭 `SHUT_WR`），非 Linux 或系统资源不足时自动降级为有界用户态拷贝。
  - **管理与注册 API**：**Axum 0.8**（`:21118/tcp`），提供设备注册、健康检查、统计指标、在线设备与中继会话管理。
- **Web 前端（`server/web/`）**：**React + TypeScript + Vite**，与桌面端 `crates/rc-ui` 完全独立。

### 端口与职责矩阵

| 模块 | 默认监听 | 传输协议 | 职责 | 状态存储 |
|---|---|---|---|---|
| **Native TCP Signaling** | `:21116` | TCP (`TCP_NODELAY`) | 设备上线注册 9 位设备 ID、心跳保活、候选地址交换、下发打洞协调（`punch_coord`）与中继令牌（`relay_ready`）、强制踢下线（`kicked`） | `DeviceStore` (PostgreSQL) + `EphemeralStore` (Memory / Redis) + 活跃连接表 (`DashMap`) |
| **UDP Endpoint Probe** | `:21116` | UDP (`RCSP`/`RCSA`) | STUN-lite 探测公网 `IP:Port` 并刷新 NAT 映射与在线公网端点缓存 | `EphemeralStore` (Memory / Redis) |
| **Zero-Copy TCP Relay** | `:21117` | TCP (`RCR1` + raw stream) | 对称 NAT 打洞失败时，凭一次性 `relay_token` 配对双端并做 Linux `splice(2)` 零拷贝透传 | `EphemeralStore`（一次性令牌）+ 内存会话表 + PostgreSQL（`relay_sessions_log`） |
| **Admin REST API** | `:21118` | HTTP/1.1 (Axum 0.8) | 健康检查、设备注册、服务统计、设备/会话查询与终止 | `DeviceStore` + `EphemeralStore` + Hub/Relay 状态 |

---

## 2. 跨公网连接主链路

```text
设备 B（被控端 rc-ui）                Rust 服务端 (rc-server)                 设备 A（控制端 rc-ui）
  │                                          │                                          │
  │ 1. TCP :21116 register(device_key, lan)  │                                          │
  │─────────────────────────────────────────>│ 分配/校验永久 9 位 ID，记录活跃连接       │
  │<─────────────────────────────────────────│ registered(device_id, udp_addr, relay)   │
  │ 2. UDP :21116 RCSP(device_id, nonce)     │                                          │
  │─────────────────────────────────────────>│ 记录 B 的公网 UDP IP:Port (TTL)           │
  │<─────────────────────────────────────────│ RCSA(observed_ip, port, nonce)           │
  │ 3. 定时 TCP heartbeat + UDP RCSP 保活    │                                          │
  │                                          │   4. TCP :21116 connect_request(target=B)│
  │                                          │<─────────────────────────────────────────│
  │   5. punch_coord(A 公网/内网, relay_tok) │   5. punch_coord(B 公网/内网, relay_tok) │
  │<─────────────────────────────────────────│─────────────────────────────────────────>│
  │                                          │                                          │
  │ 6. 并发 UDP 打洞（同局域网优先内网直连，公网并发打洞 ~1.5s）                        │
  │<═══════════════════ P2P 直连成功：直接跑现有 QUIC (rc-net + rc-session) ═══════════>│
  │                                          │                                          │
  │ 7. 打洞失败 → 可直接用预分配 token 或发 relay_alloc → 双方连 TCP :21117 (RCR1 握手)  │
  │═══════════> [Linux splice(2) 内核零拷贝双向透传密文流，不解密不缓冲媒体] <═══════════│
```

**关键复用原则（不动客户端媒体低延迟核心）**：
- 服务端严格位于 `server/`，不改动客户端 `crates/` 任何低延迟媒体代码；
- 服务端**绝不触碰** H.264 帧、不重编码、不缓冲媒体队列；
- 中继仅透传端到端加密字节流，安全鉴权仍由客户端端到端负责。

---

## 3. 核心架构决策记录（ADR）

| 决策项 | 最终方案 | 原因 |
|---|---|---|
| **服务端语言** | **Rust**（独立 workspace `server/backend/`） | 与客户端共享语言与工程习惯；单二进制无 GC 抖动；直接调用 Linux `splice(2)` 实现真正零拷贝中继 |
| **Web 框架** | **Axum 0.8** + `tower-http` | 轻量、与 `tokio` 深度集成、零宏魔法、类型安全 |
| **信令传输** | **原生 TCP 分帧 (`:21116/tcp`)，禁用 WebSocket** | 避免 HTTP Upgrade 与 RFC 6455 帧掩码开销；4 字节 BE 长度头 + UTF-8 JSON 既极简又易调试 |
| **UDP 探测** | **固定 22 字节 `RCSP` → `RCSA` (`:21116/udp`)** | 零解析歧义、零堆分配友好、固定偏移读取 |
| **数据库** | **PostgreSQL (`sqlx`)** | 强一致事务、成熟并发、`DeviceStore` trait 解耦支持离线 `MemoryDeviceStore` 单测 |
| **临时态缓存** | **可选 Redis，默认 `DashMap` + TTL** | 单机部署零外部依赖开箱即用；多实例水平扩容时仅需传入 `--redis-url` 即可无缝切 Redis |
| **中继转发** | **Linux `splice(2)` + 自动用户态降级** | 内核态 pipe 直接搬运套接字数据，大幅降低高码率中继时的 CPU 与内存拷贝开销 |
| **内存分配器** | **`mimalloc`** | 提升高并发连接/小帧 JSON 编解码下的分配吞吐与内存回收效率 |

---

## 4. 分阶段落地路线

### Phase 1（当前阶段）：Rust 服务端核心基座
- `Cargo.toml` 独立 workspace 与 `migrations/0001_init.sql`（PostgreSQL）；
- `store.rs`：`DeviceStore` trait、`PgDeviceStore`（9 位加密随机 ID 幂等签发）、`MemoryDeviceStore`；
- `ephemeral.rs`：`EphemeralStore` trait、`MemoryEphemeralStore`（`DashMap` + TTL）、`RedisEphemeralStore`；
- `signal.rs`：原生 TCP 分帧信令（`:21116/tcp`）+ UDP STUN-lite 探测（`:21116/udp`）；
- `relay/`：TCP Relay（`:21117/tcp`，`RCR1` 握手 + Linux `splice(2)` 零拷贝转发）；
- `api.rs` & `main.rs`：Axum 0.8 REST API（`:21118/tcp`）与 CLI 启动入口。

### Phase 2：Web 管理后台与客户端接入
- `server/web/`（React + TypeScript + Vite）对接 `/api/v1/*`；
- 客户端接入 9 位设备 ID 信令与打洞/中继通道。

### Phase 3：生产安全与可观测性
- Ed25519 设备公钥指纹校验与白名单；
- Prometheus 指标暴露、多节点中继调度、限速与审计增强。
