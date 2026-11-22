# T21 M2-6 控制端 UI（rc-ui）

> 按 [`08-ui-design.md`](../design/08-ui-design.md) 12 屏规范实现控制端
> `rc-ui`（Tauri v2 + 纯静态前端），并把「连接 → S1 会话窗口（画面桥接 /
> 工具栏 / 全屏 / HUD / 退出远控）」这条核心链路接到 T18 交付的
> rc-session 真会话；其余界面按设计稿以 mock 交互交付。

## 选型口径（领卡第一件事，先读）

**实现载体 = Tauri v2（2.12.7 stable）+ 纯静态 HTML/CSS/JS 前端（不上前端
框架、不上 node 构建链）。** 本卡派发摘要里的「GPUI + gpui-kit」是
2026-11-21 框架改选**之前**的旧口径，已被用户裁决推翻，出处：

- [`08-ui-design.md` §8.0](../design/08-ui-design.md)：三框架内存实测与工程
  评估，GPUI 淘汰理由——gpui-kit 三个月内 0.6→0.10 破坏式迭代、无头测试
  平台 `draw()` 为空实现（视觉验收无法自动化）、macOS 构建强制
  `xcrun metal`（需完整 Xcode，本机仅有 CLT，**直接编不过**）；
- TASK.md §0 / §13「框架改选 Tauri v2（2026-11-21 用户裁决，推翻 GPUI
  决策）」。

**不要安装 GPUI / gpui-kit。** 08 文档自称「rc-ui（M2-6 控制端 UI）的唯一
视觉与交互依据」，本文一切视觉/交互/组件/token 规格以它为准。

## 背景（为什么做这个）

M2 会话层（T13–T17）与集成出口 T18 已经把 localhost 双进程直连跑通：
`rc-cli host` / `rc-cli client` 各持完整 rc-net QUIC 栈与 rc-pipeline 管线，
client 侧落盘带 `session` 段的会话报告并按 Gate 决定 exit code。但控制端
至今只有 CLI——M2-6（里程碑表 `06-milestones.md` M2 行）要的就是图形控制
端：**连接、全屏、工具栏**。

设计稿已就绪：08 文档 12 屏（P1–P3 主窗口页、O1–O3 浮层、W1–W4 独立窗口、
S1–S4 会话中）全部自包含（token、组件、mock 数据、逐屏规格），实现者不
需要任何参考图。设计文档 §8.4 还给了接线点清单：哪些元素接真、哪些留
mock。本卡把静态原型落成 `rc-ui` crate，并按 M2-6 的三个关键词把核心链路
（P2 连接 → S1 会话）接到 rc-session；W2/W3/W4 等无后端的界面保持静态壳。

本卡**不新增任何会话机制**：会话协议零改动，只消费 T13–T18 已交付的 API。

## 平台要求

**开发与 CI：macOS**（08 §10 未决 3：设计只覆盖 macOS chrome；Windows
chrome 属后续平台期）。离线单测与进程内集成测试免任何 macOS 权限
（fake 后端 + WebView DOM 事件注入，**不走 CGEventTap**，因此不需要
「输入监控」授权；client 侧解码与 Metal 呈现也不需要屏幕录制权限——
那是 host 侧采集的事）。真画面手动验收连 `rc-cli host --backend fake` 即可
免权限；连 `--backend real` host 时由幕布工具（`./scripts/macos-dev-run.sh`）
承载权限，rc-ui 进程本身无需进幕布。

## 交付物

| 文件 | 动作 | 说明 |
|---|---|---|
| `crates/rc-ui/Cargo.toml` | 新建 | 新增 workspace 成员（序号以合并时 `members` 实际数量为准——T22 的 `rc-host-macos` 与本卡并行添加，先合并者先占位，不硬编码「第 N 个」），`version.workspace` 等字段照 20-crate-contracts.md §8 |
| `crates/rc-ui/build.rs` | 新建 | `tauri_build::build()` |
| `crates/rc-ui/tauri.conf.json` | 新建 | 窗口定义（主窗 1440×900 / 最小 1120×720）、严格 CSP（`default-src 'self'`，§8.6.3） |
| `crates/rc-ui/src/main.rs` | 新建 | Tauri Builder 入口：插件注册 + 命令注册 |
| `crates/rc-ui/src/commands.rs` | 新建 | `#[tauri::command]` 全部 IPC 命令（§8.4 接线点的 Rust 侧） |
| `crates/rc-ui/src/state.rs` | 新建 | 应用状态：当前 `SessionHandle`、mock 设备表引用 |
| `crates/rc-ui/src/model.rs` | 新建 | 纯逻辑校验（连接地址 + W4 表单，§7.1；可 cargo 单测） |
| `crates/rc-ui/src/session_bridge.rs` | 新建 | **UI ↔ 会话层的唯一通道**：会话线程 + `SessionHandle`（不依赖 tauri 类型，无窗口可测） |
| `crates/rc-ui/src/hud.rs` | 新建 | `HudSnapshot` 差分聚合（S4 七行真数据） |
| `crates/rc-ui/src/input_bridge.rs` | 新建 | WebView 输入事件 → `rc_input::InputEvent` 的纯映射 |
| `crates/rc-ui/src/metal_attach.rs` | 新建 | [macOS] **本 crate 唯一允许 unsafe 的模块**（照 `rc-render-macos/src/ffi/mod.rs:19` 先例）：NSView ↔ CAMetalLayer 挂载 |
| `crates/rc-ui/ui/index.html` | 新建 | 主窗口 P1–P3（SPA 内路由） |
| `crates/rc-ui/ui/session.html` | 新建 | S1 + S2/S3/S4 浮层（含未连接/连接中前置态） |
| `crates/rc-ui/ui/settings.html` / `transfer.html` / `terminal.html` / `portmap.html` | 新建 | W1–W4 独立窗口（静态壳） |
| `crates/rc-ui/ui/assets/css/tokens.css` | 新建 | §3 全部 Design Tokens → CSS 变量（**唯一色值来源**） |
| `crates/rc-ui/ui/assets/css/base.css`、`css/pages/*.css` | 新建 | §4 基础组件类 + 逐屏样式 |
| `crates/rc-ui/ui/assets/js/app.js` / `bridge.js` / `mock.js` | 新建 | 路由状态 / invoke 封装与事件订阅 / §6.0 mock 单一数据源 |
| `crates/rc-ui/ui/assets/icons/` | 新建 | 用到的 Lucide SVG 手动拷贝入库（一次性资产拷贝，**不引 npm**） |
| `crates/rc-ui/tests/session_flow.rs` | 新建 | 进程内 HostSession + session_bridge 集成测试（免权限、CI 可跑） |
| `Cargo.toml`（workspace 根） | 修改 | members 追加 `crates/rc-ui` |
| `TASK.md`、`docs/reference/tickets/README.md` | 修改 | 回写实测/验收记录与卡片状态表（AGENTS.md §九） |

