//! 分片重组。
//!
//! # 职责
//! 按 `frame_seq` 累积分片，按 `packet_idx`（不是到达顺序，N5）拼出完整帧。
//! datagram 通道不保证顺序也不保证送达，所以：
//! - 每个分片落到 `packet_idx` 对应的槽位，乱序到达无影响；
//! - 凑齐 `packet_cnt` 个才交付；
//! - 超过 500ms 没凑齐的帧整体丢弃（N6），由调用方记 `LostInNetwork`——
//!   等一个不会来的分片毫无意义，正确动作是请求 IDR 重新来。
//!
//! # 并发上限 = 2（N10 / 架构 Q3）
//! [`DEFAULT_MAX_CONCURRENT_FRAMES`] = 2。这是**刻意**的：
//! 重组缓冲里同时挂着的帧越多，用户看到的画面越旧。RustDesk 的接收队列
//! 深达 120 帧（约 2 秒延迟），是它延迟问题的最大来源；我们反着来。
//! 上限满了之后新帧**直接拒绝**（丢弃其分片），而不是把旧帧挤出去重排——
//! 挤出重排会让「旧帧的残留分片」继续占用缓冲。
//!
//! # 时间注入（R7）
//! [`FrameAssembler::gc`] 由调用方注入 `now_us`。包的「到达时刻」采样自
//! 最近一次注入的时钟（`gc` 每次都会刷新它），因此**调用方必须每个事件
//! 循环调用一次 `gc`**——这正好也是防内存增长所需的节奏，一举两得。
//! 所有纯逻辑测试因此精确、无 sleep。

use std::collections::HashMap;

use rc_proto::MediaHeader;
use rc_types::{FrameKind, FrameSeq, Timestamp};

use crate::fec_adapter::FecReceiver;

/// **默认并发帧数 = 2**（对应架构文档 Q3）。不要改成 8 或更大。
/// 队列越深 = 用户看到的画面越旧。RustDesk 用 120 帧，代价是 2 秒延迟。
pub const DEFAULT_MAX_CONCURRENT_FRAMES: usize = 2;

/// 超过该时长仍未凑齐的帧被 gc 丢弃（N6）。
pub const FRAME_STALE_US: u32 = 500_000;

/// 凑齐后交付的完整帧。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledFrame {
    /// 帧序号
    pub seq: FrameSeq,
    /// 帧类型（IDR / P）
    pub kind: FrameKind,
    /// 采集时刻（来自分片包头）
    pub capture_ts: Timestamp,
    /// 编码完成时刻（来自分片包头）
    pub encode_done_ts: Timestamp,
    /// 按 `packet_idx` 顺序拼出的完整负载
    pub data: Vec<u8>,
}

/// 一个未凑齐的帧的部分状态。
struct PartialFrame {
    kind: FrameKind,
    capture_ts: Timestamp,
    encode_done_ts: Timestamp,
    /// 总分片数（来自包头，同一帧的所有分片一致）
    packet_cnt: u16,
    /// 按 `packet_idx` 索引的槽位；`None` = 该分片未到
    slots: Vec<Option<Vec<u8>>>,
    /// 已收到的分片数
    received: u16,
    /// 首个分片到达时的采样时钟（微秒，来自 gc 注入）
    first_seen_us: u32,
}

/// 按 frame_seq 累积分片，凑齐后交付完整帧。
pub struct FrameAssembler {
    /// 同时挂起的帧数上限（N10）
    max_concurrent_frames: usize,
    /// 在途帧，key = `frame_seq.0`
    frames: HashMap<u64, PartialFrame>,
    /// FEC 恢复（数据与冗余分片都在 push 里喂给它）
    fec: FecReceiver,
    /// 最近一次 gc 注入的时钟（也用作包到达时刻的采样）
    last_now_us: u32,
    /// 已结案（交付或 gc 判丢）的最大帧序号：更老帧的迟到分片直接拒收，
    /// 否则被判丢的帧会被残余分片「复活」，占用并发名额。
    highest_retired: Option<u64>,
}

impl FrameAssembler {
    /// 构造。`max_concurrent_frames` 默认传 [`DEFAULT_MAX_CONCURRENT_FRAMES`]。
    ///
    /// 传入 >2 的值是可以的（例如离线抓包分析），但**在线传输路径必须用 2**。
    pub fn new(max_concurrent_frames: usize) -> Self {
        Self {
            max_concurrent_frames: max_concurrent_frames.max(1),
            frames: HashMap::new(),
            fec: FecReceiver::new(),
            last_now_us: 0,
            highest_retired: None,
        }
    }

