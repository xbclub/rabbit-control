//! RabbitControl UI —— Tauri v2 应用入口（T21 M2-6 控制端界面）。
//!
//! 唯一依据：`docs/reference/design/08-ui-design.md` 与 `docs/reference/tickets/T21-controller-ui.md`。
//! - `window`：多窗口按需创建（W1–W4 / S1）
//! - `state`：当前唯一活动会话与 §6.0 mock 设备表
//! - `commands`：全部 Tauri IPC 命令（连接校验、会话启停、输入桥接、stage 尺寸同步、剪贴板）
//! - 后台轮询线程：非阻塞排空 `SessionHandle::try_recv_update()`，向前端推送
//!   `session://update` 事件，并在主线程完成 `CAMetalLayer` 挂载与摘除（T21 坑 2）。

// Windows 上以 GUI 子系统启动，避免附带控制台窗口。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![deny(unsafe_code)]
#![deny(missing_docs)]

mod commands;
mod state;
mod window;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::rngs::OsRng;
use rand::RngCore;
use rc_tools::{FileEvent, PortMapRuleSnapshot, TerminalEvent};
use rc_ui::app_prefs;
use rc_ui::display_ctrl;
use rc_ui::host_bridge::HostUpdate;
use rc_ui::lan_discovery::{self, LanDiscoveryHandle};
use rc_ui::metal_attach::{self, DockMenuAction, Frame};
use rc_ui::relay_transport::{RelayRole, TcpRelayTransport};
use rc_ui::server_config;
use rc_ui::session_bridge::{LayerLease, SessionUpdate};
use rc_ui::signal_client::RelayIncoming;
use tauri::{AppHandle, Emitter, Manager, RunEvent, WindowEvent};

use crate::state::{AppGateState, AppState};

/// 后台状态更新轮询粒度（低开销且保证 `Connecting`/`Negotiated`/`Ended` 与审批弹窗实时送达；
/// HUD 本身在会话线程内已做 1s 节流）。
const POLLER_INTERVAL: Duration = Duration::from_millis(50);

/// 本机未开启远程协助时，并发快速拒绝入站中继请求的最大在途线程数（H10）。
const MAX_INFLIGHT_RELAY_REJECTS: usize = 4;

/// 快速拒绝入站中继请求的拨号超时（H10）。
const RELAY_REJECT_TIMEOUT: Duration = Duration::from_secs(3);

/// 当前在途的入站中继快速拒绝线程计数。
static INFLIGHT_RELAY_REJECTS: AtomicUsize = AtomicUsize::new(0);

