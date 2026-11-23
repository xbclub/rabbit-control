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

// ── 类型号（0x01..=0x12 全部实装）──

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
/// 线格式类型字节：`AuthChallenge`（host→client 密码挑战，M2-5 / T19）。
pub const TYPE_AUTH_CHALLENGE: u8 = 0x0D;
/// 线格式类型字节：`AuthResponse`（client→host 挑战应答，M2-5 / T19）。
pub const TYPE_AUTH_RESPONSE: u8 = 0x0E;
/// 线格式类型字节：`ClipboardNotify`（双向，剪贴板文本通知，M2-8 / T20）。
pub const TYPE_CLIPBOARD_NOTIFY: u8 = 0x0F;
/// 线格式类型字节：`ToolChannel`（双向，工具复用通道：文件/终端/端口映射/音频控制）。
pub const TYPE_TOOL_CHANNEL: u8 = 0x10;
/// 线格式类型字节：`AudioFrame`（双向，音频帧传输通道）。
pub const TYPE_AUDIO_FRAME: u8 = 0x11;
/// 线格式类型字节：`OwnerAuthResponse`（client→host 自控免密挑战签名应答）。
pub const TYPE_OWNER_AUTH_RESPONSE: u8 = 0x12;

/// 工具复用子通道号：文件传输（W2）。
pub const TOOL_CHANNEL_FILE: u8 = 1;
/// 工具复用子通道号：PTY 终端（W3）。
pub const TOOL_CHANNEL_TERMINAL: u8 = 2;
/// 工具复用子通道号：端口转发（W4）。
pub const TOOL_CHANNEL_PORTMAP: u8 = 3;
/// 工具复用子通道号：音频控制。
pub const TOOL_CHANNEL_AUDIO_CTRL: u8 = 4;
/// 工具复用子通道号：显示控制（分辨率与帧率热切换）。
pub const TOOL_CHANNEL_DISPLAY_CTRL: u8 = 5;

/// 工具复用单条载荷最大字节数（256 KiB）。
pub const MAX_TOOL_PAYLOAD_BYTES: usize = 256 * 1024;

/// 音频编解码标识：16 位有符号小端 PCM。
pub const AUDIO_CODEC_PCM16: u8 = 0;
/// 音频编解码标识：IMA-ADPCM。
pub const AUDIO_CODEC_IMA_ADPCM: u8 = 1;

/// 单帧音频数据最大字节数（32 KiB）。
pub const MAX_AUDIO_FRAME_BYTES: usize = 32 * 1024;

/// 音频采样率下界（Hz，8 kHz）。
pub const MIN_AUDIO_SAMPLE_RATE: u32 = 8_000;
/// 音频采样率上界（Hz，192 kHz）。
pub const MAX_AUDIO_SAMPLE_RATE: u32 = 192_000;

/// 剪贴板文本的最大**字节数**（不是字符数）。与变长段 u16 长度前缀同宽，
/// 线格式上限与引擎上限共用这一个常量（单一事实源）。
/// 要提升上限必须走 `PROTOCOL_VERSION` 版本协商，不得私自改 u32 前缀。
pub const MAX_CLIPBOARD_TEXT_BYTES: usize = u16::MAX as usize;

/// [`ControlMessage::BwProbeRequest`] 的 `total_bytes` 线格式上限
/// （设计师裁定 256KB）：恶意 client 不得借探测流量打满带宽。
pub const MAX_BW_PROBE_BYTES: u32 = 256 * 1024;

/// AuthChallenge 盐长度（字节）。固定长度，线格式无长度前缀。
pub const AUTH_SALT_LEN: usize = 16;
/// AuthResponse 证明长度（字节）= PBKDF2-HMAC-SHA256 输出长度。
pub const AUTH_PROOF_LEN: usize = 32;
/// AuthChallenge.iterations 的线格式上限。decode 拒收 0 与超限值
/// （对齐 BwProbeRequest.total_bytes 的防灌边界纪律：恶意 host
/// 不得借荒谬迭代数烧 client CPU）。
pub const MAX_AUTH_ITERATIONS: u32 = 1_000_000;
/// OwnerAuthResponse 签名载荷最大字节数（1..=128）。
pub const MAX_OWNER_SIGNATURE_LEN: usize = 128;
/// Hello.auth_payload 携带自控票据载荷的合理长度上限（4 KiB）。
pub const MAX_OWNER_TICKET_PAYLOAD_LEN: usize = 4096;

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

/// Hello 的鉴权方式：0=无、1=密码（M2-5 / T19）、2=自控免密签名票据（OwnerTicket PoP）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AuthMethod {
    /// 无鉴权。
    None = 0,
    /// 密码鉴权（M2-5 / T19 实装）。
    Password = 1,
    /// 自控免密签名票据鉴权（OwnerTicket PoP）。
    OwnerTicket = 2,
}

impl AuthMethod {
    /// 从线上字节解析鉴权方式；仅接受 `0`（None）、`1`（Password）、`2`（OwnerTicket）。
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::None),
            1 => Some(Self::Password),
            2 => Some(Self::OwnerTicket),
            _ => None,
        }
    }
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
    /// 鉴权方式（None / Password / OwnerTicket）。
    pub auth_method: AuthMethod,
    /// 鉴权载荷（线上带 auth_len u16 前缀；`OwnerTicket` 时携带序列化的 `owner_grant`，上限 [`MAX_OWNER_TICKET_PAYLOAD_LEN`]）。
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

/// 0x0D AuthChallenge（host→client，M2-5 / T19）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthChallenge {
    /// PBKDF2 盐：host 每次握手随机生成、一次性使用——重放旧 proof 必失败。
    pub salt: [u8; AUTH_SALT_LEN],
    /// PBKDF2 迭代次数（decode 校验 1..=MAX_AUTH_ITERATIONS）。
    pub iterations: u32,
}

/// 0x0E AuthResponse（client→host，M2-5 / T19）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthResponse {
    /// 证明 = PBKDF2-HMAC-SHA256(password_utf8, salt, iterations)。
    pub proof: [u8; AUTH_PROOF_LEN],
}

