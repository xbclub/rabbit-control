//! 会话线程 ⇄ 主线程的审批桥（纯标准库同步原语，无 AppKit 依赖，全平台可测）。
//!
//! 会话侧 [`SessionSide`] 实现 [`ConnectionApprover`]，三方法全部非阻塞立即返回（U2），
//! 保证审批弹窗等待期间会话驱动循环继续泵网络事件；主线程侧 [`UiSide`] 在
//! `TrayHost::run` 的手泵循环中轮询在途请求并回写机主裁决。

#![deny(unsafe_code)]

use std::sync::{Arc, Mutex};

use rc_session::auth::{ApprovalRequest, ApprovalVerdict, ConnectionApprover};

/// 审批桥的共享内部状态（同一时刻至多一个在途请求）。
#[derive(Debug, Default)]
struct BridgeInner {
    /// 是否有在途审批（用于防重复 submit 与 cancel 幂等判定）。
    in_flight: bool,
    /// 单调递增的请求代次（每次 `submit` 递增 1，用于把 `UiSide::resolve` 绑定到
    /// `take_request` 取走的那次请求，防止过期弹窗裁决误判到后续新连接）。
    generation: u64,
    /// 待 UI 侧取走的连接审批请求。
    pending_request: Option<ApprovalRequest>,
    /// 待会话侧取走的机主裁决。
    verdict: Option<ApprovalVerdict>,
    /// 待 UI 侧感知的撤销标记（`take_cancel` 取走后复位）。
    cancelled: bool,
}

/// 会话线程 ⇄ 主线程的审批通道桥。
///
/// 构造后通过 [`ApprovalBridge::ui_side`] 取得主线程侧句柄，通过
/// [`ApprovalBridge::session_side`] 取得会话侧句柄（`Send`）。
#[derive(Debug, Default, Clone)]
pub struct ApprovalBridge {
    inner: Arc<Mutex<BridgeInner>>,
}

impl ApprovalBridge {
    /// 创建一个新的空闲审批桥。
    pub fn new() -> Self {
        Self::default()
    }

    /// 取得会话侧句柄（实现 [`ConnectionApprover`]，可跨线程发送至会话线程）。
    pub fn session_side(self) -> SessionSide {
        SessionSide { inner: self.inner }
    }

    /// 取得主线程 UI 侧句柄（用于手泵事件循环取请求、回写裁决与感知撤销）。
    pub fn ui_side(&self) -> UiSide {
        UiSide {
            inner: Arc::clone(&self.inner),
            taken_gen: None,
        }
    }
}

/// 审批桥的会话侧句柄：实现 [`ConnectionApprover`]，三方法全部非阻塞。
#[derive(Debug, Clone)]
pub struct SessionSide {
    inner: Arc<Mutex<BridgeInner>>,
}

impl ConnectionApprover for SessionSide {
    fn submit(&mut self, req: &ApprovalRequest) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        debug_assert!(
            !guard.in_flight,
            "ApprovalBridge::submit called while another request is already in flight"
        );
        guard.generation = guard.generation.wrapping_add(1);
        guard.in_flight = true;
        guard.pending_request = Some(req.clone());
        guard.verdict = None;
        guard.cancelled = false;
    }

    fn poll_verdict(&mut self) -> Option<ApprovalVerdict> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let verdict = guard.verdict.take()?;
        guard.in_flight = false;
        Some(verdict)
    }

    fn cancel(&mut self) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard.in_flight {
            guard.in_flight = false;
            guard.pending_request = None;
            guard.verdict = None;
            guard.cancelled = true;
        }
    }
}

/// 审批桥的主线程 UI 侧句柄。
#[derive(Debug)]
pub struct UiSide {
    inner: Arc<Mutex<BridgeInner>>,
    /// 最近一次 `take_request` 取走的请求代次（`resolve` 消费后清空）。
    taken_gen: Option<u64>,
}

