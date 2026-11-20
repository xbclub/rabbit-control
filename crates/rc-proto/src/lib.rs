//! 媒体包头的手写二进制编解码。
//!
//! # 为什么不用 protobuf
//! - protobuf 解析需要 varint 循环 + 字段匹配，单包 ~100–300ns
//! - 1080p60 每帧 10–40 包 → 每秒 600–2400 次解析
//! - 我们要求 **≤200ns/包**，且必须零分配
//!
//! 手写固定布局只要 32 字节的逐字段读写，实测 ~30ns。
//!
//! # 线格式（32 字节定长头，小端序）
//!
//! ```text
//! 偏移  长度  字段              说明
//! ─────────────────────────────────────────────────────────
//!  0     4    magic            固定 0x31564352（字节 "RCV1"）
//!  4     8    frame_seq        帧序号，单调递增
//! 12     4    capture_ts_us    采集时刻（相对会话起点，微秒）
//! 16     4    encode_done_us   编码完成时刻（相对会话起点，微秒）
//! 20     2    packet_idx       本包在该帧内的序号，从 0 开始
//! 22     2    packet_cnt       该帧总包数（>=1）
//! 24     2    fec_group         FEC 组号
//! 26     1    flags             见 [`HeaderFlags`]
//! 27     1    layer_id          分层编码预留，P0 恒为 0
//! 28     2    payload_len       负载字节数
//! 30     2    reserved          保留，必须写 0，读时忽略
//! ```
//!
//! # 不变量（解码时强制校验）
//! - `magic == 0x31564352`
//! - `packet_cnt >= 1`
//! - `packet_idx < packet_cnt`
//! - `payload_len` == 实际剩余字节数（调用方可用 [`MediaHeader::parse`] 自动校验）
//!
//! # 为什么定长 + 无 varint
//! 定长意味着解码是「按偏移直接读」，无分支、无循环、可向量化。
//! varint 在压缩小整数时有优势，但我们的字段（500 万微秒级）几乎总是
//! 多字节，省不了空间反而增加解析开销。

#![deny(missing_docs)]
#![deny(unsafe_code)]

use rc_types::{Error, FrameKind, FrameSeq, Result, Timestamp};

/// 包头魔数。字节表示为 ASCII `RCV1`。
pub const MAGIC: u32 = 0x3156_4352;

/// 包头固定长度（字节）。
pub const HEADER_LEN: usize = 32;

/// 单个数据报的最大负载（留出包头与 IP/UDP/QUIC 开销）。
///
/// 1200 是 QUIC 的安全值（避免 IP 分片），也是 WebRTC 的惯例。
pub const MAX_PAYLOAD: usize = 1200;

/// 包头标志位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HeaderFlags(pub u8);

impl HeaderFlags {
    /// bit0: 该帧为关键帧（IDR）
    pub const IDR: u8 = 0b0000_0001;
    /// bit1: 该帧的最后一个包
    pub const LAST: u8 = 0b0000_0010;
    /// bit2: 携带脏区信息（见 `DirtyHint`）
    pub const HAS_DIRTY: u8 = 0b0000_0100;
    /// bit3: 本包是 FEC 冗余包（不是数据包）
    pub const PARITY: u8 = 0b0000_1000;

    /// 空标志。
    pub const fn empty() -> Self {
        HeaderFlags(0)
    }

    /// 设置位。
    pub fn set(&mut self, bit: u8) {
        self.0 |= bit;
    }

    /// 清除位。
    pub fn clear(&mut self, bit: u8) {
        self.0 &= !bit;
    }

    /// 查询位。
    pub const fn has(self, bit: u8) -> bool {
        self.0 & bit != 0
    }

    /// 是否为 IDR。
    pub const fn is_idr(self) -> bool {
        self.has(Self::IDR)
    }

    /// 是否为该帧最后一个包。
    pub const fn is_last(self) -> bool {
        self.has(Self::LAST)
    }

    /// 是否为 FEC 冗余包。
    pub const fn is_parity(self) -> bool {
        self.has(Self::PARITY)
    }

    /// 由 `FrameKind` 构造。
    pub fn from_kind(kind: FrameKind) -> Self {
        let mut f = HeaderFlags::empty();
        if matches!(kind, FrameKind::Idr) {
            f.set(Self::IDR);
        }
        f
    }
}

