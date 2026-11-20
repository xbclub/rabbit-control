# T06 软件编码兜底（openh264）

> 实现无 GPU 时的软件编码路径。**这不是「凑数」的实现——它是 CI 能跑回环测试的前提。**

## 背景

两个必要性：

1. **CI 无 GPU**：GitHub Actions 的 runner 没有 NVENC。回环测试必须能降级到软件编码跑通。
2. **用户端兜底**：老机器/虚拟机/远程桌面里套远程桌面，都没有硬件编码。

要求：1080p 达到 **30fps**。达不到的机器应被标记为「不推荐」。

## 平台要求

**全平台。** 这是唯一一个「所有 agent 都能领」的编码卡。

## 交付物

| 文件 | 动作 |
|---|---|
| `crates/rc-encode-sw/Cargo.toml` | 新建 |
| `crates/rc-encode-sw/src/lib.rs` | 新建 |
| `crates/rc-encode-sw/src/openh264.rs` | 新建（绑定） |
| `crates/rc-encode-sw/src/fallback.rs` | 新建（纯 Rust 极简编码器，可选） |

## 依赖选择

优先 **`openh264` crate**（Cisco 官方库的 Rust 绑定，MIT 许可，有预编译）：

```toml
[dependencies]
openh264 = "0.6"
```

如果该 crate 不可用或不合要求，备选：
- 纯 Rust 的 `rav1e`（AV1，太慢）→ 不适用
- 自己写极简 H.264（**不推荐**，工作量大且容易出错）

> **禁止使用 ffmpeg 系（`ffmpeg-next` / `ffmpeg-sys`）。** 用户明确要求不引入 ffmpeg。
> 软件编码兜底只用 `openh264` crate，它不依赖 ffmpeg。

## API 契约

同 [`20-crate-contracts.md`](../impl/20-crate-contracts.md) §7.2 的 `Encoder` trait。
特殊之处：`capabilities().hardware == false`。

```rust
impl Encoder for OpenH264Encoder {
    fn capabilities(&self) -> EncoderCaps {
        EncoderCaps {
            codec: Codec::H264,
            hardware: false,
            max_resolution: Resolution::new(1920, 1080),
            bitstream_format: BitstreamFormat::AnnexB,
            profiles: &[66, 77],   // baseline, main
        }
    }
    // ...
}
```

## 必须做到

| # | 要求 | 理由 |
|---|---|---|
| S1 | 1080p 达到 30fps（`cargo bench` 验证） | 低于此值标为不推荐 |
| S2 | 输入 `CpuFrame`（BGRA 或 NV12） | 软件路径不共享 GPU 纹理 |
| S3 | BGRA → I420 转换要在编码器外可测（独立函数） | 转换是常见 bug 源 |
| S4 | `b_frames = 0`（同 NVENC） | 延迟 |
| S5 | `bitrate` 可动态调整 | 自适应 |
| S6 | 编码输出为 Annex-B（start code `00 00 00 01`） | 与 T04 一致或明确区分 |
| S7 | 多线程编码（openh264 支持 slice 并行） | 30fps 的前提 |
| S8 | 内存不泄漏（连续 1000 帧 RSS 稳定） | 长会话 |

## 不要做什么

- ❌ 不要试图达到 60fps 1080p。软件编码物理上做不到，标为 30fps 目标。
- ❌ 不要做 AV1/VP9。只做 H.264。
- ❌ 不要实现「自适应选择软件/硬件」。那是工厂函数的事，在 T08 里做。
- ❌ 不要用 `unsafe` 手写 H.264 位流。用现成库。

## 测试清单

| 测试名 | 验证 |
|---|---|
| `bgra_to_i420_converts_correctly` | S3：用一个 2×2 已知颜色块手算期望值 |
| `bgra_to_i420_handles_odd_dimensions` | 奇数尺寸不 panic |
| `open_h264_produces_annexb_start_code` | S6：输出以 `00 00 00 01` 开头 |
| `open_h264_force_idr_produces_idr_nal` | NAL type 5 |
| `bitrate_change_takes_effect` | S5：改码率后输出大小变化 |
| `encoder_reports_hardware_false` | caps |
| `fake_or_real_encoder_rejects_invalid_resolution` | 边界（0×0） |
| `continuous_encode_does_not_leak` | S8：跑 1000 帧后检查（用 `#[ignore]`，慢） |

**BGRA→I420 的期望值手算示例**（写进测试注释）：

```
输入 2x2 BGRA:
(0,0)=B0 G0 R0 A0, (1,0)=B1 G1 R1 A1
(0,1)=B2 G2 R2 A2, (1,1)=B3 G3 R3 A3

I420 输出:
Y 平面 4 字节：Y0 Y1 Y2 Y3
  Y = 0.114*B + 0.587*G + 0.299*R   (BT.601)
U 平面 1 字节：avg(U of 4 pixels)
  U = -0.169*B - 0.331*G + 0.500*R + 128
V 平面 1 字节：avg(V of 4 pixels)
  V = 0.500*B - 0.419*G - 0.081*R + 128
```

必须用**整数运算**（`(x * 66) >> 8` 之类）以避免浮点不确定性，并在测试里对整数实现做期望值断言。

## 验收命令

```bash
cargo test -p rc-encode-sw
cargo clippy -p rc-encode-sw --all-targets -- -D warnings

# 性能验证
cargo bench -p rc-encode-sw
```

## 验收标准

- [ ] `cargo test --workspace` 全绿
- [ ] `cargo bench` 显示 1080p 编码 ≥30fps（单核）
- [ ] BGRA→I420 用手算期望值验证通过
- [ ] 输出能被 `ffprobe` 解析
- [ ] 连续 1000 帧 RSS 增长 < 10MB
- [ ] 全平台可编译（macOS/Linux/Windows CI 都过）

## 参考实现

| 读这个 | 学什么 |
|---|---|
| [`rc-proto`](../../../crates/rc-proto/src/lib.rs) | `split_frame` 如何消费编码输出、Annex-B 处理 |
| [`rc-diff`](../../../crates/rc-diff/src/lib.rs) | 如何处理 `CpuFrame` 与 stride |

## 已知坑

| # | 坑 | 处理 |
|---|---|---|
| 1 | `openh264` 需要 `libclang` 或预编译二进制 | 文档里写清安装步骤 |
| 2 | BGRA 与 I420 的 stride 不同 | 各自独立计算，不要复用 |
| 3 | 奇数宽高 | 向下取偶（`Resolution::to_even`），记录警告 |
| 4 | openh264 默认开 B 帧 | 显式设置 `i_num_b_frames = 0` |
| 5 | 码率单位是 bps 不是 kbps | 注意换算 |
| 6 | Windows 上需要 `openh264-*.dll` 在 PATH | 打包时带上 |
