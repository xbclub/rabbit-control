//! `HostPipeline::set_bitrate`（M2-10 / T15）回归：命令通道到编码线程
//! 的异步生效（≤5ms 量级）+ 既有 tick / IDR 语义零扰动（M1 基线保护）。
//!
//! 全部假后端；真实线程的有界等待与 m1.rs 既有线程测试同一形态。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rc_capture::{CaptureBackend, CapturedFrame, FakeBackend, FakeBackendConfig, FakeContent};
use rc_encode::{EncodedFrame, Encoder, EncoderCaps, EncoderConfig, FakeEncoder};
use rc_pipeline::host::datagram_header;
use rc_pipeline::{DiffMode, HostPipeline, PipelineConfig};
use rc_types::{Error, Result, Timestamp};

fn cfg() -> PipelineConfig {
    PipelineConfig {
        resolution: rc_types::Resolution::new(64, 64),
        fps: 30,
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

fn capture(content: FakeContent) -> Box<dyn CaptureBackend> {
    Box::new(
        FakeBackend::new(FakeBackendConfig {
            content,
            ..FakeBackendConfig::default()
        })
        .expect("fake capture"),
    )
}

/// 记录型编码器：包装 FakeEncoder，`set_bitrate` 成功后把下发值写入
/// 共享记录（m1.rs 包装编码器先例，69-91 行）。
struct RecordingEncoder {
    inner: Box<dyn Encoder>,
    bitrates: Arc<Mutex<Vec<u32>>>,
}

impl Encoder for RecordingEncoder {
    fn encode(
        &mut self,
        frame: &CapturedFrame,
        force_idr: bool,
        deadline_ts: Timestamp,
    ) -> Result<EncodedFrame> {
        self.inner.encode(frame, force_idr, deadline_ts)
    }
    fn capabilities(&self) -> EncoderCaps {
        self.inner.capabilities()
    }
    fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        self.inner.set_bitrate(bps)?; // 0/超范围在编码器侧先被拒
        self.bitrates.lock().expect("record lock").push(bps);
        Ok(())
    }
    fn request_idr(&mut self) {
        self.inner.request_idr()
    }
    fn flush(&mut self) -> Result<Vec<EncodedFrame>> {
        self.inner.flush()
    }
}

fn recording_encoder() -> (Box<dyn Encoder>, Arc<Mutex<Vec<u32>>>) {
    let bitrates = Arc::new(Mutex::new(Vec::new()));
    let inner: Box<dyn Encoder> = Box::new(
        FakeEncoder::new(EncoderConfig {
            resolution: rc_types::Resolution::new(64, 64),
            fps: 30,
            bitrate_bps: 1_000_000,
            ..EncoderConfig::default()
        })
        .expect("fake encoder"),
    );
    (
        Box::new(RecordingEncoder {
            inner,
            bitrates: Arc::clone(&bitrates),
        }),
        bitrates,
    )
}

/// 驱动到下一个数据报产出（真实线程 + wait_event 挂起，与 rc-session
/// host_session 测试同一手法）。返回产出该帧的数据报。
fn drive_until_datagram(host: &mut HostPipeline, max_rounds: usize) -> Vec<Vec<u8>> {
    for _ in 0..max_rounds {
        if let Some(dgs) = host.tick(0).expect("tick") {
            if !dgs.is_empty() {
                return dgs;
            }
        }
        host.wait_event(Duration::from_millis(2));
    }
    panic!("drive_until_datagram：{max_rounds} 轮内无数据报（管线饿死）");
}

/// `set_bitrate(3_500_000)` → 编码线程在 ≤2s（实际 ≤5ms 量级：命令在
/// recv_timeout 周期内被排空）内应用并调用 `Encoder::set_bitrate`。
/// 有界轮询共享记录（真实线程等待，与 m1.rs 同形态）。
#[test]
fn set_bitrate_reaches_encoder_thread() {
    let (enc, bitrates) = recording_encoder();
    let mut host =
        HostPipeline::with_backends_paced(cfg(), capture(FakeContent::Dynamic), enc, true)
            .expect("pipeline");
    host.set_bitrate(3_500_000).expect("set_bitrate 入队成功");

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if bitrates.lock().expect("record lock").contains(&3_500_000) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "编码线程 2s 内未应用 set_bitrate（记录：{:?}）",
            bitrates.lock().expect("record lock")
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    host.shutdown();
}

/// `bps == 0` → 本地拒绝（Err InvalidConfig），不发命令——编码器记录
/// 不增（给编码线程一个反证窗口后仍为空）。
#[test]
fn set_bitrate_zero_rejected_without_command() {
    let (enc, bitrates) = recording_encoder();
    let mut host =
        HostPipeline::with_backends_paced(cfg(), capture(FakeContent::Static), enc, true)
            .expect("pipeline");
    let err = host.set_bitrate(0).expect_err("bps=0 必须本地拒绝");
    assert!(matches!(err, Error::InvalidConfig(_)), "{err}");
    // 反证窗口：若命令被发出，5ms 周期的编码线程此刻早已应用。
    std::thread::sleep(Duration::from_millis(20));
    assert!(
        bitrates.lock().expect("record lock").is_empty(),
        "拒绝路径不得向编码线程发命令"
    );
    host.shutdown();
}

/// 设置码率前后 `tick` 照常产出数据报、`request_idr` 后下一帧仍是 IDR
/// （M1 语义回归保护）。
#[test]
fn set_bitrate_does_not_disturb_tick_or_idr() {
    let (enc, _bitrates) = recording_encoder();
    let mut host =
        HostPipeline::with_backends_paced(cfg(), capture(FakeContent::Dynamic), enc, true)
            .expect("pipeline");

    // 首帧天然 IDR；等第二帧数据报并确认其为 P（测试前提）。
    let first = drive_until_datagram(&mut host, 400);
    assert!(
        datagram_header(&first[0]).expect("header").is_idr(),
        "首帧必须是 IDR"
    );
    let second = drive_until_datagram(&mut host, 400);
    assert!(
        !datagram_header(&second[0]).expect("header").is_idr(),
        "前提：次帧应为 P 帧"
    );

    // 设置码率 + 请求 IDR：下一帧必须照常产出且编成 IDR。
    host.set_bitrate(2_000_000).expect("set_bitrate");
    host.request_idr();
    let third = drive_until_datagram(&mut host, 400);
    assert!(
        !third.is_empty(),
        "set_bitrate 后 tick 照常产出数据报（S7）"
    );
    assert!(
        datagram_header(&third[0]).expect("header").is_idr(),
        "request_idr 后下一帧必须是 IDR（pending_idr 粘住）"
    );
    host.shutdown();
}

/// `shutdown()` 后调用 → Err 不 panic（命令通道 Disconnected）。
#[test]
fn set_bitrate_after_shutdown_errors_cleanly() {
    let (enc, _bitrates) = recording_encoder();
    let mut host =
        HostPipeline::with_backends_paced(cfg(), capture(FakeContent::Static), enc, true)
            .expect("pipeline");
    host.shutdown();
    let res = host.set_bitrate(1_000_000);
    let err = res.expect_err("停机后 set_bitrate 必须报错");
    assert!(matches!(err, Error::InvalidConfig(_)), "{err}");
}