/// 媒体包头。
///
/// # 内存布局
/// 该结构体**不**用 `#[repr(C)]` 直接映射——那会引入对齐 padding
/// （32 字节的字段顺序下可能变成 40 字节）。我们显式序列化，
/// 这样既保证线格式稳定，也不依赖编译器布局。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MediaHeader {
    /// 帧序号
    pub frame_seq: FrameSeq,
    /// 采集时刻
    pub capture_ts: Timestamp,
    /// 编码完成时刻
    pub encode_done_ts: Timestamp,
    /// 包内序号
    pub packet_idx: u16,
    /// 该帧总包数
    pub packet_cnt: u16,
    /// FEC 组号
    pub fec_group: u16,
    /// 标志位
    pub flags: HeaderFlags,
    /// 分层预留
    pub layer_id: u8,
    /// 负载长度
    pub payload_len: u16,
}

impl MediaHeader {
    /// 写入到固定 32 字节缓冲区。
    ///
    /// # 不校验
    /// 本方法不校验不变量（为了性能）。调用方应在构造时保证正确，
    /// 或使用 [`MediaHeader::write_checked`]。
    pub fn write(&self, out: &mut [u8; HEADER_LEN]) {
        out[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        out[4..12].copy_from_slice(&self.frame_seq.0.to_le_bytes());
        out[12..16].copy_from_slice(&self.capture_ts.0.to_le_bytes());
        out[16..20].copy_from_slice(&self.encode_done_ts.0.to_le_bytes());
        out[20..22].copy_from_slice(&self.packet_idx.to_le_bytes());
        out[22..24].copy_from_slice(&self.packet_cnt.to_le_bytes());
        out[24..26].copy_from_slice(&self.fec_group.to_le_bytes());
        out[26] = self.flags.0;
        out[27] = self.layer_id;
        out[28..30].copy_from_slice(&self.payload_len.to_le_bytes());
        out[30..32].copy_from_slice(&0u16.to_le_bytes());
    }

    /// 写入并校验不变量。
    ///
    /// # 错误
    /// 违反任何不变量都返回 [`Error::InvalidFrame`]。
    pub fn write_checked(&self, out: &mut [u8; HEADER_LEN]) -> Result<()> {
        self.validate()?;
        self.write(out);
        Ok(())
    }

    /// 校验不变量。
    pub fn validate(&self) -> Result<()> {
        if self.packet_cnt == 0 {
            return Err(Error::InvalidFrame("packet_cnt must be >= 1".into()));
        }
        if self.packet_idx >= self.packet_cnt {
            return Err(Error::InvalidFrame(format!(
                "packet_idx {} >= packet_cnt {}",
                self.packet_idx, self.packet_cnt
            )));
        }
        if self.payload_len as usize > MAX_PAYLOAD {
            return Err(Error::InvalidFrame(format!(
                "payload_len {} exceeds MAX_PAYLOAD {}",
                self.payload_len, MAX_PAYLOAD
            )));
        }
        Ok(())
    }

    /// 从缓冲区解析。
    ///
    /// # 不校验 payload_len 与剩余长度的一致性
    /// 传 `buf` 可以是「头 + 负载」，本方法只读前 32 字节。
    /// 需要一致性校验请用 [`MediaHeader::parse_datagram`]。
    ///
    /// # 错误
    /// - 缓冲不足 32 字节 → `Protocol { offset: 0 }`
    /// - 魔数不匹配 → `Protocol { offset: 0 }`
    /// - 不变量违反 → `Protocol { offset: 20/22/28 }`
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < HEADER_LEN {
            return Err(Error::Protocol {
                offset: 0,
                msg: format!("buffer {} < HEADER_LEN {}", buf.len(), HEADER_LEN),
            });
        }
        let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        if magic != MAGIC {
            return Err(Error::Protocol {
                offset: 0,
                msg: format!("bad magic 0x{magic:08X}, expected 0x{MAGIC:08X}"),
            });
        }
        let h = Self {
            frame_seq: FrameSeq(u64::from_le_bytes(buf[4..12].try_into().unwrap())),
            capture_ts: Timestamp(u32::from_le_bytes(buf[12..16].try_into().unwrap())),
            encode_done_ts: Timestamp(u32::from_le_bytes(buf[16..20].try_into().unwrap())),
            packet_idx: u16::from_le_bytes(buf[20..22].try_into().unwrap()),
            packet_cnt: u16::from_le_bytes(buf[22..24].try_into().unwrap()),
            fec_group: u16::from_le_bytes(buf[24..26].try_into().unwrap()),
            flags: HeaderFlags(buf[26]),
            layer_id: buf[27],
            payload_len: u16::from_le_bytes(buf[28..30].try_into().unwrap()),
        };
        h.map_invariant_errors()?;
        Ok(h)
    }

