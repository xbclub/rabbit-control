# 04 三平台实现要点

> 本文档只记录「容易踩坑、且 AI 无法通过编译验证」的具体细节。通用逻辑见 [02 架构](02-architecture.md)。

## 1. 平台优先级

> **2026-11-20 变更**：用户主要远控 macOS，次要 Windows。原文档把 Windows 定为 P0，已修正。

| 优先 | 平台 | 理由 |
|---|---|---|
| **P0** | **macOS 13+（Apple Silicon）** | **用户主要远控目标；开发机即是，可立即验证** |
| **P1** | Windows 10 2004+ / 11 | 次要目标；需物理机验证 GPU 编码 |
| **P2** | Linux（X11 / Wayland） | 后续计划，暂不投入 |

**这意味着：**
- 主线任务 = **T12（macOS 采集）→ T08（M1 Gate）**
- Windows 任务（T03/T04/T05）降为 P1，可延后
- Linux 任务（T11）P2，归档

**关于验证环境**：macOS 的开发机与远控目标可以是同一类机器，M1 Gate 可直接在本机验证。
Windows 作为 P1，仍需要物理机（虚拟机的 GPU 编码质量不可信）。

**macOS 的最大障碍不是技术，是授权**：屏幕录制、辅助功能、输入监控三项缺一不可，
且 TCC 按「签名 + bundle id」记录，重编译后可能失效。详见 §4.1。


## 2. Windows

### 2.1 采集：DXGI Desktop Duplication

```rust
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Direct3D11::*;

// 关键 crate：windows (0.58+) 提供类型化 COM 封装
// 禁止用 raw vtable 手写绑定 —— 类型系统是本项目对抗 AI 错误的第一道防线
```

**必须处理的坑（按踩坑概率排序）**：

| # | 问题 | 表现 | 正确处理 |
|---|---|---|---|
| 1 | `AcquireNextFrame` 传 `0` 超时 | 静止画面 CPU 空转 | 传 `16`（ms）阻塞等待；或传 `0` 但配合 `GetFrameDirtyRects` 判断 |
| 2 | 忘记 `ReleaseFrame` | 下一次 `AcquireNextFrame` 返回 `DXGI_ERROR_INVALID_CALL`，**采集永久停止** | RAII 封装：`FrameGuard` 在 Drop 中 Release |
| 3 | `DXGI_ERROR_ACCESS_LOST` | 锁屏/UAC/切分辨率后黑屏 | 区分「可恢复」（重建 duplication）与「不可恢复」（重试 3 次后降级 GDI） |
| 4 | 重复帧未过滤 | 静止时 CPU 30% | 用 `GetFrameDirtyRects` 返回空 + `LastPresentTime` 未变 → 跳过 |
| 5 | 多显示器分别 duplication | 只采主屏 | 每个 output 一个 duplication；支持「全屏合成」模式 |
| 6 | HDR 输出 | 画面发灰/过曝 | `DXGI_COLOR_SPACE_TYPE` 显式转换到 sRGB，或直传 HDR 给编码器（HEVC Main10） |
| 7 | D3D11 device 丢失（TDR） | `DEVICE_REMOVED` | 重建 device + duplication，保留原帧继续发 |
| 8 | 光标被包含在画面里 | 双光标 | Duplication 默认**不包含**光标（`DesktopImageInSystemMemory` 模式下）；需单独 `GetFramePointerShape` 取形状 |
| 9 | 全屏独占游戏 | Duplication 失败 | 降级到 `Windows.Graphics.Capture`（WinRT）或 GDI BitBlt |
| 10 | 提权窗口（UAC） | 黑屏 | 需被控端进程以 SYSTEM 权限运行，或用 `Windows.Graphics.Capture`（需用户授权） |

**捕获 API 选型补充**：`Windows.Graphics.Capture`（WinRT）是微软推荐的新方式，支持 UWP/窗口级捕获、自带光标合成，但延迟略高于 DXGI Duplication。**P0 用 DXGI Duplication（延迟最低），P1 增加 WinRT 作为兜底。**

### 2.2 编码

