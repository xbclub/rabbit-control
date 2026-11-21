//! H.264 Annex-B 解析、Annex-B → AVCC 转换与 SPS 尺寸解析。
//!
//! 纯函数、无 unsafe、无平台依赖 —— 在任何目标上都可以单元测试。
//!
//! # 为什么需要它（任务卡 T05 坑 #7）
//! 管线统一消费 Annex-B（`rc-encode-vt` 在输出侧重排为 Annex-B 并在
//! `EncoderCaps::bitstream_format` 里声明）。而 `VTDecompressionSession`
//! 的输入是 **AVCC 风格**：NAL 之间用 4 字节大端长度前缀分隔、参数集
//! （SPS/PPS）经 `CMVideoFormatDescription` 传递。解码端必须在喂给
//! VideoToolbox 之前做一次 Annex-B → AVCC 的转换，两种封装不能混用。
//!
//! # 防御式解析
//! 解码端收到的码流来自网络（可能丢包、损坏、被恶意构造），这里的每个
//! 入口都是严格解析：任何畸形输入返回 `None`，由调用方映射为
//! [`Error::InvalidFrame`](rc_types::Error::InvalidFrame)，
//! **绝不把可疑字节送进 VideoToolbox**。
//!
//! 编码侧的对照实现在 `rc-encode-vt/src/annexb.rs`（方向相反：
//! AVCC → Annex-B）；两个模块刻意不共享代码，各自保持独立可测。

/// Annex-B 起始码（4 字节形式）。生产路径按字节逐位判定
/// （[`start_code_len_at`]），本常量只被单测用于拼流。
#[cfg_attr(not(test), allow(dead_code))]
const ANNEXB_START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

/// `pos` 处是否是 Annex-B 起始码；是则返回其长度（4 或 3 字节形式）。
fn start_code_len_at(buf: &[u8], pos: usize) -> Option<usize> {
    let rest = buf.get(pos..)?;
    if rest.len() >= 4 && rest[0] == 0 && rest[1] == 0 && rest[2] == 0 && rest[3] == 1 {
        Some(4)
    } else if rest.len() >= 3 && rest[0] == 0 && rest[1] == 0 && rest[2] == 1 {
        Some(3)
    } else {
        None
    }
}

/// NAL 头字节的低 5 位 = nal_unit_type（H.264 Annex 表 7-1）。
const fn nal_unit_type(header: u8) -> u8 {
    header & 0x1f
}

/// 严格切分 Annex-B 流：返回 `(nal_unit_type, 载荷)` 序列。
///
/// 载荷不含起始码、含 NAL 头字节。与编码侧宽松的 `nal_payloads` 不同，
/// 这里是**解码入口的守门人**，结构性错误一律拒绝（返回 `None`）：
/// - 空输入；
/// - 不以起始码开头（含 AVCC 形态的长度前缀输入——封装不符应在上游被
///   `bitstream_format` 校验拦下，混进来的在这里现形）；
/// - 起始码后没有任何字节（空 NAL）。
pub(crate) fn split_nals(input: &[u8]) -> Option<Vec<(u8, &[u8])>> {
    if input.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < input.len() {
        // 不以起始码开头（首字节起就不匹配）→ 整体拒绝。
        let code_len = start_code_len_at(input, pos)?;
        let payload_start = pos + code_len;
        if payload_start >= input.len() {
            // 起始码后没有任何字节：不是合法 NAL 序列。
            return None;
        }
        // 载荷延伸到下一个起始码（或流尾）。
        let mut next = payload_start;
        while next < input.len() && start_code_len_at(input, next).is_none() {
            next += 1;
        }
        let payload = input.get(payload_start..next)?;
        if payload.is_empty() {
            // 紧邻的起始码（载荷起点即是下一个起始码）：空 NAL，拒绝。
            return None;
        }
        out.push((nal_unit_type(payload[0]), payload));
        // next >= payload_start + 1 > pos：必然前进，无死循环。
        pos = next;
    }
    Some(out)
}

