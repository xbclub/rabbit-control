//! H.264 裸流的封装转换与 NAL 结构解析。
//!
//! 纯函数、无 unsafe、无平台依赖 —— 在任何目标上都可以单元测试。
//!
//! # 为什么需要它
//! VideoToolbox 的压缩输出是 **AVCC 风格**：NAL 之间用长度前缀分隔，
//! SPS/PPS 放在 `CMVideoFormatDescription` 里而不在裸流中（SDK 头文件里
//! 没有 Annex-B 输出开关，见 `ffi` 模块的说明）。而本项目管线统一消费
//! Annex-B（`EncoderCaps::bitstream_format` 声明什么，解码端就按什么解析，
//! 两者不能混用），所以输出侧必须做一次 AVCC → Annex-B 的重排。
//!
//! 转换是防御式的：输入若已经是 Annex-B（起始码开头）则按 Annex-B 规整
//! 直通，不假设某个 macOS 版本的输出细节。

/// Annex-B 起始码（4 字节形式）。
pub(crate) const ANNEXB_START_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x01];

/// 输入裸流的源封装格式（[`normalize_to_annexb`] 的判定结果）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceFormat {
    /// 长度前缀分隔的 NAL（VideoToolbox 的默认输出形态）。
    Avcc,
    /// 起始码分隔的 NAL。
    AnnexB,
}

/// `pos` 处是否是 Annex-B 起始码；是则返回其长度（4 或 3 字节形式）。
pub(crate) fn start_code_len_at(buf: &[u8], pos: usize) -> Option<usize> {
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
pub(crate) const fn nal_unit_type(header: u8) -> u8 {
    header & 0x1f
}

/// 扫描 Annex-B 流，返回其中全部 NAL 的 nal_unit_type 序列。
///
/// IDR slice = 5；SPS = 7；PPS = 8。空流返回空序列。
pub(crate) fn nal_unit_types(annexb: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut pos = 0;
    while pos < annexb.len() {
        match start_code_len_at(annexb, pos) {
            Some(code_len) => {
                let header_pos = pos + code_len;
                if let Some(&header) = annexb.get(header_pos) {
                    types.push(nal_unit_type(header));
                }
                // 至少越过 NAL 头字节，继续找下一个起始码。
                pos = header_pos + 1;
            }
            None => pos += 1,
        }
    }
    types
}

/// 是否含 IDR slice（nal_unit_type == 5）。
pub(crate) fn is_idr(annexb: &[u8]) -> bool {
    nal_unit_types(annexb).contains(&5)
}

/// 扫描 Annex-B 流，按 `(nal_unit_type, 载荷切片)` 逐 NAL 返回。
///
/// 载荷不含起始码（含 NAL 头字节）。E6 解码验证用它提取 SPS/PPS 与
/// 重建 AVCC。空载荷（紧邻的起始码）与非法位置直接跳过，绝不越界。
pub(crate) fn nal_payloads(annexb: &[u8]) -> Vec<(u8, &[u8])> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < annexb.len() {
        match start_code_len_at(annexb, pos) {
            Some(code_len) => {
                let payload_start = pos + code_len;
                // 载荷延伸到下一个起始码（或流尾）。
                let mut next = payload_start;
                while next < annexb.len() && start_code_len_at(annexb, next).is_none() {
                    next += 1;
                }
                if let Some((header, payload)) = annexb
                    .get(payload_start)
                    .zip(annexb.get(payload_start..next))
                {
                    out.push((nal_unit_type(*header), payload));
                }
                // next >= payload_start = pos + code_len >= pos + 3：必然前进。
                pos = next;
            }
            None => pos += 1,
        }
    }
    out
}

