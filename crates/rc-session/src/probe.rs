//! 探测与初始码率（M2-10 / T15）：ProbePing/Pong（RTT + 时钟偏移，兼
//! 心跳）、BwProbe 数据报带宽粗测 v0、估速 → 初始码率换算。
//!
//! # 定位（B10，写死边界）
//!
//! 带宽粗测是 **v0 粗 heuristic**：测的是 rc-net pacer 天花板
//! （`DEFAULT_INITIAL_PACING_BPS` = 50Mbps，quic.rs）与真实路径带宽的
//! 较小者，datagram 丢包使估速**偏低**——只用于会话建立时的一次初始
//! 码率校准，**M3-3 拥塞控制接管后整条探测链废弃替换**（线格式号保留
//! 不回收）。禁止把估速当精确带宽使用、禁止据此调参宣称「测得带宽」。
//!
//! # 时间纪律（铁律 4 / B2 / B9）
//!
//! 本模块不读任何系统时钟：`pump` / `on_pong` / `on_chunk` 的时间全部
//! 由调用方以 `now_us`（本端会话钟，微秒）注入。探测钟 u64 与管线钟
//! u32 **同源同纪元**（B9）：会话驱动循环用同一个时钟闭包喂两侧管线
//! 与探测泵（照 rc-loopback `run_real` 的单闭包模式）；70 分钟守卫
//! （T17 的 Bye(Refresh)）内 u64 < 2^32，u32 零扩展无损。
//!
//! # 与 QUIC keepalive 的关系（别删任何一个）
//!
//! rc-net 已有 1s QUIC keepalive（quic.rs）保**连接**活着；ProbePing
//! 的 1s 间隔泵产出 RTT/偏移并供 T16 看门狗「RTT+余量」复核复用——
//! 两者正交，不是重复。
//!
//! # proto 适配说明
//!
//! T13 把 ProbePing/ProbePong/BwProbeRequest/BwProbeData 交付为
//! [`crate::proto::ControlMessage`] 的枚举变体（非独立结构体），本模块
//! 的 API 直接以 `&ControlMessage` 等价适配，语义与线格式不动。

use rc_types::{Error, Result};

use crate::proto::{self, ControlMessage};

/// ProbePing 发送间隔（微秒）：1s，兼应用层心跳（铁律 2：保活走控制
/// 通道，不占媒体/输入通道）。与 rc-net 的 QUIC keepalive 正交（见模块
/// 文档）。
pub const PROBE_INTERVAL_US: u64 = 1_000_000;

/// 单次探测判超时（微秒）：3s 无 Pong 记丢失（≈3 个间隔的余量）。
pub const PROBE_TIMEOUT_US: u64 = 3_000_000;

/// RTT/偏移取中位数的样本窗口长度。
pub const PROBE_MEDIAN_WINDOW: usize = 8;

/// 带宽粗测总字节上限（协议 0x08 的 total_bytes 上限，与
/// `proto::MAX_BW_PROBE_BYTES` 同值：本侧常量供不用 proto 常量的调用方
/// 与测试引用）。
pub const BW_PROBE_MAX_TOTAL_BYTES: u32 = 256 * 1024;

/// 单块 padding 字节数。探测数据报 = 1 字节类型 0x09 + probe_id u32 LE +
/// chunk_idx u32 LE + padding = 9 + 1024 = 1033 字节，恒 ≤ rc-net 的
/// N2 同源预算 max_media_payload(1200) = 1120，不越界。
pub const BW_PROBE_CHUNK_PAYLOAD: usize = 1024;

/// 带宽探测结算超时（微秒）：2s 未凑齐按实收结算。
pub const BW_PROBE_SETTLE_TIMEOUT_US: u64 = 2_000_000;

/// 首末块到达间隔低于此值（微秒）= 测量退化（localhost 同拍到达），
/// 估速记 None、保持协商值——不做除零、不产出 ∞。
pub const BW_PROBE_MIN_SPAN_US: u64 = 1_000;

/// 初始码率下限（bps）。
pub const INITIAL_BITRATE_MIN_BPS: u64 = 2_000_000;

/// 估速 → 初始码率折扣因子（留 30% 余量给控制流与突发）。
pub const INITIAL_BITRATE_FACTOR: f64 = 0.7;

/// 一次成功探测的样本。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProbeSample {
    /// 探测序号。
    pub probe_id: u32,
    /// 往返时延（微秒）= t2 − t0（发起方时钟）。
    pub rtt_us: u64,
    /// 时钟偏移（微秒，有符号）= 对端时钟 − 本端时钟（NTP 中点法：
    /// t1 − (t0+t2)/2）。对端时间戳换算到本端时间轴：
    /// local = remote − offset_us。
    pub offset_us: i64,
}