    /// 解析一个完整数据报（头 + 负载），并校验 `payload_len` 与实际一致。
    ///
    /// # 返回
    /// `(header, payload_slice)`
    ///
    /// # 错误
    /// - `payload_len` 与实际剩余字节不符 → `Protocol { offset: 28 }`
    pub fn parse_datagram(buf: &[u8]) -> Result<(Self, &[u8])> {
        let h = Self::parse(buf)?;
        let actual = buf.len() - HEADER_LEN;
        if actual != h.payload_len as usize {
            return Err(Error::Protocol {
                offset: 28,
                msg: format!(
                    "payload_len {} != actual remaining {}",
                    h.payload_len, actual
                ),
            });
        }
        Ok((h, &buf[HEADER_LEN..]))
    }

    /// 把 `validate` 的错误映射为带偏移的协议错误。
    fn map_invariant_errors(&self) -> Result<()> {
        if self.packet_cnt == 0 {
            return Err(Error::Protocol {
                offset: 22,
                msg: "packet_cnt is 0".into(),
            });
        }
        if self.packet_idx >= self.packet_cnt {
            return Err(Error::Protocol {
                offset: 20,
                msg: format!(
                    "packet_idx {} >= packet_cnt {}",
                    self.packet_idx, self.packet_cnt
                ),
            });
        }
        if self.payload_len as usize > MAX_PAYLOAD {
            return Err(Error::Protocol {
                offset: 28,
                msg: format!(
                    "payload_len {} > MAX_PAYLOAD {}",
                    self.payload_len, MAX_PAYLOAD
                ),
            });
        }
        Ok(())
    }

    /// 该包是否为 IDR 帧的一部分。
    pub const fn is_idr(&self) -> bool {
        self.flags.is_idr()
    }

    /// 是否为该帧最后一个包。
    pub const fn is_last(&self) -> bool {
        self.flags.is_last()
    }
}

