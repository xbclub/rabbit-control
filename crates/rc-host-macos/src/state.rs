//! 托盘状态与弹窗文案的纯逻辑函数（无 AppKit 依赖，全平台编译可测）。
//!
//! 本模块锁死菜单项文案、可用性与审批弹窗三要素（对端地址、请求参数、
//! 鉴权状态，对应 T22 要求 U9 与已知坑 13），使 UI 呈现逻辑脱离 AppKit
//! 即可做离线确定性单测。

#![deny(unsafe_code)]

use rc_session::auth::ApprovalRequest;

use crate::TrayState;

/// 托盘菜单项的文案与可用性快照。
///
/// 菜单固定三项（不设二级菜单或设置项）：
/// - 状态行（禁用项）：「RabbitControl · 等待连接 (\<addr\>)」或
///   「RabbitControl · 会话中 (\<remote\>)」；
/// - 「断开当前会话」：仅 [`TrayState::InSession`] 时可用；
/// - 「退出」：恒可用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayMenuLabels {
    /// 顶部状态行文案（菜单中呈禁用态，仅用于展示当前状态）。
    pub status_line: String,
    /// 断开当前会话菜单项标题。
    pub disconnect_title: String,
    /// 断开当前会话菜单项是否可点击（仅会话中为 `true`）。
    pub disconnect_enabled: bool,
    /// 退出菜单项标题。
    pub quit_title: String,
}

/// 根据当前托盘状态计算菜单各项的文案与可用性（纯函数）。
pub fn menu_labels(state: &TrayState) -> TrayMenuLabels {
    let (status_line, disconnect_enabled) = match state {
        TrayState::WaitingForConnection(addr) => {
            (format!("RabbitControl · 等待连接 ({addr})"), false)
        }
        TrayState::InSession(remote) => (format!("RabbitControl · 会话中 ({remote})"), true),
    };

    TrayMenuLabels {
        status_line,
        disconnect_title: "断开当前会话".to_string(),
        disconnect_enabled,
        quit_title: "退出".to_string(),
    }
}

/// 根据 [`ApprovalRequest`] 生成审批弹窗的 `(messageText, informativeText)`。
///
/// 以 [`ApprovalRequest`] 为唯一信息源（U9），包含三要素：
/// 1. 对端传输层地址（措辞「来自 \<addr\> 的连接请求」，不承诺中继后的发起者身份，见已知坑 13）；
/// 2. 请求流参数（`WxH@fps`）；
/// 3. 鉴权状态（「密码已验证」或「未启用密码」）。
pub fn alert_texts(req: &ApprovalRequest) -> (String, String) {
    let message_text = format!("来自 {} 的连接请求", req.remote_addr);
    let auth_status = if req.password_verified {
        "密码已验证"
    } else {
        "未启用密码"
    };
    let informative_text = format!(
        "请求参数：{}x{}@{}fps\n鉴权状态：{}",
        req.requested.width, req.requested.height, req.requested.fps, auth_status
    );
    (message_text, informative_text)
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use rc_session::auth::ApprovalRequest;
    use rc_session::proto::{AuthMethod, SessionParams};

    use super::*;

    #[test]
    fn menu_labels_render_state() {
        let listen = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9901);
        let waiting = menu_labels(&TrayState::WaitingForConnection(listen));
        assert_eq!(
            waiting.status_line,
            "RabbitControl · 等待连接 (127.0.0.1:9901)"
        );
        assert_eq!(waiting.disconnect_title, "断开当前会话");
        assert!(!waiting.disconnect_enabled);
        assert_eq!(waiting.quit_title, "退出");

        let remote = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)), 54321);
        let active = menu_labels(&TrayState::InSession(remote));
        assert_eq!(
            active.status_line,
            "RabbitControl · 会话中 (192.168.1.42:54321)"
        );
        assert_eq!(active.disconnect_title, "断开当前会话");
        assert!(active.disconnect_enabled);
        assert_eq!(active.quit_title, "退出");
    }

    #[test]
    fn alert_texts_contain_three_elements() {
        let remote = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7)), 60000);
        let params = SessionParams {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_bps: 8_000_000,
        };

        let req_no_pwd = ApprovalRequest {
            session_id: 1,
            remote_addr: remote,
            requested: params,
            auth_method: AuthMethod::None,
            password_verified: false,
        };
        let (msg1, info1) = alert_texts(&req_no_pwd);
        assert!(
            msg1.contains("10.0.0.7:60000"),
            "messageText 必须含对端地址：{msg1}"
        );
        assert!(
            info1.contains("1920x1080@60"),
            "informativeText 必须含请求参数：{info1}"
        );
        assert!(
            info1.contains("未启用密码"),
            "informativeText 必须含未启用密码状态：{info1}"
        );

        let req_pwd = ApprovalRequest {
            session_id: 2,
            remote_addr: remote,
            requested: params,
            auth_method: AuthMethod::Password,
            password_verified: true,
        };
        let (msg2, info2) = alert_texts(&req_pwd);
        assert!(
            msg2.contains("10.0.0.7:60000"),
            "messageText 必须含对端地址：{msg2}"
        );
        assert!(
            info2.contains("1920x1080@60"),
            "informativeText 必须含请求参数：{info2}"
        );
        assert!(
            info2.contains("密码已验证"),
            "informativeText 必须含密码已验证状态：{info2}"
        );
    }
}
