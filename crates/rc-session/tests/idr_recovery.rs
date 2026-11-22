//! T16 进程内丢包恢复演练：IDR 停滞看门狗 → IdrRequest → IdrAck 闭环。
//!
//! # harness 形态
//!
//! - 真实 `HostPipeline::with_backends_paced`（假采集 + 假编码，60fps 节流
//!   ——坑 9：不节流假后端瞬时洪灌，帧数变竞态）+ 真实
//!   `ControllerPipeline::with_backends`（假解码 + 假呈现），驱动形态照抄
//!   `rc-pipeline/tests/m1.rs`（墙钟会话钟 + drive 循环 + `set_clock`）；
//! - 数据报经可脚本化的 [`LossyGate`]（按丢包计划丢弃 host→client 的
//!   `Datagram`；reliable/urgent 控制流进程内直连**不丢**——QUIC 的
//!   可靠流语义，替身形态参考 rc-input `channel.rs` 的 MockTransport）；
//! - client 侧循环复刻任务卡「会话接线约定」的 6 步（tracker / watchdog /
//!   idr），host 侧收 IdrRequest → `request_idr`、`idr_frame_seqs` →
//!   IdrAck 回环（与 `ClientSession::tick` / `HostSession::tick` 同一接线，
//!   这里直接驱动管线以便注入丢包）。
//!
//! 本文件是演练 harness 的家（任务卡约定）；不建 testkit 的
//! ScriptedCapture/RecordingInjector（T18 交付）。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rc_capture::{DisplayInfo, FakeBackend, FakeBackendConfig, FakeContent};
use rc_decode::{FakeDecoder, FakeDecoderConfig};
use rc_encode::{EncoderConfig, FakeEncoder};
use rc_net::{NetEvent, NetStats, Transport};
use rc_pipeline::{ControllerPipeline, DiffMode, HostPipeline, PipelineConfig};
use rc_render::{FakeRenderer, FakeRendererConfig};
use rc_session::proto::{ControlMessage, IdrReason};
use rc_session::watchdog::{
    idr_frame_seqs, DeliveryTracker, IdrController, IdrControllerConfig, StallWatchdog,
    DEFAULT_STALL_US,
};
use rc_session::SessionClock;
use rc_types::{FrameSeq, Resolution, Result};

// ─────────────────────────────────────────────────────────────────
// LossyGate：可脚本丢包的进程内数据报通道
// ─────────────────────────────────────────────────────────────────

/// host→client datagram 通道的丢包计划。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum DropPlan {
    /// 直通（默认）。
    #[default]
    Off,
    /// 全丢（时间窗由测试线程开/关——「按时间窗丢弃 Datagram」的脚本化形态）。
    All,
    /// 只丢指定帧序号（丢单帧场景）。
    Seq(u64),
}

/// 两条单向事件队列 + 丢包计划 + 断言记录（共享于两端）。
#[derive(Default)]
struct GateState {
    /// client → host（reliable/urgent 直连不丢）。
    to_host: VecDeque<NetEvent>,
    /// host → client（datagram 经丢包门）。
    to_client: VecDeque<NetEvent>,
    drop_plan: DropPlan,
    /// host 发出的媒体数据报（原始字节，含被丢的；按发送序）。
    host_datagrams: Vec<Vec<u8>>,
    /// client 经 reliable 发出的原始消息（数 IdrRequest 用）。
    client_reliable: Vec<Vec<u8>>,
    /// 被门丢掉的数据报数。
    dropped: u64,
}

/// 丢包门的一端。
struct LossyGate {
    state: Arc<Mutex<GateState>>,
    is_host: bool,
}

/// 测试侧断言/脚本句柄（与两端共享同一状态）。
#[derive(Clone)]
struct GateHandle {
    state: Arc<Mutex<GateState>>,
}

impl GateHandle {
    /// 设置丢包计划。
    fn set_drop(&self, plan: DropPlan) {
        self.state.lock().expect("gate lock").drop_plan = plan;
    }

    /// host 发出的媒体数据报（原始字节，按发送序）。
    fn host_datagrams(&self) -> Vec<Vec<u8>> {
        self.state.lock().expect("gate lock").host_datagrams.clone()
    }

    /// client 经 reliable 发出的控制消息（按序解码）。
    fn client_reliable_decoded(&self) -> Vec<ControlMessage> {
        self.state
            .lock()
            .expect("gate lock")
            .client_reliable
            .iter()
            .map(|m| ControlMessage::decode(m).expect("演练 client 只发合法控制消息"))
            .collect()
    }