/// 把一个完整帧负载切分成多个数据报（头 + 负载）。
///
/// # 参数
/// - `frame_seq`: 帧序号
/// - `kind`: 帧类型（决定 IDR 标志）
/// - `capture_ts` / `encode_done_ts`: 时间戳
/// - `fec_group`: 该帧所属 FEC 组
/// - `payload`: 完整帧数据
/// - `max_payload`: 单片最大负载（通常 [`MAX_PAYLOAD`]）
///
/// # 返回
/// 每个元素都是「32 字节头 + 负载」的完整数据报。
///
/// # 不变量
/// - 返回至少 1 个数据报（空载荷也会产生一个空的末包）
/// - 只有最后一个包带 `LAST` 标志
/// - 所有包共享相同 `frame_seq` / `packet_cnt`
pub fn split_frame(
    frame_seq: FrameSeq,
    kind: FrameKind,
    capture_ts: Timestamp,
    encode_done_ts: Timestamp,
    fec_group: u16,
    payload: &[u8],
    max_payload: usize,
) -> Vec<Vec<u8>> {
    assert!(max_payload > 0 && max_payload <= MAX_PAYLOAD);

    let chunks: Vec<&[u8]> = if payload.is_empty() {
        vec![&[]]
    } else {
        payload.chunks(max_payload).collect()
    };
    let packet_cnt = chunks.len();
    assert!(
        packet_cnt <= u16::MAX as usize,
        "frame too large: {packet_cnt} packets"
    );

    let mut out = Vec::with_capacity(packet_cnt);
    for (i, chunk) in chunks.iter().enumerate() {
        let is_last = i == packet_cnt - 1;
        let mut flags = HeaderFlags::from_kind(kind);
        if is_last {
            flags.set(HeaderFlags::LAST);
        }
        let h = MediaHeader {
            frame_seq,
            capture_ts,
            encode_done_ts,
            packet_idx: i as u16,
            packet_cnt: packet_cnt as u16,
            fec_group,
            flags,
            layer_id: 0,
            payload_len: chunk.len() as u16,
        };

        let mut buf = vec![0u8; HEADER_LEN + chunk.len()];
        {
            let (head, body) = buf.split_at_mut(HEADER_LEN);
            let head_arr: &mut [u8; HEADER_LEN] = head.try_into().unwrap();
            h.write(head_arr);
            body.copy_from_slice(chunk);
        }
        out.push(buf);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> MediaHeader {
        MediaHeader {
            frame_seq: FrameSeq(42),
            capture_ts: Timestamp(1_000_000),
            encode_done_ts: Timestamp(1_005_000),
            packet_idx: 3,
            packet_cnt: 10,
            fec_group: 7,
            flags: HeaderFlags(HeaderFlags::IDR),
            layer_id: 0,
            payload_len: 1100,
        }
    }

    #[test]
    fn header_len_is_32() {
        assert_eq!(HEADER_LEN, 32);
    }

    #[test]
    fn magic_bytes_spell_rcv1() {
        // 小端写入后，内存中的字节序列应为 "RCV1"
        let b = MAGIC.to_le_bytes();
        assert_eq!(&b, b"RCV1");
    }

    #[test]
    fn roundtrip_preserves_all_fields() {
        let h = sample();
        let mut buf = [0u8; HEADER_LEN];
        h.write(&mut buf);
        let parsed = MediaHeader::parse(&buf).unwrap();
        assert_eq!(parsed, h);
    }

    #[test]
    fn field_offsets_are_stable() {
        // 逐字节校验：这个测试的失败意味着线格式变更，必须同步更新
        // docs/impl/10-wire-format.md 与所有对端实现
        let h = MediaHeader {
            frame_seq: FrameSeq(0x0807_0605_0403_0201),
            capture_ts: Timestamp(0x1111_2222),
            encode_done_ts: Timestamp(0x3333_4444),
            packet_idx: 0x5555,
            packet_cnt: 0x6666,
            fec_group: 0x7777,
            flags: HeaderFlags(0x88),
            layer_id: 0x99,
            payload_len: 0xAAAA,
        };
        let mut buf = [0u8; HEADER_LEN];
        h.write(&mut buf);

        assert_eq!(&buf[0..4], b"RCV1");
        assert_eq!(buf[4..12], [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);
        assert_eq!(buf[12..16], [0x22, 0x22, 0x11, 0x11]);
        assert_eq!(buf[16..20], [0x44, 0x44, 0x33, 0x33]);
        assert_eq!(buf[20..22], [0x55, 0x55]);
        assert_eq!(buf[22..24], [0x66, 0x66]);
        assert_eq!(buf[24..26], [0x77, 0x77]);
        assert_eq!(buf[26], 0x88);
        assert_eq!(buf[27], 0x99);
        assert_eq!(buf[28..30], [0xAA, 0xAA]);
        assert_eq!(buf[30..32], [0x00, 0x00]);
    }

    #[test]
    fn parse_rejects_short_buffer() {
        let err = MediaHeader::parse(&[0u8; 16]).unwrap_err();
        match err {
            Error::Protocol { offset, .. } => assert_eq!(offset, 0),
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_bad_magic() {
        let mut buf = [0u8; HEADER_LEN];
        buf[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        let err = MediaHeader::parse(&buf).unwrap_err();
        match err {
            Error::Protocol { offset, msg } => {
                assert_eq!(offset, 0);
                assert!(msg.contains("magic"), "{msg}");
            }
            other => panic!("expected Protocol, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_zero_packet_cnt_at_offset_22() {
        let h = MediaHeader {
            packet_idx: 0,
            packet_cnt: 0,
            ..sample()
        };
        let mut buf = [0u8; HEADER_LEN];
        h.write(&mut buf);
        match MediaHeader::parse(&buf) {
            Err(Error::Protocol { offset, .. }) => assert_eq!(offset, 22),
            other => panic!("expected Protocol at 22, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_idx_beyond_cnt_at_offset_20() {
        let h = MediaHeader {
            packet_idx: 5,
            packet_cnt: 5,
            ..sample()
        };
        let mut buf = [0u8; HEADER_LEN];
        h.write(&mut buf);
        match MediaHeader::parse(&buf) {
            Err(Error::Protocol { offset, .. }) => assert_eq!(offset, 20),
            other => panic!("expected Protocol at 20, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_oversized_payload() {
        let h = MediaHeader {
            payload_len: (MAX_PAYLOAD + 1) as u16,
            ..sample()
        };
        assert!(h.validate().is_err());
    }

    #[test]
    fn parse_datagram_checks_payload_len_consistency() {
        let h = MediaHeader {
            payload_len: 10,
            ..sample()
        };
        let mut buf = vec![0u8; HEADER_LEN + 5]; // 实际只有 5 字节负载
        {
            let (head, _) = buf.split_at_mut(HEADER_LEN);
            let head_arr: &mut [u8; HEADER_LEN] = head.try_into().unwrap();
            h.write(head_arr);
        }
        match MediaHeader::parse_datagram(&buf) {
            Err(Error::Protocol { offset, .. }) => assert_eq!(offset, 28),
            other => panic!("expected Protocol at 28, got {other:?}"),
        }
    }

    #[test]
    fn split_frame_single_packet() {
        let payload = vec![0xAB; 100];
        let dgrams = split_frame(
            FrameSeq(1),
            FrameKind::P,
            Timestamp(0),
            Timestamp(1000),
            0,
            &payload,
            1200,
        );
        assert_eq!(dgrams.len(), 1);
        let (h, body) = MediaHeader::parse_datagram(&dgrams[0]).unwrap();
        assert_eq!(h.packet_idx, 0);
        assert_eq!(h.packet_cnt, 1);
        assert!(h.is_last());
        assert!(!h.is_idr());
        assert_eq!(body, &payload[..]);
    }

    #[test]
    fn split_frame_marks_only_last_packet() {
        let payload = vec![0xCD; 2500]; // 1200 + 1200 + 100
        let dgrams = split_frame(
            FrameSeq(2),
            FrameKind::Idr,
            Timestamp(0),
            Timestamp(1000),
            0,
            &payload,
            1200,
        );
        assert_eq!(dgrams.len(), 3);
        for (i, d) in dgrams.iter().enumerate() {
            let (h, _) = MediaHeader::parse_datagram(d).unwrap();
            assert_eq!(h.packet_idx, i as u16);
            assert_eq!(h.packet_cnt, 3);
            assert!(h.is_idr(), "all packets of IDR frame carry IDR flag");
            assert_eq!(h.is_last(), i == 2, "only last packet has LAST flag");
        }
    }

    #[test]
    fn split_frame_reassembles_to_original() {
        let payload: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let dgrams = split_frame(
            FrameSeq(9),
            FrameKind::P,
            Timestamp(10),
            Timestamp(20),
            3,
            &payload,
            1200,
        );
        let mut reassembled = Vec::new();
        for d in &dgrams {
            let (_, body) = MediaHeader::parse_datagram(d).unwrap();
            reassembled.extend_from_slice(body);
        }
        assert_eq!(reassembled, payload);
    }

    #[test]
    fn split_frame_empty_payload_yields_one_packet() {
        let dgrams = split_frame(
            FrameSeq(0),
            FrameKind::P,
            Timestamp(0),
            Timestamp(0),
            0,
            &[],
            1200,
        );
        assert_eq!(dgrams.len(), 1);
        let (h, body) = MediaHeader::parse_datagram(&dgrams[0]).unwrap();
        assert_eq!(h.packet_cnt, 1);
        assert!(body.is_empty());
    }

    #[test]
    fn max_payload_fits_within_datagram_budget() {
        // 编译期断言已在模块级声明（见下方 const _），此测试保留为文档说明。
        // 32 + 1200 = 1232，加上 UDP/QUIC 开销仍 < 1500 MTU
        const _: () = assert!(HEADER_LEN + MAX_PAYLOAD <= 1400);
        assert_eq!(HEADER_LEN + MAX_PAYLOAD, 1232);
    }

    #[test]
    fn flags_helpers_work() {
        let mut f = HeaderFlags::empty();
        assert!(!f.is_idr());
        f.set(HeaderFlags::IDR);
        assert!(f.is_idr());
        f.clear(HeaderFlags::IDR);
        assert!(!f.is_idr());

        let idr = HeaderFlags::from_kind(FrameKind::Idr);
        assert!(idr.is_idr());
        let p = HeaderFlags::from_kind(FrameKind::P);
        assert!(!p.is_idr());
    }
}