| 编码器 | crate/方案 | 备注 |
|---|---|---|
| NVENC | `cudarc` + 自写 NVENC 绑定，或 `nvenc-sys` | 需 CUDA driver，非 CUDA runtime |
| AMF | 自写绑定（`amfrt64.dll`） | 文档质量一般，需真机验证 |
| QSV | 通过 `libmfx` 或直接 `MFXVideoENCODE` | 老平台可用，新平台建议直接 NVENC |
| Media Foundation | `windows` crate 的 `IMFTransform` | **系统级兜底**：任何有 GPU 的 Windows 都有 MF 编码器，性能弱于厂商 API 但最普及 |

**推荐优先级：NVENC → AMF → QSV → Media Foundation → openh264 软件。**
Media Foundation 常被忽略，但它是唯一「所有 Windows 都能用」的硬件加速路径，必须实现。

### 2.3 输入注入

```rust
// 推荐：SendInput（用户态，需在交互式会话中）
// 高权限场景：需驱动或 SYSTEM 服务
```

| 坑 | 处理 |
|---|---|
| 注入的输入带 `LLMHF_INJECTED` 标记，部分游戏/反作弊拒绝 | 明确告知用户；不做反检测（合规红线） |
| UAC 提权窗口无法注入 | 需 SYSTEM 权限或 UIAccess 签名 |
| 键盘布局映射 | 用**扫描码**（scancode）而非虚拟键码，避免布局差异 |
| 多显示器坐标 | 用虚拟桌面坐标系，注意负坐标（副屏在主屏左侧） |
| 光标位置与注入不一致 | 注入后用 `GetCursorPos` 校准 |

### 2.4 服务化

| 需求 | 方案 |
|---|---|
| 开机自启 | 注册为服务，或 `HKCU\...\Run`（用户级） |
| 锁屏后可连 | 服务运行在 Session 0，需要「会话桥接」（`WTSQueryUserToken` + `CreateProcessAsUser` 到活动会话） |
| 高权限采集（UAC 窗口） | 服务 + SYSTEM 权限 |
| 免安装（便携版） | 单 exe，无服务，功能受限（锁屏不可连） |

**这四项是 Windows 平台最大的工作量来源，远超采集和编码本身。** 建议：P0 只做「用户级 + 便携版」，服务化放 P1。

## 3. Linux

### 3.1 采集：X11 与 Wayland 两条完全不同的路

**X11（P1 优先）**

| 方案 | 说明 |
|---|---|
| `XShmGetImage` + `XDamage` | 经典路径，`XDamage` 提供脏区通知，可达 60fps |
| `xcap` crate | 封装可用，但默认轮询，需自行接 `XDamage` |
| `xf86drm` + `XShm` | 更低延迟，直接拿 GPU 缓冲 |

**Wayland（P1 后段）**

| 方案 | 说明 |
|---|---|
| PipeWire 屏幕共享（`org.freedesktop.portal.ScreenCast`） | **唯一正道**，所有合成器都支持 |
| `xdg-desktop-portal` D-Bus 调用 | 必须走 portal，无其他合法途径 |
| dmabuf 传递 | 零拷贝关键：`SPA_VIDEO_FORMAT` + `DMA_BUF`，直接给 VAAPI 编码器 |

**Wayland 的核心困难**：

1. **必须通过 D-Bus 与 portal 交互**（`zbus` crate）
2. **授权需要用户交互**（每个会话弹窗），无法静默
3. **帧率由合成器决定**（`PipeWire` 按合成器提交节奏给帧），可能不是 60
4. **不同合成器行为不一致**（GNOME/KDE/wlroots 在 dmabuf modifier 上有差异）
5. **光标不包含在画面里**，需 `SPA_META_Cursor` 单独处理

**结论：X11 先做（可覆盖 XWayland 应用），Wayland 原生支持作为独立子任务，且需要真机矩阵测试（至少 GNOME + KDE）。**

### 3.2 编码

| 方案 | 说明 |
|---|---|
| VAAPI | `libva`，Mesa 开源驱动；需要 dmabuf 导入（`VASurface` from `DMA_BUF`） |
| NVENC | 同 Windows，`cudarc` 路线 |
| `openh264` | 软件兜底 |

**VAAPI 的坑**：
- 需要 `LIBVA_DRIVER_NAME` 正确（`iHD` / `radeonsi` / `nvidia`）
- dmabuf modifier 必须匹配（NV12 的 tiling modifier）
- 部分驱动只支持 `I420` 不支持 `NV12` → 需要手工转换（会引入一次拷贝）

### 3.3 输入注入

