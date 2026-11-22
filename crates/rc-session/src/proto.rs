//! 控制协议线格式（M2 会话层）。
//!
//! 消息体 = `[type u8] + 载荷`；多字节整数一律小端；变长段带
//! `[len u16 LE]` 前缀（[`ControlMessage::BwProbeData`] 的 padding 例外：
//! 剩余字节即 padding，见变体文档）。
//!
//! # 通道约定（边界知识，本模块不做 IO）
//!
//! 除 BwProbeData 走 rc-net datagram 通道外，其余消息全部走 reliable
//! 流。rc-net 的 reliable 流已按 `u32 LE` 长度前缀成帧并**按消息边界**
//! 交付（`NetEvent::Reliable` 一条 = 一条完整消息），因此
//! [`ControlMessage::decode`] 的输入恰是一条完整消息的字节，**不含**
//! 传输层长度前缀，本模块也不再自包一层消息级前缀（双层前缀论证见
//! rc-input `channel.rs`）。
//!
//! # 校验边界（宁拒不猜）
//!
//! decode 严格校验线格式完整性：版本号、枚举值域、布尔 0/1、flags
//! 已知位、变长段长度一致性、截断 / 尾字节。**数值参数（width/fps/bps
//! 等）不做语义校验**（0 或离谱值照收）——夹紧与裁定是 T14 host 的
//! 职责。一切错误返回 [`rc_types::Error::Protocol`]（带 offset 与出错
//! 值），绝不 panic、绝不静默跳过。

use rc_types::{Error, Result};

/// 控制协议版本。Hello 携带，不匹配即拒收（本模块校验，T14 直接
/// 消费错误：`Error::Protocol` 本就是 `is_fatal() == true`）。
pub const PROTOCOL_VERSION: u8 = 1;

// ── 类型号（0x01..=0x0C 本卡实现；0x0D..=0x0F 冻结占号，T19/T20 增量添加）──

/// 线格式类型字节：`Hello`（client→host 握手请求）。
pub const TYPE_HELLO: u8 = 0x01;
/// 线格式类型字节：`Welcome`（host→client 握手裁定）。
pub const TYPE_WELCOME: u8 = 0x02;
/// 线格式类型字节：`IdrRequest`（client→host 请求关键帧）。
pub const TYPE_IDR_REQUEST: u8 = 0x03;
/// 线格式类型字节：`IdrAck`（host→client 关键帧已编出）。
pub const TYPE_IDR_ACK: u8 = 0x04;
/// 线格式类型字节：`ProbePing`（双向 RTT/偏移探测）。
pub const TYPE_PROBE_PING: u8 = 0x05;
/// 线格式类型字节：`ProbePong`（双向探测应答）。
pub const TYPE_PROBE_PONG: u8 = 0x06;
/// 线格式类型字节：`SetBitrate`（client→host 设定码率）。
pub const TYPE_SET_BITRATE: u8 = 0x07;
/// 线格式类型字节：`BwProbeRequest`（client→host 请求探测流）。
pub const TYPE_BW_PROBE_REQUEST: u8 = 0x08;
/// 线格式类型字节：`BwProbeData`（host→client 探测数据，走 datagram）。
pub const TYPE_BW_PROBE_DATA: u8 = 0x09;
/// 线格式类型字节：`InputAck`（host→client 输入注入回执）。
pub const TYPE_INPUT_ACK: u8 = 0x0A;
/// 线格式类型字节：`ReconnectRequest`（client→host 重连续传请求）。
pub const TYPE_RECONNECT_REQUEST: u8 = 0x0B;
/// 线格式类型字节：`Bye`（双向优雅关闭 / 饱和刷新）。
pub const TYPE_BYE: u8 = 0x0C;
/// 预留：鉴权挑战（M2-5 / T19）。占号公开，载荷语义 T19 定义。
pub const TYPE_AUTH_CHALLENGE: u8 = 0x0D;
/// 预留：鉴权应答（M2-5 / T19）。
pub const TYPE_AUTH_RESPONSE: u8 = 0x0E;
/// 预留：剪贴板文本通知（M2-8 / T20）。
pub const TYPE_CLIPBOARD_NOTIFY: u8 = 0x0F;

/// [`ControlMessage::BwProbeRequest`] 的 `total_bytes` 线格式上限
/// （设计师裁定 256KB）：恶意 client 不得借探测流量打满带宽。
pub const MAX_BW_PROBE_BYTES: u32 = 256 * 1024;

/// Hello 请求 / Welcome 裁定共用的流参数（client 请求值，host 夹紧后
/// 的真值）。**线格式不做语义校验**：0 或离谱值照收，夹紧归 T14。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionParams {
    /// 画面宽（像素）。
    pub width: u32,
    /// 画面高（像素）。
    pub height: u32,
    /// 帧率（fps）。
    pub fps: u32,
    /// 视频码率（bps）。
    pub bitrate_bps: u32,
}

/// Hello 的鉴权方式。0=无（本轮唯一实装），1=密码（M2-5 / T19 预留）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AuthMethod {
    /// 无鉴权（本阶段唯一实装值）。
    None = 0,
    /// 密码鉴权（M2-5 / T19 实装）。
    Password = 1,
}

/// Welcome 的鉴权状态。2=拒绝并断开。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AuthStatus {
    /// 通过（或无需鉴权）。
    Ok = 0,
    /// 需要挑战应答（T19）。
    ChallengeRequired = 1,
    /// 拒绝并断开。
    Rejected = 2,
}

/// IdrRequest 的原因（M2-3）：0=解码器 NeedKeyframe、1=停滞看门狗、
/// 2=重连恢复。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum IdrReason {
    /// 解码器报告 NeedKeyframe。
    DecoderNeedKeyframe = 0,
    /// 停滞看门狗触发（T16）。
    WatchdogStall = 1,
    /// 重连恢复（T17）。
    ReconnectRecovery = 2,
}

/// SetBitrate 的原因（M2-10）：0=初始、1=探测、2=手动。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BitrateReason {
    /// 会话初始码率。
    Initial = 0,
    /// 带宽探测调整（T15）。
    Probe = 1,
    /// 用户手动设定。
    Manual = 2,
}

