# T05 解码与呈现

> 实现 `rc-decode`（DXVA 硬件解码）与 `rc-render`（低延迟 GPU 呈现）。

## 背景

延迟预算里解码 3ms + 呈现 2ms。两个关键点：

1. **零拷贝**：解码输出的 GPU 纹理直接交给渲染器，禁止经 CPU
2. **关 vsync**：`Present(0)` 而不是 `Present(1)`，否则每帧 +16.7ms

## 平台要求

> **优先级 P1**（2026-11-20 变更）：**P0 主线需要本卡的 macOS 版本**（VideoToolbox 解码 + 低延迟呈现）。
> 实现 Windows 版之前，请先按 T12 的模式做 macOS 版，T08 的 M1 Gate 依赖它。

Windows 物理机（与 T03/T04 同机最好，便于端到端串联）。

## 交付物

| 文件 | 动作 |
|---|---|
| `crates/rc-decode/Cargo.toml` | 新建 |
| `crates/rc-decode/src/lib.rs` | 新建（trait + 假实现） |
| `crates/rc-decode-dxva/Cargo.toml` | 新建 |
| `crates/rc-decode-dxva/src/lib.rs` | 新建 |
| `crates/rc-render/Cargo.toml` | 新建 |
| `crates/rc-render/src/lib.rs` | 新建（trait） |
| `crates/rc-render-win/src/lib.rs` | 新建（D3D11 swapchain 或窗口） |

## API 契约

```rust
// rc-decode
pub trait Decoder: Send {
    /// 输入一帧码流，返回解码出的纹理。
    /// 若不完整（缺参考帧），返回 Ok(None) 并设置 needs_keyframe。
    fn decode(&mut self, data: &[u8], kind: FrameKind,
              deadline_ts: Timestamp) -> Result<DecodeOutput>;
    fn needs_keyframe(&self) -> bool;
    fn flush(&mut self) -> Result<()>;
}

pub enum DecodeOutput {
    Frame(DecodedFrame),
    /// 参考链断裂，需要请求关键帧
    NeedKeyframe,
    /// 无输出（如码流不完整）
    Nothing,
}

pub struct DecodedFrame {
    pub texture: FrameTexture,      // GPU 纹理，零拷贝
    pub decode_done_ts: Timestamp,
    pub width: u32,
    pub height: u32,
}

// rc-render
pub trait Renderer: Send {
    /// 呈现一帧。必须关闭 vsync。
    fn present(&mut self, frame: DecodedFrame) -> Result<Timestamp>;
    fn resize(&mut self, size: (u32, u32)) -> Result<()>;
    fn set_vsync(&mut self, on: bool);
    /// 光标单独渲染（延迟感知为 0）
    fn set_cursor(&mut self, pos: (i32, i32), visible: bool);
}
```

## 必须做到

| # | 要求 | 违反后果 |
|---|---|---|
| D1 | 解码输出**零拷贝**给渲染器（同一 GPU device） | 每帧多一次 CPU 往返，+3–8ms |
| D2 | 关闭 vsync（`Present(0, ...)`） | 每帧 +16.7ms |
| D3 | 解码器低延迟标志（`AV_CODEC_FLAG_LOW_DELAY` / DXVA 等价） | 内部缓冲 |
| D4 | 缺参考帧时返回 `NeedKeyframe`，不 panic | 崩溃或花屏 |
| D5 | 解码器按「进一出一」工作（因为编码无 B 帧） | 输出顺序错乱 |
| D6 | 呈现失败（device lost）返回错误，可重建 | 崩溃 |
| D7 | 光标本地渲染，有形状缓存 | 光标位置滞后一帧 |
| D8 | 呈现时间戳在 `Present` 返回后打 | 延迟测量偏小 |

## 不要做什么

- ❌ 不要用 `ffmpeg` 的完整 `AVCodecContext` 高层封装（那是 T06 的软件路径）。
  本卡直接用 DXVA2/D3D11VA。
- ❌ 不要在呈现路径做色彩空间转换（GPU shader 里做，或用硬件 overlay）。
- ❌ 不要实现缩放算法（用双线性即可，`Linear`）。
- ❌ 不要做窗口管理/UI。只做「给定纹理，画到指定窗口」。

## 测试清单

**无 GPU 单元测试：**

| 测试名 | 验证 |
|---|---|
| `fake_decoder_returns_need_keyframe_on_missing_ref` | D4 |
| `fake_decoder_outputs_in_order` | D5 |
| `fake_renderer_present_timestamp_after_call` | D8 |
| `renderer_resize_updates_size` | resize |
| `decoder_deadline_timeout` | deadline 语义 |

**真机（`#[ignore]`）：**

```rust
#[test] #[ignore = "requires Windows GPU"]
fn h264_decode_1080p_under_3ms() { ... }

#[test] #[ignore = "requires Windows GPU"]
fn present_with_vsync_off_under_2ms() { ... }

#[test] #[ignore = "requires Windows GPU"]
fn decode_encode_roundtrip_visual_match() {
    // 用 PRNG 生成图案 → 编码 → 解码 → 对比像素
    // 允许有损（PSNR > 35dB），但结构必须对
}
```

## 验收命令

```bash
cargo test -p rc-decode -p rc-render -p rc-render-win
cargo clippy -p rc-render-win --all-targets -- -D warnings
cargo test -p rc-render-win -- --ignored   # 真机
```

## 验收标准

- [ ] `cargo test --workspace` 全绿
- [ ] 真机：1080p 解码 P50 ≤ 3ms
- [ ] 真机：`Present` 调用 P50 ≤ 2ms（vsync off）
- [ ] 真机：编码→解码往返 PSNR > 35dB
- [ ] 缺参考帧时不崩溃，返回 `NeedKeyframe`
- [ ] 呈现输出 60fps 连续 60 秒无掉帧（用 `rc-trace` 验证 `unfinished == 0`）
- [ ] `unsafe` 只在 `ffi/`

## 参考实现

| 读这个 | 学什么 |
|---|---|
| [`rc-trace`](../../../crates/rc-trace/src/lib.rs) | `Decoded` / `Presented` 标记的语义 |
| [`rc-sched`](../../../crates/rc-sched/src/lib.rs) | `Timestamp` 语义（相对会话起点） |

## 已知坑

| # | 坑 | 处理 |
|---|---|---|
| 1 | 解码纹理与渲染纹理不同 device → 隐式拷贝 | 用同一个 `ID3D11Device` |
| 2 | `Present(1)` 强制 vsync | 用 `Present(0)` |
| 3 | 窗口最小化时 `Present` 失败 | 捕获 `DXGI_STATUS_OCCLUDED`，暂停呈现 |
| 4 | 全屏独占 vs 窗口模式行为不同 | 两种都测 |
| 5 | 颜色空间不匹配导致偏色 | 显式设置 `DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709` |
| 6 | 高 DPI 下窗口尺寸与纹理尺寸不一致 | 用物理像素 |
| 7 | `h264` 的 `AVCC` 与 `Annex-B` 格式不同 | `EncoderCaps::bitstream_format` 声明，解码端按声明处理 |
