//! 进程内 `HostSession` + `session_bridge` 集成测试（T21 测试清单）。
//!
//! 全部 CI 用例使用临时端口 `127.0.0.1:0`（绝不触碰 19901）、假后端与
//! [`RecordingInjector`]，免 macOS 权限且不拉起 Tauri 窗口。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rc_input::{InputEvent, MouseButton};
use rc_session::dev_cert::{DEV_CERT_PEM, DEV_KEY_PEM};
use rc_session::proto::SessionParams;
use rc_session::testkit::RecordingInjector;
use rc_session::{HostSession, HostSessionConfig, SessionClock};
use rc_types::Resolution;
use rc_ui::session_bridge::{
    ConnectPhase, SessionCommand, SessionHandle, SessionUpdate, UiSessionConfig,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

fn test_params() -> SessionParams {
    SessionParams {
        width: 64,
        height: 64,
        fps: 60,
        bitrate_bps: 8_000_000,
    }
}

fn test_pipeline_cfg() -> rc_pipeline::PipelineConfig {
    rc_pipeline::PipelineConfig {
        resolution: Resolution::new(64, 64),
        fps: 60,
        bitrate_bps: 8_000_000,
        prefer_hardware: false,
        target_latency_us: 22_000,
        trace_capacity: 4096,
        diff: rc_pipeline::DiffMode::TrustPlatform,
        clock_origin_us: None,
        max_packet_payload: rc_pipeline::MAX_PAYLOAD,
    }
}

type OriginSlot = Arc<Mutex<Option<Instant>>>;

fn origin_clock(origin: OriginSlot) -> SessionClock {
    Arc::new(move || match *origin.lock().expect("origin lock") {
        Some(o) => u32::try_from(o.elapsed().as_micros()).unwrap_or(u32::MAX),
        None => 0,
    })
}

struct HostRunResult {
    current_bitrate_bps: u32,
}

/// 在临时端口 `127.0.0.1:0` 上拉起进程内假后端 `HostSession`，返回监听地址与线程句柄。
fn spawn_inprocess_host(
    injector: RecordingInjector,
    max_duration: Duration,
) -> (SocketAddr, thread::JoinHandle<HostRunResult>) {
    let origin: OriginSlot = Arc::new(Mutex::new(None));
    let clock = origin_clock(Arc::clone(&origin));
    let mut host = HostSession::new(
        HostSessionConfig {
            listen_addr: "127.0.0.1:0".parse().expect("valid loopback addr"),
            mtu: rc_session::DEFAULT_SESSION_MTU,
            caps: test_params(),
            pipeline: test_pipeline_cfg(),
            handshake_timeout: Duration::from_secs(5),
            injector: Box::new(injector),
            pace_capture: true,
            auth: rc_session::HostAuthConfig::default(),
            approver: Box::new(rc_session::AutoApprover),
        },
        DEV_CERT_PEM,
        DEV_KEY_PEM,
        clock,
    )
    .expect("construct in-process host session");

    let factory_origin = Arc::clone(&origin);
    host.set_pipeline_factory_for(Box::new(move |pcfg| {
        *factory_origin.lock().expect("origin lock") = Some(Instant::now());
        let capture = rc_capture::FakeBackend::new(rc_capture::FakeBackendConfig {
            display: rc_capture::DisplayInfo {
                id: rc_types::DisplayId(0),
                resolution: pcfg.resolution,
                scale: 1.0,
                hdr: false,
            },
            fps: pcfg.fps,
            content: rc_capture::FakeContent::Dynamic,
            dirty_rect: None,
        })?;
        let encoder = rc_encode::FakeEncoder::with_tuning(
            rc_encode::EncoderConfig {
                resolution: pcfg.resolution,
                fps: pcfg.fps,
                bitrate_bps: pcfg.bitrate_bps,
                ..rc_encode::EncoderConfig::default()
            },
            500,
            512,
        )?;
        rc_pipeline::HostPipeline::with_backends_paced(
            pcfg.clone(),
            Box::new(capture),
            Box::new(encoder),
            true,
        )
    }));

    let addr = host.local_addr().expect("host local_addr");
    let handle = thread::spawn(move || {
        host.accept_and_handshake(CONNECT_TIMEOUT)
            .expect("host accept_and_handshake");
        let started = Instant::now();
        let _summary = host
            .run(&mut || started.elapsed() >= max_duration)
            .expect("host run");
        HostRunResult {
            current_bitrate_bps: host.current_bitrate_bps(),
        }
    });
    (addr, handle)
}

fn ui_cfg(addr: SocketAddr, watch_only: bool) -> UiSessionConfig {
    UiSessionConfig {
        connect_addr: addr,
        password: None,
        watch_only,
        width: 64,
        height: 64,
        fps: 60,
        bitrate_bps: 8_000_000,
        prefer_hardware: false,
        clipboard_enabled: false,
        audio_enabled: false,
        force_relay: false,
        relay_grant: None,
        ..UiSessionConfig::default()
    }
}

/// 轮询拉取 `SessionUpdate`，直到 `pred` 返回 `true` 或超时。
fn collect_until(
    session: &SessionHandle,
    timeout: Duration,
    mut pred: impl FnMut(&[SessionUpdate]) -> bool,
) -> Vec<SessionUpdate> {
    let mut updates = Vec::new();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        while let Some(u) = session.try_recv_update() {
            updates.push(u);
        }
        if pred(&updates) {
            return updates;
        }
        thread::sleep(Duration::from_millis(10));
    }
    while let Some(u) = session.try_recv_update() {
        updates.push(u);
    }
    updates
}

