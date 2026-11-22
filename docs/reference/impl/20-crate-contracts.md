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
    // 平台变体只携带不透明句柄：抽象层不引 windows 等平台依赖，
    // 由各平台 crate 负责句柄 ↔ 原生对象（ID3D11Texture2D* 等）的
    // 类型还原与引用计数（见 rc-capture crate 文档「纹理载体」）。
    #[cfg(windows)] D3D11(*mut std::ffi::c_void),   // ID3D11Texture2D* 不透明句柄
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
    /// 发送控制消息（可靠有序）
    fn send_reliable(&mut self, data: &[u8]) -> Result<()>;
    /// 发送输入事件（可靠有序、独立 urgent 流）。语义：输入消息永不排在
    /// 控制消息之后（控制流被大流量灌满时不受队头阻塞）。
    /// 默认实现委托 send_reliable，单流实现与测试替身无需适配即可编译。
    fn send_reliable_urgent(&mut self, data: &[u8]) -> Result<()> {
        self.send_reliable(data)
    }
    /// 非阻塞接收。返回所有已就绪消息。
    fn poll(&mut self, timeout: Duration) -> Result<Vec<NetEvent>>;
    /// 网络统计（用于拥塞控制与诊断）
    fn stats(&self) -> NetStats;
}

pub enum NetEvent {
    Datagram(Vec<u8>),
    Reliable(Vec<u8>),
    ReliableUrgent(Vec<u8>),         // 输入独立流（不与控制共享队列）
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
| N6 | 输入走独立 urgent 流（`send_reliable_urgent` → `NetEvent::ReliableUrgent`），不得排在控制消息之后（队头阻塞） |

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
    pub inject_loss: f32,        // 0.0–1.0，0.03 = 3%
    pub inject_rtt_us: u32,      // 往返延迟（微秒），单程 = 该值 / 2
    pub inject_jitter_us: u32,   // 抖动（微秒）
    pub bandwidth_bps: u64,      // 0 = 不限
    pub encoder_slowdown: f32,   // 编码减速倍数，1.0 = 正常；JSON 的 config.encoder 由它派生
    pub static_content: bool,    // 画面静止（Q5：静止零编码）
    pub seed: u64,               // 丢包序列随机种子，默认 42（固定后结果可复现）
    pub trace_capacity: usize,   // 帧时间线容量，默认 4096
    pub max_queue_depth: usize,  // 接收队列深度硬上限，默认 2（Q3，不要调大）
    pub probe_enabled: bool,     // 网络延迟探针开关，默认 false（Q2：帧率判定不依赖探针）
}

pub struct LoopbackReport {
    pub config: LoopbackConfig,     // 本次运行的配置快照
    pub duration_s: f64,            // 实际运行时长（秒）
    pub summary: TraceSummary,      // 来自 rc-trace（全量计数器，不受 trace_capacity 截断）
    pub sched_stats: SchedulerStats, // 来自 rc-sched
    pub latency_p50_ms: f64,        // 端到端延迟分位数（毫秒）
    pub latency_p95_ms: f64,
    pub latency_p99_ms: f64,
    pub displayed_fps: f64,         // 实际呈现帧率
    pub encoded_fps: f64,           // 实际编码帧率
    pub captured_fps: f64,          // 实际采集帧率（静态场景应 > encoded，见 40-diagnostics.md §4.4）
    pub breakdown_ms: BreakdownMs,  // 分段延迟：diff/sched_queue/encode/packetize/network/decode/present
    pub convergence_ms: Option<u64>, // 达到 target × 0.9 的时刻；None = 未达到
    pub degradation_ms: Option<u64>, // 帧率跌破 50% 的时刻；None = 全程未低于
    pub max_queue_depth_seen: u32,  // 运行期间观测到的最大队列深度
    pub probe_enabled: bool,        // 取自配置
    pub static_cpu_pct: Option<f64>, // 静态画面 CPU 实测；刻意不进 JSON，只参与 Gate 判定
    pub gate: GateResult,           // Gate 判定（passed 在 gate.passed，不是顶层字段）
}
```

**这是让 AI 能自主验证的设施。** 输出必须是 JSON，字段名固定（见 [40-diagnostics.md](40-diagnostics.md) §4）。

### 7.5 `rc-session`（M2 会话层，新建；T13–T20）

> workspace 第 22 个成员，M2 会话层的家：控制协议线格式（T13）、Host/Client
> 会话状态机与握手（T14）、RTT/时钟偏移探测与初始码率（T15）、IDR 看门狗与
> 关联确认（T16）、断线重连与时钟饱和守卫（T17）、CLI 集成出口/报告/开发证书
> （T18）、密码鉴权与连接审批（T19）、剪贴板文本同步（T20）。
> 依赖只向下：`rc-types` / `rc-net` / `rc-pipeline` / `rc-input` / `rc-trace`
> （T19 另加 `rand = "0.8"` 仅挑战盐；T20 的 macOS 侧加 `core-foundation = "0.10"`）。
> 硬性要求、测试清单与验收命令见各任务卡，本节只锁公开签名；
> `TYPE_*` 类型号一经发布冻结不改、不复用。

#### proto.rs（T13 交付；T19/T20 增量变体）

```rust
pub const PROTOCOL_VERSION: u8 = 1;

// 类型号：0x01..=0x0C 由 T13 实装；0x0D..=0x0F 占号冻结，T19/T20 增量实装
pub const TYPE_HELLO: u8 = 0x01;
pub const TYPE_WELCOME: u8 = 0x02;
pub const TYPE_IDR_REQUEST: u8 = 0x03;
pub const TYPE_IDR_ACK: u8 = 0x04;
pub const TYPE_PROBE_PING: u8 = 0x05;
pub const TYPE_PROBE_PONG: u8 = 0x06;
pub const TYPE_SET_BITRATE: u8 = 0x07;
pub const TYPE_BW_PROBE_REQUEST: u8 = 0x08;
pub const TYPE_BW_PROBE_DATA: u8 = 0x09;
pub const TYPE_INPUT_ACK: u8 = 0x0A;
pub const TYPE_RECONNECT_REQUEST: u8 = 0x0B;
pub const TYPE_BYE: u8 = 0x0C;
pub const TYPE_AUTH_CHALLENGE: u8 = 0x0D;    // T19 实装
pub const TYPE_AUTH_RESPONSE: u8 = 0x0E;      // T19 实装
pub const TYPE_CLIPBOARD_NOTIFY: u8 = 0x0F;   // T20 实装

