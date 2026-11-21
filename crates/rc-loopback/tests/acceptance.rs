//! T02 验收测试：覆盖任务卡「测试清单」全部语义，以及复核发现的缺陷回归。
//!
//! 本文件按任务卡原名补齐了 4 条此前没有同名用例的项：
//! `loss_injection_drops_expected_fraction` / `rtt_injection_delays_delivery` /
//! `jitter_stays_within_bounds` / `convergence_within_one_second`，
//! 它们都从 `Loopback::run` 的端到端视图断言（而不只是调用注入器）。
//!
//! 其余用例负责任务卡其余项（部分与 `src/` 内单测互补，注释里写明分工）：
//!
//! - 无损伤时「延迟 = 纯逻辑时钟的固定成本」，逐帧精确，不是近似；
//! - L5 的「少编码不是少采集」（已有用例只断言 `encoded == 0`）；
//! - Q4 升/降对称性；
//! - Q6 在**排队时间非零**时仍与网络段分列（已有用例只验证了排队为 0 的情形）；
//! - 限速接进管线后确实抬高延迟，且延迟不随运行时长增长（L9）；
//! - L1 报告 schema 的**结构化**断言（解析后逐个对象比对键集合，而非字符串包含）；
//! - L2 构造不一致数据后 Gate 失败、修复后恢复；
//! - P50 超限这一条判定本身；
//! - 报告是**合法 JSON**（用本文件内最小解析器验证，并验证解析器能拒绝坏 JSON）。
//!
//! 全部用逻辑时钟运行：没有 `sleep`，没有真实时钟依赖。

use std::time::Duration;

use rc_loopback::{fake, Loopback, LoopbackConfig, LoopbackReport};
use rc_trace::{Mark, Outcome};
use rc_types::Resolution;

/// 小分辨率配置：让每次运行在毫秒级完成。
fn small_cfg(fps: u32) -> LoopbackConfig {
    LoopbackConfig {
        resolution: Resolution::new(64, 64),
        fps,
        ..Default::default()
    }
}

fn run(cfg: LoopbackConfig, secs: u64) -> LoopbackReport {
    Loopback::new(cfg)
        .expect("配置合法")
        .run(Duration::from_secs(secs))
        .expect("run 不应失败")
}

/// 无损伤时延迟只由逻辑时钟上的固定成本构成。
///
/// 期望值：假编码 5ms + 假解码 3ms + 假呈现 2ms = 10ms，且**每一帧都恰好是
/// 10ms**。若实现里混入了真实时钟、抖动或未计入的排队，逐帧断言会立刻失败。
/// （`breakdown_separates_queue_from_network` 只断言了平均值之和，这里断言的是
/// 「零损伤 = 零额外延迟」这一语义本身。）
#[test]
fn fake_pipeline_produces_zero_latency_without_damage() {
    let cfg = small_cfg(60);
    let pipeline = fake::run_pipeline(&cfg, 1_000_000);

    assert!(pipeline.displayed > 0, "基线运行必须有帧上屏，实际 0 帧");

    // 逐帧检查：无损伤时每帧端到端延迟都必须等于固定成本之和。
    const FIXED_COST_US: u32 = 5_000 + 3_000 + 2_000;
    let mut checked = 0u32;
    for frame in pipeline.trace.frames() {
        if frame.outcome() != Some(Outcome::Displayed) {
            continue;
        }
        checked += 1;
        let e2e = frame
            .end_to_end_us()
            .unwrap_or_else(|| panic!("帧 {} 已上屏却没有端到端延迟", frame.seq().0));
        assert_eq!(
            e2e, FIXED_COST_US,
            "帧 {}：无损伤延迟应为编码 5000us + 解码 3000us + 呈现 2000us = {FIXED_COST_US}us，实际 {e2e}us",
            frame.seq().0
        );
    }
    assert_eq!(
        u64::from(checked),
        pipeline.displayed,
        "逐帧检查数 {checked} 与报告上屏帧数 {} 不一致",
        pipeline.displayed
    );

    let report = run(cfg, 1);
    assert_eq!(
        report.breakdown_ms.network, 0.0,
        "无损伤时 network 段必须为 0，实际 {}ms",
        report.breakdown_ms.network
    );
    assert_eq!(
        report.breakdown_ms.sched_queue, 0.0,
        "60fps 下编码器（5ms）不排队，sched_queue 必须为 0，实际 {}ms",
        report.breakdown_ms.sched_queue
    );
    assert_eq!(
        report.breakdown_ms.diff, 0.0,
        "假采集器在采集时刻同刻完成脏区判定，diff 必须为 0，实际 {}ms",
        report.breakdown_ms.diff
    );
    assert_eq!(
        report.breakdown_ms.packetize, 0.0,
        "假打包瞬时完成，packetize 必须为 0，实际 {}ms",
        report.breakdown_ms.packetize
    );
    assert_eq!(
        report.latency_p50_ms, 10.0,
        "无损伤 p50 应为 10ms，实际 {}ms",
        report.latency_p50_ms
    );
    assert_eq!(
        report.latency_p95_ms, 10.0,
        "纯逻辑时钟下 p95 也必须恰好 10ms（不允许真实时钟抖动），实际 {}ms",
        report.latency_p95_ms
    );
    assert_eq!(
        report.latency_p99_ms, 10.0,
        "纯逻辑时钟下 p99 也必须恰好 10ms（不允许真实时钟抖动），实际 {}ms",
        report.latency_p99_ms
    );
    assert_eq!(
        report.summary.unfinished, 0,
        "每帧都必须有心跳，unfinished 必须为 0，实际 {}",
        report.summary.unfinished
    );
}

