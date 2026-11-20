# 20 Crate 接口契约

> **实现任何 crate 前必读。** 这里给出确切的类型签名与语义。
> 签名不要自创——其他 crate 会按这些签名调用。
> 已实现的 crate 直接看源码，本文只摘关键点。

## 现有 crate 的实际测试数（实测）

| crate | 行数 | 测试 | 状态 |
|---|---|---|---|
| `rc-types` | 148 | 27 | ✅ 完成 |
| `rc-diff` | 425 | 13 | ✅ 完成 |
| `rc-sched` | 650 | 18 | ✅ 完成 |
| `rc-fec` | 526 | 18 | ✅ 完成 |
| `rc-proto` | 610 | 16 | ✅ 完成 |
| `rc-trace` | 601 | 15 | ✅ 完成 |

---

## 1. `rc-types`（稳定层）

### 导出的类型

```rust
// ids.rs
pub struct FrameSeq(pub u64);      // 帧序号，新帧 +1
pub struct CaptureSeq(pub u64);    // 采集序号，含重复帧
pub struct DisplayId(pub u32);     // 显示器
pub struct SessionId(pub u64);     // 会话
pub struct Timestamp(pub u32);     // 相对会话起点，微秒

impl Timestamp {
    pub fn since(start: Instant, now: Instant) -> Self;   // 饱和到 u32::MAX
    pub const fn as_duration(self) -> Duration;
    pub const fn saturating_diff(self, earlier: Timestamp) -> Duration; // 不乱序 panic
    pub const fn diff_us(self, earlier: Timestamp) -> i64;             // 可负
}

// pixel.rs
pub enum PixelFormat { Bgra8, Rgba8, Nv12, I420 }
impl PixelFormat {
    pub const fn bytes_per_pixel(self) -> usize;   // YUV 返回 1（luma 平面）
    pub const fn is_yuv(self) -> bool;
    pub fn min_buffer_size(self, w: u32, h: u32) -> usize;
}

pub struct Rect { pub x: u32, pub y: u32, pub width: u32, pub height: u32 }
impl Rect {
    pub const EMPTY: Rect;
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self;
    pub const fn is_empty(&self) -> bool;
    pub const fn area(&self) -> u64;
    pub const fn right(&self) -> u32;
    pub const fn bottom(&self) -> u32;
    pub fn intersects(&self, other: &Rect) -> bool;
    pub fn union(&self, other: &Rect) -> Rect;
    pub fn clamp_to(&self, width: u32, height: u32) -> Rect;  // 越界裁剪
    pub fn validate_within(&self, width: u32, height: u32) -> Result<()>;
}

pub struct Resolution { pub width: u32, pub height: u32 }
impl Resolution {
    pub const fn new(w: u32, h: u32) -> Self;
    pub const fn is_even(&self) -> bool;      // YUV 4:2:0 前置条件
    pub const fn to_even(&self) -> Resolution;
    pub const fn pixels(&self) -> u64;
}

// lib.rs
pub enum FrameKind { Idr, P }
impl FrameKind { pub const fn is_droppable(self) -> bool; }  // Idr => false

pub struct CpuFrame {
    pub width: u32, pub height: u32,
    pub format: PixelFormat,
    pub stride: u32,          // 可能 > width * bpp（GPU 对齐）
    pub data: Vec<u8>,
}
impl CpuFrame { pub fn validate(&self) -> Result<()>; }

// error.rs
pub enum Error { InvalidFrame(String), InvalidConfig(String),
                 Protocol { offset: usize, msg: String },
                 Encoder(String), Decoder(String),
                 Capture { recoverable: bool, msg: String },
                 Network(String), Timeout(Duration), Io(std::io::Error) }
impl Error { pub fn is_fatal(&self) -> bool; }
pub type Result<T, E = Error> = std::result::Result<T, E>;
```

### 语义要点

- `Timestamp::since` **饱和**到 `u32::MAX`（约 71.6 分钟）。会话层必须在此之前重建会话。
- `Rect::right()` / `bottom()` 是**开区间**（不含）。
- `CpuFrame::stride` 是**字节数**，不是像素数。GPU 纹理对齐后常见 1920→1984。
- `Error::Capture { recoverable }` 决定上层是重建还是放弃。DXGI 的 `ACCESS_LOST` → `recoverable: true`。