/// 发起侧探测引擎（client 持有）。时间全部由调用方注入（铁律 4），
/// 不读任何系统时钟。
///
/// v0 只由 client 发起（host 无状态应答，见 [`pong_for`]；消息线格式
/// 为双向设计，host 主动发起留待后续卡）。
pub struct ProbeEngine {
    /// 下一个探测号（从 1 递增，u32 回绕允许）。
    next_id: u32,
    /// 在途探测表：(probe_id, 发送时刻 t0)，按发送序排列。
    pending: std::collections::VecDeque<(u32, u64)>,
    /// 样本窗口（最近 `PROBE_MEDIAN_WINDOW` 个，FIFO）。
    samples: std::collections::VecDeque<ProbeSample>,
    /// 上次发出 ProbePing 的时刻（None = 尚未发过，从首次 pump 起算）。
    last_ping_us: Option<u64>,
    pings_sent: u64,
    pongs_received: u64,
    probes_lost: u64,
}

impl Default for ProbeEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// 探测统计（[`ProbeEngine::stats`] 快照）。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ProbeStats {
    /// RTT 中位数（微秒；窗口为空 = None，报告侧按「无数据」处理）。
    pub rtt_us_median: Option<u64>,
    /// 时钟偏移中位数（微秒；窗口为空 = None）。
    pub offset_us_median: Option<i64>,
    /// 累计发出的 ProbePing 数。
    pub pings_sent: u64,
    /// 累计收到的 ProbePong 数。
    pub pongs_received: u64,
    /// 超时判失的探测数。
    pub probes_lost: u64,
}

impl ProbeEngine {
    /// 构造（探测号从 1 起）。
    pub fn new() -> Self {
        Self {
            next_id: 1,
            pending: std::collections::VecDeque::new(),
            samples: std::collections::VecDeque::new(),
            last_ping_us: None,
            pings_sent: 0,
            pongs_received: 0,
            probes_lost: 0,
        }
    }