/// L5：静止画面下编码帧数远小于采集帧数，且这个「少」是相对动态画面而言的。
///
/// 与 L8 用例（`static_content_encodes_zero_frames` / `static_content_reports_zero_encoded_fps`）
/// 分工不同：那两条钉死「严格等于 0」这条红线及采集帧率，本用例补的是任务卡
/// 要求的**比较性**断言——同一配置下静态与动态两份运行采集帧率一致（排除
/// 「少采集导致少编码」），而编码帧率相差一个数量级以上；同时钉死被过滤的帧
/// 必须全部记为 `dropped_repeat`，而不是混进其他丢弃原因。
#[test]
fn static_content_encodes_few_frames() {
    let dynamic_cfg = LoopbackConfig {
        resolution: Resolution::new(64, 64),
        fps: 60,
        ..Default::default()
    };
    let still_cfg = LoopbackConfig {
        static_content: true,
        ..dynamic_cfg.clone()
    };

    let dynamic = run(dynamic_cfg, 2);
    let still = run(still_cfg, 2);

    assert!(
        dynamic.captured_fps > 55.0,
        "测试前提不成立：动态运行应满帧采集，实际 {:.1}fps",
        dynamic.captured_fps
    );
    assert!(
        (still.captured_fps - dynamic.captured_fps).abs() < 1.0,
        "静止模式的采集帧率应与动态模式一致（少编码不能是少采集造成的）：动态 {:.2}fps、静态 {:.2}fps",
        dynamic.captured_fps,
        still.captured_fps
    );
    assert!(
        still.encoded_fps * 10.0 < still.captured_fps,
        "L5：静止画面编码帧数必须远小于采集帧数（< 10%），实际 encoded {:.2}fps / captured {:.2}fps",
        still.encoded_fps,
        still.captured_fps
    );
    assert!(
        still.encoded_fps * 10.0 < dynamic.encoded_fps,
        "静止与动态的编码帧率应相差一个数量级以上：静止 {:.2}fps、动态 {:.2}fps",
        still.encoded_fps,
        dynamic.encoded_fps
    );
    assert!(
        dynamic.encoded_fps > 55.0,
        "动态内容应接近满帧编码（脏区过滤不得误伤动态画面），实际 {:.2}fps",
        dynamic.encoded_fps
    );
    assert_eq!(
        still.summary.dropped_repeat, still.summary.frames,
        "静止模式下被过滤的帧必须全部记为 dropped_repeat：期望 {}，实际 {}",
        still.summary.frames, still.summary.dropped_repeat
    );
    assert_eq!(
        still.summary.unfinished, 0,
        "每帧都必须有结局：期望 unfinished = 0，实际 {}",
        still.summary.unfinished
    );
}

/// Q4：帧率上升与下降的用时对称（比值 ≤ 2:1）。
///
/// # 口径来自 `40-diagnostics.md` §4.2
/// 两边各跑一次：一次满速（测 `convergence_ms`），一次编码器 8× 慢（测
/// `degradation_ms`），要求 `max / min ≤ 2.0`。两者的帧率扫描用**同一套
/// 整 1 秒滑动窗口**（`lib.rs::scan_fps_windows`），因此比较是同一量纲。
///
/// # 为什么两边的网络条件必须相同
/// 对称性比的是「判定延迟」而不是「链路延迟」。若一侧注入 RTT 另一侧不注入，
/// 差异会来自链路而不是升降斜率。这里两边都注入同样的 20ms 单向延迟。
#[test]
fn fps_recovery_is_symmetric() {
    const ONE_WAY_US: u32 = 20_000;

    let mut rise_cfg = small_cfg(60);
    rise_cfg.inject_rtt_us = ONE_WAY_US * 2;
    let rise = run(rise_cfg, 3);

    let mut fall_cfg = small_cfg(60);
    fall_cfg.inject_rtt_us = ONE_WAY_US * 2;
    // 8× 慢：编码 40ms > 帧间隔 16.7ms，显示帧率跌到目标的一半以下。
    fall_cfg.encoder_slowdown = 8.0;
    let fall = run(fall_cfg, 3);

    let convergence = rise.convergence_ms.unwrap_or_else(|| {
        panic!(
            "上升侧必须收敛：期望 convergence_ms 非 null，实际 {:?}（displayed {:.1}fps，门槛 {:.1}）",
            rise.convergence_ms,
            rise.displayed_fps,
            rise.gate.displayed_fps_min
        )
    });
    let degradation = fall.degradation_ms.unwrap_or_else(|| {
        panic!(
            "下降侧必须观测到退化：期望 degradation_ms 非 null，实际 {:?}（displayed {:.1}fps，退化线 {:.1}）",
            fall.degradation_ms,
            fall.displayed_fps,
            f64::from(fall.config.fps) * 0.5
        )
    });

    // 两侧的公共时延一致，比较才成立。
    assert!(
        (rise.breakdown_ms.network - f64::from(ONE_WAY_US) / 1_000.0).abs() < 1.0,
        "上升侧 network 段应 ≈ 20ms，实际 {:.2}ms",
        rise.breakdown_ms.network
    );
    assert!(
        (fall.breakdown_ms.network - f64::from(ONE_WAY_US) / 1_000.0).abs() < 1.0,
        "下降侧 network 段应 ≈ 20ms，实际 {:.2}ms",
        fall.breakdown_ms.network
    );

    // 语义钉死：上升运行不应报退化，下降运行不应报收敛。
    assert!(
        rise.degradation_ms.is_none(),
        "满速上升运行不应报退化，实际 degradation_ms = {:?}",
        rise.degradation_ms
    );
    assert!(
        rise.displayed_fps >= rise.gate.displayed_fps_min,
        "上升侧显示帧率应 ≥ 目标 90%（{:.1}），实际 {:.1}",
        rise.gate.displayed_fps_min,
        rise.displayed_fps
    );
    assert!(
        fall.convergence_ms.is_none(),
        "编码器 8× 慢的运行不可能收敛到目标 90%，实际 convergence_ms = {:?}",
        fall.convergence_ms
    );
    assert!(
        fall.displayed_fps < f64::from(fall.config.fps) * 0.5,
        "下降侧显示帧率应低于目标一半（{:.1}fps），实际 {:.1}",
        f64::from(fall.config.fps) * 0.5,
        fall.displayed_fps
    );

    // 两侧都必须落在整 1 秒窗口的判定粒度内（L10 同一口径）。
    assert!(
        convergence <= 1_000,
        "上升用时必须 ≤ 1000ms，实际 {convergence}ms"
    );
    assert!(
        degradation <= 2_000,
        "下降用时应在两秒内被观测到，实际 {degradation}ms"
    );

    let ratio = (convergence.max(degradation) as f64) / (convergence.min(degradation) as f64);
    assert!(
        ratio <= 2.0,
        "Q4：升降用时比必须 ≤ 2:1，实际上升 {convergence}ms / 下降 {degradation}ms = {ratio:.2}（无升降斜率不对称的实现会远超 2）"
    );
}