---

## 2. `rc-diff`

```rust
pub const DEFAULT_TILE_SIZE: u32 = 16;

/// 合并多个脏区为最小包围盒，并裁剪越界
pub fn merge_rects(rects: &[Rect], bounds: (u32, u32)) -> Rect;

pub struct TileDiff { /* 私有 */ }
impl TileDiff {
    pub fn new(tile_size: u32) -> Self;
    pub fn with_defaults() -> Self;
    pub fn reset(&mut self);
    pub fn update(&mut self, frame: &CpuFrame) -> DiffResult;
}

pub struct DiffResult {
    pub dirty: Rect,
    pub is_repeat: bool,
    pub reused: bool,        // 仅测试用
}
impl DiffResult {
    pub fn needs_encode(&self) -> bool;              // !is_repeat && !dirty.is_empty()
    pub fn dirty_ratio(&self, w: u32, h: u32) -> f64;
}
```

### 语义要点（重要，容易写错）

- **首帧或尺寸变化后**返回 `dirty = 整帧`、`is_repeat = false`。这是「必须整帧编码」的信号。
- `is_repeat = true` 时 `dirty` 为 `EMPTY`。
- 哈希**必须逐字节混入**。绝不能把多个通道异或成一个字节——
  `255^255^255^255 == 0` 会与全黑帧碰撞，导致「整块变白」被判为无变化（画面冻结）。
  这个 bug 真实发生过，由 `single_pixel_change_detected` 测试捕获。

### 实现注意事项

- 内部 `Vec` 必须复用（`self.changed.clear()`），不能每帧分配。
- 平台提供脏区时（Windows DXGI）**不要用 TileDiff**，直接用 `merge_rects` 合并，零 CPU 开销。

---

## 3. `rc-sched`（核心）

```rust
pub struct SchedulerConfig {
    pub max_in_flight: usize,        // 默认 2，硬上限 8
    pub target_latency_us: u32,      // 默认 22_000
    pub idr_request_threshold: u32,  // 默认 3
}
impl Default for SchedulerConfig;
impl SchedulerConfig { pub fn validate(&self) -> Result<(), ConfigError>; }

pub enum ConfigError { ZeroMaxInFlight, TooManyInFlight(usize), ZeroLatency }

pub enum Decision {
    Encode { seq: FrameSeq, deadline_us: u32 },
    Drop { seq: FrameSeq, reason: DropReason, needs_keyframe: bool },
}

pub enum DropReason { Repeat, Stale, Budget, Superseded }

pub struct SchedulerStats {
    pub captured: u64, pub encoded: u64,
    pub dropped_repeat: u64, pub dropped_stale: u64,
    pub dropped_budget: u64, pub dropped_superseded: u64,
    pub keyframe_requests: u64,
}
impl SchedulerStats {
    pub fn dropped_total(&self) -> u64;
    pub fn encode_ratio(&self) -> f64;
}

pub struct Scheduler { /* 私有 */ }
impl Scheduler {
    pub fn new(cfg: SchedulerConfig) -> Result<Self, ConfigError>;
    pub fn alloc_seq(&mut self) -> FrameSeq;
    pub fn on_captured(&mut self, seq: FrameSeq, kind: FrameKind,
                       is_repeat: bool, capture_ts: Timestamp,
                       now_us: u32) -> Decision;
    pub fn on_encoded(&mut self, seq: FrameSeq, now_us: u32) -> bool;
    pub fn take_keyframe_request(&mut self) -> bool;
    pub fn in_flight_len(&self) -> usize;
    pub fn in_flight_seqs(&self) -> Vec<FrameSeq>;
    pub fn in_flight_deadlines(&self) -> Vec<(FrameSeq, u32)>;
    pub fn stats(&self) -> SchedulerStats;
    pub fn config(&self) -> SchedulerConfig;
    pub fn reset(&mut self);
}
```

### 决策顺序（**不可调换**）

```
1. is_repeat          → Drop(Repeat, needs_keyframe=false)
2. now >= deadline    → Drop(Stale, needs_keyframe=kind.is_droppable())
3. in_flight 满:
   a. 存在可丢帧(P)   → 挤出最旧的 P，继续处理当前帧
   b. 全是 IDR        → Drop(Superseded, needs_keyframe=kind.is_droppable())
4. 接受               → Encode { seq, deadline_us }
```