pub const MAX_BW_PROBE_BYTES: u32 = 256 * 1024;
pub const AUTH_SALT_LEN: usize = 16;              // T19
pub const AUTH_PROOF_LEN: usize = 32;             // T19
pub const MAX_AUTH_ITERATIONS: u32 = 1_000_000;   // T19
pub const MAX_CLIPBOARD_TEXT_BYTES: usize = u16::MAX as usize;  // T20，单一事实源

pub struct SessionParams { pub width: u32, pub height: u32,
                           pub fps: u32, pub bitrate_bps: u32 }

pub enum AuthMethod { None = 0, Password = 1 }
pub enum AuthStatus { Ok = 0, ChallengeRequired = 1, Rejected = 2 }
pub enum IdrReason { DecoderNeedKeyframe = 0, WatchdogStall = 1, ReconnectRecovery = 2 }
pub enum BitrateReason { Initial = 0, Probe = 1, Manual = 2 }
pub enum ByeReason { Normal = 0, Refresh = 1 }

pub struct HostFlags(pub u8);
impl HostFlags {
    pub const RESUME_ACCEPTED: u8 = 0b0000_0001;
    pub const CLOCK_EPOCH_RESET: u8 = 0b0000_0010;
    pub const KNOWN_MASK: u8 = Self::RESUME_ACCEPTED | Self::CLOCK_EPOCH_RESET;
    pub const fn resume_accepted(&self) -> bool;
    pub const fn clock_epoch_reset(&self) -> bool;
}

pub struct Hello { pub version: u8, pub session_id: u64, pub requested: SessionParams,
                   pub auth_method: AuthMethod, pub auth_payload: Vec<u8> }
pub struct Welcome { pub session_id: u64, pub granted: SessionParams,
                     pub auth_status: AuthStatus, pub host_flags: HostFlags }

pub struct AuthChallenge { pub salt: [u8; AUTH_SALT_LEN], pub iterations: u32 }  // T19
pub struct AuthResponse { pub proof: [u8; AUTH_PROOF_LEN] }                      // T19

pub enum ControlMessage {
    Hello(Hello), Welcome(Welcome),
    IdrRequest { reason: IdrReason },
    IdrAck { frame_seq: u64 },
    ProbePing { probe_id: u32, sender_clock_us: u64 },
    ProbePong { probe_id: u32, echoed_clock_us: u64, responder_clock_us: u64 },
    SetBitrate { bps: u32, reason: BitrateReason },
    BwProbeRequest { probe_id: u32, total_bytes: u32 },
    BwProbeData { probe_id: u32, chunk_idx: u32, padding: Vec<u8> },  // 唯一走 datagram
    InputAck { key_batch_seq: u64, inject_done_us: u64 },
    ReconnectRequest { session_id: u64, resume: bool },
    Bye { reason: ByeReason },
    AuthChallenge(AuthChallenge),      // T19，走 reliable
    AuthResponse(AuthResponse),        // T19，走 reliable
    ClipboardNotify { text: String },  // T20，走 reliable
}

impl ControlMessage {
    /// 追加式编码，不失败、不 panic（对齐 rc-input encode_events）。
    pub fn encode(&self, out: &mut Vec<u8>);
    /// 宁拒不猜：未知/预留类型号、版本不符、枚举越界、布尔非 0/1、
    /// flags 未知位、变长段不符、截断/尾字节一律 Error::Protocol（带 offset）。
    pub fn decode(buf: &[u8]) -> Result<Self>;
    /// T20：剪贴板通知唯一合法构造路径，>MAX_CLIPBOARD_TEXT_BYTES 拒绝（宁不传不截断）。
    pub fn clipboard_notify(text: String) -> Result<Self>;
}
```

线格式总表（多字节整数一律 LE；时钟字段一律 u64——不用 u32 `Timestamp`，饱和守卫是 T17 会话层职责）：

| 类型 | 消息 | 布局（type 之后） | 定长 |
|---|---|---|---|
| 0x01 | Hello | `version u8` `session_id u64` `w/h/fps/bitrate u32×4` `auth_method u8` `auth_len u16` `payload[len]` | 29+len |
| 0x02 | Welcome | `session_id u64` `w/h/fps/bitrate u32×4` `auth_status u8` `host_flags u8` | 27 |
| 0x03 | IdrRequest | `reason u8` | 2 |
| 0x04 | IdrAck | `frame_seq u64` | 9 |
| 0x05 | ProbePing | `probe_id u32` `sender_clock_us u64` | 13 |
| 0x06 | ProbePong | `probe_id u32` `echoed u64` `responder u64` | 21 |
| 0x07 | SetBitrate | `bps u32` `reason u8` | 6 |
| 0x08 | BwProbeRequest | `probe_id u32` `total_bytes u32`（≤256KB） | 9 |
| 0x09 | BwProbeData | `probe_id u32` `chunk_idx u32` `padding`（剩余字节，无前缀） | 9+n |
| 0x0A | InputAck | `key_batch_seq u64` `inject_done_us u64` | 17 |
| 0x0B | ReconnectRequest | `session_id u64` `resume u8`（严格 0/1） | 10 |
| 0x0C | Bye | `reason u8` | 2 |
| 0x0D | AuthChallenge | `salt u8[16]` `iterations u32`（1..=1_000_000） | 21 |
| 0x0E | AuthResponse | `proof u8[32]` | 33 |
| 0x0F | ClipboardNotify | `len u16` `text[len]`（UTF-8 字节，≤65535） | 3+len |

#### 会话状态机（T14 交付；T17 增补两态、T19 增鉴权字段）

```rust
/// 会话时钟（照 run_real 的 clock Arc 模式，由构造方注入；铁律 4）。
pub type SessionClock = std::sync::Arc<dyn Fn() -> u32 + Send + Sync>;