/// Q6：排队延迟计入 `sched_queue`，**绝不**混进 `network`。
///
/// 已有用例只在「编码器不排队」（sched_queue = 0）时验证分列，证明力不足：
/// 若实现把排队时间并入 network 而 sched_queue 恒 0，那种用例照样通过。
/// 这里让 120fps + 2× 慢编码器产生真实排队，再对比「同一管线加/不加 40ms RTT」：
/// 排队时间应保持不变，只有 network 段与端到端延迟随 RTT 增长。
#[test]
fn latency_breakdown_separates_queue_and_network() {
    let mut base_cfg = small_cfg(120);
    base_cfg.encoder_slowdown = 2.0;
    let base = run(base_cfg.clone(), 2);

    let mut rtt_cfg = base_cfg;
    rtt_cfg.inject_rtt_us = 80_000;
    let with_rtt = run(rtt_cfg, 2);

    // 前提：确实产生了排队，否则本用例退化成「0 == 0」的假通过。
    assert!(
        base.breakdown_ms.sched_queue > 5.0,
        "测试前提不成立：2× 慢编码器 @120fps 应产生明显排队，实际 sched_queue = {:.2}ms",
        base.breakdown_ms.sched_queue
    );
    assert_eq!(
        base.breakdown_ms.network, 0.0,
        "未注入 RTT 时 network 段必须为 0，实际 {:.2}ms",
        base.breakdown_ms.network
    );
    assert!(
        (with_rtt.breakdown_ms.network - 40.0).abs() < 1.0,
        "80ms RTT 的单程 40ms 应全部计入 network 段，实际 {:.2}ms",
        with_rtt.breakdown_ms.network
    );

    // 核心断言：排队时间不随 RTT 变化，且不会被 network 段吸收。
    let queue_delta = (with_rtt.breakdown_ms.sched_queue - base.breakdown_ms.sched_queue).abs();
    assert!(
        queue_delta < 1.0,
        "注入 RTT 不应改变排队时间：无 RTT 时 {:.2}ms，有 RTT 时 {:.2}ms（差 {queue_delta:.2}ms）",
        base.breakdown_ms.sched_queue,
        with_rtt.breakdown_ms.sched_queue
    );
    assert!(
        with_rtt.breakdown_ms.sched_queue > 5.0,
        "有 RTT 时排队时间也必须独立存在，实际 {:.2}ms",
        with_rtt.breakdown_ms.sched_queue
    );

    // 端到端延迟只增加一个单程：排队没有被重复计入。
    let e2e_delta = with_rtt.latency_p50_ms - base.latency_p50_ms;
    assert!(
        (e2e_delta - 40.0).abs() < 0.5,
        "端到端 p50 应只增加 40ms（无 RTT {:.2}ms → 有 RTT {:.2}ms，差 {e2e_delta:.2}ms）",
        base.latency_p50_ms,
        with_rtt.latency_p50_ms
    );

    // 分段之和应等于端到端 p50（排队与网络分列且不重叠）。
    let bd = with_rtt.breakdown_ms;
    let sum =
        bd.diff + bd.sched_queue + bd.encode + bd.packetize + bd.network + bd.decode + bd.present;
    assert!(
        (sum - with_rtt.latency_p50_ms).abs() < 0.5,
        "分段之和 {sum:.2}ms 应等于 p50 {:.2}ms（若排队被计入 network，此式会失衡）",
        with_rtt.latency_p50_ms
    );
}

/// 限速生效：链路排队抬高 `network` 段与端到端延迟，且**延迟不随运行时长增长**。
///
/// 与 `damage::bandwidth_limit_delays_packets` 的分工：那条证明注入器按
/// 「字节/带宽」算出串行化时间（0us → 100ms → 200ms）；这条证明限速**接进了
/// 管线**——同样的采集与编码，只把 `bandwidth_bps` 调低，延迟就必须上涨。
/// 若限速只是被写进配置却没参与投递，这里会看到两份完全相同的报告。
///
/// # 为什么慢链路下会丢包
/// 30kbps 链路每帧 64 字节需 17.1ms，大于 60fps 的 16.7ms 帧间隔，链路吞吐
/// 追不上采集。**这正是 L9 要处理的情形**：发送队列（上限 2）满了就丢新包，
/// 于是延迟稳定在「队列 ≈ 2 帧 + 传输」而不是无限增长。按 Q3 的语义，
/// 丢帧是**正确**行为——无界排队才是要避免的病灶（RustDesk 的 120 帧队列）。
#[test]
fn bandwidth_limit_queues_packets() {
    let unrestricted = run(small_cfg(60), 2);

    let limited_cfg = LoopbackConfig {
        bandwidth_bps: 30_000,
        ..small_cfg(60)
    };
    let limited = run(limited_cfg.clone(), 2);

    assert_eq!(
        unrestricted.breakdown_ms.network, 0.0,
        "不限速时 network 段必须为 0，实际 {:.2}ms",
        unrestricted.breakdown_ms.network
    );
    assert!(
        limited.breakdown_ms.network > 10.0,
        "30kbps 限速应让 network 段显著增长（> 10ms），实际 {:.2}ms",
        limited.breakdown_ms.network
    );
    assert!(
        limited.latency_p50_ms > unrestricted.latency_p50_ms + 10.0,
        "限速必须抬高端到端延迟：不限速 {:.2}ms → 限速 {:.2}ms",
        unrestricted.latency_p50_ms,
        limited.latency_p50_ms
    );

    // 每帧都必须有结局；被拥塞丢掉的包如实记为 LostInNetwork。
    assert_eq!(
        limited.summary.unfinished, 0,
        "限速下每帧仍必须有结局，实际 unfinished = {}",
        limited.summary.unfinished
    );
    assert!(
        limited.displayed_fps > 0.0,
        "限速下仍应持续上屏，实际 displayed_fps = {}",
        limited.displayed_fps
    );
    assert!(
        limited.check_invariants().is_empty(),
        "限速下一致性不变式必须成立，实际 {:?}",
        limited.check_invariants()
    );

    // Q6：链路排队不应被算成调度器排队（两个队列是不同层）。
    assert_eq!(
        limited.breakdown_ms.sched_queue, 0.0,
        "60fps 正常编码器下调度器不应排队；链路排队必须只出现在 network 段，实际 sched_queue = {:.2}ms",
        limited.breakdown_ms.sched_queue
    );
    // L9：发送队列受同一上限约束。
    assert!(
        limited.max_queue_depth_seen <= limited_cfg.max_queue_depth as u32,
        "L9：限速下队列深度 {} 超过上限 {}",
        limited.max_queue_depth_seen,
        limited_cfg.max_queue_depth
    );

    // 关键：延迟**不随运行时长增长**（无界排队的病灶）。
    let long = run(limited_cfg, 20);
    let growth = long.latency_p50_ms - limited.latency_p50_ms;
    assert!(
        growth < 5.0,
        "限速下延迟不得随运行时长增长：2 秒时 {:.2}ms、20 秒时 {:.2}ms（增长 {growth:.2}ms）——增长说明发送队列无界",
        limited.latency_p50_ms,
        long.latency_p50_ms
    );
    assert!(
        long.max_queue_depth_seen <= 2,
        "长运行下队列深度仍必须 ≤ 2，实际 {}",
        long.max_queue_depth_seen
    );

    // 限速越紧，网络段越长：单调性证明限速真的按带宽折算。
    let tighter = run(
        LoopbackConfig {
            bandwidth_bps: 15_000,
            ..small_cfg(60)
        },
        2,
    );
    assert!(
        tighter.breakdown_ms.network > limited.breakdown_ms.network,
        "带宽减半后 network 段应更长：30kbps {:.2}ms → 15kbps {:.2}ms",
        limited.breakdown_ms.network,
        tighter.breakdown_ms.network
    );
}