/// Annex-B → AVCC：每个 NAL 的起始码替换为 `length_size` 字节大端长度前缀。
///
/// [`to_length_prefixed`] 是 [`convert_avcc`](normalize_to_annexb) 的逆变换：
/// E6 解码验证把本 crate 输出的 Annex-B 帧重排回 VideoToolbox 解码器期望的
/// 长度前缀形态。输入必须以起始码开头且含至少一个非空 NAL，长度前缀
/// 越界（超过 `length_size` 字节可表示的范围）返回 `None`。
pub(crate) fn to_length_prefixed(annexb: &[u8], length_size: usize) -> Option<Vec<u8>> {
    if length_size == 0 || length_size > 8 {
        return None;
    }
    let nals = nal_payloads(annexb);
    if nals.is_empty() {
        return None;
    }
    let max_len: u128 = (1u128 << (8 * length_size)) - 1;
    let mut out = Vec::with_capacity(annexb.len());
    for (_, payload) in nals {
        let len = payload.len() as u128;
        if len > max_len {
            return None;
        }
        let bytes = len.to_be_bytes();
        out.extend_from_slice(&bytes[(16 - length_size)..]);
        out.extend_from_slice(payload);
    }
    Some(out)
}

/// 是否同时含 SPS（7）与 PPS（8）—— 一帧可独立解码的前提。
pub(crate) fn has_parameter_sets(annexb: &[u8]) -> bool {
    let types = nal_unit_types(annexb);
    types.contains(&7) && types.contains(&8)
}

/// 把输入裸流规整成 4 字节起始码的 Annex-B。
///
/// 判定顺序：**严格 AVCC 优先，再尝试 Annex-B**。
///
/// 为什么不能先查 `00 00 01` 起始码：VideoToolbox 输出 4 字节大端长度前缀 AVCC，
/// 任何长度落在 `256..=511`（`0x00000100..=0x000001FF`）的 NAL（如静屏/低纹理
/// P 帧，例如 328 字节 `00 00 01 48`）其前 3 字节恰好等于 Annex-B 3 字节起始码
/// `00 00 01`。若先按起始码判定，会把第 4 字节 `0x48` 当成损坏的 NAL 头灌给解码器。
/// 严格 AVCC 校验要求所有 NAL 长度之和**精确等于 `input.len()`**，只有在严格 AVCC
/// 不成立（或为单字节 4 字节起始码 `[00 00 00 01 XX]`）时才走 Annex-B 规整。
///
/// 返回 `None` 表示输入既非合法 AVCC 也非合法 Annex-B（长度越界 / 空 NAL），
/// 此时绝不能把可疑字节发给解码端。
pub(crate) fn normalize_to_annexb(
    input: &[u8],
    length_size: usize,
) -> Option<(Vec<u8>, SourceFormat)> {
    if length_size == 0 || length_size > 8 {
        return None;
    }
    if input.is_empty() {
        return None;
    }
    if let Some(converted) = convert_avcc(input, length_size) {
        return Some((converted, SourceFormat::Avcc));
    }
    if start_code_len_at(input, 0).is_some() {
        let normalized = normalize_annexb(input)?;
        Some((normalized, SourceFormat::AnnexB))
    } else {
        None
    }
}

/// 把 SPS/PPS（裸 NAL 字节，无起始码、无长度前缀）插入 Annex-B 流头部。
///
/// 只对 IDR 帧调用：P 帧不需要内联参数集，重复携带纯属浪费带宽。
pub(crate) fn prepend_parameter_sets(data: &mut Vec<u8>, parameter_sets: &[&[u8]]) {
    let mut with_ps =
        Vec::with_capacity(data.len() + parameter_sets.iter().map(|p| p.len() + 4).sum::<usize>());
    for ps in parameter_sets {
        with_ps.extend_from_slice(&ANNEXB_START_CODE);
        with_ps.extend_from_slice(ps);
    }
    with_ps.append(data);
    *data = with_ps;
}