pub enum SessionState { Init, Connected, Streaming, Closed,
                        Detached,       // T17：host 断线保持窗口
                        Reconnecting }  // T17：client 退避重连
pub enum StopReason { Requested, ByeReceived(ByeReason), Disconnected(String) }

pub struct HostSessionConfig {
    pub listen_addr: SocketAddr,
    pub mtu: usize,                       // 默认 rc_net::quic::DEFAULT_MTU
    pub caps: SessionParams,              // Welcome 裁定时逐字段 min 夹紧上界
    pub pipeline: rc_pipeline::PipelineConfig,
    pub handshake_timeout: Duration,      // 默认 5s
    pub injector: Box<dyn rc_input::InputInjector>,
    pub pace_capture: bool,               // 假后端必须 true
    pub auth: HostAuthConfig,             // T19 增
    pub approver: Box<dyn ConnectionApprover>,  // T19 增，默认 AutoApprover
}
impl HostSessionConfig {
    pub fn new(listen_addr: SocketAddr, caps: SessionParams,
               injector: Box<dyn rc_input::InputInjector>) -> Self;
}

pub struct HostSession { /* 私有 */ }
impl HostSession {
    pub fn new(cfg: HostSessionConfig, cert_pem: &str, key_pem: &str,
               clock: SessionClock) -> Result<Self>;
    pub fn with_parts(cfg: HostSessionConfig, transport: Box<dyn rc_net::Transport>,
                      capture: Box<dyn rc_capture::CaptureBackend>,
                      encoder: Box<dyn rc_encode::Encoder>,
                      clock: SessionClock) -> Result<Self>;
    pub fn accept_and_handshake(&mut self, timeout: Duration) -> Result<SessionParams>;
    pub fn handshake(&mut self) -> Result<SessionParams>;
    pub fn state(&self) -> SessionState;
    pub fn session_id(&self) -> Option<u64>;
    pub fn negotiated(&self) -> Option<SessionParams>;
    /// 一拍：transport.poll 单点三通道分发 + pipeline.tick 产出逐条发 datagram。
    pub fn tick(&mut self, poll_budget: Duration) -> Result<HostTickReport>;
    pub fn close(&mut self, reason: ByeReason) -> Result<HostRunSummary>;
    pub fn run(&mut self, stop: &mut dyn FnMut() -> bool) -> Result<HostRunSummary>;
    pub fn pipeline(&self) -> &rc_pipeline::HostPipeline;
}
pub struct HostTickReport { pub datagrams_sent: usize, pub events_injected: usize,
    pub control_msgs_handled: usize, pub busy: bool, pub closed: Option<StopReason> }
pub struct HostRunSummary { pub stop_reason: StopReason, pub datagrams_sent: usize,
    pub events_injected: u64, pub frames_encoded: u64, pub unfinished_traces: u64 }

pub struct ClientSessionConfig {
    pub peer_addr: SocketAddr,
    pub mtu: usize,
    pub requested: SessionParams,         // 裁定后以 Welcome 回值为准
    pub session_id: u64,
    pub pipeline: rc_pipeline::PipelineConfig,
    pub connect_timeout: Duration,        // 默认 10s
    pub handshake_timeout: Duration,      // 默认 5s（T22 审批场景放宽，见 T22 卡）
    pub capture: Box<dyn rc_input::InputCapture>,
    pub input_window: Duration,           // 默认 rc_input::DEFAULT_WINDOW
    pub auth: ClientAuthConfig,           // T19 增
}

pub struct ClientSession { /* 私有 */ }
impl ClientSession {
    pub fn new(cfg: ClientSessionConfig, server_cert_pem: &str, server_name: &str,
               clock: SessionClock) -> Result<Self>;
    pub fn with_parts(cfg: ClientSessionConfig, transport: Box<dyn rc_net::Transport>,
                      decoder: Box<dyn rc_decode::Decoder>,
                      renderer: Box<dyn rc_render::Renderer>,
                      clock: SessionClock) -> Result<Self>;
    pub fn connect_and_handshake(&mut self) -> Result<SessionParams>;
    pub fn handshake(&mut self) -> Result<SessionParams>;
    pub fn state(&self) -> SessionState;
    pub fn session_id(&self) -> u64;
    pub fn negotiated(&self) -> Option<SessionParams>;
    pub fn tick(&mut self, poll_budget: Duration, now: std::time::Instant)
        -> Result<ClientTickReport>;
    pub fn request_idr(&mut self, reason: IdrReason) -> Result<()>;
    pub fn close(&mut self, reason: ByeReason) -> Result<ClientRunSummary>;
    pub fn run(&mut self, stop: &mut dyn FnMut() -> bool) -> Result<ClientRunSummary>;
    pub fn pipeline(&self) -> &rc_pipeline::ControllerPipeline;
}
pub struct ClientTickReport { pub datagrams_pushed: usize,
    pub frames_displayed_delta: u64, pub input_bytes_sent: usize,
    pub idr_requests_sent: usize, pub busy: bool, pub closed: Option<StopReason> }
pub struct ClientRunSummary { pub stop_reason: StopReason, pub frames_displayed: u64,
    pub input_bytes_sent: u64, pub idr_requests_sent: u64, pub unfinished_traces: u64 }
