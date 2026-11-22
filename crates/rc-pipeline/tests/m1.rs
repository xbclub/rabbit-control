//! T08 M1 管线九项验收测试（全部假后端、逻辑时钟、毫秒级）。

use rc_capture::{CaptureBackend, FakeBackend, FakeBackendConfig, FakeContent};
use rc_decode::{Decoder, FakeDecoder, FakeDecoderConfig};
use rc_encode::{Encoder, EncoderCaps, EncoderConfig, FakeEncoder};
use rc_pipeline::{merge_traces, ControllerPipeline, DiffMode, HostPipeline, PipelineConfig};
use rc_render::{FakeRenderer, FakeRendererConfig};
use rc_trace::{Mark, Outcome};
use rc_types::Resolution;
use rc_types::{Error, Result, Timestamp};

fn core_duration() -> std::time::Duration {
    std::time::Duration::from_millis(0)
}

fn cfg() -> PipelineConfig {
    PipelineConfig {
        resolution: Resolution::new(64, 64),
        fps: 60,
        bitrate_bps: 1_000_000,
        prefer_hardware: false,
        target_latency_us: 22_000,
        trace_capacity: 1024,
        diff: DiffMode::TrustPlatform,
        clock_origin_us: None,
        // M1 兼容性锁定：默认预算（1200）。
        max_packet_payload: rc_pipeline::MAX_PAYLOAD,
    }
}

fn fake_capture(content: FakeContent, fps: u32) -> Box<dyn CaptureBackend> {
    Box::new(
        FakeBackend::new(FakeBackendConfig {
            fps,
            content,
            ..FakeBackendConfig::default()
        })
        .expect("fake backend config valid"),
    )
}

fn fake_encoder() -> Box<dyn Encoder> {
    Box::new(FakeEncoder::new(encoder_cfg()).expect("fake encoder config valid"))
}

fn encoder_cfg() -> EncoderConfig {
    EncoderConfig {
        resolution: Resolution::new(64, 64),
        fps: 60,
        bitrate_bps: 1_000_000,
        ..EncoderConfig::default()
    }
}

fn fake_decoder() -> Box<dyn Decoder> {
    Box::new(FakeDecoder::new(FakeDecoderConfig::default()).expect("valid"))
}

fn fake_renderer() -> Box<dyn rc_render::Renderer> {
    Box::new(FakeRenderer::new(FakeRendererConfig::default()).expect("valid"))
}

/// 注入编码失败（非致命：E5 语义的 Timeout）的包装编码器。
struct FailingEncoder {
    inner: Box<dyn Encoder>,
    fail_first: u64,
    seen: u64,
}

impl Encoder for FailingEncoder {
    fn encode(
        &mut self,
        frame: &rc_capture::CapturedFrame,
        force_idr: bool,
        deadline_ts: Timestamp,
    ) -> Result<rc_encode::EncodedFrame> {
        self.seen += 1;
        if self.seen <= self.fail_first {
            // 非致命（is_fatal == false）：管线应记录后继续（P7）。
            return Err(Error::Timeout(core_duration()));
        }
        self.inner.encode(frame, force_idr, deadline_ts)
    }
    fn capabilities(&self) -> EncoderCaps {
        self.inner.capabilities()
    }
    fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        self.inner.set_bitrate(bps)
    }
    fn request_idr(&mut self) {
        self.inner.request_idr()
    }
    fn flush(&mut self) -> Result<Vec<rc_encode::EncodedFrame>> {
        self.inner.flush()
    }
}

/// 注入编码器过载丢帧（VT RealTime 的 [`Error::EncoderDroppedFrame`]）
/// 的包装编码器：前 `drop_first` 次编码在入口丢弃，其余透传内层。
struct OverloadDroppingEncoder {
    inner: Box<dyn Encoder>,
    drop_first: u64,
    seen: u64,
}

