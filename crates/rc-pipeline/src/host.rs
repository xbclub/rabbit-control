//! 被控端管线：采集线程 → 调度 → 编码线程 → 分片。
//!
//! 按 T08 数据流图逐段接线，红线对应：
//! - P1：`next_frame` 返回 `None` 时 `tick` 返回 `Ok(None)`，不 sleep 不空转；
//! - P2：`on_encoded` 在**所有**路径调用（含编码失败）——完成通道里
//!   `Err` 与 `Ok` 都先释放调度器在途槽位再记结局；
//! - P3：每次 `on_captured` 之后检查 `needs_keyframe` 与 `take_keyframe_request`；
//! - P4：调度丢弃的帧当场写结局；
//! - P5：`capture_ts` 用帧自带的 `timestamp`（内容变化时刻），`now_us` 与之同基准。
//!
//! # 线程模型（契约 C8：采集独立线程）
//! - **采集线程**持有 `CaptureBackend`，帧经 `sync_channel(1)` 交给调用方
//!   线程——通道容量就是 Q3 的队列上限之一（交接 1 + 调度在途 1 ≤ 2），
//!   满了采集线程阻塞（背压，不是丢弃也不是无界积压）。
//! - **编码线程**持有 `Encoder`，异步提交、异步完成。这是 `Budget` 语义
//!   成立的前提：编码进行中（槽位占用）时新帧到达，调度器才会做出
//!   「挤出最旧」的决策。同步编码下在途恒 ≤1，Budget 永不触发。
//! - `tick` 在调用方线程汇聚：先收编码完成（打点/分片/Sent），再收采集
//!   帧（diff/调度/提交）。
//!
//! # 时间模型
//! 管线是计时员：`EncodeDone` / `Sent` 在操作返回后读会话时钟
//! （[`HostPipeline::set_clock`]），不信任后端内部时钟。未注入时钟时
//! 退化为 `now_us`（分段退化为 0，诚实可解释）。

use crate::{select_all, DiffMode, PipelineConfig};
use rc_capture::{CaptureBackend, CapturedFrame, FrameTexture};
use rc_diff::{DiffResult, TileDiff};
use rc_proto::{split_frame, MediaHeader};
use rc_sched::{Decision, DropReason, Scheduler, SchedulerConfig, SchedulerStats};
use rc_trace::{Mark, Outcome, TraceCollector};
use rc_types::{Error, FrameKind, Result, Timestamp};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{
    sync_channel, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError,
};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// 采集线程 → 调用方线程的消息。
enum CapMsg {
    Frame(Box<CapturedFrame>),
    Invalidated(rc_capture::InvalidationReason),
}

/// 调用方线程 → 采集线程的命令。
enum CapCmd {
    Rebuild,
}

/// 提交给编码线程的一个编码请求。
struct EncodeReq {
    seq: rc_types::FrameSeq,
    frame: CapturedFrame,
    force_idr: bool,
    deadline: Timestamp,
}

/// 编码线程 → 调用方线程的完成事件。
enum EncodeDone {
    Ok {
        seq: rc_types::FrameSeq,
        capture_ts: Timestamp,
        frame: rc_encode::EncodedFrame,
    },
    Err {
        seq: rc_types::FrameSeq,
        err: Error,
    },
}

/// 调用方线程 → 编码线程的异步命令（M2-10 / T15：[`HostPipeline::set_bitrate`]
/// 的转发通道；后续动态控制面复用此通道，不另开）。
enum EncCmd {
    /// 调整编码目标码率。
    SetBitrate(u32),
}

/// 编码线程内应用一条命令：编码器侧拒绝只进 stderr——对齐本文件
/// 「留 stderr 痕迹便于发现回归」的既有风格，不回传（会话层不该为一次
/// 码率调整阻塞等待，见 [`HostPipeline::set_bitrate`] 文档）。
fn apply_enc_cmd(encoder: &mut dyn rc_encode::Encoder, cmd: EncCmd) {
    match cmd {
        EncCmd::SetBitrate(bps) => {
            if let Err(e) = encoder.set_bitrate(bps) {
                eprintln!("rc-pipeline: set_bitrate({bps}) rejected by encoder: {e}");
            }
        }
    }
}

fn take_pending_ts(pending: &mut VecDeque<(u64, Timestamp)>, token: u64) -> Timestamp {
    if pending.front().is_some_and(|(t, _)| *t == token) {
        return pending.pop_front().map_or(Timestamp(0), |(_, ts)| ts);
    }
    if let Some(pos) = pending.iter().position(|(t, _)| *t == token) {
        return pending.remove(pos).map_or(Timestamp(0), |(_, ts)| ts);
    }
    Timestamp(0)
}

/// 处理一次 `recv_encoded` 结果并投递到 `done_tx`。
/// 返回 `None` 表示当前无就绪输出；`Some(true)` 表示已投递；`Some(false)` 表示接收端已断开。
fn handle_pipelined_recv(
    res: Result<Option<(u64, rc_encode::EncodedFrame)>>,
    pending: &mut VecDeque<(u64, Timestamp)>,
    in_vt: &mut usize,
    done_tx: &SyncSender<EncodeDone>,
    enc_wake: &(Mutex<u64>, Condvar),
) -> Option<bool> {
    let msg = match res {
        Ok(None) => return None,
        Ok(Some((token, frame))) => {
            *in_vt = in_vt.saturating_sub(1);
            let capture_ts = take_pending_ts(pending, token);
            EncodeDone::Ok {
                seq: rc_types::FrameSeq(token),
                capture_ts,
                frame,
            }
        }
        Err(err) => {
            *in_vt = in_vt.saturating_sub(1);
            let Some((seq, _)) = pending.pop_front() else {
                return Some(true);
            };
            EncodeDone::Err {
                seq: rc_types::FrameSeq(seq),
                err,
            }
        }
    };
    if done_tx.send(msg).is_err() {
        return Some(false);
    }
    signal_wake(enc_wake);
    Some(true)
}