## 依赖

```toml
# crates/rc-ui/Cargo.toml
[dependencies]
tauri = { version = "2", features = [] }
tauri-plugin-clipboard-manager = "2"   # P2「复制并分享」（08 §8.0 官方插件）
rc-session = { path = "../rc-session" } # ClientSession / dev_cert / 报告口径
rc-input = { path = "../rc-input" }     # InputEvent / modifier_bits（input_bridge 构造）
rc-types = { path = "../rc-types" }
serde = { version = "1", features = ["derive"] }  # 仅 tauri::command IPC 边界（见坑 12）

[target.'cfg(target_os = "macos")'.dependencies]
rc-render-macos = { path = "../rc-render-macos" } # MetalRenderer（画面桥接路径 A）
objc2-quartz-core = "0.3"                          # CAMetalLayer/NSView 消息（metal_attach 用）。
                                                   # 版本线对齐 rc-render-macos（其 Cargo.toml:70
                                                   # 即 0.3），同族非新家族；写 0.2 会引入第二份
                                                   # objc2 版本线（T22 U5 同款纪律）
```

**关于「不引重型依赖」的口径**：Tauri 依赖树大，但它是 2026-11-21 用户裁决
的选型（08 §8.0、TASK.md §13），且被隔离在 rc-ui 单一 crate 内——其余
workspace 成员的依赖图不受影响；`serde` 仅限 rc-ui 的 IPC 边界 derive（tauri 内建
要求），**不渗入其他 crate**。objc2 家族 workspace 里已有（rc-render-macos /
rc-capture-macos 在用），不算新家族。

## 对前置卡的消费面（不重新定义，只声明本卡怎么用）

以下能力由 T13–T18 交付，**确切签名以对应卡契约为准**；本卡按此消费。
若实现时发现签名有出入：改本卡调用处对齐前置卡；**唯一例外**见下表
第 2 行——renderer 注入口是本卡的存在前提，缺了必须回前置卡补口子：

| 消费项 | 来源卡 | 本卡用法 |
|---|---|---|
| `ClientSession`（状态机 + 驱动循环） | T14 | UI 会话线程构造后驱动；Bye 优雅关闭与停机红线由会话层保证，UI 不自写 |
| **ClientSession 构造时注入自定义 renderer** | T14 | **必须**：`ControllerPipeline::with_backends`（`crates/rc-pipeline/src/controller.rs:74`）已支持注入 `Box<dyn Renderer>`，ClientSession 必须把这个口子透传出来——UI 注入离屏 `MetalRenderer`（路径 A）或 `FakeRenderer`（测试）。若 T14 契约没留此口，回去改 T14 卡，**不在 rc-ui 重写管线** |
| RTT / 时钟偏移 / `NetStats` / `controller.trace()` 观测访问器 | T14–T17 | `hud.rs` 差分聚合的数据源（字段对齐 T18 报告 `session` 段） |
| `SetBitrate`（既有消息，reason=2 手动） | T15 | S3 画质子菜单四档码率下发，**不改协议** |
| `Bye` 正常关闭 / `ReconnectRequest` 重连 | T14/T17 | 「退出远控」与断线自动恢复全交给会话层，UI 只显示状态 |
| `dev_cert`（内嵌开发证书 + `DEV_SERVER_NAME`） | T18 | 默认零配置直连；UI 不暴露证书参数（调试需要时走 `RcUiConfig` 环境变量即可，不做界面） |
| `testkit`（`RecordingInjector` 等） | T18 | 集成测试断言 UI 输入真的到达 host |
| 连接计时口径（起点 = Endpoint 创建**之前**） | T18 | `ConnectPhase` 序列与 `connect_to_first_frame` 同起点，避免口径漂移 |

本卡对 `rc-net` / `rc-pipeline` / `rc-loopback` / `rc-cli` **只读**；
`rc-render-macos` 只读复用（`MetalRenderer` / `layer_ptr`，见 API 契约 §5）。

## API 契约

### 1. `rc-ui/src/session_bridge.rs`（UI ↔ 会话层唯一通道）

设计原则：**本模块不 import tauri**——只依赖 std + rc-session + rc-render
trait，使 `cargo test -p rc-ui` 的 bridge 测试无需窗口与事件循环。

