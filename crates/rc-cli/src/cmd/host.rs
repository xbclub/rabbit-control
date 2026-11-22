//! `rc-cli host`：被控端子命令（T18）——accept 一条 QUIC 连接，流媒体 +
//! 注入输入，stderr 摘要 + 可选报告落盘。
//!
//! 流向约定（S1，照抄 loopback）：进度/摘要只进 stderr；JSON 只进
//! `--report` 文件或 stdout。会话逻辑全在 rc-session（`HostSession`），
//! 本文件只做参数 → 会话配置 → 驱动 → 报告的接线（薄壳纪律）。
//! **不做 Gate 判定**（被控端没有 e2e 观测）；停机顺序红线由会话层保证
//! （S8），CLI 不自写停机。

use std::fs;
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rc_pipeline::{DiffMode, PipelineConfig};
use rc_session::proto::SessionParams;
use rc_session::testkit::RecordingInjector;
use rc_session::{HostSession, HostSessionConfig, SessionClock};

use super::{BackendMode, CliResult, HostArgs, InjectMode};

/// accept 等待上限（`--duration` 只计接受连接后的会话时长，坑 7）。
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(300);
/// fake 编码器的单帧编码成本（微秒）。
const FAKE_ENCODE_COST_US: u32 = 500;

/// 会话钟原点：管线工厂在构造后端的那一刻锚定（fake = Instant，与
/// FakeBackend 时间网格零点同刻；real = 主机钟 µs，与 SCK PTS 同域，
/// 照 run_real 的 session_origin_us 模式）。锚定前读数恒 0（时间冻结，
/// 调度器只会把帧判得更不超期，无害）。
#[derive(Debug, Clone)]
enum ClockOrigin {
    /// 尚未锚定（握手未完成）。
    Pending,
    /// fake 后端：锚 Instant（与假栈 WallCapture 帧时间戳**同一个**采样，
    /// 修复卡 C-4 单采样不变式——e2e 的 captured→presented 链路换算依赖
    /// 两轴同零点）。
    Fake(Instant),
    /// 真后端：锚 `host_clock_us()`（SCK PTS 同域）。
    HostClock(u64),
}

