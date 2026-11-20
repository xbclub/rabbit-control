# T04 NVENC 硬件编码

> 实现 `rc-encode` 抽象 + NVENC 后端。**延迟优化的最大单点杠杆。**

## 前置

- [ ] T01 工程基座
- [ ] T03 已交付 **`rc-capture` 的 trait 与类型层**（`CaptureBackend`、`CapturedFrame`、`FrameTexture`）

**不需要 T03 的 DXGI 实现完成。** T04 的 `encode()` 入参是 `&CapturedFrame`，
只要该类型已定义即可开工，用假帧测试。

若 T03 尚未开工：可以先领 T03 的「trait 层」部分（约 100 行，`rc-capture/src/lib.rs`），
再回来做 T04。不要自己另造一套 `CapturedFrame`。

## 背景

单个参数组合就能让帧率翻倍：`preset p1` + `B帧0` + 关闭 look-ahead + 零输出延迟。
单帧编码从 15–25ms 降到 3–5ms。

本卡的核心不是「能编出 H.264」，而是「能在 deadline 内编完且延迟可预测」。

## 平台要求

> **优先级 P1**（2026-11-20 变更）：macOS 走 VideoToolbox（T12），NVENC 为 Windows 支持路径。

**Windows 物理机 + NVIDIA 显卡（GTX 10 系 / Turing 及以上）。**
虚拟机不可信（多数用软件渲染）。

## 交付物

| 文件 | 动作 | 说明 |
|---|---|---|
| `crates/rc-encode/Cargo.toml` | 新建 | `Encoder` trait + 码率控制 |
| `crates/rc-encode/src/lib.rs` | 新建 | 抽象 + `EncoderCaps` |
| `crates/rc-encode/src/bitrate.rs` | 新建 | 码率阶梯与自适应策略 |
| `crates/rc-encode-nvenc/Cargo.toml` | 新建 | |
| `crates/rc-encode-nvenc/src/lib.rs` | 新建 | 安全封装 |
| `crates/rc-encode-nvenc/src/ffi/mod.rs` | 新建 | 所有 `unsafe` |
| `crates/rc-encode-nvenc/src/params.rs` | 新建 | 参数配置（**延迟优先**） |
| `crates/rc-encode-nvenc/src/selfcheck.rs` | 新建 | 启动自检 |

## API 契约

照抄 [`20-crate-contracts.md`](../impl/20-crate-contracts.md) §7.2。关键：

```rust
pub trait Encoder: Send {
    fn encode(&mut self, frame: &CapturedFrame, force_idr: bool,
              deadline_ts: Timestamp) -> Result<EncodedFrame>;
    fn capabilities(&self) -> EncoderCaps;
    fn set_bitrate(&mut self, bps: u32) -> Result<()>;
    fn request_idr(&mut self);
    fn flush(&mut self) -> Result<Vec<EncodedFrame>>;
}

pub struct EncoderConfig {
    pub codec: Codec,
    pub resolution: Resolution,
    pub fps: u32,
    pub bitrate_bps: u32,
    pub preset: Preset,
    pub low_latency: bool,     // 必须 true
    pub gop_len: u32,          // 0 = 无限 GOP
    pub b_frames: u32,         // 必须 0
}
```

## 必须做到（延迟优先参数）

| # | 参数 | 值 | 理由 | 违反后果 |
|---|---|---|---|---|
| E1 | `b_frames` | **0** | B 帧需要重排序，至少 +1 帧延迟 | 延迟 +17–67ms |
| E2 | `preset` | **`NV_ENC_PRESET_P1_GUID`**（最快） | p7 单帧 15–25ms | 帧率上限掉到 30 |
| E3 | `NV_ENC_PIC_PARAMS::outputDelay` | **0** | 编码器内部缓冲 | 隐藏延迟 |
| E4 | `rcParams.enableLookahead` | **0** | 前瞻缓冲 = 固定延迟 | +1~4 帧 |
| E5 | `rcParams.multiPass` | **`NV_ENC_MULTI_PASS_DISABLED`** | 2-pass 需整段缓冲 | 无法实时 |
| E6 | `rcParams.enableFillerData` | 可选 1 | 保持码率稳定（CBR 需要） | 码率波动 |
| E7 | `frameIntervalP` | **1**（无 B 帧，无 P 间隔重排） | 同 E1 | — |
| E8 | `idrPeriod` | **0 / 极大**（按需 IDR） | 定期 IDR 是带宽尖峰 | 弱网卡顿 |
| E9 | `sliceMode` + `sliceModeData` | **slice=4~8** | 允许解码端并行 | 解码延迟高 |
| E10 | **启动自检**：编一帧假数据并验证输出非空 | 编码器驱动差异 | 花屏无法定位 |
| E11 | `deadline_ts` 超出时返回 `Error::Timeout`，不阻塞 | 调度器失效 |
| E12 | 输出顺序 == 输入顺序 | 解码端乱序 |

### 参数对照表（写进代码注释）

