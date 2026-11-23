//! RabbitControl 的 VideoToolbox 硬件解码后端（任务卡 T05，D1–D5 解码侧）。
//!
//! # 职责
//! 用 `VTDecompressionSession` 实现 [`rc_decode::Decoder`]（H.264），消费
//! `rc-encode-vt` 产出的 **Annex-B** 码流，交出零拷贝的
//! [`FrameTexture::CVPixelBuffer`](rc_capture::FrameTexture::CVPixelBuffer)。
//!
//! # Annex-B → AVCC（坑 #7）
//! VideoToolbox 的解码输入是 AVCC 风格（4 字节长度前缀 NAL + 参数集经
//! `CMVideoFormatDescription` 传递），而本项目管线统一消费 Annex-B
//!（编码侧在输出侧重排并在 `EncoderCaps::bitstream_format` 里声明）。
//! 解码端因此在入口做一次 Annex-B → AVCC 转换（纯逻辑在 [`h264`]，
//! 任何畸形输入在进 FFI 之前被拒绝）；[`VtDecoderConfig::bitstream_format`]
//! 必须与编码端声明一致，不匹配在构造期报错——两种封装不能混用。
//!
//! # 延迟红线（契约 D1–D5 的落点）
//! | 契约 | 落点 |
//! |---|---|
//! | D1（零拷贝） | 输出 `CVPixelBuffer` 句柄直通（回调里 retain，会话保活到下一次 decode）；生产路径不 LockBaseAddress、无 CPU 回读 |
//! | D3（低延迟） | `kVTDecompressionPropertyKey_RealTime = true` + 严格单帧在途（无解码器内部缓冲可观感） |
//! | D4（缺参考不 panic） | [`RefChainGate`] 纯状态机：首个 IDR 前 / 断链 → `DecodeOutput::NeedKeyframe`；任何解码错误后断链（低延迟优先于完整，恢复靠下一个 IDR） |
//! | D5（进一出一） | VT 回调异步 → 帧号邮箱（[`mailbox`]）同步收集：提交带帧号、等到**该帧**回调才返回，顺序由结构保证 |
//!
//! # 会话生命周期
//! 会话**惰性**建立：解码器构造不碰 FFI，首个携带 SPS/PPS 的 IDR 到达才
//! 从参数集构建格式描述并创建会话；之后 SPS 变化（分辨率切换）自动重建。
//!
//! # 时间注入
//! 与 [`rc_decode`] 的契约一致：内部维护逻辑时钟（上次被接受调用的完成
//! 时刻），预计完成时刻（时钟 + 估算耗时）超过 `deadline_ts` 时直接返回
//! [`Error::Timeout`](rc_types::Error::Timeout)（不阻塞、不改变内部状态）；
//! 实际等待的墙钟预算 = `deadline_ts − 逻辑时钟`，到点回调未到达同样超时
//!（迟到输出在下一次调用开头丢弃，且因跳帧而断链——恢复靠 IDR）。
//!
//! 非 macOS 目标上本 crate 编译为空壳（仅保留纯逻辑模块），保证 workspace
//! 在任何平台都能构建与测试。

#![deny(missing_docs)]
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
// 每个 unsafe 块必须有 // SAFETY: 注释 —— 只对 ffi 模块有实际约束。
#![deny(clippy::undocumented_unsafe_blocks)]

/// Annex-B 解析 / Annex-B → AVCC 转换 / SPS 尺寸解析（纯函数，跨平台可测）。
///
/// 调用方只有 macOS 的 `VtDecoder` / ffi 模块；非 macOS 空壳构建里它没有
/// 调用者但仍然编译 + 跑单测（保持纯逻辑在任何目标上可验证）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) mod h264;

/// 帧序邮箱：解码回调 → 等待线程的单帧结果收集（D5 顺序核心，纯 Rust）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) mod mailbox;

#[cfg(target_os = "macos")]
pub(crate) mod ffi;

use rc_encode::BitstreamFormat;
use rc_types::Resolution;

/// 预估单帧解码耗时（微秒）：deadline 预检用（与 rc-decode 假解码器的
/// 默认 3ms 一致，即任务卡的解码延迟预算）。
pub const ESTIMATED_DECODE_COST_US: u32 = 3_000;