/// Bye 的原因：0=正常关闭、1=u32 时间戳饱和刷新（T17：host 钟逼近
/// 71.6 分钟守卫线时发 1，client 以 resume=false 全新重连）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ByeReason {
    /// 正常关闭。
    Normal = 0,
    /// 时钟饱和刷新（T17）。
    Refresh = 1,
}

/// Welcome 的 host 能力位（newtype：位的语义只在访问器里展开）。
/// 未知 bit **拒收**（版本升级走 version 协商，不靠容忍未知位）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostFlags(pub u8);

impl HostFlags {
    /// bit0：ReconnectRequest.resume 被接受。
    pub const RESUME_ACCEPTED: u8 = 0b0000_0001;
    /// bit1：时钟纪元已重置（饱和刷新后新会话）。
    pub const CLOCK_EPOCH_RESET: u8 = 0b0000_0010;
    /// 已定义位的掩码；decode 用它拒绝未知 bit。
    pub const KNOWN_MASK: u8 = Self::RESUME_ACCEPTED | Self::CLOCK_EPOCH_RESET;

    /// bit0 是否置位（ReconnectRequest.resume 被接受）。
    pub const fn resume_accepted(&self) -> bool {
        self.0 & Self::RESUME_ACCEPTED != 0
    }

    /// bit1 是否置位（时钟纪元已重置）。
    pub const fn clock_epoch_reset(&self) -> bool {
        self.0 & Self::CLOCK_EPOCH_RESET != 0
    }
}

/// 0x01 Hello（client→host）。version ≠ PROTOCOL_VERSION 在 decode 即
/// 拒收（本模块无状态机，拒收语义全部落在线格式层）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    /// 线上原样字段；decode 成功后恒等于 PROTOCOL_VERSION。
    pub version: u8,
    /// client 生成的会话标识（随机 u64，T14 负责生成策略）。
    pub session_id: u64,
    /// 请求参数（host 侧按显示器与编码器能力夹紧，见 Welcome）。
    pub requested: SessionParams,
    /// 鉴权方式。本轮会话层只接受 None（T19 实装 Password）。
    pub auth_method: AuthMethod,
    /// 鉴权载荷（线上带 auth_len u16 前缀；T19 前恒空）。
    pub auth_payload: Vec<u8>,
}

/// 0x02 Welcome（host→client）：session_id 回显 + 裁定参数 + 状态位。
/// client 一律以 granted（裁定真值）为准。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Welcome {
    /// 回显 Hello 的会话标识。
    pub session_id: u64,
    /// host 裁定后的流参数（真值）。
    pub granted: SessionParams,
    /// 鉴权状态。
    pub auth_status: AuthStatus,
    /// host 能力位。
    pub host_flags: HostFlags,
}

/// 一条控制消息。**通道约定**：除 [`ControlMessage::BwProbeData`] 走
/// datagram 通道外，其余全部走 rc-net reliable 流（rc-net 已按消息边界
/// 交付，decode 输入恰是一条完整消息的字节，不含传输层 u32 长度前缀）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlMessage {
    /// 0x01 Hello：client→host 握手请求（见 [`Hello`]）。
    Hello(Hello),
    /// 0x02 Welcome：host→client 握手裁定（见 [`Welcome`]）。
    Welcome(Welcome),
    /// 0x03 client→host。host 收到即 `HostPipeline::request_idr()`
    /// （T14 接线）。
    IdrRequest {
        /// 请求关键帧的原因。
        reason: IdrReason,
    },
    /// 0x04 host→client。frame_seq 对应 host 真编出的 IDR 帧
    /// （T16 去抖 / 复核用）。
    IdrAck {
        /// host 编出的 IDR 帧序号。
        frame_seq: u64,
    },
    /// 0x05 双向（T15）：RTT + 时钟偏移探测，默认 1s 间隔，兼保活。
    ProbePing {
        /// 探测序号（Pong 原样回显）。
        probe_id: u32,
        /// 发起方发送时刻（会话钟，微秒）。**u64 而非 rc-types 的
        /// u32 `Timestamp`**：后者 71.6 分钟饱和，线格式本身不能饱和
        /// （饱和守卫是 T17 会话层职责）。
        sender_clock_us: u64,
    },
    /// 0x06 双向（T15）：NTP 风格回显。发起方 RTT = now − t0，
    /// 偏移 = 中点差。
    ProbePong {
        /// 回显的探测序号。
        probe_id: u32,
        /// 原样回显的 Ping.sender_clock_us。
        echoed_clock_us: u64,
        /// 应答方应答时刻（会话钟，微秒，u64 同上）。
        responder_clock_us: u64,
    },
    /// 0x07 client→host（T15）：经 HostPipeline::set_bitrate 转发编码
    /// 线程。
    SetBitrate {
        /// 目标码率（bps）。
        bps: u32,
        /// 设定原因。
        reason: BitrateReason,
    },
    /// 0x08 client→host（T15 v0）：请求 host 发送 total_bytes 字节的
    /// 带宽探测流。total_bytes > MAX_BW_PROBE_BYTES 在 decode 即拒收。
    BwProbeRequest {
        /// 探测序号（Data 原样携带）。
        probe_id: u32,
        /// 请求的探测流总字节数（≤ 256KB）。
        total_bytes: u32,
    },
    /// 0x09 host→client，**走 datagram 通道**（不占 reliable 控制流）。
    /// padding 无长度前缀：消息为 `[type][probe_id u32][chunk_idx u32]`
    /// 加剩余字节（剩余即 padding，边界由承载它的数据报给出，消息内
    /// 不再自带上限）。注意：v0 测的是 pacer 天花板（50Mbps）而非真实
    /// 带宽，只是粗启发式，**禁止据此宣称「精确带宽」**（T15 / M3-3
    /// 边界）。
    BwProbeData {
        /// 回显的探测序号。
        probe_id: u32,
        /// 探测流分片序号。
        chunk_idx: u32,
        /// 不透明填充字节（线格式不解释其内容）。
        padding: Vec<u8>,
    },
    /// 0x0A host→client：key_batch_seq 按注入批计数（两侧 reliable
    /// 有序流独立计数天然对齐）；inject_done_us 为 host 会话钟，
    /// client 用偏移换算输入单程延迟（T15/T18）。
    InputAck {
        /// 被确认的输入批序号。
        key_batch_seq: u64,
        /// host 注入完成时刻（会话钟，微秒，u64 不饱和）。
        inject_done_us: u64,
    },
    /// 0x0B client→host（T17）：resume=1 请求保持管线续传；host 以
    /// Welcome.host_flags.bit0 应答是否接受。
    ReconnectRequest {
        /// 要恢复的会话标识（原 Hello 的 session_id）。
        session_id: u64,
        /// `true` = 请求保持管线续传（线上严格 0/1）。
        resume: bool,
    },
    /// 0x0C 双向：优雅关闭 / 饱和刷新。
    Bye {
        /// 关闭原因。
        reason: ByeReason,
    },
}