/// 执行 `host` 子命令：构造 Identity 与 HostSession（注入选定后端与
/// Injector）→ accept 一条连接 → stderr 打「等待连接」与后端标签 →
/// 驱动到 Bye 或 --duration 兜底 → stderr 摘要（in_flight 峰值、注入
/// 事件数）+ 可选报告落盘。正常收尾 SUCCESS；运行期错误经 CliResult
/// 上抛（main 打印后 exit 1）。**不做 Gate 判定。**
pub fn run(args: &HostArgs) -> CliResult<ExitCode> {
    // ── 证书（S3：默认内嵌开发证书，--cert/--key 可覆盖）──
    let cert_pem = match &args.cert {
        Some(p) => fs::read_to_string(p).map_err(|e| format!("无法读证书 {}: {e}", p.display()))?,
        None => rc_session::dev_cert::DEV_CERT_PEM.to_string(),
    };
    let key_pem = match &args.key {
        Some(p) => fs::read_to_string(p).map_err(|e| format!("无法读私钥 {}: {e}", p.display()))?,
        None => rc_session::dev_cert::DEV_KEY_PEM.to_string(),
    };
    // ── 开发证书非环回警告（修复卡D-3）：内嵌开发证书的私钥随源码
    //    公开，不构成安全边界；非环回监听还在用它，任何能路由到本机
    //    的人都能解密/冒充会话。打醒目警告但不阻止（0.0.0.0 快速联调
    //    是合法的开发场景）。──
    if let Some(warn) = dev_cert_warning(&args.listen, args.cert.as_deref(), args.key.as_deref()) {
        eprintln!("{warn}");
    }

    // ── 注入器（record 默认免权限；real 仅 macOS）──
    let injector: Box<dyn rc_input::InputInjector> = match args.inject_mode {
        InjectMode::Record => Box::new(RecordingInjector::new()),
        #[cfg(target_os = "macos")]
        InjectMode::Real => Box::new(
            rc_input_macos::InputBridge::new()
                .map_err(|e| format!("CGEvent 注入桥初始化失败（需辅助功能权限）: {e}"))?,
        ),
        #[cfg(not(target_os = "macos"))]
        InjectMode::Real => {
            return Err("--inject-mode real 仅 macOS 支持（其他平台用 record）".into())
        }
    };

    // ── 会话配置（管线模板：握手后以裁定值覆盖，实际后端由工厂选）──
    let pipeline = PipelineConfig {
        resolution: args.resolution,
        fps: args.fps,
        bitrate_bps: args.bitrate_bps,
        prefer_hardware: args.backend != BackendMode::Fake,
        target_latency_us: 22_000,
        trace_capacity: 4096,
        diff: DiffMode::TrustPlatform,
        clock_origin_us: None,
        // 模板默认预算；expect_hello 握手时按 N2 覆盖为 1120
        //（max_media_payload(DEFAULT_SESSION_MTU)）。
        max_packet_payload: rc_pipeline::MAX_PAYLOAD,
    };
    let cfg = HostSessionConfig {
        listen_addr: args.listen,
        mtu: rc_session::DEFAULT_SESSION_MTU,
        caps: SessionParams {
            width: args.resolution.width,
            height: args.resolution.height,
            fps: args.fps,
            bitrate_bps: args.bitrate_bps,
        },
        pipeline,
        handshake_timeout: Duration::from_secs(10),
        injector,
        pace_capture: true, // 仅 fake 工厂路径实际生效（真采集不节流）
    };

    // ── 会话钟（原点由管线工厂锚定，见 ClockOrigin 文档）──
    let origin: Arc<Mutex<ClockOrigin>> = Arc::new(Mutex::new(ClockOrigin::Pending));
    let clock: SessionClock = {
        let origin = Arc::clone(&origin);
        Arc::new(move || {
            let guard = origin.lock().expect("origin lock");
            match *guard {
                ClockOrigin::Pending => 0,
                ClockOrigin::Fake(i) => u32::try_from(i.elapsed().as_micros()).unwrap_or(u32::MAX),
                ClockOrigin::HostClock(o) => {
                    u32::try_from(rc_pipeline::host_clock_us().unwrap_or(o).saturating_sub(o))
                        .unwrap_or(u32::MAX)
                }
            }
        })
    };

    // ── 后端标签（工厂填充，stderr 与报告用）──
    let backend_label: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

    // ── 管线工厂：按裁定参数构造选定后端（fake 含 --static-content 栈）──
    let factory = {
        let origin = Arc::clone(&origin);
        let label = Arc::clone(&backend_label);
        let mode = args.backend;
        let static_content = args.static_content;
        Box::new(
            move |pcfg: &PipelineConfig| -> rc_types::Result<rc_pipeline::HostPipeline> {
                let mut hw_cfg = pcfg.clone();
                hw_cfg.prefer_hardware = true;
                match mode {
                    // fake：手动构造假栈（照 rc-loopback fake_stack 形态）。
                    // 单采样（修复卡 C-4）：先建栈拿到锚点，再锚定
                    // ClockOrigin::Fake——会话钟纪元与 WallCapture 帧时间戳
                    // 共用**同一个** Instant（两次独立 now() 的间隙会成为
                    // 两轴的固定偏差 Δ，capture_ts 系统性旧 Δ，rc-sched 的
                    // deadline 判定整体偏移）。
                    BackendMode::Fake => {
                        *label.lock().expect("label lock") = "fake".into();
                        let (p, anchor) = fake_stack_anchored(pcfg, static_content)?;
                        *origin.lock().expect("origin lock") = ClockOrigin::Fake(anchor);
                        Ok(p)
                    }
                    // real：严格真后端（SCK + VideoToolbox），任一降级即报错；
                    // auto：优先真后端，失败降级假栈（标签打 stderr，不静默）。
                    BackendMode::Real | BackendMode::Auto => match real_host_stack(&hw_cfg) {
                        Ok((p, l, origin_us)) => {
                            *label.lock().expect("label lock") = l.clone();
                            *origin.lock().expect("origin lock") =
                                ClockOrigin::HostClock(origin_us);
                            if mode == BackendMode::Auto {
                                eprintln!("rc-cli host: 后端 {l}（真后端优先）");
                            }
                            Ok(p)
                        }
                        Err(e) if mode == BackendMode::Auto => {
                            eprintln!("rc-cli host: 真后端不可用（{e}），降级假后端");
                            *label.lock().expect("label lock") = "fake(降级)".into();
                            // 同上：单采样锚点两轴共用。
                            let (p, anchor) = fake_stack_anchored(pcfg, static_content)?;
                            *origin.lock().expect("origin lock") = ClockOrigin::Fake(anchor);
                            Ok(p)
                        }
                        Err(e) => Err(e),
                    },
                }
            },
        )
    };

    // ── 组装会话并接线 ──
    let mut host = HostSession::new(cfg, &cert_pem, &key_pem, clock)
        .map_err(|e| format!("host 会话构造失败: {e}"))?;
    host.set_pipeline_factory_for(factory);

    let listen = host
        .local_addr()
        .map_err(|e| format!("无法获取监听地址: {e}"))?;
    eprintln!(
        "host 等待连接：listen={listen}（{}x{} @ {}fps，注入={}）",
        args.resolution.width,
        args.resolution.height,
        args.fps,
        inject_label(args.inject_mode),
    );

    // ── accept + 握手（等待期不消耗 --duration，坑 7）──
    host.accept_and_handshake(ACCEPT_TIMEOUT)
        .map_err(|e| format!("accept/握手失败: {e}"))?;
    let backend = backend_label.lock().expect("label lock").clone();
    let negotiated = host.negotiated().expect("握手完成即有裁定值");
    eprintln!(
        "host 已连接：后端={backend}，裁定 {}x{} @ {}fps @ {}bps，session_id={}",
        negotiated.width,
        negotiated.height,
        negotiated.fps,
        negotiated.bitrate_bps,
        host.session_id().unwrap_or(0),
    );

    // ── 驱动到 Bye 或 --duration 兜底（S8：停机顺序由会话层保证）──
    let connected = Instant::now();
    let duration = args.duration;
    let summary = host
        .run(&mut || connected.elapsed() >= duration)
        .map_err(|e| format!("host 会话运行失败: {e}"))?;
    let connected_ms = connected.elapsed().as_millis();

    // ── stderr 摘要（S5：in_flight 峰值 ≤ 2 铁律 1 的观测点；调度器
    //    帧去向四数是 2026-11-21 排查增补——0 帧编码时能立刻看出帧
    //    是没采到（captured=0）还是被调度器丢了（dropped_* 飙升）；
    //    探测块单列是发现 10 的 CLI 半边——数据报口径只计媒体）──
    eprintln!(
        "host 完成：{}（{} 数据报 / 探测块 {} / {} 帧编码 / {} 事件注入 / {} 关键批 / IdrRequest×{} / in_flight 峰值 {} / 未结帧 {} / 连接 {connected_ms}ms / 调度器：采集 {} → 编码 {}，弃帧 超期 {} + 重复 {} + 预算 {}）",
        match &summary.stop_reason {
            rc_session::StopReason::Requested => "到时收尾".to_string(),
            rc_session::StopReason::ByeReceived(_) => "对端 Bye 优雅收尾".to_string(),
            rc_session::StopReason::Disconnected(r) => format!("断线收尾（{r}）"),
        },
        summary.datagrams_sent,
        summary.probe_datagrams_sent,
        summary.frames_encoded,
        summary.events_injected,
        host.key_batches(),
        host.idr_requests_received(),
        host.in_flight_peak(),
        summary.unfinished_traces,
        summary.captured,
        summary.frames_encoded,
        summary.dropped_stale,
        summary.dropped_repeat,
        summary.dropped_budget,
    );

    // ── host 侧报告（无 gate 键；键名契约见任务卡 T18；调度器帧去向
    //    四键与 stderr 摘要同口径；probe_datagrams_sent 是发现 10 的
    //    CLI 半边）──
    let report = format!(
        concat!(
            "{{\"config\":{{\"resolution\":\"{w}x{h}\",\"fps\":{fps},\"duration_s\":{dur},",
            "\"backend\":\"{be}\",\"listen\":\"{listen}\",\"inject_mode\":\"{im}\"}},",
            "\"session\":{{\"session_id\":{sid},\"in_flight_peak\":{ifp},",
            "\"injected_events\":{iev},\"key_batches\":{kb},",
            "\"idr_requests_received\":{idrr},\"connected_ms\":{cms},",
            "\"captured\":{cap},\"dropped_stale\":{dst},",
            "\"dropped_repeat\":{drep},\"dropped_budget\":{dbud},",
            "\"probe_datagrams_sent\":{pds}}}}}"
        ),
        w = args.resolution.width,
        h = args.resolution.height,
        fps = args.fps,
        dur = duration.as_secs_f64(),
        be = escape(&backend),
        listen = escape(&listen.to_string()),
        im = inject_label(args.inject_mode),
        sid = host.session_id().unwrap_or(0),
        ifp = host.in_flight_peak(),
        iev = summary.events_injected,
        kb = host.key_batches(),
        idrr = host.idr_requests_received(),
        cms = connected_ms,
        cap = summary.captured,
        dst = summary.dropped_stale,
        drep = summary.dropped_repeat,
        dbud = summary.dropped_budget,
        pds = summary.probe_datagrams_sent,
    );
    write_report(args, &report)?;

    Ok(ExitCode::SUCCESS)
}