/// L1：报告 JSON 的键名与 `40-diagnostics.md` §4 逐对象比对。
///
/// 已有测试用 `contains("\"key\"")` 断言，无法发现「键被挪到别的对象里」或
/// 「同名键重复出现」。这里解析成树后比对每个对象的**完整键集合**，
/// 并把若干字段的值与 `LoopbackReport` 逐项核对。
#[test]
fn report_json_matches_schema() {
    let mut cfg = small_cfg(60);
    cfg.inject_rtt_us = 80_000;
    let report = run(cfg, 2);
    let json = report.to_json();
    let root =
        json::parse(&json).unwrap_or_else(|e| panic!("报告不是合法 JSON：{e}\n原文：{json}"));

    // 六个顶层对象键（任务卡点名）+ §4 的四个顶层标量。
    expect_keys(
        &root,
        &[
            "config",
            "latency_ms",
            "fps",
            "frames",
            "breakdown_ms",
            "convergence_ms",
            "degradation_ms",
            "max_queue_depth_seen",
            "probe_enabled",
            "gate",
        ],
        "root",
    );
    expect_keys(
        at(&root, "config"),
        &[
            "resolution",
            "fps",
            "duration_s",
            "inject_loss",
            "inject_rtt_us",
            "bandwidth_bps",
            "encoder",
        ],
        "config",
    );
    expect_keys(
        at(&root, "latency_ms"),
        &["p50", "p95", "p99"],
        "latency_ms",
    );
    expect_keys(
        at(&root, "fps"),
        &["displayed", "encoded", "captured"],
        "fps",
    );
    expect_keys(
        at(&root, "frames"),
        &[
            "captured",
            "encoded",
            "displayed",
            "dropped_repeat",
            "dropped_stale",
            "dropped_budget",
            "dropped_superseded",
            "lost_in_network",
            "decode_errors",
            "unfinished",
        ],
        "frames",
    );
    expect_keys(
        at(&root, "breakdown_ms"),
        &[
            "diff",
            "sched_queue",
            "encode",
            "packetize",
            "network",
            "decode",
            "present",
        ],
        "breakdown_ms",
    );
    expect_keys(
        at(&root, "gate"),
        &[
            "p50_ms_max",
            "displayed_fps_min",
            "static_cpu_pct_max",
            "convergence_ms_max",
            "max_queue_depth",
            "passed",
            "failures",
        ],
        "gate",
    );

    // 值与报告对象一致（不是只断言键名存在）。
    approx(
        at(&root, "latency_ms.p50"),
        report.latency_p50_ms,
        "latency_ms.p50",
    );
    approx(
        at(&root, "latency_ms.p95"),
        report.latency_p95_ms,
        "latency_ms.p95",
    );
    approx(
        at(&root, "latency_ms.p99"),
        report.latency_p99_ms,
        "latency_ms.p99",
    );
    approx(
        at(&root, "fps.displayed"),
        report.displayed_fps,
        "fps.displayed",
    );
    approx(at(&root, "fps.encoded"), report.encoded_fps, "fps.encoded");
    approx(
        at(&root, "fps.captured"),
        report.captured_fps,
        "fps.captured",
    );
    approx(
        at(&root, "config.duration_s"),
        report.duration_s,
        "config.duration_s",
    );
    approx(
        at(&root, "breakdown_ms.network"),
        report.breakdown_ms.network,
        "breakdown_ms.network",
    );
    approx(
        at(&root, "breakdown_ms.sched_queue"),
        report.breakdown_ms.sched_queue,
        "breakdown_ms.sched_queue",
    );
    approx(
        at(&root, "breakdown_ms.encode"),
        report.breakdown_ms.encode,
        "breakdown_ms.encode",
    );
    approx(
        at(&root, "breakdown_ms.decode"),
        report.breakdown_ms.decode,
        "breakdown_ms.decode",
    );
    approx(
        at(&root, "breakdown_ms.present"),
        report.breakdown_ms.present,
        "breakdown_ms.present",
    );

    let s = &report.summary;
    for (path, expected) in [
        ("frames.captured", s.frames),
        ("frames.encoded", report.sched_stats.encoded),
        ("frames.displayed", s.displayed),
        ("frames.dropped_repeat", s.dropped_repeat),
        ("frames.dropped_stale", s.dropped_stale),
        ("frames.dropped_budget", s.dropped_budget),
        ("frames.dropped_superseded", s.dropped_superseded),
        ("frames.lost_in_network", s.lost_in_network),
        ("frames.decode_errors", s.decode_errors),
        ("frames.unfinished", s.unfinished),
    ] {
        let actual = number(&root, path);
        assert!(
            (actual - expected as f64).abs() < 0.5,
            "{path} 应为 {expected}，实际 {actual}"
        );
    }

    assert_eq!(
        text(&root, "config.resolution"),
        format!(
            "{}x{}",
            report.config.resolution.width, report.config.resolution.height
        ),
        "config.resolution 格式必须为 WxH"
    );
    assert_eq!(
        number(&root, "config.fps"),
        60.0,
        "config.fps 应为 60，实际 {}",
        number(&root, "config.fps")
    );
    assert_eq!(
        number(&root, "config.inject_rtt_us"),
        80_000.0,
        "config.inject_rtt_us 应为 80000，实际 {}",
        number(&root, "config.inject_rtt_us")
    );
    assert_eq!(
        text(&root, "config.encoder"),
        "fake",
        "未减速时 encoder 应为 \"fake\""
    );
    assert_eq!(
        flag(&root, "probe_enabled"),
        report.probe_enabled,
        "probe_enabled 应与报告对象一致（默认 {}）",
        report.probe_enabled
    );
    // 探针默认关闭，与「是否注入 RTT」无关（40-diagnostics.md §4.1）。
    assert!(
        !flag(&root, "probe_enabled"),
        "默认配置下 probe_enabled 必须是 false，即使注入了 RTT"
    );
    assert_eq!(
        flag(&root, "gate.passed"),
        report.gate.passed,
        "gate.passed 应与报告对象一致"
    );
    assert_eq!(
        number(&root, "gate.p50_ms_max"),
        report.gate.p50_ms_max,
        "gate.p50_ms_max 应为 {}，实际 {}",
        report.gate.p50_ms_max,
        number(&root, "gate.p50_ms_max")
    );
    assert_eq!(
        number(&root, "gate.convergence_ms_max"),
        report.gate.convergence_ms_max as f64,
        "gate.convergence_ms_max 应为 {}，实际 {}",
        report.gate.convergence_ms_max,
        number(&root, "gate.convergence_ms_max")
    );
    assert_eq!(
        number(&root, "gate.max_queue_depth"),
        2.0,
        "gate.max_queue_depth 应为 2，实际 {}",
        number(&root, "gate.max_queue_depth")
    );

    let convergence = report.convergence_ms.expect("3 秒 60fps 运行必须收敛");
    assert_eq!(
        convergence, 1_000,
        "整 1 秒窗口下第一个可判定的窗口结束于 1000ms，实际 {convergence}ms"
    );
    assert_eq!(
        number(&root, "convergence_ms"),
        convergence as f64,
        "convergence_ms 应为 {convergence}，实际 {}",
        number(&root, "convergence_ms")
    );
    assert!(
        matches!(at(&root, "degradation_ms"), json::Value::Null),
        "未退化的运行 degradation_ms 必须是 null，实际 {:?}",
        at(&root, "degradation_ms")
    );

    let failures = at(&root, "gate.failures")
        .as_array()
        .expect("gate.failures 必须是数组");
    assert_eq!(
        failures.len(),
        report.gate.failures.len(),
        "gate.failures 数组长度应为 {}，实际 {}",
        report.gate.failures.len(),
        failures.len()
    );
    for (i, (got, want)) in failures.iter().zip(report.gate.failures.iter()).enumerate() {
        assert_eq!(
            got.as_str()
                .unwrap_or_else(|| panic!("gate.failures[{i}] 必须是字符串")),
            want,
            "gate.failures[{i}] 内容不符"
        );
    }
}