impl ControlMessage {
    /// 编码并**追加**到 `out`（风格对齐 rc-input 的 `encode_events`）。
    /// 空载荷变体只写类型字节。不失败、不 panic。
    ///
    /// `Hello.version` 原样写出（线上原样字段）：合法构造
    /// （version = [`PROTOCOL_VERSION`]）编出的包必可被 [`decode`] 解
    /// 回；构造非法 version 是调用方 bug，编出的包会被对端 decode
    /// 显式拒收而不是被悄悄纠正。
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            ControlMessage::Hello(h) => {
                out.push(TYPE_HELLO);
                out.push(h.version);
                out.extend_from_slice(&h.session_id.to_le_bytes());
                encode_params(&h.requested, out);
                out.push(h.auth_method as u8);
                // auth_payload 长度前缀（u16 LE）。T19 前恒空；若调用方
                // 塞入超过 u16 的载荷（bug），饱和为 u16::MAX——对端
                // decode 时长度前缀与实际字节数对不上会显式拒绝而非
                // 静默错乱（最后防线，对齐 rc-input encode_events）。
                let len = u16::try_from(h.auth_payload.len()).unwrap_or(u16::MAX);
                debug_assert!(
                    usize::from(len) == h.auth_payload.len(),
                    "auth_payload {} 字节超过 u16 长度上限（T19 前恒空，不该发生）",
                    h.auth_payload.len()
                );
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(&h.auth_payload);
            }
            ControlMessage::Welcome(w) => {
                out.push(TYPE_WELCOME);
                out.extend_from_slice(&w.session_id.to_le_bytes());
                encode_params(&w.granted, out);
                out.push(w.auth_status as u8);
                out.push(w.host_flags.0);
            }
            ControlMessage::IdrRequest { reason } => {
                out.push(TYPE_IDR_REQUEST);
                out.push(*reason as u8);
            }
            ControlMessage::IdrAck { frame_seq } => {
                out.push(TYPE_IDR_ACK);
                out.extend_from_slice(&frame_seq.to_le_bytes());
            }
            ControlMessage::ProbePing {
                probe_id,
                sender_clock_us,
            } => {
                out.push(TYPE_PROBE_PING);
                out.extend_from_slice(&probe_id.to_le_bytes());
                out.extend_from_slice(&sender_clock_us.to_le_bytes());
            }
            ControlMessage::ProbePong {
                probe_id,
                echoed_clock_us,
                responder_clock_us,
            } => {
                out.push(TYPE_PROBE_PONG);
                out.extend_from_slice(&probe_id.to_le_bytes());
                out.extend_from_slice(&echoed_clock_us.to_le_bytes());
                out.extend_from_slice(&responder_clock_us.to_le_bytes());
            }
            ControlMessage::SetBitrate { bps, reason } => {
                out.push(TYPE_SET_BITRATE);
                out.extend_from_slice(&bps.to_le_bytes());
                out.push(*reason as u8);
            }
            ControlMessage::BwProbeRequest {
                probe_id,
                total_bytes,
            } => {
                out.push(TYPE_BW_PROBE_REQUEST);
                out.extend_from_slice(&probe_id.to_le_bytes());
                out.extend_from_slice(&total_bytes.to_le_bytes());
            }
            ControlMessage::BwProbeData {
                probe_id,
                chunk_idx,
                padding,
            } => {
                out.push(TYPE_BW_PROBE_DATA);
                out.extend_from_slice(&probe_id.to_le_bytes());
                out.extend_from_slice(&chunk_idx.to_le_bytes());
                // padding 无长度前缀：剩余字节即 padding（边界由数据报给出）。
                out.extend_from_slice(padding);
            }
            ControlMessage::InputAck {
                key_batch_seq,
                inject_done_us,
            } => {
                out.push(TYPE_INPUT_ACK);
                out.extend_from_slice(&key_batch_seq.to_le_bytes());
                out.extend_from_slice(&inject_done_us.to_le_bytes());
            }
            ControlMessage::ReconnectRequest { session_id, resume } => {
                out.push(TYPE_RECONNECT_REQUEST);
                out.extend_from_slice(&session_id.to_le_bytes());
                out.push(u8::from(*resume));
            }
            ControlMessage::Bye { reason } => {
                out.push(TYPE_BYE);
                out.push(*reason as u8);
            }
        }
    }

    /// 解码一条完整消息（[`encode`](ControlMessage::encode) 的逆）。
    ///
    /// 空缓冲、未知类型字节、预留号（0x0D..=0x0F）、version 不匹配、
    /// 枚举字段越界、布尔字段非 0/1、host_flags 未知 bit、auth_len 与
    /// 实际字节数不符、载荷截断、尾部多余字节——一律返回
    /// [`rc_types::Error::Protocol`]（带 offset 与出错值），绝不 panic、
    /// 绝不静默跳过。
    pub fn decode(buf: &[u8]) -> Result<Self> {
        let Some(&ty) = buf.first() else {
            return Err(Error::Protocol {
                offset: 0,
                msg: "control message is empty (need at least a type byte)".into(),
            });
        };
        let mut r = Reader::new(buf);
        let msg = match ty {
            TYPE_HELLO => ControlMessage::Hello(decode_hello(&mut r)?),
            TYPE_WELCOME => ControlMessage::Welcome(decode_welcome(&mut r)?),
            TYPE_IDR_REQUEST => {
                let reason = decode_idr_reason(r.u8("idr_request.reason")?, r.off - 1)?;
                r.done("IdrRequest")?;
                ControlMessage::IdrRequest { reason }
            }
            TYPE_IDR_ACK => {
                let frame_seq = r.u64("idr_ack.frame_seq")?;
                r.done("IdrAck")?;
                ControlMessage::IdrAck { frame_seq }
            }
            TYPE_PROBE_PING => {
                let probe_id = r.u32("probe_ping.probe_id")?;
                let sender_clock_us = r.u64("probe_ping.sender_clock_us")?;
                r.done("ProbePing")?;
                ControlMessage::ProbePing {
                    probe_id,
                    sender_clock_us,
                }
            }
            TYPE_PROBE_PONG => {
                let probe_id = r.u32("probe_pong.probe_id")?;
                let echoed_clock_us = r.u64("probe_pong.echoed_clock_us")?;
                let responder_clock_us = r.u64("probe_pong.responder_clock_us")?;
                r.done("ProbePong")?;
                ControlMessage::ProbePong {
                    probe_id,
                    echoed_clock_us,
                    responder_clock_us,
                }
            }
            TYPE_SET_BITRATE => {
                let bps = r.u32("set_bitrate.bps")?;
                let reason = decode_bitrate_reason(r.u8("set_bitrate.reason")?, r.off - 1)?;
                r.done("SetBitrate")?;
                ControlMessage::SetBitrate { bps, reason }
            }
            TYPE_BW_PROBE_REQUEST => {
                let probe_id = r.u32("bw_probe_request.probe_id")?;
                let total_off = r.off;
                let total_bytes = r.u32("bw_probe_request.total_bytes")?;
                if total_bytes > MAX_BW_PROBE_BYTES {
                    return Err(Error::Protocol {
                        offset: total_off,
                        msg: format!(
                            "bw_probe_request.total_bytes {total_bytes} exceeds \
                             MAX_BW_PROBE_BYTES {MAX_BW_PROBE_BYTES}"
                        ),
                    });
                }
                r.done("BwProbeRequest")?;
                ControlMessage::BwProbeRequest {
                    probe_id,
                    total_bytes,
                }
            }
            TYPE_BW_PROBE_DATA => {
                let probe_id = r.u32("bw_probe_data.probe_id")?;
                let chunk_idx = r.u32("bw_probe_data.chunk_idx")?;
                // padding = 剩余全部字节（无前缀、无上限），天然无尾巴。
                let padding = r.rest().to_vec();
                ControlMessage::BwProbeData {
                    probe_id,
                    chunk_idx,
                    padding,
                }
            }
            TYPE_INPUT_ACK => {
                let key_batch_seq = r.u64("input_ack.key_batch_seq")?;
                let inject_done_us = r.u64("input_ack.inject_done_us")?;
                r.done("InputAck")?;
                ControlMessage::InputAck {
                    key_batch_seq,
                    inject_done_us,
                }
            }
            TYPE_RECONNECT_REQUEST => {
                let session_id = r.u64("reconnect_request.session_id")?;
                let resume = decode_bool(r.u8("reconnect_request.resume")?, r.off - 1, "resume")?;
                r.done("ReconnectRequest")?;
                ControlMessage::ReconnectRequest { session_id, resume }
            }
            TYPE_BYE => {
                let reason = decode_bye_reason(r.u8("bye.reason")?, r.off - 1)?;
                r.done("Bye")?;
                ControlMessage::Bye { reason }
            }
            // 预留号显式拒收：占号冻结，变体由 T19/T20 增量添加。
            TYPE_AUTH_CHALLENGE | TYPE_AUTH_RESPONSE => {
                return Err(reserved(ty, "auth (T19 / M2-5)"))
            }
            TYPE_CLIPBOARD_NOTIFY => {
                return Err(reserved(ty, "clipboard text notify (T20 / M2-8)"))
            }
            v => {
                return Err(Error::Protocol {
                    offset: 0,
                    msg: format!("unknown control message type 0x{v:02X}"),
                })
            }
        };
        Ok(msg)
    }
}