```rust
/// UI 会话配置（P2/S1 连接表单的并集）。
pub struct UiSessionConfig {
    /// 对端地址（host:port 直连，M2 无信令；由 model::parse_connect_target 校验）。
    pub connect_addr: std::net::SocketAddr,
    /// 观看模式：不 pump 输入（08 §6 S1 只读变体）。
    pub watch_only: bool,
    /// 请求参数（Hello 携带；实际以 Welcome 裁定值为准）。
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    /// 优先硬件后端（VT 解码 + Metal 呈现），失败降级假后端并如实上报标签。
    pub prefer_hardware: bool,
}

/// UI → 会话线程的命令（除输入外均为低频控制流）。
pub enum SessionCommand {
    /// 断开会话：发 Bye 正常关闭（停机红线由会话层保证），线程随后退出。
    Disconnect,
    /// 手动设置码率（S3 画质子菜单 → SetBitrate reason=2，既有消息）。
    SetBitrate { bps: u32 },
    /// 画面容器矩形变化（前端 stage 占位 div 的 rect，物理像素）。
    /// 会话线程转 `Renderer::resize`，并经主线程同步 Metal layer frame。
    StageRect { width: u32, height: u32 },
}

/// 连接进度（对齐 T18 计时口径：起点 = Endpoint 创建之前）。
pub enum ConnectPhase {
    /// 正在创建 Endpoint（计时起点之后的第一步）。
    EndpointCreating,
    /// QUIC 握手中。
    Connecting,
    /// Hello 已发，等 Welcome 裁定。
    AwaitingWelcome,
    /// Welcome 已收，等首帧上屏。
    FirstFramePending,
}

/// 会话线程 → UI 的状态推送（经 tauri event `session://update` 转发到前端）。
pub enum SessionUpdate {
    Connecting { phase: ConnectPhase },
    /// Welcome 裁定参数（S1 屏幕 tab 文案 / HUD 分辨率行以此为准）。
    Negotiated { width: u32, height: u32, fps: u32, bitrate_bps: u32,
                 backend: String },
    /// HUD 快照（会话线程 1s 节流推送；铁律 4：数字在会话侧算好，前端只格式化）。
    Hud(crate::hud::HudSnapshot),
    /// 会话结束（正常 Bye / 对端断开 / 驱动错误），UI 据此回 P1 或提示重连。
    Ended { reason: String },
}

/// UI 侧会话句柄：clone 进各 tauri command 闭包。
///
/// 通道容量纪律（铁律 1 的 UI 侧延伸）：命令通道 `sync_channel(64)`、
/// 输入通道 `sync_channel(256)`（对齐 rc-input-macos `CgCapture` 的有界
/// 哲学，`crates/rc-input-macos` tap 通道同容量）。**满载时丢最旧**
/// （移动类事件可丢；`input_bridge` 把关键事件标 `critical`，满载时关键
/// 事件保留、纯移动丢弃——映射层已按批内关键语义区分，见 `batch_is_key`
/// 的同类口径）。绝不阻塞 UI 线程，也绝不反向阻塞会话线程。
pub struct SessionHandle { /* 私有：命令/输入 Sender + 更新 Receiver + 线程 JoinHandle */ }

impl SessionHandle {
    /// 起会话线程（构造 ClientSession → 驱动循环 → Ended 后线程自然退出）。
    /// 返回的句柄立即可用；连接进度经 `try_recv_update` 流出。
    pub fn spawn(cfg: UiSessionConfig) -> Self;
    /// 发控制命令。会话已结束时静默忽略（幂等）。
    pub fn send(&self, cmd: SessionCommand);
    /// 发一条输入事件（WebView 事件映射产物；watch_only 会话内部直接丢弃）。
    pub fn send_input(&self, ev: rc_input::InputEvent);
    /// 非阻塞取一条状态更新（commands.rs 轮询后 emit 给前端）。
    pub fn try_recv_update(&self) -> Option<SessionUpdate>;
    /// 会话线程是否仍在运行。
    pub fn is_running(&self) -> bool;
}
```

会话线程内部形态照抄 T18 `client.rs` 的驱动循环（tick → send_datagram /
Datagram → push_datagram / take_keyframe_request → IdrRequest / 输入
pump 双拍），差别仅三点：① renderer 换成注入的离屏 `MetalRenderer`（或
测试用 `FakeRenderer`）；② 驱动循环不设 `--duration` 兜底，跑到 `Disconnect`
或对端 `Bye` 为止（UI 会话没有「测 30 秒」概念）；③ 每秒把观测差分聚合成
`HudSnapshot` 推给 UI。会话线程 panic 不传染 UI（线程边界天然隔离），join
到 `Err` 时以 `Ended { reason: "会话线程异常退出" }` 上报。

### 2. `rc-ui/src/hud.rs`（S4 性能 HUD 真数据）

字段与 08 §6 S4 的七行 mock 一一对应，语义对齐 `LoopbackReport` /
T18 `SessionReport`（08 原文：「字段与 LoopbackReport 语义对齐
（fps/码率/延迟/帧时间/丢包/分辨率），M2-6 接真数据」）：

```rust
/// 观测原始快照（会话线程从 ClientSession 访问器取；全部注入数据）。
pub struct HudSample {
    /// 采样时刻（微秒，会话钟）。
    pub at_us: u64,
    /// 累计上屏帧数（controller.displayed 语义，controller.rs:226）。
    pub displayed: u64,
    /// 累计接收字节（NetStats.bytes_recv）。
    pub bytes_recv: u64,
    /// 探测平滑 RTT（无样本时 None）。
    pub rtt_us: Option<u32>,
    /// 丢包率（NetStats.loss_rate）。
    pub loss_rate: f32,
    /// 当前帧时间 p50（毫秒；偏移换算后的 e2e 最近窗口，无样本 None）。
    pub e2e_p50_ms: Option<f64>,
    /// Welcome 裁定分辨率。
    pub width: u32,
    pub height: u32,
}

/// S4 HUD 快照（serde 仅为此 IPC 边界）。
#[derive(serde::Serialize, Clone, Debug, PartialEq)]
pub struct HudSnapshot {
    /// "00:00:11"（会话钟换算，<24h；71.6min 饱和刷新由 T17 守卫，UI 无需处理）。
    pub elapsed: String,
    /// 传输摘要，恒 "UDP LAN"（M2 只有直连场景，不探测拓扑）。
    pub transport: &'static str,
    pub fps: f64,
    pub bitrate_kbps: f64,
    pub rtt_ms: Option<f64>,
    pub frame_ms: Option<f64>,
    pub loss_pct: f64,
    /// "1920x1080" 一类；HUD 末行分辨率标签。
    pub resolution: String,
}