impl Encoder for OverloadDroppingEncoder {
    fn encode(
        &mut self,
        frame: &rc_capture::CapturedFrame,
        force_idr: bool,
        deadline_ts: Timestamp,
    ) -> Result<rc_encode::EncodedFrame> {
        self.seen += 1;
        if self.seen <= self.drop_first {
            // VT RealTime 过载的同构注入：非致命（is_fatal == false），
            // 管线应释放槽位、记结局后继续（P7）。
            return Err(Error::EncoderDroppedFrame {
                msg: "测试注入：模拟 RealTime 过载丢帧".to_string(),
            });
        }
        self.inner.encode(frame, force_idr, deadline_ts)
    }
    fn capabilities(&self) -> EncoderCaps {
        self.inner.capabilities()
    }
    fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        self.inner.set_bitrate(bps)
    }
    fn request_idr(&mut self) {
        self.inner.request_idr()
    }
    fn flush(&mut self) -> Result<Vec<rc_encode::EncodedFrame>> {
        self.inner.flush()
    }
}

/// 跑 n 帧：每帧轮询等采集线程供帧 + 异步编码完成到达（真实墙钟），
/// 数据报立即推给 controller。返回上屏总数。
fn drive(
    host: &mut HostPipeline,
    controller: &mut ControllerPipeline,
    clock: &std::sync::Arc<dyn Fn() -> u32 + Send + Sync>,
    n: usize,
) -> u64 {
    let mut displayed = 0;
    let now = || clock();
    let tick_push = |host: &mut HostPipeline,
                     controller: &mut ControllerPipeline,
                     now: u32,
                     displayed: &mut u64| {
        if let Some(datagrams) = host.tick(now).expect("tick ok") {
            for d in &datagrams {
                if controller.push_datagram(d, now).expect("push ok") {
                    *displayed += 1;
                }
            }
            true
        } else {
            false
        }
    };
    // 按采集计数驱动（tick 的 Some 也可能是上一帧的编码完成批，
    // 不能当「新帧到达」信号）；上限 n×100ms 防挂死。
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100 * n as u64);
    while std::time::Instant::now() < deadline {
        if controller.take_keyframe_request() {
            host.request_idr();
        }
        tick_push(host, controller, now(), &mut displayed);
        if host.sched_stats().captured >= n as u64 && !host.has_pending_encodes() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    // 排空异步编码完成。
    for _ in 0..200 {
        if host.has_pending_encodes() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        if !tick_push(host, controller, now(), &mut displayed) && !host.has_pending_encodes() {
            break;
        }
    }
    displayed
}

/// 会话时钟：构造时刻为原点的墙钟（微秒）。异步采集线程跑在墙钟网格上，
/// `now` 必须同一基准，否则健康帧会被误判超期（dropped_stale 假阳性）。
fn session_clock() -> (
    std::sync::Arc<dyn Fn() -> u32 + Send + Sync>,
    std::time::Instant,
) {
    let start = std::time::Instant::now();
    let slot = std::sync::Arc::new(std::sync::Mutex::new(start));
    let s2 = std::sync::Arc::clone(&slot);
    let clock: std::sync::Arc<dyn Fn() -> u32 + Send + Sync> = std::sync::Arc::new(move || {
        u32::try_from(s2.lock().expect("clk").elapsed().as_micros()).unwrap_or(u32::MAX)
    });
    (clock, start)
}

fn host_controller(
    capture: Box<dyn CaptureBackend>,
    encoder: Box<dyn Encoder>,
) -> (
    HostPipeline,
    ControllerPipeline,
    std::sync::Arc<dyn Fn() -> u32 + Send + Sync>,
) {
    // 节流模式：假采集按 60fps 墙钟供帧——异步采集线程下的确定性前提
    //（不节流会瞬时洪灌通道，帧数变成竞态）。
    let (clock, _start) = session_clock();
    let mut host =
        HostPipeline::with_backends_paced(cfg(), capture, encoder, true).expect("host ok");
    host.set_clock(std::sync::Arc::clone(&clock));
    let controller =
        ControllerPipeline::with_backends(cfg(), fake_decoder(), fake_renderer()).expect("ctl ok");
    (host, controller, clock)
}