    /// 投入一个已解析的包。返回 `Some(完整帧)` 表示该帧已齐。
    ///
    /// - 冗余包（`PARITY` 标志）只喂 FEC 恢复器，不占帧槽位；
    /// - 数据包同时进帧槽位与 FEC 恢复器（恢复时要 XOR 它）；
    /// - 在途帧的迟到分片照常接受；已交付帧的重复分片被忽略；
    /// - 并发上限满时**新帧被拒**（其分片全部丢弃，N10）。
    pub fn push(&mut self, header: &MediaHeader, payload: &[u8]) -> Option<AssembledFrame> {
        self.fec.observe(header, payload);
        if header.flags.is_parity() {
            return None;
        }
        self.insert_packet(header, payload)
    }

    /// 请求 FEC 恢复（在 FEC 适配层调用）。
    ///
    /// 若该组恰好只缺 1 个数据分片且冗余已到，恢复出的分片会像正常到达
    /// 一样填进帧槽位；凑齐则返回 `Some(完整帧)`。缺 ≥2 时（N7）
    /// 永远返回 `None`——不要重试，正确动作是请求 IDR。
    pub fn try_fec_recover(&mut self, group: u16) -> Option<AssembledFrame> {
        let (_idx, wire) = self.fec.try_recover(group)?;
        // 恢复出的是完整数据报（头 + 负载），重新走插入路径。
        // 不再喂 fec.observe：它已经在恢复器里了。
        match MediaHeader::parse_datagram(&wire) {
            Ok((h, payload)) => self.insert_packet(&h, payload),
            Err(_) => None,
        }
    }

    /// 超时清理（超过 [`FRAME_STALE_US`] 未凑齐的帧丢弃）。
    ///
    /// 返回被丢弃帧的序号（升序）。调用方必须为每个序号记
    /// `Outcome::LostInNetwork`（N6：统计要准，埋点不能缺）。
    /// 同时注入时钟——这也是「到达时刻」的采样点。
    ///
    /// 时钟是 u32 微秒，约 71.5 分钟回绕一次：经过时间一律用
    /// [`u32::wrapping_sub`] 计算（真实经过时间远小于回绕周期，回绕点
    /// 天然安全）。`saturating_sub` 在回绕点会把经过时间算成 0，
    /// 让超时帧永远滞留——8 小时 soak 必踩。
    pub fn gc(&mut self, now_us: u32) -> Vec<FrameSeq> {
        self.last_now_us = now_us;
        let mut dropped: Vec<FrameSeq> = Vec::new();
        self.frames.retain(|seq, f| {
            let stale = now_us.wrapping_sub(f.first_seen_us) > FRAME_STALE_US;
            if stale {
                dropped.push(FrameSeq(*seq));
            }
            !stale
        });
        dropped.sort_by_key(|s| s.0);
        // 判丢即结案：残余分片不得复活该帧（复活会占用并发名额、
        // 且上层已经按 LostInNetwork 记账）。
        if let Some(max) = dropped.last().map(|s| s.0) {
            if self.highest_retired.map_or(true, |h| max > h) {
                self.highest_retired = Some(max);
            }
        }
        self.fec.gc(now_us);
        dropped
    }

    /// 当前在途帧数（诊断用）。
    pub fn in_flight_frames(&self) -> usize {
        self.frames.len()
    }