impl HudSnapshot {
    /// 由相邻两次观测差分计算（纯函数，时间注入，单测钉死）：
    /// fps = Δdisplayed / Δt；bitrate_kbps = Δbytes_recv × 8 / Δt / 1000；
    /// Δt == 0 或首样本（无 prev）时 fps/bitrate 取 0（诚实显示，不沿旧值）。
    pub fn derive(prev: Option<&HudSample>, cur: &HudSample) -> Self;
}
```

### 3. `rc-ui/src/input_bridge.rs`（WebView 输入 → InputEvent，纯映射）

WKWebView 的 DOM 事件在前端捕获后经 `send_input_event` command 进来，
本模块把 Web 语义映射到 `rc_input::InputEvent`（`crates/rc-input/src/lib.rs:85-128`）。
**全部纯函数、全部参数注入（铁律 4），映射表单测钉死**：

```rust
/// 指针坐标归一化到 0..=65535（InputEvent::MouseMove 语义）。
/// 坐标越界（stage 外的 DOM 事件）clamp 到边界，不 panic。
pub fn normalize_pointer(x: f64, y: f64, stage_w: f64, stage_h: f64)
    -> (u16, u16);

/// DOM wheel 的 deltaY（CSS 像素，向下为正）→ 行数（规范单位 = 行，
/// 见 InputEvent::MouseWheel 文档）。deltaY 向下为正 ⇒ delta 为负
/// （delta>0 = 向上滚）。行高由前端实测注入（不写死）。
pub fn wheel_lines(delta_y_px: f64, line_height_px: f64) -> i16;

/// DOM `KeyboardEvent.code` → Set 1 扫描码（I5：扫描码而非虚拟键码）。
/// E0 前缀键编码 `0xE0 << 8 | code`（如 ArrowLeft = 0xE04B、右 Ctrl = 0xE01D）。
/// 未收录的 code 返回 None（command 层静默丢弃并计数，不报错弹窗）。
pub fn scancode_for(code: &str) -> Option<u16>;

/// 修饰键快照（I8：发送方在事件发生瞬间的精确状态，接收端不推断）。
/// 位定义复用 `rc_input::batch::modifier_bits` 的语义。
pub fn modifier_bits(ctrl: bool, shift: bool, alt: bool, meta: bool) -> u8;
```

映射表覆盖 DOM 标准键区（字母 / 数字 / 符号 / 功能 / F1–F12 / 方向 /
修饰 / Escape·Tab·Backspace·Delete 等控制键），以 HID usage → Set 1 对照
表编写；单测至少抽 24 条钉死（含全部 E0 前缀键）。滚轮符号方向是本模块
唯一不能离线证明的语义（自然滚动设置影响 deltaY 符号），集成测试里用
`RecordingInjector` 收到的 `MouseWheel.delta` 符号做一次真机钉死。

### 4. `rc-ui/src/model.rs`（纯逻辑校验，§7.1 的落地）

08 §7.1 说校验「可单测，放 model.rs」——本卡把它放进 Rust 侧（cargo 单测，
不引 node 测试链）：

```rust
/// 解析连接目标。M2 直连只收 `ip:port`（对齐 T18 `--connect` 的 SocketAddr
/// 口径）；域名 / 设备 ID 体系属 M4/M5 信令，返回 Err 提示「暂仅支持 IP:端口直连」。
/// 端口范围 1–65535，拒绝 0 / 70000 / 非数字 / 空 / 多余字段。
pub fn parse_connect_target(s: &str) -> Result<std::net::SocketAddr, String>;

/// W4 添加映射表单校验（§7.1 唯一纯逻辑校验）：
pub fn validate_port(s: &str) -> Result<u16, String>;      // 1–65535 整数
pub fn validate_map_name(s: &str) -> Result<(), String>;   // 非空且 ≤64 字符
pub fn validate_target_addr(s: &str) -> Result<(), String>; // 非空；IP/域名严格校验留 TODO
```

### 5. `rc-ui/src/metal_attach.rs`（画面桥接路径 A，唯一 unsafe 模块）

模块头 `#![allow(unsafe_code)]`（照 `rc-render-macos/src/ffi/mod.rs:19` 先例，
rc-input-macos/src/ffi/mod.rs:19 同款），每个 `unsafe` 块带 `// SAFETY:`
注释，**crate 其余模块 `deny(unsafe_code)`**：

```rust
/// stage 区域（物理像素；Retina 上 = CSS 像素 × backingScaleFactor）。
pub struct Frame { pub x: f64, pub y: f64, pub width: f64, pub height: f64 }

/// 把 `MetalRenderer::layer_ptr()`（`crates/rc-render-macos/src/renderer.rs:158`，
/// 离屏 `CAMetalLayer` 借用指针）挂到 Tauri 窗口 NSView 的子层。
/// **必须在主线程调用**（AppKit UI 操作；Tauri `run_on_main_thread` 派发）。
/// 借用语义遵守 layer_ptr 文档：不 release、不改引用计数——所有权在渲染器。
pub fn attach_layer(ns_view: *mut std::ffi::c_void, layer: *mut std::ffi::c_void,
                    frame: Frame) -> Result<(), String>;

/// 窗口 resize / stage 布局变化时同步 layer frame（主线程）。
pub fn sync_layer_frame(layer: *mut std::ffi::c_void, frame: Frame)
    -> Result<(), String>;

/// 会话结束：从 superlayer 移除（主线程）。**只 remove，不 release**。
pub fn detach_layer(layer: *mut std::ffi::c_void);
```

