# T12 macOS 采集

> 实现 `rc-capture-macos`：ScreenCaptureKit + VideoToolbox 零拷贝，含三重授权引导。

## 前置

- [ ] T03 完成（`CaptureBackend` trait 与 `CapturedFrame` 已定型）

## 背景

macOS 的门槛不在采集 API，而在**授权**：屏幕录制、辅助功能、输入监控三项，
缺一不可，且用户拒绝后必须引导到系统设置手动开启。

同时 `ScreenCaptureKit` → `VideoToolbox` 有天然的零拷贝路径（`CVPixelBuffer`），
用对了延迟很低；但有一个隐藏陷阱：**未设 `RealTime=true` 时 VideoToolbox 会引入 20–40ms 缓冲**。

## 平台要求

> **优先级 P0，主线任务**（2026-11-20 变更）：用户主要远控 macOS。
> 本卡与 T08（M1 Gate）构成项目主线，是第一个必须拿到真实延迟数字的地方。

**macOS 13+ 真机。** 需要能改系统设置的权限（测试授权流程）。

## 交付物

| 文件 | 动作 |
|---|---|
| `crates/rc-capture-macos/Cargo.toml` | 新建 |
| `crates/rc-capture-macos/src/lib.rs` | 新建 |
| `crates/rc-capture-macos/src/sck.rs` | 新建（ScreenCaptureKit） |
| `crates/rc-capture-macos/src/permission.rs` | 新建（三重授权检测与引导） |
| `crates/rc-capture-macos/src/ffi/mod.rs` | 新建（objc2 绑定） |
| `crates/rc-encode-vt/src/lib.rs` | 新建（VideoToolbox） |
| `crates/rc-input-macos/src/lib.rs` | 新建（CGEvent） |

## API 契约

同 `CaptureBackend`。额外：

```rust
// permission.rs
pub struct PermissionStatus {
    pub screen_recording: bool,
    pub accessibility: bool,
    pub input_monitoring: bool,
}

impl PermissionStatus {
    /// 检测三项权限。不弹窗。
    pub fn check() -> Self;
    /// 全部具备
    pub fn all_granted(&self) -> bool;
    /// 打开对应的系统设置页
    pub fn open_settings(&self, which: PermissionKind) -> Result<()>;
    /// 请求屏幕录制权限（首次会弹窗，之后不再弹）
    pub fn request_screen_recording() -> bool;
}

pub enum PermissionKind { ScreenRecording, Accessibility, InputMonitoring }
```

## 必须做到

| # | 要求 | 违反后果 |
|---|---|---|
| M1 | 用 `CGPreflightScreenCaptureAccess()` 检测，不弹窗 | 启动就弹窗骚扰用户 |
| M2 | 用 `AXIsProcessTrusted()` 检测辅助功能 | 注入静默失败 |
| M3 | 用 `IOHIDCheckAccess` 检测输入监控 | — |
| M4 | 授权变更后提示**重启进程**（macOS 限制） | 用户以为已生效 |
| M5 | **`SCStreamConfiguration.pixelFormat = '420v'`**（NV12） | 需 CPU 色彩转换 |
| M6 | **`kVTCompressionPropertyKey_RealTime = true`** | 延迟 +20–40ms |
| M7 | `SCStreamConfiguration.showsCursor = false`，光标单独传 | 双光标/光标滞后 |
| M8 | `SCStream` 实例**不可复用**，停止后必须重建 | 崩溃 |
| M9 | Retina 2x 下按需下采样（不要传 2x 分辨率给编码器） | 带宽翻倍 |
| M10 | 监听权限失效，优雅重建 | 崩溃 |

## 不要做什么

- ❌ 不要用已废弃的 `CGDisplayStream` / `CGWindowListCreateImage`。
- ❌ 不要在子进程里做采集（授权会额外复杂）。
- ❌ 不要做签名/公证（那是 M5 打包）。
- ❌ 不要假设权限已授予——必须在启动时检查。

## 测试清单

**无 GPU 单元测试：**

| 测试名 | 验证 |
|---|---|
| `permission_status_all_granted_requires_three` | 逻辑 |
| `permission_open_settings_maps_kind_correctly` | URL 映射 |
| `retina_downsample_computes_correct_target` | M9 |
| `frame_timestamp_monotonic` | — |

**真机（`#[ignore]`）：**