#[test]
fn bridge_connects_to_inprocess_host() {
    let injector = RecordingInjector::new();
    let (host_addr, host_thread) = spawn_inprocess_host(injector, Duration::from_secs(10));

    let session = SessionHandle::spawn(ui_cfg(host_addr, false));

    // 等待收到 Connecting → Negotiated → 至少一条 fps > 0 的 Hud 快照。
    let updates = collect_until(&session, Duration::from_secs(6), |list| {
        list.iter()
            .any(|u| matches!(u, SessionUpdate::Hud(h) if h.fps > 0.0))
    });

    assert!(
        updates.iter().any(|u| matches!(
            u,
            SessionUpdate::Connecting {
                phase: ConnectPhase::EndpointCreating,
                watch_only: false,
            }
        )),
        "应先上报 EndpointCreating，实际更新：{updates:?}"
    );
    assert!(
        updates.iter().any(|u| matches!(
            u,
            SessionUpdate::Negotiated {
                width: 64,
                height: 64,
                fps: 60,
                bitrate_bps: 8_000_000,
                backend,
                watch_only: false,
                ..
            } if backend == "fake"
        )),
        "应收到 Welcome 裁定参数 Negotiated，实际更新：{updates:?}"
    );
    assert!(
        updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::Hud(h) if h.fps > 0.0)),
        "应收到至少一条 fps > 0 的 HUD 快照，实际更新：{updates:?}"
    );

    // 主动发送 Disconnect，应收到 Ended 且线程干净退出。
    session.send(SessionCommand::Disconnect);
    let end_updates = collect_until(&session, Duration::from_secs(5), |list| {
        list.iter()
            .any(|u| matches!(u, SessionUpdate::Ended { .. }))
    });
    assert!(
        end_updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::Ended { .. })),
        "Disconnect 后应收到 Ended，实际：{end_updates:?}"
    );

    session.join();
    let _ = host_thread.join().expect("host thread join");
}

#[test]
fn watch_mode_sends_no_input() {
    // 场景 A：watch_only = true → 持续 send_input，host 侧 RecordingInjector 收到 0 条事件（U9）。
    {
        let injector = RecordingInjector::new();
        let observer = injector.clone();
        let (host_addr, host_thread) = spawn_inprocess_host(injector, Duration::from_secs(10));
        let session = SessionHandle::spawn(ui_cfg(host_addr, true));

        let updates = collect_until(&session, Duration::from_secs(5), |list| {
            list.iter()
                .any(|u| matches!(u, SessionUpdate::Negotiated { .. }))
        });
        assert!(
            updates
                .iter()
                .any(|u| matches!(u, SessionUpdate::Negotiated { .. })),
            "watch_only 会话应正常握手"
        );

        for _ in 0..10 {
            session.send_input(InputEvent::MouseMove { x: 1000, y: 2000 });
            session.send_input(InputEvent::MouseButton {
                button: MouseButton::Left,
                pressed: true,
            });
            session.send_input(InputEvent::MouseButton {
                button: MouseButton::Left,
                pressed: false,
            });
            thread::sleep(Duration::from_millis(20));
        }

        session.send(SessionCommand::Disconnect);
        session.join();
        let _ = host_thread.join().expect("host thread join");

        assert_eq!(
            observer.injected_count(),
            0,
            "watch_only=true 时 host RecordingInjector 必须收到 0 事件"
        );
    }

    // 场景 B：watch_only = false → 同场景必须有事件注入（U9 双向钉死）。
    {
        let injector = RecordingInjector::new();
        let observer = injector.clone();
        let (host_addr, host_thread) = spawn_inprocess_host(injector, Duration::from_secs(10));
        let session = SessionHandle::spawn(ui_cfg(host_addr, false));

        let updates = collect_until(&session, Duration::from_secs(5), |list| {
            list.iter()
                .any(|u| matches!(u, SessionUpdate::Negotiated { .. }))
        });
        assert!(updates
            .iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. })));

        for _ in 0..5 {
            session.send_input(InputEvent::MouseMove { x: 1000, y: 2000 });
            session.send_input(InputEvent::MouseButton {
                button: MouseButton::Left,
                pressed: true,
            });
            thread::sleep(Duration::from_millis(30));
        }

        session.send(SessionCommand::Disconnect);
        session.join();
        let _ = host_thread.join().expect("host thread join");

        assert!(
            observer.injected_count() > 0,
            "watch_only=false 时 host RecordingInjector 必须收到输入事件"
        );
    }
}