fn merged(host: &HostPipeline, controller: &ControllerPipeline) -> rc_trace::TraceCollector {
    merge_traces(host.trace(), controller.trace(), 4096)
}

// ── 1. P4：每帧恰好一个结局 ─────────────────────────────────
#[test]
fn pipeline_produces_trace_for_every_frame() {
    let (mut host, mut controller, clock) =
        host_controller(fake_capture(FakeContent::Dynamic, 60), fake_encoder());
    let displayed = drive(&mut host, &mut controller, &clock, 30);
    controller.finish_in_flight();
    let trace = merged(&host, &controller);
    let s = trace.summary();
    assert_eq!(s.unfinished, 0, "P4：不允许未结帧，实际 {s:?}");
    assert_eq!(s.frames, 30, "30 次 tick 应产生 30 帧");
    assert!(displayed > 0);
    assert_eq!(s.displayed, displayed);
}

// ── 2. 静止画面零编码 ──────────────────────────────────────
#[test]
fn static_content_produces_no_encoded_frames() {
    let (mut host, mut controller, clock) =
        host_controller(fake_capture(FakeContent::Static, 60), fake_encoder());
    // 静止画面无帧：跑等价墙钟时长（20 帧 × 16.7ms）的 tick 轮询。
    let mut displayed = 0;
    let t0 = std::time::Instant::now();
    while t0.elapsed() < std::time::Duration::from_millis(334) {
        if let Some(d) = host.tick(clock()).expect("tick") {
            for p in &d {
                if controller.push_datagram(p, clock()).expect("push") {
                    displayed += 1;
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert_eq!(displayed, 0, "静止画面不应上屏");
    assert_eq!(host.sched_stats().encoded, 0, "静止画面零编码（Q5）");
    assert_eq!(host.trace().summary().unfinished, 0);
}

// ── 3. P7：编码失败不终止管线 ───────────────────────────────
#[test]
fn encoder_failure_does_not_stop_pipeline() {
    let encoder = Box::new(FailingEncoder {
        inner: fake_encoder(),
        fail_first: 3,
        seen: 0,
    });
    let (mut host, mut controller, clock) =
        host_controller(fake_capture(FakeContent::Dynamic, 60), encoder);
    let displayed = drive(&mut host, &mut controller, &clock, 20);
    controller.finish_in_flight();
    let s = merged(&host, &controller).summary();
    assert_eq!(s.decode_errors, 3, "前 3 帧编码失败应记 DecodeError");
    assert!(displayed >= 17, "失败后的帧继续上屏，实际 {displayed}");
    assert_eq!(s.unfinished, 0);
}

// ── 3b. P7：编码器过载丢帧（EncoderDroppedFrame）不终止管线 ──
// 复现 1080p×120 真机缺陷的离线形态：VT RealTime 过载丢帧曾被映射为
// 恒致命的 Error::Encoder，导致 host Err 上抛拆会话。本测试注入同构的
// 非致命丢帧，断言会话存活、槽位释放、后续帧照常编码上屏。
#[test]
fn encoder_overload_drop_does_not_stop_pipeline() {
    let encoder = Box::new(OverloadDroppingEncoder {
        inner: fake_encoder(),
        drop_first: 3,
        seen: 0,
    });
    let (mut host, mut controller, clock) =
        host_controller(fake_capture(FakeContent::Dynamic, 60), encoder);
    // drive 内部对每次 tick expect("tick ok")：若过载丢帧被上抛（回归），
    // 这里直接 panic，即「管线不终止」断言。
    let displayed = drive(&mut host, &mut controller, &clock, 20);
    controller.finish_in_flight();
    let s = merged(&host, &controller).summary();
    // 过载丢帧按语义最近的现有 Outcome 记 DroppedStale（host.rs 注释）。
    assert_eq!(
        s.dropped_stale, 3,
        "前 3 帧过载丢帧应记 DroppedStale，实际 {s:?}"
    );
    assert!(displayed >= 17, "丢帧后的帧继续上屏，实际 {displayed}");
    assert_eq!(s.unfinished, 0, "P4：每帧必须有结局，实际 {s:?}");
    // 槽位释放：失败路径漏调 on_encoded 会让 max_in_flight=2 被占满，
    // 第 4 帧起全部 Drop(Budget)。
    let st = host.sched_stats();
    assert_eq!(
        st.dropped_budget, 0,
        "过载丢帧后槽位必须释放，实际 budget 丢弃 {st:?}"
    );
    assert!(st.encoded > 3, "丢帧后仍应继续编码成功，实际 {st:?}");
}

// ── 4. P2：on_encoded 在所有路径调用（在途槽位释放）─────────
#[test]
fn on_encoded_called_on_all_paths() {
    let encoder = Box::new(FailingEncoder {
        inner: fake_encoder(),
        fail_first: 3,
        seen: 0,
    });
    let (mut host, mut controller, clock) =
        host_controller(fake_capture(FakeContent::Dynamic, 60), encoder);
    let _ = drive(&mut host, &mut controller, &clock, 20);
    let st = host.sched_stats();
    // 若失败路径漏调 on_encoded，max_in_flight=2 会被永久占满，
    // 第 4 帧起全部 Drop(Budget)。
    assert_eq!(
        st.dropped_budget, 0,
        "编码失败后槽位必须释放，实际 budget 丢弃 {st:?}"
    );
    assert!(st.encoded > 3, "失败后仍应继续编码，实际 {st:?}");
}

// ── 5. P6：缺包不交付 ──────────────────────────────────────
#[test]
fn reassembly_waits_for_all_packets() {
    // 输出 3000 字节 → 1200+1200+600 三包。
    let encoder = Box::new(FakeEncoder::with_tuning(encoder_cfg(), 5_000, 3_000).expect("valid"));
    let (mut host, mut controller, _clock) =
        host_controller(fake_capture(FakeContent::Dynamic, 60), encoder);
    let mut datagrams = Vec::new();
    // 异步编码：多 tick 几轮让完成到达。
    for i in 0..12u32 {
        if let Some(d) = host.tick(i * 5_000).expect("tick") {
            datagrams.extend(d);
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    // 取第一帧的三包（同 frame_seq）。
    let (h0, _) = rc_proto::MediaHeader::parse_datagram(&datagrams[0]).expect("header");
    let first_seq = h0.frame_seq;
    let frame_pkts: Vec<Vec<u8>> = datagrams
        .iter()
        .filter(|d| {
            rc_proto::MediaHeader::parse_datagram(d)
                .map(|(h, _)| h.frame_seq == first_seq)
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    assert_eq!(frame_pkts.len(), 3, "3000B 输出应切成 3 包");
    assert_eq!(h0.packet_cnt, 3, "头部声明 3 包");
    // 逐包投递：前两包不交付，第三包交付。
    let now = 100_000;
    assert!(!controller.push_datagram(&frame_pkts[0], now).expect("p0"));
    assert!(!controller.push_datagram(&frame_pkts[1], now).expect("p1"));
    assert!(
        controller.push_datagram(&frame_pkts[2], now).expect("p2"),
        "凑齐才交付"
    );
}

// ── 6. P3：关键帧请求传到编码器 ─────────────────────────────
#[test]
fn keyframe_request_propagates_to_encoder() {
    let decoder = Box::new(
        FakeDecoder::new(FakeDecoderConfig {
            break_ref_chain_at: Some(5),
            ..FakeDecoderConfig::default()
        })
        .expect("valid"),
    );
    let (clock, _) = session_clock();
    let mut controller =
        ControllerPipeline::with_backends(cfg(), decoder, fake_renderer()).expect("ctl ok");
    let mut host = HostPipeline::with_backends_paced(
        cfg(),
        fake_capture(FakeContent::Dynamic, 60),
        fake_encoder(),
        true,
    )
    .expect("host ok");
    host.set_clock(std::sync::Arc::clone(&clock));
    // 手动驱动（不用 drive：它会替我们把请求转发掉，这里要亲自断言）。
    // 按采集计数驱动到第 10 帧（断链点 5 之后），1ms 步进等异步线程。
    let mut kf_seen = false;
    let mut idr_after_kf = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1_500);
    while host.sched_stats().captured < 10 && std::time::Instant::now() < deadline {
        let now = clock();
        if controller.take_keyframe_request() {
            kf_seen = true;
            host.request_idr();
        }
        if let Some(d) = host.tick(now).expect("tick") {
            for pkt in &d {
                let (h, _) = rc_proto::MediaHeader::parse_datagram(pkt).expect("hdr");
                if h.flags.is_idr() && kf_seen {
                    idr_after_kf = true;
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(kf_seen, "断链后必须请求关键帧");
    assert!(idr_after_kf, "请求后的下一帧必须是 IDR");
}

// ── 7. 一致性不变式 ─────────────────────────────────────────
#[test]
fn pipeline_reports_consistent_counts() {
    let (mut host, mut controller, clock) =
        host_controller(fake_capture(FakeContent::Dynamic, 60), fake_encoder());
    let _ = drive(&mut host, &mut controller, &clock, 40);
    controller.finish_in_flight();
    let s = merged(&host, &controller).summary();
    let st = host.sched_stats();
    assert_eq!(s.frames, st.captured, "trace 帧数 == 调度器采集数");
    let dropped = s.dropped_repeat + s.dropped_stale + s.dropped_budget + s.dropped_superseded;
    assert_eq!(
        s.frames,
        s.displayed + dropped + s.lost_in_network + s.decode_errors + s.unfinished,
        "不变式 A"
    );
    assert_eq!(
        st.encoded,
        s.displayed + s.lost_in_network + s.decode_errors + s.unfinished,
        "不变式 B（调度器授权编码 == 最终去向）"
    );
}

// ── 8. P8：主后端不可用时降级 ────────────────────────────────
#[test]
fn factory_falls_back_when_primary_unavailable() {
    // VT 硬编拒绝奇数分辨率 → auto 工厂降级假编码器（可复现注入）。
    let mut c = cfg();
    c.prefer_hardware = true;
    c.resolution = Resolution::new(63, 63);
    let (enc, label) = rc_pipeline::select_encoder_labeled(&c).expect("factory ok");
    assert_eq!(label, "fake", "奇分辨率下应降级");
    assert!(!enc.capabilities().hardware);
    // 关闭硬件偏好 → 直接假后端。
    let mut c2 = cfg();
    c2.prefer_hardware = false;
    let (enc2, label2) = rc_pipeline::select_encoder_labeled(&c2).expect("ok");
    assert_eq!(label2, "fake");
    assert!(!enc2.capabilities().hardware);
}

// ── 9. 分段之和 == 总延迟（容差 1ms）────────────────────────
#[test]
fn latency_breakdown_sums_to_total() {
    let (mut host, mut controller, clock) =
        host_controller(fake_capture(FakeContent::Dynamic, 60), fake_encoder());
    let _ = drive(&mut host, &mut controller, &clock, 20);
    controller.finish_in_flight();
    let trace = merged(&host, &controller);
    let mut checked = 0;
    for f in trace.frames() {
        if f.outcome() != Some(Outcome::Displayed) {
            continue;
        }
        let total = f.end_to_end_us().expect("displayed has e2e");
        let segs = f.latency_breakdown();
        let sum: u64 = segs
            .iter()
            .filter(|(n, _)| *n != "total")
            .map(|(_, us)| u64::from(*us))
            .sum();
        assert!(
            (sum as i64 - i64::from(total)).abs() <= 1_000,
            "seq {:?} 分段和 {sum} != 总延迟 {total}（>1ms）",
            f.seq()
        );
        checked += 1;
    }
    assert!(checked > 0, "至少应有一帧上屏可校验");
}

#[test]
fn datagram_header_roundtrip() {
    let (mut host, mut controller, clock) =
        host_controller(fake_capture(FakeContent::Dynamic, 60), fake_encoder());
    let _ = drive(&mut host, &mut controller, &clock, 3);
    // drive 排空后 trace 里至少有一帧走完 Sent。
    let frame = host.trace().frames().first().expect("has frame");
    assert!(
        frame.at(Mark::Sent).is_some(),
        "排空后首帧应有 Sent 打点（异步完成已收取）"
    );
}

// ── 10. 铁律 1：并发重组残帧硬上限 = 2，超限淘汰最旧并记 LostInNetwork ──
#[test]
fn reassembly_caps_concurrent_partials_at_two() {
    use rc_types::{FrameKind, FrameSeq};

    let make_two_pkts = |seq: u64, ts: u32| -> Vec<Vec<u8>> {
        let payload = vec![0xAB; rc_proto::MAX_PAYLOAD + 64];
        let pkts = rc_proto::split_frame(
            FrameSeq(seq),
            FrameKind::Idr,
            Timestamp(ts),
            Timestamp(ts),
            0,
            &payload,
            rc_proto::MAX_PAYLOAD,
        );
        assert_eq!(pkts.len(), 2);
        pkts
    };
    let make_one_pkt = |seq: u64, ts: u32| -> Vec<u8> {
        let pkts = rc_proto::split_frame(
            FrameSeq(seq),
            FrameKind::Idr,
            Timestamp(ts),
            Timestamp(ts),
            0,
            &[0xCD; 64],
            rc_proto::MAX_PAYLOAD,
        );
        assert_eq!(pkts.len(), 1);
        pkts.into_iter().next().expect("single packet")
    };

    let mut controller =
        ControllerPipeline::with_backends(cfg(), fake_decoder(), fake_renderer()).expect("ctl ok");

    // 场景 A：乱序降序到达 12(0/2) -> 11(0/2) -> 10(0/2)，验证并发上限 = 2。
    let f12 = make_two_pkts(12, 12_000);
    let f11 = make_two_pkts(11, 11_000);
    let f10 = make_two_pkts(10, 10_000);

    assert!(!controller.push_datagram(&f12[0], 12_500).expect("12/0"));
    assert!(!controller.push_datagram(&f11[0], 13_000).expect("11/0"));
    assert_eq!(
        controller.trace().summary().lost_in_network,
        0,
        "2 个并发残帧在上限内，不应提前淘汰"
    );

    // 第 3 个并发残帧（最旧的 10）触发容量上限，应立即淘汰 10 并记 LostInNetwork。
    assert!(!controller.push_datagram(&f10[0], 13_500).expect("10/0"));
    assert_eq!(
        controller
            .trace()
            .get(FrameSeq(10))
            .and_then(|f| f.outcome()),
        Some(Outcome::LostInNetwork),
        "超限时最旧残帧 10 应记 LostInNetwork"
    );
    assert_eq!(controller.trace().summary().lost_in_network, 1);
    assert_eq!(controller.trace().summary().unfinished, 0);

    // 迟到的 10(1/2) 不得复活已淘汰帧，也不得重复 begin 或产生 unfinished。
    assert!(!controller.push_datagram(&f10[1], 14_000).expect("10/1"));
    assert_eq!(controller.trace().summary().lost_in_network, 1);
    assert_eq!(controller.trace().summary().unfinished, 0);

    // 保留在表内的 11、12 补齐后仍可正常解码上屏。
    assert!(controller.push_datagram(&f11[1], 20_000).expect("11/1"));
    assert!(controller.push_datagram(&f12[1], 30_000).expect("12/1"));
    assert_eq!(controller.displayed(), 2);

    // 场景 B：表内已有 2 个更新残帧（22、21）时，更旧的单包帧 20 到达不得 panic，
    // 应被淘汰并记 LostInNetwork。
    let f22 = make_two_pkts(22, 42_000);
    let f21 = make_two_pkts(21, 41_000);
    let f20 = make_one_pkt(20, 40_000);
    assert!(!controller.push_datagram(&f22[0], 42_500).expect("22/0"));
    assert!(!controller.push_datagram(&f21[0], 43_000).expect("21/0"));
    assert!(!controller.push_datagram(&f20, 43_500).expect("20/0"));
    assert_eq!(
        controller
            .trace()
            .get(FrameSeq(20))
            .and_then(|f| f.outcome()),
        Some(Outcome::LostInNetwork)
    );
    controller.finish_in_flight();
    assert_eq!(
        controller
            .trace()
            .get(FrameSeq(21))
            .and_then(|f| f.outcome()),
        Some(Outcome::LostInNetwork)
    );
    assert_eq!(
        controller
            .trace()
            .get(FrameSeq(22))
            .and_then(|f| f.outcome()),
        Some(Outcome::LostInNetwork)
    );

    // 场景 C：升序均未凑齐 30(0/2) -> 31(0/2) -> 32(0/2)，旧残帧应逐一淘汰，
    // 迟到分片不复活，finish_in_flight 后 unfinished == 0。
    let f30 = make_two_pkts(30, 50_000);
    let f31 = make_two_pkts(31, 51_000);
    let f32 = make_two_pkts(32, 52_000);
    assert!(!controller.push_datagram(&f30[0], 50_500).expect("30/0"));
    assert!(!controller.push_datagram(&f31[0], 51_500).expect("31/0"));
    assert!(!controller.push_datagram(&f32[0], 52_500).expect("32/0"));
    assert!(!controller.push_datagram(&f30[1], 53_000).expect("30/1"));
    controller.finish_in_flight();

    let s = controller.trace().summary();
    assert_eq!(s.unfinished, 0, "所有帧必须有明确结局，实际 {s:?}");
    assert_eq!(s.displayed, 2, "仅 11、12 上屏，实际 {s:?}");
    assert_eq!(
        s.lost_in_network, 7,
        "10, 20, 21, 22, 30, 31, 32 共 7 帧应记 LostInNetwork，实际 {s:?}"
    );
    assert_eq!(s.frames, 9, "共 9 帧且无重复 trace 条目，实际 {s:?}");
}

// ── 11. 采集线程背压采用事件驱动 Condvar（无 1ms 轮询阻塞 & 停机秒级退出）──
#[test]
fn capture_backpressure_wakes_without_polling_sleep() {
    let (clock, _) = session_clock();
    // 关闭节流让假采集持续撞满 cap_tx（容量 1），专门放大背压交接路径。
    let mut host = HostPipeline::with_backends_paced(
        cfg(),
        fake_capture(FakeContent::Dynamic, 60),
        fake_encoder(),
        false,
    )
    .expect("host ok");
    host.set_clock(std::sync::Arc::clone(&clock));

    // 先等采集线程把容量 1 的通道灌满并进入背压等待。
    std::thread::sleep(std::time::Duration::from_millis(5));

    let started = std::time::Instant::now();
    let target = 40u64;
    while host.sched_stats().captured < target {
        let out = host.tick(clock()).expect("tick ok");
        if out.is_none() {
            host.wait_event(std::time::Duration::from_millis(5));
        }
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "背压交接超时（已采集 {}）",
            host.sched_stats().captured
        );
    }
    let elapsed = started.elapsed();
    // 旧 sleep(1ms) 在 macOS 定时器合并下 40 帧背压需 80–160ms；Condvar 应在数毫秒内完成。
    assert!(
        elapsed < std::time::Duration::from_millis(40),
        "40 帧背压交接耗时过高: {elapsed:?}（疑似退化为 sleep 轮询）"
    );

    // 停机时通道仍可能满载，stop_capture 必须通过 cap_space 立即唤醒并退出。
    std::thread::sleep(std::time::Duration::from_millis(2));
    let stop_start = std::time::Instant::now();
    host.stop_capture();
    assert!(
        stop_start.elapsed() < std::time::Duration::from_millis(50),
        "背压中 stop_capture 退出过慢: {:?}",
        stop_start.elapsed()
    );
    host.shutdown();
}
