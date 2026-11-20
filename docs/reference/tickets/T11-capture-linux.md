# T11 Linux 采集

> 实现 `rc-capture-linux`：X11（XDamage）与 Wayland（PipeWire + portal）两条路径。

## 前置

- [ ] T03 完成（`CaptureBackend` trait 与 `CapturedFrame` 已定型）

## 背景

Linux 有两条完全不同的采集路径，且**不能互相替代**：

| 会话 | 路径 | 难度 |
|---|---|---|
| X11（含 XWayland 应用） | `XShmGetImage` + `XDamage` | 低 |
| Wayland 原生 | PipeWire + `xdg-desktop-portal` + dmabuf | **高** |

Wayland 的困难在于：必须走 D-Bus 与 portal 交互、必须用户授权弹窗、
不同合成器（GNOME/KDE/wlroots）行为不一致。

## 平台要求

> **优先级 P2**（2026-11-20 变更）：Linux 暂不投入，本卡归档待命。

**Linux 真机，且需要 X11 与 Wayland 两种会话都能切换测试。**
建议：GNOME 46+ 与 KDE 6+ 各测一遍。

## 交付物

| 文件 | 动作 | 说明 |
|---|---|---|
| `crates/rc-capture-linux/Cargo.toml` | 新建 | |
| `crates/rc-capture-linux/src/lib.rs` | 新建 | 工厂（按会话类型选择） |
| `crates/rc-capture-linux/src/x11.rs` | 新建 | XShm + XDamage |
| `crates/rc-capture-linux/src/wayland/mod.rs` | 新建 | PipeWire |
| `crates/rc-capture-linux/src/wayland/portal.rs` | 新建 | D-Bus portal 调用 |
| `crates/rc-capture-linux/src/wayland/dmabuf.rs` | 新建 | dmabuf 描述符管理 |
| `crates/rc-input-linux/src/lib.rs` | 新建 | XTest / uinput |

## API 契约

同 `CaptureBackend`（见 [`20-crate-contracts.md`](../impl/20-crate-contracts.md) §7.1）。

X11 后端额外提供脏区（来自 `XDamage`），Wayland 后端**不提供**脏区（需 `TileDiff`）。

```rust
/// 检测当前会话类型
pub fn detect_session() -> SessionType;

pub enum SessionType {
    X11,
    Wayland { composer: Composer },   // Gnome / Kde / Wlroots / Unknown
}

/// 工厂：按会话类型创建后端
pub fn create_backend(cfg: &CaptureConfig) -> Result<Box<dyn CaptureBackend>>;
```

## 必须做到

### X11 路径

| # | 要求 | 违反后果 |
|---|---|---|
| X1 | 用 `XDamage` 驱动，不轮询 | 静态画面 CPU 满载 |
| X2 | `XShm` 零拷贝（共享内存段） | 每帧 `memcpy` |
| X3 | 处理 `XDamage` 的 `NotifyNonEmpty` 与 `NotifyLevel` | 漏帧或重复 |
| X4 | `XShmGetImage` 失败（如 screen 变化）时重建 | 崩溃 |

### Wayland 路径

| # | 要求 | 违反后果 |
|---|---|---|
| W1 | **必须走 `xdg-desktop-portal`**（`org.freedesktop.portal.ScreenCast`） | 无其他合法途径 |
| W2 | 正确处理用户拒绝授权 | 崩溃或死锁 |
| W3 | dmabuf fd 正确传递与关闭（防 fd 泄漏） | fd 耗尽 |
| W4 | 不同合成器的 dmabuf modifier 差异 | 编码器拒绝 |
| W5 | 光标需 `SPA_META_Cursor` 单独处理（不在画面里） | 无光标 |
| W6 | 帧率由合成器决定，不假设 60 | 帧率统计错误 |

## 不要做什么

- ❌ 不要试图绕过 Wayland 的 portal（那是违反设计的行为）。
- ❌ 不要用 `xcap` 的默认轮询路径。它不适合 60fps。
- ❌ 不要在本卡做 VAAPI 编码（独立任务，可复用 T04 的抽象）。
- ❌ 不要假设 X11 一定可用（Wayland 会越来越普遍）。