    /// 把一个数据分片放进帧槽位；凑齐即组装交付。
    fn insert_packet(&mut self, header: &MediaHeader, payload: &[u8]) -> Option<AssembledFrame> {
        let seq = header.frame_seq.0;
        let idx = header.packet_idx as usize;
        let cnt = header.packet_cnt as usize;
        if cnt == 0 || idx >= cnt {
            // parse 已挡掉非法头，这里防御直接丢弃（不 panic：恶意流量
            // 的目标是打爆我们，不是让我们崩）。
            return None;
        }

        if let Some(f) = self.frames.get_mut(&seq) {
            if f.slots[idx].is_none() {
                f.slots[idx] = Some(payload.to_vec());
                f.received += 1;
            }
            // 重复分片不计入 received，不会提前触发交付。
            if f.received == f.packet_cnt {
                let f = self.frames.remove(&seq).expect("just borrowed");
                return Some(self.assemble(seq, f));
            }
            return None;
        }

        // 新帧的三道闸门：迟到重复 → 并发上限 → 接受。
        if self.highest_retired.is_some_and(|h| seq <= h) {
            return None; // 该帧已结案（交付或判丢），这是迟到分片
        }
        if self.frames.len() >= self.max_concurrent_frames {
            return None; // N10：满了拒绝新帧，而不是无限增长
        }

        let mut slots = vec![None; cnt];
        slots[idx] = Some(payload.to_vec());
        let received = 1;
        if received == cnt {
            let f = PartialFrame {
                kind: kind_of(header),
                capture_ts: header.capture_ts,
                encode_done_ts: header.encode_done_ts,
                packet_cnt: header.packet_cnt,
                slots,
                received: received as u16,
                first_seen_us: self.last_now_us,
            };
            return Some(self.assemble(seq, f));
        }
        self.frames.insert(
            seq,
            PartialFrame {
                kind: kind_of(header),
                capture_ts: header.capture_ts,
                encode_done_ts: header.encode_done_ts,
                packet_cnt: header.packet_cnt,
                slots,
                received: received as u16,
                first_seen_us: self.last_now_us,
            },
        );
        None
    }

    /// 把凑齐的部分帧拼成 [`AssembledFrame`]。
    fn assemble(&mut self, seq: u64, f: PartialFrame) -> AssembledFrame {
        let total: usize = f
            .slots
            .iter()
            .map(|s| s.as_ref().map_or(0, |v| v.len()))
            .sum();
        let mut data = Vec::with_capacity(total);
        // N5：按 packet_idx 顺序拼接，与到达顺序无关。
        for bytes in f.slots.into_iter().flatten() {
            data.extend_from_slice(&bytes);
        }
        if self.highest_retired.map_or(true, |h| seq > h) {
            self.highest_retired = Some(seq);
        }
        AssembledFrame {
            seq: FrameSeq(seq),
            kind: f.kind,
            capture_ts: f.capture_ts,
            encode_done_ts: f.encode_done_ts,
            data,
        }
    }
}

/// 从包头标志取帧类型。
fn kind_of(header: &MediaHeader) -> FrameKind {
    if header.is_idr() {
        FrameKind::Idr
    } else {
        FrameKind::P
    }
}