呈现路径零新增：`MetalRenderer` 实现了 `rc_render::Renderer`，经
`ControllerPipeline::with_backends` 注入后，`push_datagram → decode →
present` 全在会话线程内完成（`CAMetalLayer` 后台渲染线程安全）；本模块只
负责「层挂到窗口 + frame 同步」两件事。`MetalRendererConfig.size` 是
**物理像素**（`renderer.rs:47-49`），构造时用 Welcome 裁定分辨率 ×
backingScaleFactor 初始化。

### 6. `rc-ui/src/commands.rs`（IPC 命令，签名要点）

```rust
/// mock 设备表（§6.0），state.rs 持有单一数据源。
#[tauri::command] fn list_devices(state) -> Vec<DeviceEntry>;
/// 连接目标校验（model::parse_connect_target 的 IPC 壳）。
#[tauri::command] fn parse_connect_target(target: String) -> Result<String, String>;
/// 打开会话：校验 → state 中 spawn SessionHandle（已有会话先 Disconnect）。
#[tauri::command] fn open_session(cfg: OpenSessionRequest, app, state) -> Result<(), String>;
/// 低频控制命令转发（全屏/退出等窗口操作由前端 Tauri window API 直做，不走这）。
#[tauri::command] fn session_command(cmd: SessionCommand, state);
/// WebView 输入事件 → input_bridge 映射 → SessionHandle::send_input。
/// 映射失败（未收录键）静默丢并计数；watch_only 会话直接忽略。
#[tauri::command] fn send_input_event(ev: WebInputEvent, state) -> Result<(), String>;
/// 前端 ResizeObserver 上报 stage rect（CSS 像素）：Rust 侧换算物理像素后
/// 转 StageRect 命令 + 主线程 sync_layer_frame。
#[tauri::command] fn stage_rect(rect: StageRectMsg, app, state);
/// W4 表单校验（model 三函数的 IPC 壳，错误文案原样给前端红框）。
#[tauri::command] fn validate_map_form(form: MapForm) -> MapFormErrors;
/// P2「复制并分享」：写剪贴板 + 返回 Toast 文案（tauri-plugin-clipboard-manager）。
#[tauri::command] fn copy_text(text: String, app) -> Result<(), String>;
```

`main.rs` 另起 1s 定时器轮询 `SessionHandle::try_recv_update` 并
`app.emit("session://update", ...)`（事件推送主通道；`hud_snapshot()` 轮询
command 不再单独提供，避免双源）。**S1 前端不做任何本地时钟计算**：计时器、
HUD 第一行同用推送的 `elapsed`（08 §8.6.2「便于后续替换为 Rust 侧会话
时间」——本卡就是那个替换）。

## 必须做到（硬性要求）

| # | 要求 | 违反后果 |
|---|---|---|
| U1 | 视觉与交互唯一依据 = 08 文档：12 屏、token、组件、交互、mock 数据逐项照抄；`tokens.css` 是唯一色值来源，业务样式禁止裸写色值/字号 | 验收对不上设计稿 |
| U2 | 实现载体 Tauri v2 + 纯静态前端；不装 GPUI/gpui-kit，不引前端框架与 node 构建链 | 编不过（本机无完整 Xcode）/ 范围蔓延 |
| U3 | **会话协议零改动**：不动 rc-session proto/状态机/消息；S3 画质仅经既有 `SetBitrate(reason=2)`；帧率切换无消息支持 → 保持 Toast 占位文案 | 越权改稳定层 |
| U4 | 画面桥接 spike 是**开工首项**（08 §8.5）：优先路径 A（离屏 `MetalRenderer` + `layer_ptr` 挂 NSView）；A 不可行才退 B，且决策与证据记 TASK.md §13 | 界面做完才发现画面进不去 |
| U5 | 时间注入（铁律 4）：计时器/HUD 数字全部由会话线程推送，前端只格式化；`HudSnapshot::derive` 纯函数；离线测试无 sleep | 数字两套钟、测试 flaky |
| U6 | 队列深度（铁律 1 的 UI 侧延伸）：UI↔会话通道全部有界（命令 64 / 输入 256），满载丢最旧、关键输入优先保留；**不在管线外新增任何帧缓冲层** | UI 反压拖垮会话线程 |
| U7 | unsafe 仅限 `metal_attach.rs`（模块级 allow + 逐块 `// SAFETY:`），crate 其余部分 `deny(unsafe_code)` | 违反 20-crate-contracts §8 卫生线 |
| U8 | 连接口径对齐 T18：`ip:port` 直连、默认内嵌开发证书（`dev_cert`）零配置、连接计时起点 = Endpoint 创建之前 | 口径漂移，数字不可比 |
| U9 | 观看模式零注入：`watch_only` 会话不 pump 输入（集成测试断言 host `RecordingInjector` 收到 0 事件）；控制模式注入走 WebView DOM 事件（免「输入监控」权限） | 权限无谓升级 / 语义违反设计稿 |
| U10 | 「退出远控」= 关 S1 窗口回主窗 P1 + `Disconnect`（Bye 正常关闭）；会话线程必须可 join 退出，不留僵尸线程与残留 Metal 子层（`detach_layer`） | 泄漏 + 下次连接叠两层 |
| U11 | 完成后回写 TASK.md §0/§12/§13（验收记录带场景与命令）与 tickets README 状态表（AGENTS.md §九） | 视为未完成 |

## 不要做什么

- ❌ 不改会话协议（proto 消息、ClientSession/HostSession 状态机、rc-net /
  rc-pipeline / rc-loopback / rc-cli 任何代码——全只读；`rc-render-macos`
  也只读，缺口子回前置卡改卡，不打补丁）。
- ❌ 不做 W2 文件传输 / W3 终端 PTY / W4 端口映射的**真实后端**——三窗按
  设计稿交静态壳 + mock 交互（08 §8.4 已标「需建卡」，属 M6 及以后）。
- ❌ 不做 T19 鉴权 / T20 剪贴板同步实装（P2 验证码、S2 安全状态维持 mock；
  T19 的 AuthChallenge UI 联动位属 T22/T19 卡）。