/// fake 假栈 + 共享锚点（修复卡 C-4 单采样不变式）：一次采样锚点，
/// 既作 [`WallCapture`] 帧时间戳的零点、也返回给调用方锚定
/// `ClockOrigin::Fake`——会话钟与 capture_ts 两轴同零点（顺序红线同
/// real 路径：锚点先确定、栈内全程用它，不二次采样）。
fn fake_stack_anchored(
    pcfg: &PipelineConfig,
    static_content: bool,
) -> rc_types::Result<(rc_pipeline::HostPipeline, Instant)> {
    let anchor = Instant::now();
    Ok((fake_stack(pcfg, static_content, anchor)?, anchor))
}

/// fake 假栈（照 rc-loopback fake_stack 形态）：FakeBackend（静止/动态）
/// + FakeEncoder，带节流（假后端必须 pace，坑 4）。
///
/// `anchor` 是帧时间戳的零点（修复卡 C-4：由 [`fake_stack_anchored`]
/// 单采样传入，与会话钟共用——不要在本函数内再 `Instant::now()`）。
/// 输出尺寸随分辨率缩放，封顶单包帧（64×64 与 1080p 均单包；尺寸预算
/// 见函数体内的注释）。
fn fake_stack(
    pcfg: &PipelineConfig,
    static_content: bool,
    anchor: Instant,
) -> rc_types::Result<rc_pipeline::HostPipeline> {
    let content = if static_content {
        rc_capture::FakeContent::Static
    } else {
        rc_capture::FakeContent::Dynamic
    };
    let capture = rc_capture::FakeBackend::new(rc_capture::FakeBackendConfig {
        display: rc_capture::DisplayInfo {
            id: rc_types::DisplayId(0),
            resolution: pcfg.resolution,
            scale: 1.0,
            hdr: false,
        },
        fps: pcfg.fps,
        content,
        dirty_rect: None,
    })?;
    let pixels = pcfg.resolution.width as usize * pcfg.resolution.height as usize;
    // 单包帧上限：媒体数据报 = 32B 包头 + 载荷 ≤ rc-net 预算
    // （mtu 1200 − QUIC_OVERHEAD 48 = 1152）→ 载荷 ≤ 1120，留余量取 1088。
    // 1080p fake 帧刻意压成单包：rc-proto 的 MAX_PAYLOAD=1200 满尺寸包
    // （1232B > 1152）会被 rc-net send_datagram 拒收——多包帧的预算修正
    // 属 rc-proto/rc-pipeline 的后续卡，本卡以单包 fake 帧实测。
    const SINGLE_PACKET_PAYLOAD: usize = 1_088;
    let output_bytes = (pixels / 8).clamp(128, SINGLE_PACKET_PAYLOAD);
    let encoder = rc_encode::FakeEncoder::with_tuning(
        rc_encode::EncoderConfig {
            resolution: pcfg.resolution,
            fps: pcfg.fps,
            bitrate_bps: pcfg.bitrate_bps,
            ..rc_encode::EncoderConfig::default()
        },
        FAKE_ENCODE_COST_US,
        output_bytes,
    )?;
    rc_pipeline::HostPipeline::with_backends_paced(
        pcfg.clone(),
        Box::new(WallCapture {
            inner: Box::new(capture),
            anchor,
        }),
        Box::new(encoder),
        true,
    )
}