/// L2：构造不一致的统计后，Gate 必须失败并给出说明；恢复数据后必须重新通过。
#[test]
fn invariant_violation_fails_gate() {
    let mut report = run(small_cfg(60), 1);
    assert!(
        report.gate.passed,
        "基线报告应通过 Gate，实际失败项 {:?}",
        report.gate.failures
    );
    assert!(
        report.check_invariants().is_empty(),
        "基线报告不应有不变式违规，实际 {:?}",
        report.check_invariants()
    );

    // 构造不一致：声称有 1 帧未完成，但总帧数没有相应增加。
    report.summary.unfinished += 1;
    let violations = report.check_invariants();
    assert!(
        !violations.is_empty(),
        "不一致数据必须被 check_invariants 抓到，实际返回空列表"
    );

    report.apply_gate();
    assert!(
        !report.gate.passed,
        "不变式被破坏后 Gate 必须失败，实际 passed = true、failures = {:?}",
        report.gate.failures
    );
    assert!(
        !report.gate.failures.is_empty(),
        "Gate 失败时 failures 不能为空"
    );
    assert!(
        report.gate.failures.iter().any(|f| f.contains("invariant")),
        "failures 必须指明是不变式违规，实际 {:?}",
        report.gate.failures
    );

    // 恢复：Gate 判定必须基于当前数据重新计算，不能是粘滞状态。
    report.summary.unfinished -= 1;
    report.apply_gate();
    assert!(
        report.gate.passed,
        "数据恢复后 Gate 必须重新通过，实际 failures = {:?}",
        report.gate.failures
    );
}

/// 报告必须是合法 JSON。除正例外，还验证解析器会拒绝坏 JSON，
/// 否则「解析成功」可能只是因为解析器什么都接受。
#[test]
fn report_json_parses_as_valid_json() {
    let mut damaged = small_cfg(60);
    damaged.inject_rtt_us = 80_000;
    damaged.inject_loss = 0.03;
    damaged.seed = 7;

    let cases: Vec<(&str, LoopbackReport)> = vec![
        ("正常报告", run(small_cfg(60), 1)),
        // 带损伤：gate.passed = false 且 failures 非空，覆盖字符串数组路径。
        ("损伤报告", run(damaged, 1)),
        // 空运行：convergence_ms / degradation_ms 均为 null。
        ("空运行报告", run(small_cfg(60), 0)),
        // 静止画面：encoded_fps = 0 且无上屏帧。
        (
            "静止报告",
            run(
                LoopbackConfig {
                    static_content: true,
                    ..small_cfg(60)
                },
                1,
            ),
        ),
    ];

    for (label, report) in cases {
        let json = report.to_json();
        let parsed = json::parse(&json)
            .unwrap_or_else(|e| panic!("{label}不是合法 JSON：{e}\n原文：{json}"));
        assert!(
            matches!(parsed, json::Value::Object(_)),
            "{label}的顶层必须是 JSON 对象，实际 {parsed:?}"
        );
        assert!(
            parsed.field("gate").is_some(),
            "{label}缺少 gate 对象：{json}"
        );
    }

    // 解析器有牙齿：这些都不是合法 JSON，必须被拒绝。
    for bad in [
        "{\"a\":1,}",
        "{\"a\":}",
        "{} trailing",
        "{\"a\":01}",
        "[1,]",
    ] {
        assert!(
            json::parse(bad).is_err(),
            "解析器错误地接受了非法 JSON：{bad}"
        );
    }
}

/// Gate 判定逻辑：P50 超过 `p50_ms_max` 时必须失败，且失败项恰好指向 P50。
#[test]
fn gate_fails_when_p50_exceeds() {
    let mut cfg = small_cfg(60);
    cfg.inject_rtt_us = 80_000;
    let report = run(cfg, 2);

    assert!(
        report.latency_p50_ms > report.gate.p50_ms_max,
        "测试前提不成立：p50 {:.2}ms 应超过上限 {:.1}ms",
        report.latency_p50_ms,
        report.gate.p50_ms_max
    );
    assert!(
        !report.gate.passed,
        "p50 超限时 Gate 必须失败，实际 passed = true，p50 = {:.2}ms",
        report.latency_p50_ms
    );

    // 其他门槛都满足，证明失败确实只来自 p50（判定逻辑没有连坐）。
    assert!(
        report.displayed_fps >= report.gate.displayed_fps_min,
        "本用例要求显示帧率达标：期望 ≥ {:.1}，实际 {:.1}",
        report.gate.displayed_fps_min,
        report.displayed_fps
    );
    assert!(
        report
            .convergence_ms
            .is_some_and(|ms| ms <= report.gate.convergence_ms_max),
        "本用例要求收敛达标：期望 ≤ {}ms，实际 {:?}",
        report.gate.convergence_ms_max,
        report.convergence_ms
    );
    assert!(
        report.max_queue_depth_seen <= report.gate.max_queue_depth,
        "本用例要求队列达标：期望 ≤ {}，实际 {}",
        report.gate.max_queue_depth,
        report.max_queue_depth_seen
    );
    assert!(
        report.check_invariants().is_empty(),
        "本用例要求不变式成立，实际 {:?}",
        report.check_invariants()
    );

    assert_eq!(
        report.gate.failures.len(),
        1,
        "p50 单项超限不应连带其他失败项，实际 failures = {:?}",
        report.gate.failures
    );
    let only = &report.gate.failures[0];
    assert!(
        only.contains("latency p50"),
        "失败项应描述 p50 超限，实际 {only:?}"
    );

    // 对照：同一管线不注入 RTT 时 p50 = 10ms，Gate 必须通过。
    let clean = run(small_cfg(60), 2);
    assert!(
        clean.gate.passed,
        "无损伤报告应通过 Gate，实际 p50 = {:.2}ms、failures = {:?}",
        clean.latency_p50_ms, clean.gate.failures
    );
}

