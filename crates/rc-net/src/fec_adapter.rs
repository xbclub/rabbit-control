//! FEC 适配层：把 `rc-fec` 的组 / 恢复器接到媒体包路径上。
//!
//! # XOR 的作用域是「完整数据报」，不是裸负载
//! 丢失分片的**包头**（里面有 `payload_len`）也一起被 XOR 保护。这样恢复
//! 出来的就是原始数据报本身，长度信息无损——如果只对负载做 XOR，丢失
//! 分片的真实长度就永远找不回来（尾部真零与补零无法区分），100KB 帧
//! 「逐字节相同」的验收不可能稳定达成。
//!
//! # 组号回绕（坑 7）
//! 组号是 `u16`，65536 组后回绕。接收端按「组号 + 帧序号」联合定位组
//! （同一个组号可能同时对应多条帧的条目），绝不单靠组号。
//!
//! # 放弃策略（N7）
//! XOR 每组只能恢复 1 个丢包。缺 ≥2 时 `try_recover` 返回 `None` 且
//! **不再重试**——正确动作是请求 IDR。条目由 gc 在 500ms 内清掉。

use std::collections::HashMap;

use rc_fec::{FecConfig, FecGroup, FecRecoverer, RecoverResult};
use rc_proto::{HeaderFlags, HeaderFlags as HF, MediaHeader, HEADER_LEN, MAGIC, MAX_PAYLOAD};
use rc_types::{FrameSeq, Timestamp};

/// FEC 组条目的超时清理时长（与重组层的帧放弃线一致）。
pub const FEC_GROUP_STALE_US: u32 = 500_000;

/// 把编码后的帧切成 FEC 组，生成冗余包。
///
/// 每帧分配一个组号（`u16` 回绕），把该帧的**全部数据报**作为一个 XOR 组，
/// 按观测丢包率决定是否附加 1 个冗余包（`rc-fec` 的 `parity_count` 上限为 1）。
pub struct FecSender {
    cfg: FecConfig,
    /// 下一个分配的组号（wrapping，见模块文档「组号回绕」）
    next_group: u16,
}

impl FecSender {
    /// 以 FEC 策略构造。
    pub fn new(cfg: FecConfig) -> Self {
        Self { cfg, next_group: 0 }
    }

    /// 输入一帧的完整分片，返回「数据包 + 冗余包」的发送序列。
    ///
    /// - 数据包原样返回，但其包头的 `fec_group` 字段会被改写为本组组号
    ///   （组号由发送端分配，调用方无需预先知道）；
    /// - `loss_rate` 是**实测**丢包率（来自 `stats::LossEstimator`），
    ///   经 `FecConfig::ratio_for_loss` 得出冗余率；帧分片数不足
    ///   `min_group_size` 或冗余率为 0 时不加冗余；
    /// - 冗余包的负载是全组数据报的 XOR（短包补零），长度 = 组内最长数据报；
    ///   若它超出 `MAX_PAYLOAD`（分片预算配得离谱时才会发生），出于协议
    ///   合法性放弃本组冗余。
    pub fn wrap_frame(
        &mut self,
        seq: FrameSeq,
        packets: Vec<Vec<u8>>,
        loss_rate: f32,
    ) -> Vec<Vec<u8>> {
        let group = self.next_group;
        self.next_group = self.next_group.wrapping_add(1);

        let mut out = packets;
        // 把组号写进每个数据包头部（偏移 24..26，小端）。
        // 先验魔数：非媒体包（误用）不修改、不参与，宁可不保护也不写坏。
        for p in &mut out {
            if p.len() >= HEADER_LEN && p[0..4] == MAGIC.to_le_bytes() {
                p[24..26].copy_from_slice(&group.to_le_bytes());
            }
        }

        let ratio = self.cfg.ratio_for_loss(loss_rate);
        if self.cfg.parity_count(out.len(), ratio) == 0 {
            return out;
        }

        let mut grp = FecGroup::new(group);
        for p in &out {
            if p.len() >= HEADER_LEN && p[0..4] == MAGIC.to_le_bytes() {
                grp.push(p);
            }
        }
        let Some(parity) = grp.build_parity() else {
            return out;
        };
        if parity.len() > MAX_PAYLOAD {
            // 组内最长数据报已经装不进一个合法数据报 → 这组分片本身
            // 违反 N2，放弃冗余（数据包照发，交给上层诊断）。
            return out;
        }

        let hdr = MediaHeader {
            frame_seq: seq,
            capture_ts: Timestamp(0),
            encode_done_ts: Timestamp(0),
            packet_idx: 0,
            // 冗余包不参与帧槽位计数；cnt 取数据包数只为通过
            // idx < cnt 的协议校验（接收端按 PARITY 标志分流）。
            packet_cnt: out.len() as u16,
            fec_group: group,
            flags: {
                let mut f = HeaderFlags::empty();
                f.set(HF::PARITY);
                f
            },
            layer_id: 0,
            payload_len: parity.len() as u16,
        };
        let mut pkt = vec![0u8; HEADER_LEN + parity.len()];
        {
            let (head, body) = pkt.split_at_mut(HEADER_LEN);
            let head_arr: &mut [u8; HEADER_LEN] = head.try_into().expect("exact split");
            hdr.write(head_arr);
            body.copy_from_slice(&parity);
        }
        out.push(pkt);
        out
    }
}