/// 假采集的墙钟对齐包装（定位对齐 rc-loopback 的 `WallEncoder`：测量
/// 基础设施在 IO 边界读墙钟，真实睡眠模拟硬件）。
///
/// 为什么必须包：FakeBackend 的 Dynamic 时间戳是**按调用次数推进**的
/// 逻辑网格（`next_content_ts += interval`），节流包装让网格贴着墙钟走；
/// 但 T17 的 Detached 暂停会背压阻塞采集线程，暂停期间网格停摆、墙钟
/// 照走——恢复后两者出现不可恢复的缺口，每帧的 `capture_ts + 22ms`
/// deadline 都在过去，调度器把此后**所有**帧判 Stale 丢弃（重连演练
/// 实测复现：resume 后 8 秒零编码、IdrRequest 重发 68 次）。把时间戳
/// 改写为「锚定构造时刻的墙钟读数」后，deadline 永远新鲜，语义也诚实
/// （合成内容确实是在该时刻生成的）。
struct WallCapture {
    inner: Box<dyn rc_capture::CaptureBackend>,
    anchor: Instant,
}

impl rc_capture::CaptureBackend for WallCapture {
    fn next_frame(
        &mut self,
        timeout: Duration,
    ) -> rc_types::Result<Option<rc_capture::CapturedFrame>> {
        match self.inner.next_frame(timeout)? {
            Some(mut frame) => {
                frame.timestamp = rc_types::Timestamp(
                    u32::try_from(self.anchor.elapsed().as_micros()).unwrap_or(u32::MAX),
                );
                Ok(Some(frame))
            }
            None => Ok(None),
        }
    }
    fn poll_invalidation(&mut self) -> Option<rc_capture::InvalidationReason> {
        self.inner.poll_invalidation()
    }
    fn rebuild(&mut self) -> rc_types::Result<()> {
        self.inner.rebuild()
    }
    fn display_info(&self) -> rc_capture::DisplayInfo {
        self.inner.display_info()
    }
}