```

#### probe.rs（T15：RTT/偏移探测 + 带宽粗测 + 初始码率）

```rust
pub const PROBE_INTERVAL_US: u64 = 1_000_000;        // 1s，兼心跳
pub const PROBE_TIMEOUT_US: u64 = 3_000_000;
pub const PROBE_MEDIAN_WINDOW: usize = 8;
pub const BW_PROBE_MAX_TOTAL_BYTES: u32 = 256 * 1024;
pub const BW_PROBE_CHUNK_PAYLOAD: usize = 1024;      // 单块总长 1033 ≤ 1120（N2 预算）
pub const BW_PROBE_SETTLE_TIMEOUT_US: u64 = 2_000_000;
pub const BW_PROBE_MIN_SPAN_US: u64 = 1_000;         // 退化门槛，防除零
pub const INITIAL_BITRATE_MIN_BPS: u64 = 2_000_000;
pub const INITIAL_BITRATE_FACTOR: f64 = 0.7;

pub struct ProbeSample { pub probe_id: u32, pub rtt_us: u64, pub offset_us: i64 }
pub struct ProbeStats { pub rtt_us_median: Option<u64>, pub offset_us_median: Option<i64>,
    pub pings_sent: u64, pub pongs_received: u64, pub probes_lost: u64 }
pub struct ProbeEngine { /* 私有 */ }
impl ProbeEngine {
    pub fn new() -> Self;
    pub fn pump(&mut self, now_us: u64) -> Option<ProbePing>;
    pub fn on_pong(&mut self, pong: &ProbePong, now_us: u64) -> Option<ProbeSample>;
    pub fn to_local_us(&self, remote_us: u64) -> Option<u64>;
    pub fn stats(&self) -> ProbeStats;
}
pub fn pong_for(ping: &ProbePing, responder_now_us: u64) -> ProbePong;
pub fn bw_probe_chunks(probe_id: u32, total_bytes: u32) -> Result<Vec<Vec<u8>>>;
pub fn parse_bw_datagram(buf: &[u8]) -> Result<BwProbeData>;
pub struct BwOutcome { pub probe_id: u32, pub estimate_bps: Option<u64>,
    pub received_bytes: u64, pub expected_bytes: u64, pub completed: bool }
pub struct BwEstimator { /* 私有 */ }
impl BwEstimator {
    pub fn start(&mut self, probe_id: u32, expected_total_bytes: u32, now_us: u64);
    pub fn on_chunk(&mut self, data: &BwProbeData, now_us: u64) -> Option<BwOutcome>;
    pub fn pump(&mut self, now_us: u64) -> Option<BwOutcome>;
    pub fn is_settled(&self) -> bool;
}
pub fn initial_bitrate_from(estimate_bps: Option<u64>, host_cap_bps: u32) -> Option<u32>;

// ClientSession 增量（T15）
impl ClientSession {
    pub fn pump_probes(&mut self, now_us: u64) -> Result<()>;
    pub fn on_pong(&mut self, pong: &ProbePong, now_us: u64);
    pub fn start_bw_probe(&mut self, now_us: u64) -> Result<()>;
    pub fn on_bw_datagram(&mut self, buf: &[u8], now_us: u64) -> Result<()>;
    pub fn host_to_local_us(&self, host_us: u32) -> Option<u64>;
    pub fn probe_snapshot(&self) -> ProbeSnapshot;
}
pub struct ProbeSnapshot { pub rtt_us_median: Option<u64>,
    pub offset_us_median: Option<i64>, pub bw_estimate_bps: Option<u64>,
    pub initial_bitrate_bps: Option<u32>, pub negotiated_bitrate_bps: u32,
    pub pings_sent: u64, pub pongs_received: u64, pub probes_lost: u64 }

// HostSession 增量（T15）
impl HostSession {
    pub fn on_set_bitrate(&mut self, bps: u32, reason: u8) -> Result<()>;
    pub fn on_bw_probe_request(&mut self, req: &BwProbeRequest) -> Result<()>;
    pub fn current_bitrate_bps(&self) -> u32;
}
```

#### watchdog.rs（T16：停滞看门狗 + IdrRequest 去抖 + IdrAck 关联确认）

```rust
pub const DEFAULT_STALL_US: u32 = 500_000;            // 与 rc-net FRAME_STALE_US 刻意对齐
pub const DEFAULT_ACK_MARGIN_US: u32 = 50_000;
pub const DEFAULT_MIN_RETRY_INTERVAL_US: u32 = 100_000;

pub struct DeliveredFrame { pub seq: FrameSeq, pub is_idr: bool, pub at_us: u32 }
pub struct DeliveryTracker { /* 私有；在途残帧 ≤2（铁律 1），完成历史环形 8 */ }
impl DeliveryTracker {
    pub fn new() -> Self;
    pub fn observe(&mut self, datagram: &[u8], now_us: u32) -> Result<()>;
    pub fn last_complete(&self) -> Option<DeliveredFrame>;
    pub fn completed(&self, seq: FrameSeq) -> Option<DeliveredFrame>;
    pub fn gc(&mut self, now_us: u32);                // u32::wrapping_sub 回绕免疫
}
pub struct StallWatchdog { /* 私有 */ }
impl StallWatchdog {
    pub fn new(stall_after_us: u32) -> Self;
    pub fn arm(&mut self, now_us: u32);               // 进入 Streaming 时锚基线
    pub fn on_frame_complete(&mut self, now_us: u32);
    pub fn poll(&mut self, now_us: u32) -> bool;      // 触发后自锚定（500ms 一报）
    pub fn fires(&self) -> u64;
}
pub struct IdrControllerConfig { pub ack_margin_us: u32,
                                 pub min_retry_interval_us: u32 }
pub struct SendIdrRequest { pub reason: crate::proto::IdrReason, pub attempt: u8 }
pub struct IdrStats { pub requests_sent: u64, pub requests_by_reason: [u64; 3],
    pub retries: u64, pub acks_received: u64, pub resolved_by_idr: u64 }