### 调用者必须做的三件事

```rust
// 1. 每次 on_captured 后检查关键帧请求
let decision = sched.on_captured(...);
if sched.take_keyframe_request() {
    request_keyframe();   // 否则画面会花屏
}

// 2. 编码完成后必须调用 on_encoded（否则在途永不释放 → 画面卡死但 CPU 很低）
match encode(frame) {
    Ok(bytes) => { sched.on_encoded(seq, now_us); send(bytes); }
    Err(e) => { sched.on_encoded(seq, now_us); /* 记录错误 */ }
}

// 3. 时间基准必须一致：capture_ts 和 now_us 都是「相对会话起点的微秒」
```

### 为什么时间要注入

`on_captured` 不读系统时钟，全部由参数传入。这让 18 个测试全部精确、无 `sleep`、无 flaky。
**新写任何时序逻辑的 crate 都必须照这个模式。**

---

## 4. `rc-fec`

```rust
pub type FecGroupId = u16;

pub struct FecConfig {
    pub min_ratio: f32,        // 默认 0.0
    pub max_ratio: f32,        // 默认 0.30
    pub ratio_multiplier: f32, // 默认 1.5
    pub max_group_size: u8,    // 默认 16
    pub min_group_size: u8,    // 默认 4
}
impl FecConfig {
    pub fn ratio_for_loss(&self, loss_rate: f32) -> f32;
    pub fn parity_count(&self, data_packets: usize, ratio: f32) -> usize; // 0 或 1
}

pub struct FecGroup { /* 私有 */ }
impl FecGroup {
    pub fn new(group_id: FecGroupId) -> Self;
    pub fn group_id(&self) -> FecGroupId;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    pub fn is_sealed(&self) -> bool;
    pub fn push(&mut self, payload: &[u8]) -> bool;   // false = 已封组
    pub fn build_parity(&mut self) -> Option<Vec<u8>>;
    pub fn packets(&self) -> &[Vec<u8>];
}

pub struct FecRecoverer { /* 私有 */ }
impl FecRecoverer {
    pub fn new(group_id: FecGroupId, expected: usize) -> Self;
    pub fn group_id(&self) -> FecGroupId;
    pub fn push_data(&mut self, idx: usize, payload: Vec<u8>) -> bool;
    pub fn push_parity(&mut self, payload: Vec<u8>);
    pub fn try_recover(&self) -> RecoverResult;
}

pub enum RecoverResult {
    Complete(Vec<Vec<u8>>),
    Recovered(Vec<u8>, usize),   // (负载, 缺失位置)
    Unrecoverable,               // 缺 ≥2
    Incomplete,                  // 缺 1 但冗余未到
}
```

### 能力边界（写进文档，避免误用）

| 缺失包数 | 结果 |
|---|---|
| 0 | `Complete` |
| 1（冗余已到） | `Recovered` |
| 1（冗余未到） | `Incomplete` |
| ≥2 | `Unrecoverable` —— **不要重试，直接请求 IDR** |

---

## 5. `rc-proto`