#[test]
fn set_bitrate_command_flows_to_host() {
    let injector = RecordingInjector::new();
    let (host_addr, host_thread) = spawn_inprocess_host(injector, Duration::from_secs(10));
    let session = SessionHandle::spawn(ui_cfg(host_addr, false));

    let updates = collect_until(&session, Duration::from_secs(5), |list| {
        list.iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. }))
    });
    assert!(updates
        .iter()
        .any(|u| matches!(u, SessionUpdate::Negotiated { .. })));

    // 稍等初次带宽粗测在回环上结算完毕，再下发手动 SetBitrate(reason=2)。
    thread::sleep(Duration::from_millis(300));
    let target_bps = 3_500_000;
    session.send(SessionCommand::SetBitrate { bps: target_bps });
    thread::sleep(Duration::from_millis(250));

    session.send(SessionCommand::Disconnect);
    session.join();
    let host_result = host_thread.join().expect("host thread join");

    assert_eq!(
        host_result.current_bitrate_bps, target_bps,
        "host 侧当前生效码率应更新为 UI 下发的手动码率"
    );
}

#[test]
fn input_events_reach_injector_normalized() {
    let injector = RecordingInjector::new();
    let mut observer = injector.clone();
    let (host_addr, host_thread) = spawn_inprocess_host(injector, Duration::from_secs(10));
    let session = SessionHandle::spawn(ui_cfg(host_addr, false));

    let updates = collect_until(&session, Duration::from_secs(5), |list| {
        list.iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. }))
    });
    assert!(updates
        .iter()
        .any(|u| matches!(u, SessionUpdate::Negotiated { .. })));

    session.send_input(InputEvent::MouseMove {
        x: 32_768,
        y: 32_768,
    });
    session.send_input(InputEvent::MouseButton {
        button: MouseButton::Left,
        pressed: true,
    });
    session.send_input(InputEvent::MouseButton {
        button: MouseButton::Left,
        pressed: false,
    });
    session.send_input(InputEvent::MouseWheel {
        delta: 3,
        horizontal: false,
    });

    thread::sleep(Duration::from_millis(250));
    session.send(SessionCommand::Disconnect);
    session.join();
    let _ = host_thread.join().expect("host thread join");

    let events = observer.take_events();
    assert!(
        events.iter().any(|e| matches!(
            e,
            InputEvent::MouseMove {
                x: 32_768,
                y: 32_768
            }
        )),
        "应收到原样归一化坐标 MouseMove(32768, 32768)，实际：{events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            InputEvent::MouseButton {
                button: MouseButton::Left,
                pressed: true
            }
        )),
        "应收到 MouseButton(Left, pressed=true)，实际：{events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            InputEvent::MouseWheel {
                delta: 3,
                horizontal: false
            }
        )),
        "应收到 MouseWheel(delta=3, horizontal=false)，实际：{events:?}"
    );
}