// ─────────────────────────────────────────────────────────────────
// 私有编码 / 解码辅助
// ─────────────────────────────────────────────────────────────────

/// 编码 SessionParams（4 × u32 LE，追加式）。
fn encode_params(p: &SessionParams, out: &mut Vec<u8>) {
    out.extend_from_slice(&p.width.to_le_bytes());
    out.extend_from_slice(&p.height.to_le_bytes());
    out.extend_from_slice(&p.fps.to_le_bytes());
    out.extend_from_slice(&p.bitrate_bps.to_le_bytes());
}

/// 小端读取器：在整条消息缓冲上从 offset 1（跳过类型字节）逐字段前进。
/// 任何截断以「缓冲末尾」为 offset 报 Protocol 错误（对齐 rc-input 的
/// `decode_one`）。
struct Reader<'a> {
    buf: &'a [u8],
    /// 下一个待读字节的偏移。
    off: usize,
}

impl<'a> Reader<'a> {
    /// 从一条完整消息（首字节为类型号）开始读取。
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, off: 1 }
    }

    /// 确保剩余至少还有 `n` 字节可读，否则报截断（offset = 缓冲末尾）。
    fn need(&self, n: usize, field: &str) -> Result<()> {
        if self.buf.len() - self.off < n {
            return Err(Error::Protocol {
                offset: self.buf.len(),
                msg: format!(
                    "{field} truncated at offset {}: need {n} bytes, got {}",
                    self.off,
                    self.buf.len() - self.off
                ),
            });
        }
        Ok(())
    }

    /// 读 1 字节。
    fn u8(&mut self, field: &str) -> Result<u8> {
        self.need(1, field)?;
        let v = self.buf[self.off];
        self.off += 1;
        Ok(v)
    }

    /// 读 u16 LE。
    fn u16(&mut self, field: &str) -> Result<u16> {
        self.need(2, field)?;
        let v = u16::from_le_bytes([self.buf[self.off], self.buf[self.off + 1]]);
        self.off += 2;
        Ok(v)
    }

    /// 读 u32 LE。
    fn u32(&mut self, field: &str) -> Result<u32> {
        self.need(4, field)?;
        let b = &self.buf[self.off..self.off + 4];
        let v = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        self.off += 4;
        Ok(v)
    }

    /// 读 u64 LE。
    fn u64(&mut self, field: &str) -> Result<u64> {
        self.need(8, field)?;
        let b = &self.buf[self.off..self.off + 8];
        let v = u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
        self.off += 8;
        Ok(v)
    }

    /// 读 `n` 字节切片（变长段，长度来自前缀）。
    fn take(&mut self, n: usize, field: &str) -> Result<&'a [u8]> {
        self.need(n, field)?;
        let s = &self.buf[self.off..self.off + n];
        self.off += n;
        Ok(s)
    }

    /// 剩余全部字节（BwProbeData 的 padding 语义）。
    fn rest(&self) -> &'a [u8] {
        &self.buf[self.off..]
    }

    /// 定长消息应在此结束；多余字节是成帧错误（offset = 期望消息末尾）。
    fn done(&self, ty: &str) -> Result<()> {
        if self.off < self.buf.len() {
            return Err(Error::Protocol {
                offset: self.off,
                msg: format!(
                    "{} trailing bytes after {ty} message (expected exactly {} bytes)",
                    self.buf.len() - self.off,
                    self.off
                ),
            });
        }
        Ok(())
    }
}