```rust
pub const MAGIC: u32 = 0x3156_4352;   // "RCV1"
pub const HEADER_LEN: usize = 32;
pub const MAX_PAYLOAD: usize = 1200;

pub struct HeaderFlags(pub u8);
impl HeaderFlags {
    pub const IDR: u8 = 0x01;
    pub const LAST: u8 = 0x02;
    pub const HAS_DIRTY: u8 = 0x04;
    pub const PARITY: u8 = 0x08;
    pub const fn empty() -> Self;
    pub fn set(&mut self, bit: u8);
    pub fn clear(&mut self, bit: u8);
    pub const fn has(self, bit: u8) -> bool;
    pub const fn is_idr(self) -> bool;
    pub const fn is_last(self) -> bool;
    pub const fn is_parity(self) -> bool;
    pub fn from_kind(kind: FrameKind) -> Self;
}

pub struct MediaHeader {
    pub frame_seq: FrameSeq,
    pub capture_ts: Timestamp,
    pub encode_done_ts: Timestamp,
    pub packet_idx: u16,
    pub packet_cnt: u16,
    pub fec_group: u16,
    pub flags: HeaderFlags,
    pub layer_id: u8,
    pub payload_len: u16,
}
impl MediaHeader {
    pub fn write(&self, out: &mut [u8; HEADER_LEN]);              // 不校验
    pub fn write_checked(&self, out: &mut [u8; HEADER_LEN]) -> Result<()>;
    pub fn validate(&self) -> Result<()>;
    pub fn parse(buf: &[u8]) -> Result<Self>;
    pub fn parse_datagram(buf: &[u8]) -> Result<(Self, &[u8])>;
    pub const fn is_idr(&self) -> bool;
    pub const fn is_last(&self) -> bool;
}

pub fn split_frame(frame_seq: FrameSeq, kind: FrameKind,
                   capture_ts: Timestamp, encode_done_ts: Timestamp,
                   fec_group: u16, payload: &[u8],
                   max_payload: usize) -> Vec<Vec<u8>>;
```

逐字节规格见 [10-wire-format.md](10-wire-format.md)。**改线格式必须同步改那里。**

---

## 6. `rc-trace`

```rust
pub enum Mark { Captured, Diffed, EncodeStart, EncodeDone,
                Sent, Acked, Received, Decoded, Presented }
impl Mark {
    pub const ALL: [Mark; 9];
    pub const fn as_str(self) -> &'static str;
}

pub enum Outcome { Displayed, DroppedRepeat, DroppedStale, DroppedBudget,
                   DroppedSuperseded, LostInNetwork, DecodeError }
impl Outcome {
    pub const fn is_displayed(self) -> bool;
    pub const fn as_str(self) -> &'static str;
}

pub struct FrameTrace { /* 私有 */ }
impl FrameTrace {
    pub fn new(seq: FrameSeq) -> Self;
    pub fn seq(&self) -> FrameSeq;
    pub fn mark(&mut self, mark: Mark, ts: Timestamp);   // 重复打点保留首次
    pub fn at(&self, mark: Mark) -> Option<Timestamp>;
    pub fn finish(&mut self, outcome: Outcome);          // 重复设置保留首次
    pub fn outcome(&self) -> Option<Outcome>;
    pub fn end_to_end_us(&self) -> Option<u32>;          // Presented - Captured
    pub fn latency_breakdown(&self) -> Vec<(&'static str, u32)>;
    pub fn to_json(&self) -> String;
}

pub struct TraceCollector { /* 私有 */ }
impl TraceCollector {
    pub fn new(capacity: usize) -> Self;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    pub fn evicted(&self) -> u64;
    pub fn begin(&mut self, seq: FrameSeq) -> &mut FrameTrace;  // 满则淘汰最旧
    pub fn get(&self, seq: FrameSeq) -> Option<&FrameTrace>;
    pub fn frames(&self) -> &[FrameTrace];
    pub fn summary(&self) -> TraceSummary;
    pub fn export_jsonl(&self) -> String;
}

pub struct TraceSummary {
    pub frames: u64, pub displayed: u64,
    pub dropped_repeat: u64, pub dropped_stale: u64,
    pub dropped_budget: u64, pub dropped_superseded: u64,
    pub lost_in_network: u64, pub decode_errors: u64,
    pub unfinished: u64, pub evicted: u64,
    pub e2e_p50_us: Option<u32>, pub e2e_p95_us: Option<u32>, pub e2e_p99_us: Option<u32>,
}
impl TraceSummary {
    pub fn dropped_total(&self) -> u64;
    pub fn display_ratio(&self) -> f64;
    pub fn to_json(&self) -> String;
}
```

### 语义要点

- 每帧**必须恰好有一个** `Outcome`。`unfinished > 0` 说明埋点缺失。
- 百分位只统计 `Displayed` 帧（丢帧的 e2e 无意义）。
- `latency_breakdown` 段落名固定为：
  `diff / sched_queue / encode / packetize / network / decode / present / total`
  —— 这 8 个名字会被 `docs/impl/40-diagnostics.md` 的验收脚本解析，**不能改**。

---

## 7. 待实现 crate 的契约

以下签名是**契约**，实现必须以此为准。

### 7.1 `rc-capture`