fn submit_pipelined_req(
    encoder: &mut dyn rc_encode::Encoder,
    r: EncodeReq,
    pending: &mut VecDeque<(u64, Timestamp)>,
    in_vt: &mut usize,
    done_tx: &SyncSender<EncodeDone>,
    enc_wake: &(Mutex<u64>, Condvar),
) -> bool {
    match encoder.submit_encode(r.seq.0, &r.frame, r.force_idr, r.deadline) {
        Ok(()) => {
            pending.push_back((r.seq.0, r.frame.timestamp));
            *in_vt += 1;
            true
        }
        Err(err) => {
            if done_tx.send(EncodeDone::Err { seq: r.seq, err }).is_err() {
                return false;
            }
            signal_wake(enc_wake);
            true
        }
    }
}

fn run_pipelined_encode_loop(
    encoder: &mut dyn rc_encode::Encoder,
    req_rx: &Receiver<Mutex<Option<EncodeReq>>>,
    done_tx: &SyncSender<EncodeDone>,
    enc_cmd_rx: &Receiver<EncCmd>,
    enc_flag: &AtomicBool,
    enc_wake: &(Mutex<u64>, Condvar),
) {
    let mut in_vt = 0usize;
    let mut pending: VecDeque<(u64, Timestamp)> = VecDeque::with_capacity(2);
    let mut req_disconnected = false;

    while !enc_flag.load(Ordering::Relaxed) && !req_disconnected {
        // 1) 应用所有挂起命令（如码率调整）。
        while let Ok(cmd) = enc_cmd_rx.try_recv() {
            apply_enc_cmd(encoder, cmd);
        }

        let mut did_work = false;

        // 2) 非阻塞排空所有已完成输出，尽快腾出 VT 在途槽位并唤醒发送端。
        while let Some(ok) = handle_pipelined_recv(
            encoder.recv_encoded(Duration::ZERO),
            &mut pending,
            &mut in_vt,
            done_tx,
            enc_wake,
        ) {
            if !ok {
                return;
            }
            did_work = true;
        }

        // 3) 补满最多 2 帧在途编码（铁律 1）。
        while in_vt < 2 {
            match req_rx.try_recv() {
                Ok(slot) => {
                    if let Some(r) = slot.lock().expect("req lock").take() {
                        did_work = true;
                        if !submit_pipelined_req(
                            encoder,
                            r,
                            &mut pending,
                            &mut in_vt,
                            done_tx,
                            enc_wake,
                        ) {
                            return;
                        }
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    req_disconnected = true;
                    break;
                }
            }
        }

        // 4) 本轮未产生有效进展时再阻塞等待，避免空转。
        if !did_work && !req_disconnected {
            if in_vt > 0 {
                let wait = if in_vt == 1 {
                    Duration::from_millis(1)
                } else {
                    Duration::from_millis(5)
                };
                if let Some(false) = handle_pipelined_recv(
                    encoder.recv_encoded(wait),
                    &mut pending,
                    &mut in_vt,
                    done_tx,
                    enc_wake,
                ) {
                    return;
                }
            } else {
                match req_rx.recv_timeout(Duration::from_millis(5)) {
                    Ok(slot) => {
                        if let Some(r) = slot.lock().expect("req lock").take() {
                            if !submit_pipelined_req(
                                encoder,
                                r,
                                &mut pending,
                                &mut in_vt,
                                done_tx,
                                enc_wake,
                            ) {
                                return;
                            }
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => {
                        req_disconnected = true;
                    }
                }
            }
        }
    }

    // 5) 停机排空：应用剩余命令、提交并冲刷全部已提交/在途帧，保证 unfinished == 0。
    while let Ok(cmd) = enc_cmd_rx.try_recv() {
        apply_enc_cmd(encoder, cmd);
    }
    let drain_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        while let Some(ok) = handle_pipelined_recv(
            encoder.recv_encoded(Duration::ZERO),
            &mut pending,
            &mut in_vt,
            done_tx,
            enc_wake,
        ) {
            if !ok {
                return;
            }
        }
        let mut req_empty = req_disconnected;
        while in_vt < 2 && !req_empty {
            match req_rx.try_recv() {
                Ok(slot) => {
                    if let Some(r) = slot.lock().expect("req lock").take() {
                        if !submit_pipelined_req(
                            encoder,
                            r,
                            &mut pending,
                            &mut in_vt,
                            done_tx,
                            enc_wake,
                        ) {
                            return;
                        }
                    }
                }
                Err(TryRecvError::Empty) => {
                    req_empty = true;
                }
                Err(TryRecvError::Disconnected) => {
                    req_disconnected = true;
                    req_empty = true;
                }
            }
        }
        if in_vt == 0 && req_empty {
            break;
        }
        if Instant::now() >= drain_deadline {
            while let Some((seq, _)) = pending.pop_front() {
                in_vt = in_vt.saturating_sub(1);
                if done_tx
                    .send(EncodeDone::Err {
                        seq: rc_types::FrameSeq(seq),
                        err: Error::Timeout(Duration::from_secs(2)),
                    })
                    .is_err()
                {
                    return;
                }
                signal_wake(enc_wake);
            }
            break;
        }
        if in_vt > 0 {
            if let Some(false) = handle_pipelined_recv(
                encoder.recv_encoded(Duration::from_millis(5)),
                &mut pending,
                &mut in_vt,
                done_tx,
                enc_wake,
            ) {
                return;
            }
        }
    }
}

/// 被控端管线。
pub struct HostPipeline {
    cfg: PipelineConfig,
    trace: TraceCollector,
    diff: TileDiff,
    sched: Scheduler,
    /// 采集线程的收件/发件端与停机标志。
    cap_rx: Receiver<CapMsg>,
    cap_cmd: SyncSender<CapCmd>,
    cap_stop: Arc<AtomicBool>,
    /// 采集交接通道空位 / 停机唤醒信号（替代背压 `sleep(1ms)` 轮询）。
    cap_space: Arc<(Mutex<u64>, Condvar)>,
    cap_handle: Option<std::thread::JoinHandle<()>>,
    /// 编码线程的收件/发件端与停机标志。
    enc_req: SyncSender<Mutex<Option<EncodeReq>>>,
    enc_done: Receiver<EncodeDone>,
    enc_stop: Arc<AtomicBool>,
    enc_handle: Option<std::thread::JoinHandle<()>>,
    /// 编码线程命令通道（[`HostPipeline::set_bitrate`] 的转发路径，M2-10）。
    enc_cmd: SyncSender<EncCmd>,
    /// 会话时钟（见模块文档）。
    clock: Option<Arc<dyn Fn() -> u32 + Send + Sync>>,
    /// 下一帧应编成 IDR（丢帧恢复 / 控制端请求）。粘住直到真正编出 IDR。
    pending_idr: bool,
    /// 是否还没编出过任何帧（首帧强制 IDR）。
    no_idr_yet: bool,
    /// FEC 组号（每帧 +1，回绕）。
    fec_group: u16,
    /// 假采集的墙钟节流（见 `pace_capture`）。
    pace_capture: bool,
    /// 首次采集的墙钟时刻。会话时钟的零点必须锚在这里：平台时间戳
    /// （FakeBackend 网格 / SCK 的 PTS−首PTS）都以首次采集为零，
    /// 锚在别处（如首个数据报产出）会把管线延迟整体吃进零点偏移。
    first_capture: Option<std::time::Instant>,
    /// 事件唤醒信号（采集帧到达或编码完成时递增并 notify），消除调用方轮询 sleep 延迟。
    wake: Arc<(Mutex<u64>, Condvar)>,
    /// 最近一次 `tick()` 开始前观测到的事件代次（防丢失唤醒）。
    last_wake_gen: u64,
}

fn signal_wake(wake: &(Mutex<u64>, Condvar)) {
    let (lock, cv) = wake;
    if let Ok(mut g) = lock.lock() {
        *g = g.wrapping_add(1);
        cv.notify_one();
    }
}

impl HostPipeline {
    /// 用工厂后端创建（auto 模式：真后端失败降级假后端）。
    pub fn new(cfg: PipelineConfig) -> Result<Self> {
        cfg.validate()?;
        let (capture, encoder, _decoder, _renderer, labels) = select_all(&cfg)?;
        eprintln!(
            "rc-pipeline: host backends capture={} encoder={}",
            labels.capture, labels.encoder
        );
        Self::with_backends(cfg, capture, encoder)
    }

    /// 注入指定后端（测试 / 会话层显式控制）。
    ///
    /// `pace_capture == true` 时采集线程按 `1/fps` 节流假采集（假后端
    /// 瞬时出帧，不节流会把逻辑时间戳跑到墙钟前面）；真实采集的
    /// `next_frame` 自带阻塞语义，不额外节流。
    pub fn with_backends(
        cfg: PipelineConfig,
        capture: Box<dyn CaptureBackend>,
        encoder: Box<dyn rc_encode::Encoder>,
    ) -> Result<Self> {
        Self::with_backends_paced(cfg, capture, encoder, false)
    }

    /// 同 [`with_backends`]，但允许对假采集启用墙钟节流。
    pub fn with_backends_paced(
        cfg: PipelineConfig,
        mut capture: Box<dyn CaptureBackend>,
        encoder: Box<dyn rc_encode::Encoder>,
        pace_capture: bool,
    ) -> Result<Self> {
        cfg.validate()?;
        let sched_cfg = SchedulerConfig {
            // 铁律 1（Q3）：编码在途上限。真机数据（1080p60，SCK 源 ~70fps 供给）：
            // - =1：P50 13.87ms 更低，但 60s 长测里预算丢帧 842 次，displayed
            //   掉到 56.7fps < 58（帧率下限是硬门槛，短测 6s 看不出）；
            // - =2：P50 14.27ms / P99 28.28ms / 72.3fps / 0 预算丢帧 ——
            //   双达标且帧率有余量。VT 是异步会话，在途 2 能重叠提交/完成
            //   开销（假串行编码器的「在途 2 = 白等一个周期」不适用于它）。
            // 总深度 = 交接 1 + 在途 2；各队列均 ≤2（Q3 按队列计）。
            max_in_flight: 2,
            target_latency_us: cfg.target_latency_us,
            ..SchedulerConfig::default()
        };
        let sched = Scheduler::new(sched_cfg)
            .map_err(|e| Error::InvalidConfig(format!("scheduler config: {e}")))?;

        // ── 采集线程 ──────────────────────────────────────────
        // 通道容量 1 = Q3 的交接队列；满则采集阻塞（背压）。
        // 交接 1 + 调度在途 2：各队列均 ≤2（Q3 按队列计，见 sched_cfg 注释）。
        let wake = Arc::new((Mutex::new(0u64), Condvar::new()));
        let cap_wake = Arc::clone(&wake);
        let enc_wake = Arc::clone(&wake);
        let cap_space = Arc::new((Mutex::new(0u64), Condvar::new()));
        let cap_space_tx = Arc::clone(&cap_space);
        let (cap_tx, cap_rx) = sync_channel::<CapMsg>(1);
        let (cmd_tx, cmd_rx) = sync_channel::<CapCmd>(1);
        let cap_stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&cap_stop);
        let fps = cfg.fps;
        let pace = pace_capture;
        let cap_handle = std::thread::Builder::new()
            .name("rc-host-capture".into())
            .spawn(move || {
                let timeout = Duration::from_millis(16); // C10
                let interval = Duration::from_micros(u64::from(1_000_000u32 / fps.max(1)));
                let mut next_emit = std::time::Instant::now();
                while !stop_flag.load(Ordering::Acquire) {
                    // 处理重建命令（非阻塞；无命令继续采集）。
                    if let Ok(CapCmd::Rebuild) = cmd_rx.try_recv() {
                        if let Err(e) = capture.rebuild() {
                            let _ = cap_tx
                                .send(CapMsg::Invalidated(rc_capture::InvalidationReason::Unknown));
                            signal_wake(&cap_wake);
                            let _ = e;
                        }
                        continue;
                    }
                    match capture.next_frame(timeout) {
                        Ok(Some(frame)) => {
                            // 背压发送：快路径无锁 try_send；满时通过 Condvar 等待调用方
                            // 消费通知（微秒级唤醒，5ms 兜底查停机），消除 sleep(1ms) 在
                            // macOS 定时器合并下实睡 2-4ms 抬高 diff 延迟。
                            match cap_tx.try_send(CapMsg::Frame(Box::new(frame))) {
                                Ok(()) => signal_wake(&cap_wake),
                                Err(TrySendError::Full(mut back)) => loop {
                                    if stop_flag.load(Ordering::Acquire) {
                                        return; // 停机：丢弃在制帧
                                    }
                                    // 先读代次再复试 try_send，杜绝 try_send→wait 间丢唤醒。
                                    let seen_gen = cap_space_tx.0.lock().map(|g| *g).unwrap_or(0);
                                    match cap_tx.try_send(back) {
                                        Ok(()) => {
                                            signal_wake(&cap_wake);
                                            break;
                                        }
                                        Err(TrySendError::Full(b)) => back = b,
                                        Err(TrySendError::Disconnected(_)) => return,
                                    }
                                    let (lock, cv) = &*cap_space_tx;
                                    if let Ok(guard) = lock.lock() {
                                        if *guard == seen_gen && !stop_flag.load(Ordering::Acquire)
                                        {
                                            let _ =
                                                cv.wait_timeout(guard, Duration::from_millis(5));
                                        }
                                    }
                                },
                                Err(TrySendError::Disconnected(_)) => return,
                            }
                            if pace {
                                next_emit += interval;
                                let n = std::time::Instant::now();
                                if next_emit > n {
                                    std::thread::sleep(next_emit - n);
                                } else {
                                    next_emit = n;
                                }
                            }
                        }
                        Ok(None) => { /* C1/P1：无变化，继续循环 */ }
                        Err(e) => {
                            let recoverable = !e.is_fatal();
                            let reason = if recoverable {
                                rc_capture::InvalidationReason::AccessLost
                            } else {
                                rc_capture::InvalidationReason::Unknown
                            };
                            let _ = cap_tx.send(CapMsg::Invalidated(reason));
                            signal_wake(&cap_wake);
                            if !recoverable {
                                return;
                            }
                            // 可恢复：等待调用方发 Rebuild。
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    }
                }
            })
            .map_err(|e| Error::InvalidConfig(format!("spawn capture thread: {e}")))?;

        // ── 编码线程 ──────────────────────────────────────────
        // 请求通道容量 = max_in_flight(2) + 孤儿余量(2)：调度器驱逐在途帧
        // 时只释放自己的槽位，受害帧的编码请求仍占物理槽位直到完成。
        // 余量不足会让后续合法提交 try_send 假满（受害帧完成时由
        // `on_encoded == false` 识别并丢弃产出）。
        let (req_tx, req_rx) = sync_channel::<Mutex<Option<EncodeReq>>>(4);
        let (done_tx, done_rx) = sync_channel::<EncodeDone>(8);
        // 命令通道（M2-10）：有界容量 8——满即向上报错而不无界堆积，
        // 对齐 rc-net URGENT_QUEUE_CAPACITY 的有界通道纪律。
        let (enc_cmd_tx, enc_cmd_rx) = sync_channel::<EncCmd>(8);
        let enc_stop = Arc::new(AtomicBool::new(false));
        let enc_flag = Arc::clone(&enc_stop);
        let mut encoder = encoder;
        let enc_handle = std::thread::Builder::new()
            .name("rc-host-encode".into())
            .spawn(move || {
                #[cfg(target_os = "macos")]
                rc_capture_macos::promote_current_thread_qos();

                if encoder.supports_pipelined_encode() {
                    run_pipelined_encode_loop(
                        encoder.as_mut(),
                        &req_rx,
                        &done_tx,
                        &enc_cmd_rx,
                        &enc_flag,
                        &enc_wake,
                    );
                    return;
                }
                while !enc_flag.load(Ordering::Relaxed) {
                    // 每轮迭代先应用命令（含 5ms 超时醒来后的轮次：
                    // 空闲时命令最迟 5ms 生效）。
                    while let Ok(cmd) = enc_cmd_rx.try_recv() {
                        apply_enc_cmd(encoder.as_mut(), cmd);
                    }
                    match req_rx.recv_timeout(Duration::from_millis(5)) {
                        Ok(slot) => {
                            let req = slot.lock().expect("req lock").take();
                            if let Some(r) = req {
                                let out = encoder.encode(&r.frame, r.force_idr, r.deadline);
                                let msg = match out {
                                    Ok(f) => EncodeDone::Ok {
                                        seq: r.seq,
                                        capture_ts: r.frame.timestamp,
                                        frame: f,
                                    },
                                    Err(e) => EncodeDone::Err { seq: r.seq, err: e },
                                };
                                if done_tx.send(msg).is_err() {
                                    return;
                                }
                                signal_wake(&enc_wake);
                            }
                        }
                        Err(_) => continue,
                    }
                }
                // 停机排空段同样先排命令再排编码请求。
                while let Ok(cmd) = enc_cmd_rx.try_recv() {
                    apply_enc_cmd(encoder.as_mut(), cmd);
                }
                // 停机前排空已提交的请求（P2：每帧都要有结局）。
                while let Ok(slot) = req_rx.try_recv() {
                    if let Some(r) = slot.lock().expect("req lock").take() {
                        let out = encoder.encode(&r.frame, r.force_idr, r.deadline);
                        let msg = match out {
                            Ok(f) => EncodeDone::Ok {
                                seq: r.seq,
                                capture_ts: r.frame.timestamp,
                                frame: f,
                            },
                            Err(e) => EncodeDone::Err { seq: r.seq, err: e },
                        };
                        if done_tx.send(msg).is_err() {
                            break;
                        }
                        signal_wake(&enc_wake);
                    }
                }
            })
            .map_err(|e| Error::InvalidConfig(format!("spawn encode thread: {e}")))?;

        Ok(Self {
            diff: TileDiff::with_defaults(),
            trace: TraceCollector::new(cfg.trace_capacity),
            sched,
            cfg,
            cap_rx,
            cap_cmd: cmd_tx,
            cap_stop,
            cap_space,
            cap_handle: Some(cap_handle),
            enc_req: req_tx,
            enc_done: done_rx,
            enc_stop,
            enc_handle: Some(enc_handle),
            enc_cmd: enc_cmd_tx,
            clock: None,
            pending_idr: false,
            no_idr_yet: true,
            fec_group: 0,
            pace_capture: pace,
            first_capture: None,
            wake,
            last_wake_gen: 0,
        })
    }

    /// 注入会话时钟（微秒，与帧 `timestamp` 同基准）。见模块文档。
    pub fn set_clock(&mut self, clock: Arc<dyn Fn() -> u32 + Send + Sync>) {
        self.clock = Some(clock);
    }

    /// 读会话时钟；未注入时退化为 `now_us`。
    fn now(&self, fallback_us: u32) -> Timestamp {
        Timestamp(self.clock.as_ref().map_or(fallback_us, |c| c()))
    }

    /// 阻塞等待采集新帧或编码完成事件（最长 `timeout`）。
    ///
    /// 采用代次计数器避免丢失唤醒：若在上一轮 [`Self::tick`] 开头之后已有新事件
    /// 到达，本调用立即返回而不睡眠；无事件时由 Condvar 零自旋挂起并在事件抵达后
    /// 微秒级唤醒，替代 `sleep(1ms)` 轮询以消除 macOS 定时器合并带来的数毫秒延迟。
    pub fn wait_event(&self, timeout: Duration) {
        let (lock, cv) = &*self.wake;
        if let Ok(guard) = lock.lock() {
            if *guard == self.last_wake_gen {
                let _ = cv.wait_timeout(guard, timeout);
            }
        }
    }

    /// 跑一次迭代：收编码完成 → 收采集帧 → 调度提交。
    /// 返回 `Ok(Some(datagrams))` 表示有帧完成编码、应发送；
    /// `Ok(None)` 表示本次无产出（无帧 / 全部被丢弃）。
    pub fn tick(&mut self, now_us: u32) -> Result<Option<Vec<Vec<u8>>>> {
        if let Ok(g) = self.wake.0.lock() {
            self.last_wake_gen = *g;
        }
        let mut out = Vec::new();

        // ── 1. 编码完成（先收完成：槽位尽早释放给新帧）─────────
        while let Ok(done) = self.enc_done.try_recv() {
            match done {
                EncodeDone::Ok {
                    seq,
                    capture_ts,
                    frame,
                } => {
                    // 调度器裁决先行：false = 该帧在提交后被驱逐（Budget），
                    // 产出不得进入发送路径——否则「调度器已丢弃的帧被偷偷
                    // 上屏」，Q3 的丢帧语义就名存实亡。
                    if !self.sched.on_encoded(seq, now_us) {
                        if let Some(f) = self.trace.get_mut(seq) {
                            f.finish(Outcome::DroppedBudget);
                        }
                        continue;
                    }
                    let encode_done_ts = self.now(now_us);
                    if frame.is_keyframe {
                        self.no_idr_yet = false;
                        self.pending_idr = false;
                    }
                    let datagrams = split_frame(
                        seq,
                        frame.kind,
                        capture_ts,
                        encode_done_ts,
                        self.fec_group,
                        &frame.data,
                        // 分片预算来自配置（默认 MAX_PAYLOAD = M1 行为；真 QUIC
                        // 会话层按 rc-net 的 N2 预算覆盖，见字段文档）。
                        self.cfg.max_packet_payload,
                    );
                    self.fec_group = self.fec_group.wrapping_add(1);
                    let sent_at = self.now(now_us);
                    if let Some(f) = self.trace.get_mut(seq) {
                        f.mark(Mark::EncodeDone, encode_done_ts);
                        f.mark(Mark::Sent, sent_at);
                    }
                    out.extend(datagrams);
                }
                EncodeDone::Err { seq, err } => {
                    // P2：Err 路径同样先释放槽位（P7：不终止管线）。
                    self.sched.on_encoded(seq, now_us);
                    if let Some(f) = self.trace.get_mut(seq) {
                        // 编码器过载丢帧（EncoderDroppedFrame）：帧在编码器
                        // 入口被丢、未进参考链。最贴近的现有结局是
                        // DroppedStale（「超过 deadline 主动丢弃」——RealTime
                        // 过载丢帧的本质就是编码器判定本帧编不进实时预算）；
                        // 本次修复不新增 Outcome 变体（红线），其余编码
                        // 错误沿用既有 DecodeError 记法。
                        let outcome = match &err {
                            Error::EncoderDroppedFrame { .. } => Outcome::DroppedStale,
                            _ => Outcome::DecodeError,
                        };
                        f.finish(outcome);
                    }
                    if err.is_fatal() {
                        return Err(err);
                    }
                }
            }
        }

        // ── 2. 采集帧（消费当前交接槽并立刻唤醒背压中的采集线程）────────
        // 交接通道容量为 1：每轮处理 1 帧后返回 `Some`，调用方立刻进入下一轮
        // 用最新 `now_us` 先收 `enc_done` 再收下一帧，既保持微秒级交接又防止无节流
        // 假采集在 `Condvar` 瞬时唤醒下把单次 `tick` 锁死在循环内。
        let mut saw_frame = false;
        while let Ok(msg) = self.cap_rx.try_recv() {
            // 通道已腾出空位，立即唤醒背压等待中的采集线程。
            signal_wake(&self.cap_space);
            match msg {
                CapMsg::Invalidated(reason) => {
                    if reason.is_recoverable() {
                        let _ = self.cap_cmd.try_send(CapCmd::Rebuild);
                    } else {
                        return Err(Error::Capture {
                            recoverable: false,
                            msg: format!("fatal invalidation: {reason:?}"),
                        });
                    }
                }
                CapMsg::Frame(frame) => {
                    saw_frame = true;
                    if let Some(dgs) = self.process_frame(*frame, now_us)? {
                        out.extend(dgs);
                    }
                    break;
                }
            }
        }

        Ok(if out.is_empty() && !saw_frame {
            None
        } else {
            Some(out)
        })
    }

    /// 控制端请求关键帧（经会话层转发）。
    pub fn request_idr(&mut self) {
        self.pending_idr = true;
    }

    /// 动态调整编码目标码率（会话层 SetBitrate 消息的落点，M2-10）。
    ///
    /// 编码器在构造时被 move 进编码线程，本方法经**命令通道**转发：
    /// 纯增量——不改既有 tick / request_idr / stop_capture / shutdown 的
    /// 任何签名或时序。命令入队即返回（非阻塞），编码线程在下一个循环
    /// 迭代（≤5ms，其 recv_timeout 周期）应用并调用 `Encoder::set_bitrate`
    /// （Fake/VT 均实现）。编码器侧拒绝（超范围）只进 stderr——不回传：
    /// 会话层不该为一次码率调整阻塞等待。
    ///
    /// - `bps == 0`：本地拒绝（Err InvalidConfig），不发命令；
    /// - 命令通道（sync_channel(8)，有界）满：Err——调用方失控重发的信号；
    /// - 编码线程已退出（停机后）：Err——调用方按会话错误策略记错。
    pub fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        if bps == 0 {
            return Err(Error::InvalidConfig(
                "set_bitrate: bps must be > 0".to_string(),
            ));
        }
        self.enc_cmd
            .try_send(EncCmd::SetBitrate(bps))
            .map_err(|e| match e {
                TrySendError::Full(_) => Error::InvalidConfig(format!(
                    "encode cmd channel full (capacity 8): set_bitrate({bps}) dropped \
                     (caller is re-sending uncontrollably)"
                )),
                TrySendError::Disconnected(_) => Error::InvalidConfig(format!(
                    "encode thread exited (pipeline shut down): set_bitrate({bps}) dropped"
                )),
            })
    }

    /// 帧时间线（本端半场：Captured→Sent + 调度丢弃结局）。
    pub fn trace(&self) -> &TraceCollector {
        &self.trace
    }

    /// 调度器统计。
    pub fn sched_stats(&self) -> SchedulerStats {
        self.sched.stats()
    }

    /// 当前在途帧数（Q3/L9 观测用：会话层逐 tick 采样取最大值）。
    pub fn in_flight_len(&self) -> usize {
        self.sched.in_flight_len()
    }

    /// 是否仍有已提交未完成的编码（会话收尾排空用）。
    pub fn has_pending_encodes(&self) -> bool {
        self.sched.in_flight_len() > 0
    }

    /// 是否有尚未被 [`Self::tick`] 消费的唤醒事件（采集到达或编码完成）。
    pub fn has_pending_work(&self) -> bool {
        self.wake
            .0
            .lock()
            .map(|g| *g != self.last_wake_gen)
            .unwrap_or(false)
    }

    /// 会话收尾第一步：只停采集线程（不再有新帧进入交接通道）。
    /// 之后调用方应继续 [`Self::tick`] 到静默（交接通道空、无在途编码），
    /// 再 [`Self::shutdown`] 停编码线程。**先 shutdown 再 tick 是错的**：
    /// 交接通道里可能还压着帧，停机后的 tick 会把新编码提交给已退出的
    /// 编码线程（Disconnected），授权帧凭空消失，账目破洞。
    pub fn stop_capture(&mut self) {
        self.cap_stop.store(true, Ordering::Release);
        signal_wake(&self.cap_space);
        if let Some(h) = self.cap_handle.take() {
            let _ = h.join();
        }
    }

    /// 会话收尾：停两个线程（编码线程排空已提交请求，完成经
    /// [`Self::tick`] 收取——调用方应在停机后继续 tick 到
    /// [`Self::has_pending_encodes`] 为假，保证每帧有结局）。
    pub fn shutdown(&mut self) {
        self.cap_stop.store(true, Ordering::Release);
        signal_wake(&self.cap_space);
        self.enc_stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.cap_handle.take() {
            let _ = h.join();
        }
        if let Some(h) = self.enc_handle.take() {
            let _ = h.join();
        }
    }

    /// 处理一帧采集数据：diff → 调度 →（Encode 决策时）提交编码线程。
    fn process_frame(&mut self, frame: CapturedFrame, now_us: u32) -> Result<Option<Vec<Vec<u8>>>> {
        if self.first_capture.is_none() {
            self.first_capture = Some(std::time::Instant::now());
        }
        let is_repeat = self.judge_repeat(&frame);
        let seq = self.sched.alloc_seq();
        {
            // 坑 3：Captured 打内容变化时刻，不是采集返回时刻。
            let f = self.trace.begin(seq);
            f.mark(Mark::Captured, frame.timestamp);
            f.mark(Mark::Diffed, Timestamp(now_us));
        }

        let kind = if self.no_idr_yet || self.pending_idr {
            FrameKind::Idr
        } else {
            FrameKind::P
        };

        let decision = self
            .sched
            .on_captured(seq, kind, is_repeat, frame.timestamp, now_us);

        // P3：每次 on_captured 后检查两个关键帧来源。
        let mut want_idr = false;
        if let Decision::Drop { needs_keyframe, .. } = &decision {
            want_idr = *needs_keyframe;
        }
        if want_idr || self.sched.take_keyframe_request() {
            self.pending_idr = true;
        }

        match decision {
            Decision::Drop { reason, .. } => {
                self.finish_drop(seq, reason);
                Ok(None)
            }
            Decision::Encode { deadline_us, .. } => {
                if let Some(f) = self.trace.get_mut(seq) {
                    f.mark(Mark::EncodeStart, Timestamp(now_us));
                }
                let req = EncodeReq {
                    seq,
                    frame,
                    force_idr: self.no_idr_yet || self.pending_idr,
                    deadline: Timestamp(deadline_us),
                };
                // 提交（通道容量 2 == 在途上限；满了说明调度器算错了）。
                let slot = Mutex::new(Some(req));
                if let Err(e) = self.enc_req.try_send(slot) {
                    // 不该发生（容量 = 在途上限 + 孤儿余量）。防御性释放并
                    // 记 Budget 结局；留 stderr 痕迹便于发现账目回归。
                    // Full/Disconnected 分别指向容量计算错误与编码线程
                    // 提前退出——修的是根因，不是这里的兜底。
                    eprintln!("rc-pipeline: encode req rejected ({e:?}) despite scheduler budget");
                    self.sched.on_encoded(seq, now_us);
                    if let Some(f) = self.trace.get_mut(seq) {
                        f.finish(Outcome::DroppedBudget);
                    }
                }
                Ok(None) // 本帧的数据报在完成时（后续 tick）产出
            }
        }
    }

    /// 重复帧判定（见 [`DiffMode`]）。
    fn judge_repeat(&mut self, frame: &CapturedFrame) -> bool {
        if frame.is_repeat {
            return true;
        }
        if self.cfg.diff == DiffMode::TileDiff {
            if let FrameTexture::Cpu(cpu) = &frame.texture {
                let r: DiffResult = self.diff.update(cpu);
                return r.is_repeat;
            }
        }
        false
    }

    fn finish_drop(&mut self, seq: rc_types::FrameSeq, reason: DropReason) {
        let outcome = match reason {
            DropReason::Repeat => Outcome::DroppedRepeat,
            DropReason::Stale => Outcome::DroppedStale,
            DropReason::Budget => Outcome::DroppedBudget,
            DropReason::Superseded => Outcome::DroppedSuperseded,
        };
        if let Some(f) = self.trace.get_mut(seq) {
            f.finish(outcome);
        }
    }

    /// 假采集节流标志（诊断）。
    pub fn is_paced(&self) -> bool {
        self.pace_capture
    }

    /// 首次采集的墙钟时刻（会话零点锚点，见字段文档）。
    pub fn first_capture(&self) -> Option<std::time::Instant> {
        self.first_capture
    }
}

impl Drop for HostPipeline {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// 从数据报解析帧头（测试与控制端诊断用）。
pub fn datagram_header(buf: &[u8]) -> Result<MediaHeader> {
    let (h, _) = MediaHeader::parse_datagram(buf)?;
    Ok(h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_capture::{FakeBackend, FakeBackendConfig, FakeContent};
    use rc_encode::{BitstreamFormat, EncodedFrame, Encoder, EncoderCaps, EncoderConfig};
    use rc_types::{Resolution, Timestamp};
    use std::sync::atomic::AtomicUsize;

    /// 支持 2-deep 流水线编码的可编程假编码器：记录最大在途深度，支持按 token 触发过载丢帧。
    struct MockPipelinedEncoder {
        cfg: EncoderConfig,
        queue: VecDeque<(u64, Timestamp, bool)>,
        max_in_flight: Arc<AtomicUsize>,
        drop_token: Option<u64>,
        submitted_count: Arc<AtomicUsize>,
    }

    impl Encoder for MockPipelinedEncoder {
        fn encode(
            &mut self,
            _frame: &CapturedFrame,
            _force_idr: bool,
            _deadline_ts: Timestamp,
        ) -> Result<EncodedFrame> {
            unreachable!("pipelined encoder path should call submit_encode/recv_encoded")
        }

        fn capabilities(&self) -> EncoderCaps {
            EncoderCaps {
                codec: self.cfg.codec,
                hardware: true,
                max_resolution: Resolution::new(4096, 4096),
                bitstream_format: BitstreamFormat::AnnexB,
                profiles: &[66],
            }
        }

        fn set_bitrate(&mut self, _bps: u32) -> Result<()> {
            Ok(())
        }

        fn request_idr(&mut self) {}

        fn flush(&mut self) -> Result<Vec<EncodedFrame>> {
            Ok(Vec::new())
        }

        fn supports_pipelined_encode(&self) -> bool {
            true
        }

        fn submit_encode(
            &mut self,
            token: u64,
            frame: &CapturedFrame,
            force_idr: bool,
            _deadline_ts: Timestamp,
        ) -> Result<()> {
            assert!(
                self.queue.len() < 2,
                "HostPipeline 必须严格遵守 in_vt <= 2（当前 {}）",
                self.queue.len()
            );
            self.queue.push_back((token, frame.timestamp, force_idr));
            self.submitted_count.fetch_add(1, Ordering::SeqCst);
            let len = self.queue.len();
            self.max_in_flight.fetch_max(len, Ordering::SeqCst);
            Ok(())
        }

        fn recv_encoded(&mut self, timeout: Duration) -> Result<Option<(u64, EncodedFrame)>> {
            let Some((token, ts, is_keyframe)) = self.queue.pop_front() else {
                if !timeout.is_zero() {
                    std::thread::sleep(timeout.min(Duration::from_millis(1)));
                }
                return Ok(None);
            };
            if self.drop_token == Some(token) {
                return Err(Error::EncoderDroppedFrame {
                    msg: "simulated overload drop".into(),
                });
            }
            let nal = if is_keyframe { 0x65 } else { 0x41 };
            Ok(Some((
                token,
                EncodedFrame {
                    data: vec![0, 0, 0, 1, nal, 0xaa],
                    kind: if is_keyframe {
                        FrameKind::Idr
                    } else {
                        FrameKind::P
                    },
                    is_keyframe,
                    encode_done_ts: Timestamp(ts.0.saturating_add(1_000)),
                },
            )))
        }

        fn in_flight_count(&self) -> usize {
            self.queue.len()
        }
    }

    #[test]
    fn has_pending_work_reflects_wake_generation_changes() {
        let cap = FakeBackend::new(FakeBackendConfig {
            fps: 60,
            content: FakeContent::Static,
            ..FakeBackendConfig::default()
        })
        .expect("fake capture");
        let enc = rc_encode::FakeEncoder::new(EncoderConfig {
            resolution: Resolution::new(1920, 1080),
            fps: 60,
            ..EncoderConfig::default()
        })
        .expect("fake encoder");
        let mut pipe = HostPipeline::with_backends_paced(
            PipelineConfig {
                resolution: Resolution::new(1920, 1080),
                fps: 60,
                ..PipelineConfig::default()
            },
            Box::new(cap),
            Box::new(enc),
            true,
        )
        .expect("pipeline");

        assert!(!pipe.has_pending_work());
        signal_wake(&pipe.wake);
        assert!(pipe.has_pending_work());
        let _ = pipe.tick(1_000).expect("tick");
        assert!(!pipe.has_pending_work());
        pipe.shutdown();
    }

    #[test]
    fn pipelined_encoder_runs_2deep_handles_drops_and_drains_on_shutdown() {
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let submitted_count = Arc::new(AtomicUsize::new(0));
        let cap = FakeBackend::new(FakeBackendConfig {
            fps: 60,
            content: FakeContent::Dynamic,
            ..FakeBackendConfig::default()
        })
        .expect("fake capture");
        let enc = MockPipelinedEncoder {
            cfg: EncoderConfig {
                resolution: Resolution::new(1920, 1080),
                fps: 60,
                ..EncoderConfig::default()
            },
            queue: VecDeque::new(),
            max_in_flight: Arc::clone(&max_in_flight),
            drop_token: Some(2),
            submitted_count: Arc::clone(&submitted_count),
        };
        let mut pipe = HostPipeline::with_backends_paced(
            PipelineConfig {
                resolution: Resolution::new(1920, 1080),
                fps: 60,
                ..PipelineConfig::default()
            },
            Box::new(cap),
            Box::new(enc),
            true,
        )
        .expect("pipeline");

        let mut produced_datagrams = 0usize;
        let mut now_us = 0u32;
        for _ in 0..80 {
            if let Some(dgs) = pipe.tick(now_us).expect("tick") {
                produced_datagrams += dgs.len();
            }
            if submitted_count.load(Ordering::SeqCst) >= 5 && produced_datagrams >= 3 {
                break;
            }
            pipe.wait_event(Duration::from_millis(5));
            now_us = now_us.saturating_add(16_667);
        }
        assert!(
            produced_datagrams > 0,
            "流水线编码路径必须成功产出媒体数据报"
        );

        // 严格 S6 停机：先 stop_capture，再排空到静默，再 shutdown + 末轮收尾。
        pipe.stop_capture();
        for _ in 0..50 {
            let _ = pipe.tick(now_us).expect("drain tick");
            if !pipe.has_pending_encodes() && !pipe.has_pending_work() {
                break;
            }
            pipe.wait_event(Duration::from_millis(5));
            now_us = now_us.saturating_add(16_667);
        }
        pipe.shutdown();
        let _ = pipe.tick(now_us).expect("final drain tick");

        let frames = pipe.trace().frames();
        let finished_or_sent = frames
            .iter()
            .filter(|f| f.outcome().is_some() || f.at(Mark::Sent).is_some())
            .count();
        assert_eq!(
            finished_or_sent,
            frames.len(),
            "停机排空后所有已提交帧必须有结局或 Sent 打点"
        );
        assert!(
            max_in_flight.load(Ordering::SeqCst) <= 2,
            "VT 在途数不得超过 2"
        );
    }
}