/// 参考链状态机（D4）：是否持有可用的前序参考。
///
/// 语义与 `rc-decode` 假解码器一致：
/// - 初始即断裂（「入会后从未解出过 IDR」）——[`needs_keyframe`](rc_decode::Decoder::needs_keyframe)
///   起始为 `true`；
/// - 成功解出 IDR 重建链；断链后 P 帧一律 `NeedKeyframe`，直到下一个 IDR；
/// - 解码失败 / 超时跳帧都会断链（错误恢复统一走「请求关键帧」路径）。
///
/// 纯逻辑、无 FFI——D4 契约因此可以在无 GPU 的 CI 上直接断言。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug)]
pub(crate) struct RefChainGate {
    chain_ok: bool,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl RefChainGate {
    /// 新建：链断裂（首个 IDR 之前）。
    pub(crate) fn new() -> Self {
        Self { chain_ok: false }
    }

    /// 链是否完整。
    pub(crate) fn is_ok(&self) -> bool {
        self.chain_ok
    }

    /// 成功解出一个 IDR：链重建。
    pub(crate) fn mark_idr_decoded(&mut self) {
        self.chain_ok = true;
    }

    /// 断链（丢帧 / 解码失败 / 超时跳帧）。
    pub(crate) fn break_chain(&mut self) {
        self.chain_ok = false;
    }
}

/// 解码器配置。
///
/// `bitstream_format` 必须与发送端 `EncoderCaps::bitstream_format` 的声明
/// 一致（本后端只支持 Annex-B 输入，坑 #7）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VtDecoderConfig {
    /// 输入码流封装格式。**必须为 [`BitstreamFormat::AnnexB`]**：
    /// 管线统一消费 Annex-B（rc-encode-vt 输出侧已重排并声明），本后端
    /// 在内部转 AVCC 喂 VideoToolbox——两种封装不能混用，不匹配在
    /// 构造期报错而不是运行期花屏。
    pub bitstream_format: BitstreamFormat,
    /// 接受的最大帧尺寸（SPS 声明超限的码流在进 FFI 前被拒绝，恶意/损坏
    /// 码流防御）。默认 4096×4096，与编码侧 caps 的保守上限一致。
    pub max_resolution: Resolution,
}

impl Default for VtDecoderConfig {
    /// Annex-B 输入、4096×4096 上限——与 rc-encode-vt 的 caps 声明对齐。
    fn default() -> Self {
        VtDecoderConfig {
            bitstream_format: BitstreamFormat::AnnexB,
            max_resolution: Resolution::new(4096, 4096),
        }
    }
}