```rust
/// 采集后端。所有实现必须满足：无屏幕变化时不产生 Frame。
pub trait CaptureBackend: Send {
    /// 阻塞等待下一帧。超时返回 Ok(None)（不是错误）。
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>>;
    /// 检查是否需要重建（分辨率变化、ACCESS_LOST 等）。
    fn poll_invalidation(&mut self) -> Option<InvalidationReason>;
    /// 重建内部资源。调用前必须已处理 poll_invalidation。
    fn rebuild(&mut self) -> Result<()>;
    /// 当前捕获的显示器信息。
    fn display_info(&self) -> DisplayInfo;
}

pub struct CapturedFrame {
    pub texture: FrameTexture,
    pub timestamp: Timestamp,      // 内容变化时刻，不是采集返回时刻
    pub dirty: Option<Rect>,       // 平台提供时填写（dxgi/r14 有）
    pub seq: CaptureSeq,
    pub is_repeat: bool,           // 平台能判断时填写
}

pub enum FrameTexture {
    Cpu(CpuFrame),                 // 兜底：软件采集
    #[cfg(windows)] D3D11(windows::Win32::Graphics::Direct3D11::ID3D11Texture2D),
    #[cfg(target_os = "macos")] CVPixelBuffer(*mut std::ffi::c_void),
    #[cfg(target_os = "linux")] DmaBuf(DmaBufHandle),
}

pub enum InvalidationReason {
    ResolutionChanged, DisplayChanged, SessionLocked,
    AccessLost, DeviceRemoved, HdrToggled, Unknown,
}

pub struct DisplayInfo { pub id: DisplayId, pub resolution: Resolution,
                         pub scale: f32, pub hdr: bool }
```

**硬性要求：**

| 编号 | 要求 | 违反后果 |
|---|---|---|
| C1 | 无变化时不返回帧（阻塞到超时） | 静态画面 CPU 满载 |
| C2 | `timestamp` 是内容变化时刻 | 延迟测量偏大，无法优化 |
| C3 | 所有 GPU 资源用 RAII guard 释放 | `DXGI_ERROR_INVALID_CALL` 后采集永久停止 |
| C4 | `ACCESS_LOST` 映射为 `recoverable: true` | 锁屏后永久黑屏 |
| C5 | `rebuild()` 幂等，可连续调用 | 崩溃 |
| C6 | 采集线程独立，不阻塞调用方 | 编码阻塞采集 |

### 7.2 `rc-encode`

```rust
pub trait Encoder: Send {
    fn encode(&mut self, frame: &CapturedFrame, force_idr: bool,
              deadline_ts: Timestamp) -> Result<EncodedFrame>;
    fn capabilities(&self) -> EncoderCaps;
    fn set_bitrate(&mut self, bps: u32) -> Result<()>;
    fn request_idr(&mut self);
    fn flush(&mut self) -> Result<Vec<EncodedFrame>>;
}

pub struct EncodedFrame {
    pub data: Vec<u8>,             // Annex-B 或 AVCC，由 caps 声明
    pub kind: FrameKind,
    pub is_keyframe: bool,
    pub encode_done_ts: Timestamp,
}

pub struct EncoderCaps {
    pub codec: Codec,              // H264 / Hevc / Av1
    pub hardware: bool,
    pub max_resolution: Resolution,
    pub bitstream_format: BitstreamFormat,   // AnnexB / Avcc
    pub profiles: &'static [u8],
}

pub struct EncoderConfig {
    pub codec: Codec,
    pub resolution: Resolution,
    pub fps: u32,
    pub bitrate_bps: u32,
    pub preset: Preset,            // P1..P7（NVENC）/ Speed..Quality（VT）
    pub low_latency: bool,         // 必须 true
    pub gop_len: u32,              // 建议 0 = 无限 GOP，按需 IDR
    pub b_frames: u32,             // 必须 0
}
```

**硬性要求：**

| 编号 | 要求 | 违反后果 |
|---|---|---|
| E1 | `b_frames == 0` | 延迟 +1~4 帧 |
| E2 | 关闭 look-ahead / 2-pass | 固定延迟 |
| E3 | 输出零延迟（`outputDelay=0`） | 编码器内部缓冲 |
| E4 | 编码器返回的帧顺序 == 输入顺序 | 解码端乱序 |
| E5 | `deadline_ts` 超出时返回 `Error::Timeout`，不阻塞 | 调度器失效 |
| E6 | 启动时自检（编一帧假数据并解码验证） | 花屏无法定位 |

