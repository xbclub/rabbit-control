//! XOR 块 FEC（前向纠错）与自适应冗余率。
//!
//! # 为什么用 XOR 而不是 Reed-Solomon
//!
//! | 方案 | 恢复能力 | CPU 成本 | 延迟 | 实现复杂度 |
//! |---|---|---|---|---|
//! | XOR 块 | 每组恢复 1 个丢包 | 极低（纯异或） | 0（组收齐即解） | ~100 行 |
//! | Reed-Solomon | 每组恢复 k 个丢包 | 中（GF(256) 运算） | 组收齐即解 | ~800 行 + 查表 |
//!
//! 远程桌面的丢包特征是「随机单点丢失」而非「突发块丢失」，
//! XOR 已能覆盖大部分场景，且实现简单到 AI 不会写错。RS 留作 P1 优化。
//!
//! # 工作方式
//!
//! ```text
//! 发送端（一个 FEC 组包含 k 个数据包 + 1 个冗余包）：
//!
//!   data[0] ─┐
//!   data[1] ─┼─ XOR ──▶ parity
//!   data[2] ─┘
//!
//! 接收端：
//!   收到 data[0], data[2], parity  →  data[1] = data[0] ^ data[2] ^ parity
//! ```
//!
//! # 约束
//! - 组内数据包**长度必须一致**（不足的补零）。编码端负责 padding，
//!   解码端负责按 `payload_len` 截断。
//! - 冗余包只在**整组数据包都发出后**立即发出，不等下一帧。

#![deny(missing_docs)]
#![deny(unsafe_code)]

/// FEC 组编号。与媒体包头的 `fec_group` 对应。
pub type FecGroupId = u16;

/// 冗余率策略配置。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FecConfig {
    /// 最小冗余率（丢包为 0 时也发这么多冗余）
    pub min_ratio: f32,
    /// 最大冗余率（丢包很高时最多发这么多）
    pub max_ratio: f32,
    /// 冗余率 = 观测丢包率 × 该倍数
    pub ratio_multiplier: f32,
    /// 每组最多数据包数（决定恢复延迟上限）
    pub max_group_size: u8,
    /// 每组最少数据包数（小帧不值得做 FEC）
    pub min_group_size: u8,
}

impl Default for FecConfig {
    fn default() -> Self {
        Self {
            min_ratio: 0.0,
            max_ratio: 0.30,
            ratio_multiplier: 1.5,
            max_group_size: 16,
            min_group_size: 4,
        }
    }
}

impl FecConfig {
    /// 根据观测丢包率计算冗余率。
    ///
    /// # 公式
    /// `ratio = clamp(loss * multiplier, min_ratio, max_ratio)`
    ///
    /// # 参数
    /// - `loss_rate`: 观测到的丢包率（0.0 ~ 1.0）
    pub fn ratio_for_loss(&self, loss_rate: f32) -> f32 {
        let loss = loss_rate.clamp(0.0, 1.0);
        (loss * self.ratio_multiplier).clamp(self.min_ratio, self.max_ratio)
    }

    /// 根据冗余率与数据包数计算需要几个冗余包。
    ///
    /// # 返回
    /// 至少 0，最多 `max_group_size`（冗余包数不能超过数据包数，否则无意义）。
    ///
    /// # 注意
    /// 当前实现每组只支持 **1 个冗余包**（XOR 的固有限制），
    /// 因此返回值只有 0 或 1。保留该函数签名是为了将来换 RS 时接口不变。
    pub fn parity_count(&self, data_packets: usize, ratio: f32) -> usize {
        if data_packets < self.min_group_size as usize {
            return 0;
        }
        if ratio <= 0.0 {
            return 0;
        }
        let needed = (data_packets as f32 * ratio).ceil() as usize;
        needed.min(data_packets).min(1)
    }
}

/// 一个 FEC 组的数据包缓冲。
///
/// 调用方按组累积数据包，然后调用 [`FecGroup::build_parity`] 生成冗余包。
pub struct FecGroup {
    group_id: FecGroupId,
    /// 各数据包（长度已统一为 max_payload_len）
    packets: Vec<Vec<u8>>,
    /// 组内统一负载长度
    max_len: usize,
    /// 是否已完成
    sealed: bool,
}