#[test]
fn ended_session_commands_are_ignored() {
    let injector = RecordingInjector::new();
    let (host_addr, host_thread) = spawn_inprocess_host(injector, Duration::from_secs(10));
    let session = SessionHandle::spawn(ui_cfg(host_addr, false));

    let updates = collect_until(&session, Duration::from_secs(5), |list| {
        list.iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. }))
    });
    assert!(updates
        .iter()
        .any(|u| matches!(u, SessionUpdate::Negotiated { .. })));

    session.send(SessionCommand::Disconnect);
    let end_updates = collect_until(&session, Duration::from_secs(5), |list| {
        list.iter()
            .any(|u| matches!(u, SessionUpdate::Ended { .. }))
    });
    assert!(end_updates
        .iter()
        .any(|u| matches!(u, SessionUpdate::Ended { .. })));

    // 等待 running 标志置 false。
    let deadline = Instant::now() + Duration::from_secs(2);
    while session.is_running() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!session.is_running(), "Ended 后 is_running 必须为 false");

    // Ended 后重复发控制命令与输入必须静默幂等、不 panic、不阻塞（U10）。
    session.send(SessionCommand::SetBitrate { bps: 4_000_000 });
    session.send(SessionCommand::StageRect {
        width: 1280,
        height: 720,
    });
    session.send(SessionCommand::Disconnect);
    session.send_input(InputEvent::MouseMove { x: 100, y: 200 });
    assert!(!session.is_running());

    session.join();
    let _ = host_thread.join().expect("host thread join");
}

#[test]
fn live_display_reconfigure_updates_resolution_fps_and_keeps_streaming() {
    use rc_ui::host_bridge::{HostHandle, UiHostConfig};

    let mut host = HostHandle::spawn(UiHostConfig {
        listen_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        password: None,
        approval_timeout_us: 5_000_000,
        prefer_hardware: false,
        require_approval: false,
        auto_accept_with_password: true,
        auto_rotate_password: false,
        clipboard_enabled: false,
        audio_enabled: false,
        ..UiHostConfig::default()
    })
    .expect("spawn offline host handle");

    let mut cfg = ui_cfg(host.listen_addr(), false);
    cfg.width = 1920;
    cfg.height = 1080;
    cfg.fps = 60;
    cfg.bitrate_bps = 12_000_000;
    let session = SessionHandle::spawn(cfg);

    // 1) 握手入流后应立刻收到初始 DisplayConfig（含 144fps 上限与原生预设）及 fps > 0 的 HUD
    let initial = collect_until(&session, Duration::from_secs(6), |list| {
        let got_cfg = list
            .iter()
            .any(|u| matches!(u, SessionUpdate::DisplayConfig { .. }));
        let got_hud = list
            .iter()
            .any(|u| matches!(u, SessionUpdate::Hud(h) if h.fps > 0.0));
        got_cfg && got_hud
    });
    assert!(
        initial.iter().any(|u| matches!(
            u,
            SessionUpdate::DisplayConfig {
                width: 1920,
                height: 1080,
                fps: 60,
                native_width: 1920,
                native_height: 1080,
                max_fps: 144,
                presets,
            } if presets.iter().any(|p| p.is_native && p.width == 1920 && p.height == 1080)
                && presets.iter().any(|p| p.width == 1280 && p.height == 720)
        )),
        "入流后应收到 1920x1080@60 (max_fps=144) 初始 DisplayConfig，实际：{initial:?}"
    );

    // 2) 会话中热切换到 1280x720@120fps
    session.send(SessionCommand::SetStreamParams {
        width: 1280,
        height: 720,
        fps: 120,
    });

    // 3) 应收到更新后的 DisplayConfig(1280x720@120) 且后续 HUD 分辨率更新为 1280x720 并持续出帧
    let after_switch = collect_until(&session, Duration::from_secs(6), |list| {
        let got_new_cfg = list.iter().any(|u| {
            matches!(
                u,
                SessionUpdate::DisplayConfig {
                    width: 1280,
                    height: 720,
                    fps: 120,
                    ..
                }
            )
        });
        let got_new_hud = list.iter().any(|u| {
            matches!(
                u,
                SessionUpdate::Hud(h) if h.resolution == "1280x720" && h.fps > 0.0
            )
        });
        got_new_cfg && got_new_hud
    });
    assert!(
        after_switch.iter().any(|u| matches!(
            u,
            SessionUpdate::DisplayConfig {
                width: 1280,
                height: 720,
                fps: 120,
                native_width: 1920,
                native_height: 1080,
                max_fps: 144,
                ..
            }
        )),
        "热切换后应收到更新后的 DisplayConfig(1280x720@120)，实际：{after_switch:?}"
    );
    assert!(
        after_switch.iter().any(|u| matches!(
            u,
            SessionUpdate::Hud(h) if h.resolution == "1280x720" && h.fps > 0.0
        )),
        "热切换后 HUD 应更新为 1280x720 且持续产出新帧(fps > 0)，实际：{after_switch:?}"
    );

    session.send(SessionCommand::Disconnect);
    session.join();
    host.stop();
}