/// 解码 Hello 载荷（类型字节已由调用方消耗）。
fn decode_hello(r: &mut Reader) -> Result<Hello> {
    let version_off = r.off;
    let version = r.u8("hello.version")?;
    if version != PROTOCOL_VERSION {
        return Err(Error::Protocol {
            offset: version_off,
            msg: format!(
                "hello version {version} != PROTOCOL_VERSION {PROTOCOL_VERSION} \
                 (peer speaks a different protocol, refusing)"
            ),
        });
    }
    let session_id = r.u64("hello.session_id")?;
    let width = r.u32("hello.requested.width")?;
    let height = r.u32("hello.requested.height")?;
    let fps = r.u32("hello.requested.fps")?;
    let bitrate_bps = r.u32("hello.requested.bitrate_bps")?;
    let method_off = r.off;
    let auth_method = match r.u8("hello.auth_method")? {
        0 => AuthMethod::None,
        1 => AuthMethod::Password,
        v => {
            return Err(Error::Protocol {
                offset: method_off,
                msg: format!("hello.auth_method must be 0(None) or 1(Password), got {v}"),
            })
        }
    };
    let auth_len = usize::from(r.u16("hello.auth_len")?);
    let auth_payload = r.take(auth_len, "hello.auth_payload")?.to_vec();
    r.done("Hello")?;
    Ok(Hello {
        version,
        session_id,
        requested: SessionParams {
            width,
            height,
            fps,
            bitrate_bps,
        },
        auth_method,
        auth_payload,
    })
}

/// 解码 Welcome 载荷（类型字节已由调用方消耗）。
fn decode_welcome(r: &mut Reader) -> Result<Welcome> {
    let session_id = r.u64("welcome.session_id")?;
    let width = r.u32("welcome.granted.width")?;
    let height = r.u32("welcome.granted.height")?;
    let fps = r.u32("welcome.granted.fps")?;
    let bitrate_bps = r.u32("welcome.granted.bitrate_bps")?;
    let status_off = r.off;
    let auth_status = match r.u8("welcome.auth_status")? {
        0 => AuthStatus::Ok,
        1 => AuthStatus::ChallengeRequired,
        2 => AuthStatus::Rejected,
        v => {
            return Err(Error::Protocol {
                offset: status_off,
                msg: format!(
                "welcome.auth_status must be 0(Ok), 1(ChallengeRequired) or 2(Rejected), got {v}"
            ),
            })
        }
    };
    let flags_off = r.off;
    let raw_flags = r.u8("welcome.host_flags")?;
    if raw_flags & !HostFlags::KNOWN_MASK != 0 {
        return Err(Error::Protocol {
            offset: flags_off,
            msg: format!(
                "welcome.host_flags 0x{raw_flags:02X} has unknown bits 0x{:02X} \
                 (known mask 0x{:02X})",
                raw_flags & !HostFlags::KNOWN_MASK,
                HostFlags::KNOWN_MASK
            ),
        });
    }
    r.done("Welcome")?;
    Ok(Welcome {
        session_id,
        granted: SessionParams {
            width,
            height,
            fps,
            bitrate_bps,
        },
        auth_status,
        host_flags: HostFlags(raw_flags),
    })
}

/// 严格解码布尔字节：只接受 0/1（对齐 rc-input 的 `decode_bool`）。
fn decode_bool(v: u8, off: usize, field: &str) -> Result<bool> {
    match v {
        0 => Ok(false),
        1 => Ok(true),
        v => Err(Error::Protocol {
            offset: off,
            msg: format!("{field} must be 0 or 1, got {v}"),
        }),
    }
}

/// 解码 IdrReason（严格 0..=2）。
fn decode_idr_reason(v: u8, off: usize) -> Result<IdrReason> {
    match v {
        0 => Ok(IdrReason::DecoderNeedKeyframe),
        1 => Ok(IdrReason::WatchdogStall),
        2 => Ok(IdrReason::ReconnectRecovery),
        v => Err(Error::Protocol {
            offset: off,
            msg: format!(
                "idr_request.reason must be 0(DecoderNeedKeyframe), 1(WatchdogStall) \
                 or 2(ReconnectRecovery), got {v}"
            ),
        }),
    }
}

/// 解码 BitrateReason（严格 0..=2）。
fn decode_bitrate_reason(v: u8, off: usize) -> Result<BitrateReason> {
    match v {
        0 => Ok(BitrateReason::Initial),
        1 => Ok(BitrateReason::Probe),
        2 => Ok(BitrateReason::Manual),
        v => Err(Error::Protocol {
            offset: off,
            msg: format!("set_bitrate.reason must be 0(Initial), 1(Probe) or 2(Manual), got {v}"),
        }),
    }
}

/// 解码 ByeReason（严格 0..=1）。
fn decode_bye_reason(v: u8, off: usize) -> Result<ByeReason> {
    match v {
        0 => Ok(ByeReason::Normal),
        1 => Ok(ByeReason::Refresh),
        v => Err(Error::Protocol {
            offset: off,
            msg: format!("bye.reason must be 0(Normal) or 1(Refresh), got {v}"),
        }),
    }
}

/// 预留类型号的显式拒收错误（占号冻结，变体由 T19/T20 增量添加）。
fn reserved(ty: u8, owner: &str) -> Error {
    Error::Protocol {
        offset: 0,
        msg: format!(
            "reserved control message type 0x{ty:02X}: {owner} will define it, refusing to guess"
        ),
    }
}