/// 真后端 host 半链：SCK + VideoToolbox（任一降级报错）。返回管线、
/// 标签与主机钟原点（µs；调用方锚进会话钟，保证 SCK PTS 同域——照
/// run_real 的 session_origin_us 模式）。
///
/// 顺序红线：必须**先**采样原点写进 `clock_origin_us`，**再**用该配置
/// 调 `select_capture_labeled`——SckConfig.origin_us 取自它，SCK 帧时间
/// 戳 = pts − origin，与会话钟（host_clock_us() − origin）同域同零点
///（照 rc-loopback run_real 的顺序）。2026-11-21 localhost 真后端实测
/// 教训：顺序颠倒（先建 SCK 流、后设零点）时 SCK 以首帧 PTS 为零点，
/// 首帧晚到 Δ（流启动 + 等屏幕变化）后每帧 capture_ts 都比会话钟旧
/// Δ，rc-sched 的 deadline = capture_ts + target_latency(22ms) 恒已过
/// → 每帧即到即 Drop(Stale)，30s 会话 0 帧编码、in_flight_peak=0、
/// IdrRequest 全饿死（同 app 内 SCK 直测与 M1 loopback 均正常，唯
/// host 双进程路径中招——差的就是这一处顺序）。
fn real_host_stack(
    hw_cfg: &PipelineConfig,
) -> rc_types::Result<(rc_pipeline::HostPipeline, String, u64)> {
    // 编码器先行自检（VT E6），再采样原点，最后启动 SCK 采集流。
    let (encoder, el) = rc_pipeline::select_encoder_labeled(hw_cfg)?;
    let origin_us = rc_pipeline::host_clock_us()
        .ok_or_else(|| rc_types::Error::InvalidConfig("平台无主机钟（real 仅 macOS）".into()))?;
    // 先锚零点再建 SCK 流（顺序红线见函数文档：SCK 与会话钟必须同零点）。
    let mut pcfg = hw_cfg.clone();
    pcfg.clock_origin_us = Some(origin_us);
    let (capture, cl) = rc_pipeline::select_capture_labeled(&pcfg)?;
    if el != "videotoolbox" || cl != "sck" {
        return Err(rc_types::Error::InvalidConfig(format!(
            "real 后端要求 sck+videotoolbox，实际 {cl}+{el}"
        )));
    }
    let pipeline = rc_pipeline::HostPipeline::with_backends_paced(
        pcfg, capture, encoder, false, // 真采集不节流
    )?;
    Ok((pipeline, format!("{cl}+{el}"), origin_us))
}