impl UiSide {
    /// 非阻塞取走待审批请求（同一时刻至多一个在途；取走后清空槽位并记录代次）。
    pub fn take_request(&mut self) -> Option<ApprovalRequest> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let req = guard.pending_request.take()?;
        self.taken_gen = Some(guard.generation);
        Some(req)
    }

    /// 回写机主裁决（点击「接受」或「拒绝」后调用）。
    ///
    /// 仅当当前仍在途且代次与最近一次 `take_request` 匹配时生效，避免已取消/超时
    /// 弹窗的迟到点击误把裁决套用到后续新连接上。
    pub fn resolve(&mut self, verdict: ApprovalVerdict) {
        let taken = self.taken_gen.take();
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard.in_flight && Some(guard.generation) == taken {
            guard.verdict = Some(verdict);
        }
    }

    /// 查询是否有待处理的撤销信号（client 在等待期断开或审批超时）；
    /// 返回 `true` 一次后自动复位为 `false`。
    pub fn take_cancel(&mut self) -> bool {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::replace(&mut guard.cancelled, false)
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use rc_session::auth::{ApprovalRequest, ApprovalVerdict, ConnectionApprover};
    use rc_session::proto::{AuthMethod, SessionParams};

    use super::*;

    fn sample_request() -> ApprovalRequest {
        ApprovalRequest {
            session_id: 42,
            remote_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 51234),
            requested: SessionParams {
                width: 1920,
                height: 1080,
                fps: 60,
                bitrate_bps: 8_000_000,
            },
            auth_method: AuthMethod::None,
            password_verified: false,
        }
    }

    #[test]
    fn bridge_request_before_submit_is_none() {
        let bridge = ApprovalBridge::new();
        let mut ui = bridge.ui_side();
        let mut session = bridge.session_side();

        assert!(ui.take_request().is_none());
        assert!(!ui.take_cancel());
        assert!(session.poll_verdict().is_none());
    }

    #[test]
    fn approval_bridge_roundtrip() {
        let bridge = ApprovalBridge::new();
        let mut ui = bridge.ui_side();
        let mut session = bridge.session_side();

        let req = sample_request();
        session.submit(&req);

        // 未决时 poll_verdict 必须返回 None。
        assert!(session.poll_verdict().is_none());

        // UI 侧取走同值请求，且再次取走为空。
        let got = ui.take_request().expect("submit 后应能取到请求");
        assert_eq!(got, req);
        assert!(ui.take_request().is_none());

        // 裁决写入前仍为 None。
        assert!(session.poll_verdict().is_none());

        // UI 侧提交裁决后，会话侧取回终局，且再次轮询为 None。
        ui.resolve(ApprovalVerdict::Accepted);
        assert_eq!(session.poll_verdict(), Some(ApprovalVerdict::Accepted));
        assert!(session.poll_verdict().is_none());
    }

    #[test]
    fn approval_bridge_cancel_idempotent() {
        let bridge = ApprovalBridge::new();
        let mut ui = bridge.ui_side();
        let mut session = bridge.session_side();

        // 无在途请求时 cancel 为幂等 no-op。
        session.cancel();
        assert!(!ui.take_cancel());

        // 提交在途请求后连调两次 cancel：不 panic、UI 侧只感知一次。
        let req = sample_request();
        session.submit(&req);
        session.cancel();
        session.cancel();

        assert!(ui.take_request().is_none());
        assert!(ui.take_cancel(), "首次 take_cancel 应为 true");
        assert!(!ui.take_cancel(), "复位后再次 take_cancel 应为 false");
    }

    #[test]
    fn stale_resolve_does_not_apply_to_subsequent_request() {
        let bridge = ApprovalBridge::new();
        let mut ui = bridge.ui_side();
        let mut session = bridge.session_side();

        // Client 1 提交并被 UI 取走（模拟模态弹窗正在展示）。
        let mut req1 = sample_request();
        req1.session_id = 1;
        session.submit(&req1);
        assert_eq!(ui.take_request(), Some(req1));

        // 弹窗阻塞期间 Client 1 超时/断开触发 cancel，紧接着 Client 2 发起连接。
        session.cancel();
        let mut req2 = sample_request();
        req2.session_id = 2;
        session.submit(&req2);

        // 用户在 Client 1 的过期弹窗上点了「接受」：代次不匹配，必须被忽略，不能误判给 Client 2。
        ui.resolve(ApprovalVerdict::Accepted);
        assert!(
            session.poll_verdict().is_none(),
            "过期弹窗的 resolve 不得应用到新连接的审批上"
        );

        // 下一轮 UI 循环取走 Client 2 的请求并回写「拒绝」，应准确生效。
        assert_eq!(ui.take_request(), Some(req2));
        ui.resolve(ApprovalVerdict::Rejected);
        assert_eq!(session.poll_verdict(), Some(ApprovalVerdict::Rejected));
    }

    #[test]
    fn resolve_without_take_request_is_ignored() {
        let bridge = ApprovalBridge::new();
        let mut ui = bridge.ui_side();
        let mut session = bridge.session_side();

        let req = sample_request();
        session.submit(&req);

        // 未调用 take_request 时直接 resolve 必须被忽略。
        ui.resolve(ApprovalVerdict::Accepted);
        assert!(session.poll_verdict().is_none());
        assert_eq!(ui.take_request(), Some(req));
    }
}
