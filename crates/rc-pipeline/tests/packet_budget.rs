//! 分片预算参数化回归（N2 数据报预算，T18 空档的修复验证）。
//!
//! - [`PipelineConfig::max_packet_payload`] 生效到 `split_frame`：预算
//!   1120（= rc-net `max_media_payload(DEFAULT_MTU)`，数据报 32B 头 +
//!   1120B 片 = 1152B = `mtu - QUIC_OVERHEAD`）时，100KB 帧分出的每条
//!   数据报 ≤ 1152B——真 QUIC `send_datagram` 不再拒收；
//! - 默认配置（1200）行为逐字节不变（M1 兼容性锁定）；
//! - 构造期拒绝非法预算（0 / > [`MAX_PAYLOAD`]），错误带字段名。
//!
//! 全假后端、墙钟会话钟（m1.rs 同一形态）；rc-pipeline 不依赖 rc-net，
//! 1152/1120 在此按 N2 定义（`1200 - 48(QUIC 开销)` / `1152 - 32(包头)`）
//! 写成常数。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rc_capture::{CaptureBackend, FakeBackend, FakeBackendConfig, FakeContent};
use rc_encode::{Encoder, EncoderConfig, FakeEncoder};
use rc_pipeline::host::datagram_header;
use rc_pipeline::{DiffMode, HostPipeline, PipelineConfig, MAX_PAYLOAD};
use rc_proto::{split_frame, HEADER_LEN};
use rc_types::{FrameKind, Resolution};

/// rc-net N2 数据报预算：32B 包头 + 媒体片 ≤ 1200 - 48（QUIC 开销）。
const N2_DATAGRAM_BUDGET: usize = 1152;
/// 会话层应采用的标准分片预算：`max_media_payload(1200)` = 1152 - 32。
const SESSION_MEDIA_BUDGET: usize = 1120;
/// 测试帧载荷：假编码器合成输出 100 KB（远超单片预算，必为多包帧）。
const FRAME_BYTES: usize = 100_000;

fn cfg(max_packet_payload: usize) -> PipelineConfig {
    PipelineConfig {
        resolution: Resolution::new(64, 64),
        fps: 60,
        bitrate_bps: 1_000_000,
        prefer_hardware: false,
        target_latency_us: 22_000,
        trace_capacity: 1024,
        diff: DiffMode::TrustPlatform,
        clock_origin_us: None,
        max_packet_payload,
    }
}

fn fake_capture() -> Box<dyn CaptureBackend> {
    Box::new(
        FakeBackend::new(FakeBackendConfig {
            fps: 60,
            content: FakeContent::Dynamic,
            ..FakeBackendConfig::default()
        })
        .expect("fake backend config valid"),
    )
}

/// 假编码器：输出恒 100 KB（首帧 IDR 的合成 Annex-B，FakeEncoder 格式）。
fn big_encoder() -> Box<dyn Encoder> {
    Box::new(
        FakeEncoder::with_tuning(
            EncoderConfig {
                resolution: Resolution::new(64, 64),
                fps: 60,
                bitrate_bps: 1_000_000,
                ..EncoderConfig::default()
            },
            5_000,
            FRAME_BYTES,
        )
        .expect("fake encoder config valid"),
    )
}

/// 墙钟会话钟（微秒，m1.rs 同款）。
fn session_clock() -> Arc<dyn Fn() -> u32 + Send + Sync> {
    let start = Instant::now();
    Arc::new(move || u32::try_from(start.elapsed().as_micros()).unwrap_or(u32::MAX))
}