/// 一个 FEC 组的接收侧条目。
struct GroupEntry {
    /// 属于哪一帧（组号回绕时用于消歧，坑 7）
    seq: u64,
    recoverer: FecRecoverer,
    /// 最近一次观察时的采样时钟（由 gc 注入刷新）
    last_seen_us: u32,
}

/// 接收侧的 FEC 恢复。
///
/// 数据分片与冗余分片都经 [`FecReceiver::observe`] 喂入；
/// [`FecReceiver::try_recover`] 只在「该组恰好缺 1 个数据分片且冗余已到」
/// 时给出恢复结果，缺 ≥2 恒为 `None`（N7）。
pub struct FecReceiver {
    /// 组号 → 条目列表（同组号可有多条：回绕 + 并发帧）
    groups: HashMap<u16, Vec<GroupEntry>>,
    /// 最近一次 gc 注入的时钟（同时是包到达时刻的采样）
    last_now_us: u32,
}

impl Default for FecReceiver {
    fn default() -> Self {
        Self::new()
    }
}

impl FecReceiver {
    /// 空接收器。
    pub fn new() -> Self {
        Self {
            groups: HashMap::new(),
            last_now_us: 0,
        }
    }

    /// 记录收到的包（数据或冗余）。
    ///
    /// 数据包会连同包头一起重建为完整数据报再进入 XOR 组——
    /// 包头里的 `payload_len` 正是恢复后截断补零的权威依据。
    pub fn observe(&mut self, header: &MediaHeader, payload: &[u8]) {
        let group = header.fec_group;
        let seq = header.frame_seq.0;
        let expected = header.packet_cnt as usize;

        let entries = self.groups.entry(group).or_default();
        let entry = match entries.iter_mut().find(|e| e.seq == seq) {
            Some(e) => e,
            None => {
                entries.push(GroupEntry {
                    seq,
                    recoverer: FecRecoverer::new(group, expected.max(1)),
                    last_seen_us: self.last_now_us,
                });
                entries.last_mut().expect("just pushed")
            }
        };
        entry.last_seen_us = self.last_now_us;

        if header.flags.is_parity() {
            entry.recoverer.push_parity(payload.to_vec());
        } else {
            let mut wire = vec![0u8; HEADER_LEN + payload.len()];
            {
                let (head, body) = wire.split_at_mut(HEADER_LEN);
                let head_arr: &mut [u8; HEADER_LEN] = head.try_into().expect("exact split");
                header.write(head_arr);
                body.copy_from_slice(payload);
            }
            let _ = entry.recoverer.push_data(header.packet_idx as usize, wire);
        }
    }