    /// 探测泵（驱动循环每拍调用，now_us = 本端会话钟）：
    /// 距上次 ProbePing ≥ [`PROBE_INTERVAL_US`] 时产出一条新探测
    /// （probe_id 从 1 递增，u32 回绕允许；首次泵调用不立即发——从该拍
    /// 起算满一个间隔才发第一条）；同时把挂起 ≥ [`PROBE_TIMEOUT_US`]
    /// 的探测结算为丢失。返回本拍应发送的 ProbePing（多数拍为 None）。
    pub fn pump(&mut self, now_us: u64) -> Option<ControlMessage> {
        // ① 结算超时探测（在途表按发送序排列，队首最老）。
        while let Some(&(_, t0)) = self.pending.front() {
            if now_us.saturating_sub(t0) >= PROBE_TIMEOUT_US {
                self.pending.pop_front();
                self.probes_lost += 1;
            } else {
                break;
            }
        }
        // ② 到点发新探测（首次泵只锚定起点不立即发——从该拍起算满一个
        // 间隔才发第一条；`ping_pump_fires_once_per_interval` 钉死该语义）。
        if self.last_ping_us.is_none() {
            self.last_ping_us = Some(now_us);
            return None;
        }
        let last = self.last_ping_us.expect("上面刚锚定");
        if now_us.saturating_sub(last) < PROBE_INTERVAL_US {
            return None;
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.pending.push_back((id, now_us));
        self.last_ping_us = Some(now_us);
        self.pings_sent += 1;
        Some(ControlMessage::ProbePing {
            probe_id: id,
            sender_clock_us: now_us,
        })
    }

    /// 消费一条 ProbePong（now_us = 本端收到时刻 t2）。
    /// probe_id 不在途（迟到 / 伪造 / 重复）→ 忽略并返回 None。
    /// RTT 与偏移以**本端记录的发送时刻 t0** 为准（echoed 仅作线格式
    /// 回显，不参与计算）。
    pub fn on_pong(&mut self, pong: &ControlMessage, now_us: u64) -> Option<ProbeSample> {
        let ControlMessage::ProbePong {
            probe_id,
            responder_clock_us: t1,
            ..
        } = pong
        else {
            return None; // 非 Pong 输入（防御）
        };
        let pos = self.pending.iter().position(|(id, _)| id == probe_id)?;
        let (_, t0) = self.pending.remove(pos)?;
        self.pongs_received += 1;
        let rtt_us = now_us.saturating_sub(t0);
        // NTP 中点法（B2）：offset = t1 − (t0+t2)/2，方向 = 对端 − 本端。
        let mid = (t0 + now_us) / 2;
        let offset_us =
            i64::try_from(*t1).unwrap_or(i64::MAX) - i64::try_from(mid).unwrap_or(i64::MAX);
        let sample = ProbeSample {
            probe_id: *probe_id,
            rtt_us,
            offset_us,
        };
        self.samples.push_back(sample);
        while self.samples.len() > PROBE_MEDIAN_WINDOW {
            self.samples.pop_front();
        }
        Some(sample)
    }

    /// 对端时间戳 → 本端时间轴（用窗口中位偏移换算：local = remote −
    /// offset）。无样本返回 None——报告侧必须把 None 当「无数据」，
    /// 不是当 0（B3）。换算结果按 0 饱和（会话 µs 轴从 0 起）。
    pub fn to_local_us(&self, remote_us: u64) -> Option<u64> {
        let offsets: Vec<i64> = self.samples.iter().map(|s| s.offset_us).collect();
        let off = median_i64(&offsets)?;
        let local = i64::try_from(remote_us)
            .unwrap_or(i64::MAX)
            .saturating_sub(off)
            .max(0);
        Some(u64::try_from(local).unwrap_or(0))
    }

    /// 统计快照（中位数取当前窗口）。
    pub fn stats(&self) -> ProbeStats {
        let rtts: Vec<u64> = self.samples.iter().map(|s| s.rtt_us).collect();
        let offsets: Vec<i64> = self.samples.iter().map(|s| s.offset_us).collect();
        ProbeStats {
            rtt_us_median: median_u64(&rtts),
            offset_us_median: median_i64(&offsets),
            pings_sent: self.pings_sent,
            pongs_received: self.pongs_received,
            probes_lost: self.probes_lost,
        }
    }
}

/// 应答侧（host）无状态应答：echoed_clock_us 原样回显 t0，
/// responder_clock_us 打本端会话钟 t1（由调用方注入，铁律 4）。
/// host 不持 [`ProbeEngine`]（v0 只由 client 发起；消息设计为双向，
/// host 主动发起留待后续）。输入非 ProbePing 变体 → None（防御）。
pub fn pong_for(ping: &ControlMessage, responder_now_us: u64) -> Option<ControlMessage> {
    match ping {
        ControlMessage::ProbePing {
            probe_id,
            sender_clock_us,
        } => Some(ControlMessage::ProbePong {
            probe_id: *probe_id,
            echoed_clock_us: *sender_clock_us,
            responder_clock_us: responder_now_us,
        }),
        _ => None,
    }
}

/// host 侧：把 BwProbeRequest 展开成探测数据报块序列（纯函数）。
///
/// total_bytes == 0 或 > [`BW_PROBE_MAX_TOTAL_BYTES`] → Err(InvalidConfig)。
/// 块数 = ceil(total / [`BW_PROBE_CHUNK_PAYLOAD`])，末块 padding 取余量；
/// 每块字节布局 = `[0x09][probe_id u32 LE][chunk_idx u32 LE][padding]`，
/// 单块总长 ≤ 9 + 1024 = 1033 ≤ `rc_net::quic::max_media_payload(1200)`
/// （N2 预算，B5）。
pub fn bw_probe_chunks(probe_id: u32, total_bytes: u32) -> Result<Vec<Vec<u8>>> {
    if total_bytes == 0 || total_bytes > BW_PROBE_MAX_TOTAL_BYTES {
        return Err(Error::InvalidConfig(format!(
            "bw probe total_bytes {total_bytes} must be in 1..={BW_PROBE_MAX_TOTAL_BYTES}"
        )));
    }
    let chunks = total_bytes.div_ceil(BW_PROBE_CHUNK_PAYLOAD as u32) as usize;
    let mut out = Vec::with_capacity(chunks);
    for idx in 0..chunks {
        let pad = if idx + 1 == chunks {
            total_bytes as usize - (chunks - 1) * BW_PROBE_CHUNK_PAYLOAD
        } else {
            BW_PROBE_CHUNK_PAYLOAD
        };
        let mut buf = Vec::with_capacity(9 + pad);
        buf.push(proto::TYPE_BW_PROBE_DATA);
        buf.extend_from_slice(&probe_id.to_le_bytes());
        buf.extend_from_slice(&(idx as u32).to_le_bytes());
        buf.resize(9 + pad, 0); // padding = 不透明零填充
        out.push(buf);
    }
    Ok(out)
}

/// 解析一条探测数据报（client 的 Datagram 分发用）。
/// 首字节非 0x09（含截断）→ Err(Protocol)。padding 长度不校验（估速按
/// 实收算，与 proto 的「padding = 剩余字节」语义一致）。
pub fn parse_bw_datagram(buf: &[u8]) -> Result<ControlMessage> {
    match ControlMessage::decode(buf)? {
        m @ ControlMessage::BwProbeData { .. } => Ok(m),
        other => Err(Error::Protocol {
            offset: 0,
            msg: format!("not a bw probe datagram (first byte must be 0x09): {other:?}"),
        }),
    }
}

/// 一次带宽粗测的结算结果。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BwOutcome {
    /// 探测序号。
    pub probe_id: u32,
    /// 估速（bps）= 实收 padding 字节 × 8 / (末块到达 − 首块到达)。
    /// None = 退化（间隔 < [`BW_PROBE_MIN_SPAN_US`]）或零实收。
    /// 口径：只计 padding，不含 9 字节探测头（与 NetStats.bytes_recv 的
    /// 传输层口径区分，报告侧写清）。
    pub estimate_bps: Option<u64>,
    /// 实收 padding 字节总数（去重后）。
    pub received_bytes: u64,
    /// 请求的总字节数。
    pub expected_bytes: u64,
    /// true = 收到最高 chunk_idx 立即结算；false = 超时按实收结算。
    pub completed: bool,
}