/// 一帧音频数据包（用于会话层发送与排空接口）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFramePacket {
    /// 音频帧序号。
    pub seq: u32,
    /// 采集/播放时间戳（微秒）。
    pub timestamp_us: u32,
    /// 采样率（Hz，必须大于 0）。
    pub sample_rate: u32,
    /// 声道数（1=单声道，2=立体声）。
    pub channels: u8,
    /// 编解码标识（[`AUDIO_CODEC_PCM16`] 或 [`AUDIO_CODEC_IMA_ADPCM`]）。
    pub codec: u8,
    /// 音频帧数据，字节数 ≤ [`MAX_AUDIO_FRAME_BYTES`]。
    pub data: Vec<u8>,
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
    /// 0x0D host→client：密码挑战（M2-5 / T19）。
    AuthChallenge(AuthChallenge),
    /// 0x0E client→host：挑战应答（M2-5 / T19）。
    AuthResponse(AuthResponse),
    /// 0x0F ClipboardNotify（双向，M2-8 / T20）。
    /// 线格式：`[0x0F][len u16 LE][text]`——text 是 UTF-8 原始字节，
    /// 可含 NUL，长度前缀定界、不做转义；len 按字节计（非字符数）。
    /// 空文本合法（len=0，语义：对端剪贴板无文本可粘）。
    /// 走 reliable 控制流（Stream A），绝不走 datagram / urgent。
    ClipboardNotify {
        /// UTF-8 文本，字节数 ≤ [`MAX_CLIPBOARD_TEXT_BYTES`]。
        text: String,
    },
    /// 0x10 ToolChannel（双向，工具复用通道）。
    /// 线格式：`[0x10][channel: u8][len: u32 LE][payload: len bytes]`。
    /// `channel` 必须为 `1..=5`，`len` ≤ [`MAX_TOOL_PAYLOAD_BYTES`]。
    /// 走 reliable 控制流（Stream A）。
    ToolChannel {
        /// 子通道号（[`TOOL_CHANNEL_FILE`]..=[`TOOL_CHANNEL_DISPLAY_CTRL`]）。
        channel: u8,
        /// 子通道载荷，字节数 ≤ [`MAX_TOOL_PAYLOAD_BYTES`]。
        payload: Vec<u8>,
    },
    /// 0x11 AudioFrame（双向，音频帧通道）。
    /// 线格式：`[0x11][seq: u32 LE][timestamp_us: u32 LE][sample_rate: u32 LE][channels: u8][codec: u8][len: u16 LE][data: len bytes]`。
    /// 走 reliable 控制流（Stream A）。
    AudioFrame {
        /// 音频帧序号。
        seq: u32,
        /// 采集/播放时间戳（微秒）。
        timestamp_us: u32,
        /// 采样率（Hz，必须大于 0）。
        sample_rate: u32,
        /// 声道数（1 或 2）。
        channels: u8,
        /// 编解码标识（[`AUDIO_CODEC_PCM16`] 或 [`AUDIO_CODEC_IMA_ADPCM`]）。
        codec: u8,
        /// 音频帧数据，字节数 ≤ [`MAX_AUDIO_FRAME_BYTES`]。
        data: Vec<u8>,
    },
    /// 0x12 OwnerAuthResponse（client→host，自控免密挑战签名应答）。
    /// 线格式：`[0x12][len: u16 LE (1..=128)][signature: len bytes]`。
    /// 走 reliable 控制流（Stream A）。
    OwnerAuthResponse {
        /// 对挑战 `(session_id, salt)` 的 PoP 签名（长度在 `1..=MAX_OWNER_SIGNATURE_LEN`）。
        signature: Vec<u8>,
    },
}

