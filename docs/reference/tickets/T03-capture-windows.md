# T03 Windows DXGI 采集

> 实现 `rc-capture` 的抽象 + Windows DXGI Desktop Duplication 后端。
> **这是整个媒体管线的第一环，也是坑最多的一环。**

## 背景

RustDesk 类产品帧率低的第一个源头是采集：固定周期全帧轮询。
DXGI Desktop Duplication 提供 `GetFrameDirtyRects` 和「有新帧才返回」的语义，
用对了可以在静态画面下把 CPU 降到接近 0。

但 DXGI 有 10 个经典坑（见下方「已知坑」），其中任何一个处理错都表现为
「黑屏」或「采集永久停止」，且**编译期无法发现**。

## 平台要求

> **优先级 P1**（2026-11-20 变更）：macOS 是 P0 主线，本卡在 M1 之后作为 P1 支持。

**必须在 Windows 物理机上完成。** 虚拟机可以跑通 DXGI 但：
- 多数 VM 用软件渲染，测不出真实帧率
- 多显示器/分辨率变化的验证不可信

若手上只有 macOS，**不要领这张卡**，去领 T02 / T06 / T07 / T09。

## 交付物

| 文件 | 动作 | 说明 |
|---|---|---|
| `crates/rc-capture/Cargo.toml` | 新建 | 抽象层，平台无关 |
| `crates/rc-capture/src/lib.rs` | 新建 | `CaptureBackend` trait + 工厂 |
| `crates/rc-capture-win/Cargo.toml` | 新建 | `[target.'cfg(windows)'.dependencies]` |
| `crates/rc-capture-win/src/lib.rs` | 新建 | 安全封装 |
| `crates/rc-capture-win/src/duplication.rs` | 新建 | DXGI 逻辑 |
| `crates/rc-capture-win/src/ffi/mod.rs` | 新建 | **所有 `unsafe` 集中在此** |
| `crates/rc-capture-win/src/frame_guard.rs` | 新建 | RAII ReleaseFrame |
| `crates/rc-capture-win/src/testing.rs` | 新建 | 测试用假后端 |

## API 契约

照抄 [`20-crate-contracts.md`](../impl/20-crate-contracts.md) §7.1。核心：

```rust
pub trait CaptureBackend: Send {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>>;
    fn poll_invalidation(&mut self) -> Option<InvalidationReason>;
    fn rebuild(&mut self) -> Result<()>;
    fn display_info(&self) -> DisplayInfo;
}

pub struct CapturedFrame {
    pub texture: FrameTexture,
    pub timestamp: Timestamp,      // 内容变化时刻！
    pub dirty: Option<Rect>,
    pub seq: CaptureSeq,
    pub is_repeat: bool,
}
```

## 必须做到

| # | 要求 | 违反后果 |
|---|---|---|
| C1 | **无变化时返回 `Ok(None)`**，不返回帧 | 静态画面 CPU 满载 |
| C2 | `timestamp` 是**内容变化时刻**（用 `LastPresentTime` 或采集返回前一刻） | 延迟测量虚高 |
| C3 | `ReleaseFrame` 用 RAII guard，Drop 中调用 | 第二次 `AcquireNextFrame` 返回 `INVALID_CALL`，采集永久停止 |
| C4 | `DXGI_ERROR_ACCESS_LOST` → `Error::Capture { recoverable: true }` | 锁屏后永久黑屏 |
| C5 | `rebuild()` 幂等，连续调用不报错 | 崩溃 |
| C6 | `GetFrameDirtyRects` 返回空 且 `LastPresentTime` 未变 → 返回 `Ok(None)` | 重复帧进管线 |
| C7 | 所有 `unsafe` 在 `ffi/` 内，每个 `unsafe fn` 有 `// SAFETY:` | 无法 review |
| C8 | 采集在独立线程，`next_frame` 阻塞不持锁 | 编码阻塞采集 |
| C9 | 多显示器：每个 output 一个 duplication 实例 | 只采主屏 |
| C10 | `AcquireNextFrame` 超时传 **`16` ms**（不是 0） | CPU 空转 |

## 不要做什么

- ❌ **不要用 `windows` crate 的 raw vtable 手写绑定。** 用类型化封装，让编译器帮你抓错。
- ❌ 不要在这个卡里做编码。只交付采集。
- ❌ 不要做 WinRT `Windows.Graphics.Capture`。那是 P1 兜底，本卡只做 DXGI。
- ❌ 不要在业务层用 `#[cfg(windows)]`。平台判断只允许在工厂函数里。
- ❌ 不要吞掉 `ACCESS_LOST`。必须上报，由上层决定重建。
- ❌ 不要用 `std::thread::sleep` 轮询等新帧。用 `AcquireNextFrame(16)` 阻塞。

## 测试清单

**无 GPU 的单元测试（必须有，保证 CI 可跑）：**