```rust
#[test] #[ignore = "requires macOS + permissions granted"]
fn sck_captures_at_60fps() { ... }

#[test] #[ignore = "requires macOS"]
fn videotoolbox_realtime_flag_is_set() {
    // 关键：验证 kVTCompressionPropertyKey_RealTime 确实生效
    // 方法：编码 300 帧，统计从输入到输出的延迟分布
    // 未设标志时 P50 会 > 30ms；设了应 < 12ms
}
```

## 验收命令

```bash
cargo test -p rc-capture-macos -p rc-encode-vt -p rc-input-macos
cargo clippy -p rc-capture-macos --all-targets -- -D warnings

# 真机
cargo test -p rc-capture-macos -- --ignored
```

## 验收标准

- [ ] `cargo test --workspace` 全绿（macOS 上）
- [ ] 首次运行：三项权限引导流程完整，用户能看懂要做什么
- [ ] 拒绝授权时：明确提示 + 可跳转设置页，不崩溃
- [ ] 1080p 采集 ≥58fps
- [ ] `SCStream` → `VTCompressionSession` 零拷贝（GPU 采样器验证无 CPU 拷贝）
- [ ] VideoToolbox 单帧编码 P50 ≤ 12ms（证明 `RealTime` 生效）
- [ ] Retina 下编码分辨率是逻辑分辨率（不是 2x）
- [ ] 光标单独处理，拖动无滞后
- [ ] 权限被撤销后不崩溃

## 参考实现

| 读这个 | 学什么 |
|---|---|
| [`rc-types`](../../../crates/rc-types/src/lib.rs) | `Resolution::to_even`（Retina 下采样） |
| [`rc-diff`](../../../crates/rc-diff/src/lib.rs) | macOS 无脏区 API，必须用 `TileDiff` |

## 已知坑（macOS 特有）

| # | 坑 | 处理 |
|---|---|---|
| 1 | TCC 数据库按「签名 + bundle id」记录，开发版与发布版签名不同会导致权限反复失效 | 开发期用固定签名 |
| 2 | 每次重编译签名变化 → 需重新授权 | 用固定证书或 `codesign -s -` 自签 |
| 3 | `CGRequestScreenCaptureAccess()` 只弹一次 | 之后只能跳设置页 |
| 4 | `SCStream` 停止后再 `start` 会失败 | 必须新建实例 |
| 5 | `SCContentFilter` 需要 `SCShareableContent` 先枚举 | 异步获取 |
| 6 | `SCStream` 回调在独立 dispatch queue | 不要在里面做重活 |
| 7 | `CVPixelBuffer` 的 `IOSurface` 需要 `CVPixelBufferLockBaseAddress` 才能读 | 零拷贝前提是不读 CPU |
| 8 | `kVTCompressionPropertyKey_RealTime` 未设 → 默认按文件转码优化 | **最常见的延迟来源** |
| 9 | Apple Silicon 与 Intel 的 VT 能力不同 | 用 `VTCopySupportedPropertyDictionaryForEncoder` 查询 |
| 10 | 多显示器：`SCContentFilter` 需指定 display | 每显示器一个 stream |
| 11 | `kVTVideoEncoderSpecification_EnableLowLatencyRateControl = true` 在高帧率下会导致 Apple Silicon VideoToolbox 约 3s 后触发媒体引擎降速（M1 Pro `1512x982@120` 30s loopback 跌至 ~84fps / encode 19.3ms） | `rc-encode-vt` 在 `fps >= 90` 时自动关闭 LLRC、保持全帧率 `RealTime = true`（M6），实测 30s loopback 稳态 `118.20 fps`、25s 会话后 15s 中位 `119.7 fps`；`fps < 90` 路径与 T12 60fps 延迟不变（P50=7.72ms） |

## 依赖

```toml
[target.'cfg(target_os = "macos")'.dependencies]
objc2 = "0.5"
objc2-foundation = "0.2"
objc2-core-media = "0.2"
objc2-core-video = "0.2"
objc2-screen-capture-kit = "0.1"   # 若不可用，用 objc2 手写绑定
core-graphics = "0.24"
```

## 与其他卡的关系

- **`rc-encode-vt`** 与 `rc-capture-macos` 同卡交付，因为它们共享 `CVPixelBuffer` 零拷贝路径，强行拆开会引入一次拷贝
- **`rc-input-macos`** 可独立交付，但需要 M2（辅助功能）授权才有意义