pub struct IdrController { /* 私有；Idle→InFlight→AwaitFrame 状态机 */ }
impl IdrController {
    pub fn new(cfg: IdrControllerConfig) -> Self;
    pub fn request(&mut self, reason: crate::proto::IdrReason, now_us: u32);
    pub fn on_ack(&mut self, frame_seq: FrameSeq, now_us: u32);
    pub fn on_frame_complete(&mut self, frame: DeliveredFrame);
    pub fn poll(&mut self, now_us: u32, rtt_us: u32) -> Option<SendIdrRequest>;  // 唯一发送出口
    pub fn stats(&self) -> IdrStats;
}
pub fn idr_frame_seqs(datagrams: &[Vec<u8>]) -> Vec<FrameSeq>;
```

#### reconnect.rs（T17：保持窗口 + 指数退避 + 时钟饱和守卫）

```rust
pub const DEFAULT_RESUME_TTL: Duration = Duration::from_secs(10);
pub const DEFAULT_BACKOFF_INITIAL: Duration = Duration::from_millis(500);
pub const DEFAULT_BACKOFF_MAX: Duration = Duration::from_secs(8);
pub const DEFAULT_BACKOFF_MAX_ATTEMPTS: u32 = 5;
pub const REFRESH_GUARD_US: u64 = 4_200_000_000;      // 70min，距 u32::MAX 留 ≈95s

pub struct ReconnectConfig { pub resume_ttl: Duration, pub backoff_initial: Duration,
    pub backoff_max: Duration, pub backoff_max_attempts: u32, pub refresh_guard_us: u64 }
impl Default for ReconnectConfig;
pub struct ReconnectStats { pub disconnects: u32, pub resumes: u32, pub rebuilds: u32,
    pub reconnect_attempts: u32, pub path_changes: u32, pub refreshes: u32 }
pub struct ResumeWindow { /* 私有 */ }
impl ResumeWindow {
    pub fn new(ttl: Duration) -> Self;
    pub fn detach(&mut self, now_us: u64);            // 幂等，不续期
    pub fn is_open(&self, now_us: u64) -> bool;
    pub fn remaining_ms(&self, now_us: u64) -> u64;
}
pub struct Backoff { /* 私有；不持内部时钟（铁律 4） */ }
impl Backoff {
    pub fn new(initial: Duration, max: Duration, max_attempts: u32) -> Result<Self>;
    pub fn delay_before(&self, attempt: u32) -> Duration;
    pub fn on_failure(&mut self) -> Option<Duration>;
    pub fn reset(&mut self);
    pub fn attempts_used(&self) -> u32;
}
pub struct RefreshGuard { /* 私有 */ }
impl RefreshGuard {
    pub fn new(guard_us: u64) -> Self;
    pub fn due(&self, now_us: u64) -> bool;
}

pub enum DetachOutcome { Waiting, TornDown, Resumed, Replaced }
impl HostSession {   // T17 增量
    pub fn set_reconnect(&mut self, cfg: ReconnectConfig);
    pub fn reconnect_config(&self) -> &ReconnectConfig;
    pub fn reconnect_stats(&self) -> ReconnectStats;
    pub fn pipeline_generation(&self) -> u32;         // 「管线不拆」的可观测判据
    pub fn set_pipeline_factory(&mut self, f: Box<dyn FnMut() -> Result<HostPipeline> + Send>);
    pub fn on_disconnected(&mut self, now_us: u64);
    pub fn tick_detached(&mut self, now_us: u64) -> Result<DetachOutcome>;
    pub fn handle_reconnect(&mut self, req: &ReconnectRequest, now_us: u64) -> Result<Welcome>;
    pub fn check_refresh(&mut self, now_us: u64) -> Result<bool>;
}
pub enum ReconnectOutcome { Waiting { next_attempt_in_ms: u64 }, Connecting,
                            Resumed, Rebuilt, GaveUp }
impl ClientSession {  // T17 增量
    pub fn set_reconnect(&mut self, cfg: ReconnectConfig);
    pub fn reconnect_config(&self) -> &ReconnectConfig;
    pub fn reconnect_stats(&self) -> ReconnectStats;
    pub fn pipeline_generation(&self) -> u32;
    pub fn set_pipeline_factory(&mut self, f: Box<dyn FnMut() -> Result<ControllerPipeline> + Send>);
    pub fn on_disconnected(&mut self, now_us: u64);
    pub fn tick_reconnect(&mut self, now_us: u64) -> Result<ReconnectOutcome>;
    pub fn on_bye_refresh(&mut self, now_us: u64) -> Result<()>;  // 零退避全新重连
}
```

#### auth.rs（T19：手写 PBKDF2 挑战应答 + 全项目唯一审批抽象）

```rust
pub const DEFAULT_AUTH_ITERATIONS: u32 = 10_000;
pub const DEFAULT_AUTH_TIMEOUT_US: u64 = 30_000_000;

pub fn compute_proof(password: &[u8], salt: &[u8; AUTH_SALT_LEN],
                     iterations: u32) -> [u8; AUTH_PROOF_LEN];
pub fn verify_proof(password: &[u8], salt: &[u8; AUTH_SALT_LEN], iterations: u32,
                    proof: &[u8; AUTH_PROOF_LEN]) -> bool;  // 常数时间比较

pub struct HostAuthConfig { pub password: Option<String>, pub iterations: u32,
                            pub approval_timeout_us: u64 }
pub struct ClientAuthConfig { pub password: Option<String>, pub auth_timeout_us: u64 }

pub struct ApprovalRequest { pub session_id: u64,
    pub remote_addr: std::net::SocketAddr,     // accept 得到的传输层地址，非自报
    pub requested: SessionParams, pub auth_method: AuthMethod,
    pub password_verified: bool }
pub enum ApprovalVerdict { Accepted, Rejected }  // 超时不在此枚举，会话层判 Rejected

/// 连接审批人（M2-7 接受弹窗的联动位）。三方法全部非阻塞立即返回；
/// 超时判定只在 HostAuthenticator（注入 now_us），实现方不感知超时。
pub trait ConnectionApprover: Send {
    fn submit(&mut self, req: &ApprovalRequest);
    fn poll_verdict(&mut self) -> Option<ApprovalVerdict>;
    fn cancel(&mut self);
}
pub struct AutoApprover;      // submit 即 Accepted（headless 默认，T18 行为不变）
pub struct DenyApprover;      // 一律拒绝（--approval deny）
pub struct ScriptedApprover { /* 私有 */ }
impl ScriptedApprover { pub fn new(responses: Vec<Option<ApprovalVerdict>>) -> Self; }