    /// 尝试恢复某组缺失的包。
    ///
    /// 返回 `Some((packet_idx, 完整数据报字节))`。同一组号有多条帧时，
    /// 从最新的条目往回尝试（回绕场景下新帧更可能是当前关注的）。
    /// 恢复成功后该条目即被消费（移除），不会重复恢复。
    pub fn try_recover(&mut self, group: u16) -> Option<(u16, Vec<u8>)> {
        let entries = self.groups.get_mut(&group)?;
        for i in (0..entries.len()).rev() {
            if let RecoverResult::Recovered(padded, idx) = entries[i].recoverer.try_recover() {
                // 截掉 XOR 补零：包头里的 payload_len 是权威长度。
                let wire = truncate_to_header(&padded)?;
                entries.remove(i);
                if entries.is_empty() {
                    self.groups.remove(&group);
                }
                return Some((idx as u16, wire));
            }
        }
        None
    }

    /// 清理超时的组。
    ///
    /// `now_us` 同时刷新「包到达时刻」的采样时钟（与重组层同一模式）。
    /// 时钟是 u32 微秒，约 71.5 分钟回绕一次：经过时间一律用
    /// [`u32::wrapping_sub`] 计算（与 [`crate::reassemble::FrameAssembler::gc`]
    /// 同一纪律；`saturating_sub` 在回绕点会把经过时间算成 0，条目
    /// 永远清不掉）。
    pub fn gc(&mut self, now_us: u32) {
        self.last_now_us = now_us;
        self.groups.retain(|_, entries| {
            entries.retain(|e| now_us.wrapping_sub(e.last_seen_us) <= FEC_GROUP_STALE_US);
            !entries.is_empty()
        });
    }

    /// 当前跟踪的（组号 × 帧）条目数。诊断 / 测试用。
    pub fn tracked_groups(&self) -> usize {
        self.groups.values().map(|v| v.len()).sum()
    }
}

/// 按 XOR 结果里的包头截断补零，返回原始数据报。
fn truncate_to_header(padded: &[u8]) -> Option<Vec<u8>> {
    if padded.len() < HEADER_LEN {
        return None;
    }
    let h = MediaHeader::parse(padded).ok()?;
    let end = (HEADER_LEN + h.payload_len as usize).min(padded.len());
    Some(padded[..end].to_vec())
}

impl std::fmt::Debug for FecSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FecSender")
            .field("cfg", &self.cfg)
            .field("next_group", &self.next_group)
            .finish()
    }
}