/// 跑管线到收满第一个完整帧（同 `frame_seq` 的实收数 == 声明 `packet_cnt`），
/// 返回按 `packet_idx` 排序的数据报组。同一 tick 批可能混入多帧，按
/// `frame_seq` 分组后取最小序号（最早帧）的完整组。
fn first_frame_datagrams(budget: usize) -> Vec<Vec<u8>> {
    let clock = session_clock();
    let mut host = HostPipeline::with_backends_paced(
        cfg(budget),
        fake_capture(),
        big_encoder(),
        true, // 假采集必须节流（坑 4）
    )
    .expect("host pipeline ok");
    host.set_clock(Arc::clone(&clock));

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(datagrams) = host.tick(clock()).expect("tick ok") {
            let mut groups: BTreeMap<u64, BTreeMap<u16, Vec<u8>>> = BTreeMap::new();
            for d in &datagrams {
                let h = datagram_header(d).expect("datagram header");
                groups
                    .entry(h.frame_seq.0)
                    .or_default()
                    .insert(h.packet_idx, d.clone());
            }
            // 找到「实收数 == packet_cnt 且 idx 连续」的最小序号完整组。
            let mut declared: BTreeMap<u64, u16> = BTreeMap::new();
            for d in &datagrams {
                let h = datagram_header(d).expect("datagram header");
                declared.insert(h.frame_seq.0, h.packet_cnt);
            }
            for (&seq, pkts) in &groups {
                let cnt = declared[&seq] as usize;
                if pkts.len() == cnt && (0..cnt).all(|i| pkts.contains_key(&(i as u16))) {
                    host.shutdown();
                    return (0..cnt).map(|i| pkts[&(i as u16)].clone()).collect();
                }
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("5s 内未收满任何完整帧（预算 {budget}）");
}

/// FakeEncoder 首帧 IDR 的合成输出（期望载荷）。
fn expected_synthetic(is_idr: bool) -> Vec<u8> {
    let mut data = vec![0u8; FRAME_BYTES];
    data[..4].copy_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    data[4] = if is_idr { 0x65 } else { 0x41 };
    data
}

/// 预算 1120：100KB 帧分出的每条数据报 ≤ 1152B（N2），分片几何精确。
#[test]
fn budget_1120_caps_every_datagram_at_n2_budget() {
    let dgrams = first_frame_datagrams(SESSION_MEDIA_BUDGET);
    // 100_000 = 89×1120 + 320 → 90 包。
    assert_eq!(
        dgrams.len(),
        FRAME_BYTES.div_ceil(SESSION_MEDIA_BUDGET),
        "分片数必须等于 ceil(载荷/预算)"
    );
    for (i, d) in dgrams.iter().enumerate() {
        assert!(
            d.len() <= N2_DATAGRAM_BUDGET,
            "数据报 #{i} 长 {} 超过 N2 预算 {N2_DATAGRAM_BUDGET}",
            d.len()
        );
        if i + 1 < dgrams.len() {
            assert_eq!(d.len(), HEADER_LEN + SESSION_MEDIA_BUDGET, "满片几何");
        }
    }
    // 末片 = 32 + (100_000 - 89×1120) = 352。
    assert_eq!(
        dgrams.last().unwrap().len(),
        HEADER_LEN + (FRAME_BYTES - 89 * SESSION_MEDIA_BUDGET)
    );
    // 逐字节重组 == 编码器合成输出。
    let h0 = datagram_header(&dgrams[0]).expect("header");
    let mut payload = Vec::with_capacity(FRAME_BYTES);
    for d in &dgrams {
        payload.extend_from_slice(&d[HEADER_LEN..]);
    }
    assert_eq!(
        payload,
        expected_synthetic(h0.flags.is_idr()),
        "重组载荷必须与编码输出逐字节一致"
    );
}

/// 默认配置（1200 预算）：与 `split_frame(.., MAX_PAYLOAD)` 逐字节相同
/// ——M1 兼容性锁定。
#[test]
fn default_budget_matches_m1_byte_for_byte() {
    assert_eq!(PipelineConfig::default().max_packet_payload, MAX_PAYLOAD);
    let dgrams = first_frame_datagrams(MAX_PAYLOAD);
    let h0 = datagram_header(&dgrams[0]).expect("header");
    // 与 rc-proto 用 MAX_PAYLOAD 直接分片的结果逐字节比对。
    let kind = if h0.flags.is_idr() {
        FrameKind::Idr
    } else {
        FrameKind::P
    };
    let reference = split_frame(
        h0.frame_seq,
        kind,
        h0.capture_ts,
        h0.encode_done_ts,
        h0.fec_group,
        &expected_synthetic(h0.flags.is_idr()),
        MAX_PAYLOAD,
    );
    assert_eq!(
        dgrams, reference,
        "默认预算的产出必须与 M1 既有行为逐字节一致"
    );
    // 几何：100_000 = 83×1200 + 400 → 84 包，满片 1232B。
    assert_eq!(dgrams.len(), FRAME_BYTES.div_ceil(MAX_PAYLOAD));
    assert_eq!(dgrams[0].len(), HEADER_LEN + MAX_PAYLOAD);
    assert_eq!(
        dgrams.last().unwrap().len(),
        HEADER_LEN + (FRAME_BYTES - 83 * MAX_PAYLOAD)
    );
}

/// 构造期拒绝非法预算：0 与 > MAX_PAYLOAD 都在管线构造时给出带字段名
/// 的 InvalidConfig（而不是运行期 split_frame 的 assert panic）。
#[test]
fn constructor_rejects_invalid_budget() {
    for bad in [0usize, MAX_PAYLOAD + 1] {
        let built =
            HostPipeline::with_backends_paced(cfg(bad), fake_capture(), big_encoder(), true);
        let err = match built {
            Err(e) => e,
            Ok(_) => panic!("非法预算 {bad} 必须被构造期拒绝"),
        };
        assert!(
            matches!(&err, rc_types::Error::InvalidConfig(msg) if msg.contains("max_packet_payload")),
            "预算 {bad} 的错误应带字段名，实际 {err:?}"
        );
    }
    // 会话层预算（1120）合法。
    HostPipeline::with_backends_paced(
        cfg(SESSION_MEDIA_BUDGET),
        fake_capture(),
        big_encoder(),
        true,
    )
    .expect("1120 预算合法");
}
