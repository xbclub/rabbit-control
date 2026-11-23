//! QUIC 传输层：datagram 媒体 + reliable 控制/输入（输入独立流）、分片重组、FEC 集成、pacing。
//!
//! # 为什么是 QUIC 而不是 TCP
//! TCP 的队头阻塞是「画面冻结」的直接原因：丢一个包，后面全部等待重传。
//! QUIC 的 datagram 扩展（RFC 9221）提供「不重传的不可靠」——丢包时宁可丢帧
//! 也不等待，这正是实时屏幕流的正确取舍。
//!
//! # 模块地图
//! - [`quic`]：`quinn` 封装，`Transport` trait 的实现（含 async→同步桥接）
//! - [`reassemble`]：分片重组（[`reassemble::FrameAssembler`]）
//! - [`fec_adapter`]：接 `rc-fec`（[`fec_adapter::FecSender`] / [`fec_adapter::FecReceiver`]）
//! - [`pacer`]：平滑发送（[`pacer::Pacer`]）
//! - [`stats`]：统计与拥塞信号（[`stats::LossEstimator`]）
//!
//! # 通道分工（N1，不可混用）
//! | 通道 | 语义 | 承载 |
//! |---|---|---|
//! | datagram | 不可靠、无序、丢包不重传 | 媒体分片（`rc-proto` 数据报） |
//! | reliable stream | 可靠、有序 | 控制消息 |
//! | reliable urgent stream | 可靠、有序、独立流（高发送优先级） | 输入事件 |
//!
//! 媒体绝不走 reliable：一次重传（RTT 级别）就会让画面冻结一个 RTT。
//! 输入绝不与控制共享流：控制流被大流量灌满时，urgent 消息走独立的
//! 第二条 reliable 流（见 [`Transport::send_reliable_urgent`]），
//! **输入消息永不排在控制消息之后**。

#![deny(missing_docs)]
#![deny(unsafe_code)]

pub mod fec_adapter;
pub mod mtls;
pub mod pacer;
pub mod quic;
pub mod reassemble;
pub mod stats;

pub use mtls::{cert_sha256_hex, parse_rc_san_uri, MtlsConfig, PeerMtlsIdentity};

use std::net::SocketAddr;
use std::time::Duration;

use rc_types::Result;

/// 本端在会话中的角色。
///
/// Host 是被控端（发送媒体、接收输入）；Controller 是控制端（发送输入、接收媒体）。
/// 本卡里两者只影响谁监听、谁发起连接；媒体/输入的方向由上层决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// 被控端：监听并接受连接
    Host,
    /// 控制端：主动连接对端
    Controller,
}

/// 传输层配置。
///
/// `mtu` 是路径 MTU 预算（默认 1200）。单个 datagram（含 32 字节媒体包头）
/// 不得超过 `mtu - QUIC_OVERHEAD`，这是 N2 的硬约束：QUIC datagram 不能
/// 跨 QUIC 包，超了要么被对端拒收、要么触发 IP 分片（丢一等于丢全）。
#[derive(Debug, Clone)]
pub struct NetConfig {
    /// 本端角色
    pub role: Role,
    /// 本地绑定地址（`127.0.0.1:0` 表示任选空闲端口）
    pub local_addr: SocketAddr,
    /// 对端地址。Controller 必填；Host 可空（由 accept 得到）
    pub peer_addr: Option<SocketAddr>,
    /// 路径 MTU 预算（字节），默认 1200
    pub mtu: usize,
}

impl NetConfig {
    /// 以指定角色与本地地址构造，`mtu` 取默认值 1200。
    pub fn new(role: Role, local_addr: SocketAddr) -> Self {
        Self {
            role,
            local_addr,
            peer_addr: None,
            mtu: quic::DEFAULT_MTU,
        }
    }
}

/// 传输层统一接口（契约 §7.3，签名不可自创）。
///
/// 实现见 [`quic::QuicTransport`]。所有方法都是**同步**的：本项目的管线
/// 是同步模型，异步只被封装在实现内部（见 `quic.rs` 的桥接说明）。
pub trait Transport: Send {
    /// 发送媒体数据报（不可靠）。数据格式是 `rc-proto` 的「32 字节头 + 负载」。
    ///
    /// 超过 `mtu - QUIC_OVERHEAD` 立即返回错误（N2）；否则进入 pacing 队列
    /// 由后台线程平滑写出（N3），本方法立即返回。
    fn send_datagram(&mut self, data: &[u8]) -> Result<()>;

    /// 发送控制/输入（可靠有序）。消息按长度前缀成帧，保证边界。
    fn send_reliable(&mut self, data: &[u8]) -> Result<()>;

    /// 发送输入事件（可靠有序，**独立 urgent 流**）。
    ///
    /// 语义：输入消息永不排在控制消息之后——控制流被大流量灌满时，
    /// urgent 消息不受控制流的队头阻塞（[`quic::QuicTransport`] 用独立的
    /// 第二条 reliable 流实现，并叠加 quinn stream priority）。
    /// 接收侧以 [`NetEvent::ReliableUrgent`] 交付，边界与有序性同
    /// [`NetEvent::Reliable`]。
    ///
    /// 默认实现委托 [`Transport::send_reliable`]：单流实现与测试替身
    /// 无需感知独立流即可编译（降级为「与控制共享流」的旧行为）。
    fn send_reliable_urgent(&mut self, data: &[u8]) -> Result<()> {
        self.send_reliable(data)
    }

    /// 非阻塞接收。返回所有已就绪消息；最多阻塞 `timeout`（N4）。
    fn poll(&mut self, timeout: Duration) -> Result<Vec<NetEvent>>;

    /// 网络统计（用于拥塞控制与诊断）。
    fn stats(&self) -> NetStats;
}

/// 从传输层收到的事件。
#[derive(Debug, Clone, PartialEq)]
pub enum NetEvent {
    /// 媒体数据报（不可靠通道，原样字节）
    Datagram(Vec<u8>),
    /// 控制/输入消息（可靠通道，一条完整消息）
    Reliable(Vec<u8>),
    /// 输入消息（独立 urgent 流，一条完整消息）。与 [`NetEvent::Reliable`]
    /// 同样可靠有序，但不与控制消息共享队列——控制流再堵也不挡输入。
    ReliableUrgent(Vec<u8>),
    /// 连接建立完成
    Connected,
    /// 连接断开
    Disconnected {
        /// 人类可读原因
        reason: String,
    },
    /// 连接迁移（路径变化，N9：媒体流不中断，继续用原连接收发）
    PathChanged,
}

/// 网络统计（N8：拥塞控制的输入）。
///
/// 含 `f32`（`loss_rate`），因此不能派生 `Eq`。
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct NetStats {
    /// 平滑 RTT 估计（微秒）
    pub rtt_us: u32,
    /// 实测丢包率（由 NACK/缺失推断，0.0~1.0）
    pub loss_rate: f32,
    /// 当前 pacing 速率（bps）
    pub pacing_rate_bps: u64,
    /// 拥塞窗口（字节）
    pub cwnd_bytes: u64,
    /// 应用层已发出字节数（datagram + reliable）
    pub bytes_sent: u64,
    /// 应用层已接收字节数
    pub bytes_recv: u64,
}
