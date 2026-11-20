# T10 输入通道

> 键鼠事件的捕获、批处理、传输、注入。**用户判断「流畅」主要依据输入响应，不是画面帧率。**

## 前置

- [ ] T09 QUIC 传输（需要 `send_reliable`）

## 背景

画面 60fps 意味着每帧 16.7ms 一次更新。但用户拖动窗口时，如果鼠标指针滞后 30ms，
主观感受是「卡」——即使画面本身很流畅。

因此输入必须：
- 走**独立通道**，永不排在视频帧之后
- **批处理**高频移动（1000Hz 鼠标会产生 1000 包/秒）
- 传**绝对坐标**，不传增量（避免累积漂移）
- 键事件**不可丢**，移动事件**可丢旧值**

## 平台要求

Windows 优先（`SendInput` 成熟）。T11/T12 完成后补 Linux/macOS 注入。

## 交付物

| 文件 | 动作 |
|---|---|
| `crates/rc-input/Cargo.toml` | 新建（抽象 + 批处理） |
| `crates/rc-input/src/lib.rs` | 新建（`InputEvent` 编码/解码） |
| `crates/rc-input/src/batch.rs` | 新建（批处理与合并） |
| `crates/rc-input-win/Cargo.toml` | 新建 |
| `crates/rc-input-win/src/inject.rs` | 新建（`SendInput`） |
| `crates/rc-input-win/src/capture.rs` | 新建（`SetWindowsHookEx` 或 `RawInput`） |

## API 契约

```rust
// 输入事件（手写二进制，不用 protobuf）
pub enum InputEvent {
    MouseMove { x: u16, y: u16 },           // 归一化到 0..65535
    MouseButton { button: MouseButton, pressed: bool },
    MouseWheel { delta: i16, horizontal: bool },
    Key { scancode: u16, pressed: bool, modifiers: u8 },
    /// 客户端心跳/校准
    Sync,
}

pub enum MouseButton { Left, Right, Middle, X1, X2 }

/// 批处理：把同一时间窗内的移动事件合并
pub struct InputBatcher { /* 私有 */ }
impl InputBatcher {
    /// `window` 建议 4ms（250Hz 发送率，足够跟手）
    pub fn new(window: Duration) -> Self;
    pub fn push(&mut self, ev: InputEvent, now: Instant);
    /// 取出到期批次。移动事件只保留最新，按键全部保留。
    pub fn poll(&mut self, now: Instant) -> Vec<InputEvent>;
    /// 立即冲刷（松键、点击等关键事件）
    pub fn flush(&mut self) -> Vec<InputEvent>;
}

/// 注入器
pub trait InputInjector: Send {
    fn inject(&mut self, events: &[InputEvent]) -> Result<()>;
    /// 屏幕尺寸，用于坐标反归一化
    fn set_screen_size(&mut self, w: u32, h: u32);
}

/// 捕获器（控制端）
pub trait InputCapture: Send {
    fn next_events(&mut self, timeout: Duration) -> Result<Vec<InputEvent>>;
}

// 编码/解码（手写二进制）
pub fn encode_events(events: &[InputEvent], out: &mut Vec<u8>);
pub fn decode_events(buf: &[u8]) -> Result<Vec<InputEvent>>;
```

## 线格式（输入事件）

**不使用 `rc-proto` 的媒体包头**——输入事件走 reliable stream，不需要帧语义。

```
每个事件：1 字节类型 + 变长负载

type 0x01 MouseMove:    x(u16 LE) y(u16 LE)            = 5 字节
type 0x02 MouseButton:  button(u8) pressed(u8)         = 3 字节
type 0x03 MouseWheel:   delta(i16 LE) horizontal(u8)   = 4 字节
type 0x04 Key:          scancode(u16 LE) pressed(u8) mods(u8) = 5 字节
type 0x05 Sync:         —                              = 1 字节

一个 stream 消息 = [len(u16 LE)] + 连续多个事件
```

**为什么移动只传 2 字节坐标**：归一化到 0..65535 后，1080p 的精度是
`1920/65536 ≈ 0.03px`，4K 是 `0.06px`，远超需要。

## 必须做到

| # | 要求 | 违反后果 |
|---|---|---|
| I1 | 输入走**独立 stream**，不与媒体共享队列 | 输入延迟随帧率波动 |
| I2 | 移动事件在批内**只保留最新**（覆盖旧值） | 1000Hz 鼠标 → 1000 包/秒打爆网络 |
| I3 | 按键事件**全部保留**，不合并 | 按键丢失/卡键 |
| I4 | 传**绝对坐标**（归一化 0..65535） | 增量累积漂移 |
| I5 | 用**扫描码**而非虚拟键码 | 跨布局错乱 |
| I6 | 批窗口 4ms（250Hz） | 太小浪费带宽，太大不跟手 |
| I7 | 关键事件（按钮/滚轮/按键）**立即 flush**，不等窗口 | 点击延迟 |
| I8 | 修饰键状态精确跟踪（不依赖对端推断） | 组合键错乱 |
| I9 | 注入失败（如无权限）返回错误，不 panic | 崩溃 |
| I10 | 注入后用 `GetCursorPos` 校准（Windows） | 光标位置漂移 |