pub enum HostAuthAction { NotifyPending, Challenge(AuthChallenge),
                          Conclude(AuthStatus), Wait }
pub struct HostAuthenticator { /* 私有；transport 无关纯状态机 */ }
impl HostAuthenticator {
    pub fn new(cfg: HostAuthConfig) -> Self;
    pub fn on_hello(&mut self, hello: &Hello, remote_addr: std::net::SocketAddr)
        -> HostAuthAction;   // 决策表唯一入口，盐在本方法内随机生成
    pub fn on_auth_response(&mut self, response: &AuthResponse) -> HostAuthAction;
    pub fn poll_approval(&mut self, now_us: u64,
                         approver: &mut dyn ConnectionApprover) -> HostAuthAction;
    pub fn concluded(&self) -> bool;
}
pub enum ClientAuthAction { Done, Respond(AuthResponse), Waiting, Failed(String) }
pub struct ClientAuthenticator { /* 私有 */ }
impl ClientAuthenticator {
    pub fn new(cfg: ClientAuthConfig) -> Self;
    pub fn hello_auth(&self) -> (AuthMethod, Vec<u8>);  // 密码绝不进 Hello
    pub fn on_hello_sent(&mut self, now_us: u64);
    pub fn on_welcome(&mut self, welcome: &Welcome) -> ClientAuthAction;
    pub fn on_challenge(&mut self, challenge: &AuthChallenge) -> ClientAuthAction;
    pub fn poll_timeout(&mut self, now_us: u64) -> bool;
}
```

#### clipboard.rs / clipboard_macos.rs（T20：UTF-8 文本双向同步，默认关闭）

```rust
pub const CLIPBOARD_MIN_NOTIFY_INTERVAL_US: u64 = 100_000;

pub trait ClipboardBackend: Send {
    fn change_count(&mut self) -> rc_types::Result<u64>;
    fn read_text(&mut self) -> rc_types::Result<Option<String>>;  // None=非文本，非错误
    fn write_text(&mut self, text: &str) -> rc_types::Result<()>;
}
pub struct ClipboardConfig { pub max_text_bytes: usize,
                             pub min_notify_interval_us: u64 }
pub enum ClipboardAction { SendToPeer(String), WrittenLocal(String), Idle }
pub struct ClipboardStats { pub sent: u64, pub received: u64,
    pub echo_suppressed: u64, pub coalesced: u64, pub oversize_dropped: u64,
    pub non_text_dropped: u64, pub superseded_local: u64,
    pub received_while_disabled: u64 }
pub struct ClipboardChannel { /* 私有；时间全注入（铁律 4） */ }
impl ClipboardChannel {
    pub fn new(cfg: ClipboardConfig) -> Self;
    pub fn pump_local(&mut self, backend: &mut dyn ClipboardBackend,
                      now_us: u64) -> rc_types::Result<ClipboardAction>;
    pub fn on_peer_text(&mut self, backend: &mut dyn ClipboardBackend,
                        text: &str) -> rc_types::Result<ClipboardAction>;
    pub fn stats(&self) -> ClipboardStats;
}

// clipboard_macos.rs（cfg gate macOS；rc-session 唯一 unsafe 模块，块块 // SAFETY:）
pub struct NspasteboardBackend { /* CarbonCore Pasteboard 纯 C API */ }
impl NspasteboardBackend { pub fn new() -> rc_types::Result<Self>; }

// HostSession / ClientSession 对称四方法（T20；未启用时零开销）
pub fn enable_clipboard(&mut self, backend: Box<dyn ClipboardBackend>);
pub fn pump_clipboard(&mut self, now_us: u64) -> Result<()>;
pub fn on_clipboard_notify(&mut self, text: &str) -> Result<()>;
pub fn clipboard_stats(&self) -> ClipboardStats;
```

#### testkit.rs（T14 建；T18/T20/T22 增补，只追加不改既有符号）

```rust
pub struct MockTransport { /* 私有；poll 忽略 timeout 即刻返回（确定性优先） */ }
impl MockTransport {
    pub fn new() -> Self;
    pub fn deliver(&mut self, ev: rc_net::NetEvent);
    pub fn deliver_reliable(&mut self, msg: &ControlMessage);
    pub fn datagrams_sent(&self) -> &[Vec<u8>];
    pub fn reliable_sent(&self) -> &[Vec<u8>];
    pub fn urgent_sent(&self) -> &[Vec<u8>];
    pub fn reliable_decoded(&self) -> Vec<ControlMessage>;
}
pub fn pair() -> (MockTransport, MockTransport);  // 双工互连，无损无延迟

pub struct ScriptedCapture { /* 私有 */ }   // T18：脚本输入源（IO 边界允许墙钟）
impl ScriptedCapture {
    pub fn standard() -> Self;
    pub fn from_timeline(timeline: Vec<(u64, InputEvent)>) -> Result<Self>;
    pub fn duration_us(&self) -> u64;
}
pub struct RecordingInjector { /* 私有 */ }  // T18：记录不真注入（localhost 免权限）
impl RecordingInjector {
    pub fn new() -> Self;
    pub fn injected_count(&self) -> u64;
    pub fn key_batches(&self) -> u64;
    pub fn take_events(&mut self) -> Vec<InputEvent>;
}
pub fn batch_is_key(events: &[InputEvent]) -> bool;  // InputAck 关键批共享口径

pub struct FakeClipboard { /* 私有 */ }     // T20：可编程假剪贴板
impl FakeClipboard {
    pub fn new() -> Self;
    pub fn simulate_local_copy(&mut self, text: Option<&str>);
    pub fn writes(&self) -> &[String];
}
pub struct ScriptedClipboard { /* 私有 */ } // T20：CLI scripted 模式（IO 边界墙钟）
impl ScriptedClipboard { pub fn standard() -> Self; }