- ❌ 不做设备发现 / 信令 / 账号体系（侧栏与 P3 设备表 = `mock.js` 静态数据；
  设备 ID 体系属 M4/M5）。
- ❌ 不做 Windows / Linux chrome 与深色模式（08 §10 未决 2/3）。
- ❌ 不做输入的 CGEventTap 捕获路径（那是无窗口场景的方案；WebView DOM
  事件免权限，别引入权限需求）。
- ❌ 不做 JS 侧输入微批 / 节流「优化」（rc-input Batcher 已做 4ms 批处理，
  JS 再批一次只加延迟；IPC 开销超标属 M3 优化项，先测量再说）。
- ❌ 不做跨平台交叉编译配置、安装包打包（dmg 等）——`cargo run` 能起即可。

## 测试清单

**rc-ui 离线单测（确定性、无窗口、CI 可跑）：**

| 测试名 | 验证 |
|---|---|
| `parse_connect_target_accepts_ip_port` | `127.0.0.1:9901` 合法；含端口 0 / 70000 / 空串 / 无端口 / 域名各拒绝且文案区分「暂仅支持 IP:端口直连」 |
| `validate_map_form_rejects_bad_ports` | §7.1：0 / 70000 / 非数字 / 空 → 端口错误文案；空名称 / 65 字符名称拒绝；全过才无错误 |
| `normalize_pointer_clamps_to_stage` | (0,0)→(0,0)、(w,h)→(65535,65535)、越界 clamp、除以 stage 尺寸不 panic（stage_w=0 防御） |
| `wheel_lines_sign_and_rounding` | deltaY>0（向下）→ 负行数；|deltaY|<半行 → 0；行高注入生效 |
| `scancode_table_covers_common_keys` | ≥24 条：KeyA=0x1E、Digit1、Space、Enter、Escape、Backspace、Tab、F1–F12 抽查、方向四键（0xE04B/0xE04D/0xE048/0xE050）、左右修饰（左 Ctrl=0x1D、右 Ctrl=0xE01D、左 Win=0xE05B）、未知 code → None |
| `modifier_bits_match_input_semantics` | 与 `rc_input::batch::modifier_bits` 位定义逐位一致（ctrl/shift/alt/meta 组合） |
| `hud_derive_computes_deltas` | 两次样本 Δdisplayed=60 / Δt=1e6 → fps=60.0；Δbytes=125000 → bitrate_kbps=1000.0；**首样本（无 prev）fps=bitrate=0**；Δt=0 不 NaN |
| `hud_elapsed_formats_hms` | 0→"00:00:00"、3661e6 μs→"01:01:01"；71min 内不越界（饱和刷新由 T17 守卫） |
| `hud_zero_division_is_safe` | loss_rate=NaN/Inf 输入防御、rtt=None → rtt_ms=None，快照永不 NaN |
| `session_channels_are_bounded` | 命令通道容量 64 / 输入通道 256（常量断言，铁律 1 延伸）；满载 send 不阻塞（try_send 语义） |

**rc-ui 集成测试（`tests/session_flow.rs`，进程内 HostSession + bridge，
免权限、CI 可跑；不起 Tauri 窗口——session_bridge 不依赖 tauri 的回报）：**

| 测试名 | 验证 |
|---|---|
| `bridge_connects_to_inprocess_host` | 进程内起 T18 形态 HostSession（fake 后端 + `RecordingInjector`，照 `rc-session/tests/localhost.rs` pair 形态）→ `SessionHandle::spawn` → 依次收到 `Connecting` → `Negotiated`（Welcome 裁定值）→ 若干 `Hud`（fps>0）→ `Disconnect` 后 `Ended` 且线程 join 退出 |
| `watch_mode_sends_no_input` | `watch_only=true` + 持续 `send_input` → host `RecordingInjector::injected_count() == 0`；同场景 `watch_only=false` → count>0（U9 双向钉死） |
| `set_bitrate_command_flows_to_host` | `SetBitrate{bps}` → host 侧收到 SetBitrate（T15 转发链计数/`RecordingInjector` 同款断言口径） |
| `input_events_reach_injector_normalized` | UI 发 (32768, 32768) 移动 + 点击 + 滚轮 → host 记录事件类型齐全、坐标原样归一化值 |
| `ended_session_commands_are_ignored` | `Ended` 后 `send`/`send_input` 静默幂等、`is_running()==false`（U10） |

**真窗口 / 手动验收测试（`#[ignore]`，人工或幕布环境跑）：**

| 测试名 | 验证 |
|---|---|
| `#[ignore] metal_layer_spike_embeds_and_resizes` | **开工首项 spike**（U4）：起 Tauri 窗口 + 离屏 `MetalRenderer` + `attach_layer` → 塞入动态内容会话 → 肉眼确认层内有画面；拖拽 resize 窗口 → `sync_layer_frame` 后 `MetalRenderer::size()` 跟随（物理像素）；detach 后无残留子层 |
| `#[ignore] fullscreen_toggle_roundtrip` | S3「进入全屏幕」/ `F` 键 → 窗口全屏态切换，stage rect 推送、layer 跟随满屏 |
| `#[ignore] wheel_direction_real_machine` | 真机滚轮一次向上 → host 收到 `MouseWheel.delta > 0`（自然滚动的符号方向钉死，映射函数吃注入参数所以离线部分已在单测覆盖） |

## 验收命令