/// 从 NAL 序列里取第一组参数集：`(SPS, PPS)`（载荷含 NAL 头字节）。
///
/// 返回 `(None, _)` / `(_, None)` 表示缺 SPS / 缺 PPS——没有参数集就
/// 无法构建 `CMVideoFormatDescription`，解码会话无从建立。
pub(crate) fn parameter_sets<'a>(
    nals: &'a [(u8, &'a [u8])],
) -> (Option<&'a [u8]>, Option<&'a [u8]>) {
    let sps = nals
        .iter()
        .find(|(t, _)| *t == 7)
        .map(|(_, payload)| *payload);
    let pps = nals
        .iter()
        .find(|(t, _)| *t == 8)
        .map(|(_, payload)| *payload);
    (sps, pps)
}

/// Annex-B → AVCC：每个 NAL 的起始码替换为 4 字节大端长度前缀，并
/// **剥离参数集**（SPS=7 / PPS=8）。
///
/// 参数集剥离的原因：VTDecompressionSession 的参数集经
/// `CMVideoFormatDescription`（由 [`parameter_sets`] 提取构建）传递，
/// 附着在 `CMSampleBuffer` 上；样体里只留 slice（及其他非参数集 NAL，
/// 如 SEI）。
///
/// 返回 `None`（畸形拒绝）的情形：[`split_nals`] 拒绝的一切、单 NAL
/// 载荷超过 4 字节前缀可表示的范围、剥离参数集后没有任何 NAL 剩余
/// （「帧」只有参数集、没有 slice——解不出画面）。
pub(crate) fn to_avcc_without_parameter_sets(input: &[u8]) -> Option<Vec<u8>> {
    let nals = split_nals(input)?;
    let mut out = Vec::with_capacity(input.len());
    let mut slice_count = 0usize;
    for (nal_type, payload) in nals {
        if nal_type == 7 || nal_type == 8 {
            continue;
        }
        if payload.len() > u32::MAX as usize {
            return None;
        }
        slice_count += 1;
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
    }
    if slice_count == 0 {
        return None;
    }
    Some(out)
}

// ─────────────────────────────────────────────────────────────────────────────
// SPS 尺寸解析（pic_width/height_in_mbs + frame_cropping → 像素尺寸）
// ─────────────────────────────────────────────────────────────────────────────

/// 去除 SPS 载荷里的 emulation prevention 字节（`00 00 03` → `00 00`，
/// ITU-T H.264 §7.4.1）。
///
/// 载荷里的 `00 00 03` 是编码器为防止起始码误匹配插入的转义，解析语法
/// 元素前必须去掉。返回新分配的 Vec（输入通常不含转义字节，此时原样拷贝）。
fn strip_emulation_prevention(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len());
    let mut i = 0;
    while i < payload.len() {
        if i + 2 < payload.len() && payload[i] == 0 && payload[i + 1] == 0 && payload[i + 2] == 3 {
            out.extend_from_slice(&[0, 0]);
            i += 3;
        } else {
            out.push(payload[i]);
            i += 1;
        }
    }
    out
}

/// 位读取器（MSB 优先）。任何越界读取返回 `None`。
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize, // 位偏移
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// 读 `n` 位（n ≤ 32）为 u32。
    fn bits(&mut self, n: u32) -> Option<u32> {
        if n > 32 || (self.pos + n as usize) > self.data.len() * 8 {
            return None;
        }
        let mut value = 0u32;
        for _ in 0..n {
            let byte = self.data[self.pos / 8];
            let bit = (byte >> (7 - self.pos % 8)) & 1;
            value = (value << 1) | u32::from(bit);
            self.pos += 1;
        }
        Some(value)
    }

    /// 读 1 位为 bool。
    fn flag(&mut self) -> Option<bool> {
        self.bits(1).map(|v| v != 0)
    }

    /// 无符号 Exp-Golomb 码 ue(v)（H.264 §9.1）。前导零超过 31 位按畸形拒绝。
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        while self.bits(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let suffix = if zeros == 0 { 0 } else { self.bits(zeros)? };
        Some((1u64 << zeros) as u32 - 1 + suffix)
    }

    /// 有符号 Exp-Golomb 码 se(v)：ue(v) 的低 32 位映射（值域足够，溢出按 None 拒绝）。
    fn se(&mut self) -> Option<i32> {
        let k = self.ue()?;
        if k == u32::MAX {
            return None; // 2^31-1+2^31 溢出 i32：畸形。
        }
        let k = k as i64;
        Some(if k % 2 == 1 {
            ((k + 1) / 2) as i32
        } else {
            -((k / 2) as i32)
        })
    }
}