/// client 侧带宽估计器（每次会话一次探测；时间注入）。
///
/// **v0 粗 heuristic（B10）**：测的是 pacer 天花板（50Mbps）与真实路径
/// 带宽的较小者、丢包使估速偏低，只用于初始码率，M3-3 拥塞控制接管后
/// 整条链废弃替换。
pub struct BwEstimator {
    /// None = 未发起或已结算（空操作）。
    active: Option<ActiveBw>,
    /// 是否已结算（区分「从未发起」）。
    settled: bool,
}

/// 一次进行中的探测（私有）。
struct ActiveBw {
    probe_id: u32,
    expected_total_bytes: u32,
    /// 按 `bw_probe_chunks` 同一规则推得的期望块数（最高
    /// chunk_idx = expected_chunks − 1）。
    expected_chunks: usize,
    /// chunk_idx 去重表（B5）。
    seen: Vec<bool>,
    received_bytes: u64,
    first_arrival_us: Option<u64>,
    last_arrival_us: Option<u64>,
    started_us: u64,
}

impl Default for BwEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl BwEstimator {
    /// 构造（未发起状态）。
    pub fn new() -> Self {
        Self {
            active: None,
            settled: false,
        }
    }

    /// 发起一次探测（expected_chunks 按 [`bw_probe_chunks`] 同一分块规则
    /// 推得）。重复调用会重置上一次未结算的探测（会话层保证只调一次）。
    pub fn start(&mut self, probe_id: u32, expected_total_bytes: u32, now_us: u64) {
        let expected_chunks = expected_total_bytes.div_ceil(BW_PROBE_CHUNK_PAYLOAD as u32) as usize;
        self.active = Some(ActiveBw {
            probe_id,
            expected_total_bytes,
            expected_chunks,
            seen: vec![false; expected_chunks],
            received_bytes: 0,
            first_arrival_us: None,
            last_arrival_us: None,
            started_us: now_us,
        });
        self.settled = false;
    }

    /// 记录一块到达（按 chunk_idx 去重，B5）。收到最高 chunk_idx 时立即
    /// 结算并返回 [`BwOutcome`]；未发起 / 已结算 / 探测号不符 / 分片
    /// 越界（伪造）→ 忽略返回 None。
    pub fn on_chunk(&mut self, data: &ControlMessage, now_us: u64) -> Option<BwOutcome> {
        let ControlMessage::BwProbeData {
            probe_id,
            chunk_idx,
            padding,
        } = data
        else {
            return None; // 非探测块输入（防御）
        };
        let act = self.active.as_mut()?;
        if *probe_id != act.probe_id {
            return None; // 过期/伪造探测号
        }
        let idx = usize::try_from(*chunk_idx).ok()?;
        if idx >= act.expected_chunks {
            return None; // 越界分片：防御性忽略
        }
        if act.seen[idx] {
            return None; // 重复分片：去重（B5，防估速虚高）
        }
        act.seen[idx] = true;
        act.received_bytes += padding.len() as u64;
        act.first_arrival_us = Some(act.first_arrival_us.map_or(now_us, |f| f.min(now_us)));
        act.last_arrival_us = Some(act.last_arrival_us.map_or(now_us, |l| l.max(now_us)));
        if idx + 1 == act.expected_chunks {
            return Some(self.settle(true)); // 最高块到达：立即结算
        }
        None
    }

    /// 超时泵：超过 [`BW_PROBE_SETTLE_TIMEOUT_US`] 未凑齐按实收结算
    /// （一次性；未发起 / 已结算返回 None）。
    pub fn pump(&mut self, now_us: u64) -> Option<BwOutcome> {
        let act = self.active.as_ref()?;
        if now_us.saturating_sub(act.started_us) >= BW_PROBE_SETTLE_TIMEOUT_US {
            return Some(self.settle(false));
        }
        None
    }

    /// 是否已结算（会话只探测一次，结算后本估计器变空操作）。
    pub fn is_settled(&self) -> bool {
        self.settled
    }

    /// 结算（仅在有活跃探测时调用）。
    fn settle(&mut self, completed: bool) -> BwOutcome {
        let act = self
            .active
            .take()
            .expect("settle 仅在 active 时由 on_chunk/pump 调用");
        self.settled = true;
        let estimate_bps = match (
            act.received_bytes,
            act.first_arrival_us,
            act.last_arrival_us,
        ) {
            (0, _, _) => None, // 零实收
            (_, Some(first), Some(last)) => {
                let span_us = last.saturating_sub(first);
                if span_us >= BW_PROBE_MIN_SPAN_US {
                    // 只计 padding × 8 / 秒 = bytes*8*1e6/µs（B10：粗估）。
                    Some(act.received_bytes * 8 * 1_000_000 / span_us)
                } else {
                    None // 退化（同拍到达）：不除零、不产出 ∞（坑 5）
                }
            }
            (_, _, _) => None,
        };
        BwOutcome {
            probe_id: act.probe_id,
            estimate_bps,
            received_bytes: act.received_bytes,
            expected_bytes: u64::from(act.expected_total_bytes),
            completed,
        }
    }
}