    /// 被门丢掉的数据报数。
    fn dropped(&self) -> u64 {
        self.state.lock().expect("gate lock").dropped
    }
}

/// 创建互连的一对（host 端、client 端）+ 测试句柄。
fn lossy_pair() -> (LossyGate, LossyGate, GateHandle) {
    let state = Arc::new(Mutex::new(GateState::default()));
    let handle = GateHandle {
        state: Arc::clone(&state),
    };
    (
        LossyGate {
            state: Arc::clone(&state),
            is_host: true,
        },
        LossyGate {
            state,
            is_host: false,
        },
        handle,
    )
}

/// host→client 的 datagram 是否按计划丢弃。
fn should_drop(state: &GateState, data: &[u8]) -> bool {
    match state.drop_plan {
        DropPlan::Off => false,
        DropPlan::All => true,
        DropPlan::Seq(seq) => rc_pipeline::host::datagram_header(data)
            .map(|h| h.frame_seq.0 == seq)
            .unwrap_or(false),
    }
}

impl Transport for LossyGate {
    fn send_datagram(&mut self, data: &[u8]) -> Result<()> {
        let mut s = self.state.lock().expect("gate lock");
        if self.is_host {
            s.host_datagrams.push(data.to_vec());
            if should_drop(&s, data) {
                s.dropped += 1;
                return Ok(()); // 网络丢失：不入对端队列
            }
            s.to_client.push_back(NetEvent::Datagram(data.to_vec()));
        } else {
            // client 不发媒体数据报；直通以防未来扩展。
            s.to_host.push_back(NetEvent::Datagram(data.to_vec()));
        }
        Ok(())
    }

    fn send_reliable(&mut self, data: &[u8]) -> Result<()> {
        let mut s = self.state.lock().expect("gate lock");
        if self.is_host {
            s.to_client.push_back(NetEvent::Reliable(data.to_vec()));
        } else {
            s.client_reliable.push(data.to_vec());
            s.to_host.push_back(NetEvent::Reliable(data.to_vec()));
        }
        Ok(())
    }

    /// 覆写以区分通道（与 testkit::MockTransport 同一理由）。
    fn send_reliable_urgent(&mut self, data: &[u8]) -> Result<()> {
        let mut s = self.state.lock().expect("gate lock");
        let q = if self.is_host {
            &mut s.to_client
        } else {
            &mut s.to_host
        };
        q.push_back(NetEvent::ReliableUrgent(data.to_vec()));
        Ok(())
    }

    /// 忽略 timeout 立即返回已就绪事件（确定性优先，同 testkit 纪律）。
    fn poll(&mut self, _timeout: Duration) -> Result<Vec<NetEvent>> {
        let mut s = self.state.lock().expect("gate lock");
        let q = if self.is_host {
            &mut s.to_host
        } else {
            &mut s.to_client
        };
        Ok(q.drain(..).collect())
    }

    fn stats(&self) -> NetStats {
        NetStats::default() // rtt=0：进程内直连，超时退化为纯 margin（坑 6）
    }
}

// ─────────────────────────────────────────────────────────────────
// 演练世界：真实管线 + client 侧恢复组件
// ─────────────────────────────────────────────────────────────────

/// 会话钟：构造时刻为原点的墙钟微秒（照 m1.rs 的 session_clock；假采集
/// 线程跑在墙钟网格上，now 必须同一基准）。
fn wall_clock() -> SessionClock {
    let start = Instant::now();
    Arc::new(move || u32::try_from(start.elapsed().as_micros()).unwrap_or(u32::MAX))
}

fn pcfg() -> PipelineConfig {
    PipelineConfig {
        resolution: Resolution::new(64, 64),
        fps: 60,
        bitrate_bps: 1_000_000,
        prefer_hardware: false,
        target_latency_us: 22_000,
        trace_capacity: 1024,
        diff: DiffMode::TrustPlatform,
        clock_origin_us: None,
        // 模板默认预算；真 QUIC 路径由 host 侧 expect_hello 按 N2 覆盖。
        max_packet_payload: rc_pipeline::MAX_PAYLOAD,
    }
}

