//! macOS 系统权限预检与引导（屏幕录制 / 辅助功能 / 输入监控）。
//!
//! 全模块 `#![deny(unsafe_code)]`：macOS 上委托 [`rc_capture_macos::PermissionStatus`]
//! 完成只查不弹的 preflight 检测、首次系统授权请求与系统设置跳转，并在进程首次检测时
//! 冻结启动期权限快照以生成 `restart_hint`；非 macOS 平台返回 `platform_supported: false`
//! 的全放行占位快照。

#![deny(unsafe_code)]
#![deny(missing_docs)]

/// 供前端渲染与启动前门禁使用的系统权限状态快照（IPC `camelCase`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionSnapshot {
    /// 当前平台是否需要系统权限预检（macOS 为 `true`，其余平台为 `false`）。
    pub platform_supported: bool,
    /// 屏幕录制权限状态（macOS: `CGPreflightScreenCaptureAccess`）。
    pub screen_recording: bool,
    /// 辅助功能权限状态（macOS: `AXIsProcessTrusted`）。
    pub accessibility: bool,
    /// 输入监控权限状态（macOS: `IOHIDCheckAccess(ListenEvent)`）。
    pub input_monitoring: bool,
    /// 本机作为被控端所需必需权限是否齐备（`screen_recording && accessibility`）。
    pub host_ready: bool,
    /// 三项系统权限是否全部授予（`screen_recording && accessibility && input_monitoring`）。
    pub all_granted: bool,
    /// 缺失的必需权限中文名称列表（例如 `["屏幕录制", "辅助功能"]`）。
    pub missing_required: Vec<String>,
    /// 若较进程启动时授权状态发生变化，提示用户重启进程使授权生效。
    pub restart_hint: Option<String>,
}

impl PermissionSnapshot {
    /// 由平台支持标记、三项权限布尔状态与可选重启提示构造快照（纯逻辑，便于离线单测）。
    pub fn from_flags(
        platform_supported: bool,
        screen_recording: bool,
        accessibility: bool,
        input_monitoring: bool,
        restart_hint: Option<String>,
    ) -> Self {
        let mut missing_required = Vec::new();
        if !screen_recording {
            missing_required.push("屏幕录制".to_string());
        }
        if !accessibility {
            missing_required.push("辅助功能".to_string());
        }
        Self {
            platform_supported,
            screen_recording,
            accessibility,
            input_monitoring,
            host_ready: screen_recording && accessibility,
            all_granted: screen_recording && accessibility && input_monitoring,
            missing_required,
            restart_hint,
        }
    }
}

/// 跨平台权限类型枚举（解析前端传入的 `kind` 字符串参数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParsedPermissionKind {
    ScreenRecording,
    Accessibility,
    InputMonitoring,
}

fn parse_permission_kind(kind: &str) -> Result<ParsedPermissionKind, String> {
    match kind {
        "screen_recording" => Ok(ParsedPermissionKind::ScreenRecording),
        "accessibility" => Ok(ParsedPermissionKind::Accessibility),
        "input_monitoring" => Ok(ParsedPermissionKind::InputMonitoring),
        other => Err(format!(
            "不支持的权限类型 \"{other}\"，仅支持 screen_recording / accessibility / input_monitoring"
        )),
    }
}

/// 校验开启本机被控端服务前的必需系统权限；在不支持权限预检的平台直接放行。
pub fn ensure_host_ready(snapshot: &PermissionSnapshot) -> Result<(), String> {
    if !snapshot.platform_supported || snapshot.host_ready {
        return Ok(());
    }
    let missing = if snapshot.missing_required.is_empty() {
        "屏幕录制、辅助功能".to_string()
    } else {
        snapshot.missing_required.join("、")
    };
    Err(format!(
        "开启远程协助前请先在 macOS 系统设置中授予「{missing}」权限，授权后若未立即生效请重启 RabbitControl"
    ))
}

#[cfg(target_os = "macos")]
fn boot_permission_status() -> rc_capture_macos::PermissionStatus {
    use std::sync::OnceLock;
    static BOOT_STATUS: OnceLock<rc_capture_macos::PermissionStatus> = OnceLock::new();
    *BOOT_STATUS.get_or_init(rc_capture_macos::PermissionStatus::check)
}