/// 估速 → 初始码率（B6）：clamp(estimate × [`INITIAL_BITRATE_FACTOR`],
/// [`INITIAL_BITRATE_MIN_BPS`], host_cap)。estimate 为 None → 返回 None
/// （调用方保持协商值）。host_cap < 下限（病态协商）→ 取 host_cap
/// （host 裁定优先，不得超发）。
pub fn initial_bitrate_from(estimate_bps: Option<u64>, host_cap_bps: u32) -> Option<u32> {
    let est = estimate_bps?;
    let cap = u64::from(host_cap_bps);
    if cap < INITIAL_BITRATE_MIN_BPS {
        return Some(host_cap_bps); // 病态协商：host 裁定优先
    }
    let scaled = (est as f64 * INITIAL_BITRATE_FACTOR) as u64;
    Some(scaled.clamp(INITIAL_BITRATE_MIN_BPS, cap) as u32)
}

/// u64 中位数（偶数个取中间两数均值向下取整；空 = None）。
fn median_u64(v: &[u64]) -> Option<u64> {
    if v.is_empty() {
        return None;
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    let n = s.len();
    Some(if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2
    })
}

/// i64 中位数（同 [`median_u64`]；空 = None）。
///
/// 偶数窗均值经 i128 中转（修复卡 C-2，发现 2）：裸加 `(a+b)/2` 在
/// 两样本都近 [`i64::MAX`] 时溢出（debug panic / release 回绕）。时钟
/// 偏移样本是 i64 且可为负，位技巧 `a/2+b/2+(a&b&1)` 在负数截断除法
/// 语义下不成立（如 a=b=-3 时算出 -1 而非 -3），故用加宽中转——语义
/// 与旧实现的无溢出区间完全一致（截断除法）。
fn median_i64(v: &[i64]) -> Option<i64> {
    if v.is_empty() {
        return None;
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    let n = s.len();
    Some(if n % 2 == 1 {
        s[n / 2]
    } else {
        ((i128::from(s[n / 2 - 1]) + i128::from(s[n / 2])) / 2) as i64
    })
}

// ─────────────────────────────────────────────────────────────────
// 测试：全部离线确定性、逻辑时钟注入，无网络、无系统钟
// ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一条 ProbePing。
    fn ping(id: u32, t0: u64) -> ControlMessage {
        ControlMessage::ProbePing {
            probe_id: id,
            sender_clock_us: t0,
        }
    }

    /// B1/B2：逻辑时钟 0 / 0.9s / 1.0s / 2.0s 推进 → 恰 2 条 ping
    /// （首次泵不立即发，从首拍起算满一个间隔），probe_id 递增不重复。
    #[test]
    fn ping_pump_fires_once_per_interval() {
        let mut e = ProbeEngine::new();
        let mut pings = Vec::new();
        for now in [0u64, 900_000, 1_000_000, 2_000_000] {
            if let Some(p) = e.pump(now) {
                pings.push(p);
            }
        }
        assert_eq!(pings.len(), 2, "四拍恰两条 ping：{pings:?}");
        let ids: Vec<u32> = pings
            .iter()
            .map(|p| match p {
                ControlMessage::ProbePing { probe_id, .. } => *probe_id,
                _ => panic!("pump 只应产出 ProbePing：{p:?}"),
            })
            .collect();
        assert_eq!(ids, vec![1, 2], "probe_id 从 1 递增不重复");
        assert_eq!(e.stats().pings_sent, 2);
    }

    /// B2：t0=1_000_000、t1=1_000_500、t2=1_002_000 → rtt=2_000µs、
    /// offset=−500µs（方向 = 对端 − 本端，NTP 中点法）。
    #[test]
    fn pong_yields_rtt_and_ntp_midpoint_offset() {
        let mut e = ProbeEngine::new();
        assert!(e.pump(0).is_none(), "首次泵只锚定不立即发");
        let Some(ControlMessage::ProbePing {
            probe_id: id,
            sender_clock_us: t0,
        }) = e.pump(1_000_000)
        else {
            panic!("锚定后一秒应发 ping");
        };
        assert_eq!(t0, 1_000_000);
        let pong = ControlMessage::ProbePong {
            probe_id: id,
            echoed_clock_us: 1_000_000,
            responder_clock_us: 1_000_500,
        };
        let s = e.on_pong(&pong, 1_002_000).expect("在途 pong 必产样本");
        assert_eq!(s.rtt_us, 2_000, "rtt = t2 − t0");
        assert_eq!(s.offset_us, -500, "offset = t1 − (t0+t2)/2");
        assert_eq!(e.stats().pongs_received, 1);
    }

    /// B3：to_local_us(remote) = remote − offset，正/负偏移各一例，
    /// 换算误差为 0。
    #[test]
    fn offset_converts_remote_to_local() {
        // 正偏移 +500：t0=1_000_000、t2=1_001_000 → mid=1_000_500，
        // t1=1_001_000 → offset = +500。
        let mut e = ProbeEngine::new();
        assert!(e.pump(0).is_none());
        let Some(ControlMessage::ProbePing { probe_id: id, .. }) = e.pump(1_000_000) else {
            unreachable!()
        };
        e.on_pong(
            &ControlMessage::ProbePong {
                probe_id: id,
                echoed_clock_us: 1_000_000,
                responder_clock_us: 1_001_000,
            },
            1_001_000,
        )
        .expect("样本");
        assert_eq!(e.to_local_us(10_000), Some(9_500), "local = remote − 500");

        // 负偏移 −500：t0=1_000_000、t2=1_001_000、t1=1_000_000。
        let mut e2 = ProbeEngine::new();
        assert!(e2.pump(0).is_none());
        let Some(ControlMessage::ProbePing { probe_id: id2, .. }) = e2.pump(1_000_000) else {
            unreachable!()
        };
        e2.on_pong(
            &ControlMessage::ProbePong {
                probe_id: id2,
                echoed_clock_us: 1_000_000,
                responder_clock_us: 1_000_000,
            },
            1_001_000,
        )
        .expect("样本");
        assert_eq!(e2.to_local_us(10_000), Some(10_500), "local = remote + 500");

        // 无样本 → None（不是 0）。
        assert_eq!(ProbeEngine::new().to_local_us(10_000), None);
    }

    /// 3s 无应答 → probes_lost 递增，样本窗口不受污染；迟到 pong 被忽略。
    #[test]
    fn stale_ping_counts_lost_after_timeout() {
        let mut e = ProbeEngine::new();
        assert!(e.pump(0).is_none());
        let Some(ControlMessage::ProbePing {
            probe_id: id,
            sender_clock_us,
        }) = e.pump(1_000_000)
        else {
            unreachable!()
        };
        assert_eq!(sender_clock_us, 1_000_000);
        // t0=1s，3s 后（now=4s+1µs ≥ t0+3s）判失并发出新探测。
        let new_ping = e.pump(4_000_001).expect("间隔早已到点，应发新 ping");
        let _ = new_ping;
        assert_eq!(e.stats().probes_lost, 1);
        // 迟到的 pong（id 已不在途）→ 忽略，不入窗口。
        let late = ControlMessage::ProbePong {
            probe_id: id,
            echoed_clock_us: 1_000_000,
            responder_clock_us: 1_000_500,
        };
        assert!(e.on_pong(&late, 4_500_000).is_none());
        let s = e.stats();
        assert_eq!(s.pongs_received, 0);
        assert_eq!(s.rtt_us_median, None, "窗口不受污染");
        assert_eq!(s.offset_us_median, None);
    }

    /// 未知 probe_id / 重复 pong → None，不 panic 不入窗口。
    #[test]
    fn unsolicited_pong_is_ignored() {
        let mut e = ProbeEngine::new();
        // 未发过任何 ping 的未知 id。
        let ghost = ControlMessage::ProbePong {
            probe_id: 99,
            echoed_clock_us: 0,
            responder_clock_us: 0,
        };
        assert!(e.on_pong(&ghost, 100).is_none());
        // 重复 pong：第一次消费、第二次忽略。
        assert!(e.pump(0).is_none());
        let Some(ControlMessage::ProbePing { probe_id: id, .. }) = e.pump(1_000_000) else {
            unreachable!()
        };
        let pong = ControlMessage::ProbePong {
            probe_id: id,
            echoed_clock_us: 1_000_000,
            responder_clock_us: 1_000_100,
        };
        assert!(e.on_pong(&pong, 1_001_000).is_some());
        assert!(e.on_pong(&pong, 1_002_000).is_none(), "重复 pong 忽略");
        assert_eq!(e.stats().pongs_received, 1);
    }

    /// 喂超窗口数量样本（含离群值）→ 中位数取最近
    /// `PROBE_MEDIAN_WINDOW` 个。
    #[test]
    fn median_window_uses_recent_samples() {
        let mut e = ProbeEngine::new();
        assert!(e.pump(0).is_none());
        let mut now = 1_000_000u64;
        for i in 0..(PROBE_MEDIAN_WINDOW + 2) {
            let id = u32::try_from(i).unwrap() + 1;
            let Some(ControlMessage::ProbePing { .. }) = e.pump(now) else {
                unreachable!("步进 2s ≥ 1s 间隔，每拍必发 ping");
            };
            // 前两个样本是离群值（offset=10_000：t1 = mid + 10_000），
            // 其余恒 100（t1 = mid + 100 = now + 200）；mid = now + 100
            // （t2 = now + 200）。离群值被窗口挤出后中位数应为 100。
            let t1 = if i < 2 { now + 10_100 } else { now + 200 };
            e.on_pong(
                &ControlMessage::ProbePong {
                    probe_id: id,
                    echoed_clock_us: now,
                    responder_clock_us: t1,
                },
                now + 200,
            )
            .expect("样本");
            now += 2_000_000;
        }
        let s = e.stats();
        assert_eq!(s.offset_us_median, Some(100), "离群值被窗口挤出");
        assert_eq!(s.rtt_us_median, Some(200));
    }

    /// host 侧应答：echoed 原样回显、responder = 注入钟；非 Ping 输入
    /// 返回 None。
    #[test]
    fn pong_for_echoes_and_stamps_responder_clock() {
        let pong = pong_for(&ping(7, 123), 9_876).expect("ProbePing 必有 Pong");
        assert_eq!(
            pong,
            ControlMessage::ProbePong {
                probe_id: 7,
                echoed_clock_us: 123,
                responder_clock_us: 9_876,
            }
        );
        assert!(pong_for(
            &ControlMessage::Bye {
                reason: proto::ByeReason::Normal
            },
            1
        )
        .is_none());
    }

    /// B5：256KB → 块数 = ceil(256KB/1024) = 256、单块 ≤
    /// `max_media_payload(1200)` = 1120、padding 总和 = total_bytes；
    /// 逐块解析还原 (probe_id, chunk_idx, padding)。
    #[test]
    fn bw_chunks_respect_size_and_count_budget() {
        let total = 256 * 1024u32;
        let chunks = bw_probe_chunks(42, total).expect("256KB 合法");
        assert_eq!(
            chunks.len(),
            (total as usize).div_ceil(BW_PROBE_CHUNK_PAYLOAD)
        );
        let budget = rc_net::quic::max_media_payload(1200);
        let mut padding_sum = 0u64;
        for (idx, c) in chunks.iter().enumerate() {
            assert!(
                c.len() <= budget,
                "单块 {} 字节越 N2 预算 {}",
                c.len(),
                budget
            );
            assert_eq!(c[0], proto::TYPE_BW_PROBE_DATA);
            match parse_bw_datagram(c).expect("探测块必可解析") {
                ControlMessage::BwProbeData {
                    probe_id,
                    chunk_idx,
                    padding,
                } => {
                    assert_eq!(probe_id, 42);
                    assert_eq!(chunk_idx as usize, idx, "chunk_idx 连续从 0 起");
                    padding_sum += padding.len() as u64;
                }
                other => panic!("应解出 BwProbeData：{other:?}"),
            }
        }
        assert_eq!(padding_sum, u64::from(total), "padding 总和 = total_bytes");
        assert_eq!(chunks[0].len(), 9 + BW_PROBE_CHUNK_PAYLOAD);
        // 末块取余量：total=1025 → 2 块，末块 padding=1。
        let tail = bw_probe_chunks(1, 1025).expect("合法");
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[1].len(), 9 + 1);
    }

    /// total_bytes = 0 / 256KB+1 → Err(InvalidConfig)，不产任何块；
    /// parse 首字节非 0x09 → Err(Protocol)。
    #[test]
    fn bw_request_over_limit_is_rejected() {
        assert!(bw_probe_chunks(1, 0).is_err());
        assert!(bw_probe_chunks(1, BW_PROBE_MAX_TOTAL_BYTES + 1).is_err());
        let err = bw_probe_chunks(1, 0).unwrap_err();
        assert!(matches!(err, Error::InvalidConfig(_)), "{err}");
        // 非 0x09 首字节（合法的其他控制消息）→ Protocol 拒收。
        let mut hello = Vec::new();
        ControlMessage::IdrRequest {
            reason: proto::IdrReason::WatchdogStall,
        }
        .encode(&mut hello);
        assert!(matches!(
            parse_bw_datagram(&hello),
            Err(Error::Protocol { .. })
        ));
    }

    /// 脚本化到达（注入首末间隔）→ 估速 = 实收字节×8/间隔（确定性断言）。
    #[test]
    fn bw_estimate_from_first_last_arrival_span() {
        let mut est = BwEstimator::new();
        est.start(3, 8 * 1024, 0);
        for i in 0..8u32 {
            let chunk = bw_probe_chunks(3, 8 * 1024).unwrap()[i as usize].clone();
            let now = u64::from(i) * 1_000; // 首块 t=0，末块 t=7_000µs
            let out = est.on_chunk(&parse_bw_datagram(&chunk).unwrap(), now);
            if i == 7 {
                let o = out.expect("最高块到达立即结算");
                assert!(o.completed);
                assert_eq!(o.probe_id, 3);
                assert_eq!(o.received_bytes, 8 * 1024);
                assert_eq!(o.expected_bytes, 8 * 1024);
                // 8192×8 bit / 7_000µs = 65_536×1e6/7_000 bps（整数除法）。
                assert_eq!(o.estimate_bps, Some(8 * 1024 * 8 * 1_000_000 / 7_000));
            } else {
                assert!(out.is_none(), "中间块不结算");
            }
        }
        assert!(est.is_settled());
    }

    /// 丢 25% 块 → 超时路径仍按实收结算出估速（偏保守）。
    #[test]
    fn bw_estimate_tolerates_missing_chunks() {
        let mut est = BwEstimator::new();
        let total = 8 * 1024u32;
        let chunks = bw_probe_chunks(5, total).unwrap();
        est.start(5, total, 0);
        // 丢 chunk 1 与最高块 7（25%），其余按 1ms 间隔到达。
        for i in [0u32, 2, 3, 4, 5, 6] {
            let now = u64::from(i) * 1_000;
            assert!(
                est.on_chunk(&parse_bw_datagram(&chunks[i as usize]).unwrap(), now)
                    .is_none(),
                "未到最高块不结算"
            );
        }
        let o = est.pump(2_000_000).expect("结算超时按实收结算");
        assert!(!o.completed, "超时路径");
        assert_eq!(o.received_bytes, 6 * 1024, "实收 6 块");
        // 首块 t=0、末块（idx 6）t=6_000µs → span=6_000。
        assert_eq!(o.estimate_bps, Some(6 * 1024 * 8 * 1_000_000 / 6_000));
        assert!(est.is_settled());
        assert!(est.pump(3_000_000).is_none(), "结算后为空操作");
    }

    /// 收到最高 chunk_idx 即结算，不等超时（上一测试已覆盖正常路径，
    /// 这里钉「先收最高块」的乱序场景）。
    #[test]
    fn bw_settles_on_last_chunk_immediately() {
        let mut est = BwEstimator::new();
        let total = 4 * 1024u32;
        let chunks = bw_probe_chunks(9, total).unwrap();
        est.start(9, total, 100_000);
        // 乱序：最高块先到 → 立即结算（completed=true，实收仅 1 块）。
        let o = est
            .on_chunk(&parse_bw_datagram(&chunks[3]).unwrap(), 150_000)
            .expect("最高块先到也立即结算");
        assert!(o.completed);
        assert_eq!(o.received_bytes, 1024);
        assert!(est.is_settled());
        // 后到的低块：结算后空操作。
        assert!(est
            .on_chunk(&parse_bw_datagram(&chunks[0]).unwrap(), 200_000)
            .is_none());
    }

    /// 首末块同拍（间隔 <1ms）→ estimate = None → 保持协商值（B6）。
    #[test]
    fn bw_degenerate_span_yields_none() {
        let mut est = BwEstimator::new();
        let total = 2 * 1024u32;
        let chunks = bw_probe_chunks(2, total).unwrap();
        est.start(2, total, 0);
        assert!(
            est.on_chunk(&parse_bw_datagram(&chunks[0]).unwrap(), 500)
                .is_none(),
            "首块不结算"
        );
        let o = est
            .on_chunk(&parse_bw_datagram(&chunks[1]).unwrap(), 900)
            .expect("最高块到达结算");
        assert_eq!(o.estimate_bps, None, "间隔 400µs < 1ms = 退化");
        assert!(o.completed);
        assert!(initial_bitrate_from(o.estimate_bps, 8_000_000).is_none());
    }

    /// est=1Mbps → 2M 下限；est=200Mbps → host 上限；est=None → None；
    /// host_cap < 下限 → host_cap；中间值不打折断（10M×0.7=7M）。
    #[test]
    fn initial_bitrate_clamps_low_high_and_cap() {
        assert_eq!(
            initial_bitrate_from(Some(1_000_000), 50_000_000),
            Some(2_000_000),
            "1M×0.7=0.7M → 下限 2M"
        );
        assert_eq!(
            initial_bitrate_from(Some(200_000_000), 50_000_000),
            Some(50_000_000),
            "200M×0.7=140M → host 上限"
        );
        assert_eq!(initial_bitrate_from(None, 50_000_000), None);
        assert_eq!(
            initial_bitrate_from(Some(10_000_000), 1_000_000),
            Some(1_000_000),
            "host_cap < 下限 → host_cap（host 裁定优先）"
        );
        assert_eq!(
            initial_bitrate_from(Some(10_000_000), 50_000_000),
            Some(7_000_000),
            "中间值 = est×0.7"
        );
    }

    /// 修复卡 C-2（发现 2）：median_i64 偶数窗的均值不得溢出——两个
    /// i64::MAX 样本不 panic、不回绕，中值就是 i64::MAX 本身；极值混窗
    /// 与负数窗的截断语义与旧实现的无溢出区间一致。
    #[test]
    fn median_i64_overflow_safe_at_extremes() {
        assert_eq!(
            median_i64(&[i64::MAX, i64::MAX]),
            Some(i64::MAX),
            "MAX+MAX 裸加必溢出；安全中值 = i64::MAX"
        );
        assert_eq!(median_i64(&[i64::MIN, i64::MIN]), Some(i64::MIN));
        // MIN+MAX 同样溢出：均值 = -0.5 → 截断 0。
        assert_eq!(median_i64(&[i64::MIN, i64::MAX]), Some(0));
        assert_eq!(median_i64(&[i64::MAX, i64::MIN]), Some(0), "无序输入先排序");
        // 负数偶数窗：(-3 + -4)/2 = -3.5 → 截断 -3（与旧实现一致）。
        assert_eq!(median_i64(&[-3, -4]), Some(-3));
        // 奇数窗不经过均值路径，极值原样返回。
        assert_eq!(median_i64(&[i64::MAX, 0, i64::MAX]), Some(i64::MAX));
        assert_eq!(median_i64(&[]), None);
    }
}