/// 3% 丢包配置下，**端到端**实际丢帧比例落在 2%–4%。
///
/// 与 `damage::loss_rate_matches_config` 的分工：那条直接调注入器、数 10000 个
/// 包，证明 RNG 与伯努利判定的统计正确性；这条从 `Loopback::run` 出发，
/// 数真正进入时间线的 `lost_in_network`，证明拆包/收包路径上的丢包**没有
/// 被别的环节掩盖或重复计数**（例如被调度器提前丢掉、或丢了却没记结局）。
#[test]
fn loss_injection_drops_expected_fraction() {
    let cfg = LoopbackConfig {
        inject_loss: 0.03,
        seed: 4_242,
        // 链路必须够快，否则拥塞丢包会混进统计，掩盖随机丢包。
        bandwidth_bps: 0,
        ..small_cfg(60)
    };
    // 20 秒 = 1200 帧，期望丢 36 帧；2%–4% 的区间对应 [24, 48]。
    let report = run(cfg, 20);

    assert!(
        report.summary.frames >= 1_000,
        "样本不足：期望 ≥ 1000 帧，实际 {}",
        report.summary.frames
    );
    let lost = report.summary.lost_in_network;
    let frames = report.summary.frames;
    let rate = lost as f64 / frames as f64;
    assert!(
        (0.02..0.04).contains(&rate),
        "3% 丢包配置下实际丢帧率应在 2%–4%，实际 {:.4}（{lost}/{frames} 帧）",
        rate
    );
    assert_eq!(
        report.summary.unfinished, 0,
        "丢包也必须给每帧定案，unfinished 必须为 0，实际 {}",
        report.summary.unfinished
    );
    // 一致性不变式仍成立：丢掉的帧在 `displayed + lost + ...` 里被数到一次。
    assert!(
        report.check_invariants().is_empty(),
        "丢包下不变式必须成立，实际 {:?}",
        report.check_invariants()
    );
}

/// 注入 80ms RTT → `network` 段**每一帧**都恰为单程 40ms。
///
/// 与 `fake::rtt_injection_shows_in_breakdown`（断言平均值 ≈ 40ms）的分工：
/// 平均值会掩盖「一半帧 20ms、另一半 60ms」这种抖动型错误。这里逐帧断言
/// `Received - Sent` 恒为 40ms，且端到端 p50 = 40ms + 本地 10ms。
#[test]
fn rtt_injection_delays_delivery() {
    let mut cfg = small_cfg(60);
    cfg.inject_rtt_us = 80_000;
    let pipeline = fake::run_pipeline(&cfg, 2_000_000);

    let mut checked = 0u32;
    for frame in pipeline.trace.frames() {
        if frame.outcome() != Some(Outcome::Displayed) {
            continue;
        }
        checked += 1;
        let sent = frame
            .at(Mark::Sent)
            .unwrap_or_else(|| panic!("上屏帧 {} 缺少 Sent 标记", frame.seq().0))
            .0;
        let received = frame
            .at(Mark::Received)
            .unwrap_or_else(|| panic!("上屏帧 {} 缺少 Received 标记", frame.seq().0))
            .0;
        assert_eq!(
            received - sent,
            40_000,
            "帧 {}：80ms RTT 的单程延迟应为 40000us，实际 {}us",
            frame.seq().0,
            received - sent
        );
    }
    assert!(
        checked > 100,
        "测试应覆盖足够多帧，实际只检查了 {checked} 帧"
    );

    let report = run(cfg, 2);
    assert!(
        (report.breakdown_ms.network - 40.0).abs() < 0.01,
        "network 段均值应为 40.00ms，实际 {:.2}ms",
        report.breakdown_ms.network
    );
    assert!(
        (report.latency_p50_ms - 50.0).abs() < 0.01,
        "端到端 p50 应为 40ms 网络 + 10ms 本地 = 50.00ms，实际 {:.2}ms",
        report.latency_p50_ms
    );
    assert_eq!(
        report.summary.displayed, report.summary.frames,
        "延迟不丢帧：期望全部 {} 帧上屏，实际 {}",
        report.summary.frames, report.summary.displayed
    );
}

/// 抖动不超过配置值：每帧延迟落在 `[rtt/2, rtt/2 + jitter]`，且确实用满了区间。
///
/// 与 `damage::jitter_within_bounds` 的分工：那条在注入器层面检查
/// `deliver_at`；这条从时间线检查 `Received - Sent` 分段，证明抖动没有被
/// 队列、打包或统计环节吞掉或放大。区间两端都要求出现，否则「抖动恒为 0」
/// 也能通过「不超过上限」的断言。
#[test]
fn jitter_stays_within_bounds() {
    const ONE_WAY_US: u32 = 40_000;
    const JITTER_US: u32 = 5_000;

    let mut cfg = small_cfg(60);
    cfg.inject_rtt_us = ONE_WAY_US * 2;
    cfg.inject_jitter_us = JITTER_US;
    cfg.seed = 99;
    let pipeline = fake::run_pipeline(&cfg, 2_000_000);

    let mut min = u32::MAX;
    let mut max = 0u32;
    let mut checked = 0u32;
    for frame in pipeline.trace.frames() {
        if frame.outcome() != Some(Outcome::Displayed) {
            continue;
        }
        let sent = frame.at(Mark::Sent).unwrap().0;
        let received = frame.at(Mark::Received).unwrap().0;
        let delay = received - sent;
        checked += 1;
        assert!(
            (ONE_WAY_US..=ONE_WAY_US + JITTER_US).contains(&delay),
            "帧 {}：抖动后的单程延迟应在 [{ONE_WAY_US}, {}]us 内，实际 {delay}us",
            frame.seq().0,
            ONE_WAY_US + JITTER_US
        );
        min = min.min(delay);
        max = max.max(delay);
    }
    assert!(checked > 100, "样本不足：只检查了 {checked} 帧");
    // 1000+ 个样本下，均匀采样几乎必然覆盖两端（漏掉任一侧的概率 < 1e-8）。
    assert!(
        min <= ONE_WAY_US + 100,
        "抖动下界没有被采到：期望出现接近 {ONE_WAY_US}us 的延迟，实际最小 {min}us"
    );
    assert!(
        max >= ONE_WAY_US + JITTER_US - 100,
        "抖动上界没有被采到：期望出现接近 {}us 的延迟，实际最大 {max}us",
        ONE_WAY_US + JITTER_US
    );
}

