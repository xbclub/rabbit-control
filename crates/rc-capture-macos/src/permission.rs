//! 三重授权检测与引导（T12 的 M1–M4）。
//!
//! macOS 上远控被控端需要三项独立授权，缺一不可：
//!
//! | 权限 | 检测 API | 备注 |
//! |---|---|---|
//! | 屏幕录制 | `CGPreflightScreenCaptureAccess` | 只查不弹（M1） |
//! | 辅助功能 | `AXIsProcessTrusted` | 只查不弹（M2），注入事件的前置 |
//! | 输入监控 | `IOHIDCheckAccess(ListenEvent)` | 只查不弹（M3），监听事件的前置 |
//!
//! **M4（macOS 限制）**：这三项授权变更后，已运行进程**不会立即生效**，
//! 必须重启进程。用 [`PermissionStatus::restart_hint`] 在前后两次
//! [`PermissionStatus::check`] 之间生成给用户看的提示。
//!
//! 坑 1/2（TCC 按签名记录授权）：开发期频繁重编译会让授权“反复失效”，
//! 用固定证书签名可避免；那是打包（M5 打包卡）的事，本 crate 只负责如实检测。
//!
//! 本文件不含 unsafe —— 所有 FFI 都在 [`crate::ffi`]。

use rc_types::{Error, Result};

/// 三项授权之一的标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionKind {
    /// 屏幕录制（Screen Recording）。
    ScreenRecording,
    /// 辅助功能（Accessibility）。
    Accessibility,
    /// 输入监控（Input Monitoring）。
    InputMonitoring,
}

/// 三项授权的状态快照。
///
/// 构造途径有两个：
/// - [`PermissionStatus::check`]：真机上的真实检测（不弹窗）；
/// - 直接结构体字面量：测试 / UI 预填。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PermissionStatus {
    /// 屏幕录制（`CGPreflightScreenCaptureAccess`）。
    pub screen_recording: bool,
    /// 辅助功能（`AXIsProcessTrusted`）。
    pub accessibility: bool,
    /// 输入监控（`IOHIDCheckAccess(ListenEvent)`）。
    pub input_monitoring: bool,
}

impl PermissionStatus {
    /// 检测三项权限。**不弹窗**（M1/M2/M3：三项全部用 preflight 类 API）。
    ///
    /// 建议的引导流程：
    /// 1. 启动时 `check()`，`all_granted()` 为真才继续；
    /// 2. 缺哪项就 [`open_settings`](Self::open_settings) 跳哪项，
    ///    屏幕录制首跑可用 [`request_screen_recording`](Self::request_screen_recording)
    ///    触发一次系统弹窗（坑 3：之后只能跳设置页）；
    /// 3. 用户在系统设置里改完后再次 `check()`，并用
    ///    [`restart_hint`](Self::restart_hint) 提示**重启进程**（M4）。
    pub fn check() -> Self {
        Self {
            screen_recording: crate::ffi::preflight_screen_capture_access(),
            accessibility: crate::ffi::ax_is_process_trusted(),
            input_monitoring: crate::ffi::iohid_check_input_monitoring(),
        }
    }

    /// 三项全部具备。
    pub fn all_granted(&self) -> bool {
        self.screen_recording && self.accessibility && self.input_monitoring
    }

    /// 打开 `which` 对应的系统设置页（Privacy & Security 下的对应条目）。
    ///
    /// 用 `open` 命令跳转 —— 单测里只验证 URL 映射
    /// （`settings_url`），**不真的调用**本函数（会弹 GUI）。
    pub fn open_settings(&self, which: PermissionKind) -> Result<()> {
        let url = settings_url(which);
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(Error::Io)
    }

    /// 请求屏幕录制权限：首次会弹系统对话框，之后**不再弹**（坑 3），
    /// 用户拒绝过就只能走 [`open_settings`](Self::open_settings)。
    ///
    /// 返回请求后的授权状态。注意：即使用户当场同意，**本进程也需要重启**
    /// 才能真正采集（M4，见 [`restart_hint`](Self::restart_hint)）。
    pub fn request_screen_recording() -> bool {
        crate::ffi::request_screen_capture_access()
    }