### 7.3 `rc-net`

```rust
pub struct NetConfig {
    pub role: Role,                  // Host / Controller
    pub local_addr: SocketAddr,
    pub peer_addr: Option<SocketAddr>,
    pub mtu: usize,                  // 默认 1200
}

pub trait Transport: Send {
    /// 发送媒体数据报（不可靠）
    fn send_datagram(&mut self, data: &[u8]) -> Result<()>;
    /// 发送控制/输入（可靠有序）
    fn send_reliable(&mut self, data: &[u8]) -> Result<()>;
    /// 非阻塞接收。返回所有已就绪消息。
    fn poll(&mut self, timeout: Duration) -> Result<Vec<NetEvent>>;
    /// 网络统计（用于拥塞控制与诊断）
    fn stats(&self) -> NetStats;
}

pub enum NetEvent {
    Datagram(Vec<u8>),
    Reliable(Vec<u8>),
    Connected,
    Disconnected { reason: String },
    PathChanged,                     // 连接迁移
}

pub struct NetStats {
    pub rtt_us: u32,
    pub loss_rate: f32,
    pub pacing_rate_bps: u64,
    pub cwnd_bytes: u64,
    pub bytes_sent: u64,
    pub bytes_recv: u64,
}
```

**硬性要求：**

| 编号 | 要求 |
|---|---|
| N1 | 媒体走 datagram，控制走 reliable stream（不可混用） |
| N2 | 单数据报 ≤ `mtu - 48`（QUIC 开销） |
| N3 | pacing 必须实现（不能一轮发完） |
| N4 | `poll` 不阻塞超过 timeout |
| N5 | 连接迁移（`PathChanged`）后媒体流不中断 |

### 7.4 `rc-loopback`（M1 的关键设施）

```rust
/// 在单进程内跑通采集→编码→解码→呈现，并注入网络损伤。
pub struct Loopback {
    pub fn new(cfg: LoopbackConfig) -> Result<Self>;
    pub fn run(&mut self, duration: Duration) -> Result<LoopbackReport>;
}

pub struct LoopbackConfig {
    pub resolution: Resolution,
    pub fps: u32,
    pub duration: Duration,
    pub inject_loss: f32,        // 0.03 = 3%
    pub inject_rtt_us: u32,      // 单程延迟的一半
    pub inject_jitter_us: u32,
    pub bandwidth_bps: u64,      // 0 = 不限
    pub encoder: Option<String>, // None = 自动选择
    pub trace_capacity: usize,   // 默认 4096
}

pub struct LoopbackReport {
    pub summary: TraceSummary,      // 来自 rc-trace
    pub displayed_fps: f64,
    pub sched_stats: SchedulerStats, // 来自 rc-sched
    pub capture_frames: u64,        // 采集帧数（应 >> 编码帧数）
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub passed: bool,               // 是否满足 Gate
}
```

**这是让 AI 能自主验证的设施。** 输出必须是 JSON，字段名固定（见 [40-diagnostics.md](40-diagnostics.md)）。

---

## 8. 实现新 crate 的检查清单

- [ ] crate 头部有 `#![deny(missing_docs)]`、`#![deny(unsafe_code)]`（有 unsafe 时改为限定模块）
- [ ] `Cargo.toml` 使用 `version.workspace = true` 等 workspace 字段
- [ ] 依赖只向下（见 AGENTS.md 的依赖图）
- [ ] 公开 API 有 doc comment，说明**为什么**而不只是**是什么**
- [ ] 时间通过参数注入，不读 `Instant::now()`
- [ ] 错误用 `thiserror` 枚举，不跨 crate 用 `Box<dyn Error>`
- [ ] 有「不变量违反」的测试，不只测 happy path
- [ ] 有边界测试：0、最大值、空输入、越界、奇数尺寸
- [ ] `cargo test -p <crate>` 全绿
- [ ] `cargo clippy -p <crate> --all-targets -- -D warnings` 无警告