```bash
# 0. 开工首项：spike（U4，真窗口手动跑，即测试清单首条 #[ignore] 项）
cargo test -p rc-ui -- --ignored metal_layer_spike_embeds_and_resizes --nocapture

# 1. 内循环（铁律 3：只碰本 crate）
cargo check -p rc-ui --all-targets
cargo test -p rc-ui

# 2. 全仓回归 + 质量门（rc-ui 进 members 后 --workspace 会连它一起编）
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check

# 3. 真链路手动验收（终端 1：fake host，免权限；终端 2：UI）
./scripts/macos-dev-run.sh cli --release -- host \
    --listen 127.0.0.1:9901 --backend fake --resolution 1920x1080 \
    --fps 60 --duration 300s --inject-mode record
cargo run -p rc-ui --release
#   UI 内操作链：P2 输入 127.0.0.1:9901 → 连接 → S1 画面出现（spike A）→
#   HUD 每秒刷新（fps≈60 / rtt / loss）→ S3 切「高清 8M」→ host stderr 见
#   SetBitrate 生效 → F 全屏往返 → 退出远控 → 回 P1 且 host 侧收到 Bye 优雅收尾

# 4. 视觉验收：逐屏截图存 target/ui-shots/，对照 08 §9 清单人工过一遍
#    （元素级抽查法：随机抽 5 个元素查四要素：位置/颜色/尺寸/内容）
#    环境有 node 时可选脚本化：npx playwright screenshot（连静态页 mock 模式）

# 5. 集成测试里的真机钉死项（滚轮方向）
cargo test -p rc-ui -- --ignored wheel_direction_real_machine --nocapture
```

说明：命令 3 的 host 用 fake 后端本可直跑子进程，但统一走幕布工具保持与
T18 实测口径一致（脚本以完成摘要行判定子进程结束，`scripts/macos-dev-run.sh`；
两终端各自调用，不要在同一终端 `&` 拼后台）。rc-ui 是 GUI 进程，直接
`cargo run` 即可，无需进幕布（client 侧无 TCC 需求，见「平台要求」）。

## 验收标准