/// 需要 chroma_format_idc / scaling matrix 的 profile（H.264 表 A-1 高档族）。
const HIGH_PROFILES: [u8; 13] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

/// 解析 SPS 的像素尺寸 `(width, height)`（含 frame_cropping）。
///
/// 输入是 SPS 的 **NAL 载荷**（含 1 字节 NAL 头，调用方从 [`split_nals`] 拿到）。
/// 返回 `None` = 解析不了（畸形 SPS，或带 scaling matrix 的高档 SPS——后者
/// 罕见且解析成本高，尺寸交给 VideoToolbox 权威处理，见下）。
///
/// # 生产用途
/// - 构造期守门：IDR 的 SPS 声明尺寸超过 [`VtDecoderConfig::max_resolution`](crate::VtDecoderConfig::max_resolution)
///   时提前拒绝（恶意/损坏码流防御，不给 FFI 机会）；
/// - 错误诊断（带尺寸的报错信息）。
///
/// 输出帧的上报尺寸**不**取自这里——以解码输出 `CVPixelBuffer` 的实际
/// 尺寸为准（VideoToolbox 是权威）。
pub(crate) fn sps_dimensions(sps_nal: &[u8]) -> Option<(u32, u32)> {
    // 跳过 NAL 头字节；去掉 emulation prevention 后按位解析。
    let body = strip_emulation_prevention(sps_nal.get(1..)?);
    let mut r = BitReader::new(&body);

    let profile_idc = r.bits(8)? as u8;
    let _constraint_flags = r.bits(8)?;
    let _level_idc = r.bits(8)?;
    let _sps_id = r.ue()?;

    // 高档族：额外的 chroma_format / bit_depth / scaling matrix 语法元素。
    let mut chroma_format_idc = 1u32; // 未出现时按 4:2:0（H.264 §7.4.2.1.1）
    if HIGH_PROFILES.contains(&profile_idc) {
        chroma_format_idc = r.ue()?;
        let mut separate_colour_planes = false;
        if chroma_format_idc == 3 {
            separate_colour_planes = r.flag()?;
        }
        if separate_colour_planes {
            chroma_format_idc = 0; // 单色平面按 monochrome 处理
        }
        let _bit_depth_luma = r.ue()?;
        let _bit_depth_chroma = r.ue()?;
        let _qp_prime_bypass = r.flag()?;
        if r.flag()? {
            // seq_scaling_matrix_present：解析成本高（4/8 个缩放矩阵列表），
            // 罕见于实时流；放弃解析，尺寸交给 VT。
            return None;
        }
    }

    let _log2_max_frame_num = r.ue()?;
    let pic_order_cnt_type = r.ue()?;
    if pic_order_cnt_type == 0 {
        let _log2_max_poc_lsb = r.ue()?;
    } else if pic_order_cnt_type == 1 {
        let _delta_pic_order_always_zero = r.flag()?;
        let _offset_to_non_ref_pic = r.se()?;
        let _offset_to_top_bottom = r.se()?;
        let cycle = r.ue()?;
        for _ in 0..cycle {
            let _offset_for_ref: i32 = r.se()?;
        }
    } // poc_type == 2：无额外语法元素。

    let _max_num_ref_frames = r.ue()?;
    let _gaps_in_frame_num = r.flag()?;
    let pic_width_in_mbs_minus1 = r.ue()?;
    let pic_height_in_map_units_minus1 = r.ue()?;
    let frame_mbs_only_flag = r.flag()?;
    if !frame_mbs_only_flag {
        let _mb_adaptive = r.flag()?;
    }
    let _direct_8x8 = r.flag()?;
    let frame_cropping_flag = r.flag()?;
    let (crop_left, crop_right, crop_top, crop_bottom) = if frame_cropping_flag {
        (r.ue()?, r.ue()?, r.ue()?, r.ue()?)
    } else {
        (0, 0, 0, 0)
    };

    // 编码尺寸（宏块对齐）；frame_mbs_only=0（场宏块）时垂直方向 ×2。
    let frame_mbs_only = u32::from(frame_mbs_only_flag);
    let width_mbs = u64::from(pic_width_in_mbs_minus1) + 1;
    let height_map = u64::from(pic_height_in_map_units_minus1) + 1;
    let crop_unit_y_factor = 2 - frame_mbs_only;

    // 裁剪单位（H.264 §7.4.2.1.1，含 4:4:4 / monochrome 的 SubWidthC=1）。
    let (crop_unit_x, crop_unit_y) = match chroma_format_idc {
        1 => (2u64, 2 * u64::from(crop_unit_y_factor)), // 4:2:0
        2 => (2, u64::from(crop_unit_y_factor)),        // 4:2:2
        _ => (1, u64::from(crop_unit_y_factor)),        // 4:4:4 / monochrome
    };

    let width = width_mbs
        .checked_mul(16)?
        .checked_sub(u64::from(crop_left) * crop_unit_x)?
        .checked_sub(u64::from(crop_right) * crop_unit_x)?;
    let height = height_map
        .checked_mul(16 * u64::from(crop_unit_y_factor))?
        .checked_sub(u64::from(crop_top) * crop_unit_y)?
        .checked_sub(u64::from(crop_bottom) * crop_unit_y)?;

    // 溢出防护 + 非零校验（裁剪把整帧裁没 = 畸形 SPS）。
    let width = u32::try_from(width).ok()?;
    let height = u32::try_from(height).ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    Some((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 合成 NAL（头字节 + 载荷）。
    fn nal(header: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![header];
        v.extend_from_slice(payload);
        v
    }

    /// 拼一段 4 字节起始码的 Annex-B 流。
    fn annexb(nals: &[Vec<u8>]) -> Vec<u8> {
        let mut v = Vec::new();
        for n in nals {
            v.extend_from_slice(&ANNEXB_START_CODE);
            v.extend_from_slice(n);
        }
        v
    }

    /// Annex-B → AVCC：SPS/PPS 剥离、其余 NAL 换 4 字节长度前缀；
    /// 往返结构（去掉前缀即原 NAL）保持不变。
    #[test]
    fn avcc_conversion_strips_parameter_sets_and_prefixes_lengths() {
        let sps = nal(0x67, &[1, 2, 3]);
        let pps = nal(0x68, &[4, 5]);
        let idr = nal(0x65, &[9; 16]);
        let sei = nal(0x06, &[7, 8]); // 非 VCL、非参数集：应保留在样体内
        let input = annexb(&[sps.clone(), pps.clone(), sei.clone(), idr.clone()]);

        let avcc = to_avcc_without_parameter_sets(&input).expect("合法 Annex-B 必须转换成功");
        // 期望：SEI + IDR，各带 4 字节大端长度前缀；SPS/PPS 不在样体里。
        let mut expected = Vec::new();
        for n in [&sei, &idr] {
            expected.extend_from_slice(&(n.len() as u32).to_be_bytes());
            expected.extend_from_slice(n);
        }
        assert_eq!(avcc, expected);

        // 参数集提取：SPS/PPS 各就各位（供 CMVideoFormatDescription 用）。
        let nals = split_nals(&input).expect("切分必须成功");
        let (s, p) = parameter_sets(&nals);
        assert_eq!(s, Some(sps.as_slice()));
        assert_eq!(p, Some(pps.as_slice()));
    }

    /// 畸形输入必须全部拒绝（返回 None），绝不能产出可疑 AVCC：
    /// 空输入；不以起始码开头（含 AVCC 形态的字节流）；起始码后无字节；
    /// 紧邻起始码（空 NAL）；只有参数集没有 slice 的「帧」。
    #[test]
    fn malformed_inputs_are_rejected_everywhere() {
        // 空输入。
        assert!(to_avcc_without_parameter_sets(&[]).is_none());
        // 不以起始码开头：AVCC 形态（长度前缀）与随机字节都算。
        let avcc_style = {
            let mut v = (16u32).to_be_bytes().to_vec();
            v.extend_from_slice(&nal(0x65, &[9; 15]));
            v
        };
        assert!(to_avcc_without_parameter_sets(&avcc_style).is_none());
        assert!(to_avcc_without_parameter_sets(&[0x00, 0x65, 1, 2]).is_none());
        // 起始码后没有任何字节。
        assert!(to_avcc_without_parameter_sets(&ANNEXB_START_CODE).is_none());
        // 紧邻起始码 = 空 NAL。
        assert!(to_avcc_without_parameter_sets(&[
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x01, 0x65, 1
        ])
        .is_none());
        // 只有参数集、没有 slice：解不出画面。
        let ps_only = annexb(&[nal(0x67, &[1]), nal(0x68, &[2])]);
        assert!(to_avcc_without_parameter_sets(&ps_only).is_none());

        // 对照：合法 IDR 流（带参数集）在每一层都通过。
        let good = annexb(&[nal(0x67, &[1]), nal(0x68, &[2]), nal(0x65, &[3; 8])]);
        assert!(split_nals(&good).is_some());
        assert!(to_avcc_without_parameter_sets(&good).is_some());
    }

    // —— SPS 尺寸解析的测试用具：手工按位构造 SPS ——

    /// MSB 优先的测试位写入器。
    struct BitWriter {
        out: Vec<u8>,
        /// 当前字节剩余可写位数（0 = 需要新字节）。
        bit: u32,
    }

    impl BitWriter {
        fn new() -> Self {
            Self {
                out: Vec::new(),
                bit: 0,
            }
        }
        fn put(&mut self, v: u32, n: u32) {
            for i in (0..n).rev() {
                let b = (v >> i) & 1;
                if self.bit == 0 {
                    self.out.push(0);
                    self.bit = 8;
                }
                self.bit -= 1;
                *self.out.last_mut().unwrap() |= (b << self.bit) as u8;
            }
        }
        fn ue(&mut self, v: u32) {
            // v+1 的位长 k，写 k-1 个 0、1、低 k-1 位。
            let mut k = 0u32;
            while ((v + 1) >> k) > 1 {
                k += 1;
            }
            self.put(0, k);
            self.put(1, 1);
            if k > 0 {
                self.put(v + 1, k);
            }
        }
        /// 组装成 SPS NAL（头字节 0x67 = baseline SPS，nal_unit_type=7）。
        fn into_sps(self) -> Vec<u8> {
            let mut v = vec![0x67];
            v.extend_from_slice(&self.out);
            v
        }
    }

    /// 构造 baseline（profile 66）SPS：poc_type 0、frame_mbs_only=1。
    /// `crop_bottom` 以 4:2:0 裁剪单位计（1 单位 = 2 像素行）。
    fn make_sps(width_in_mbs: u32, height_in_map_units: u32, crop_bottom_units: u32) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.put(66, 8); // profile_idc = baseline
        w.put(0, 8); // constraint flags/reserved
        w.put(40, 8); // level_idc
        w.ue(0); // sps_id
        w.ue(0); // log2_max_frame_num_minus4
        w.ue(0); // pic_order_cnt_type = 0
        w.ue(0); // log2_max_pic_order_cnt_lsb_minus4
        w.ue(1); // max_num_ref_frames
        w.put(0, 1); // gaps_in_frame_num_value_allowed_flag
        w.ue(width_in_mbs - 1);
        w.ue(height_in_map_units - 1);
        w.put(1, 1); // frame_mbs_only_flag
        w.put(1, 1); // direct_8x8_inference_flag
        if crop_bottom_units > 0 {
            w.put(1, 1); // frame_cropping_flag
            w.ue(0); // crop_left
            w.ue(0); // crop_right
            w.ue(0); // crop_top
            w.ue(crop_bottom_units);
        } else {
            w.put(0, 1);
        }
        w.into_sps()
    }

    /// 1080p = 120×68 宏块 − 底部 8 行（4 个 4:2:0 裁剪单位，1 单位 = 2 行）；
    /// 720p = 80×45 宏块，无裁剪；畸形输入返回 None。
    #[test]
    fn sps_dimensions_are_parsed_with_and_without_cropping() {
        let uncropped = sps_dimensions(&make_sps(120, 68, 0)).expect("无裁剪必须可解析");
        assert_eq!(uncropped, (1920, 1088), "宏块对齐的编码尺寸");

        let cropped = sps_dimensions(&make_sps(120, 68, 4)).expect("裁剪必须可解析");
        assert_eq!(cropped, (1920, 1080), "底部 8 行裁掉后是 1080p");

        let exact = sps_dimensions(&make_sps(80, 45, 0)).expect("720p 必须可解析");
        assert_eq!(exact, (1280, 720));

        // 畸形：太短（连 profile/level 都不完整）、空载荷。
        assert!(sps_dimensions(&[0x67, 0x42]).is_none());
        assert!(sps_dimensions(&[]).is_none());
    }
}