/// 注入模式的报告标签。
fn inject_label(mode: InjectMode) -> &'static str {
    match mode {
        InjectMode::Record => "record",
        InjectMode::Real => "real",
    }
}

/// 报告输出：`--report <path>` 写文件，`--report -` 或省略写 stdout（S1）。
fn write_report(args: &HostArgs, json: &str) -> CliResult<()> {
    match args.report.as_deref() {
        Some(path) if path != Path::new("-") => fs::write(path, format!("{json}\n"))
            .map_err(|e| format!("无法写报告文件 {}: {e}", path.display())),
        _ => {
            println!("{json}");
            Ok(())
        }
    }
}

/// 最小 JSON 字符串转义（对齐 rc-loopback report.rs）。
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// 开发证书 + 非环回监听的警告文案（修复卡D-3）；`None` = 无需警告。
///
/// 拆成纯函数锁定判定口径，供单测断言：仅当「`--cert` / `--key` 未成对
/// 提供（仍在用内嵌开发证书——只给一半时另一半也回退开发证书，私钥
/// 同样公开）」且「监听地址非环回」时警告。
fn dev_cert_warning(
    listen: &std::net::SocketAddr,
    cert: Option<&Path>,
    key: Option<&Path>,
) -> Option<String> {
    let using_dev_cert = !(cert.is_some() && key.is_some());
    (using_dev_cert && !listen.ip().is_loopback()).then(|| {
        format!(
            "⚠️  警告：--listen {listen} 非环回地址，且未成对提供 --cert/--key：\
             当前使用内嵌开发证书，其私钥随源码公开，不构成安全边界，\
             任何拿到该证书的人都能解密或冒充本会话。生产环境请提供自有证书。"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{dev_cert_warning, fake_stack, WallCapture};
    use rc_capture::CaptureBackend as _;
    use rc_pipeline::host::datagram_header;
    use rc_pipeline::{DiffMode, PipelineConfig};
    use std::net::SocketAddr;
    use std::path::Path;
    use std::time::{Duration, Instant};

    fn addr(s: &str) -> SocketAddr {
        s.parse().expect("合法 SocketAddr")
    }

    /// fake 栈的最小配置（64×64 @ 60fps 单包帧；字段照 run() 的模板）。
    fn tiny_pcfg() -> PipelineConfig {
        PipelineConfig {
            resolution: rc_types::Resolution::new(64, 64),
            fps: 60,
            bitrate_bps: 8_000_000,
            prefer_hardware: false,
            target_latency_us: 22_000,
            trace_capacity: 64,
            diff: DiffMode::TrustPlatform,
            clock_origin_us: None,
            max_packet_payload: rc_pipeline::MAX_PAYLOAD,
        }
    }

    /// 修复卡D-3：内嵌开发证书（--cert/--key 未成对提供）+ 非环回监听
    /// 必须给出醒目警告（点明「不构成安全边界」）；环回地址（v4/v6）
    /// 或成对提供了自有证书时不警告；只提供一半仍警告（另一半回退
    /// 开发证书）。
    #[test]
    fn dev_cert_warning_only_for_non_loopback_without_own_cert() {
        // 环回（v4 / v6）+ 开发证书：本机调试场景，不警告。
        for a in ["127.0.0.1:9901", "127.0.0.1:0", "[::1]:9901"] {
            assert!(
                dev_cert_warning(&addr(a), None, None).is_none(),
                "环回 {a} 不应警告"
            );
        }
        // 非环回（任意地址 / 局域网 / v6 任意）+ 开发证书：必须警告。
        for a in ["0.0.0.0:9901", "192.168.1.10:9901", "[::]:9901"] {
            let msg = dev_cert_warning(&addr(a), None, None)
                .unwrap_or_else(|| panic!("非环回 {a} 必须警告"));
            assert!(msg.contains("不构成安全边界"), "文案要点明风险：{msg}");
            assert!(msg.contains(a), "文案应带上监听地址 {a}：{msg}");
        }
        // 成对提供了 --cert/--key：自有证书，不警告。
        let p = Path::new("/tmp/rc-cert.pem");
        assert!(dev_cert_warning(&addr("0.0.0.0:9901"), Some(p), Some(p)).is_none());
        // 只提供一半（另一半回退开发证书）：仍警告。
        assert!(dev_cert_warning(&addr("0.0.0.0:9901"), Some(p), None).is_some());
        assert!(dev_cert_warning(&addr("0.0.0.0:9901"), None, Some(p)).is_some());
    }

    /// 修复卡 C-4：WallCapture 的帧时间戳严格跟随**传入锚点**的轴
    ///（next_frame 调用前后夹逼，误差 < 1ms 级；注入假构造验证换轴
    /// 行为）。锚点共享的另一侧（ClockOrigin::Fake）由 fake_stack_anchored
    /// 的单变量传递保证——同一次采样既进本包装也返回给调用方。
    #[test]
    fn wall_capture_timestamps_follow_shared_anchor() {
        let inner = rc_capture::FakeBackend::new(rc_capture::FakeBackendConfig {
            display: rc_capture::DisplayInfo {
                id: rc_types::DisplayId(0),
                resolution: rc_types::Resolution::new(64, 64),
                scale: 1.0,
                hdr: false,
            },
            fps: 60,
            content: rc_capture::FakeContent::Dynamic,
            dirty_rect: None,
        })
        .expect("假采集后端构造");
        let anchor = Instant::now();
        let mut cap = WallCapture {
            inner: Box::new(inner),
            anchor,
        };
        let lo = anchor.elapsed().as_micros() as u64;
        let frame = cap
            .next_frame(Duration::from_millis(100))
            .expect("采集不失败")
            .expect("动态内容立即出帧");
        let hi = anchor.elapsed().as_micros() as u64;
        let ts = u64::from(frame.timestamp.0);
        assert!(
            ts + 1_000 >= lo && ts <= hi + 1_000,
            "帧时间戳 {ts}µs 应落在传入锚点轴的 [{}, {}]（±1ms）内",
            lo.saturating_sub(1_000),
            hi + 1_000
        );
    }

    /// 修复卡 C-4：fake_stack 必须用**传入的**锚点换算帧时间戳，不得在
    /// 栈内二次采样 `Instant::now()`——传入 3 秒前的旧锚点，首帧数据报
    /// 头的 capture_ts 必须落在旧锚点的轴上（≈3s；二次采样会得到 ≈0）。
    #[test]
    fn fake_stack_capture_axis_uses_passed_anchor() {
        let anchor = Instant::now() - Duration::from_secs(3);
        let mut stack = fake_stack(&tiny_pcfg(), false, anchor).expect("fake 栈构造");
        let deadline = Instant::now() + Duration::from_millis(2_000);
        let mut capture_ts = None;
        while Instant::now() < deadline {
            stack.wait_event(Duration::from_millis(50));
            if let Some(datagrams) = stack.tick(0).expect("tick") {
                if let Some(first) = datagrams.first() {
                    let h = datagram_header(first).expect("帧头解析");
                    capture_ts = Some(h.capture_ts.0);
                    break;
                }
            }
        }
        stack.stop_capture();
        let ts = u64::from(capture_ts.expect("2s 内应产出首帧数据报"));
        assert!(
            (2_500_000..=3_600_000).contains(&ts),
            "capture_ts {ts}µs 应落在传入旧锚点的轴上（≈3s）——栈内二次采样会得到 ≈0"
        );
    }
}