/// 当本机关闭远程协助（`host.handle == None`）收到入站中继通知时，快速拨号中继并立即关闭，
/// 使中继完成配对后立刻向远端控制端发送 EOF，避免对端空等 10–45 秒（H10）。
fn reject_relay_incoming_fast(
    incoming: RelayIncoming,
    mtls_cfg: Option<rc_ui::relay_transport::RelayMtlsConfig>,
) {
    let mut current = INFLIGHT_RELAY_REJECTS.load(Ordering::Acquire);
    loop {
        if current >= MAX_INFLIGHT_RELAY_REJECTS {
            return;
        }
        match INFLIGHT_RELAY_REJECTS.compare_exchange_weak(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(actual) => current = actual,
        }
    }

    struct RejectCounterGuard;
    impl Drop for RejectCounterGuard {
        fn drop(&mut self) {
            INFLIGHT_RELAY_REJECTS.fetch_sub(1, Ordering::AcqRel);
        }
    }

    let spawned = thread::Builder::new()
        .name("rc-relay-reject".to_string())
        .spawn(move || {
            let _guard = RejectCounterGuard;
            let cancel = Arc::new(AtomicBool::new(false));
            let transport = match mtls_cfg.as_ref() {
                Some(cfg) => TcpRelayTransport::connect_mtls_cancellable(
                    &incoming.relay_addr,
                    RelayRole::Host,
                    &incoming.relay_token,
                    RELAY_REJECT_TIMEOUT,
                    cancel,
                    cfg,
                ),
                None => TcpRelayTransport::connect_cancellable(
                    &incoming.relay_addr,
                    RelayRole::Host,
                    &incoming.relay_token,
                    RELAY_REJECT_TIMEOUT,
                    cancel,
                ),
            };
            drop(transport);
        });
    if spawned.is_err() {
        INFLIGHT_RELAY_REJECTS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 启动后台状态轮询线程：
/// 1) 排空 `SessionHandle` 更新队列 → 缓存快照（M8）→ emit `"session://update"`，
///    并协调主线程上的 `CAMetalLayer` 挂载 / 摘除；
/// 2) 排空 `HostHandle` 更新队列 → 折算 `HostRuntime` 状态（含断开后刷新临时验证码）
///    → emit `"host://update"`；
/// 3) 排空信令状态与入站中继分发（局域网扫描仅保留作后端连接路由优化，不再向前端推送 `lan://devices`）。
fn start_background_poller(app: AppHandle) {
    let _ = thread::Builder::new()
        .name("rc-ui-poller".to_string())
        .spawn(move || {
            loop {
                thread::sleep(POLLER_INTERVAL);

                // ── 1) 控制端会话更新 + 外围工具事件 + Metal 挂载/摘除 ──
                let mut polled_sid: Option<u64> = None;
                let mut polled_saw_ended = false;
                let mut updates: Vec<SessionUpdate> = Vec::new();
                let mut file_events: Vec<FileEvent> = Vec::new();
                let mut terminal_events: Vec<TerminalEvent> = Vec::new();
                let mut portmap_update: Option<Vec<PortMapRuleSnapshot>> = None;
                let mut attach_req: Option<(u64, LayerLease, Frame)> = None;
                let mut sync_layer_req: Option<(u64, LayerLease, Frame)> = None;
                let mut auto_fit_req: Option<(u32, u32)> = None;
                let mut emit_active_state = false;
                let mut negotiated_record: Option<(String, String)> = None;
                let mut restore_portmaps_req: Option<(u64, String)> = None;
                let mut wallpaper_updates: Vec<(String, String, String)> = Vec::new();
                let mut ended_session = None;

                {
                    let st = app.state::<AppState>();
                    let mut slot = st.session.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(active) = slot.as_mut() {
                        let sid = active.id;
                        polled_sid = Some(sid);
                        let mut saw_ended = false;
                        while let Some(mut upd) = active.handle.try_recv_update() {
                            match upd {
                                SessionUpdate::Connecting {
                                    ref mut watch_only, ..
                                } => {
                                    *watch_only = active.watch_only;
                                }
                                SessionUpdate::Negotiated {
                                    width,
                                    height,
                                    ref transport,
                                    mtls_active,
                                    ref peer_cert_fingerprint,
                                    ref mut watch_only,
                                    ..
                                } => {
                                    *watch_only = active.watch_only;
                                    active.set_video_size(width, height);
                                    auto_fit_req = Some((width, height));
                                    active.meta.connected = true;
                                    active.meta.transport = Some(transport.clone());
                                    active.meta.mtls_active = mtls_active;
                                    active.meta.peer_cert_fingerprint =
                                        peer_cert_fingerprint.clone();
                                    emit_active_state = true;
                                    if !active.meta.device_key.is_empty() {
                                        negotiated_record = Some((
                                            active.meta.device_key.clone(),
                                            active.meta.device_name.clone(),
                                        ));
                                    }
                                    if !active.watch_only && !active.portmaps_restored {
                                        active.portmaps_restored = true;
                                        let restore_key = if !active.meta.device_key.trim().is_empty()
                                        {
                                            active.meta.device_key.trim().to_string()
                                        } else {
                                            active.meta.target.trim().to_string()
                                        };
                                        if !restore_key.is_empty() {
                                            restore_portmaps_req = Some((sid, restore_key));
                                        }
                                    }
                                    let fallback = Frame {
                                        x: 0.0,
                                        y: 0.0,
                                        width: f64::from(width),
                                        height: f64::from(height),
                                    };
                                    let eff = active.effective_layer_frame(fallback);
                                    let eff_w = eff.width.round() as u32;
                                    let eff_h = eff.height.round() as u32;
                                    if eff_w > 0 && eff_h > 0 {
                                        active.handle.send(
                                            rc_ui::session_bridge::SessionCommand::StageRect {
                                                width: eff_w,
                                                height: eff_h,
                                            },
                                        );
                                    }
                                    if let Some(lease) = active.handle.layer_lease() {
                                        if let Some(req) = active.begin_attach(lease, fallback) {
                                            attach_req = Some(req);
                                        }
                                    }
                                }
                                SessionUpdate::DisplayConfig { width, height, .. }
                                    if width > 0 && height > 0 =>
                                {
                                    let prev_size = active.video_size;
                                    active.set_video_size(width, height);
                                    if let Some((pw, ph)) = prev_size {
                                        if pw > 0 && ph > 0 {
                                            let prev_ratio = f64::from(pw) / f64::from(ph);
                                            let next_ratio = f64::from(width) / f64::from(height);
                                            if (prev_ratio - next_ratio).abs() > 0.01 {
                                                auto_fit_req = Some((width, height));
                                            }
                                        }
                                    }
                                    if let Some(stage_frame) = active.last_stage_frame {
                                        let eff = active.effective_layer_frame(stage_frame);
                                        let eff_w = eff.width.round() as u32;
                                        let eff_h = eff.height.round() as u32;
                                        if eff_w > 0 && eff_h > 0 {
                                            active.handle.send(
                                                rc_ui::session_bridge::SessionCommand::StageRect {
                                                    width: eff_w,
                                                    height: eff_h,
                                                },
                                            );
                                        }
                                        if let Some(lease) = active.layer_state.attached_lease() {
                                            sync_layer_req = Some((sid, lease, eff));
                                        }
                                    }
                                }
                                SessionUpdate::Wallpaper { data_url } => {
                                    if !active.meta.device_key.is_empty() {
                                        wallpaper_updates.push((
                                            active.meta.device_key.clone(),
                                            active.meta.device_name.clone(),
                                            data_url,
                                        ));
                                    }
                                    // 不并入 `"session://update"`，亦不覆写 `last_session_update`。
                                    continue;
                                }
                                _ => {}
                            }
                            if matches!(upd, SessionUpdate::Ended { .. }) {
                                saw_ended = true;
                                emit_active_state = true;
                            }
                            updates.push(upd);
                        }
                        while let Some(ev) = active.handle.try_recv_file_event() {
                            file_events.push(ev);
                        }
                        while let Some(ev) = active.handle.try_recv_terminal_event() {
                            terminal_events.push(ev);
                        }
                        portmap_update = active.handle.take_portmap_update();
                        polled_saw_ended = saw_ended;

                        if saw_ended {
                            // U-H2 / U-H3：会话结束时在 `session` 锁内原子摘除并清空快照，
                            // 防止新窗口拉到过期 `Ended`，同时通知 W3 终端退出与 W4 清空映射列表。
                            for term_id in active.handle.tools().active_terminal_ids() {
                                let already_exited = terminal_events.iter().any(|ev| {
                                    matches!(ev, TerminalEvent::Exited { term_id: id, .. } if *id == term_id)
                                });
                                if !already_exited {
                                    terminal_events
                                        .push(TerminalEvent::Exited { term_id, code: -1 });
                                }
                            }
                            ended_session = slot.take();
                            state::clear_session_snapshot_locked(&st.last_session_update);
                            portmap_update = Some(Vec::new());
                        } else if let Some(latest) = updates
                            .iter()
                            .rev()
                            .find(|u| !matches!(u, SessionUpdate::DisplayConfig { .. }))
                        {
                            // Review M8 + U-H3：在 `session` 锁内绑定 `sid` 缓存最新快照，跳过纯控制元数据帧。
                            state::record_session_update_locked(
                                &st.last_session_update,
                                sid,
                                latest.clone(),
                            );
                        }
                    }
                }

                // 若同批次内会话尚未结束：
                // 1) 首次 Negotiated 自动将 S1 窗口适配到远端画面宽高比（消除左右/上下黑条，仍保留自由缩放能力）；
                // 2) 硬件 layer 就绪时在主线程挂载到 S1 窗口 NSView；
                // 3) 分辨率切换后若已挂载，同步更新 `CAMetalLayer` 等比视口。
                if ended_session.is_none() {
                    if let Some((pm_sid, pm_key)) = restore_portmaps_req {
                        let app_for_pm = app.clone();
                        let _ = thread::Builder::new()
                            .name("rc-portmap-restore".to_string())
                            .spawn(move || {
                                let st = app_for_pm.state::<AppState>();
                                commands::restore_saved_portmaps_for_active_session(
                                    &st, pm_sid, &pm_key,
                                );
                            });
                    }
                    if let Some((fw, fh)) = auto_fit_req {
                        let app_for_fit = app.clone();
                        let _ = app.run_on_main_thread(move || {
                            let _ = window::fit_session_window_to_video(&app_for_fit, fw, fh, false);
                        });
                    }
                    if let Some((sid, lease, frame)) = attach_req {
                        attach_layer_on_main_thread(&app, sid, lease, frame);
                    } else if let Some((sid, lease, eff_frame)) = sync_layer_req {
                        let app_for_sync = app.clone();
                        let _ = app.run_on_main_thread(move || {
                            let st = app_for_sync.state::<AppState>();
                            if state::should_sync_layer_on_main_thread(&st.session, sid, &lease) {
                                let _ =
                                    rc_ui::metal_attach::sync_leased_layer_frame(&lease, eff_frame);
                            }
                            drop(lease);
                        });
                    }
                }

                // U-H3 + H11：发射 `"session://update"` 与 W2/W3/W4 工具事件前复核 `sid` 未被替换，
                // 且若槽位已被 `disconnect_active_session` 清空则屏蔽陈旧非终态批次。
                if let Some(sid) = polled_sid {
                    let st = app.state::<AppState>();
                    let (should_emit, latest_active_state) = state::resolve_session_update_emit(
                        &st.session,
                        sid,
                        polled_saw_ended,
                        emit_active_state,
                    );
                    if should_emit {
                        for upd in updates {
                            let _ = app.emit("session://update", &upd);
                        }
                        for ev in file_events {
                            let _ = app.emit("file://event", &ev);
                        }
                        for ev in terminal_events {
                            let _ = app.emit("terminal://event", &ev);
                        }
                        if let Some(rules) = portmap_update {
                            let _ = app.emit("portmap://update", &rules);
                        }
                        if let Some(info) = latest_active_state {
                            let _ = app.emit("session://active-state", &info);
                        }
                    }
                }

                if negotiated_record.is_some() || !wallpaper_updates.is_empty() {
                    let st = app.state::<AppState>();
                    let wallpapers_dir = app_prefs::default_wallpapers_dir();
                    let _io = st.prefs_io_lock.lock().unwrap_or_else(|e| e.into_inner());
                    let now_ts = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();

                    let mut saved_wallpapers: Vec<(String, String, String)> = Vec::new();
                    for (key, name, data_url) in wallpaper_updates {
                        let b64 = data_url
                            .split_once("base64,")
                            .map(|(_, rest)| rest)
                            .unwrap_or(data_url.as_str());
                        if let Some(bytes) = display_ctrl::base64_decode(b64) {
                            if let Ok(filename) =
                                app_prefs::save_device_wallpaper(&wallpapers_dir, &key, &bytes)
                            {
                                saved_wallpapers.push((key, name, filename));
                            }
                        }
                    }

                    let (next_prefs, views) = {
                        let mut prefs = st.prefs.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some((key, name)) = negotiated_record {
                            let rec = prefs.device_snapshots.entry(key).or_default();
                            if !name.is_empty() {
                                rec.name = name;
                            }
                            rec.last_connected_ts = now_ts;
                        }
                        for (key, name, filename) in saved_wallpapers {
                            let rec = prefs.device_snapshots.entry(key).or_default();
                            if !name.is_empty() {
                                rec.name = name;
                            }
                            rec.last_connected_ts = rec.last_connected_ts.max(now_ts);
                            rec.wallpaper_file = Some(filename);
                        }
                        let cloned = prefs.clone();
                        let views = cloned.snapshot_views(&wallpapers_dir);
                        (cloned, views)
                    };
                    let _ = app_prefs::save_to_path(&app_prefs::default_prefs_path(), &next_prefs);
                    let _ = app.emit("device://snapshots", &views);
                }

                if let Some(old) = ended_session {
                    commands::teardown_session(&app, old);
                }

                // ── 2) 本机被控端状态更新 ──
                let mut host_updates: Vec<HostUpdate> = Vec::new();
                let mut stopped_host_handle = None;

                {
                    let st = app.state::<AppState>();
                    let mut host = st.host.lock().unwrap_or_else(|e| e.into_inner());
                    let mut host_stopped = false;
                    while let Some(upd) = host.handle.as_ref().and_then(|h| h.try_recv_update()) {
                        if host.apply_update(&upd) {
                            host_stopped = true;
                        }
                        host_updates.push(upd);
                    }
                    if host_stopped {
                        // M8：收到 HostUpdate::Stopped 时无条件回收句柄（工作线程已先清除 running 再推送 Stopped）。
                        stopped_host_handle = host.handle.take();
                        // U-M6：在持有 `st.host` 锁期间按 `host -> lan` 锁序同步撤回广播。
                        commands::sync_lan_advertising_locked(&st, &host);
                    }
                }

                drop(stopped_host_handle);

                for upd in host_updates {
                    let _ = app.emit("host://update", &upd);
                }

                // ── 3) 信令服务器状态更新与入站中继会话分发（H2 + H8 + H10） ──
                let signal_client = {
                    let st = app.state::<AppState>();
                    let rt = st.server.lock().unwrap_or_else(|e| e.into_inner());
                    rt.signal_client.as_ref().cloned()
                };
                if let Some(client) = signal_client {
                    let st = app.state::<AppState>();
                    while let Some(snap) = client.try_recv_status() {
                        let (accepted, persist_target) = {
                            let mut rt = st.server.lock().unwrap_or_else(|e| e.into_inner());
                            let is_current = rt
                                .signal_client
                                .as_ref()
                                .is_some_and(|cur| Arc::ptr_eq(cur, &client));
                            if !is_current {
                                break;
                            }
                            if !rt.apply_status_snapshot(&snap) {
                                (false, None)
                            } else {
                                let persist = snap.device_id.as_ref().and_then(|new_id| {
                                    if rt.config.cached_device_id.as_deref()
                                        != Some(new_id.as_str())
                                    {
                                        rt.config.cached_device_id = Some(new_id.clone());
                                        Some((rt.config_epoch, rt.config.clone()))
                                    } else {
                                        None
                                    }
                                });
                                (true, persist)
                            }
                        };

                        if !accepted {
                            continue;
                        }

                        let updated_device_id = persist_target.is_some();
                        if let Some((epoch, cfg_to_save)) = persist_target {
                            let _io_guard =
                                st.server_io_lock.lock().unwrap_or_else(|e| e.into_inner());
                            let still_same_epoch = st
                                .server
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .config_epoch
                                == epoch;
                            if still_same_epoch {
                                let _ = server_config::save_to_path(
                                    &server_config::default_config_path(),
                                    &cfg_to_save,
                                );
                                st.auth.update_server_config(cfg_to_save);
                            }
                        }

                        if let Some(lan) = st
                            .lan
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .as_ref()
                        {
                            lan.set_local_device_id(snap.device_id.clone());
                        }
                        if updated_device_id {
                            if let Some(new_did) = snap.device_id.clone() {
                                let cert_matches = st
                                    .auth
                                    .cert_manager()
                                    .load_device_cert()
                                    .is_some_and(|(_, m)| m.device_id == new_did);
                                if !cert_matches && st.gate_state() == AppGateState::Authenticated {
                                    let _ = st.auth.cert_manager().clear_device_cert();
                                    let _ = app.emit("gate://status", &st.gate_status_snapshot());
                                    let app_cert = app.clone();
                                    let _ = thread::Builder::new()
                                        .name("rc-cert-sync-did".to_string())
                                        .spawn(move || {
                                            let s = app_cert.state::<AppState>();
                                            if s.auth.ensure_device_cert(&new_did).is_ok() {
                                                commands::refresh_host_security_runtime(&s);
                                            }
                                            let _ = app_cert.emit(
                                                "gate://status",
                                                &s.gate_status_snapshot(),
                                            );
                                        });
                                }
                            }
                            commands::refresh_host_security_runtime(&st);
                        }
                        let _ = app.emit("server://status", &snap);
                    }

                    while let Some(dev_ev) = client.try_recv_device_status() {
                        let is_current = {
                            let rt = st.server.lock().unwrap_or_else(|e| e.into_inner());
                            rt.signal_client
                                .as_ref()
                                .is_some_and(|cur| Arc::ptr_eq(cur, &client))
                        };
                        if !is_current {
                            break;
                        }
                        let _ = app.emit("device://status-changed", &dev_ev);
                    }

                    while let Some(raw_reason) = client.try_recv_kicked_reason() {
                        let Some(zh_reason) =
                            rc_ui::signal_client::classify_auth_revocation_kick(&raw_reason)
                        else {
                            continue;
                        };
                        let is_current = {
                            let rt = st.server.lock().unwrap_or_else(|e| e.into_inner());
                            rt.signal_client
                                .as_ref()
                                .is_some_and(|cur| Arc::ptr_eq(cur, &client))
                        };
                        if is_current {
                            let app_for_kick = app.clone();
                            let _ = thread::Builder::new()
                                .name("rc-auth-kicked".to_string())
                                .spawn(move || {
                                    commands::force_logout_from_kick(&app_for_kick, zh_reason);
                                });
                            break;
                        }
                    }

                    while let Some(incoming) = client.try_recv_incoming() {
                        let is_current = {
                            let rt = st.server.lock().unwrap_or_else(|e| e.into_inner());
                            rt.signal_client
                                .as_ref()
                                .is_some_and(|cur| Arc::ptr_eq(cur, &client))
                        };
                        if !is_current {
                            break;
                        }
                        let rejected = {
                            let host = st.host.lock().unwrap_or_else(|e| e.into_inner());
                            if let Some(host_handle) = host.handle.as_ref() {
                                host_handle.offer_relay_incoming(incoming);
                                None
                            } else {
                                Some(incoming)
                            }
                        };
                        if let Some(incoming) = rejected {
                            let mtls_cfg = commands::build_relay_reject_mtls(&st);
                            reject_relay_incoming_fast(incoming, mtls_cfg);
                        }
                    }

                    // H2：绝不在轮询线程上 drop/join 最后一个旧 SignalClientHandle 引用。
                    if let Some(owned) = Arc::into_inner(client) {
                        let _ = thread::Builder::new()
                            .name("rc-signal-reaper".to_string())
                            .spawn(move || drop(owned));
                    }
                }
            }
        });
}

/// 在主线程将离屏 `CAMetalLayer` 挂载到 `"session"` 窗口的 `NSView`（T21 坑 2）。
///
/// 闭包持有 [`LayerLease`] 保活底层渲染器，并在挂载前后按会话代际号 `sid` 复核槽位：
/// - 挂载前会话已结束/替换：直接跳过；
/// - 挂载失败（窗口缺失/`ns_view` 失败/`attach_layer` 报错）：回滚状态机为 `None`；
/// - 挂载期间会话被并发替换/断开：立刻 `detach_layer` 避免残留子层。
#[cfg(target_os = "macos")]
fn attach_layer_on_main_thread(
    app: &AppHandle,
    sid: u64,
    lease: LayerLease,
    fallback_frame: Frame,
) {
    let app_for_attach = app.clone();
    let _ = app.run_on_main_thread(move || {
        let st = app_for_attach.state::<AppState>();
        let Some(effective_frame) =
            state::should_attach_on_main_thread(&st.session, sid, &lease, fallback_frame)
        else {
            return;
        };

        let attach_ok = match app_for_attach.get_webview_window("session") {
            Some(win) => {
                match rc_ui::metal_attach::attach_layer_to_window(&win, &lease, effective_frame) {
                    Ok(()) => true,
                    Err(e) => {
                        eprintln!("rc-ui: attach_layer_to_window 失败: {e}");
                        false
                    }
                }
            }
            None => false,
        };

        match state::complete_attach_on_main_thread(
            &st.session,
            sid,
            lease.clone(),
            effective_frame,
            attach_ok,
        ) {
            state::CompleteAttachOutcome::Attached {
                updated_frame: Some(latest),
            } => {
                let _ = rc_ui::metal_attach::sync_leased_layer_frame(&lease, latest);
            }
            state::CompleteAttachOutcome::Attached {
                updated_frame: None,
            }
            | state::CompleteAttachOutcome::Failed => {}
            state::CompleteAttachOutcome::MustDetachNow => {
                rc_ui::metal_attach::detach_leased_layer(&lease);
            }
        }
    });
}

/// 非 macOS 平台占位实现：回滚待挂载状态。
#[cfg(not(target_os = "macos"))]
fn attach_layer_on_main_thread(
    app: &AppHandle,
    sid: u64,
    lease: LayerLease,
    fallback_frame: Frame,
) {
    let st = app.state::<AppState>();
    let _ = state::complete_attach_on_main_thread(&st.session, sid, lease, fallback_frame, false);
}

/// 应用入口：构建 Tauri 应用、启动局域网发现与信令客户端、注册全部 IPC 命令，然后进入事件循环。
///
/// 本机被控端服务默认关闭，仅在用户手动开启「允许他人远程协助」并通过系统权限预检后启动。
fn main() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(AppState::new())
        .setup(|app| {
            let state = app.state::<AppState>();

            // 0) 按当前显示器尺寸自适应主窗口大小并居中（兼容 13"/14" 笔记本与 2K/4K 大屏）。
            window::adapt_main_window_on_startup(app.handle());

            // 1) 启动局域网多播发现后台线程（随机非零实例 ID + 本机设备名 + 已缓存 9 位设备码）。
            let mut instance_id = OsRng.next_u64();
            if instance_id == 0 {
                instance_id = 1;
            }
            let lan_handle =
                LanDiscoveryHandle::spawn(instance_id, lan_discovery::local_hostname());
            let initial_device_id = state
                .server
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .config
                .cached_device_id
                .clone();
            lan_handle.set_local_device_id(initial_device_id);
            *state.lan.lock().unwrap_or_else(|e| e.into_inner()) = Some(lan_handle);

            // 2) 安装 macOS Dock 右键设备与屏幕子菜单处理器（需在主线程调用）。
            let app_dock = app.handle().clone();
            let _ = app.handle().run_on_main_thread(move || {
                let app_cb = app_dock.clone();
                let _ =
                    metal_attach::install_dock_menu_handler(Arc::new(move |action| match action {
                        DockMenuAction::OpenMain => {
                            if let Some(win) = app_cb.get_webview_window("main") {
                                let _ = win.unminimize();
                                let _ = win.show();
                                let _ = win.set_focus();
                            }
                        }
                        DockMenuAction::ConnectDevice {
                            target,
                            name,
                            requires_password,
                        } => {
                            let st = app_cb.state::<AppState>();
                            if st.gate_state() != AppGateState::Authenticated {
                                if let Some(win) = app_cb.get_webview_window("main") {
                                    let _ = win.unminimize();
                                    let _ = win.show();
                                    let _ = win.set_focus();
                                }
                                return;
                            }
                            let info = st.active_session_info();
                            if info.active && info.target.as_deref() == Some(target.as_str()) {
                                let _ = window::focus_session_window(app_cb.clone());
                                return;
                            }
                            if let Some(win) = app_cb.get_webview_window("main") {
                                let _ = win.unminimize();
                                let _ = win.show();
                                let _ = win.set_focus();
                            }
                            let _ = app_cb.emit(
                                "dock://connect-device",
                                serde_json::json!({
                                    "target": target,
                                    "name": name,
                                    "requiresPassword": requires_password,
                                }),
                            );
                        }
                    }));
            });

            // 3) 后台恢复登录态（`auth.json` + Token 静默刷新 + 设备证书续签），仅在门禁放行后启动信令、被控端与轮换维护线程。
            let app_for_restore = app.handle().clone();
            let _ = thread::Builder::new()
                .name("rc-auth-restore".to_string())
                .spawn(move || {
                    let st = app_for_restore.state::<AppState>();
                    match st.auth.restore_on_startup() {
                        Ok(Some(sess)) if !sess.must_change_password => {
                            commands::post_auth_ready(&app_for_restore, &st);
                        }
                        Ok(_) => {
                            let _ =
                                app_for_restore.emit("gate://status", &st.gate_status_snapshot());
                        }
                        Err(_) => {
                            if st.gate_state() == AppGateState::Authenticated {
                                commands::post_auth_ready(&app_for_restore, &st);
                            } else {
                                let _ = app_for_restore
                                    .emit("gate://status", &st.gate_status_snapshot());
                            }
                        }
                    }
                });

            // 4) 启动后台轮询线程（会话更新 + 被控端更新 + 信令状态/中继分发）。
            start_background_poller(app.handle().clone());
            Ok(())
        })
        .on_window_event(|win, event| {
            if matches!(event, WindowEvent::Destroyed) {
                match win.label() {
                    "session" => commands::disconnect_active_session(win.app_handle()),
                    "terminal" => commands::close_active_terminals(win.app_handle()),
                    "main" => commands::shutdown_host_and_lan(win.app_handle()),
                    _ => {}
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            window::open_tool_window,
            window::open_session_window,
            window::fit_session_window,
            window::focus_session_window,
            window::close_session_window,
            commands::list_devices,
            commands::parse_connect_target,
            commands::open_session,
            commands::set_session_relay_mode,
            commands::set_session_watch_only,
            commands::set_session_mtls_mode,
            commands::session_command,
            commands::session_snapshot,
            commands::get_app_prefs,
            commands::save_stream_prefs,
            commands::get_device_snapshots,
            commands::get_active_session_info,
            commands::refresh_device_id,
            commands::get_gate_status,
            commands::probe_and_save_server,
            commands::fetch_auth_config,
            commands::auth_register,
            commands::auth_login,
            commands::auth_logout,
            commands::auth_change_password,
            commands::list_my_devices,
            commands::rename_my_device,
            commands::remove_my_device,
            commands::get_security_prefs,
            commands::save_security_prefs,
            commands::renew_device_cert,
            commands::send_input_event,
            commands::stage_rect,
            commands::validate_map_form,
            commands::copy_text,
            commands::check_permissions,
            commands::request_screen_recording_permission,
            commands::open_permission_settings,
            commands::get_host_status,
            commands::set_host_enabled,
            commands::set_host_password,
            commands::respond_approval,
            commands::disconnect_host_client,
            commands::list_lan_devices,
            commands::get_server_config,
            commands::save_server_config,
            commands::test_server_connection,
            commands::file_list_local,
            commands::file_mkdir_local,
            commands::file_list_remote,
            commands::file_mkdir_remote,
            commands::file_upload,
            commands::file_download,
            commands::file_cancel,
            commands::file_list_transfers,
            commands::file_clear_finished,
            commands::file_remove_transfer,
            commands::terminal_open,
            commands::terminal_input,
            commands::terminal_resize,
            commands::terminal_close,
            commands::portmap_list,
            commands::portmap_add,
            commands::portmap_toggle,
            commands::portmap_delete,
        ])
        .build(tauri::generate_context!())
        .expect("RabbitControl 主窗口启动失败");

    app.run(|app_handle, event| {
        if matches!(event, RunEvent::Exit) {
            commands::shutdown_host_and_lan(app_handle);
        }
    });
}