## 不要做什么

- ❌ 不要传**增量**坐标（`dx, dy`）。必须绝对坐标。增量在网络丢包时会永久漂移。
- ❌ 不要做游戏级输入（1000Hz 直传）。4ms 批处理已经够。
- ❌ 不要做反作弊绕过/输入伪装。合规红线。
- ❌ 不要在本卡做剪贴板（独立功能）。
- ❌ 不要用 protobuf（高频小包，开销太大）。

## 测试清单

| 测试名 | 验证 |
|---|---|
| `move_events_coalesce_to_latest` | I2：push 10 次移动 → 只出 1 个 |
| `key_events_are_never_coalesced` | I3：push 3 次按键 → 出 3 个 |
| `button_events_flush_immediately` | I7 |
| `encoder_decoder_roundtrip` | 编解码对称 |
| `decode_rejects_truncated_event` | 非法输入 |
| `decode_rejects_unknown_type` | 未知类型报错 |
| `normalized_coords_have_subpixel_precision` | 1080p 下误差 < 0.1px |
| `batch_window_respected` | I6：窗口内不发送 |
| `modifier_state_tracked_across_events` | I8 |
| `injector_failure_is_returned_not_panicked` | I9 |
| `empty_batch_produces_no_message` | 边界 |

**坐标精度测试：**

```rust
#[test]
fn normalized_coords_have_subpixel_precision() {
    for width in [1920u32, 2560, 3840] {
        let max_err = width as f64 / 65536.0;
        assert!(max_err < 0.1, "{width}: err {max_err}px exceeds 0.1px");
    }
}
```

## 验收命令

```bash
cargo test -p rc-input -p rc-input-win
cargo test --workspace
cargo clippy -p rc-input-win --all-targets -- -D warnings
```

真机手动验收：

```
1. 1000Hz 鼠标快速划动 10 秒
   → 发送包数应 ≈ 2500（4ms 批 × 10s），不是 10000
   → 对端光标轨迹连续，无跳变
2. 按住 Shift 输入大写字母
   → 全部正确
3. 按 Win 键 / Cmd 键
   → 对端正确响应
4. 快速连点左键 10 次
   → 对端准确 10 次，不多不少
5. Ctrl+Alt+Del
   → 通过特殊通道发送（说明：本卡可留 TODO）
```

## 验收标准

- [ ] `cargo test --workspace` 全绿
- [ ] 1000Hz 鼠标输入，实际发包 ≤ 300/秒
- [ ] 按键/点击零丢失（连续 1000 次操作计数一致）
- [ ] 坐标精度 < 0.1px @1080p
- [ ] 输入延迟 ≤ RTT + 5ms
- [ ] 注入失败返回错误，不 panic
- [ ] 无 `unsafe`（`rc-input-win` 的 `SendInput` 是安全 API）

## 参考实现

| 读这个 | 学什么 |
|---|---|
| [`rc-proto`](../../../crates/rc-proto/src/lib.rs) | 手写二进制的模式（不带 varint） |
| [`rc-sched`](../../../crates/rc-sched/src/lib.rs) | 时间注入（`InputBatcher` 不应读时钟） |
| [`rc-trace`](../../../crates/rc-trace/src/lib.rs) | 可选的输入延迟埋点 |

## 已知坑

| # | 坑 | 处理 |
|---|---|---|
| 1 | `SendInput` 注入的输入带 `LLMHF_INJECTED` 标记 | 明确告知用户；不做反检测 |
| 2 | UAC 提权窗口无法注入 | 需 SYSTEM 权限，记 TODO |
| 3 | 多显示器负坐标 | 用虚拟桌面坐标系 |
| 4 | 键盘布局差异 | 用扫描码（`MapVirtualKey` 转换） |
| 5 | `WH_MOUSE_LL` 钩子需要消息循环 | 在专用线程跑 `GetMessage` |
| 6 | 高 DPI 下坐标换算 | 用物理像素，不用逻辑像素 |
| 7 | 松键事件丢失 → 按键卡住 | I3：按键不可合并；且超时兜底发「全松」 |
| 8 | 焦点切换后 modifier 状态不同步 | 定期发 `Sync` 事件携带完整 modifier 状态 |