## 测试清单

| 测试名 | 验证 |
|---|---|
| `detect_session_identifies_x11` | 环境变量模拟 |
| `detect_session_identifies_wayland` | `WAYLAND_DISPLAY` 存在 |
| `x11_damage_rects_merge_correctly` | 脏区合并 |
| `wayland_dmabuf_fd_not_leaked` | W3：用 `/proc/self/fd` 计数 |
| `wayland_portal_permission_denied_returns_error` | W2 |
| `frame_timestamp_monotonic` | 时间戳 |
| `create_backend_fails_gracefully_without_display` | 无 DISPLAY 环境 |

真机（`#[ignore]`）：

```rust
#[test] #[ignore = "requires X11 session"]
fn x11_captures_at_60fps() { ... }

#[test] #[ignore = "requires Wayland session + portal permission"]
fn wayland_captures_via_pipewire() { ... }
```

## 验收命令

```bash
cargo test -p rc-capture-linux -p rc-input-linux
cargo clippy -p rc-capture-linux --all-targets -- -D warnings

# 真机（X11 会话）
cargo test -p rc-capture-linux -- --ignored

# 真机（Wayland 会话）
# 先切换到 Wayland 登录，再跑同样命令
```

## 验收标准

### X11

- [ ] 1080p 采集 ≥58fps
- [ ] 静态画面 CPU（单核）≤5%
- [ ] 拖动窗口时脏区有效缩小（不是整帧）
- [ ] 分辨率变化后自动重建

### Wayland（GNOME 46+）

- [ ] portal 授权流程完整（拒绝 → 明确错误）
- [ ] 采集成功，帧率 ≥30（受合成器限制）
- [ ] dmabuf 零拷贝到编码器
- [ ] **无 fd 泄漏**：连续 5 分钟，`/proc/self/fd` 数量稳定
- [ ] 光标单独处理

### Wayland（KDE 6+）

- [ ] 同 GNOME 全部项

### 输入

- [ ] X11：`XTest` 注入可用
- [ ] Wayland：uinput 或 libei 可用（若均不可用，记录为已知限制）

## 参考实现

| 读这个 | 学什么 |
|---|---|
| [`rc-diff`](../../../crates/rc-diff/src/lib.rs) | Wayland 无脏区时用 `TileDiff` |
| [`rc-types`](../../../crates/rc-types/src/lib.rs) | `PixelFormat`、`Resolution::to_even` |

## 已知坑（Linux 特有）

| # | 坑 | 处理 |
|---|---|---|
| 1 | `XDamage` 需要 `XDamageQueryExtension` 检查 | 缺失时降级轮询 |
| 2 | `XShm` 需要 `MIT-SHM` 扩展 | 缺失时降级 `XGetImage` |
| 3 | Wayland portal 每个应用每次会话都要授权 | 提供清晰的引导 UI |
| 4 | `SPA_VIDEO_FORMAT_BGRA` vs `BGRx` | 按实际格式处理，不要假设 |
| 5 | dmabuf modifier 必须传递给编码器 | `VASurface` 导入时指定 |
| 6 | GNOME 的 portal 版本差异（`ScreenCast` vs `RemoteDesktop`） | 都试 |
| 7 | 多次 `Start` 相同 session 会挂起 | 复用或显式 `Stop` |
| 8 | uinput 需要 `input` 组或 root | 安装时配置 udev 规则 |

## 依赖

```toml
[dependencies]
rc-types = { path = "../rc-types" }
rc-diff = { path = "../rc-diff" }

[target.'cfg(target_os = "linux")'.dependencies]
x11rb = "0.13"           # 纯 Rust X11 绑定，比 xcb 安全
zbus = "4"               # D-Bus
pipewire = "0.8"
libc = "0.2"
```

**建议用 `x11rb` 而非 `x11`/`xcb`**：纯 Rust，无 unsafe 暴露给业务层。