// ─────────────────────────────────────────────────────────────────
// 真窗口 / 真机手动验收测试（#[ignore]，人工或幕布环境跑）
// ─────────────────────────────────────────────────────────────────

/// U4 开工首项 spike：Tauri 窗口 + 离屏 `MetalRenderer` + `attach_layer` 嵌入、resize 跟随、detach 清理。
#[test]
#[ignore = "需要 macOS 图形桌面与 Tauri 主线程事件循环，手动运行验收"]
fn metal_layer_spike_embeds_and_resizes() {
    // 验收命令：cargo test -p rc-ui -- --ignored metal_layer_spike_embeds_and_resizes --nocapture
    // 验证：attach_layer 后层内可见动态画面，窗口拖拽 resize 时 sync_layer_frame + MetalRenderer::resize
    // 同步更新物理像素尺寸，detach_layer 后无残留子层。
}

/// S3「进入全屏幕」/ `F` 键往返验收。
#[test]
#[ignore = "需要 macOS 图形桌面与 Tauri 窗口全屏交互，手动运行验收"]
fn fullscreen_toggle_roundtrip() {
    // 验证：S3「进入全屏幕」或按 F 键切换窗口全屏态，stage rect 推送、layer 跟随满屏并可正常退出全屏。
}

/// 真机滚轮符号方向钉死：向上滚动一次 → host `RecordingInjector` 收到 `MouseWheel.delta > 0`。
#[test]
#[ignore = "需要 macOS 真机物理滚轮输入（受系统自然滚动设置影响），手动运行验收"]
fn wheel_direction_real_machine() {
    // 验收命令：cargo test -p rc-ui -- --ignored wheel_direction_real_machine --nocapture
    // 验证：WebView 捕获真实向上滚轮事件后经 input_bridge::wheel_lines 映射，host 收到 delta > 0。
}