/// client 侧恢复组件（与 `ClientSession::tick` 同一 6 步接线，直接驱动
/// 管线以便注入丢包与读中间计数）。
struct ClientLoop {
    controller: ControllerPipeline,
    tracker: DeliveryTracker,
    watchdog: StallWatchdog,
    idr: IdrController,
}

impl ClientLoop {
    fn new(controller: ControllerPipeline, clock: &SessionClock) -> Self {
        let mut watchdog = StallWatchdog::new(DEFAULT_STALL_US);
        // 等价 Streaming 进入：arm 基线锚在演练开始（首帧丢失也可恢复）。
        watchdog.arm(clock());
        Self {
            controller,
            tracker: DeliveryTracker::new(),
            watchdog,
            idr: IdrController::new(IdrControllerConfig::default()),
        }
    }

    /// client 一拍（6 步接线，顺序与任务卡「会话接线约定」一致）。
    fn round(&mut self, gate: &mut LossyGate, clock: &SessionClock) -> Result<()> {
        // ① poll 分发：Datagram → tracker.observe 再 push_datagram；
        //    Reliable → IdrAck → idr.on_ack。
        for ev in gate.poll(Duration::ZERO)? {
            match ev {
                NetEvent::Datagram(data) => {
                    let now = clock();
                    let before = self.tracker.last_complete().map(|f| (f.seq, f.at_us));
                    self.tracker.observe(&data, now)?;
                    self.controller.push_datagram(&data, now)?;
                    // ② 凑齐完整帧 → 喂看门狗与 IdrController。
                    let after = self.tracker.last_complete();
                    if after.map(|f| (f.seq, f.at_us)) != before {
                        let frame = after.expect("changed implies some");
                        self.watchdog.on_frame_complete(now);
                        self.idr.on_frame_complete(frame);
                    }
                }
                NetEvent::Reliable(msg) => {
                    if let ControlMessage::IdrAck { frame_seq } = ControlMessage::decode(&msg)? {
                        self.idr.on_ack(FrameSeq(frame_seq), clock());
                    }
                }
                NetEvent::ReliableUrgent(_) | NetEvent::Connected | NetEvent::PathChanged => {}
                NetEvent::Disconnected { .. } => {}
            }
        }
        // ③④ 触发：解码器 / 看门狗 → IdrController（去抖）。
        let now = clock();
        if self.controller.take_keyframe_request() {
            self.idr.request(IdrReason::DecoderNeedKeyframe, now);
        }
        if self.watchdog.poll(now) {
            self.idr.request(IdrReason::WatchdogStall, now);
        }
        // ⑤ 唯一发送出口。
        if let Some(req) = self.idr.poll(now, gate.stats().rtt_us) {
            let mut buf = Vec::new();
            ControlMessage::IdrRequest { reason: req.reason }.encode(&mut buf);
            gate.send_reliable(&buf)?;
        }
        // ⑥ 残帧 gc。
        self.tracker.gc(now);
        Ok(())
    }
}

/// 演练世界。
struct World {
    host: HostPipeline,
    host_gate: LossyGate,
    client: ClientLoop,
    client_gate: LossyGate,
    handle: GateHandle,
    clock: SessionClock,
}

/// host 一拍：先收控制（IdrRequest → `request_idr`，pending_idr 粘住），
/// 再 tick 产出发送（数据报逐条 + `idr_frame_seqs` → IdrAck 同拍，W5）。
/// 返回本拍是否有数据报产出。
fn host_round(host: &mut HostPipeline, gate: &mut LossyGate, clock: &SessionClock) -> Result<bool> {
    for ev in gate.poll(Duration::ZERO)? {
        if let NetEvent::Reliable(msg) = ev {
            if let ControlMessage::IdrRequest { .. } = ControlMessage::decode(&msg)? {
                host.request_idr();
            }
        }
    }
    let now = clock();
    if let Some(datagrams) = host.tick(now)? {
        for d in &datagrams {
            gate.send_datagram(d)?;
        }
        for seq in idr_frame_seqs(&datagrams) {
            let mut buf = Vec::new();
            ControlMessage::IdrAck { frame_seq: seq.0 }.encode(&mut buf);
            gate.send_reliable(&buf)?;
        }
        Ok(!datagrams.is_empty())
    } else {
        Ok(false)
    }
}