impl FecGroup {
    /// 创建新组。
    pub fn new(group_id: FecGroupId) -> Self {
        Self {
            group_id,
            packets: Vec::new(),
            max_len: 0,
            sealed: false,
        }
    }

    /// 组编号。
    pub fn group_id(&self) -> FecGroupId {
        self.group_id
    }

    /// 已加入的数据包数。
    pub fn len(&self) -> usize {
        self.packets.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }

    /// 是否已封组（不能再加包）。
    pub fn is_sealed(&self) -> bool {
        self.sealed
    }

    /// 加入一个数据包。
    ///
    /// # 返回
    /// - `true`: 已加入
    /// - `false`: 组已封，未加入（调用方应新建组）
    ///
    /// # 注意
    /// 组内允许不同长度的包（短包在生成冗余时补零）。
    pub fn push(&mut self, payload: &[u8]) -> bool {
        if self.sealed {
            return false;
        }
        self.max_len = self.max_len.max(payload.len());
        self.packets.push(payload.to_vec());
        true
    }

    /// 封组并生成 XOR 冗余包。
    ///
    /// # 返回
    /// `None` 表示组太小、不值得生成冗余（见 [`FecConfig::min_group_size`]）。
    /// `Some(bytes)` 为冗余包负载，长度 = 组内最长数据包长度。
    ///
    /// # 算法
    /// 逐字节对全部数据包异或；短包在末尾以 0 补齐。
    pub fn build_parity(&mut self) -> Option<Vec<u8>> {
        self.sealed = true;
        if self.packets.is_empty() || self.max_len == 0 {
            return None;
        }
        let mut parity = vec![0u8; self.max_len];
        for pkt in &self.packets {
            for (i, byte) in pkt.iter().enumerate() {
                parity[i] ^= byte;
            }
        }
        Some(parity)
    }

    /// 数据包引用（供发送端打包）。
    pub fn packets(&self) -> &[Vec<u8>] {
        &self.packets
    }
}

/// 接收端的组重组缓冲。
///
/// 累积收到的数据包与冗余包，凑够后尝试恢复。
#[derive(Debug)]
pub struct FecRecoverer {
    group_id: FecGroupId,
    /// (包内序号, 负载)。None 表示该位置尚未收到
    slots: Vec<Option<Vec<u8>>>,
    /// 冗余包负载
    parity: Option<Vec<u8>>,
    /// 期望的数据包总数
    expected: usize,
    /// 冗余包负载长度
    parity_len: usize,
}

/// 恢复结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoverResult {
    /// 该组已完整，无需恢复。返回按序排列的数据包。
    Complete(Vec<Vec<u8>>),
    /// 成功恢复了丢失的包。返回 (恢复出的负载, 其包内序号)。
    Recovered(Vec<u8>, usize),
    /// 还缺多个包，无法用 XOR 恢复（XOR 只能恢复 1 个）。
    Unrecoverable,
    /// 仍在等待更多包。
    Incomplete,
}

impl FecRecoverer {
    /// 创建重组器。
    ///
    /// # 参数
    /// - `group_id`: FEC 组号
    /// - `expected`: 该组的数据包总数
    pub fn new(group_id: FecGroupId, expected: usize) -> Self {
        Self {
            group_id,
            slots: vec![None; expected],
            parity: None,
            expected,
            parity_len: 0,
        }
    }

    /// 组编号。
    pub fn group_id(&self) -> FecGroupId {
        self.group_id
    }

    /// 收到一个数据包。
    ///
    /// # 参数
    /// - `idx`: 包在组内的序号（0..expected）
    ///
    /// # 返回
    /// `false` 表示序号越界或重复（被忽略）。
    pub fn push_data(&mut self, idx: usize, payload: Vec<u8>) -> bool {
        if idx >= self.expected {
            return false;
        }
        if self.slots[idx].is_some() {
            return false; // 重复包，忽略
        }
        self.slots[idx] = Some(payload);
        true
    }

    /// 收到冗余包。
    pub fn push_parity(&mut self, payload: Vec<u8>) {
        self.parity_len = payload.len();
        self.parity = Some(payload);
    }