impl std::fmt::Debug for FrameAssembler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 不展开分片数据（可能是 MB 级），只给拓扑信息。
        f.debug_struct("FrameAssembler")
            .field("max_concurrent_frames", &self.max_concurrent_frames)
            .field("in_flight", &self.frames.len())
            .field("highest_retired", &self.highest_retired)
            .field("last_now_us", &self.last_now_us)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_proto::{split_frame, HeaderFlags, MAX_PAYLOAD};

    /// 造一帧的分片：payload 切成 `chunk` 大小的片。
    fn frame_packets(seq: u64, payload: &[u8], chunk: usize) -> Vec<Vec<u8>> {
        split_frame(
            FrameSeq(seq),
            FrameKind::P,
            Timestamp(1_000),
            Timestamp(2_000),
            0,
            payload,
            chunk,
        )
    }

    fn parse(pkt: &[u8]) -> (MediaHeader, &[u8]) {
        MediaHeader::parse_datagram(pkt).expect("test packets are well-formed")
    }

    #[test]
    fn assembler_waits_for_all_packets() {
        // N5/N6：缺片不交付；500ms 内不当作丢失。
        let mut a = FrameAssembler::new(DEFAULT_MAX_CONCURRENT_FRAMES);
        a.gc(0); // 注入时钟起点

        let payload: Vec<u8> = (0..1500u32).map(|i| (i % 251) as u8).collect();
        let pkts = frame_packets(1, &payload, 500);
        assert_eq!(pkts.len(), 3);

        let (h0, p0) = parse(&pkts[0]);
        assert!(a.push(&h0, p0).is_none(), "只有 1/3 片，不能交付");
        let (h2, p2) = parse(&pkts[2]);
        assert!(a.push(&h2, p2).is_none(), "乱序补上最后一片，仍缺中间片");

        // 300ms 过去：没凑齐，但还不到 500ms 的放弃线（N6）。
        assert!(a.gc(300_000).is_empty(), "300ms < 500ms，不该被丢");
        assert_eq!(a.in_flight_frames(), 1);

        let (h1, p1) = parse(&pkts[1]);
        let frame = a.push(&h1, p1).expect("补齐中间片后必须交付");
        assert_eq!(frame.seq, FrameSeq(1));
        assert_eq!(frame.kind, FrameKind::P);
        assert_eq!(frame.capture_ts, Timestamp(1_000));
        assert_eq!(frame.encode_done_ts, Timestamp(2_000));
        assert_eq!(frame.data, payload, "数据必须逐字节还原");
        assert_eq!(a.in_flight_frames(), 0);
    }

    #[test]
    fn assembler_delivers_in_order() {
        // 到达顺序 2,1,0 → 仍按 packet_idx 重组（N5）。
        let mut a = FrameAssembler::new(DEFAULT_MAX_CONCURRENT_FRAMES);
        a.gc(0);

        let payload: Vec<u8> = (0..900u32).map(|i| (i * 7 % 256) as u8).collect();
        let pkts = frame_packets(7, &payload, 300);
        assert_eq!(pkts.len(), 3);

        let mut got = None;
        for i in [2usize, 1, 0] {
            let (h, p) = parse(&pkts[i]);
            if let Some(f) = a.push(&h, p) {
                assert_eq!(i, 0, "只有最后一片（idx 0）到达时才可能凑齐");
                got = Some(f);
            }
        }
        let frame = got.expect("全乱序到达也必须交付");
        assert_eq!(frame.data, payload, "按 idx 重组，与到达顺序无关");
    }

    #[test]
    fn assembler_gc_drops_stale_frames() {
        // N6：超过 500ms 未凑齐 → 丢弃并回报序号；恰好 500ms 不丢。
        let mut a = FrameAssembler::new(DEFAULT_MAX_CONCURRENT_FRAMES);
        a.gc(1_000); // 时钟起点 1000us

        let pkts = frame_packets(42, &[0xAB; 700], 350);
        assert_eq!(pkts.len(), 2);
        let (h, p) = parse(&pkts[0]);
        assert!(a.push(&h, p).is_none());

        // 恰好 500ms：不丢（严格大于才丢）。
        assert!(a.gc(501_000).is_empty(), "恰好 500ms 不该丢");
        assert_eq!(a.in_flight_frames(), 1);
        // 501ms：丢。
        let dropped = a.gc(501_001);
        assert_eq!(dropped, vec![FrameSeq(42)], "调用方拿它去记 LostInNetwork");
        assert_eq!(a.in_flight_frames(), 0, "缓冲被回收，不泄漏");
        // 丢掉的帧再补片也不会复活（已按丢失处理）。
        let (h1, p1) = parse(&pkts[1]);
        assert!(a.push(&h1, p1).is_none());
        assert_eq!(a.in_flight_frames(), 0);
    }

    #[test]
    fn assembler_respects_concurrency_limit() {
        // N10：上限 2。满了之后新帧被拒；释放后恢复接受。
        let mut a = FrameAssembler::new(DEFAULT_MAX_CONCURRENT_FRAMES);
        a.gc(0);

        let p1 = frame_packets(1, &[1u8; 600], 300); // 2 片
        let p2 = frame_packets(2, &[2u8; 600], 300);
        let p3 = frame_packets(3, &[3u8; 600], 300);

        let (h, p) = parse(&p1[0]);
        assert!(a.push(&h, p).is_none());
        let (h, p) = parse(&p2[0]);
        assert!(a.push(&h, p).is_none());
        assert_eq!(a.in_flight_frames(), 2);

        // 第三帧：被拒（不是把旧帧挤出去）。
        let (h, p) = parse(&p3[0]);
        assert!(a.push(&h, p).is_none());
        assert_eq!(a.in_flight_frames(), 2, "第三帧不得进入缓冲");
        // 被拒的帧后续分片同样进不来——它已经注定丢失。
        let (h, p) = parse(&p3[1]);
        assert!(a.push(&h, p).is_none());
        assert_eq!(a.in_flight_frames(), 2);

        // 在途帧的分片不受影响，照常补齐。
        let (h, p) = parse(&p1[1]);
        assert!(a.push(&h, p).is_some(), "帧 1 凑齐交付，腾出一个槽");
        let (h, p) = parse(&p2[1]);
        assert!(a.push(&h, p).is_some(), "帧 2 凑齐交付");

        // 槽位释放后，新帧（注意：序号必须更大）重新被接受。
        let p4 = frame_packets(4, &[4u8; 600], 300);
        let (h, p) = parse(&p4[0]);
        assert!(a.push(&h, p).is_none());
        assert_eq!(a.in_flight_frames(), 1);
        let (h, p) = parse(&p4[1]);
        assert!(a.push(&h, p).is_some());
    }

    #[test]
    fn assembler_default_limit_is_two() {
        // N10 / Q3：默认并发 = 2。改成 8（或 120）是在复刻 RustDesk 的错误。
        assert_eq!(DEFAULT_MAX_CONCURRENT_FRAMES, 2);
        // 默认构造真的按 2 执行（上面一条已验证行为，这里验证常量被使用）。
        let a = FrameAssembler::new(DEFAULT_MAX_CONCURRENT_FRAMES);
        assert_eq!(a.max_concurrent_frames, 2);
    }

    #[test]
    fn empty_frame_assembles_to_one_packet() {
        // 边界：空负载帧 = 1 个空分片，push 一次立即交付。
        let mut a = FrameAssembler::new(DEFAULT_MAX_CONCURRENT_FRAMES);
        a.gc(0);

        let pkts = frame_packets(0, &[], 1200);
        assert_eq!(pkts.len(), 1, "空负载也产生一个（空）末包");
        let (h, p) = parse(&pkts[0]);
        assert_eq!(h.packet_cnt, 1);
        assert!(p.is_empty());

        let frame = a.push(&h, p).expect("单片帧必须立即交付");
        assert_eq!(frame.seq, FrameSeq(0));
        assert!(frame.data.is_empty());
    }

    #[test]
    fn duplicate_and_late_packets_do_not_double_deliver() {
        // 同一分片重复投递 / 已交付帧的迟到分片：都不得再交付或占槽。
        let mut a = FrameAssembler::new(DEFAULT_MAX_CONCURRENT_FRAMES);
        a.gc(0);
        let pkts = frame_packets(9, &[7u8; 100], 100);
        assert_eq!(pkts.len(), 1);
        let (h, p) = parse(&pkts[0]);
        assert!(a.push(&h, p).is_some());
        assert!(a.push(&h, p).is_none(), "重复分片不能再交付");
        assert_eq!(a.in_flight_frames(), 0, "重复分片不得新开槽位");
    }

    #[test]
    fn parity_packets_do_not_count_as_frame_data() {
        // 冗余包不占帧槽位（它没有有效的 packet_idx 语义）。
        let mut a = FrameAssembler::new(DEFAULT_MAX_CONCURRENT_FRAMES);
        a.gc(0);
        let pkts = frame_packets(5, &[9u8; 100], 100);
        let (h, p) = parse(&pkts[0]);
        let mut parity_hdr = h;
        parity_hdr.flags.set(HeaderFlags::PARITY);
        assert!(a.push(&parity_hdr, p).is_none());
        assert_eq!(a.in_flight_frames(), 0, "冗余包不建帧槽");
        // 真正的数据分片仍正常交付。
        assert!(a.push(&h, &[9u8; 100]).is_some());
    }

    #[test]
    fn max_payload_constant_still_aligned() {
        // 防漂移：rc-proto 的预算与重组无关但被本模块的调用方依赖。
        assert_eq!(MAX_PAYLOAD, 1200);
    }

    #[test]
    fn gc_survives_u32_microsecond_wraparound() {
        // u32 微秒 ~71.5 分钟回绕一次（Stage F 有 8 小时 soak）。
        // 修复前 saturating_sub 在回绕点把经过时间算成 0，超时帧永远
        // 滞留在缓冲里。构造 first_seen 贴近回绕点的跨回绕对。
        let mut a = FrameAssembler::new(DEFAULT_MAX_CONCURRENT_FRAMES);
        let t0 = u32::MAX - 10_000; // 回绕前 10ms
        a.gc(t0);

        let pkts = frame_packets(77, &[0xAB; 700], 350);
        let (h, p) = parse(&pkts[0]);
        assert!(a.push(&h, p).is_none(), "只到 1/2 片");

        // 跨回绕后恰好 500ms（t0 + 500_000 已绕到 ~490_000）：不丢。
        assert!(
            a.gc(t0.wrapping_add(FRAME_STALE_US)).is_empty(),
            "恰好 500ms 不丢（严格大于才丢），回绕不得影响判定"
        );
        assert_eq!(a.in_flight_frames(), 1);
        // 501ms：必须丢（saturating_sub 版本在此处算出经过时间 ~0，
        // 永远不丢——这正是回绕 bug）。
        let dropped = a.gc(t0.wrapping_add(FRAME_STALE_US).wrapping_add(1));
        assert_eq!(dropped, vec![FrameSeq(77)], "跨回绕的超时判定仍正确");
        assert_eq!(a.in_flight_frames(), 0);
    }
}