/// 非阻塞检测当前系统权限状态（macOS 只查不弹；非 macOS 返回全放行占位快照）。
pub fn check_permissions() -> PermissionSnapshot {
    #[cfg(target_os = "macos")]
    {
        let boot = boot_permission_status();
        let current = rc_capture_macos::PermissionStatus::check();
        let restart_hint = rc_capture_macos::PermissionStatus::restart_hint(&boot, &current);
        PermissionSnapshot::from_flags(
            true,
            current.screen_recording,
            current.accessibility,
            current.input_monitoring,
            restart_hint,
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        PermissionSnapshot::from_flags(false, true, true, true, None)
    }
}

/// 请求屏幕录制权限（macOS 首次会弹出系统授权对话框），并返回最新的权限状态快照。
pub fn request_screen_recording() -> PermissionSnapshot {
    #[cfg(target_os = "macos")]
    {
        let _ = boot_permission_status();
        let _ = rc_capture_macos::PermissionStatus::request_screen_recording();
        check_permissions()
    }
    #[cfg(not(target_os = "macos"))]
    {
        check_permissions()
    }
}

/// 打开指定权限对应的系统设置隐私面板。
///
/// 支持的 `kind` 取值：`"screen_recording"`、`"accessibility"`、`"input_monitoring"`。
pub fn open_permission_settings(kind: &str) -> Result<(), String> {
    let parsed = parse_permission_kind(kind)?;
    #[cfg(target_os = "macos")]
    {
        let mapped = match parsed {
            ParsedPermissionKind::ScreenRecording => {
                rc_capture_macos::PermissionKind::ScreenRecording
            }
            ParsedPermissionKind::Accessibility => rc_capture_macos::PermissionKind::Accessibility,
            ParsedPermissionKind::InputMonitoring => {
                rc_capture_macos::PermissionKind::InputMonitoring
            }
        };
        rc_capture_macos::PermissionStatus::default()
            .open_settings(mapped)
            .map_err(|e| format!("打开系统设置失败：{e}"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = parsed;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_from_flags_computes_host_ready_all_granted_and_missing_required() {
        let none = PermissionSnapshot::from_flags(true, false, false, false, None);
        assert!(none.platform_supported);
        assert!(!none.host_ready);
        assert!(!none.all_granted);
        assert_eq!(none.missing_required, vec!["屏幕录制", "辅助功能"]);
        assert_eq!(none.restart_hint, None);

        let only_screen = PermissionSnapshot::from_flags(true, true, false, false, None);
        assert!(!only_screen.host_ready);
        assert!(!only_screen.all_granted);
        assert_eq!(only_screen.missing_required, vec!["辅助功能"]);

        let only_ax = PermissionSnapshot::from_flags(true, false, true, true, None);
        assert!(!only_ax.host_ready);
        assert!(!only_ax.all_granted);
        assert_eq!(only_ax.missing_required, vec!["屏幕录制"]);

        let host_ready_without_input = PermissionSnapshot::from_flags(
            true,
            true,
            true,
            false,
            Some("屏幕录制授权已变更，需要重启本进程后才能生效（macOS 限制）".to_string()),
        );
        assert!(host_ready_without_input.host_ready);
        assert!(!host_ready_without_input.all_granted);
        assert!(host_ready_without_input.missing_required.is_empty());
        assert!(host_ready_without_input.restart_hint.is_some());

        let all = PermissionSnapshot::from_flags(true, true, true, true, None);
        assert!(all.host_ready);
        assert!(all.all_granted);
        assert!(all.missing_required.is_empty());
    }

    #[test]
    fn snapshot_serializes_with_camel_case_fields() {
        let snap = PermissionSnapshot::from_flags(
            true,
            true,
            false,
            false,
            Some("需重启生效".to_string()),
        );
        let json = serde_json::to_value(&snap).expect("serialize PermissionSnapshot");
        assert_eq!(json["platformSupported"], true);
        assert_eq!(json["screenRecording"], true);
        assert_eq!(json["accessibility"], false);
        assert_eq!(json["inputMonitoring"], false);
        assert_eq!(json["hostReady"], false);
        assert_eq!(json["allGranted"], false);
        assert_eq!(json["missingRequired"], serde_json::json!(["辅助功能"]));
        assert_eq!(json["restartHint"], "需重启生效");
    }

    #[test]
    fn ensure_host_ready_enforces_required_permissions_only_when_supported() {
        let missing_both = PermissionSnapshot::from_flags(true, false, false, false, None);
        let err = ensure_host_ready(&missing_both).unwrap_err();
        assert!(err.contains("屏幕录制"), "错误信息应包含屏幕录制: {err}");
        assert!(err.contains("辅助功能"), "错误信息应包含辅助功能: {err}");

        let ready = PermissionSnapshot::from_flags(true, true, true, false, None);
        assert!(ensure_host_ready(&ready).is_ok());

        let unsupported = PermissionSnapshot::from_flags(false, false, false, false, None);
        assert!(ensure_host_ready(&unsupported).is_ok());
    }

    #[test]
    fn open_permission_settings_rejects_invalid_kind_without_spawning() {
        assert_eq!(
            parse_permission_kind("screen_recording"),
            Ok(ParsedPermissionKind::ScreenRecording)
        );
        assert_eq!(
            parse_permission_kind("accessibility"),
            Ok(ParsedPermissionKind::Accessibility)
        );
        assert_eq!(
            parse_permission_kind("input_monitoring"),
            Ok(ParsedPermissionKind::InputMonitoring)
        );

        let err = open_permission_settings("camera").unwrap_err();
        assert!(err.contains("camera"), "错误信息应指明非法类型: {err}");
        assert!(open_permission_settings("").is_err());
    }
}