```rust
// 质量优先（错，会造成 30fps 上限）
// preset:      NV_ENC_PRESET_P7_GUID      (single frame 15-25ms)
// b_frames:    3
// lookahead:   1
// multipass:   TWO_PASS
// idrPeriod:   60
//
// 延迟优先（对，60fps 可达）
// preset:      NV_ENC_PRESET_P1_GUID      (single frame 3-5ms)
// b_frames:    0
// lookahead:   0
// multipass:   DISABLED
// idrPeriod:   0 (按需)
```

## 不要做什么

- ❌ 不要做质量自适应调优（那是 P1）。本卡只保证「快且正确」。
- ❌ 不要实现 QSV / AMF / Media Foundation。只做 NVENC。
- ❌ 不要做分层编码（`layer_id` 留 0）。
- ❌ 不要在编码线程里做色彩空间转换（会引入 CPU 拷贝）。NVENC 直接吃 GPU 纹理。
- ❌ 不要用 `-` 号量化参数（`qp`）作为主要码控，用 CBR 或低延迟 VBR。

## 测试清单

**无 GPU 单元测试（CI 可跑）：**

| 测试名 | 验证 |
|---|---|
| `bitrate_ladder_steps_down_on_congestion` | 码率自适应逻辑 |
| `bitrate_respects_min_max_bounds` | 边界 |
| `encoder_config_rejects_b_frames` | 校验器拒绝 `b_frames > 0` |
| `encoder_config_rejects_lookahead` | 校验器拒绝 `low_latency=false` |
| `fake_encoder_returns_frames_in_order` | E12 |
| `fake_encoder_times_out_past_deadline` | E11 |
| `force_idr_produces_keyframe` | 强制关键帧 |
| `capabilities_report_hardware_flag` | caps 正确 |

**需要真机（`#[ignore]`）：**

```rust
#[test] #[ignore = "requires NVIDIA GPU"]
fn nvenc_selfcheck_passes() { ... }

#[test] #[ignore = "requires NVIDIA GPU"]
fn nvenc_1080p_encode_under_6ms() {
    // 编 300 帧，统计单帧耗时 P50 <= 6ms
}
```

## 验收命令

```bash
cargo test -p rc-encode -p rc-encode-nvenc
cargo clippy -p rc-encode-nvenc --all-targets -- -D warnings

# 真机
cargo test -p rc-encode-nvenc -- --ignored
```

## 验收标准

- [ ] `cargo test --workspace` 全绿
- [ ] 校验器拒绝 `b_frames > 0` / `low_latency = false` / `multipass != disabled`
- [ ] 真机：1080p 单帧编码 P50 ≤ 6ms、P99 ≤ 10ms
- [ ] 真机：编码输出能被 ffmpeg 解码（`ffprobe` 检查无错误）
- [ ] 真机：连续 300 帧无花屏（用 `ffmpeg` 解码后比对 PSNR > 35dB）
- [ ] 自检失败时返回明确错误（不是 panic）
- [ ] `unsafe` 只在 `ffi/`
- [ ] 强制 IDR 确实产出 `is_keyframe = true`

## 参考实现

| 读这个 | 学什么 |
|---|---|
| [`rc-sched`](../../../crates/rc-sched/src/lib.rs) | `deadline_us` 的来源与语义、时间注入 |
| [`rc-proto`](../../../crates/rc-proto/src/lib.rs) | `split_frame` 如何消费编码输出 |
| [`rc-types`](../../../crates/rc-types/src/lib.rs) | `FrameKind`、`Encoder` 错误变体 |

## 已知坑

| # | 坑 | 处理 |
|---|---|---|
| 1 | NVENC 需要 **CUDA driver**，不是 CUDA runtime | 检测 `nvcuda.dll` |
| 2 | 会话数上限（消费级卡 2–3 路） | 会话开始时检测，失败则降级 |
| 3 | 纹理格式必须是 NV12 | BGRA 需 `NV_ENC_BUFFER_FORMAT_ARGB` 或先转换 |
| 4 | `NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY` | 驱动版本不匹配，给出明确错误 |
| 5 | 未设 `outputDelay=0` 时延迟不可预测 | E3 |
| 6 | `INVALID_PARAM` 常因 `frameIntervalP` 与 `b_frames` 冲突 | 两者同时设 1/0 |
| 7 | GTX 10 系不支持 AV1/B帧 HEVC 某些组合 | 用 `nvEncGetEncodeCaps` 查询能力，不要硬编码 |

## 依赖

```toml
[target.'cfg(windows)'.dependencies]
windows = { version = "0.58", features = [
    "Win32_Foundation",
    "Win32_Graphics_Direct3D11",
    "Win32_Graphics_Dxgi",
] }
# NVENC SDK 通过 build.rs 链接 nvEncodeAPI64.lib 或动态加载 nvcuda
```

**建议：动态加载 NVENC**（`LoadLibrary("nvEncodeAPI64.dll")` + `NvEncodeAPIGetMaxSupportedVersion`）。
静态链接会让没有 NVIDIA 驱动的机器直接启动失败。