| 会话类型 | 方案 |
|---|---|
| X11 | `XTest` (`XTestFakeKeyEvent` / `XTestFakeMotionEvent`) |
| Wayland | **无通用方案**。需 `libei`（新标准，合成器支持不全）或 `uinput`（需 root / `input` 组） |
| 通用兜底 | `uinput` 创建虚拟设备（需权限配置） |

**Wayland 输入注入是全项目最不确定的一块**。`libei`（`libei` crate）是正确方向，但目前 GNOME 46+ 与 KDE 6+ 才支持，需作为已知限制记录。

### 3.4 打包与权限

| 项 | 方案 |
|---|---|
| 分发 | AppImage（便携）+ deb/rpm（系统集成） |
| 屏幕捕获 | Wayland 需 portal；X11 无特殊权限 |
| 输入注入 | `uinput` 需 `udev` 规则 + 用户组 |
| 自启动 | systemd user unit |
| 系统服务 | systemd system unit（需 root 安装） |

## 4. macOS

### 4.1 授权（最大的门槛）

**macOS 要求三重授权，缺一不可，且用户拒绝后必须引导到系统设置手动开启。**

| 权限 | 用途 | 检测方式 |
|---|---|---|
| 屏幕录制（Screen Recording） | `ScreenCaptureKit` | `CGPreflightScreenCaptureAccess()` |
| 辅助功能（Accessibility） | 输入注入 | `AXIsProcessTrusted()` |
| 输入监控（Input Monitoring） | 全局键鼠监听 | `IOHIDCheckAccess(kIOHIDRequestTypeListenEvent)` |

```rust
// 必须实现的引导流程：
// 1. 启动时检查三项权限
// 2. 缺失时显示中文说明 + 一键跳转到对应设置页
// 3. 用户授权后需重启进程（macOS 限制）
```

**坑**：
- TCC 数据库按「签名 + bundle id」记录，**开发版与发布版签名不同会导致权限反复失效**
- 每次重新编译（签名变化）都要重新授权 → 开发期需用固定签名
- 用户拒绝后 `CGRequestScreenCaptureAccess()` 不会再次弹窗，只能跳设置页

### 4.2 采集：ScreenCaptureKit

```rust
// crate: screencapturekit-rs (或自写 objc2 绑定)
// 最低版本：macOS 12.3；推荐 macOS 13+
```

| 坑 | 处理 |
|---|---|
| `SCStream` 输出回调在独立队列 | 用 `dispatch_queue` 指定专用队列，避免与主线程竞争 |
| 输出格式默认为 BGRA | 需 `SCStreamConfiguration.pixelFormat` 设为 `420v`（NV12）以直接喂 VideoToolbox |
| `SCContentFilter` 需要窗口/显示器列表 | 显示器列表用 `SCShareableContent.current` |
| 光标包含在画面里 | `SCStreamConfiguration.showsCursor = false`，单独传形状 |
| Retina 缩放 | 采集分辨率是物理像素（2x），编码前按需下采样 |
| 停止后无法重启 | `SCStream` 实例**不可复用**，必须重建 |
| 权限变更后崩溃 | 监听权限变化，失效时优雅重建 |

### 4.3 编码：VideoToolbox

| 项 | 说明 |
|---|---|
| 硬件 H.264/HEVC | `VTCompressionSession`，Apple Silicon 全系支持 |
| 零拷贝输入 | `CVPixelBuffer` 从 `SCStream` 直接给 `VTCompressionSession`，无需 CPU 参与 |
| 低延迟配置 | `kVTVideoEncoderSpecification_EnableLowLatencyRateControl = true` |
| 关键帧 | `kVTEncodeFrameOptionKey_ForceKeyFrame` |
| 实时模式 | `kVTCompressionPropertyKey_RealTime = true`（**必须设，否则按文件转码优化**） |

**注意**：`kVTCompressionPropertyKey_RealTime` 未设置时，VideoToolbox 会引入额外缓冲以提升压缩率，**延迟直接增加 20–40ms**。这是 macOS 上最常见的延迟问题。

### 4.4 输入注入

| 方案 | 权限 | 说明 |
|---|---|---|
| `CGEventPost` | 辅助功能 | 标准方案，`kCGHIDEventTap` |
| `IOHIDPostEvent` | 辅助功能 | 更底层，部分场景需要 |
| `NSEvent` 级注入 | 辅助功能 | 应用内有效 |