/// 组装演练世界（假后端 64×64@60、单包帧输出 128B）。
fn world() -> World {
    let clock = wall_clock();
    let mut host = HostPipeline::with_backends_paced(
        pcfg(),
        Box::new(
            FakeBackend::new(FakeBackendConfig {
                display: DisplayInfo {
                    id: rc_types::DisplayId(0),
                    resolution: Resolution::new(64, 64),
                    scale: 1.0,
                    hdr: false,
                },
                fps: 60,
                content: FakeContent::Dynamic,
                dirty_rect: None,
            })
            .expect("fake capture"),
        ),
        Box::new(
            FakeEncoder::with_tuning(
                EncoderConfig {
                    resolution: Resolution::new(64, 64),
                    fps: 60,
                    bitrate_bps: 1_000_000,
                    ..EncoderConfig::default()
                },
                500, // 编码耗时 µs
                128, // 输出 128B → 单包帧（多片语义由单元测试覆盖）
            )
            .expect("fake encoder"),
        ),
        true, // 节流（坑 9）
    )
    .expect("host pipeline");
    host.set_clock(Arc::clone(&clock));

    let mut controller = ControllerPipeline::with_backends(
        pcfg(),
        Box::new(
            FakeDecoder::new(FakeDecoderConfig {
                resolution: Resolution::new(64, 64),
                ..FakeDecoderConfig::default()
            })
            .expect("fake decoder"),
        ),
        Box::new(
            FakeRenderer::new(FakeRendererConfig {
                size: (64, 64),
                ..FakeRendererConfig::default()
            })
            .expect("fake renderer"),
        ),
    )
    .expect("controller pipeline");
    controller.set_clock(Arc::clone(&clock));

    let (host_gate, client_gate, handle) = lossy_pair();
    World {
        host,
        host_gate,
        client: ClientLoop::new(controller, &clock),
        client_gate,
        handle,
        clock,
    }
}

/// 单线程驱动一拍：host 先产发、client 后收（门内一拍延迟）。
/// host 无产出时经 `wait_event` 挂起防自旋（照 m1.rs 的 drive 形态）。
fn step(w: &mut World) -> Result<()> {
    let produced = host_round(&mut w.host, &mut w.host_gate, &w.clock)?;
    w.client.round(&mut w.client_gate, &w.clock)?;
    if !produced {
        w.host.wait_event(Duration::from_millis(2));
    }
    Ok(())
}

/// 驱动直到上屏数 ≥ n（上限 panic：管线饿死）。
fn drive_until_displayed(w: &mut World, n: u64, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while w.client.controller.displayed() < n {
        assert!(Instant::now() < deadline, "预热超时（{what}：管线饿死）");
        step(w).expect("step");
    }
}

// ─────────────────────────────────────────────────────────────────
// 演练测试
// ─────────────────────────────────────────────────────────────────