/// L10：`convergence_ms ≤ 1000`，且口径是**整 1 秒滑动窗口**（不是瞬时速率）。
///
/// 除门槛本身，这里还钉死三个容易走样的性质：
/// 1. 满速 60fps 运行的收敛值必须恰好落在第一个满窗口（1000ms）——若实现用
///    短跨度窗口凑数，会给出 80ms 之类的值；
/// 2. 运行不足 1 秒时无法判定，必须报 `null`（而不是编一个数）；
/// 3. 30 秒内未达标必须是 `null`。
#[test]
fn convergence_within_one_second() {
    let report = run(small_cfg(60), 2);
    let convergence = report.convergence_ms.unwrap_or_else(|| {
        panic!(
            "60fps 满速运行必须收敛，实际 null（displayed {:.1}fps）",
            report.displayed_fps
        )
    });
    assert!(
        convergence <= 1_000,
        "L10：convergence_ms 必须 ≤ 1000，实际 {convergence}ms"
    );
    // 1 秒窗口口径：第一个可判定的窗口结束于 1000ms，因此满速运行的收敛值就是 1000。
    assert_eq!(
        convergence, 1_000,
        "整 1 秒窗口下满速运行的 convergence_ms 必须是 1000ms（更小说明用了短跨度窗口），实际 {convergence}ms"
    );
    assert!(
        report.gate.passed,
        "满速运行的 Gate 必须通过，实际 failures = {:?}",
        report.gate.failures
    );

    // 不足 1 秒：拿不到满窗口，诚实报 null。
    let short = run(small_cfg(60), 0);
    assert!(
        short.convergence_ms.is_none(),
        "空运行无法收敛，convergence_ms 必须是 null，实际 {:?}",
        short.convergence_ms
    );

    // 从未达标 → null（30 秒规则的「永不达标」一侧；「31 秒才达标」那一侧需要
    // 合成时间线，见 `lib.rs::convergence_times_out_after_thirty_seconds`）。
    // 8× 慢编码器的显示帧率约为目标 40%，永远达不到 90%。
    let slow = run(
        LoopbackConfig {
            encoder_slowdown: 8.0,
            ..small_cfg(60)
        },
        5,
    );
    assert!(
        slow.convergence_ms.is_none(),
        "从未达到目标 90% 时必须报 null，实际 convergence_ms = {:?}（displayed {:.1}fps）",
        slow.convergence_ms,
        slow.displayed_fps
    );
    assert!(
        !slow.gate.passed,
        "未收敛必须让 Gate 失败，实际 passed = true"
    );
    assert!(
        slow.gate
            .failures
            .iter()
            .any(|f| f.contains("convergence_ms is null")),
        "失败项必须说明是 convergence null，实际 {:?}",
        slow.gate.failures
    );

    // 满速运行的收敛必须真的发生在 1 秒内（L10 的实质要求）。
    assert!(
        report.latency_p50_ms.is_finite() && report.displayed_fps > 59.0,
        "满速运行的 p50 与帧率都应正常：p50={:.2}ms、fps={:.1}",
        report.latency_p50_ms,
        report.displayed_fps
    );
}

/// 断言 JSON 对象 `value` 的键集合恰好是 `expected`（顺序无关）。
fn expect_keys(value: &json::Value, expected: &[&str], path: &str) {
    let mut actual: Vec<&str> = value
        .object_entries()
        .iter()
        .map(|(k, _)| k.as_str())
        .collect();
    actual.sort_unstable();
    let mut want: Vec<&str> = expected.to_vec();
    want.sort_unstable();
    assert_eq!(
        actual, want,
        "{path} 的键集合不符：期望 {want:?}，实际 {actual:?}"
    );
}

/// 按 `a.b.c` 路径取字段，缺失时给出完整路径。
fn at<'a>(root: &'a json::Value, path: &str) -> &'a json::Value {
    let mut current = root;
    for part in path.split('.') {
        current = current
            .field(part)
            .unwrap_or_else(|| panic!("JSON 缺少字段 {path}（在 {part} 处中断）"));
    }
    current
}

fn number(root: &json::Value, path: &str) -> f64 {
    at(root, path)
        .as_f64()
        .unwrap_or_else(|| panic!("{path} 必须是 JSON 数字，实际 {:?}", at(root, path)))
}

fn approx(value: &json::Value, expected: f64, path: &str) {
    let actual = value
        .as_f64()
        .unwrap_or_else(|| panic!("{path} 必须是 JSON 数字，实际 {value:?}"));
    assert!(
        (actual - expected).abs() < 1e-6,
        "{path} 应为 {expected}，实际 {actual}"
    );
}

fn text<'a>(root: &'a json::Value, path: &str) -> &'a str {
    at(root, path)
        .as_str()
        .unwrap_or_else(|| panic!("{path} 必须是 JSON 字符串，实际 {:?}", at(root, path)))
}

fn flag(root: &json::Value, path: &str) -> bool {
    at(root, path)
        .as_bool()
        .unwrap_or_else(|| panic!("{path} 必须是 JSON 布尔值，实际 {:?}", at(root, path)))
}

/// 最小 JSON 解析器（仅用于测试）。
///
/// 覆盖 RFC 8259 的子集：对象、数组、字符串（含转义）、数字、`true/false/null`。
/// 严格拒绝尾随数据、多余逗号、前导零等——这样「解析成功」才真正说明报告合法。
mod json {
    /// 解析结果。
    #[derive(Debug, Clone, PartialEq)]
    pub enum Value {
        /// `null`
        Null,
        /// `true` / `false`
        Bool(bool),
        /// 数字（统一按 `f64` 保存）
        Number(f64),
        /// 字符串
        Str(String),
        /// 数组
        Array(Vec<Value>),
        /// 对象（保留键顺序）
        Object(Vec<(String, Value)>),
    }

    impl Value {
        /// 取对象字段；非对象或字段缺失返回 `None`。
        pub fn field(&self, key: &str) -> Option<&Value> {
            match self {
                Value::Object(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
                _ => None,
            }
        }

        /// 取对象的键值对列表。
        pub fn object_entries(&self) -> &[(String, Value)] {
            match self {
                Value::Object(entries) => entries,
                other => panic!("期望 JSON 对象，实际 {other:?}"),
            }
        }

        /// 取数字。
        pub fn as_f64(&self) -> Option<f64> {
            match self {
                Value::Number(n) => Some(*n),
                _ => None,
            }
        }

        /// 取布尔值。
        pub fn as_bool(&self) -> Option<bool> {
            match self {
                Value::Bool(b) => Some(*b),
                _ => None,
            }
        }

        /// 取字符串。
        pub fn as_str(&self) -> Option<&str> {
            match self {
                Value::Str(s) => Some(s.as_str()),
                _ => None,
            }
        }

        /// 取数组。
        pub fn as_array(&self) -> Option<&[Value]> {
            match self {
                Value::Array(items) => Some(items.as_slice()),
                _ => None,
            }
        }
    }