    /// 尝试恢复。
    ///
    /// # 判定逻辑
    /// 1. 数据包全到 → `Complete`
    /// 2. 缺 1 个且冗余包已到 → `Recovered`
    /// 3. 缺 ≥2 个 → `Unrecoverable`（XOR 只能恢复 1 个）
    /// 4. 缺 1 个但冗余未到 → `Incomplete`
    pub fn try_recover(&self) -> RecoverResult {
        let mut missing: Vec<usize> = Vec::new();
        for (i, slot) in self.slots.iter().enumerate() {
            if slot.is_none() {
                missing.push(i);
            }
        }

        if missing.is_empty() {
            let all = self
                .slots
                .iter()
                .map(|s| s.clone().unwrap_or_default())
                .collect();
            return RecoverResult::Complete(all);
        }

        if missing.len() == 1 {
            match &self.parity {
                Some(par) => {
                    let idx = missing[0];
                    // 异或所有已收到的包 + 冗余包
                    let mut recovered = par.clone();
                    for (i, slot) in self.slots.iter().enumerate() {
                        if i == idx {
                            continue;
                        }
                        if let Some(p) = slot {
                            for (j, b) in p.iter().enumerate() {
                                if j < recovered.len() {
                                    recovered[j] ^= b;
                                }
                            }
                        }
                    }
                    // 冗余包比数据包长时，尾部零填充的异或结果会引入垃圾；
                    // 调用方按自身 payload_len 截断即可
                    RecoverResult::Recovered(recovered, idx)
                }
                None => RecoverResult::Incomplete,
            }
        } else {
            RecoverResult::Unrecoverable
        }
    }
}