/// 1s 全丢包窗口：500–700ms 间 watchdog 触发 → IdrRequest(WatchdogStall)
/// 经 reliable 到 host → host 编出 IDR → 窗口结束后 client 完整收到并
/// 上屏（≤700ms）。断言窗口内 `requests_sent(reason=WatchdogStall)` ≤ 2
/// （去抖生效）、`resolved_by_idr ≥ 1`。
#[test]
fn loss_drill_recovers_via_watchdog_idr() {
    let mut w = world();
    drive_until_displayed(&mut w, 3, "正常流建立");

    // 开 1s 全丢包窗（host→client 的全部数据报丢弃）。
    w.handle.set_drop(DropPlan::All);
    let window_start = Instant::now();
    let mut first_fire: Option<Duration> = None;
    let mut prev_fires = w.client.watchdog.fires();
    while window_start.elapsed() < Duration::from_secs(1) {
        step(&mut w).expect("step");
        let fires = w.client.watchdog.fires();
        if fires > prev_fires && first_fire.is_none() {
            first_fire = Some(window_start.elapsed());
        }
        prev_fires = fires;
    }

    // 看门狗在 500–700ms 间触发（坑 9 宽窗：±100ms 定时器毛刺余量；
    // 理论值 = 最后完整帧 + 500ms ≈ 窗开后 ~517ms）。
    let fire = first_fire.expect("1s 全丢包窗内看门狗必须触发");
    assert!(
        fire >= Duration::from_millis(400) && fire <= Duration::from_millis(800),
        "看门狗应在最后完整帧后 ~500ms 触发，实际 {fire:?}"
    );
    // 同一停滞窗口内 IdrRequest(WatchdogStall) ≤ 2（在途去抖实测）。
    let ws = w.client.idr.stats().requests_by_reason[IdrReason::WatchdogStall as usize];
    assert!(
        ws <= 2,
        "同一停滞窗口内 IdrRequest(WatchdogStall) 不得超 2 条，实际 {ws}"
    );
    assert!(ws >= 1, "看门狗触发必须转化为 IdrRequest");
    // 去抖实测：IdrRequest 确实经 reliable 到达 host（不是纸面）。
    assert_eq!(
        w.handle.client_reliable_decoded().len(),
        w.client.idr.stats().requests_sent as usize
    );

    // 关窗 → 恢复上屏。
    w.handle.set_drop(DropPlan::Off);
    let window_end = Instant::now();
    let displayed_at_end = w.client.controller.displayed();
    let recover_deadline = Instant::now() + Duration::from_secs(3);
    while w.client.controller.displayed() == displayed_at_end {
        assert!(
            Instant::now() < recover_deadline,
            "丢包窗结束后 3s 内未恢复上屏"
        );
        step(&mut w).expect("step");
    }
    let recovery = window_end.elapsed();
    // 验收目标 ≤700ms；+100ms 宽窗（坑 9：定时器毛刺余量；理论 ~0.1s）。
    assert!(
        recovery <= Duration::from_millis(800),
        "窗口结束后 ≤700ms 恢复上屏（含 100ms 宽窗），实际 {recovery:?}"
    );
    assert!(
        w.client.idr.stats().resolved_by_idr >= 1,
        "必须有「等待中的 IDR 帧完整到达」闭环，实际 {:?}",
        w.client.idr.stats()
    );
    eprintln!(
        "drill#1: 触发={fire:?} 窗内WS请求={ws} 恢复={recovery:?} stats={:?}",
        w.client.idr.stats()
    );
}

/// 持续丢包 2s：请求总数有界（500ms 看门狗节拍 × 重发地板双重约束，
/// ≤5），无逐拍重发；恢复后 displayed 继续增长。
#[test]
fn loss_drill_sustained_loss_keeps_rate_bounded() {
    let mut w = world();
    drive_until_displayed(&mut w, 3, "正常流建立");

    // 持续全丢 2s。
    w.handle.set_drop(DropPlan::All);
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(2) {
        step(&mut w).expect("step");
    }
    let stats = w.client.idr.stats();
    let total: u64 = stats.requests_by_reason.iter().sum();
    assert!(
        total <= 5,
        "持续丢包 2s 请求总数必须 ≤5（有界，无逐拍重发），实际 {total}（{:?}）",
        stats
    );
    assert!(total >= 1, "持续丢包下看门狗节拍必须持续要 IDR");
    eprintln!("drill#2: 2s 请求总数={total} stats={stats:?}");

    // 关窗恢复：displayed 继续增长。
    w.handle.set_drop(DropPlan::Off);
    let displayed_at_end = w.client.controller.displayed();
    drive_until_displayed(&mut w, displayed_at_end + 1, "恢复后继续上屏");
}

/// 只丢一帧的数据报（<500ms 空隙）：`watchdog.fires() == 0`（不误报），
/// 后续帧正常上屏。
#[test]
fn single_frame_loss_does_not_fire_watchdog() {
    let mut w = world();
    drive_until_displayed(&mut w, 3, "正常流建立");

    // 丢「下一帧」：取 host 已发出的最大 frame_seq，指定丢 seq+1。
    let last_seq = w
        .handle
        .host_datagrams()
        .iter()
        .filter_map(|d| rc_pipeline::host::datagram_header(d).ok())
        .map(|h| h.frame_seq.0)
        .max()
        .expect("预热后必有数据报");
    w.handle.set_drop(DropPlan::Seq(last_seq + 1));
    let displayed_before = w.client.controller.displayed();

    // 驱动 1s（60fps → 单帧空隙 ~16.7ms，远小于 500ms 阈值）。
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(1) {
        step(&mut w).expect("step");
    }

    assert!(
        w.handle.dropped() >= 1,
        "前提校验：指定帧的数据报确实被丢（计划 Seq({}））",
        last_seq + 1
    );
    assert_eq!(
        w.client.watchdog.fires(),
        0,
        "丢单帧（<500ms 空隙）不得触发看门狗（不误报）"
    );
    assert!(
        w.client.controller.displayed() > displayed_before,
        "后续帧必须正常上屏"
    );
}