/// 真机全链路验证：
/// 1) `HostHandle(prefer_hardware=true)` 与 `SessionHandle(1920x1080, prefer_hardware=true)` 握手后，
///    协商分辨率严格保持本机主显示器宽高比（如 14" MBP `1512x982`），无 SCK 黑边；
/// 2) 通过 `input_bridge::normalize_pointer_in_video` 发送多个 stage 坐标点（含左下 Dock 位），
///    实时 `CGEventTap` 观测到的系统鼠标位置与期望屏幕坐标误差 <= 2.0 pt。
#[cfg(target_os = "macos")]
#[test]
#[ignore = "需要 macOS 屏幕录制与辅助功能授权，经 ./scripts/macos-dev-run.sh test rc-ui --test session_flow -- --ignored real_hardware_aspect_and_pointer_accuracy --nocapture 运行"]
fn real_hardware_aspect_and_pointer_accuracy() {
    use rc_input::InputCapture as _;
    use rc_ui::host_bridge::{HostHandle, UiHostConfig};
    use rc_ui::input_bridge;

    let display_res = rc_capture_macos::main_display_logical_resolution()
        .expect("macOS 主显示器逻辑分辨率应可读");
    let mut capture =
        rc_input_macos::CgCapture::new().expect("需要辅助功能授权（CGEventTap 见证鼠标注入坐标）");
    let tap_bounds = capture.screen_bounds();

    let mut host = HostHandle::spawn(UiHostConfig {
        listen_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        password: None,
        approval_timeout_us: 10_000_000,
        prefer_hardware: true,
        require_approval: false,
        auto_accept_with_password: true,
        auto_rotate_password: false,
        clipboard_enabled: false,
        audio_enabled: false,
        ..UiHostConfig::default()
    })
    .expect("spawn hardware host");

    let mut cfg = ui_cfg(host.listen_addr(), false);
    cfg.width = 1920;
    cfg.height = 1080;
    cfg.fps = 144;
    cfg.prefer_hardware = true;
    let session = SessionHandle::spawn(cfg);

    let updates = collect_until(&session, Duration::from_secs(10), |list| {
        let got_neg = list
            .iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. }));
        let got_disp = list
            .iter()
            .any(|u| matches!(u, SessionUpdate::DisplayConfig { .. }));
        got_neg && got_disp
    });
    let (neg_w, neg_h, neg_fps, backend) = updates
        .iter()
        .find_map(|u| match u {
            SessionUpdate::Negotiated {
                width,
                height,
                fps,
                backend,
                ..
            } => Some((*width, *height, *fps, backend.clone())),
            _ => None,
        })
        .expect("应收到 Negotiated 更新");
    let init_presets = updates
        .iter()
        .find_map(|u| match u {
            SessionUpdate::DisplayConfig {
                max_fps, presets, ..
            } => Some((*max_fps, presets.clone())),
            _ => None,
        })
        .expect("应收到初始 DisplayConfig");
    println!(
        "real_hardware_aspect_and_pointer_accuracy: display={}x{}, negotiated={}x{}@{}fps, max_fps={}, presets={}, backend={backend}, tap_bounds={tap_bounds:?}",
        display_res.width,
        display_res.height,
        neg_w,
        neg_h,
        neg_fps,
        init_presets.0,
        init_presets.1.len()
    );
    assert_eq!(neg_fps, 144, "应协商至 144fps（消除原 60/70fps 上限）");
    assert_eq!(init_presets.0, 144);
    assert!(
        init_presets.1.len() >= 2,
        "应返回至少 2 个等比分辨率预设，实际 {:?}",
        init_presets.1
    );

    // 验证协商分辨率严格保持主显示器宽高比（相对误差 < 0.2%），且不超出 1920x1080 与主屏尺寸。
    let display_aspect = f64::from(display_res.width) / f64::from(display_res.height);
    let neg_aspect = f64::from(neg_w) / f64::from(neg_h);
    assert!(
        (display_aspect - neg_aspect).abs() / display_aspect < 0.002,
        "协商分辨率 {neg_w}x{neg_h} (aspect={neg_aspect:.4}) 偏离主屏 {}x{} (aspect={display_aspect:.4})",
        display_res.width,
        display_res.height
    );
    assert!(neg_w <= display_res.width && neg_h <= display_res.height);

    // 模拟默认 S1 舞台（1280x752 CSS px），测试 3 个代表点（含左下 Dock 区）通过真实 QUIC + InputBridge 注入后的系统光标坐标。
    let stage_w = 1280.0;
    let stage_h = 752.0;
    let (ox, oy, fw, fh) = input_bridge::fit_video_rect(stage_w, stage_h, neg_w, neg_h);
    let samples = [
        ("dock_left_chrome", 0.14_f64, 0.94_f64),
        ("center", 0.50_f64, 0.50_f64),
        ("top_right", 0.85_f64, 0.15_f64),
    ];

    // 清空启动残留事件
    let _ = capture.next_events(Duration::from_millis(150));

    for (label, rx, ry) in samples {
        let stage_x = ox + rx * fw;
        let stage_y = oy + ry * fh;
        let (nx, ny) = input_bridge::normalize_pointer_in_video(
            stage_x,
            stage_y,
            stage_w,
            stage_h,
            Some((neg_w, neg_h)),
        );
        // 期望落点（主显示器逻辑点坐标）
        let want_pt_x = (f64::from(nx) / 65535.0) * (f64::from(display_res.width) - 1.0);
        let want_pt_y = (f64::from(ny) / 65535.0) * (f64::from(display_res.height) - 1.0);

        session.send_input(InputEvent::MouseMove { x: nx, y: ny });

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut best_err = f64::INFINITY;
        let mut matched = false;
        while Instant::now() < deadline {
            let evs = capture
                .next_events(Duration::from_millis(50))
                .expect("tap next_events");
            for ev in evs {
                if let InputEvent::MouseMove { x: tx, y: ty } = ev {
                    let got_pt_x =
                        tap_bounds.x + (f64::from(tx) / 65535.0) * (tap_bounds.width - 1.0);
                    let got_pt_y =
                        tap_bounds.y + (f64::from(ty) / 65535.0) * (tap_bounds.height - 1.0);
                    let err = (got_pt_x - want_pt_x).hypot(got_pt_y - want_pt_y);
                    if err < best_err {
                        best_err = err;
                    }
                    if err <= 2.0 {
                        println!(
                            "  [{label}] stage=({stage_x:.1},{stage_y:.1}) -> norm=({nx},{ny}) -> want=({want_pt_x:.1},{want_pt_y:.1}) got=({got_pt_x:.1},{got_pt_y:.1}) err={err:.2}pt OK"
                        );
                        matched = true;
                        break;
                    }
                }
            }
            if matched {
                break;
            }
        }
        assert!(
            matched,
            "采样点 {label} 未命中期望坐标 ({want_pt_x:.1}, {want_pt_y:.1})，最小误差 {best_err:.2}pt"
        );
    }

    // 3) 真机硬件热切换分辨率与帧率：切至预设中的低一档分辨率 @ 120fps，再切回原生分辨率 @ 144fps。
    let alt = init_presets
        .1
        .iter()
        .find(|p| p.width != neg_w || p.height != neg_h)
        .expect("应存在可切换的非当前分辨率预设");
    session.send(SessionCommand::SetStreamParams {
        width: alt.width,
        height: alt.height,
        fps: 120,
    });
    let switched = collect_until(&session, Duration::from_secs(10), |list| {
        list.iter().any(|u| {
            matches!(
                u,
                SessionUpdate::DisplayConfig {
                    width,
                    height,
                    fps: 120,
                    ..
                } if *width == alt.width && *height == alt.height
            )
        })
    });
    assert!(
        switched.iter().any(|u| matches!(
            u,
            SessionUpdate::DisplayConfig {
                width,
                height,
                fps: 120,
                ..
            } if *width == alt.width && *height == alt.height
        )),
        "真机热切换至 {}x{}@120fps 应收到更新后的 DisplayConfig，实际：{switched:?}",
        alt.width,
        alt.height
    );
    println!(
        "  [live_switch] switched to {}x{}@120fps OK",
        alt.width, alt.height
    );

    // 4) 验证被控端异步发送真实桌面壁纸 JPEG（Data URL），且主显示器真实分辨率随后随连接断开自动恢复。
    let mut saw_wallpaper = updates
        .iter()
        .chain(switched.iter())
        .any(|u| matches!(u, SessionUpdate::Wallpaper { data_url } if data_url.starts_with("data:image/jpeg;base64,")));
    if !saw_wallpaper {
        let wp_updates = collect_until(&session, Duration::from_secs(5), |list| {
            list.iter().any(|u| {
                matches!(u, SessionUpdate::Wallpaper { data_url } if data_url.starts_with("data:image/jpeg;base64,"))
            })
        });
        saw_wallpaper = wp_updates.iter().any(|u| {
            matches!(u, SessionUpdate::Wallpaper { data_url } if data_url.starts_with("data:image/jpeg;base64,"))
        });
    }
    assert!(
        saw_wallpaper,
        "真机 HostHandle 应通过 DisplayCtrl 推送桌面壁纸 JPEG"
    );

    session.send(SessionCommand::Disconnect);
    session.join();
    host.stop();

    let restored_res = rc_capture_macos::main_display_logical_resolution()
        .expect("断开后应可读主显示器逻辑分辨率");
    assert_eq!(
        restored_res, display_res,
        "断开远控后主显示器真实分辨率应恢复为 {:?}，实际 {:?}",
        display_res, restored_res
    );
    println!(
        "  [restore] display restored to {}x{} OK",
        restored_res.width, restored_res.height
    );
}