- [ ] `cargo test --workspace` 全绿（含 rc-ui 离线单测 + 进程内集成测试）
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` 0 警告；
      unsafe 仅存在于 `metal_attach.rs`（U7，grep 可证）
- [ ] spike 结论已记录（U4）：路径 A 可行（层内见画面、随窗口缩放、
      60fps 会话 present 计数正常）或退 B 的证据与残留限制写入 TASK.md §13
- [ ] 命令 3 全链手动过：连接 → 画面 → HUD 真数据（fps/rtt/loss 每秒变化，
      非静态 mock）→ SetBitrate 生效 → 全屏往返 → 退出远控 host 收 Bye
      正常收尾、无僵尸线程
- [ ] 观看模式：host `RecordingInjector` 0 事件（集成测试 + 手动各一次）
- [ ] 12 屏截图对照 08 §9 清单：mock 名称逐字一致、token 无裸写、
      O3/S2/S3 毛玻璃生效（`backdrop-filter`，Tauri 加分项）
- [ ] 前端零 node 依赖：`ui/` 目录无 package.json / node_modules，
      Lucide SVG 为入库资产
- [ ] rc-net / rc-pipeline / rc-loopback / rc-cli / rc-render-macos 的
      `git diff` 为空（全只读）
- [ ] TASK.md §0/§12/§13 已回写（含 spike 结论与手动验收记录），
      tickets README 状态表已更新

## 参考实现

| 读这个 | 学什么 |
|---|---|
| [`docs/reference/design/08-ui-design.md`](../design/08-ui-design.md) | **唯一视觉与交互依据**：12 屏规格、§3 tokens、§4 组件、§6.0 mock、§7 交互、§8 Tauri 实现映射（§8.5 画面桥接、§8.4 接线点） |
| [`crates/rc-cli/src/cmd/client.rs`](../../../crates/rc-cli/src/cmd/client.rs)（T18 交付） | 会话驱动循环形态、连接计时起点、Bye 收尾——session_bridge 的会话线程照此改写 |
| [`crates/rc-render-macos/src/renderer.rs`](../../../crates/rc-render-macos/src/renderer.rs) | `MetalRenderer::new`（105，离屏 layer）、`from_raw_layer`（135）、`layer_ptr`（158，借用语义）、物理像素尺寸语义（47-49） |
| [`crates/rc-render-macos/src/ffi/mod.rs`](../../../crates/rc-render-macos/src/ffi/mod.rs) | unsafe 集中模块先例（19 行模块级 allow + 逐块 SAFETY 注释）、objc2 家族用法 |
| [`crates/rc-pipeline/src/controller.rs`](../../../crates/rc-pipeline/src/controller.rs) | `with_backends`（74，renderer 注入口）、`push_datagram`（105）、`displayed`（226，HUD 数据源） |
| [`crates/rc-input/src/lib.rs`](../../../crates/rc-input/src/lib.rs) | `InputEvent` 语义（85-128）：归一化坐标、滚轮行单位、Set 1 扫描码、修饰键快照 |
| [`crates/rc-loopback/src/lib.rs`](../../../crates/rc-loopback/src/lib.rs) | 时钟锚定与 Arc 闭包模式（246-263）、驱动循环（336-423）——UI 会话线程的会话钟同款 |

## 已知坑

| # | 坑 | 处理 |
|---|---|---|
| 1 | **派发摘要写 GPUI 是旧口径**（2026-11-21 用户裁决已推翻，见本卡「选型口径」节）。GPUI 在本机连编译都过不了（强制 `xcrun metal` 需完整 Xcode，本机仅 CLT） | 领卡先读 08 §8.0；不装 GPUI，一切以 08 文档为准 |
| 2 | **AppKit 主线程纪律**：`addSublayer` / `setFrame:` / `removeFromSuperlayer` 必须主线程；而 `MetalRenderer::present` 在会话线程调用（`CAMetalLayer` 后台渲染线程安全）。混线程操作 layer 会得到未定义行为或布局不生效 | 挂载/同步/摘除统一经 Tauri `run_on_main_thread` 派发（metal_attach 三函数文档已标）；呈现路径不动 |
| 3 | **Retina 物理像素换算**：`MetalRendererConfig.size` 与 `Frame` 都是物理像素（`renderer.rs:47-49`），前端 `getBoundingClientRect` 给的是 CSS 像素 | `stage_rect` command 在 Rust 侧统一乘 `backingScaleFactor`（`window.scale_factor()`），前端不做换算 |
| 4 | **`layer_ptr` 是借用**（`renderer.rs:158-162`：不得释放/改引用计数，存活期由渲染器保证） | 会话结束只 `detach_layer`（removeFromSuperlayer），绝不 release；渲染器随 ControllerPipeline 在会话线程析构 |
| 5 | **renderer 注入口是本卡存在前提**：T14 的 ClientSession 若没透传 `with_backends` 的 renderer 注入，UI 无法接管画面 | 消费面表第 2 行已声明：回 T14 卡补口子，不在 rc-ui 重写管线（这是本卡对前置卡的唯一反向要求） |
| 6 | **计时/HUD 双时钟漂移**：前端若自设计时器（JS `Date.now` 起），与 host 钟域的 e2e、T17 饱和刷新（71.6min 守卫）全部对不上 | U5：前端零本地时钟计算，`elapsed` 一律用会话推送值；S1 计时器与 HUD 首行同源（08 §6 S4「随计时同步」） |
| 7 | **UI 反压会话线程**：无界 channel 让会话驱动循环卡在 send 上，画面停摆但 UI 看不出原因 | U6：全通道有界（64/256），满载 try_send 丢最旧；HUD 1s 一条本来就稀疏，丢得起；关键输入保留、纯移动可丢 |
| 8 | **WebView 吞快捷键**：`F` / `Esc` 不 `preventDefault` 会被 WKWebView 当浏览器行为（搜索/退出） | 前端在 stage 聚焦域拦截（08 §7.3 注册表）；`Esc` 语义 = 关当前浮层/模态，不是退出会话 |
| 9 | **滚轮符号方向**受系统「自然滚动」设置影响，离线证明不了符号对不对 | 映射函数吃注入参数（单测钉死数学关系）；真机一次 `wheel_direction_real_machine` 钉死物理方向（设计师 risks 同类：量纲单位错位） |
| 10 | **同机双进程 CPU 竞争**（设计师风险 4）：UI 真解码 + Metal 与 host 的 SCK+VT 编码争同一台机器，手动验收的 HUD 数字含调度噪声 | 手动验收只看「HUD 活着且量级合理」（fake host 下 fps≈60、loss≈0），不做门槛断言；定量测量属 T18 报告口径，不属 UI 卡 |
| 11 | **连接计时起点漂移**（设计师风险 7）：真机 LAN 主要耗时在证书加载与 Endpoint 建立 | `ConnectPhase` 从 `EndpointCreating` 起步，起点 = Endpoint 创建之前（T18 口径），`Connecting` 阶段文案对齐设计稿「连接中」灰字 |
| 12 | **serde 口径**：项目纪律是报告手写 JSON 不引 serde（T18 S9），但 `tauri::command` 的 IPC 边界内建要求 derive | serde 仅限 rc-ui（依赖表已注明），HudSnapshot 等 IPC 类型用 derive；rc-session 报告口径不受影响；不向其他 crate 渗透 |
| 13 | **rc-ui 编译扇出**（tauri + rc-session 全链，远大于其他 crate；clean build 明显变慢） | 遵守 M0 纪律：内循环只 `cargo check -p rc-ui` / `cargo test -p rc-ui`；全仓验证留给验收命令（tauri 编一次后走缓存） |
| 14 | **会话线程 panic 静默**：驱动循环 panic 后 UI 停在「连接中」，用户无从得知 | 线程边界天然隔离 + join 结果上报 `Ended { reason }`；`is_running()` 供 commands 层兜底 |
| 15 | **逐事件 IPC 的性能预算**：mousemove 125Hz → 每秒百余次 invoke，WKWebView IPC 单次 ~0.1ms 量级，M2 可用但不优雅 | 本卡不做 JS 微批（见「不要做什么」：rc-input Batcher 已批，JS 再批加延迟）；若实测超标记 TASK.md §13 留 M3 |
| 16 | **lucide-static 是 npm 包**：设计稿图标经它引入，但本卡禁 node 链 | 用到的 SVG 手动拷贝入 `ui/assets/icons/`（一次性资产拷贝，所用图标清单 = 08 §3.5 表去重，拷贝记录随 TASK.md §13 回写） |
| 17 | **12 屏静态验收无法纯 cargo 自动化**：WKWebView 截图需 tauri-driver / Playwright（可选） | 验收标准里视觉项为手动清单（08 §9）+ 环境有 node 时可选脚本化；不要为此引 node 依赖进仓库 |

## 与后续卡的接口

- **T19 鉴权 / T22 被控端托盘 + 接受弹窗**：P2 验证码、S2 安全状态届时从
  mock 换 AuthChallenge 联动；本卡前端已按 08 §8.4 留了接线位（mock.js 单
  一数据源换 invoke 即可），Rust 侧 `commands.rs` 届时加命令，不动
  session_bridge 的通道协议。
- **T20 剪贴板**：`copy_text` command 已走 clipboard-manager 插件，双向
  同步届时在会话层扩展，UI 只加 S2 入口。
- **W2 / W3 / W4 真实后端**（文件通道 / PTY / QUIC 端口转发，各需建卡）：
  前端静态壳与 mock 数据结构就位，换数据源不动布局。
- **M3 拥塞控制**：HUD 字段语义（fps/码率/rtt/loss）不变；若加带宽估速
  显示，另立 HUD 行不动既有键。
- **M4 / M5 设备发现与信令**：侧栏 / P3 设备表换真数据源，连接表单届时
  增加设备 ID 解析（`parse_connect_target` 扩展，错误文案已预留口径）。
- **S3 帧率切换**：协议无中途改 fps 的消息（Hello 才携带），本卡 Toast
  占位；若后续加消息属协议卡，届时 UI 只换占位为 command。