坑：
- 修饰键状态需精确跟踪（`CGEventSource.flagsState`）
- 键盘布局（输入法）会改变事件语义 → 尽量用物理键码（`kVK_*`）
- 系统快捷键（如 `Cmd+Tab`）需特殊处理，`CGEventPost` 可能被系统拦截

### 4.5 签名与分发

| 项 | 要求 |
|---|---|
| 签名 | Developer ID + Hardened Runtime |
| 公证 | `notarytool` 公证，否则 Gatekeeper 拦截 |
| 权限描述 | `Info.plist` 中 `NSScreenCaptureUsageDescription` 等 |
| 自启动 | `SMAppService`（macOS 13+），弃用 `LaunchAgents` |
| 更新 | Sparkle 2 + EdDSA 签名 |

## 5. 跨平台抽象设计

```rust
// rc-capture/src/lib.rs
pub trait CaptureBackend: Send {
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>>;
    fn poll_invalidation(&mut self) -> Option<InvalidationReason>;
    fn rebuild(&mut self) -> Result<()>;
}

// 平台模块通过 cfg 条件编译，但业务逻辑层不出现任何 #[cfg]
#[cfg(target_os = "windows")]
mod win;
#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "linux")]
mod linux;

pub fn create_backend(cfg: &CaptureConfig) -> Result<Box<dyn CaptureBackend>> {
    #[cfg(target_os = "windows")]
    return Ok(Box::new(win::DxgiBackend::new(cfg)?));
    #[cfg(target_os = "macos")]
    return Ok(Box::new(mac::SckBackend::new(cfg)?));
    #[cfg(target_os = "linux")]
    return Ok(Box::new(linux::PipeWireBackend::new(cfg)?));
}
```

**硬性规则：`#[cfg]` 只允许出现在 `rc-capture` / `rc-encode` / `rc-input` 的工厂函数里，业务逻辑层一律平台无关。** 否则三平台代码会互相污染，AI 改动一处会破坏另一处。

## 6. 平台特性对比表

| 能力 | Windows | Linux X11 | Linux Wayland | macOS |
|---|---|---|---|---|
| 零拷贝采集 | ✅ DXGI texture | ✅ SHM/dmabuf | ✅ dmabuf | ✅ CVPixelBuffer |
| 脏区通知 | ✅ 原生 | ✅ XDamage | ❌ 需自算 | ❌ 需自算 |
| 硬件编码 | ✅ 4 种 | ✅ VAAPI/NVENC | ✅ 同 X11 | ✅ VT |
| 硬件解码 | ✅ DXVA | ✅ VAAPI | ✅ 同 X11 | ✅ VT |
| 输入注入 | ✅ SendInput | ✅ XTest | ⚠️ libei/uinput | ✅ CGEvent |
| 无授权可用 | ✅ | ✅ | ❌ portal 弹窗 | ❌ 三重授权 |
| 锁屏后可连 | ⚠️ 需服务 | ⚠️ 需服务 | ⚠️ 需服务 | ⚠️ 需登录 |
| 开发难度 | 中 | 低 | **高** | 中 |
| 验证难度 | 中（需真机） | 低 | **高（合成器矩阵）** | 中（签名） |

## 7. 平台验收清单

### Windows
- [ ] 1080p60 NVENC 稳定，CPU <15%
- [ ] 切换分辨率/插拔显示器/锁屏/UAC 后自动恢复
- [ ] HDR 屏幕不偏色
- [ ] 多显示器（3 屏）可切换/合成
- [ ] 唯一 `ReleaseFrame` 路径，无 `INVALID_CALL`
- [ ] Media Foundation 兜底可用（无 NVENC 的机器）

### Linux
- [ ] X11 下 1080p60 XDamage 驱动，STATIC 场景 CPU <5%
- [ ] Wayland + GNOME 46 下 PipeWire 采集可用
- [ ] Wayland + KDE 6 下 PipeWire 采集可用
- [ ] VAAPI 编码 + dmabuf 零拷贝
- [ ] XTest 与 uinput 两条输入路径
- [ ] AppImage 在纯净环境运行

### macOS
- [ ] 三项权限引导流程完整
- [ ] ScreenCaptureKit → VideoToolbox 零拷贝
- [ ] `RealTime=true` 生效（延迟 <40ms）
- [ ] Retina 2x 正确下采样
- [ ] 签名公证通过，Gatekeeper 放行
- [ ] 权限被拒时提示清晰，可跳转设置