pub struct MockApprover { /* 私有 */ }      // T22：可编排审批人
impl MockApprover {
    pub fn new() -> Self;
    pub fn preset(&mut self, verdict: ApprovalVerdict);
    pub fn take_request(&mut self) -> Option<ApprovalRequest>;
    pub fn submit_count(&self) -> u32;
    pub fn cancel_count(&self) -> u32;
}
```

#### report.rs / dev_cert.rs（T18：client 会话报告 + Gate + 开发证书）

```rust
pub struct FrameObservation { pub seq: rc_types::FrameSeq,
    pub outcome: Option<rc_trace::Outcome>, pub captured_us: u32,
    pub encode_done_us: u32, pub received_us: Option<u32>,
    pub decoded_us: Option<u32>, pub presented_us: Option<u32> }
/// 报告口径的重连观测（与 reconnect::ReconnectStats 同名不同物，注意模块归属）。
pub struct ReconnectStats { pub attempts: u64, pub resumed: bool,
                            pub recovery_ms: Option<u64> }
pub struct SessionObservations { pub frames: Vec<FrameObservation>,
    pub duration_us: u32, pub connect_to_first_frame_us: Option<u64>,
    pub rtt_us: Option<u32>, pub clock_offset_us: Option<i64>,
    pub input_one_way_samples_us: Vec<u32>, pub idr_requests: u64,
    pub idr_acks: u64, pub watchdog_fires: u64, pub max_reassembly_depth: u32,
    pub net: rc_net::NetStats, pub reconnect: ReconnectStats, pub session_id: u64 }
pub struct SessionGateResult { /* M2 出口门槛：首帧 800ms / p50 25 / p99 40 /
                                  fps×0.9 / 输入 RTT/2+5ms / 重连 500ms / 队列 ≤2 */ }