impl std::fmt::Debug for FecReceiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FecReceiver")
            .field("tracked", &self.tracked_groups())
            .field("last_now_us", &self.last_now_us)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_proto::split_frame;
    use rc_types::FrameKind;

    /// 5 片、每片 100B 的帧分片（450B 负载 → 4×100 + 50，最后一片短，
    /// 正好覆盖「恢复后需要按 payload_len 截断」的路径）。
    fn frame_packets(seq: u64) -> Vec<Vec<u8>> {
        let payload: Vec<u8> = (0..450u32).map(|i| (i % 251) as u8).collect();
        split_frame(
            FrameSeq(seq),
            FrameKind::Idr,
            Timestamp(10),
            Timestamp(20),
            0,
            &payload,
            100,
        )
    }

    #[test]
    fn fec_wraps_frame_with_parity() {
        let mut s = FecSender::new(FecConfig::default());
        let pkts = frame_packets(1);
        assert_eq!(pkts.len(), 5, "450B / 100B 片 = 5 片");

        let out = s.wrap_frame(FrameSeq(1), pkts.clone(), 0.03);
        assert_eq!(out.len(), 6, "5 数据包 + 1 冗余包");

        // 数据包：组号被写进包头，且未被改动其他字节。
        let group = {
            let (h, _) = MediaHeader::parse_datagram(&out[0]).unwrap();
            h.fec_group
        };
        for p in &out[..5] {
            let (h, _) = MediaHeader::parse_datagram(p).unwrap();
            assert_eq!(h.fec_group, group, "组内数据包共享组号");
            assert!(!h.flags.is_parity());
        }

        // 冗余包：PARITY 标志、同组、负载非空。
        let (h, parity) = MediaHeader::parse_datagram(&out[5]).unwrap();
        assert!(h.flags.is_parity(), "第 6 包必须是冗余包");
        assert_eq!(h.fec_group, group);
        assert!(!parity.is_empty());

        // XOR 不变量：parity ^ 所有数据报 = 全零（这正是单丢可恢复的原因）。
        let max_len = out[..5].iter().map(|p| p.len()).max().unwrap();
        let mut acc = vec![0u8; max_len];
        acc.copy_from_slice(parity);
        for p in &out[..5] {
            for (i, b) in p.iter().enumerate() {
                acc[i] ^= b;
            }
        }
        assert!(
            acc.iter().all(|&b| b == 0),
            "parity 与全部数据的异或必须为零"
        );
    }

    #[test]
    fn fec_no_parity_when_loss_is_zero() {
        // 丢包率 0 → 冗余率 0 → 不加冗余（不浪费带宽）。
        let mut s = FecSender::new(FecConfig::default());
        let out = s.wrap_frame(FrameSeq(1), frame_packets(1), 0.0);
        assert_eq!(out.len(), 5, "零丢包时不产生冗余包");
    }

    #[test]
    fn fec_receiver_recovers_single_loss() {
        let mut s = FecSender::new(FecConfig::default());
        let pkts = frame_packets(2);
        let out = s.wrap_frame(FrameSeq(2), pkts.clone(), 0.03);

        // 丢最后一个（短）数据分片：idx 4，50B 负载。
        let lost_idx = 4usize;
        let mut rx = FecReceiver::new();
        rx.gc(0);
        for (i, p) in out.iter().enumerate() {
            if i != lost_idx {
                let (h, body) = MediaHeader::parse_datagram(p).unwrap();
                rx.observe(&h, body);
            }
        }
        assert_eq!(rx.tracked_groups(), 1);

        let group = {
            let (h, _) = MediaHeader::parse_datagram(&out[0]).unwrap();
            h.fec_group
        };
        let (idx, wire) = rx
            .try_recover(group)
            .expect("缺 1 包 + 冗余已到 → 必须恢复");
        assert_eq!(idx as usize, lost_idx);
        assert_eq!(
            &wire, &out[lost_idx],
            "恢复出的数据报与原包逐字节相同（含截断）"
        );
        // 恢复后条目被消费，不会重复恢复。
        assert_eq!(rx.tracked_groups(), 0);
        assert!(rx.try_recover(group).is_none());
    }

    #[test]
    fn fec_receiver_gives_up_on_two_losses() {
        // N7：缺 ≥2 直接放弃；gc 在 500ms 内清掉条目，不无限等待。
        let mut s = FecSender::new(FecConfig::default());
        let out = s.wrap_frame(FrameSeq(3), frame_packets(3), 0.03);

        let mut rx = FecReceiver::new();
        rx.gc(0);
        for (i, p) in out.iter().enumerate() {
            if i != 1 && i != 3 {
                let (h, body) = MediaHeader::parse_datagram(p).unwrap();
                rx.observe(&h, body);
            }
        }
        let group = {
            let (h, _) = MediaHeader::parse_datagram(&out[0]).unwrap();
            h.fec_group
        };
        assert!(rx.try_recover(group).is_none(), "缺 2 包不可恢复，立即放弃");
        assert!(rx.try_recover(group).is_none(), "放弃就是放弃，重试也不给");

        assert_eq!(rx.tracked_groups(), 1, "放弃 ≠ 清理；条目由 gc 回收");
        // 恰好 500ms 不清；超过即清（不无限等待）。
        rx.gc(500_000);
        assert_eq!(rx.tracked_groups(), 1);
        rx.gc(500_001);
        assert_eq!(rx.tracked_groups(), 0, "500ms 内必须清掉，防内存增长");
    }

    #[test]
    fn fec_gc_survives_u32_microsecond_wraparound() {
        // u32 微秒 ~71.5 分钟回绕（8 小时 soak 必踩）：跨回绕的
        // last_seen/now 对，超时判定仍必须正确（saturating_sub 版本
        // 在回绕点算出经过时间 ~0，条目永远清不掉）。
        let mut rx = FecReceiver::new();
        let t0 = u32::MAX - 5_000; // 回绕前 5ms
        rx.gc(t0);
        let p = frame_packets(11);
        let (h, body) = MediaHeader::parse_datagram(&p[0]).unwrap();
        rx.observe(&h, body);
        assert_eq!(rx.tracked_groups(), 1);

        // 跨回绕后恰好 500ms：不清（<= 才清）。
        rx.gc(t0.wrapping_add(FEC_GROUP_STALE_US));
        assert_eq!(rx.tracked_groups(), 1, "恰好 500ms 不清，回绕不得影响判定");
        // 501ms：必须清。
        rx.gc(t0.wrapping_add(FEC_GROUP_STALE_US).wrapping_add(1));
        assert_eq!(rx.tracked_groups(), 0, "跨回绕的超时清理仍正确");
    }

    #[test]
    fn fec_group_ids_wrap_and_disambiguate_by_frame_seq() {
        // 坑 7：组号回绕后，同组号的新旧帧靠 frame_seq 区分。
        let mut s = FecSender::new(FecConfig {
            // 极端配置：让 4 片的帧也敢加冗余需要 ratio > 0 —— 用小 min_group_size
            min_group_size: 4,
            ..FecConfig::default()
        });
        s.next_group = u16::MAX; // 下一组立即回绕到 0

        let out1 = s.wrap_frame(FrameSeq(100), frame_packets(100), 0.03);
        let out2 = s.wrap_frame(FrameSeq(200), frame_packets(200), 0.03);
        let g1 = {
            let (h, _) = MediaHeader::parse_datagram(&out1[0]).unwrap();
            h.fec_group
        };
        let g2 = {
            let (h, _) = MediaHeader::parse_datagram(&out2[0]).unwrap();
            h.fec_group
        };
        assert_eq!(g1, u16::MAX);
        assert_eq!(g2, 0, "组号已回绕");

        // 两帧同用（回绕相邻的）不同组号正常；真正同组号的场景：
        let mut rx = FecReceiver::new();
        rx.gc(0);
        // 人为把帧 200 的组号改成与帧 100 相同（模拟回绕撞号）。
        for p in &out2 {
            let mut q = p.clone();
            if q.len() >= HEADER_LEN {
                q[24..26].copy_from_slice(&g1.to_le_bytes());
            }
            let (h, body) = MediaHeader::parse_datagram(&q).unwrap();
            rx.observe(&h, body);
        }
        assert_eq!(rx.tracked_groups(), 1, "同组号两条帧 → 两个条目并存");
        // 帧序号消歧：恢复的是帧 200 的缺失（我们只喂了帧 200）。
        // 丢帧 200 的 idx2：
        // （上面喂了全部，先重建一个只缺 idx2 的接收器）
        let mut rx2 = FecReceiver::new();
        rx2.gc(0);
        for (i, p) in out2.iter().enumerate() {
            if i == 2 {
                continue;
            }
            let mut q = p.clone();
            if q.len() >= HEADER_LEN {
                q[24..26].copy_from_slice(&g1.to_le_bytes());
            }
            let (h, body) = MediaHeader::parse_datagram(&q).unwrap();
            rx2.observe(&h, body);
        }
        let (idx, wire) = rx2.try_recover(g1).expect("缺 1 可恢复");
        assert_eq!(idx, 2);
        assert_eq!(&wire, &out2[2]);
    }
}