    /// 解析完整 JSON 文本；有任何多余字符都视为错误。
    pub fn parse(input: &str) -> Result<Value, String> {
        let mut parser = Parser {
            src: input.as_bytes(),
            pos: 0,
        };
        let value = parser.value()?;
        parser.skip_ws();
        if parser.pos == parser.src.len() {
            Ok(value)
        } else {
            Err(format!("第 {} 字节处有多余字符", parser.pos))
        }
    }

    struct Parser<'a> {
        src: &'a [u8],
        pos: usize,
    }

    impl<'a> Parser<'a> {
        fn peek(&self) -> Option<u8> {
            self.src.get(self.pos).copied()
        }

        fn skip_ws(&mut self) {
            while let Some(b) = self.peek() {
                if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                    self.pos += 1;
                } else {
                    break;
                }
            }
        }

        fn expect(&mut self, byte: u8) -> Result<(), String> {
            if self.peek() == Some(byte) {
                self.pos += 1;
                Ok(())
            } else {
                Err(format!(
                    "第 {} 字节处期望 '{}'，实际 {:?}",
                    self.pos,
                    byte as char,
                    self.peek().map(|b| b as char)
                ))
            }
        }

        fn value(&mut self) -> Result<Value, String> {
            self.skip_ws();
            match self.peek() {
                None => Err("输入意外结束".to_string()),
                Some(b'{') => self.object(),
                Some(b'[') => self.array(),
                Some(b'"') => self.string().map(Value::Str),
                Some(b't') => self.keyword("true", Value::Bool(true)),
                Some(b'f') => self.keyword("false", Value::Bool(false)),
                Some(b'n') => self.keyword("null", Value::Null),
                Some(b'-' | b'0'..=b'9') => self.number().map(Value::Number),
                Some(other) => Err(format!("第 {} 字节处出现非法字符 0x{other:02x}", self.pos)),
            }
        }

        fn keyword(&mut self, word: &str, value: Value) -> Result<Value, String> {
            if self.src[self.pos..].starts_with(word.as_bytes()) {
                self.pos += word.len();
                Ok(value)
            } else {
                Err(format!("第 {} 字节处期望 {word}", self.pos))
            }
        }

        fn object(&mut self) -> Result<Value, String> {
            self.expect(b'{')?;
            let mut entries = Vec::new();
            self.skip_ws();
            if self.peek() == Some(b'}') {
                self.pos += 1;
                return Ok(Value::Object(entries));
            }
            loop {
                self.skip_ws();
                let key = self.string()?;
                self.skip_ws();
                self.expect(b':')?;
                let value = self.value()?;
                entries.push((key, value));
                self.skip_ws();
                match self.peek() {
                    Some(b',') => self.pos += 1,
                    Some(b'}') => {
                        self.pos += 1;
                        return Ok(Value::Object(entries));
                    }
                    other => {
                        return Err(format!(
                            "第 {} 字节处期望 ',' 或 '}}'，实际 {:?}",
                            self.pos,
                            other.map(|b| b as char)
                        ))
                    }
                }
            }
        }

        fn array(&mut self) -> Result<Value, String> {
            self.expect(b'[')?;
            let mut items = Vec::new();
            self.skip_ws();
            if self.peek() == Some(b']') {
                self.pos += 1;
                return Ok(Value::Array(items));
            }
            loop {
                items.push(self.value()?);
                self.skip_ws();
                match self.peek() {
                    Some(b',') => self.pos += 1,
                    Some(b']') => {
                        self.pos += 1;
                        return Ok(Value::Array(items));
                    }
                    other => {
                        return Err(format!(
                            "第 {} 字节处期望 ',' 或 ']'，实际 {:?}",
                            self.pos,
                            other.map(|b| b as char)
                        ))
                    }
                }
            }
        }

        fn string(&mut self) -> Result<String, String> {
            self.expect(b'"')?;
            let mut out: Vec<u8> = Vec::new();
            loop {
                let byte = self.peek().ok_or_else(|| "字符串未闭合".to_string())?;
                self.pos += 1;
                match byte {
                    b'"' => {
                        return String::from_utf8(out)
                            .map_err(|e| format!("字符串不是合法 UTF-8：{e}"))
                    }
                    b'\\' => {
                        let escape = self.peek().ok_or_else(|| "转义序列未结束".to_string())?;
                        self.pos += 1;
                        match escape {
                            b'"' => out.push(b'"'),
                            b'\\' => out.push(b'\\'),
                            b'/' => out.push(b'/'),
                            b'b' => out.push(0x08),
                            b'f' => out.push(0x0c),
                            b'n' => out.push(b'\n'),
                            b'r' => out.push(b'\r'),
                            b't' => out.push(b'\t'),
                            b'u' => {
                                let code = self.hex4()?;
                                let ch = char::from_u32(u32::from(code))
                                    .ok_or_else(|| format!("不支持的 \\u 转义 {code:04x}"))?;
                                let mut buf = [0u8; 4];
                                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                            }
                            other => return Err(format!("非法转义 \\{}", other as char)),
                        }
                    }
                    b if b < 0x20 => return Err(format!("字符串中出现未转义的控制字符 0x{b:02x}")),
                    b => out.push(b),
                }
            }
        }

        fn hex4(&mut self) -> Result<u16, String> {
            let end = self.pos + 4;
            let digits = self
                .src
                .get(self.pos..end)
                .ok_or_else(|| "\\u 转义长度不足 4 位".to_string())?;
            let text =
                std::str::from_utf8(digits).map_err(|e| format!("\\u 转义不是 ASCII：{e}"))?;
            let code = u16::from_str_radix(text, 16)
                .map_err(|e| format!("\\u 转义 {text:?} 非法：{e}"))?;
            self.pos = end;
            Ok(code)
        }

        fn number(&mut self) -> Result<f64, String> {
            let start = self.pos;
            if self.peek() == Some(b'-') {
                self.pos += 1;
            }
            match self.peek() {
                Some(b'0') => self.pos += 1,
                Some(b'1'..=b'9') => {
                    while matches!(self.peek(), Some(b'0'..=b'9')) {
                        self.pos += 1;
                    }
                }
                _ => return Err(format!("第 {start} 字节处的数字缺少整数部分")),
            }
            if self.peek() == Some(b'.') {
                self.pos += 1;
                if !matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(format!("第 {} 字节处小数点后缺少数字", self.pos));
                }
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            if matches!(self.peek(), Some(b'e' | b'E')) {
                self.pos += 1;
                if matches!(self.peek(), Some(b'+' | b'-')) {
                    self.pos += 1;
                }
                if !matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(format!("第 {} 字节处指数缺少数字", self.pos));
                }
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            let text = std::str::from_utf8(&self.src[start..self.pos])
                .map_err(|e| format!("数字不是 ASCII：{e}"))?;
            text.parse::<f64>()
                .map_err(|e| format!("数字 {text:?} 无法解析：{e}"))
        }
    }
}