    /// M4：授权状态变化 → 需要重启进程的提示文案。
    ///
    /// macOS 不允许已运行进程吃到授权变更（尤其是屏幕录制），
    /// 授权面板操作完成后必须重启 RabbitControl。对比前后两次
    /// [`check`](Self::check) 的结果，任何一项发生变化就返回提示；
    /// 完全相同则返回 `None`。
    pub fn restart_hint(before: &PermissionStatus, after: &PermissionStatus) -> Option<String> {
        let mut changed: Vec<&str> = Vec::new();
        if before.screen_recording != after.screen_recording {
            changed.push("屏幕录制");
        }
        if before.accessibility != after.accessibility {
            changed.push("辅助功能");
        }
        if before.input_monitoring != after.input_monitoring {
            changed.push("输入监控");
        }
        if changed.is_empty() {
            return None;
        }
        Some(format!(
            "{}授权已变更，需要重启本进程后才能生效（macOS 限制）",
            changed.join("、")
        ))
    }
}

/// 每项权限对应的系统设置页 URL。
///
/// 锚点取系统设置（System Settings）兼容 System Preferences 的老 scheme：
/// - 屏幕录制 → `Privacy_ScreenCapture`
/// - 辅助功能 → `Privacy_Accessibility`
/// - 输入监控 → `Privacy_ListenEvent`
const fn settings_url(kind: PermissionKind) -> &'static str {
    match kind {
        PermissionKind::ScreenRecording => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
        }
        PermissionKind::Accessibility => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
        }
        PermissionKind::InputMonitoring => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_status_all_granted_requires_three() {
        // 纯逻辑：三项全真才算 granted。
        let all = PermissionStatus {
            screen_recording: true,
            accessibility: true,
            input_monitoring: true,
        };
        assert!(all.all_granted());

        // 缺任何一项都不行（三种单项缺失 + 全缺）。
        assert!(!PermissionStatus {
            screen_recording: false,
            accessibility: true,
            input_monitoring: true,
        }
        .all_granted());
        assert!(!PermissionStatus {
            screen_recording: true,
            accessibility: false,
            input_monitoring: true,
        }
        .all_granted());
        assert!(!PermissionStatus {
            screen_recording: true,
            accessibility: true,
            input_monitoring: false,
        }
        .all_granted());
        assert!(!PermissionStatus::default().all_granted());
    }

    #[test]
    fn permission_open_settings_maps_kind_correctly() {
        // 只验证 kind → URL 的映射（open_settings 真正执行会弹系统设置 GUI，
        // 不能在单测里调用）。锚点写错会静默跳到设置主页，用户找不到条目。
        for kind in [
            PermissionKind::ScreenRecording,
            PermissionKind::Accessibility,
            PermissionKind::InputMonitoring,
        ] {
            let url = settings_url(kind);
            assert!(
                url.starts_with("x-apple.systempreferences:com.apple.preference.security?Privacy_"),
                "{kind:?} 的 URL 应指向隐私与安全性面板: {url}"
            );
        }
        assert!(settings_url(PermissionKind::ScreenRecording).contains("Privacy_ScreenCapture"));
        assert!(settings_url(PermissionKind::Accessibility).contains("Privacy_Accessibility"));
        assert!(settings_url(PermissionKind::InputMonitoring).contains("Privacy_ListenEvent"));
    }

    #[test]
    fn restart_hint_reports_changed_kinds() {
        // M4 的辅助逻辑：变化才有提示，没变化不打扰。
        let before = PermissionStatus::default();
        let same = PermissionStatus::default();
        assert!(PermissionStatus::restart_hint(&before, &same).is_none());

        let after = PermissionStatus {
            screen_recording: true,
            ..before
        };
        let hint =
            PermissionStatus::restart_hint(&before, &after).expect("屏幕录制变化必须产生提示");
        assert!(hint.contains("屏幕录制"), "提示要点名变化的权限: {hint}");
        assert!(hint.contains("重启"), "提示必须要求重启进程: {hint}");
    }
}