impl ControlMessage {
    /// 构造一条剪贴板通知（唯一合法构造路径）。
    /// `text` 字节数 > [`MAX_CLIPBOARD_TEXT_BYTES`] → `Err(InvalidConfig)`：
    /// 发送侧「宁不传不截断」在构造点就拦住，`encode` 因此可以保持
    /// T13 契约「不失败、不 panic」（越界值属构造契约违反，
    /// `debug_assert` 钉开发期；release 下长度字段饱和 `u16::MAX` 仅为
    /// 防御性处理，按契约不可达）。
    pub fn clipboard_notify(text: String) -> Result<Self> {
        if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
            return Err(Error::InvalidConfig(format!(
                "clipboard text {} bytes exceeds MAX_CLIPBOARD_TEXT_BYTES {MAX_CLIPBOARD_TEXT_BYTES}",
                text.len()
            )));
        }
        Ok(ControlMessage::ClipboardNotify { text })
    }

    /// 校验并构造一条 `ToolChannel` 消息（发送侧入口：`channel` 须在 `1..=5`、
    /// `payload.len()` ≤ [`MAX_TOOL_PAYLOAD_BYTES`]，否则返回 `Error::InvalidConfig`）。
    pub fn tool_channel(channel: u8, payload: Vec<u8>) -> Result<Self> {
        validate_tool_message(channel, payload.len())?;
        Ok(ControlMessage::ToolChannel { channel, payload })
    }

    /// 校验并构造一条 `AudioFrame` 消息（发送侧入口：`sample_rate` 须在
    /// `MIN_AUDIO_SAMPLE_RATE..=MAX_AUDIO_SAMPLE_RATE`、`channels in 1..=2`、
    /// `codec <= AUDIO_CODEC_IMA_ADPCM`、`data.len() <= MAX_AUDIO_FRAME_BYTES`，
    /// 且 `AUDIO_CODEC_PCM16` 时 `data.len()` 须按 `2 * channels` 整帧对齐，
    /// 否则返回 `Error::InvalidConfig`）。
    pub fn audio_frame(packet: AudioFramePacket) -> Result<Self> {
        validate_audio_frame(&packet)?;
        Ok(ControlMessage::AudioFrame {
            seq: packet.seq,
            timestamp_us: packet.timestamp_us,
            sample_rate: packet.sample_rate,
            channels: packet.channels,
            codec: packet.codec,
            data: packet.data,
        })
    }
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
                // auth_payload 长度前缀（u16 LE）。若调用方塞入超过上限的载荷，
                // 饱和为 u16::MAX——对端 decode 时长度前缀或上限校验会显式拒绝。
                let len = u16::try_from(h.auth_payload.len()).unwrap_or(u16::MAX);
                debug_assert!(
                    h.auth_payload.len() <= MAX_OWNER_TICKET_PAYLOAD_LEN,
                    "auth_payload {} 字节超过 MAX_OWNER_TICKET_PAYLOAD_LEN ({MAX_OWNER_TICKET_PAYLOAD_LEN})",
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
            ControlMessage::AuthChallenge(ch) => {
                out.push(TYPE_AUTH_CHALLENGE);
                out.extend_from_slice(&ch.salt);
                out.extend_from_slice(&ch.iterations.to_le_bytes());
            }
            ControlMessage::AuthResponse(resp) => {
                out.push(TYPE_AUTH_RESPONSE);
                out.extend_from_slice(&resp.proof);
            }
            ControlMessage::ClipboardNotify { text } => {
                out.push(TYPE_CLIPBOARD_NOTIFY);
                let len = u16::try_from(text.len()).unwrap_or(u16::MAX);
                debug_assert!(
                    text.len() <= MAX_CLIPBOARD_TEXT_BYTES,
                    "clipboard text {} 字节超过 MAX_CLIPBOARD_TEXT_BYTES ({MAX_CLIPBOARD_TEXT_BYTES})，应经 ControlMessage::clipboard_notify 构造",
                    text.len()
                );
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(text.as_bytes());
            }
            ControlMessage::ToolChannel { channel, payload } => {
                out.push(TYPE_TOOL_CHANNEL);
                out.push(*channel);
                let len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
                debug_assert!(
                    payload.len() <= MAX_TOOL_PAYLOAD_BYTES,
                    "tool_channel payload {} bytes exceeds MAX_TOOL_PAYLOAD_BYTES ({MAX_TOOL_PAYLOAD_BYTES})",
                    payload.len()
                );
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(payload);
            }
            ControlMessage::AudioFrame {
                seq,
                timestamp_us,
                sample_rate,
                channels,
                codec,
                data,
            } => {
                out.push(TYPE_AUDIO_FRAME);
                out.extend_from_slice(&seq.to_le_bytes());
                out.extend_from_slice(&timestamp_us.to_le_bytes());
                out.extend_from_slice(&sample_rate.to_le_bytes());
                out.push(*channels);
                out.push(*codec);
                let len = u16::try_from(data.len()).unwrap_or(u16::MAX);
                debug_assert!(
                    data.len() <= MAX_AUDIO_FRAME_BYTES,
                    "audio_frame data {} bytes exceeds MAX_AUDIO_FRAME_BYTES ({MAX_AUDIO_FRAME_BYTES})",
                    data.len()
                );
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(data);
            }
            ControlMessage::OwnerAuthResponse { signature } => {
                out.push(TYPE_OWNER_AUTH_RESPONSE);
                let len = u16::try_from(signature.len()).unwrap_or(u16::MAX);
                debug_assert!(
                    (1..=MAX_OWNER_SIGNATURE_LEN).contains(&signature.len()),
                    "owner_auth_response signature {} bytes must be in 1..={MAX_OWNER_SIGNATURE_LEN}",
                    signature.len()
                );
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(signature);
            }
        }
    }

    /// 解码一条完整消息（[`encode`](ControlMessage::encode) 的逆）。
    ///
    /// 空缓冲、未知类型字节、version 不匹配、枚举字段越界、布尔字段非 0/1、
    /// host_flags 未知 bit、auth_len 与实际字节数不符、iterations 越界、
    /// 剪贴板文本非合法 UTF-8、载荷截断、尾部多余字节——一律返回
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
            TYPE_AUTH_CHALLENGE => {
                let salt_slice = r.take(AUTH_SALT_LEN, "auth_challenge.salt")?;
                let mut salt = [0u8; AUTH_SALT_LEN];
                salt.copy_from_slice(salt_slice);
                let iters_off = r.off;
                let iterations = r.u32("auth_challenge.iterations")?;
                if iterations == 0 {
                    return Err(Error::Protocol {
                        offset: iters_off,
                        msg: format!(
                            "auth_challenge.iterations must be in 1..={MAX_AUTH_ITERATIONS}, got 0"
                        ),
                    });
                }
                if iterations > MAX_AUTH_ITERATIONS {
                    return Err(Error::Protocol {
                        offset: iters_off,
                        msg: format!(
                            "auth_challenge.iterations {iterations} exceeds \
                             MAX_AUTH_ITERATIONS {MAX_AUTH_ITERATIONS}"
                        ),
                    });
                }
                r.done("AuthChallenge")?;
                ControlMessage::AuthChallenge(AuthChallenge { salt, iterations })
            }
            TYPE_AUTH_RESPONSE => {
                let proof_slice = r.take(AUTH_PROOF_LEN, "auth_response.proof")?;
                let mut proof = [0u8; AUTH_PROOF_LEN];
                proof.copy_from_slice(proof_slice);
                r.done("AuthResponse")?;
                ControlMessage::AuthResponse(AuthResponse { proof })
            }
            TYPE_CLIPBOARD_NOTIFY => {
                let len = usize::from(r.u16("clipboard_notify.len")?);
                let text_off = r.off;
                let raw = r.take(len, "clipboard_notify.text")?;
                let text = std::str::from_utf8(raw)
                    .map_err(|e| Error::Protocol {
                        offset: text_off + e.valid_up_to(),
                        msg: format!("clipboard_notify.text is not valid UTF-8: {e}"),
                    })?
                    .to_owned();
                r.done("ClipboardNotify")?;
                ControlMessage::ClipboardNotify { text }
            }
            TYPE_TOOL_CHANNEL => {
                let ch_off = r.off;
                let channel = r.u8("tool_channel.channel")?;
                if channel == 0 || channel > TOOL_CHANNEL_DISPLAY_CTRL {
                    return Err(Error::Protocol {
                        offset: ch_off,
                        msg: format!(
                            "tool_channel.channel must be in {TOOL_CHANNEL_FILE}..={TOOL_CHANNEL_DISPLAY_CTRL}, got {channel}"
                        ),
                    });
                }
                let len_off = r.off;
                let len_u32 = r.u32("tool_channel.len")?;
                let len = usize::try_from(len_u32).unwrap_or(usize::MAX);
                if len > MAX_TOOL_PAYLOAD_BYTES {
                    return Err(Error::Protocol {
                        offset: len_off,
                        msg: format!(
                            "tool_channel.len {len_u32} exceeds MAX_TOOL_PAYLOAD_BYTES {MAX_TOOL_PAYLOAD_BYTES}"
                        ),
                    });
                }
                let payload = r.take(len, "tool_channel.payload")?.to_vec();
                r.done("ToolChannel")?;
                ControlMessage::ToolChannel { channel, payload }
            }
            TYPE_AUDIO_FRAME => {
                let seq = r.u32("audio_frame.seq")?;
                let timestamp_us = r.u32("audio_frame.timestamp_us")?;
                let sr_off = r.off;
                let sample_rate = r.u32("audio_frame.sample_rate")?;
                if !(MIN_AUDIO_SAMPLE_RATE..=MAX_AUDIO_SAMPLE_RATE).contains(&sample_rate) {
                    return Err(Error::Protocol {
                        offset: sr_off,
                        msg: format!(
                            "audio_frame.sample_rate must be in {MIN_AUDIO_SAMPLE_RATE}..={MAX_AUDIO_SAMPLE_RATE}, got {sample_rate}"
                        ),
                    });
                }
                let ch_off = r.off;
                let channels = r.u8("audio_frame.channels")?;
                if channels == 0 || channels > 2 {
                    return Err(Error::Protocol {
                        offset: ch_off,
                        msg: format!("audio_frame.channels must be 1 or 2, got {channels}"),
                    });
                }
                let codec_off = r.off;
                let codec = r.u8("audio_frame.codec")?;
                if codec > AUDIO_CODEC_IMA_ADPCM {
                    return Err(Error::Protocol {
                        offset: codec_off,
                        msg: format!(
                            "audio_frame.codec must be {AUDIO_CODEC_PCM16}(PCM16) or {AUDIO_CODEC_IMA_ADPCM}(IMA_ADPCM), got {codec}"
                        ),
                    });
                }
                let len_off = r.off;
                let len = usize::from(r.u16("audio_frame.len")?);
                if len > MAX_AUDIO_FRAME_BYTES {
                    return Err(Error::Protocol {
                        offset: len_off,
                        msg: format!(
                            "audio_frame.len {len} exceeds MAX_AUDIO_FRAME_BYTES {MAX_AUDIO_FRAME_BYTES}"
                        ),
                    });
                }
                if codec == AUDIO_CODEC_PCM16 {
                    let frame_align = 2 * usize::from(channels);
                    if len % frame_align != 0 {
                        return Err(Error::Protocol {
                            offset: len_off,
                            msg: format!(
                                "audio_frame.len {len} for PCM16 ({channels}ch) must be a multiple of {frame_align}"
                            ),
                        });
                    }
                }
                let data = r.take(len, "audio_frame.data")?.to_vec();
                r.done("AudioFrame")?;
                ControlMessage::AudioFrame {
                    seq,
                    timestamp_us,
                    sample_rate,
                    channels,
                    codec,
                    data,
                }
            }
            TYPE_OWNER_AUTH_RESPONSE => {
                let len_off = r.off;
                let len = usize::from(r.u16("owner_auth_response.len")?);
                if len == 0 || len > MAX_OWNER_SIGNATURE_LEN {
                    return Err(Error::Protocol {
                        offset: len_off,
                        msg: format!(
                            "owner_auth_response.len must be in 1..={MAX_OWNER_SIGNATURE_LEN}, got {len}"
                        ),
                    });
                }
                let signature = r.take(len, "owner_auth_response.signature")?.to_vec();
                r.done("OwnerAuthResponse")?;
                ControlMessage::OwnerAuthResponse { signature }
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

/// 发送侧校验 `ToolChannel` 参数。
pub(crate) fn validate_tool_message(channel: u8, payload_len: usize) -> Result<()> {
    if channel == 0 || channel > TOOL_CHANNEL_DISPLAY_CTRL {
        return Err(Error::InvalidConfig(format!(
            "tool_channel.channel must be in {TOOL_CHANNEL_FILE}..={TOOL_CHANNEL_DISPLAY_CTRL}, got {channel}"
        )));
    }
    if payload_len > MAX_TOOL_PAYLOAD_BYTES {
        return Err(Error::InvalidConfig(format!(
            "tool_channel payload {payload_len} bytes exceeds MAX_TOOL_PAYLOAD_BYTES {MAX_TOOL_PAYLOAD_BYTES}"
        )));
    }
    Ok(())
}

/// 发送侧校验 `AudioFramePacket` 参数。
pub(crate) fn validate_audio_frame(packet: &AudioFramePacket) -> Result<()> {
    if !(MIN_AUDIO_SAMPLE_RATE..=MAX_AUDIO_SAMPLE_RATE).contains(&packet.sample_rate) {
        return Err(Error::InvalidConfig(format!(
            "audio_frame.sample_rate must be in {MIN_AUDIO_SAMPLE_RATE}..={MAX_AUDIO_SAMPLE_RATE}, got {}",
            packet.sample_rate
        )));
    }
    if packet.channels == 0 || packet.channels > 2 {
        return Err(Error::InvalidConfig(format!(
            "audio_frame.channels must be 1 or 2, got {}",
            packet.channels
        )));
    }
    if packet.codec > AUDIO_CODEC_IMA_ADPCM {
        return Err(Error::InvalidConfig(format!(
            "audio_frame.codec must be {AUDIO_CODEC_PCM16}(PCM16) or {AUDIO_CODEC_IMA_ADPCM}(IMA_ADPCM), got {}",
            packet.codec
        )));
    }
    if packet.data.len() > MAX_AUDIO_FRAME_BYTES {
        return Err(Error::InvalidConfig(format!(
            "audio_frame data {} bytes exceeds MAX_AUDIO_FRAME_BYTES {MAX_AUDIO_FRAME_BYTES}",
            packet.data.len()
        )));
    }
    if packet.codec == AUDIO_CODEC_PCM16 {
        let frame_align = 2 * usize::from(packet.channels);
        if packet.data.len() % frame_align != 0 {
            return Err(Error::InvalidConfig(format!(
                "audio_frame PCM16 data length {} for {}ch must be a multiple of {frame_align}",
                packet.data.len(),
                packet.channels
            )));
        }
    }
    Ok(())
}

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
    let raw_method = r.u8("hello.auth_method")?;
    let auth_method = AuthMethod::from_u8(raw_method).ok_or_else(|| Error::Protocol {
        offset: method_off,
        msg: format!(
            "hello.auth_method must be 0(None), 1(Password) or 2(OwnerTicket), got {raw_method}"
        ),
    })?;
    let auth_len_off = r.off;
    let auth_len = usize::from(r.u16("hello.auth_len")?);
    if auth_len > MAX_OWNER_TICKET_PAYLOAD_LEN {
        return Err(Error::Protocol {
            offset: auth_len_off,
            msg: format!(
                "hello.auth_len {auth_len} exceeds MAX_OWNER_TICKET_PAYLOAD_LEN \
                 {MAX_OWNER_TICKET_PAYLOAD_LEN}"
            ),
        });
    }
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

    /// 15 个变体各一条合法样例。截断 / 尾巴测试用：Hello 的 auth_payload
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
            ControlMessage::AuthChallenge(AuthChallenge {
                salt: [
                    0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D,
                    0x0E, 0x0F, 0x10,
                ],
                iterations: 10_000,
            }),
            ControlMessage::AuthResponse(AuthResponse { proof: [0xAB; 32] }),
            ControlMessage::ClipboardNotify {
                text: String::new(),
            },
            ControlMessage::ToolChannel {
                channel: TOOL_CHANNEL_FILE,
                payload: vec![],
            },
            ControlMessage::AudioFrame {
                seq: 1,
                timestamp_us: 20_000,
                sample_rate: 48_000,
                channels: 2,
                codec: AUDIO_CODEC_PCM16,
                data: vec![],
            },
            ControlMessage::OwnerAuthResponse {
                signature: vec![0xAB],
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

    /// 18 个变体（含结构体变体与带非空 padding 的 BwProbeData）逐一
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
        assert_eq!(msgs.len(), 19, "18 个变体 + 非空 padding 的 BwProbeData");
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

        // 全部 18 个变体的总长度逐一对照线格式总表（顺序同 all_samples）。
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
            ("AuthChallenge", 21),
            ("AuthResponse", 33),
            ("ClipboardNotify(空文本)", 3),
            ("ToolChannel(空载荷)", 6),
            ("AudioFrame(空数据)", 17),
            ("OwnerAuthResponse(1B 签名)", 4),
        ];
        let samples = all_samples();
        assert_eq!(samples.len(), expects.len());
        for (m, (_, want)) in samples.into_iter().zip(&expects) {
            let mut buf = Vec::new();
            m.encode(&mut buf);
            assert_eq!(buf.len(), *want, "{m:?} 线长度与总表不符");
        }
    }

    /// 空缓冲与未知类型字节（0x00/0x13/0x99）一律 Protocol 拒收，
    /// 错误文本含出错字节值。
    #[test]
    fn decode_rejects_empty_and_unknown_type() {
        let err = ControlMessage::decode(&[]).unwrap_err();
        assert!(matches!(err, Error::Protocol { .. }));
        assert!(err.to_string().contains("empty"), "{err}");

        for bad in [0x00u8, 0x13, 0x99] {
            let err = ControlMessage::decode(&[bad]).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains(&format!("0x{bad:02X}")),
                "错误文本应含出错字节值 0x{bad:02X}: {msg}"
            );
            assert!(matches!(err, Error::Protocol { .. }));
        }
    }

    /// 0x0D/0x0E（T19）、0x0F（T20）与 0x10/0x11/0x12 均已实装，0x01..=0x12 已无预留未实装号；
    /// 未知类型号（0x13/0x99）照旧以 Protocol 拒收且错误文本含出错字节值。
    #[test]
    fn decode_rejects_reserved_types() {
        // 0x0F/0x10/0x11/0x12 已实装，仅给 1 字节类型号应报截断而非 unknown。
        for ty in [
            TYPE_CLIPBOARD_NOTIFY,
            TYPE_TOOL_CHANNEL,
            TYPE_AUDIO_FRAME,
            TYPE_OWNER_AUTH_RESPONSE,
        ] {
            let err = ControlMessage::decode(&[ty]).unwrap_err();
            assert!(matches!(err, Error::Protocol { .. }), "{err}");
            assert!(
                err.to_string().contains("truncated"),
                "0x{ty:02X} 实装后单字节应报截断: {err}"
            );
        }

        // 未知类型号 0x13/0x99 照旧拒收且错误文本含出错字节值。
        for unknown in [0x13u8, 0x99] {
            let err = ControlMessage::decode(&[unknown]).unwrap_err();
            let msg = err.to_string();
            assert!(matches!(err, Error::Protocol { .. }), "{msg}");
            assert!(
                msg.contains(&format!("0x{unknown:02X}")),
                "错误文本应含出错字节值 0x{unknown:02X}: {msg}"
            );
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

    /// 枚举 / 布尔字段的严格值域：auth_method=3、auth_status=3、
    /// IdrReason=3、BitrateReason=3、ByeReason=2、resume=2 逐个手工
    /// 构包，全部拒收且文本含出错值。
    #[test]
    fn enum_fields_strict_value_domains() {
        // auth_method=3（Hello 的 offset 26；0/1/2 为合法枚举值）
        let mut wire = hello_wire();
        wire[26] = 3;
        let err = ControlMessage::decode(&wire).unwrap_err();
        assert!(err.to_string().contains("got 3"), "{err}");

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

    /// AuthChallenge：21 字节定长 `[0x0D][salt 16B][iters LE]`；roundtrip 相等。
    #[test]
    fn auth_challenge_roundtrip_and_wire_layout() {
        let salt = [
            0x10, 0x21, 0x32, 0x43, 0x54, 0x65, 0x76, 0x87, 0x98, 0xA9, 0xBA, 0xCB, 0xDC, 0xED,
            0xFE, 0x0F,
        ];
        let iterations = 10_000u32; // 0x0000_2710 -> LE [0x10, 0x27, 0x00, 0x00]
        let msg = ControlMessage::AuthChallenge(AuthChallenge { salt, iterations });
        let mut buf = Vec::new();
        msg.encode(&mut buf);

        let mut expected = vec![TYPE_AUTH_CHALLENGE];
        expected.extend_from_slice(&salt);
        expected.extend_from_slice(&[0x10, 0x27, 0x00, 0x00]);
        assert_eq!(buf.len(), 21);
        assert_eq!(buf, expected, "AuthChallenge 线格式与规范不符");
        assert_eq!(ControlMessage::decode(&buf).unwrap(), msg);
    }

    /// AuthResponse：33 字节定长 `[0x0E][proof 32B]`；roundtrip 相等。
    #[test]
    fn auth_response_roundtrip_and_wire_layout() {
        let mut proof = [0u8; AUTH_PROOF_LEN];
        for (i, b) in proof.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(7).wrapping_add(3);
        }
        let msg = ControlMessage::AuthResponse(AuthResponse { proof });
        let mut buf = Vec::new();
        msg.encode(&mut buf);

        let mut expected = vec![TYPE_AUTH_RESPONSE];
        expected.extend_from_slice(&proof);
        assert_eq!(buf.len(), 33);
        assert_eq!(buf, expected, "AuthResponse 线格式与规范不符");
        assert_eq!(ControlMessage::decode(&buf).unwrap(), msg);
    }

    /// AuthChallenge.iterations == 0 报 Error::Protocol（带 offset=17）。
    #[test]
    fn auth_challenge_rejects_zero_iterations() {
        let mut wire = vec![TYPE_AUTH_CHALLENGE];
        wire.extend_from_slice(&[0x55u8; AUTH_SALT_LEN]);
        wire.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(wire.len(), 21);

        let err = ControlMessage::decode(&wire).unwrap_err();
        match &err {
            Error::Protocol { offset, msg } => {
                assert_eq!(*offset, 1 + AUTH_SALT_LEN, "offset 应指向 iterations 字段");
                assert!(msg.contains('0'), "错误文本应含出错值 0: {msg}");
            }
            other => panic!("应报 Error::Protocol，实际: {other}"),
        }
    }

    /// AuthChallenge.iterations：恰 MAX_AUTH_ITERATIONS 可解；+1 拒收且错误文本含上限值。
    #[test]
    fn auth_challenge_iteration_bounds() {
        // 下界 1 与上界 MAX_AUTH_ITERATIONS 均可解
        for iters in [1u32, MAX_AUTH_ITERATIONS] {
            let msg = ControlMessage::AuthChallenge(AuthChallenge {
                salt: [0x22; AUTH_SALT_LEN],
                iterations: iters,
            });
            let mut buf = Vec::new();
            msg.encode(&mut buf);
            assert_eq!(
                ControlMessage::decode(&buf).unwrap(),
                msg,
                "iterations={iters} 应可解"
            );
        }

        // MAX_AUTH_ITERATIONS + 1 拒收且带 offset 与上限值
        let mut wire = vec![TYPE_AUTH_CHALLENGE];
        wire.extend_from_slice(&[0x22; AUTH_SALT_LEN]);
        wire.extend_from_slice(&(MAX_AUTH_ITERATIONS + 1).to_le_bytes());
        let err = ControlMessage::decode(&wire).unwrap_err();
        match &err {
            Error::Protocol { offset, msg } => {
                assert_eq!(*offset, 1 + AUTH_SALT_LEN, "offset 应指向 iterations 字段");
                assert!(
                    msg.contains(&MAX_AUTH_ITERATIONS.to_string()),
                    "错误文本应含上限值 {MAX_AUTH_ITERATIONS}: {msg}"
                );
            }
            other => panic!("应报 Error::Protocol，实际: {other}"),
        }
    }

    /// AuthChallenge（21B）与 AuthResponse（33B）合法编码的每个严格前缀拒收；
    /// +1 尾字节拒收且带 trailing 与准确 offset。
    #[test]
    fn auth_messages_truncation_and_trailing() {
        let cases = [
            (
                ControlMessage::AuthChallenge(AuthChallenge {
                    salt: [0xA5; AUTH_SALT_LEN],
                    iterations: 64,
                }),
                21usize,
            ),
            (
                ControlMessage::AuthResponse(AuthResponse {
                    proof: [0x5A; AUTH_PROOF_LEN],
                }),
                33usize,
            ),
        ];

        for (msg, expected_len) in cases {
            let mut wire = Vec::new();
            msg.encode(&mut wire);
            assert_eq!(wire.len(), expected_len);

            // 穷举每个严格前缀 0..expected_len
            for cut in 0..expected_len {
                let err = ControlMessage::decode(&wire[..cut]).unwrap_err();
                assert!(
                    matches!(err, Error::Protocol { .. }),
                    "{msg:?} 截断到 {cut} 字节应报 Protocol: {err}"
                );
            }

            // +1 尾字节拒收
            wire.push(0xEE);
            let err = ControlMessage::decode(&wire).unwrap_err();
            match &err {
                Error::Protocol { offset, msg: text } => {
                    assert_eq!(*offset, expected_len, "trailing offset 应等于消息定长");
                    assert!(text.contains("trailing"), "应含 trailing 字样: {text}");
                }
                other => panic!("{msg:?} 尾字节应报 Protocol，实际: {other}"),
            }
        }
    }

    /// ClipboardNotify：空文本（`[0x0F, 0x00, 0x00]`）、纯 ASCII、中文+emoji 多字节、
    /// 含 NUL 字节——encode→decode 相等，且逐字节断言 `[0x0F][len u16 LE][text]` 布局
    /// （NUL 透明传输、len 按字节计）。
    #[test]
    fn clipboard_roundtrip_and_wire_layout() {
        let cases = ["", "hello world", "你好，兔子🐇", "before\0middle\0after"];
        for text in cases {
            let msg = ControlMessage::clipboard_notify(text.to_owned()).unwrap();
            let mut wire = Vec::new();
            msg.encode(&mut wire);

            let bytes = text.as_bytes();
            let mut expected = vec![TYPE_CLIPBOARD_NOTIFY];
            expected.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
            expected.extend_from_slice(bytes);

            assert_eq!(wire, expected, "ClipboardNotify({text:?}) 线布局与规范不符");
            if text.is_empty() {
                assert_eq!(wire, vec![0x0F, 0x00, 0x00]);
            }
            let decoded = ControlMessage::decode(&wire)
                .unwrap_or_else(|e| panic!("ClipboardNotify({text:?}) 必须可解: {e}"));
            assert_eq!(decoded, msg);
        }
    }

    /// ClipboardNotify：合法编码的每个严格前缀报 Protocol；合法编码 + 1 尾字节报 Protocol；
    /// 非法 UTF-8 载荷报 Protocol 且带准确偏移。
    #[test]
    fn clipboard_decode_rejects_truncation_and_trailing() {
        for text in ["", "abc", "你好🐇", "a\0b"] {
            let msg = ControlMessage::clipboard_notify(text.to_owned()).unwrap();
            let mut wire = Vec::new();
            msg.encode(&mut wire);
            let expected_len = wire.len();

            // 穷举每个严格前缀 0..expected_len
            for cut in 0..expected_len {
                let err = ControlMessage::decode(&wire[..cut]).unwrap_err();
                assert!(
                    matches!(err, Error::Protocol { .. }),
                    "ClipboardNotify({text:?}) 截断到 {cut} 字节应报 Protocol: {err}"
                );
            }

            // +1 尾字节拒收
            wire.push(0xCC);
            let err = ControlMessage::decode(&wire).unwrap_err();
            match &err {
                Error::Protocol {
                    offset,
                    msg: detail,
                } => {
                    assert_eq!(*offset, expected_len, "trailing offset 应等于消息长度");
                    assert!(detail.contains("trailing"), "应含 trailing 字样: {detail}");
                }
                other => panic!("ClipboardNotify({text:?}) 尾字节应报 Protocol，实际: {other}"),
            }
        }

        // 非法 UTF-8 载荷：前 2 字节 ASCII 合法，第 3 字节 0xFF 非法 → offset = 3 + 2 = 5
        let invalid_utf8 = vec![TYPE_CLIPBOARD_NOTIFY, 0x04, 0x00, b'o', b'k', 0xFF, b'!'];
        let err = ControlMessage::decode(&invalid_utf8).unwrap_err();
        match &err {
            Error::Protocol { offset, msg } => {
                assert_eq!(*offset, 5, "offset 应指向首个非法 UTF-8 字节: {msg}");
                assert!(msg.contains("UTF-8"), "应注明 UTF-8 错误: {msg}");
            }
            other => panic!("非法 UTF-8 应报 Protocol，实际: {other}"),
        }
    }

    /// 0x0F 从预留名单移出、可解出 `ClipboardNotify`；0x0D/0x0E 已由 T19 实装可解；
    /// 未知号 0x10/0x99 照旧拒收且错误文本含出错字节值。
    #[test]
    fn clipboard_type_decodes_and_unknowns_still_rejected() {
        // 0x0F 可解出 ClipboardNotify
        let wire = vec![TYPE_CLIPBOARD_NOTIFY, 0x02, 0x00, b'h', b'i'];
        assert_eq!(
            ControlMessage::decode(&wire).unwrap(),
            ControlMessage::ClipboardNotify {
                text: "hi".to_owned(),
            }
        );

        // 0x0D/0x0E 已由 T19 实装，按当下实装集合验证可正常解码
        let mut ch_wire = vec![TYPE_AUTH_CHALLENGE];
        ch_wire.extend_from_slice(&[0x11; AUTH_SALT_LEN]);
        ch_wire.extend_from_slice(&1u32.to_le_bytes());
        assert!(matches!(
            ControlMessage::decode(&ch_wire).unwrap(),
            ControlMessage::AuthChallenge(_)
        ));

        let mut resp_wire = vec![TYPE_AUTH_RESPONSE];
        resp_wire.extend_from_slice(&[0x22; AUTH_PROOF_LEN]);
        assert!(matches!(
            ControlMessage::decode(&resp_wire).unwrap(),
            ControlMessage::AuthResponse(_)
        ));

        // 未知类型号 0x13/0x99 照旧拒收且含出错字节值
        for unknown in [0x13u8, 0x99] {
            let err = ControlMessage::decode(&[unknown]).unwrap_err();
            let msg = err.to_string();
            assert!(matches!(err, Error::Protocol { offset: 0, .. }), "{msg}");
            assert!(
                msg.contains(&format!("0x{unknown:02X}")),
                "错误文本应含出错字节值 0x{unknown:02X}: {msg}"
            );
        }
    }

    /// 65535 字节恰好可构造且 encode 不 panic、可往返；65536 → Err(InvalidConfig)。
    #[test]
    fn clipboard_constructor_enforces_cap() {
        let max_text = "a".repeat(MAX_CLIPBOARD_TEXT_BYTES);
        assert_eq!(max_text.len(), 65535);
        let msg = ControlMessage::clipboard_notify(max_text.clone())
            .expect("恰好 MAX_CLIPBOARD_TEXT_BYTES 字节必须可构造");
        let mut wire = Vec::new();
        msg.encode(&mut wire);
        assert_eq!(wire.len(), 3 + MAX_CLIPBOARD_TEXT_BYTES);
        assert_eq!(ControlMessage::decode(&wire).unwrap(), msg);

        let over_text = "a".repeat(MAX_CLIPBOARD_TEXT_BYTES + 1);
        assert_eq!(over_text.len(), 65536);
        let err = ControlMessage::clipboard_notify(over_text).unwrap_err();
        assert!(
            matches!(err, Error::InvalidConfig(_)),
            "65536 字节必须返回 Error::InvalidConfig，实际: {err}"
        );
    }

    /// ToolChannel：全 5 个合法通道（含空载荷、二进制载荷、恰好 MAX_TOOL_PAYLOAD_BYTES）
    /// 往返相等，且逐字节断言 `[0x10][channel: u8][len: u32 LE][payload]` 线布局。
    #[test]
    fn tool_channel_roundtrip_and_wire_layout() {
        for channel in [
            TOOL_CHANNEL_FILE,
            TOOL_CHANNEL_TERMINAL,
            TOOL_CHANNEL_PORTMAP,
            TOOL_CHANNEL_AUDIO_CTRL,
            TOOL_CHANNEL_DISPLAY_CTRL,
        ] {
            let payload = vec![0x00, channel, 0xAA, 0xFF, 0x42];
            let msg = ControlMessage::tool_channel(channel, payload.clone()).unwrap();
            let mut wire = Vec::new();
            msg.encode(&mut wire);

            let mut expected = vec![TYPE_TOOL_CHANNEL, channel];
            expected.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            expected.extend_from_slice(&payload);
            assert_eq!(wire, expected, "ToolChannel(ch={channel}) 线布局不符");
            assert_eq!(ControlMessage::decode(&wire).unwrap(), msg);
        }

        // 恰好 MAX_TOOL_PAYLOAD_BYTES 合法往返
        let max_msg =
            ControlMessage::tool_channel(TOOL_CHANNEL_FILE, vec![0x5Au8; MAX_TOOL_PAYLOAD_BYTES])
                .expect("MAX_TOOL_PAYLOAD_BYTES 必须合法");
        let mut wire = Vec::new();
        max_msg.encode(&mut wire);
        assert_eq!(wire.len(), 6 + MAX_TOOL_PAYLOAD_BYTES);
        assert_eq!(ControlMessage::decode(&wire).unwrap(), max_msg);
    }

    /// ToolChannel：非法通道（0 / 6 / 255）、超长载荷（MAX+1）、任意偏移截断与尾字节
    /// 一律报 `Error::Protocol`（构造器报 `Error::InvalidConfig`）。
    #[test]
    fn tool_channel_rejects_invalid_channel_oversized_truncated_trailing() {
        // 非法 channel
        for bad_ch in [0u8, 6, 255] {
            assert!(matches!(
                ControlMessage::tool_channel(bad_ch, vec![1, 2]),
                Err(Error::InvalidConfig(_))
            ));
            let mut wire = vec![TYPE_TOOL_CHANNEL, bad_ch];
            wire.extend_from_slice(&0u32.to_le_bytes());
            let err = ControlMessage::decode(&wire).unwrap_err();
            assert!(
                matches!(err, Error::Protocol { offset: 1, .. }),
                "channel={bad_ch} 应报 Protocol(offset=1)，实际: {err}"
            );
        }

        // 超长载荷（MAX_TOOL_PAYLOAD_BYTES + 1）
        assert!(matches!(
            ControlMessage::tool_channel(TOOL_CHANNEL_FILE, vec![0u8; MAX_TOOL_PAYLOAD_BYTES + 1]),
            Err(Error::InvalidConfig(_))
        ));
        let mut over_wire = vec![TYPE_TOOL_CHANNEL, TOOL_CHANNEL_FILE];
        over_wire.extend_from_slice(&((MAX_TOOL_PAYLOAD_BYTES + 1) as u32).to_le_bytes());
        let err = ControlMessage::decode(&over_wire).unwrap_err();
        assert!(
            matches!(err, Error::Protocol { offset: 2, .. }),
            "超长 ToolChannel 应报 Protocol(offset=2)，实际: {err}"
        );

        // 截断与尾字节
        let msg = ControlMessage::tool_channel(TOOL_CHANNEL_TERMINAL, b"ls -la".to_vec()).unwrap();
        let mut wire = Vec::new();
        msg.encode(&mut wire);
        let expected_len = wire.len();
        for cut in 0..expected_len {
            let err = ControlMessage::decode(&wire[..cut]).unwrap_err();
            assert!(
                matches!(err, Error::Protocol { .. }),
                "ToolChannel 截断到 {cut} 字节应报 Protocol: {err}"
            );
        }
        wire.push(0xEE);
        let err = ControlMessage::decode(&wire).unwrap_err();
        match &err {
            Error::Protocol { offset, msg } => {
                assert_eq!(*offset, expected_len);
                assert!(msg.contains("trailing"), "{msg}");
            }
            other => panic!("尾字节应报 Protocol，实际: {other}"),
        }
    }

    /// AudioFrame：PCM16 / IMA-ADPCM、单声道 / 立体声、空数据与恰好 MAX_AUDIO_FRAME_BYTES
    /// 往返相等，且逐字节断言 17 字节头 + data 线布局。
    #[test]
    fn audio_frame_roundtrip_and_wire_layout() {
        let packet = AudioFramePacket {
            seq: 0x0102_0304,
            timestamp_us: 0x1122_3344,
            sample_rate: 48_000,
            channels: 2,
            codec: AUDIO_CODEC_IMA_ADPCM,
            data: vec![0x10, 0x20, 0x30, 0x40],
        };
        let msg = ControlMessage::audio_frame(packet.clone()).unwrap();
        let mut wire = Vec::new();
        msg.encode(&mut wire);

        let mut expected = vec![TYPE_AUDIO_FRAME];
        expected.extend_from_slice(&packet.seq.to_le_bytes());
        expected.extend_from_slice(&packet.timestamp_us.to_le_bytes());
        expected.extend_from_slice(&packet.sample_rate.to_le_bytes());
        expected.push(packet.channels);
        expected.push(packet.codec);
        expected.extend_from_slice(&(packet.data.len() as u16).to_le_bytes());
        expected.extend_from_slice(&packet.data);
        assert_eq!(wire.len(), 17 + 4);
        assert_eq!(wire, expected, "AudioFrame 线布局不符");
        assert_eq!(ControlMessage::decode(&wire).unwrap(), msg);

        // 单声道 + PCM16 + 恰好 MAX_AUDIO_FRAME_BYTES
        let max_pkt = AudioFramePacket {
            seq: 9,
            timestamp_us: 0,
            sample_rate: 16_000,
            channels: 1,
            codec: AUDIO_CODEC_PCM16,
            data: vec![0x7Fu8; MAX_AUDIO_FRAME_BYTES],
        };
        let max_msg = ControlMessage::audio_frame(max_pkt).unwrap();
        let mut max_wire = Vec::new();
        max_msg.encode(&mut max_wire);
        assert_eq!(max_wire.len(), 17 + MAX_AUDIO_FRAME_BYTES);
        assert_eq!(ControlMessage::decode(&max_wire).unwrap(), max_msg);
    }

    /// AudioFrame：sample_rate 越界（0 / 4000 / 7999 / 192001 / u32::MAX）、channels=0/3、
    /// codec=2/255、len > MAX_AUDIO_FRAME_BYTES、PCM16 帧长未对齐、任意偏移截断与尾字节
    /// 一律报 `Error::Protocol`（构造器报 `Error::InvalidConfig`）。
    #[test]
    fn audio_frame_rejects_invalid_fields_oversized_truncated_trailing() {
        let valid_wire = || {
            let mut w = Vec::new();
            ControlMessage::AudioFrame {
                seq: 1,
                timestamp_us: 1000,
                sample_rate: 48_000,
                channels: 2,
                codec: AUDIO_CODEC_PCM16,
                data: vec![1, 2, 3, 4],
            }
            .encode(&mut w);
            w
        };

        // sample_rate out of MIN_AUDIO_SAMPLE_RATE..=MAX_AUDIO_SAMPLE_RATE (offset 9..13)
        for bad_rate in [
            0u32,
            4_000,
            MIN_AUDIO_SAMPLE_RATE - 1,
            MAX_AUDIO_SAMPLE_RATE + 1,
            u32::MAX,
        ] {
            let mut w = valid_wire();
            w[9..13].copy_from_slice(&bad_rate.to_le_bytes());
            let err = ControlMessage::decode(&w).unwrap_err();
            assert!(
                matches!(err, Error::Protocol { offset: 9, .. }),
                "sample_rate={bad_rate} should fail with Protocol at offset 9, got {err}"
            );
            assert!(
                matches!(
                    ControlMessage::audio_frame(AudioFramePacket {
                        seq: 1,
                        timestamp_us: 0,
                        sample_rate: bad_rate,
                        channels: 2,
                        codec: AUDIO_CODEC_PCM16,
                        data: vec![],
                    }),
                    Err(Error::InvalidConfig(_))
                ),
                "sample_rate={bad_rate} should fail in audio_frame()"
            );
        }

        // Boundary valid sample rates (8_000 and 192_000) succeed.
        for ok_rate in [MIN_AUDIO_SAMPLE_RATE, MAX_AUDIO_SAMPLE_RATE] {
            let msg = ControlMessage::audio_frame(AudioFramePacket {
                seq: 1,
                timestamp_us: 0,
                sample_rate: ok_rate,
                channels: 2,
                codec: AUDIO_CODEC_PCM16,
                data: vec![0u8; 4],
            })
            .expect("boundary sample_rate must be accepted");
            let mut wire = Vec::new();
            msg.encode(&mut wire);
            assert_eq!(ControlMessage::decode(&wire).unwrap(), msg);
        }

        // PCM16 unaligned payload lengths rejected on both encode constructor and wire decode.
        for (ch, bad_len) in [(1u8, 1usize), (1, 3), (2, 1), (2, 2), (2, 3), (2, 6)] {
            assert!(
                matches!(
                    ControlMessage::audio_frame(AudioFramePacket {
                        seq: 1,
                        timestamp_us: 0,
                        sample_rate: 48_000,
                        channels: ch,
                        codec: AUDIO_CODEC_PCM16,
                        data: vec![0u8; bad_len],
                    }),
                    Err(Error::InvalidConfig(_))
                ),
                "PCM16 ch={ch} len={bad_len} must be rejected in constructor"
            );
            let mut w = Vec::new();
            ControlMessage::AudioFrame {
                seq: 1,
                timestamp_us: 0,
                sample_rate: 48_000,
                channels: ch,
                codec: AUDIO_CODEC_PCM16,
                data: vec![0u8; bad_len],
            }
            .encode(&mut w);
            let err = ControlMessage::decode(&w).unwrap_err();
            assert!(
                matches!(err, Error::Protocol { offset: 15, .. }),
                "PCM16 ch={ch} len={bad_len} must be rejected at len offset 15, got {err}"
            );
        }

        // IMA-ADPCM does not require PCM16 2*ch alignment (odd/arbitrary lengths allowed).
        let adpcm_msg = ControlMessage::audio_frame(AudioFramePacket {
            seq: 2,
            timestamp_us: 0,
            sample_rate: 48_000,
            channels: 2,
            codec: AUDIO_CODEC_IMA_ADPCM,
            data: vec![0u8; 9],
        })
        .expect("IMA-ADPCM with odd byte length must succeed");
        let mut adpcm_wire = Vec::new();
        adpcm_msg.encode(&mut adpcm_wire);
        assert_eq!(ControlMessage::decode(&adpcm_wire).unwrap(), adpcm_msg);

        // channels == 0 / 3 (offset 13)
        for bad_ch in [0u8, 3, 255] {
            let mut w = valid_wire();
            w[13] = bad_ch;
            let err = ControlMessage::decode(&w).unwrap_err();
            assert!(matches!(err, Error::Protocol { offset: 13, .. }), "{err}");
            assert!(matches!(
                ControlMessage::audio_frame(AudioFramePacket {
                    seq: 1,
                    timestamp_us: 0,
                    sample_rate: 48_000,
                    channels: bad_ch,
                    codec: AUDIO_CODEC_PCM16,
                    data: vec![],
                }),
                Err(Error::InvalidConfig(_))
            ));
        }

        // codec > AUDIO_CODEC_IMA_ADPCM (offset 14)
        for bad_codec in [2u8, 255] {
            let mut w = valid_wire();
            w[14] = bad_codec;
            let err = ControlMessage::decode(&w).unwrap_err();
            assert!(matches!(err, Error::Protocol { offset: 14, .. }), "{err}");
            assert!(matches!(
                ControlMessage::audio_frame(AudioFramePacket {
                    seq: 1,
                    timestamp_us: 0,
                    sample_rate: 48_000,
                    channels: 2,
                    codec: bad_codec,
                    data: vec![],
                }),
                Err(Error::InvalidConfig(_))
            ));
        }

        // len > MAX_AUDIO_FRAME_BYTES (offset 15..17)
        let mut w = valid_wire();
        let over_len = (MAX_AUDIO_FRAME_BYTES + 1) as u16;
        w[15..17].copy_from_slice(&over_len.to_le_bytes());
        let err = ControlMessage::decode(&w).unwrap_err();
        assert!(matches!(err, Error::Protocol { offset: 15, .. }), "{err}");
        assert!(matches!(
            ControlMessage::audio_frame(AudioFramePacket {
                seq: 1,
                timestamp_us: 0,
                sample_rate: 48_000,
                channels: 2,
                codec: AUDIO_CODEC_PCM16,
                data: vec![0u8; MAX_AUDIO_FRAME_BYTES + 1],
            }),
            Err(Error::InvalidConfig(_))
        ));

        // 截断与尾字节
        let mut wire = valid_wire();
        let expected_len = wire.len();
        for cut in 0..expected_len {
            let err = ControlMessage::decode(&wire[..cut]).unwrap_err();
            assert!(
                matches!(err, Error::Protocol { .. }),
                "AudioFrame 截断到 {cut} 字节应报 Protocol: {err}"
            );
        }
        wire.push(0xBB);
        let err = ControlMessage::decode(&wire).unwrap_err();
        match &err {
            Error::Protocol { offset, msg } => {
                assert_eq!(*offset, expected_len);
                assert!(msg.contains("trailing"), "{msg}");
            }
            other => panic!("尾字节应报 Protocol，实际: {other}"),
        }
    }

    /// AuthMethod::OwnerTicket 往返编解码、from_u8 校验与 Hello.auth_payload 上限校验（<= 4096）。
    #[test]
    fn owner_ticket_hello_roundtrip_and_payload_bounds() {
        assert_eq!(AuthMethod::from_u8(0), Some(AuthMethod::None));
        assert_eq!(AuthMethod::from_u8(1), Some(AuthMethod::Password));
        assert_eq!(AuthMethod::from_u8(2), Some(AuthMethod::OwnerTicket));
        assert_eq!(AuthMethod::from_u8(3), None);
        assert_eq!(AuthMethod::from_u8(255), None);

        for payload_len in [0usize, 1, 64, MAX_OWNER_TICKET_PAYLOAD_LEN] {
            let mut h = sample_hello();
            h.auth_method = AuthMethod::OwnerTicket;
            h.auth_payload = vec![0x5C; payload_len];
            let msg = ControlMessage::Hello(h);
            let mut wire = Vec::new();
            msg.encode(&mut wire);
            assert_eq!(wire.len(), 29 + payload_len);
            assert_eq!(wire[26], 2);
            assert_eq!(ControlMessage::decode(&wire).unwrap(), msg);
        }

        // 超过 MAX_OWNER_TICKET_PAYLOAD_LEN (4096) 的 auth_len 必须拒收
        let mut wire = hello_wire();
        wire[26] = AuthMethod::OwnerTicket as u8;
        let over_len = (MAX_OWNER_TICKET_PAYLOAD_LEN + 1) as u16;
        wire[27..29].copy_from_slice(&over_len.to_le_bytes());
        wire.extend(std::iter::repeat_n(
            0x5Cu8,
            MAX_OWNER_TICKET_PAYLOAD_LEN + 1,
        ));
        let err = ControlMessage::decode(&wire).unwrap_err();
        assert!(
            matches!(err, Error::Protocol { offset: 27, .. }),
            "超长 auth_payload 应报 Protocol(offset=27)，实际: {err}"
        );
    }

    /// OwnerAuthResponse（0x12）：[0x12][len: u16 LE (1..=128)][signature] 往返与错包校验。
    #[test]
    fn owner_auth_response_roundtrip_and_validation() {
        for sig_len in [1usize, 64, MAX_OWNER_SIGNATURE_LEN] {
            let sig: Vec<u8> = (0..sig_len).map(|i| (i as u8).wrapping_mul(3)).collect();
            let msg = ControlMessage::OwnerAuthResponse {
                signature: sig.clone(),
            };
            let mut wire = Vec::new();
            msg.encode(&mut wire);

            let mut expected = vec![TYPE_OWNER_AUTH_RESPONSE];
            expected.extend_from_slice(&(sig_len as u16).to_le_bytes());
            expected.extend_from_slice(&sig);
            assert_eq!(wire, expected);
            assert_eq!(ControlMessage::decode(&wire).unwrap(), msg);

            // 严格前缀截断拒收
            for cut in 0..wire.len() {
                assert!(
                    matches!(
                        ControlMessage::decode(&wire[..cut]),
                        Err(Error::Protocol { .. })
                    ),
                    "OwnerAuthResponse(len={sig_len}) 截断到 {cut} 字节应报 Protocol"
                );
            }
            // +1 尾字节拒收
            let expected_len = wire.len();
            wire.push(0x99);
            let err = ControlMessage::decode(&wire).unwrap_err();
            assert!(
                matches!(err, Error::Protocol { offset, ref msg } if offset == expected_len && msg.contains("trailing")),
                "尾字节应报 Protocol(offset={expected_len})，实际: {err}"
            );
        }

        // 长度 0 或 > 128 必须拒收
        for bad_len in [0u16, (MAX_OWNER_SIGNATURE_LEN + 1) as u16, u16::MAX] {
            let mut wire = vec![TYPE_OWNER_AUTH_RESPONSE];
            wire.extend_from_slice(&bad_len.to_le_bytes());
            wire.extend(std::iter::repeat_n(0xAAu8, usize::from(bad_len)));
            let err = ControlMessage::decode(&wire).unwrap_err();
            assert!(
                matches!(err, Error::Protocol { offset: 1, .. }),
                "len={bad_len} 应报 Protocol(offset=1)，实际: {err}"
            );
        }
    }
}