impl VtDecoderConfig {
    /// 校验配置。非法项以 [`Error::InvalidConfig`](rc_types::Error::InvalidConfig)
    /// 返回，信息含字段与期望值。
    pub fn validate(&self) -> rc_types::Result<()> {
        if self.bitstream_format != BitstreamFormat::AnnexB {
            return Err(rc_types::Error::InvalidConfig(format!(
                "rc-decode-vt 只接受 Annex-B 输入（EncoderCaps::bitstream_format 必须为 annexb，当前 {}）；\
                 VideoToolbox 走 AVCC（长度前缀 + 格式描述），解码端在内部转换——两种封装不能混用（坑 #7）",
                self.bitstream_format.as_str()
            )));
        }
        if self.max_resolution.width == 0 || self.max_resolution.height == 0 {
            return Err(rc_types::Error::InvalidConfig(
                "rc-decode-vt: max_resolution must be non-zero".to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{ffi, RefChainGate, VtDecoderConfig, ESTIMATED_DECODE_COST_US};
    use rc_capture::FrameTexture;
    use rc_decode::{DecodeOutput, DecodedFrame, Decoder};
    use rc_types::{Error, FrameKind, Result, Timestamp};
    use std::fmt;
    use std::time::{Duration, Instant};

    /// VideoToolbox 硬件 H.264 解码器（实现 [`Decoder`](rc_decode::Decoder)）。
    ///
    /// - 构造不碰 FFI：会话惰性建立（首个带 SPS/PPS 的 IDR），
    ///   [`VtDecoderConfig::validate`] 的失败在构造期暴露；
    /// - 输出 [`FrameTexture::CVPixelBuffer`] 零拷贝句柄（D1），存活期与
    ///   采集侧句柄语义对称：**下一次 `decode` / 解码器销毁之前有效**，
    ///   渲染器要跨帧持有必须自行 retain；
    /// - `flush` 恒空操作：进一出一（D5）的实现没有内部缓冲。
    pub struct VtDecoder {
        cfg: VtDecoderConfig,
        gate: RefChainGate,
        /// 惰性建立的解码会话；SPS 变化时重建。
        session: Option<ffi::DecompressionSession>,
        /// 当前会话对应的 SPS（字节比较，检测参数集变化/分辨率切换）。
        session_sps: Option<Vec<u8>>,
        /// 会话内单调帧号（邮箱匹配 + PTS 单调）。
        frame_id: u64,
        /// 逻辑时钟（微秒，与 `Timestamp` 同为 u32 域）：最近一次实际解码
        /// 调用的完成时刻。
        clock_us: u32,
    }

    impl fmt::Debug for VtDecoder {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            // session 是不透明 FFI 句柄，不展开（打印指针地址无诊断价值）。
            f.debug_struct("VtDecoder")
                .field("cfg", &self.cfg)
                .field("chain_ok", &self.gate.is_ok())
                .field("has_session", &self.session.is_some())
                .field("frame_id", &self.frame_id)
                .finish_non_exhaustive()
        }
    }

    impl VtDecoder {
        /// 创建解码器：仅校验配置（不碰 FFI，任何平台/无 GPU 会话可构造）。
        ///
        /// 真正的 VideoToolbox 会话在首个带 SPS/PPS 的 IDR 到达时建立；
        /// 建不了（如无可用解码器）在那一步报 [`Error::Decoder`]。
        pub fn new(cfg: VtDecoderConfig) -> Result<Self> {
            cfg.validate()?;
            Ok(VtDecoder {
                cfg,
                gate: RefChainGate::new(),
                session: None,
                session_sps: None,
                frame_id: 0,
                clock_us: 0,
            })
        }

        /// 已提交给 VideoToolbox 的帧数（测试断言「进一出一」用）。
        #[cfg(test)]
        pub(crate) fn submitted_frames(&self) -> u64 {
            self.frame_id
        }

        /// 推进逻辑时钟（按实测墙钟耗时，饱和到 u32::MAX）并返回完成时刻。
        fn advance_clock(&mut self, elapsed: Duration) -> Timestamp {
            let us = u32::try_from(elapsed.as_micros()).unwrap_or(u32::MAX);
            self.clock_us = self.clock_us.saturating_add(us);
            Timestamp(self.clock_us)
        }
    }

    impl Decoder for VtDecoder {
        fn decode(
            &mut self,
            data: &[u8],
            kind: FrameKind,
            deadline_ts: Timestamp,
        ) -> Result<DecodeOutput> {
            // 1) 严格解析（纯逻辑，无 FFI）：非 Annex-B / 畸形输入在这里现形。
            //    bitstream_format 由配置声明并已校验（坑 #7）；形态不符的
            //    字节（如 AVCC 长度前缀流）不进 VideoToolbox。
            let nals = crate::h264::split_nals(data).ok_or_else(|| {
                Error::InvalidFrame(format!(
                    "rc-decode-vt: 码流不是合法 Annex-B（配置声明 bitstream_format={}）：{} 字节输入",
                    self.cfg.bitstream_format.as_str(),
                    data.len()
                ))
            })?;

            // 2) 声明与内容一致性：发送端的 kind 不得说谎（协议 bug 在这里
            //    暴露，而不是让参考链语义悄悄错掉）。
            let content_is_idr = nals.iter().any(|(t, _)| *t == 5);
            match (kind, content_is_idr) {
                (FrameKind::Idr, false) => {
                    return Err(Error::InvalidFrame(
                        "声明为 IDR 但码流不含 IDR slice（nal_unit_type 5）".to_string(),
                    ))
                }
                (FrameKind::P, true) => {
                    return Err(Error::InvalidFrame(
                        "声明为 P 但码流含 IDR slice（nal_unit_type 5）".to_string(),
                    ))
                }
                _ => {}
            }

            // 3) deadline 预检（注入时间域，先于任何状态变更）：预计完成
            //    时刻超过截止立即超时，不提交、不等待、不消耗帧号。
            let est_done_us = self.clock_us.saturating_add(ESTIMATED_DECODE_COST_US);
            if est_done_us > deadline_ts.0 {
                return Err(Error::Timeout(Duration::from_micros(u64::from(
                    est_done_us - deadline_ts.0,
                ))));
            }

            // 4) D4 门：链断（含首个 IDR 前）且本帧不自足 → NeedKeyframe。
            //    不 panic、不碰 FFI；置位与返回总是同时出现。
            if !self.gate.is_ok() && !content_is_idr {
                return Ok(DecodeOutput::NeedKeyframe);
            }

            // 5) IDR：确保会话存在 / SPS 变化时重建（分辨率切换路径）。
            if content_is_idr {
                let (sps, pps) = crate::h264::parameter_sets(&nals);
                match (sps, pps) {
                    (Some(sps), Some(pps)) => {
                        let needs_rebuild = match self.session_sps.as_deref() {
                            Some(cur) => cur != sps,
                            None => true,
                        };
                        if needs_rebuild {
                            // 守门：SPS 声明尺寸超上限的码流不进 FFI
                            //（恶意/损坏 SPS 防御；解析不了的交给 VT 权威，
                            //  构造期失败会变 Err）。
                            if let Some((w, h)) = crate::h264::sps_dimensions(sps) {
                                if w > self.cfg.max_resolution.width
                                    || h > self.cfg.max_resolution.height
                                {
                                    return Err(Error::InvalidFrame(format!(
                                        "SPS 声明 {w}x{h} 超过解码上限 {}x{}",
                                        self.cfg.max_resolution.width,
                                        self.cfg.max_resolution.height
                                    )));
                                }
                            }
                            self.session = Some(
                                ffi::DecompressionSession::new(sps, pps).map_err(Error::Decoder)?,
                            );
                            self.session_sps = Some(sps.to_vec());
                        }
                    }
                    // IDR 不带参数集：没有会话就无从建格式描述——需要一个
                    // 带 SPS/PPS 的 IDR（这正是 NeedKeyframe 的语义）。
                    _ if self.session.is_none() => return Ok(DecodeOutput::NeedKeyframe),
                    _ => {}
                }
            }

            // 6) 会话必须就绪（P 帧 + 链 OK ⇔ 会话存在；防御兜底不 panic）。
            let Some(session) = self.session.as_mut() else {
                return Ok(DecodeOutput::NeedKeyframe);
            };

            // 7) Annex-B → AVCC（剥离参数集，4 字节长度前缀）。「只有参数集
            //    没有 slice」的畸形在这里兜住。
            let avcc = crate::h264::to_avcc_without_parameter_sets(data).ok_or_else(|| {
                Error::InvalidFrame("帧不含任何 slice NAL（无法解码出画面）".to_string())
            })?;

            // 8) 提交 + 邮箱等待（D5）：帧号单调，墙钟预算 = 剩余注入预算。
            let frame_id = self.frame_id;
            self.frame_id = frame_id
                .checked_add(1)
                .ok_or_else(|| Error::Decoder("会话帧号耗尽 u64".to_string()))?;
            let wait = Duration::from_micros(
                u64::from(deadline_ts.0.saturating_sub(self.clock_us)).max(1),
            );
            let started = Instant::now();
            match session.decode_frame(&avcc, frame_id, wait) {
                Ok(out) => {
                    let done_ts = self.advance_clock(started.elapsed());
                    if content_is_idr {
                        self.gate.mark_idr_decoded();
                    }
                    // D1：零拷贝句柄直通（会话保活到下一次 decode）。
                    Ok(DecodeOutput::Frame(DecodedFrame {
                        texture: FrameTexture::CVPixelBuffer(out.ptr),
                        decode_done_ts: done_ts,
                        width: out.width,
                        height: out.height,
                    }))
                }
                Err(ffi::DecodeFailure::Timeout) => {
                    // 跳帧断链（后续帧将缺参考）+ 迟到输出下次丢弃。
                    self.gate.break_chain();
                    let elapsed = started.elapsed();
                    self.advance_clock(elapsed);
                    Err(Error::Timeout(elapsed))
                }
                Err(e) => {
                    // 回调错误/提交失败：断链，恢复统一走 IDR。
                    self.gate.break_chain();
                    Err(Error::Decoder(e.to_string()))
                }
            }
        }

        fn needs_keyframe(&self) -> bool {
            // 参考链断裂（含「从未解出过 IDR」）即需要关键帧。
            !self.gate.is_ok()
        }

        fn flush(&mut self) -> Result<()> {
            // D5：进一出一的实现没有内部缓冲，无帧可冲刷。
            Ok(())
        }
    }
}

#[cfg(target_os = "macos")]
pub use platform::VtDecoder;

/// 工厂：按配置创建 VideoToolbox 解码器（[`VtDecoder::new`] 的别名入口）。
///
/// ```no_run
/// # #[cfg(target_os = "macos")]
/// # fn main() -> rc_types::Result<()> {
/// use rc_decode::Decoder as _;
/// let mut dec = rc_decode_vt::new_decoder(rc_decode_vt::VtDecoderConfig::default())?;
/// assert!(dec.needs_keyframe(), "首个 IDR 之前必然缺关键帧");
/// # Ok(())
/// # }
/// # #[cfg(not(target_os = "macos"))]
/// # fn main() {}
/// ```
#[cfg(target_os = "macos")]
pub fn new_decoder(config: VtDecoderConfig) -> rc_types::Result<VtDecoder> {
    VtDecoder::new(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 坑 #7：bitstream_format 与编码端声明不符时，构造期报错且信息指明
    /// 两种封装（AVCC 输入不能喂给本解码器）。
    #[test]
    fn config_rejects_mismatched_bitstream_format() {
        let err = VtDecoderConfig {
            bitstream_format: BitstreamFormat::Avcc,
            ..VtDecoderConfig::default()
        }
        .validate()
        .expect_err("AVCC 声明必须被拒");
        assert!(
            matches!(&err, rc_types::Error::InvalidConfig(msg)
                if msg.contains("annexb") && msg.contains("avcc")),
            "错误信息必须同时点出期望（annexb）与实际（avcc），实际 {err:?}"
        );
        // 合法配置（默认）通过；零上限被拒。
        assert!(VtDecoderConfig::default().validate().is_ok());
        let zero = VtDecoderConfig {
            max_resolution: Resolution::new(0, 1080),
            ..VtDecoderConfig::default()
        }
        .validate()
        .expect_err("零分辨率必须被拒");
        assert!(matches!(zero, rc_types::Error::InvalidConfig(_)));
    }

    /// D4 语义（纯状态机）：初始断链；IDR 重建；断链后粘住直到下一个 IDR。
    #[test]
    fn ref_chain_gate_need_keyframe_semantics() {
        let mut gate = RefChainGate::new();
        assert!(!gate.is_ok(), "首个 IDR 之前链必然断裂");
        gate.break_chain();
        assert!(!gate.is_ok(), "断链是幂等的");
        gate.mark_idr_decoded();
        assert!(gate.is_ok(), "解出 IDR 后链重建");
        gate.break_chain();
        assert!(!gate.is_ok(), "解码失败/跳帧重新断链");
        gate.mark_idr_decoded();
        assert!(gate.is_ok(), "恢复路径：下一个 IDR");
    }
}

/// 无 GPU 的 macOS 单元测试：构造不碰 FFI、链断早退不碰 FFI——整个测试
/// 不创建任何 VideoToolbox 会话（纯逻辑路径全覆盖）。
#[cfg(all(test, target_os = "macos"))]
mod gates_no_gpu {
    use super::*;
    use rc_decode::{DecodeOutput, Decoder};
    use rc_types::{Error, FrameKind, Timestamp};

    /// 拼一段合法 Annex-B 单 NAL 帧（头字节由调用方给）。
    fn frame_of(header: u8) -> Vec<u8> {
        let mut v = vec![0x00, 0x00, 0x00, 0x01];
        v.push(header);
        v.extend_from_slice(&[0x42; 8]);
        v
    }

    /// D4（入口段）+ 畸形拒绝 + deadline 语义：首个 IDR 之前 P 帧返回
    /// NeedKeyframe 并置位标志；不带参数集的 IDR 无法引导 → 同样
    /// NeedKeyframe；畸形输入与 kind 谎报在进任何 FFI 之前被拒；预计完成
    /// 超过截止 → Timeout 且不消耗帧号。
    #[test]
    fn decoder_gates_before_first_idr_and_malformed_input() {
        let mut dec = VtDecoder::new(VtDecoderConfig::default()).expect("默认配置合法");
        assert!(dec.needs_keyframe(), "从未解出 IDR：初始即缺关键帧");

        // P 帧（合法 Annex-B、非 IDR slice）：NeedKeyframe，不 panic（D4）。
        match dec
            .decode(&frame_of(0x41), FrameKind::P, Timestamp(10_000_000))
            .expect("缺参考不是错误")
        {
            DecodeOutput::NeedKeyframe => {}
            other => panic!("缺参考的 P 帧应返回 NeedKeyframe，实际 {other:?}"),
        }
        assert!(dec.needs_keyframe());

        // 畸形输入（非起始码开头 / 空输入）：InvalidFrame，绝不进 FFI。
        for garbage in [vec![0x00, 0x65, 1, 2], Vec::new()] {
            let err = dec
                .decode(&garbage, FrameKind::P, Timestamp(10_000_000))
                .expect_err("畸形输入必须报错");
            assert!(
                matches!(err, Error::InvalidFrame(ref msg) if msg.contains("Annex-B")),
                "应报 InvalidFrame 并点出 Annex-B，实际 {err:?}"
            );
        }

        // kind 谎报：声明 IDR 但无 IDR slice / 声明 P 但含 IDR slice。
        let err = dec
            .decode(&frame_of(0x41), FrameKind::Idr, Timestamp(10_000_000))
            .expect_err("谎报必须被拒");
        assert!(matches!(err, Error::InvalidFrame(_)));
        let err = dec
            .decode(&frame_of(0x65), FrameKind::P, Timestamp(10_000_000))
            .expect_err("谎报必须被拒");
        assert!(matches!(err, Error::InvalidFrame(_)));

        // 不带参数集的 IDR（无会话可沿用）：无法引导 → NeedKeyframe。
        match dec
            .decode(&frame_of(0x65), FrameKind::Idr, Timestamp(10_000_000))
            .expect("无法引导不是错误（请求关键帧即可恢复）")
        {
            DecodeOutput::NeedKeyframe => {}
            other => panic!("无从建会话的 IDR 应返回 NeedKeyframe，实际 {other:?}"),
        }
        assert!(dec.needs_keyframe());

        // deadline 预检：截止时刻小于预估完成时刻 → Timeout，且不消耗帧号。
        let before = dec.submitted_frames();
        let err = dec
            .decode(&frame_of(0x41), FrameKind::P, Timestamp(1_000))
            .expect_err("预计完成超过截止必须超时");
        assert!(matches!(err, Error::Timeout(_)), "实际 {err:?}");
        assert_eq!(dec.submitted_frames(), before, "超时路径不消耗帧号");

        // flush：无缓冲，恒 Ok。
        dec.flush().expect("进一出一的实现没有缓冲");
    }
}

/// 真机测试：需要 macOS + 可用的 VideoToolbox 解码会话（输入是 rc-encode-vt
/// 编码的合成帧，不依赖屏幕采集权限）。
#[cfg(all(test, target_os = "macos"))]
mod real_machine {
    use super::*;
    use rc_capture::{CapturedFrame, FrameTexture};
    use rc_decode::{DecodeOutput, Decoder};
    use rc_encode::Encoder as _;
    use rc_types::{CaptureSeq, Timestamp};
    use std::time::{Duration, Instant};

    /// 宽裕的解码截止（注入时间域；解码器逻辑时钟远小于它）。
    const RELAXED_DEADLINE: Timestamp = Timestamp(60_000_000);

    /// T05 真机验收：1080p 解码段延迟。
    ///
    /// 方法（按任务卡）：用 rc-encode-vt（dev-dependency）先按 60fps 节奏
    /// 编码 300 帧（PRNG 图案，码流形态与真实管线一致），再逐帧解码，
    /// 统计每次 `decode()` 全程（Annex-B 解析 → AVCC 转换 → 装帧 → VT 提交
    /// → 回调等待 → 输出组装）的耗时分布。断言 **P50 ≤ 3ms**（T05 验收）。
    /// 顺带断言进一出一（300 进 300 出）与首帧尺寸上报。
    #[test]
    #[ignore = "requires macOS + GPU session"]
    fn h264_decode_1080p_under_3ms() {
        // —— 编码段：300 帧 1080p（rc-encode-vt，Annex-B 输出）——
        let mut encoder = rc_encode_vt::new_encoder(rc_encode::EncoderConfig::default())
            .expect("构造编码器（含 E6 自检）");
        let buffer = ffi::test_support::make_pixel_buffer(1920, 1080).expect("创建测试像素缓冲");
        let mut streams: Vec<(Vec<u8>, rc_types::FrameKind)> = Vec::with_capacity(300);
        // 按 60fps 节奏提交：与编码侧真机测试一致（突发提交会让码控误判，
        // 产出的码流不代表稳态）。
        let frame_period = Duration::from_nanos(16_666_667);
        let mut next_submit = Instant::now() + frame_period;
        for i in 0u32..300 {
            ffi::test_support::write_nv12_pattern(&buffer, i, 0x5EED_0000_0000_0001)
                .expect("写入测试图案");
            let ts = Timestamp(i * 16_667);
            let frame = CapturedFrame {
                texture: FrameTexture::CVPixelBuffer(buffer.as_opaque_ptr()),
                timestamp: ts,
                dirty: None,
                seq: CaptureSeq(u64::from(i)),
                is_repeat: false,
                resource_guard: None,
            };
            let out = encoder
                .encode(&frame, false, Timestamp(ts.0.saturating_add(50_000)))
                .unwrap_or_else(|e| panic!("第 {i} 帧编码失败: {e}"));
            if i == 0 {
                assert!(out.is_keyframe, "编码器首个正式帧必须是 IDR");
            }
            streams.push((out.data, out.kind));
            if let Some(sleep) = next_submit.checked_duration_since(Instant::now()) {
                std::thread::sleep(sleep);
            }
            next_submit += frame_period;
        }

        // —— 解码段：进一出一逐帧解码，测 decode() 全程 ——
        let mut decoder = VtDecoder::new(VtDecoderConfig::default()).expect("默认配置合法");
        let mut samples_ms: Vec<f64> = Vec::with_capacity(300);
        let mut outputs = 0usize;
        let decode_started = Instant::now();
        for (i, (data, kind)) in streams.iter().enumerate() {
            let started = Instant::now();
            let out = decoder
                .decode(data, *kind, RELAXED_DEADLINE)
                .unwrap_or_else(|e| panic!("第 {i} 帧解码失败: {e}"));
            samples_ms.push(started.elapsed().as_secs_f64() * 1000.0);
            match out {
                DecodeOutput::Frame(f) => {
                    outputs += 1;
                    assert_eq!(
                        (f.width, f.height),
                        (1920, 1080),
                        "帧尺寸上报必须与码流一致（帧 {i}）"
                    );
                    assert!(f.decode_done_ts.0 > 0);
                }
                other => panic!("第 {i} 帧应进一出一，实际 {other:?}"),
            }
        }
        let wall_ms = decode_started.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(outputs, 300, "300 帧进必须 300 帧出（D5）");
        assert_eq!(decoder.submitted_frames(), 300);

        samples_ms.sort_by(|a, b| a.partial_cmp(b).expect("延迟样本均为有限值"));
        let p50 = samples_ms[samples_ms.len() / 2];
        let p95 = samples_ms[(samples_ms.len() * 95) / 100];
        eprintln!(
            "VideoToolbox 1080p 解码段（300 帧，含 Annex-B→AVCC + 装帧 + VT 提交 + 回调等待）：\
             P50 = {p50:.2}ms, P95 = {p95:.2}ms, min = {:.2}ms, max = {:.2}ms（总墙钟 {wall_ms:.0}ms）",
            samples_ms[0],
            samples_ms[samples_ms.len() - 1]
        );
        assert!(
            p50 <= 3.0,
            "解码段 P50 = {p50:.2}ms 超过 3ms 预算（P95 = {p95:.2}ms）"
        );
    }

    /// T05 真机验收：编码 → 解码视觉往返。
    ///
    /// 方法（按任务卡）：PRNG 图案（梯度 + 6 个 PRNG 实心矩形，逐帧平移）
    /// → rc-encode-vt 编码（IDR + 2 个 P 帧）→ 解码 → **测试内 CPU 回读
    /// 一次**（仅测试允许；生产路径零 CPU 回读）→ 对比最后一帧（P 帧，
    /// 真正走到参考链）的 Y 平面算 PSNR。断言 **PSNR > 35dB 且尺寸一致**。
    #[test]
    #[ignore = "requires macOS + GPU session"]
    fn decode_encode_roundtrip_visual_match() {
        const WIDTH: u32 = 1920;
        const HEIGHT: u32 = 1080;
        const FRAMES: u32 = 3;

        // —— 编码段（24Mbps：视觉测试要质量余量，延迟测试在另一条）——
        let enc_cfg = rc_encode::EncoderConfig {
            bitrate_bps: 24_000_000,
            ..rc_encode::EncoderConfig::default()
        };
        let mut encoder = rc_encode_vt::new_encoder(enc_cfg).expect("构造编码器");
        let buffer = ffi::test_support::make_pixel_buffer(WIDTH, HEIGHT).expect("创建测试像素缓冲");
        let mut src_luma: Vec<Vec<u8>> = Vec::with_capacity(FRAMES as usize);
        let mut streams: Vec<(Vec<u8>, rc_types::FrameKind)> = Vec::with_capacity(FRAMES as usize);
        for i in 0..FRAMES {
            let dense = ffi::test_support::write_nv12_pattern(&buffer, i, 0x00C0_FFEE_0000_0001)
                .expect("写入测试图案");
            src_luma.push(dense);
            let ts = Timestamp(i * 16_667);
            let frame = CapturedFrame {
                texture: FrameTexture::CVPixelBuffer(buffer.as_opaque_ptr()),
                timestamp: ts,
                dirty: None,
                seq: CaptureSeq(u64::from(i)),
                is_repeat: false,
                resource_guard: None,
            };
            let out = encoder
                .encode(&frame, false, Timestamp(ts.0.saturating_add(50_000)))
                .unwrap_or_else(|e| panic!("第 {i} 帧编码失败: {e}"));
            streams.push((out.data, out.kind));
        }

        // —— 解码段：全部帧进一出一，末帧句柄仍在保活窗口内 ——
        let mut decoder = VtDecoder::new(VtDecoderConfig::default()).expect("默认配置合法");
        let mut last_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        for (i, (data, kind)) in streams.iter().enumerate() {
            match decoder
                .decode(data, *kind, RELAXED_DEADLINE)
                .unwrap_or_else(|e| panic!("第 {i} 帧解码失败: {e}"))
            {
                DecodeOutput::Frame(f) => {
                    assert_eq!((f.width, f.height), (WIDTH, HEIGHT), "尺寸一致（帧 {i}）");
                    last_ptr = match f.texture {
                        FrameTexture::CVPixelBuffer(ptr) => ptr,
                        ref other => panic!("应输出 CVPixelBuffer 零拷贝句柄，实际 {other:?}"),
                    };
                }
                other => panic!("第 {i} 帧应进一出一，实际 {other:?}"),
            }
        }

        // —— 一次性 CPU 回读（仅测试允许）+ Y 平面 PSNR ——
        let (w, h, decoded_luma) =
            ffi::test_support::read_luma_plane(last_ptr).expect("回读末帧 Y 平面");
        assert_eq!((w, h), (WIDTH, HEIGHT), "解码输出尺寸与编码输入一致");
        let reference = &src_luma[usize::try_from(FRAMES - 1).expect("末帧索引")];
        let mse = decoded_luma
            .iter()
            .zip(reference)
            .map(|(a, b)| {
                let d = i32::from(*a) - i32::from(*b);
                (d * d) as f64
            })
            .sum::<f64>()
            / decoded_luma.len() as f64;
        let psnr = 10.0 * (255.0f64 * 255.0 / mse).log10();
        eprintln!(
            "编码→解码往返（{WIDTH}x{HEIGHT}，{FRAMES} 帧，末帧为 P 帧）：Y 平面 PSNR = {psnr:.2}dB（MSE = {mse:.2}）"
        );
        assert!(
            psnr > 35.0,
            "视觉往返 PSNR = {psnr:.2}dB ≤ 35dB（结构不匹配或参考链断裂）"
        );
    }
}