// ─────────────────────────────────────────────────────────────────
// 测试：全部离线确定性，无网络、无时间依赖
// ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 样例参数（数值本身不做语义校验，取有辨识度的值锁线格式）。
    fn sample_params() -> SessionParams {
        SessionParams {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_bps: 8_000_000,
        }
    }

    /// 合法 Hello（version=PROTOCOL_VERSION、auth 恒空）。
    fn sample_hello() -> Hello {
        Hello {
            version: PROTOCOL_VERSION,
            session_id: 0xDEAD_BEEF_1234_5678,
            requested: sample_params(),
            auth_method: AuthMethod::None,
            auth_payload: vec![],
        }
    }

    /// 合法 Welcome（auth_status=ChallengeRequired、host_flags=0x03，
    /// 两个非零字节便于定位字段偏移）。
    fn sample_welcome() -> Welcome {
        Welcome {
            session_id: 0xDEAD_BEEF_1234_5678,
            granted: sample_params(),
            auth_status: AuthStatus::ChallengeRequired,
            host_flags: HostFlags(0x03),
        }
    }

    /// 12 个变体各一条合法样例。截断 / 尾巴测试用：Hello 的 auth_payload
    /// 与 BwProbeData 的 padding 都为空（padding 无前缀，非空时短前缀
    /// 仍是合法消息）。
    fn all_samples() -> Vec<ControlMessage> {
        vec![
            ControlMessage::Hello(sample_hello()),
            ControlMessage::Welcome(sample_welcome()),
            ControlMessage::IdrRequest {
                reason: IdrReason::WatchdogStall,
            },
            ControlMessage::IdrAck { frame_seq: 42 },
            ControlMessage::ProbePing {
                probe_id: 7,
                sender_clock_us: u64::MAX,
            },
            ControlMessage::ProbePong {
                probe_id: 0x1122_3344,
                echoed_clock_us: 0x0123_4567_89AB_CDEF,
                responder_clock_us: u64::MAX - 1,
            },
            // 数值参数不做语义校验：bps=0 照收（校验边界见模块文档）。
            ControlMessage::SetBitrate {
                bps: 0,
                reason: BitrateReason::Probe,
            },
            ControlMessage::BwProbeRequest {
                probe_id: 9,
                total_bytes: MAX_BW_PROBE_BYTES,
            },
            ControlMessage::BwProbeData {
                probe_id: 9,
                chunk_idx: 3,
                padding: vec![],
            },
            ControlMessage::InputAck {
                key_batch_seq: u64::MAX,
                inject_done_us: 123_456_789,
            },
            ControlMessage::ReconnectRequest {
                session_id: 99,
                resume: true,
            },
            ControlMessage::Bye {
                reason: ByeReason::Refresh,
            },
        ]
    }

    /// 合法 Hello 的 29 字节线编码。
    fn hello_wire() -> Vec<u8> {
        let mut buf = Vec::new();
        ControlMessage::Hello(sample_hello()).encode(&mut buf);
        assert_eq!(buf.len(), 29);
        buf
    }

    /// 指定 host_flags 的合法 Welcome 线编码。
    fn welcome_wire_with_flags(flags: u8) -> Vec<u8> {
        let mut w = sample_welcome();
        w.host_flags = HostFlags(flags);
        let mut buf = Vec::new();
        ControlMessage::Welcome(w).encode(&mut buf);
        buf
    }

    /// 12 个变体（含结构体变体与带非空 padding 的 BwProbeData）逐一
    /// encode→decode→assert_eq 相等。
    #[test]
    fn roundtrip_all_variants() {
        let mut msgs = all_samples();
        // roundtrip 覆盖非空 padding（截断/尾巴测试才要求空 padding）。
        msgs.push(ControlMessage::BwProbeData {
            probe_id: 9,
            chunk_idx: 3,
            padding: vec![0x01, 0x09, 0x00, 0xFF],
        });
        assert_eq!(msgs.len(), 13, "12 个变体 + 非空 padding 的 BwProbeData");
        for m in &msgs {
            let mut buf = Vec::new();
            m.encode(&mut buf);
            let back =
                ControlMessage::decode(&buf).unwrap_or_else(|e| panic!("{m:?} 必须可往返: {e}"));
            assert_eq!(&back, m);
        }
    }

    /// 代表性消息逐字节断言，锁死线格式总表：Hello（29 字节，LE、
    /// 字段顺序、auth_len=0）、Welcome（27 字节）、IdrRequest
    /// （`[0x03, 0x01]`）、ProbePong（21 字节）。
    #[test]
    fn wire_layout_is_locked_to_spec() {
        // Hello：type version session_id LE w h fps bps auth_method auth_len
        let mut buf = Vec::new();
        ControlMessage::Hello(sample_hello()).encode(&mut buf);
        assert_eq!(
            buf,
            vec![
                0x01, 0x01, // type + version=1
                0x78, 0x56, 0x34, 0x12, 0xEF, 0xBE, 0xAD, 0xDE, // session_id LE
                0x80, 0x07, 0x00, 0x00, // 1920
                0x38, 0x04, 0x00, 0x00, // 1080
                0x3C, 0x00, 0x00, 0x00, // 60
                0x00, 0x12, 0x7A, 0x00, // 8_000_000
                0x00, // auth_method=None
                0x00, 0x00, // auth_len=0
            ],
            "Hello 线格式与总表不符"
        );
        assert_eq!(buf.len(), 29);

        // Welcome：type session_id granted(4×u32) auth_status host_flags
        let mut buf = Vec::new();
        ControlMessage::Welcome(sample_welcome()).encode(&mut buf);
        assert_eq!(
            buf,
            vec![
                0x02, 0x78, 0x56, 0x34, 0x12, 0xEF, 0xBE, 0xAD, 0xDE, 0x80, 0x07, 0x00, 0x00, 0x38,
                0x04, 0x00, 0x00, 0x3C, 0x00, 0x00, 0x00, 0x00, 0x12, 0x7A, 0x00,
                0x01, // auth_status=ChallengeRequired
                0x03, // host_flags=RESUME_ACCEPTED|CLOCK_EPOCH_RESET
            ],
            "Welcome 线格式与总表不符"
        );
        assert_eq!(buf.len(), 27);

        // IdrRequest：恰好 [type, reason]
        let mut buf = Vec::new();
        ControlMessage::IdrRequest {
            reason: IdrReason::WatchdogStall,
        }
        .encode(&mut buf);
        assert_eq!(buf, vec![0x03, 0x01]);

        // ProbePong：type + 4 + 8 + 8 = 21 字节
        let mut buf = Vec::new();
        ControlMessage::ProbePong {
            probe_id: 0x1122_3344,
            echoed_clock_us: 0x0123_4567_89AB_CDEF,
            responder_clock_us: u64::MAX,
        }
        .encode(&mut buf);
        assert_eq!(
            buf,
            vec![
                0x06, 0x44, 0x33, 0x22, 0x11, // probe_id LE
                0xEF, 0xCD, 0xAB, 0x89, 0x67, 0x45, 0x23, 0x01, // echoed LE
                0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, // responder LE
            ],
            "ProbePong 线格式与总表不符"
        );
        assert_eq!(buf.len(), 21);

        // 全部 12 个变体的总长度逐一对照线格式总表（顺序同 all_samples）。
        let expects: Vec<(&str, usize)> = vec![
            ("Hello", 29),
            ("Welcome", 27),
            ("IdrRequest", 2),
            ("IdrAck", 9),
            ("ProbePing", 13),
            ("ProbePong", 21),
            ("SetBitrate", 6),
            ("BwProbeRequest", 9),
            ("BwProbeData(空 padding)", 9),
            ("InputAck", 17),
            ("ReconnectRequest", 10),
            ("Bye", 2),
        ];
        for (m, (_, want)) in all_samples().into_iter().zip(&expects) {
            let mut buf = Vec::new();
            m.encode(&mut buf);
            assert_eq!(buf.len(), *want, "{m:?} 线长度与总表不符");
        }
    }

    /// 空缓冲与未知类型字节（0x00/0x10/0x99）一律 Protocol 拒收，
    /// 错误文本含出错字节值。
    #[test]
    fn decode_rejects_empty_and_unknown_type() {
        let err = ControlMessage::decode(&[]).unwrap_err();
        assert!(matches!(err, Error::Protocol { .. }));
        assert!(err.to_string().contains("empty"), "{err}");

        for bad in [0x00u8, 0x10, 0x99] {
            let err = ControlMessage::decode(&[bad]).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains(&format!("0x{bad:02X}")),
                "错误文本应含出错字节值 0x{bad:02X}: {msg}"
            );
            assert!(matches!(err, Error::Protocol { .. }));
        }
    }

    /// 预留号 0x0D/0x0E/0x0F 拒收：错误文本含 "reserved" 与对应
    /// M2-5（T19 鉴权）/ M2-8（T20 剪贴板）归属。
    #[test]
    fn decode_rejects_reserved_types() {
        for (ty, owner, milestone) in [
            (TYPE_AUTH_CHALLENGE, "T19", "M2-5"),
            (TYPE_AUTH_RESPONSE, "T19", "M2-5"),
            (TYPE_CLIPBOARD_NOTIFY, "T20", "M2-8"),
        ] {
            let err = ControlMessage::decode(&[ty]).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("reserved"), "应注明 reserved: {msg}");
            assert!(msg.contains(owner), "应注明归属卡 {owner}: {msg}");
            assert!(msg.contains(milestone), "应注明里程碑 {milestone}: {msg}");
            assert!(matches!(err, Error::Protocol { .. }));
        }
    }

    /// 对每个变体的合法编码，取每个严格前缀（0..len−1 字节）解码
    /// 全部报错（消息最长 29 字节，穷举代价可忽略）。
    #[test]
    fn decode_rejects_truncation_at_every_offset() {
        for m in all_samples() {
            let mut buf = Vec::new();
            m.encode(&mut buf);
            for cut in 0..buf.len() {
                assert!(
                    ControlMessage::decode(&buf[..cut]).is_err(),
                    "{m:?} 截断到 {cut} 字节必须报错"
                );
            }
        }
    }

    /// 合法编码 + 1 字节尾巴报错（含 trailing 字样与 offset）。
    /// BwProbeData 是设计例外：padding 无前缀、边界由数据报给出，
    /// 追加字节语义上就是 padding 增长，必须无损接受。
    #[test]
    fn decode_rejects_trailing_bytes() {
        for m in all_samples() {
            let mut buf = Vec::new();
            m.encode(&mut buf);
            let expected_len = buf.len();
            buf.push(0xAA);
            if matches!(m, ControlMessage::BwProbeData { .. }) {
                let back = ControlMessage::decode(&buf)
                    .unwrap_or_else(|e| panic!("BwProbeData 尾巴应并入 padding: {e}"));
                assert_eq!(
                    back,
                    ControlMessage::BwProbeData {
                        probe_id: 9,
                        chunk_idx: 3,
                        padding: vec![0xAA],
                    }
                );
                continue;
            }
            let err = ControlMessage::decode(&buf).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("trailing"), "{m:?} 应报 trailing: {msg}");
            match &err {
                Error::Protocol { offset, .. } => {
                    assert_eq!(*offset, expected_len, "offset 应指向期望消息末尾: {msg}")
                }
                _ => panic!("{m:?} 应报 Protocol: {err}"),
            }
        }
    }

    /// version=0/2/255 的 Hello 拒收且错误文本含收到的版本号与
    /// PROTOCOL_VERSION；encode 出的 Hello 恒 version=1 可解。
    #[test]
    fn hello_version_mismatch_rejected_with_value() {
        for v in [0u8, 2, 255] {
            let mut wire = hello_wire();
            wire[1] = v;
            let err = ControlMessage::decode(&wire).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains(&format!("version {v}")),
                "错误文本应含收到的版本号 {v}: {msg}"
            );
            assert!(
                msg.contains(&format!("PROTOCOL_VERSION {PROTOCOL_VERSION}")),
                "错误文本应含本地版本号: {msg}"
            );
        }
        // encode 原样写出合法构造的 version，decode 必可解回。
        let mut buf = Vec::new();
        ControlMessage::Hello(sample_hello()).encode(&mut buf);
        assert_eq!(buf[1], PROTOCOL_VERSION);
        assert_eq!(
            ControlMessage::decode(&buf).unwrap(),
            ControlMessage::Hello(sample_hello())
        );
    }

    /// auth_len 与实际字节数不符：声称 3 实给 0 → truncated；
    /// 声称 0 多给 3 → trailing bytes。
    #[test]
    fn auth_len_must_equal_payload_len() {
        // auth_len=3（offset 27..29 LE）而载荷 0 字节
        let mut wire = hello_wire();
        wire[27] = 3;
        wire[28] = 0;
        let err = ControlMessage::decode(&wire).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("truncated"), "应报截断: {msg}");
        assert!(msg.contains("auth_payload"), "应指明字段: {msg}");
        assert!(matches!(err, Error::Protocol { .. }));

        // auth_len=0 而带 3 字节尾巴
        let mut wire = hello_wire();
        wire.extend_from_slice(&[1, 2, 3]);
        let err = ControlMessage::decode(&wire).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("trailing"), "应报尾巴: {msg}");
        assert!(matches!(err, Error::Protocol { .. }));
    }

    /// 枚举 / 布尔字段的严格值域：auth_method=2、auth_status=3、
    /// IdrReason=3、BitrateReason=3、ByeReason=2、resume=2 逐个手工
    /// 构包，全部拒收且文本含出错值。
    #[test]
    fn enum_fields_strict_value_domains() {
        // auth_method=2（合法 Welcome 无关，Hello 的 offset 26）
        let mut wire = hello_wire();
        wire[26] = 2;
        let err = ControlMessage::decode(&wire).unwrap_err();
        assert!(err.to_string().contains("got 2"), "{err}");

        // auth_status=3（Welcome 的 offset 25）
        let mut wire = Vec::new();
        ControlMessage::Welcome(sample_welcome()).encode(&mut wire);
        assert_eq!(wire.len(), 27);
        wire[25] = 3;
        let err = ControlMessage::decode(&wire).unwrap_err();
        assert!(err.to_string().contains("got 3"), "{err}");

        // IdrReason=3
        let err = ControlMessage::decode(&[TYPE_IDR_REQUEST, 3]).unwrap_err();
        assert!(err.to_string().contains("got 3"), "{err}");

        // BitrateReason=3
        let mut wire = vec![TYPE_SET_BITRATE];
        wire.extend_from_slice(&0u32.to_le_bytes());
        wire.push(3);
        let err = ControlMessage::decode(&wire).unwrap_err();
        assert!(err.to_string().contains("got 3"), "{err}");

        // ByeReason=2（2 对 AuthStatus 合法，对 Bye 越界）
        let err = ControlMessage::decode(&[TYPE_BYE, 2]).unwrap_err();
        assert!(err.to_string().contains("got 2"), "{err}");

        // resume=2（严格 0/1）
        let mut wire = vec![TYPE_RECONNECT_REQUEST];
        wire.extend_from_slice(&7u64.to_le_bytes());
        wire.push(2);
        let err = ControlMessage::decode(&wire).unwrap_err();
        assert!(err.to_string().contains("must be 0 or 1"), "{err}");
    }

    /// host_flags：0x00..=0x03（已定义位全集）可解且访问器正确；
    /// 0x04/0x80/0xFF（未知 bit）拒收。
    #[test]
    fn host_flags_reject_unknown_bits() {
        for flags in [0x00u8, 0x01, 0x02, 0x03] {
            let wire = welcome_wire_with_flags(flags);
            let decoded = ControlMessage::decode(&wire)
                .unwrap_or_else(|e| panic!("host_flags=0x{flags:02X} 应可解: {e}"));
            let ControlMessage::Welcome(w) = decoded else {
                panic!("应解出 Welcome")
            };
            assert_eq!(w.host_flags, HostFlags(flags));
            assert_eq!(
                w.host_flags.resume_accepted(),
                flags & HostFlags::RESUME_ACCEPTED != 0,
                "resume_accepted(0x{flags:02X})"
            );
            assert_eq!(
                w.host_flags.clock_epoch_reset(),
                flags & HostFlags::CLOCK_EPOCH_RESET != 0,
                "clock_epoch_reset(0x{flags:02X})"
            );
        }
        for flags in [0x04u8, 0x80, 0xFF] {
            let wire = welcome_wire_with_flags(flags);
            let err = ControlMessage::decode(&wire).expect_err("未知 bit 应拒收");
            let msg = err.to_string();
            assert!(msg.contains("unknown bits"), "应报未知 bit: {msg}");
            assert!(msg.contains(&format!("0x{flags:02X}")), "应含出错值: {msg}");
        }
    }

    /// total_bytes = MAX_BW_PROBE_BYTES 恰好可解；+1 拒收且文本含上限值。
    #[test]
    fn bw_probe_request_caps_total_bytes() {
        // 恰好上限：roundtrip 可解
        let m = ControlMessage::BwProbeRequest {
            probe_id: 1,
            total_bytes: MAX_BW_PROBE_BYTES,
        };
        let mut buf = Vec::new();
        m.encode(&mut buf);
        assert_eq!(ControlMessage::decode(&buf).unwrap(), m);

        // +1 拒收
        let mut wire = vec![TYPE_BW_PROBE_REQUEST];
        wire.extend_from_slice(&1u32.to_le_bytes());
        wire.extend_from_slice(&(MAX_BW_PROBE_BYTES + 1).to_le_bytes());
        let err = ControlMessage::decode(&wire).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(&format!("exceeds MAX_BW_PROBE_BYTES {MAX_BW_PROBE_BYTES}")),
            "应含上限值: {msg}"
        );
        assert!(matches!(err, Error::Protocol { .. }));
    }

    /// padding 为不透明字节：空、0x00/0xFF、与类型号同款的二进制垃圾
    /// 均无损往返（剩余字节语义不被解释）。
    #[test]
    fn bw_probe_data_padding_is_opaque() {
        let paddings: Vec<Vec<u8>> = vec![
            vec![],
            vec![0x00],
            vec![0xFF],
            vec![TYPE_HELLO, TYPE_BYE, TYPE_AUTH_CHALLENGE, 0x00, 0xFF],
            (0..=255u8).collect(),
        ];
        for padding in paddings {
            let m = ControlMessage::BwProbeData {
                probe_id: 5,
                chunk_idx: 2,
                padding: padding.clone(),
            };
            let mut buf = Vec::new();
            m.encode(&mut buf);
            assert_eq!(
                ControlMessage::decode(&buf).unwrap_or_else(|e| panic!("padding 必须可解: {e}")),
                m,
                "padding={padding:02X?} 应逐字节还原"
            );
        }
    }
}