impl std::fmt::Debug for FecGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FecGroup")
            .field("group_id", &self.group_id)
            .field("packets", &self.packets.len())
            .field("max_len", &self.max_len)
            .field("sealed", &self.sealed)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratio_zero_loss_is_zero() {
        let cfg = FecConfig::default();
        assert_eq!(cfg.ratio_for_loss(0.0), 0.0);
    }

    #[test]
    fn ratio_scales_with_loss() {
        let cfg = FecConfig::default();
        // 10% 丢包 → 15%
        let r = cfg.ratio_for_loss(0.10);
        assert!((r - 0.15).abs() < 1e-6, "got {r}");
    }

    #[test]
    fn ratio_clamped_at_max() {
        let cfg = FecConfig::default();
        // 50% 丢包 × 1.5 = 75%，应被限制到 30%
        assert_eq!(cfg.ratio_for_loss(0.5), cfg.max_ratio);
    }

    #[test]
    fn ratio_input_clamped() {
        let cfg = FecConfig::default();
        // 非法输入 1.5 应视为 1.0
        assert_eq!(cfg.ratio_for_loss(1.5), cfg.max_ratio);
        assert_eq!(cfg.ratio_for_loss(-1.0), cfg.min_ratio);
    }

    #[test]
    fn parity_zero_for_small_group() {
        let cfg = FecConfig::default();
        // 3 包 < min_group_size(4)，不做 FEC
        assert_eq!(cfg.parity_count(3, 0.3), 0);
    }

    #[test]
    fn parity_one_for_valid_group() {
        let cfg = FecConfig::default();
        assert_eq!(cfg.parity_count(8, 0.3), 1);
    }

    #[test]
    fn group_builds_parity_by_xor() {
        let mut g = FecGroup::new(7);
        g.push(&[0b0000_1111, 0b1111_0000]);
        g.push(&[0b0000_1111, 0b0000_1111]);
        let parity = g.build_parity().expect("parity");
        // 异或：0b0000_1111 ^ 0b0000_1111 = 0
        //       0b1111_0000 ^ 0b0000_1111 = 0b1111_1111
        assert_eq!(parity, vec![0b0000_0000, 0b1111_1111]);
    }

    #[test]
    fn group_handles_unequal_lengths() {
        let mut g = FecGroup::new(1);
        g.push(&[0xFF]); // 1 字节
        g.push(&[0x00, 0xAA]); // 2 字节
        let parity = g.build_parity().expect("parity");
        // 长度取 2；第 1 包末尾补 0
        assert_eq!(parity, vec![0xFF, 0xAA]);
    }

    #[test]
    fn group_seals_and_rejects_more() {
        let mut g = FecGroup::new(1);
        g.push(&[1, 2, 3]);
        g.build_parity();
        assert!(g.is_sealed());
        assert!(!g.push(&[4, 5, 6]), "sealed group must reject push");
    }

    #[test]
    fn empty_group_has_no_parity() {
        let mut g = FecGroup::new(0);
        assert!(g.build_parity().is_none());
    }

    /// 核心用例：单包丢失可恢复
    #[test]
    fn single_loss_is_recovered() {
        let a = vec![0b1010_1010u8, 0b1111_0000];
        let b = vec![0b0101_0101u8, 0b0000_1111];
        let c = vec![0b1100_1100u8, 0b1010_1010];

        let mut g = FecGroup::new(0);
        g.push(&a);
        g.push(&b);
        g.push(&c);
        let parity = g.build_parity().expect("parity");

        // 模拟 b 丢失
        let mut rec = FecRecoverer::new(0, 3);
        rec.push_data(0, a.clone());
        rec.push_data(2, c.clone());
        rec.push_parity(parity);

        match rec.try_recover() {
            RecoverResult::Recovered(payload, idx) => {
                assert_eq!(idx, 1);
                assert_eq!(
                    &payload[..2],
                    &b[..],
                    "recovered payload must equal original"
                );
            }
            other => panic!("expected Recovered, got {other:?}"),
        }
    }

    #[test]
    fn complete_group_returns_all_packets_in_order() {
        let mut rec = FecRecoverer::new(0, 3);
        rec.push_data(0, vec![1]);
        rec.push_data(1, vec![2]);
        rec.push_data(2, vec![3]);
        match rec.try_recover() {
            RecoverResult::Complete(all) => {
                assert_eq!(all, vec![vec![1], vec![2], vec![3]]);
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    /// XOR 的固有限制：缺 2 个包无法恢复
    #[test]
    fn two_losses_are_unrecoverable() {
        let mut rec = FecRecoverer::new(0, 3);
        rec.push_data(0, vec![1]);
        rec.push_parity(vec![9, 9]);
        match rec.try_recover() {
            RecoverResult::Unrecoverable => {}
            other => panic!("expected Unrecoverable, got {other:?}"),
        }
    }

    #[test]
    fn missing_parity_is_incomplete() {
        let mut rec = FecRecoverer::new(0, 3);
        rec.push_data(0, vec![1]);
        rec.push_data(2, vec![3]);
        assert_eq!(rec.try_recover(), RecoverResult::Incomplete);
    }

    #[test]
    fn duplicate_packet_is_ignored() {
        let mut rec = FecRecoverer::new(0, 2);
        assert!(rec.push_data(0, vec![1]));
        assert!(!rec.push_data(0, vec![9]), "duplicate must be rejected");
    }

    #[test]
    fn out_of_range_index_is_rejected() {
        let mut rec = FecRecoverer::new(0, 2);
        assert!(!rec.push_data(5, vec![1]));
    }

    /// 端到端：随机丢 1 包，反复验证恢复正确
    #[test]
    fn recovery_is_correct_for_any_position() {
        for lost in 0..4usize {
            let payloads: Vec<Vec<u8>> = (0..4u8).map(|i| vec![i * 17; 8]).collect();
            let mut g = FecGroup::new(0);
            for p in &payloads {
                g.push(p);
            }
            let parity = g.build_parity().unwrap();

            let mut rec = FecRecoverer::new(0, 4);
            for (i, p) in payloads.iter().enumerate() {
                if i != lost {
                    rec.push_data(i, p.clone());
                }
            }
            rec.push_parity(parity);

            match rec.try_recover() {
                RecoverResult::Recovered(payload, idx) => {
                    assert_eq!(idx, lost);
                    assert_eq!(payload, payloads[lost], "lost={lost}");
                }
                other => panic!("lost={lost}: expected Recovered, got {other:?}"),
            }
        }
    }

    #[test]
    fn zero_loss_full_group_works() {
        let mut g = FecGroup::new(3);
        for i in 0u8..5 {
            g.push(&[i; 4]);
        }
        let parity = g.build_parity().unwrap();

        let mut rec = FecRecoverer::new(3, 5);
        for i in 0u8..5 {
            rec.push_data(i as usize, vec![i; 4]);
        }
        rec.push_parity(parity);
        assert!(matches!(rec.try_recover(), RecoverResult::Complete(_)));
    }
}