pub struct SessionReport { /* 私有字段 */ }
impl SessionReport {
    pub fn from_observations(cfg: &SessionConfigSnapshot,
                             obs: &SessionObservations) -> Self;  // 换算+Gate 都在此
    pub fn apply_gate(&mut self);
    pub fn to_json(&self) -> String;  // 手写 JSON（不引 serde），键名固定（T18 schema）
}
pub struct SessionConfigSnapshot { pub resolution: rc_types::Resolution,
    pub fps: u32, pub duration_s: f64, pub backend: String,
    pub connect_addr: std::net::SocketAddr, pub input_mode: &'static str }

// dev_cert.rs：内嵌开发证书（仅开发/localhost，不构成安全边界；真实体系属 M5）
pub const DEV_CERT_PEM: &str = include_str!("../certs/dev-cert.pem");
pub const DEV_KEY_PEM: &str = include_str!("../certs/dev-key.pem");
pub const DEV_SERVER_NAME: &str = "rabbitcontrol";
pub fn dev_identity() -> Result<rc_net::quic::Identity>;
pub fn dev_server_cert_pem() -> &'static str;
```

#### 对既有 crate 的 M2 增补（复用，不建新 crate 的部分）

| crate | 增补 | 来源卡 |
|---|---|---|
| `rc-input` | `channel.rs` 新增 `pub fn inject_message(msg: &[u8], injector: &mut dyn InputInjector) -> Result<usize>`——`pump_receiver` 循环体的提取（行为不变，既有测试零改动）；会话层独占 poll 后逐条注入 urgent 消息 | T14 |
| `rc-pipeline` | `HostPipeline` 新增 `pub fn set_bitrate(&mut self, bps: u32) -> Result<()>`——编码线程命令通道（`sync_channel(8)` 有界，≤5ms 生效，fire-and-forget）；本轮 rc-pipeline 唯一改动点 | T15 |
| `rc-cli` | 新增 `host` / `client` 子命令（`HostArgs` / `ClientArgs` / `InjectMode` / `InputMode`）；后续增补：`--password`（T19）、`--clipboard <off\|on\|scripted>` + `ClipboardMode`（T20）、`--tray` / `--approval` / `--approval-timeout` + `ApprovalMode`（T22）。签名详见对应卡 | T18–T22 |

### 7.6 `rc-ui`（M2-6 控制端 UI，新建；T21）

> 实现载体 Tauri v2 + 纯静态 HTML/CSS/JS 前端（2026-11-21 用户裁决，不上
> GPUI / 前端框架 / node 构建链）。依赖 `tauri` / `tauri-plugin-clipboard-manager`
> / `rc-session` / `rc-input` / `rc-types`；`serde` 仅限本 crate IPC 边界 derive，
> 不渗入其他 crate；macOS 侧只读复用 `rc-render-macos`（`MetalRenderer`/`layer_ptr`）。
> unsafe 仅限 `metal_attach.rs`（模块级 allow + 逐块 `// SAFETY:`）。

```rust
// session_bridge.rs —— UI ↔ 会话层唯一通道（不依赖 tauri，无窗口可测）
pub struct UiSessionConfig { pub connect_addr: std::net::SocketAddr,
    pub watch_only: bool, pub width: u32, pub height: u32, pub fps: u32,
    pub bitrate_bps: u32, pub prefer_hardware: bool }
pub enum SessionCommand { Disconnect, SetBitrate { bps: u32 },
                          StageRect { width: u32, height: u32 } }
pub enum ConnectPhase { EndpointCreating, Connecting, AwaitingWelcome, FirstFramePending }
pub enum SessionUpdate { Connecting { phase: ConnectPhase },
    Negotiated { width: u32, height: u32, fps: u32, bitrate_bps: u32, backend: String },
    Hud(crate::hud::HudSnapshot), Ended { reason: String } }
pub struct SessionHandle { /* 私有：命令 sync_channel(64) / 输入 sync_channel(256)，
                              满载丢最旧、关键输入保留——铁律 1 的 UI 侧延伸 */ }
impl SessionHandle {
    pub fn spawn(cfg: UiSessionConfig) -> Self;
    pub fn send(&self, cmd: SessionCommand);          // 会话已结束静默忽略
    pub fn send_input(&self, ev: rc_input::InputEvent);
    pub fn try_recv_update(&self) -> Option<SessionUpdate>;
    pub fn is_running(&self) -> bool;
}

// hud.rs —— S4 性能 HUD（差分纯函数，时间注入）
pub struct HudSample { pub at_us: u64, pub displayed: u64, pub bytes_recv: u64,
    pub rtt_us: Option<u32>, pub loss_rate: f32, pub e2e_p50_ms: Option<f64>,
    pub width: u32, pub height: u32 }
pub struct HudSnapshot { pub elapsed: String, pub transport: &'static str,
    pub fps: f64, pub bitrate_kbps: f64, pub rtt_ms: Option<f64>,
    pub frame_ms: Option<f64>, pub loss_pct: f64, pub resolution: String }
impl HudSnapshot { pub fn derive(prev: Option<&HudSample>, cur: &HudSample) -> Self; }

// input_bridge.rs —— WebView 输入 → InputEvent（纯映射，参数注入）
pub fn normalize_pointer(x: f64, y: f64, stage_w: f64, stage_h: f64) -> (u16, u16);
pub fn wheel_lines(delta_y_px: f64, line_height_px: f64) -> i16;
pub fn scancode_for(code: &str) -> Option<u16>;   // DOM KeyboardEvent.code → Set 1
pub fn modifier_bits(ctrl: bool, shift: bool, alt: bool, meta: bool) -> u8;

// model.rs —— 纯逻辑校验（§7.1 的落地）
pub fn parse_connect_target(s: &str) -> Result<std::net::SocketAddr, String>; // M2 仅 ip:port
pub fn validate_port(s: &str) -> Result<u16, String>;
pub fn validate_map_name(s: &str) -> Result<(), String>;
pub fn validate_target_addr(s: &str) -> Result<(), String>;

// metal_attach.rs —— 画面桥接路径 A（本 crate 唯一 unsafe 模块，主线程限定）
pub struct Frame { pub x: f64, pub y: f64, pub width: f64, pub height: f64 }
pub fn attach_layer(ns_view: *mut std::ffi::c_void, layer: *mut std::ffi::c_void,
                    frame: Frame) -> Result<(), String>;
pub fn sync_layer_frame(layer: *mut std::ffi::c_void, frame: Frame) -> Result<(), String>;
pub fn detach_layer(layer: *mut std::ffi::c_void);  // 只 remove 不 release
```

IPC 命令（`commands.rs`）清单见 T21 卡 §6：`list_devices` / `parse_connect_target` /
`open_session` / `session_command` / `send_input_event` / `stage_rect` /
`validate_map_form` / `copy_text`；`main.rs` 1s 轮询 `try_recv_update` 并
`emit("session://update")`。

### 7.7 `rc-host-macos`（M2-7 被控端托盘 + 接受弹窗，新建；T22）

> macOS 平台 crate，分层对齐 rc-input-macos：state/bridge 纯逻辑全平台可测，
> appkit 模块（cfg macos）集中本 crate 全部 unsafe。objc2 0.6 / 平台绑定 0.3
> 版本线对齐 rc-render-macos，不引 Tauri/GPUI/托盘抽象库。审批**复用** T19 的
> `rc_session::auth::ConnectionApprover`（全项目唯一审批抽象），不另立；
> 不加控制消息号（复用 `Welcome.auth_status`）。

```rust
pub struct TrayConfig { pub listen: std::net::SocketAddr }
pub enum TrayState { WaitingForConnection(std::net::SocketAddr),
                     InSession(std::net::SocketAddr) }
pub enum TrayEvent { DisconnectRequested, QuitRequested }
pub enum HostUiError { NoGuiSession, AppKit(String) }

pub struct TrayHost { /* 私有；主线程限定（MainThreadMarker） */ }
impl TrayHost {
    pub fn new(cfg: TrayConfig) -> Result<Self, HostUiError>;  // Accessory 策略
    pub fn approver(&self) -> TrayApprover;   // spawn 会话线程之前取，装进 HostSessionConfig
    pub fn set_state(&self, state: TrayState);  // 经 channel 转主线程，不跨线程碰 AppKit
    pub fn run(&mut self, mut on_event: impl FnMut(TrayEvent)) -> Result<(), HostUiError>;
}
pub struct TrayApprover { /* 私有：仅桥的会话端点，Send */ }
impl rc_session::auth::ConnectionApprover for TrayApprover { /* 转发桥 */ }

// state.rs（纯逻辑，无 AppKit，全平台可测）
pub struct TrayMenuLabels { pub status_line: String, pub disconnect_title: String,
    pub disconnect_enabled: bool, pub quit_title: String }
pub fn menu_labels(state: &TrayState) -> TrayMenuLabels;
pub fn alert_texts(req: &rc_session::auth::ApprovalRequest)
    -> (String /* messageText */, String /* informativeText */);

// bridge.rs（纯 std channel，全平台可测）
pub struct ApprovalBridge { /* 私有 */ }
impl ApprovalBridge {
    pub fn new() -> Self;
    pub fn session_side(self) -> SessionSide;
    pub fn ui_side(&self) -> UiSide;
}
pub struct SessionSide { /* 私有 */ }
impl rc_session::auth::ConnectionApprover for SessionSide { /* 转发通道 */ }
pub struct UiSide { /* 私有 */ }
impl UiSide {
    pub fn take_request(&mut self) -> Option<rc_session::auth::ApprovalRequest>;
    pub fn resolve(&mut self, verdict: rc_session::auth::ApprovalVerdict);
    pub fn take_cancel(&mut self) -> bool;
}
```

rc-session 侧配套增量（T22）：`ClientSession` 的 Hello→Welcome 等待超时放宽为
`max(T14 默认, approval_timeout_us + 15s)`；testkit 增 `MockApprover`（已列入
§7.5 testkit 节）；rc-cli 增 `--tray` / `--approval <prompt|auto|deny>` /
`--approval-timeout`（`ApprovalMode`）。

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