/// Annex-B 输入的规整：按起始码切分，统一重发 4 字节起始码。
fn normalize_annexb(input: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len());
    let mut pos = 0;
    while pos < input.len() {
        let code_len = start_code_len_at(input, pos)?;
        let payload_start = pos + code_len;
        // 载荷延伸到下一个起始码（或流尾）。
        let mut next = payload_start;
        while next < input.len() && start_code_len_at(input, next).is_none() {
            next += 1;
        }
        let payload = input.get(payload_start..next)?;
        if payload.is_empty() {
            // 起始码后没有任何字节：不是合法 NAL 序列。
            return None;
        }
        out.extend_from_slice(&ANNEXB_START_CODE);
        out.extend_from_slice(payload);
        pos = next;
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// 严格 AVCC 输入转换：`[长度前缀][NAL]...` → `[起始码][NAL]...`。
///
/// 要求所有 NAL 长度均为正且严格拼满整个 `input`；不做流中途 `00 00 01`
/// 猜测（否则长度在 `256..=511` 的后续 NAL 会被误判为 3 字节起始码）。
fn convert_avcc(input: &[u8], length_size: usize) -> Option<Vec<u8>> {
    // 5 字节的 `[00, 00, 00, 01, X]` 优先视为 4 字节起始码的 Annex-B 单字节 NAL。
    if length_size == 4 && input.len() == 5 && input.starts_with(&ANNEXB_START_CODE) {
        return None;
    }
    let mut out = Vec::with_capacity(input.len());
    let mut pos = 0;
    while pos < input.len() {
        let len_bytes = input.get(pos..pos + length_size)?;
        let mut nal_len = 0usize;
        for &b in len_bytes {
            nal_len = (nal_len << 8) | b as usize;
        }
        if nal_len == 0 {
            // 长度为 0 的“NAL”必然是误读的前缀（比如把起始码读成了长度）。
            return None;
        }
        let end = pos.checked_add(length_size)?.checked_add(nal_len)?;
        let payload = input.get(pos + length_size..end)?;
        out.extend_from_slice(&ANNEXB_START_CODE);
        out.extend_from_slice(payload);
        pos = end;
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPS / PPS / IDR slice 的合成 NAL（头字节 0x67 / 0x68 / 0x65 + 载荷）。
    fn nal(header: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![header];
        v.extend_from_slice(payload);
        v
    }

    fn annexb_stream(nals: &[Vec<u8>]) -> Vec<u8> {
        let mut v = Vec::new();
        for n in nals {
            v.extend_from_slice(&ANNEXB_START_CODE);
            v.extend_from_slice(n);
        }
        v
    }

    fn avcc_stream(nals: &[Vec<u8>]) -> Vec<u8> {
        let mut v = Vec::new();
        for n in nals {
            v.extend_from_slice(&(n.len() as u32).to_be_bytes());
            v.extend_from_slice(n);
        }
        v
    }

    /// AVCC 输入：长度前缀全部替换为起始码，来源标记为 Avcc。
    #[test]
    fn converts_length_prefixed_avcc_input() {
        let sps = nal(0x67, &[1, 2, 3]);
        let pps = nal(0x68, &[4, 5]);
        let idr = nal(0x65, &[9; 16]);
        let input = avcc_stream(&[sps.clone(), pps.clone(), idr.clone()]);

        let (out, fmt) = normalize_to_annexb(&input, 4).expect("合法 AVCC 必须转换成功");
        assert_eq!(fmt, SourceFormat::Avcc);
        assert_eq!(out, annexb_stream(&[sps, pps, idr]));
        // 结构可解析：SPS、PPS、IDR 顺序出现。
        assert_eq!(nal_unit_types(&out), vec![7, 8, 5]);
    }

    /// 已经是 Annex-B 的输入：直通规整（3 字节起始码升为 4 字节），来源标记 Annex-B。
    #[test]
    fn passthrough_normalizes_annexb_input() {
        let idr = nal(0x65, &[7; 8]);
        let p = nal(0x41, &[1]);

        // 4 字节起始码：输出与输入逐字节一致。
        let input4 = annexb_stream(&[idr.clone(), p.clone()]);
        let (out4, fmt4) = normalize_to_annexb(&input4, 4).expect("Annex-B 必须直通");
        assert_eq!(fmt4, SourceFormat::AnnexB);
        assert_eq!(out4, input4);

        // 3 字节起始码：规整为 4 字节。
        let mut input3 = Vec::new();
        input3.extend_from_slice(&[0x00, 0x00, 0x01]);
        input3.extend_from_slice(&idr);
        let (out3, fmt3) = normalize_to_annexb(&input3, 4).expect("3 字节起始码也是合法 Annex-B");
        assert_eq!(fmt3, SourceFormat::AnnexB);
        assert_eq!(out3, annexb_stream(&[idr]));
    }

    /// 关键帧与参数集判定：IDR=5 判关键帧；SPS+PPS 齐备才算可独立解码。
    #[test]
    fn detects_idr_and_parameter_sets() {
        let idr_stream = annexb_stream(&[nal(0x67, &[1]), nal(0x68, &[1]), nal(0x65, &[2; 4])]);
        assert!(is_idr(&idr_stream));
        assert!(has_parameter_sets(&idr_stream));

        let p_stream = annexb_stream(&[nal(0x41, &[1])]);
        assert!(!is_idr(&p_stream), "非 IDR slice（type 1）不能判为关键帧");
        assert!(!has_parameter_sets(&p_stream), "缺 SPS/PPS 必须能被发现");

        // 只有 SPS 没有 PPS：参数集不齐。
        let sps_only = annexb_stream(&[nal(0x67, &[1]), nal(0x41, &[1])]);
        assert!(!has_parameter_sets(&sps_only));
    }

    /// 畸形输入必须返回 None，绝不能产出可疑码流：
    /// - 长度前缀越界（声明长度超过剩余字节）；
    /// - 长度前缀为 0（把起始码误读成长度的典型形态）；
    /// - 非法 length_size；
    /// - 空输入。
    #[test]
    fn rejects_malformed_input() {
        // 声明长度 64，实际只剩 4 字节。
        let truncated = {
            let mut v = 64u32.to_be_bytes().to_vec();
            v.extend_from_slice(&[0x65, 1, 2, 3]);
            v
        };
        assert!(normalize_to_annexb(&truncated, 4).is_none());

        // 长度前缀为 0。
        let zero_len = {
            let mut v = 0u32.to_be_bytes().to_vec();
            v.extend_from_slice(&[0x65, 1]);
            v
        };
        assert!(normalize_to_annexb(&zero_len, 4).is_none());

        assert!(
            normalize_to_annexb(&[0x65, 1], 0).is_none(),
            "length_size=0 非法"
        );
        assert!(
            normalize_to_annexb(&[0x65, 1], 9).is_none(),
            "length_size>8 非法"
        );
        assert!(normalize_to_annexb(&[], 4).is_none(), "空输入非法");
    }

    /// 参数集前插只动头部：原有 NAL 逐字节保留在其后。
    #[test]
    fn prepends_parameter_sets_in_order() {
        let sps = nal(0x67, &[1]);
        let pps = nal(0x68, &[2]);
        let idr = nal(0x65, &[3]);
        let mut body = annexb_stream(std::slice::from_ref(&idr));

        prepend_parameter_sets(&mut body, &[&sps, &pps]);
        assert_eq!(body, annexb_stream(&[sps, pps, idr]));
        assert_eq!(nal_unit_types(&body), vec![7, 8, 5]);
        assert!(has_parameter_sets(&body));
    }

    /// 逐 NAL 提取：类型与载荷一一对应，载荷不含起始码、含头字节。
    #[test]
    fn extracts_nal_payloads_with_types() {
        let sps = nal(0x67, &[1, 2]);
        let pps = nal(0x68, &[3]);
        let idr = nal(0x65, &[9; 12]);
        let stream = annexb_stream(&[sps.clone(), pps.clone(), idr.clone()]);

        let payloads = nal_payloads(&stream);
        assert_eq!(
            payloads,
            vec![
                (7u8, sps.as_slice()),
                (8u8, pps.as_slice()),
                (5u8, idr.as_slice())
            ],
            "(类型, 载荷) 必须与输入 NAL 逐个对应"
        );
        // 3 字节起始码同样可解析（规整前的流也能直接提取）。
        let mut stream3 = Vec::new();
        stream3.extend_from_slice(&[0x00, 0x00, 0x01]);
        stream3.extend_from_slice(&idr);
        assert_eq!(nal_payloads(&stream3), vec![(5u8, idr.as_slice())]);
        // 空流与不含起始码的字节：返回空（不越界、不产假 NAL）。
        assert!(nal_payloads(&[]).is_empty());
        assert!(nal_payloads(&[0x65, 1, 2]).is_empty());
    }

    /// Annex-B → AVCC 是 convert_avcc 的逆变换：往返（AVCC → Annex-B → AVCC）
    /// 必须无损；畸形输入（无 NAL、非法 length_size）返回 None。
    #[test]
    fn converts_annexb_back_to_length_prefixed() {
        let sps = nal(0x67, &[1, 2, 3]);
        let pps = nal(0x68, &[4, 5]);
        let idr = nal(0x65, &[7; 16]);
        let original = avcc_stream(&[sps.clone(), pps.clone(), idr.clone()]);

        let (annexb, _) = normalize_to_annexb(&original, 4).expect("AVCC → Annex-B 必须成功");
        let back = to_length_prefixed(&annexb, 4).expect("Annex-B → AVCC 必须成功");
        assert_eq!(back, original, "往返必须逐字节还原（逆变换无损）");

        // 非默认长度前缀（2 字节）同样支持。
        let short = to_length_prefixed(&annexb, 2).expect("2 字节长度前缀合法");
        let mut expected = Vec::new();
        for n in [&sps, &pps, &idr] {
            expected.extend_from_slice(&(n.len() as u16).to_be_bytes());
            expected.extend_from_slice(n);
        }
        assert_eq!(short, expected);

        // 畸形输入：无起始码（不是 Annex-B）、空流、非法 length_size。
        assert!(
            to_length_prefixed(&original, 4).is_none(),
            "长度前缀输入不是 Annex-B"
        );
        assert!(to_length_prefixed(&[], 4).is_none());
        assert!(to_length_prefixed(&annexb, 0).is_none());
        assert!(to_length_prefixed(&annexb, 9).is_none());
    }

    /// 回归测试：长度在 256..=511 的 4 字节 AVCC NAL（前缀 `00 00 01 XX`，如 328 字节
    /// `00 00 01 48`）绝不能被误判为 3 字节 Annex-B 起始码导致 `XX` 变成损坏 NAL 头。
    #[test]
    fn avcc_nal_length_256_to_511_not_misclassified_as_annexb() {
        // 1) 单 NAL：328 字节（0x00000148）P-slice（头 0x21）
        let p_slice = nal(0x21, &[0xE0; 327]);
        assert_eq!(p_slice.len(), 328);
        let single_avcc = avcc_stream(std::slice::from_ref(&p_slice));
        assert_eq!(&single_avcc[..4], &[0x00, 0x00, 0x01, 0x48]);

        let (out, fmt) = normalize_to_annexb(&single_avcc, 4).expect("328 字节 AVCC 必须成功转换");
        assert_eq!(fmt, SourceFormat::Avcc);
        assert_eq!(out, annexb_stream(std::slice::from_ref(&p_slice)));
        assert_eq!(nal_unit_types(&out), vec![1]);

        // 2) 多 NAL：首 NAL 正常，次 NAL 长度 300（0x0000012C）也不能被中途误判
        let sei = nal(0x06, &[1, 2, 3]);
        let p_mid = nal(0x41, &[0xAB; 299]);
        assert_eq!(p_mid.len(), 300);
        let multi_avcc = avcc_stream(&[sei.clone(), p_mid.clone()]);
        let (out2, fmt2) = normalize_to_annexb(&multi_avcc, 4).expect("多 NAL AVCC 必须成功转换");
        assert_eq!(fmt2, SourceFormat::Avcc);
        assert_eq!(out2, annexb_stream(&[sei, p_mid]));
        assert_eq!(nal_unit_types(&out2), vec![6, 1]);
    }
}