#[test]
#[ignore = "需要屏幕录制权限且建议前景有 120Hz 动画窗口（通过 ./scripts/macos-dev-run.sh 运行）"]
fn real_hardware_sustained_120fps() {
    use rc_ui::host_bridge::{HostHandle, UiHostConfig};

    let mut host = HostHandle::spawn(UiHostConfig {
        listen_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        password: None,
        approval_timeout_us: 10_000_000,
        prefer_hardware: true,
        require_approval: false,
        auto_accept_with_password: true,
        auto_rotate_password: false,
        clipboard_enabled: false,
        audio_enabled: false,
        ..UiHostConfig::default()
    })
    .expect("spawn hardware host");

    let mut cfg = ui_cfg(host.listen_addr(), false);
    cfg.width = 1512;
    cfg.height = 982;
    cfg.fps = 120;
    cfg.prefer_hardware = true;
    let session = SessionHandle::spawn(cfg);

    let init = collect_until(&session, Duration::from_secs(10), |list| {
        list.iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. }))
    });
    let negotiated = init
        .iter()
        .find_map(|u| match u {
            SessionUpdate::Negotiated {
                width,
                height,
                fps,
                backend,
                ..
            } => Some((*width, *height, *fps, backend.clone())),
            _ => None,
        })
        .expect("应收到 Negotiated 更新");
    println!(
        "real_hardware_sustained_120fps: negotiated={}x{}@{}fps backend={}",
        negotiated.0, negotiated.1, negotiated.2, negotiated.3
    );

    let mut hud_samples: Vec<f64> = Vec::new();
    for u in init {
        if let SessionUpdate::Hud(h) = u {
            if h.elapsed != "00:00:00" && h.fps > 0.0 {
                println!(
                    "  [hud {}] {:.1} fps, {:.0} kbps, rtt={:?} ms, frame={:?} ms, loss={:.2}% ({})",
                    h.elapsed, h.fps, h.bitrate_kbps, h.rtt_ms, h.frame_ms, h.loss_pct, h.resolution
                );
                hud_samples.push(h.fps);
            }
        }
    }

    let deadline = Instant::now() + Duration::from_secs(25);
    while Instant::now() < deadline {
        while let Some(u) = session.try_recv_update() {
            match u {
                SessionUpdate::Hud(h) if h.elapsed != "00:00:00" => {
                    println!(
                        "  [hud {}] {:.1} fps, {:.0} kbps, rtt={:?} ms, frame={:?} ms, loss={:.2}% ({})",
                        h.elapsed,
                        h.fps,
                        h.bitrate_kbps,
                        h.rtt_ms,
                        h.frame_ms,
                        h.loss_pct,
                        h.resolution
                    );
                    hud_samples.push(h.fps);
                }
                SessionUpdate::Ended { reason } => {
                    panic!("25s 稳态压测中途会话意外结束：{reason}");
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    session.send(SessionCommand::Disconnect);
    session.join();
    host.stop();

    assert!(
        hud_samples.len() >= 18,
        "25s 内应至少采集到 18 个有效 1s HUD 样本，实际 {}：{hud_samples:?}",
        hud_samples.len()
    );
    let tail = &hud_samples[hud_samples.len() - 15..];
    let mut sorted = tail.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let median_last_15 = sorted[sorted.len() / 2];
    let min_last_15 = sorted[0];
    println!(
        "real_hardware_sustained_120fps summary: total_samples={}, last_15s_median={median_last_15:.1} fps, last_15s_min={min_last_15:.1} fps",
        hud_samples.len()
    );
    assert!(
        median_last_15 >= 100.0,
        "后 15 秒中位帧率应 >= 100.0 fps，实际 {median_last_15:.1} fps (samples={tail:?})"
    );
    assert!(
        min_last_15 >= 85.0,
        "后 15 秒最低帧率样本应 >= 85.0 fps，实际 {min_last_15:.1} fps (samples={tail:?})"
    );
}

#[test]
fn lock_remote_screen_live_and_on_disconnect_reaches_host() {
    use rc_ui::host_bridge::{HostHandle, UiHostConfig};

    let mut host = HostHandle::spawn(UiHostConfig {
        listen_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        password: None,
        approval_timeout_us: 10_000_000,
        prefer_hardware: false,
        require_approval: false,
        auto_accept_with_password: true,
        auto_rotate_password: false,
        clipboard_enabled: false,
        audio_enabled: false,
        ..UiHostConfig::default()
    })
    .expect("spawn fake host");

    let mut cfg = ui_cfg(host.listen_addr(), false);
    cfg.prefer_hardware = false;
    let session = SessionHandle::spawn(cfg);

    let init = collect_until(&session, Duration::from_secs(10), |list| {
        list.iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. }))
    });
    assert!(
        init.iter()
            .any(|u| matches!(u, SessionUpdate::Negotiated { .. })),
        "应成功协商连接，实际：{init:?}"
    );
    assert_eq!(host.lock_screen_count(), 0);

    // 1) 在线立即锁屏
    session.send(SessionCommand::LockRemoteScreen);
    let wait_live = Instant::now() + Duration::from_secs(3);
    while Instant::now() < wait_live && host.lock_screen_count() < 1 {
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        host.lock_screen_count(),
        1,
        "在线 LockRemoteScreen 应在 3s 内送达 Host"
    );

    // 2) 退出时自动锁屏（随 Disconnect 前刷新发出）
    session.request_lock_on_disconnect();
    session.send(SessionCommand::Disconnect);
    session.join();

    let wait_exit = Instant::now() + Duration::from_secs(3);
    while Instant::now() < wait_exit && host.lock_screen_count() < 2 {
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        host.lock_screen_count(),
        2,
        "request_lock_on_disconnect + Disconnect 应在 3s 内送达 Host"
    );

    host.stop();
}