| 测试名 | 验证 |
|---|---|
| `fake_backend_returns_frames_at_requested_rate` | 假后端基础行为 |
| `fake_backend_returns_none_when_static` | C1 |
| `capture_error_recoverable_flag_maps_correctly` | C4 的错误映射 |
| `rebuild_is_idempotent_on_fake` | C5 |
| `dirty_rects_are_clipped_to_bounds` | 越界脏区裁剪 |
| `frame_timestamp_is_monotonic` | C2 |
| `capture_seq_increases_even_for_repeats` | seq 语义 |

**需要真机的集成测试（`#[ignore]`，任务卡要求手动跑）：**

```rust
#[test]
#[ignore = "requires Windows + real GPU: cargo test -p rc-capture-win -- --ignored"]
fn dxgi_captures_at_60fps() { ... }

#[test]
#[ignore = "requires manual display change"]
fn dxgi_recovers_after_resolution_change() { ... }
```

## 验收命令

```bash
# 通用（CI 可跑，无 GPU）
cargo test -p rc-capture -p rc-capture-win
cargo clippy -p rc-capture-win --all-targets -- -D warnings

# Windows 真机手动验收
cargo test -p rc-capture-win -- --ignored
```

**真机验收脚本**（临时二进制或测试，必须实际跑一遍）：

```
1. 启动采集 → 打印每秒采集帧数
2. 静止画面 10s  → 期望采集帧数接近 0，CPU < 2%
3. 拖动窗口 10s  → 期望采集帧数 ≈ 60/s
4. 改分辨率      → 期望自动重建，不崩溃，采集恢复
5. 锁屏再解锁    → 期望 ACCESS_LOST 被捕获，自动重建
6. 插拔外接显示器 → 期望重建成功
7. 开 UAC 弹窗   → 期望不崩溃
```

## 验收标准

- [ ] `cargo test --workspace` 全绿（含新测试）
- [ ] 静止画面 10 秒：`next_frame` 返回 `None` 次数 > 550（60fps × 10s − 少量实际变化）
- [ ] 静止画面 CPU 单核 ≤ 2%
- [ ] 拖动窗口时采集帧率 ≥ 55
- [ ] 上述真机验收脚本 7 步全过
- [ ] `grep -rn unsafe crates/rc-capture-win/src` 只出现在 `ffi/`
- [ ] 每个 `unsafe fn` 有 `// SAFETY:` 注释

## 参考实现

| 读这个 | 学什么 |
|---|---|
| [`rc-types`](../../../crates/rc-types/src/lib.rs) | newtype、错误定义、`CpuFrame::validate` |
| [`rc-diff`](../../../crates/rc-diff/src/lib.rs) | 平台传脏区时如何配合（`merge_rects`） |
| [`rc-sched`](../../../crates/rc-sched/src/lib.rs) | 时间注入（采集的 `timestamp` 要喂给它） |

## 已知坑（来自 docs/04-platforms.md，逐条对照）

| # | 坑 | 正确做法 |
|---|---|---|
| 1 | `AcquireNextFrame(0)` 导致 CPU 空转 | 传 `16` |
| 2 | 忘记 `ReleaseFrame` | RAII guard，Drop 中释放 |
| 3 | `AccessLost` 后黑屏 | 映射 `recoverable: true`，重建 |
| 4 | 重复帧未过滤 | 检查 `DirtyRects` 空 + `LastPresentTime` |
| 5 | 只采主屏 | 每 output 一个实例 |
| 6 | HDR 偏色 | `DXGI_COLOR_SPACE_TYPE` 转 sRGB |
| 7 | TDR（device removed） | 重建 device + duplication |
| 8 | 双光标 | Duplication 不含光标，单独 `GetFramePointerShape` |
| 9 | 全屏独占游戏 | 降级（本卡不处理，记 TODO） |
| 10 | UAC 提权窗口 | 需 SYSTEM 权限（本卡不处理，记 TODO） |

**第 11 个坑（本卡新增）**：
`IDXGIOutputDuplication` 的 `AcquireNextFrame` 返回的 texture 在下一次
`AcquireNextFrame` 前有效。若上层需要异步使用，必须 `CopyResource` 到自己的纹理。
**不要**把 duplication 的纹理裸指针传出去。

## 依赖（Cargo.toml）

```toml
# crates/rc-capture-win/Cargo.toml
[target.'cfg(windows)'.dependencies]
windows = { version = "0.58", features = [
    "Win32_Foundation",
    "Win32_Graphics_Dxgi",
    "Win32_Graphics_Dxgi_Common",
    "Win32_Graphics_Direct3D11",
    "Win32_Graphics_Gdi",
    "Win32_System_Com",
] }
```

**注意**：`rc-capture-win` 在非 Windows 平台应能编译为空 crate（或干脆不加入 members 的默认构建）。
推荐做法：

```toml
# crates/rc-capture-win/Cargo.toml
[package]
# ...
# 非 windows 平台上整个 crate 是空的
```

```rust
// lib.rs
#![cfg(windows)]
```
