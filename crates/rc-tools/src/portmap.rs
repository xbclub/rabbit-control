//! TCP 端口映射引擎（W4）：本地端口监听、隧道多路复用代理与实时流量统计。
//!
//! 线协议采用 `[kind: u8][header_len: u32 LE][json_header][raw_bytes]`，
//! 单帧严格限制在 [`MAX_PORTMAP_WIRE_FRAME_BYTES`]（64 KiB），单个数据块不超过
//! [`PORTMAP_CHUNK_SIZE`]（32 KiB）。

#![deny(missing_docs)]
#![deny(unsafe_code)]

use std::collections::{BTreeMap, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// 单个端口映射线帧最大字节数（64 KiB）。
pub const MAX_PORTMAP_WIRE_FRAME_BYTES: usize = 64 * 1024;

/// 单条 `ConnData` 负载最大字节数（32 KiB）。
pub const PORTMAP_CHUNK_SIZE: usize = 32 * 1024;

/// 被控端允许同时代理的最大 TCP 连接数。
pub const MAX_HOST_PROXY_CONNS: usize = 32;

/// 被控端连接目标 TCP 地址的超时时间（5 秒）。
pub const HOST_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// 端口映射引擎待发队列上限（帧数），超过后暂停从工作线程通道拉取数据，形成真实 TCP 反压。
pub const MAX_OUTGOING_FRAMES: usize = 128;

/// 每条代理连接的初始隧道发送窗口（2 MiB）。
pub const INITIAL_CONN_WINDOW_BYTES: usize = 2 * 1024 * 1024;

/// 写入端每向本地/目标 TCP 写入该字节数后回充一次 `ConnWindow` 信用（256 KiB）。
pub const CONN_WINDOW_UPDATE_THRESHOLD: usize = 256 * 1024;

/// 控制端允许同时处于 `Pending`（等待 Host 建连）状态的连接上限。
pub const MAX_CLIENT_PENDING_CONNS: usize = 64;

/// 控制端单条 `Pending` 连接的最大等待超时（15 秒）。
pub const CLIENT_PENDING_TIMEOUT: Duration = Duration::from_secs(15);

const FRAME_KIND_CONTROL: u8 = 1;
const FRAME_KIND_DATA: u8 = 2;
const FRAME_PREFIX_LEN: usize = 5;

const IO_POLL_TIMEOUT: Duration = Duration::from_millis(50);
const TCP_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const SEND_RETRY_INTERVAL: Duration = Duration::from_millis(10);
const LISTENER_POLL_INTERVAL: Duration = Duration::from_millis(5);
const WINDOW_WAIT_INTERVAL: Duration = Duration::from_millis(25);
const EVENT_CHANNEL_CAP: usize = 512;
const WRITE_CHANNEL_CAP: usize = 256;

fn validate_target_addr(addr: &str) -> bool {
    !addr.is_empty() && !addr.chars().any(|c| c.is_whitespace() || c.is_control())
}

struct SendWindow {
    available: AtomicUsize,
    wait_lock: Mutex<()>,
    cvar: Condvar,
}

impl SendWindow {
    fn new(initial: usize) -> Self {
        Self {
            available: AtomicUsize::new(initial),
            wait_lock: Mutex::new(()),
            cvar: Condvar::new(),
        }
    }

    fn add_credit(&self, delta: u32) {
        if delta == 0 {
            return;
        }
        let _guard = self.wait_lock.lock().unwrap_or_else(|e| e.into_inner());
        let _ = self
            .available
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                Some(cur.saturating_add(delta as usize))
            });
        self.cvar.notify_all();
    }

    fn notify_all(&self) {
        let _guard = self.wait_lock.lock().unwrap_or_else(|e| e.into_inner());
        self.cvar.notify_all();
    }

    fn wait_for_budget(&self, max_bytes: usize, stop: &AtomicBool) -> usize {
        let fast = self.available.load(Ordering::Acquire);
        if fast > 0 {
            return fast.min(max_bytes);
        }
        let mut guard = self.wait_lock.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if stop.load(Ordering::Acquire) {
                return 0;
            }
            let avail = self.available.load(Ordering::Acquire);
            if avail > 0 {
                return avail.min(max_bytes);
            }
            let (g, _) = self
                .cvar
                .wait_timeout(guard, WINDOW_WAIT_INTERVAL)
                .unwrap_or_else(|e| e.into_inner());
            guard = g;
        }
    }

    fn consume(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        let _ = self
            .available
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                Some(cur.saturating_sub(bytes))
            });
    }
}

struct WorkerCountGuard(Arc<AtomicUsize>);

impl Drop for WorkerCountGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn send_cancellable<T>(tx: &SyncSender<T>, mut item: T, stop: &AtomicBool) -> bool {
    while !stop.load(Ordering::Acquire) {
        match tx.try_send(item) {
            Ok(()) => return true,
            Err(TrySendError::Disconnected(_)) => return false,
            Err(TrySendError::Full(returned)) => {
                item = returned;
                thread::sleep(SEND_RETRY_INTERVAL);
            }
        }
    }
    false
}

/// 单条端口映射规则的运行状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PortMapStatus {
    /// 本地端口已绑定就绪。
    Ok,
    /// 本地端口绑定失败或发生错误。
    Fail,
    /// 规则已暂停/未激活。
    Pending,
}

/// 单条端口映射规则的完整 UI 状态快照。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortMapRuleSnapshot {
    /// 规则唯一 ID。
    pub id: u64,
    /// 规则名称。
    pub name: String,
    /// 本地监听端口（绑定 `127.0.0.1:<local_port>`）。
    pub local_port: u16,
    /// 被控端侧目标主机/IP（如 `127.0.0.1`）。
    pub target_addr: String,
    /// 被控端侧目标端口。
    pub target_port: u16,
    /// 规则是否启用。
    pub enabled: bool,
    /// 当前运行状态。
    pub status: PortMapStatus,
    /// 当前活跃的代理连接数。
    pub active_conns: u32,
    /// 累计发送字节数（本地 -> 远端）。
    pub bytes_tx: u64,
    /// 累计接收字节数（远端 -> 本地）。
    pub bytes_rx: u64,
    /// 最近错误信息（若有）。
    pub error: Option<String>,
}

/// 端口映射线协议消息。
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortMapWireMsg {
    /// 客户端通知主机建立到目标地址的代理 TCP 连接。
    ConnOpen {
        /// 连接唯一 ID。
        conn_id: u64,
        /// 所属规则 ID。
        rule_id: u64,
        /// 目标地址。
        target_addr: String,
        /// 目标端口。
        target_port: u16,
    },
    /// 主机通知目标 TCP 连接已建立就绪。
    ConnReady {
        /// 连接唯一 ID。
        conn_id: u64,
        /// 所属规则 ID。
        rule_id: u64,
    },
    /// 主机通知目标 TCP 连接建立失败。
    ConnError {
        /// 连接唯一 ID。
        conn_id: u64,
        /// 所属规则 ID。
        rule_id: u64,
        /// 错误详情。
        message: String,
    },
    /// 双向 TCP 负载数据块（`<= 32 KiB`）。
    ConnData {
        /// 连接唯一 ID。
        conn_id: u64,
        /// 原始 TCP 字节流切片。
        data: Vec<u8>,
    },
    /// 关闭代理 TCP 连接（硬中止/重置）。
    ConnClose {
        /// 连接唯一 ID。
        conn_id: u64,
    },
    /// 优雅半关闭写方向（发送方读到 TCP EOF，通知对端刷完缓冲后关闭写端）。
    ConnHalfClose {
        /// 连接唯一 ID。
        conn_id: u64,
    },
    /// 每连接隧道流控窗口回充信用（字节）。
    ConnWindow {
        /// 连接唯一 ID。
        conn_id: u64,
        /// 新增可用发送窗口字节数。
        delta_bytes: u32,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
enum PortMapControlPayload {
    #[serde(rename = "conn_open")]
    Open {
        conn_id: u64,
        rule_id: u64,
        target_addr: String,
        target_port: u16,
    },
    #[serde(rename = "conn_ready")]
    Ready { conn_id: u64, rule_id: u64 },
    #[serde(rename = "conn_error")]
    Error {
        conn_id: u64,
        rule_id: u64,
        message: String,
    },
    #[serde(rename = "conn_close")]
    Close { conn_id: u64 },
    #[serde(rename = "conn_half_close")]
    HalfClose { conn_id: u64 },
    #[serde(rename = "conn_window")]
    Window { conn_id: u64, delta_bytes: u32 },
}

#[derive(Debug, Serialize, Deserialize)]
struct PortMapDataHeader {
    conn_id: u64,
}

impl PortMapWireMsg {
    /// 编码为严格受限（`<= 64 KiB`）的二进制线帧。
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        match self {
            PortMapWireMsg::ConnData { conn_id, data } => {
                if data.len() > PORTMAP_CHUNK_SIZE {
                    return Err(format!(
                        "portmap chunk too large ({} > {PORTMAP_CHUNK_SIZE})",
                        data.len()
                    ));
                }
                let header = serde_json::to_vec(&PortMapDataHeader { conn_id: *conn_id })
                    .map_err(|e| format!("serialize portmap data header failed: {e}"))?;
                let total = FRAME_PREFIX_LEN + header.len() + data.len();
                if total > MAX_PORTMAP_WIRE_FRAME_BYTES {
                    return Err(format!(
                        "portmap data frame exceeds {MAX_PORTMAP_WIRE_FRAME_BYTES} bytes"
                    ));
                }
                let mut out = Vec::with_capacity(total);
                out.push(FRAME_KIND_DATA);
                out.extend_from_slice(&(header.len() as u32).to_le_bytes());
                out.extend_from_slice(&header);
                out.extend_from_slice(data);
                Ok(out)
            }
            other => {
                let ctrl = match other {
                    PortMapWireMsg::ConnOpen {
                        conn_id,
                        rule_id,
                        target_addr,
                        target_port,
                    } => PortMapControlPayload::Open {
                        conn_id: *conn_id,
                        rule_id: *rule_id,
                        target_addr: target_addr.clone(),
                        target_port: *target_port,
                    },
                    PortMapWireMsg::ConnReady { conn_id, rule_id } => {
                        PortMapControlPayload::Ready {
                            conn_id: *conn_id,
                            rule_id: *rule_id,
                        }
                    }
                    PortMapWireMsg::ConnError {
                        conn_id,
                        rule_id,
                        message,
                    } => PortMapControlPayload::Error {
                        conn_id: *conn_id,
                        rule_id: *rule_id,
                        message: message.clone(),
                    },
                    PortMapWireMsg::ConnClose { conn_id } => {
                        PortMapControlPayload::Close { conn_id: *conn_id }
                    }
                    PortMapWireMsg::ConnHalfClose { conn_id } => {
                        PortMapControlPayload::HalfClose { conn_id: *conn_id }
                    }
                    PortMapWireMsg::ConnWindow {
                        conn_id,
                        delta_bytes,
                    } => PortMapControlPayload::Window {
                        conn_id: *conn_id,
                        delta_bytes: *delta_bytes,
                    },
                    PortMapWireMsg::ConnData { .. } => unreachable!(),
                };
                let header = serde_json::to_vec(&ctrl)
                    .map_err(|e| format!("serialize portmap control failed: {e}"))?;
                let total = FRAME_PREFIX_LEN + header.len();
                if total > MAX_PORTMAP_WIRE_FRAME_BYTES {
                    return Err(format!(
                        "portmap control frame exceeds {MAX_PORTMAP_WIRE_FRAME_BYTES} bytes"
                    ));
                }
                let mut out = Vec::with_capacity(total);
                out.push(FRAME_KIND_CONTROL);
                out.extend_from_slice(&(header.len() as u32).to_le_bytes());
                out.extend_from_slice(&header);
                Ok(out)
            }
        }
    }

    /// 严格校验并解码二进制线帧。
    pub fn decode(frame: &[u8]) -> Result<Self, String> {
        if frame.len() < FRAME_PREFIX_LEN {
            return Err("portmap wire frame too short".to_string());
        }
        if frame.len() > MAX_PORTMAP_WIRE_FRAME_BYTES {
            return Err(format!(
                "portmap wire frame exceeds {MAX_PORTMAP_WIRE_FRAME_BYTES} bytes"
            ));
        }

        let kind = frame[0];
        let hdr_len = u32::from_le_bytes([frame[1], frame[2], frame[3], frame[4]]) as usize;
        let hdr_end = FRAME_PREFIX_LEN
            .checked_add(hdr_len)
            .ok_or_else(|| "portmap header length overflow".to_string())?;
        if hdr_end > frame.len() {
            return Err("portmap header length out of bounds".to_string());
        }

        let hdr_bytes = &frame[FRAME_PREFIX_LEN..hdr_end];
        let raw_bytes = &frame[hdr_end..];

        match kind {
            FRAME_KIND_CONTROL => {
                if !raw_bytes.is_empty() {
                    return Err(
                        "portmap control frame must not contain trailing payload".to_string()
                    );
                }
                let ctrl: PortMapControlPayload = serde_json::from_slice(hdr_bytes)
                    .map_err(|e| format!("invalid portmap control json: {e}"))?;
                Ok(match ctrl {
                    PortMapControlPayload::Open {
                        conn_id,
                        rule_id,
                        target_addr,
                        target_port,
                    } => PortMapWireMsg::ConnOpen {
                        conn_id,
                        rule_id,
                        target_addr,
                        target_port,
                    },
                    PortMapControlPayload::Ready { conn_id, rule_id } => {
                        PortMapWireMsg::ConnReady { conn_id, rule_id }
                    }
                    PortMapControlPayload::Error {
                        conn_id,
                        rule_id,
                        message,
                    } => PortMapWireMsg::ConnError {
                        conn_id,
                        rule_id,
                        message,
                    },
                    PortMapControlPayload::Close { conn_id } => {
                        PortMapWireMsg::ConnClose { conn_id }
                    }
                    PortMapControlPayload::HalfClose { conn_id } => {
                        PortMapWireMsg::ConnHalfClose { conn_id }
                    }
                    PortMapControlPayload::Window {
                        conn_id,
                        delta_bytes,
                    } => PortMapWireMsg::ConnWindow {
                        conn_id,
                        delta_bytes,
                    },
                })
            }
            FRAME_KIND_DATA => {
                if raw_bytes.len() > PORTMAP_CHUNK_SIZE {
                    return Err(format!(
                        "portmap data chunk exceeds {PORTMAP_CHUNK_SIZE} bytes"
                    ));
                }
                let hdr: PortMapDataHeader = serde_json::from_slice(hdr_bytes)
                    .map_err(|e| format!("invalid portmap data header: {e}"))?;
                Ok(PortMapWireMsg::ConnData {
                    conn_id: hdr.conn_id,
                    data: raw_bytes.to_vec(),
                })
            }
            _ => Err(format!("unknown portmap wire frame kind: {kind}")),
        }
    }
}

enum HostWorkerEvent {
    Ready {
        conn_id: u64,
        rule_id: u64,
    },
    Error {
        conn_id: u64,
        rule_id: u64,
        message: String,
    },
    Data {
        conn_id: u64,
        data: Vec<u8>,
    },
    HalfClose {
        conn_id: u64,
    },
    Window {
        conn_id: u64,
        delta_bytes: u32,
    },
    Closed {
        conn_id: u64,
    },
}

struct ReapedHostWorker {
    abort: Arc<AtomicBool>,
    send_window: Arc<SendWindow>,
    stream_slot: Arc<Mutex<Option<TcpStream>>>,
    handle: JoinHandle<()>,
}

impl ReapedHostWorker {
    fn force_abort_and_join(self) {
        if !self.handle.is_finished() {
            self.abort.store(true, Ordering::Release);
            self.send_window.notify_all();
            if let Ok(mut slot) = self.stream_slot.lock() {
                if let Some(stream) = slot.take() {
                    let _ = stream.shutdown(Shutdown::Both);
                }
            }
        }
        let _ = self.handle.join();
    }
}

struct HostConn {
    rule_id: u64,
    abort: Arc<AtomicBool>,
    send_window: Arc<SendWindow>,
    stream_slot: Arc<Mutex<Option<TcpStream>>>,
    write_tx: Option<SyncSender<Vec<u8>>>,
    main_thread: Option<JoinHandle<()>>,
    local_eof: bool,
    remote_eof: bool,
}

impl HostConn {
    fn abort_and_reap(&mut self, reaper: &mut Vec<ReapedHostWorker>) {
        self.abort.store(true, Ordering::Release);
        self.send_window.notify_all();
        self.write_tx.take();
        if let Ok(mut slot) = self.stream_slot.lock() {
            if let Some(stream) = slot.take() {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
        if let Some(handle) = self.main_thread.take() {
            reaper.push(ReapedHostWorker {
                abort: Arc::clone(&self.abort),
                send_window: Arc::clone(&self.send_window),
                stream_slot: Arc::clone(&self.stream_slot),
                handle,
            });
        }
    }

    fn finish_graceful_and_reap(&mut self, reaper: &mut Vec<ReapedHostWorker>) {
        self.write_tx.take();
        if let Some(handle) = self.main_thread.take() {
            reaper.push(ReapedHostWorker {
                abort: Arc::clone(&self.abort),
                send_window: Arc::clone(&self.send_window),
                stream_slot: Arc::clone(&self.stream_slot),
                handle,
            });
        }
    }
}

/// 被控端（Host）TCP 端口映射代理引擎。
///
/// 按需向 `target_addr:target_port` 发起带 5 秒总超时与 `TCP_NODELAY` 的出站连接，
/// 最多并发承载 [`MAX_HOST_PROXY_CONNS`]（32）条代理连接（含仍在建连中的工作线程）。
pub struct PortMapHostEngine {
    outgoing: VecDeque<Vec<u8>>,
    conns: BTreeMap<u64, HostConn>,
    live_workers: Arc<AtomicUsize>,
    reaper: Vec<ReapedHostWorker>,
    event_tx: SyncSender<HostWorkerEvent>,
    event_rx: Option<Receiver<HostWorkerEvent>>,
}

impl Default for PortMapHostEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl PortMapHostEngine {
    /// 创建新的被控端 TCP 端口映射引擎。
    pub fn new() -> Self {
        let (event_tx, event_rx) = mpsc::sync_channel(EVENT_CHANNEL_CAP);
        Self {
            outgoing: VecDeque::new(),
            conns: BTreeMap::new(),
            live_workers: Arc::new(AtomicUsize::new(0)),
            reaper: Vec::new(),
            event_tx,
            event_rx: Some(event_rx),
        }
    }

    /// 返回当前活跃的代理连接表项数量。
    pub fn active_conn_count(&self) -> usize {
        self.conns.len()
    }

    /// 返回当前存活的代理工作线程总数（含建连中与已转入后台回收队列的线程）。
    pub fn live_worker_count(&self) -> usize {
        self.live_workers.load(Ordering::Acquire)
    }

    fn prune_reaper(&mut self) {
        self.reaper.retain(|w| !w.handle.is_finished());
    }

    fn enqueue(&mut self, msg: &PortMapWireMsg) {
        if let Ok(frame) = msg.encode() {
            self.outgoing.push_back(frame);
        }
    }

    /// 处理一条来自控制端的端口映射线帧。
    pub fn handle_incoming(&mut self, frame: &[u8]) {
        self.drain_worker_events();
        let msg = match PortMapWireMsg::decode(frame) {
            Ok(m) => m,
            Err(_) => return,
        };

        match msg {
            PortMapWireMsg::ConnOpen {
                conn_id,
                rule_id,
                target_addr,
                target_port,
            } => {
                self.handle_conn_open(conn_id, rule_id, target_addr, target_port);
            }
            PortMapWireMsg::ConnData { conn_id, data } => {
                let mut stalled_reason: Option<Option<&'static str>> = None;
                if let Some(conn) = self.conns.get(&conn_id) {
                    if let Some(tx) = &conn.write_tx {
                        match tx.try_send(data) {
                            Ok(()) => {}
                            Err(TrySendError::Disconnected(_)) => {
                                stalled_reason = Some(None);
                            }
                            Err(TrySendError::Full(_)) => {
                                stalled_reason = Some(Some("proxy write queue full"));
                            }
                        }
                    }
                }
                if let Some(err_msg) = stalled_reason {
                    if let Some(mut conn) = self.conns.remove(&conn_id) {
                        let rule_id = conn.rule_id;
                        conn.abort_and_reap(&mut self.reaper);
                        if let Some(msg) = err_msg {
                            self.enqueue(&PortMapWireMsg::ConnError {
                                conn_id,
                                rule_id,
                                message: msg.to_string(),
                            });
                        }
                    }
                    self.enqueue(&PortMapWireMsg::ConnClose { conn_id });
                }
            }
            PortMapWireMsg::ConnHalfClose { conn_id } => {
                let mut both_eof = false;
                if let Some(conn) = self.conns.get_mut(&conn_id) {
                    conn.remote_eof = true;
                    // 仅丢弃 write_tx 让写线程刷完缓冲后调用 Shutdown::Write，绝不硬中止。
                    conn.write_tx.take();
                    both_eof = conn.local_eof && conn.remote_eof;
                }
                if both_eof {
                    if let Some(mut conn) = self.conns.remove(&conn_id) {
                        conn.finish_graceful_and_reap(&mut self.reaper);
                    }
                }
            }
            PortMapWireMsg::ConnWindow {
                conn_id,
                delta_bytes,
            } => {
                if delta_bytes > 0 {
                    if let Some(conn) = self.conns.get(&conn_id) {
                        conn.send_window.add_credit(delta_bytes);
                    }
                }
            }
            PortMapWireMsg::ConnClose { conn_id } => {
                if let Some(mut conn) = self.conns.remove(&conn_id) {
                    conn.abort_and_reap(&mut self.reaper);
                }
            }
            // Host 忽略下行控制帧。
            PortMapWireMsg::ConnReady { .. } | PortMapWireMsg::ConnError { .. } => {}
        }
    }

    fn handle_conn_open(
        &mut self,
        conn_id: u64,
        rule_id: u64,
        target_addr: String,
        target_port: u16,
    ) {
        if self.conns.contains_key(&conn_id) {
            self.enqueue(&PortMapWireMsg::ConnError {
                conn_id,
                rule_id,
                message: format!("duplicate conn_id {conn_id}"),
            });
            return;
        }
        if !validate_target_addr(&target_addr) || target_port == 0 {
            self.enqueue(&PortMapWireMsg::ConnError {
                conn_id,
                rule_id,
                message: "invalid target address or port".to_string(),
            });
            return;
        }
        if self.conns.len() >= MAX_HOST_PROXY_CONNS {
            self.enqueue(&PortMapWireMsg::ConnError {
                conn_id,
                rule_id,
                message: format!(
                    "maximum concurrent proxy connections ({MAX_HOST_PROXY_CONNS}) reached"
                ),
            });
            return;
        }
        let reserved = self
            .live_workers
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                if cur < MAX_HOST_PROXY_CONNS {
                    Some(cur + 1)
                } else {
                    None
                }
            });
        if reserved.is_err() {
            self.enqueue(&PortMapWireMsg::ConnError {
                conn_id,
                rule_id,
                message: format!(
                    "maximum concurrent proxy connections ({MAX_HOST_PROXY_CONNS}) reached"
                ),
            });
            return;
        }
        let worker_guard = WorkerCountGuard(Arc::clone(&self.live_workers));

        let abort = Arc::new(AtomicBool::new(false));
        let send_window = Arc::new(SendWindow::new(INITIAL_CONN_WINDOW_BYTES));
        let stream_slot = Arc::new(Mutex::new(None));
        let (write_tx, write_rx) = mpsc::sync_channel::<Vec<u8>>(WRITE_CHANNEL_CAP);

        let w_abort = Arc::clone(&abort);
        let w_window = Arc::clone(&send_window);
        let w_slot = Arc::clone(&stream_slot);
        let w_event_tx = self.event_tx.clone();

        let thread_res = thread::Builder::new()
            .name(format!("rc-pm-host-{conn_id}"))
            .spawn(move || {
                host_conn_worker(
                    conn_id,
                    rule_id,
                    &target_addr,
                    target_port,
                    w_abort,
                    w_window,
                    w_slot,
                    write_rx,
                    w_event_tx,
                    worker_guard,
                );
            });

        match thread_res {
            Ok(handle) => {
                self.conns.insert(
                    conn_id,
                    HostConn {
                        rule_id,
                        abort,
                        send_window,
                        stream_slot,
                        write_tx: Some(write_tx),
                        main_thread: Some(handle),
                        local_eof: false,
                        remote_eof: false,
                    },
                );
            }
            Err(e) => {
                self.enqueue(&PortMapWireMsg::ConnError {
                    conn_id,
                    rule_id,
                    message: format!("failed to spawn proxy worker: {e}"),
                });
            }
        }
    }

    fn drain_worker_events(&mut self) {
        self.prune_reaper();
        let Some(rx) = self.event_rx.take() else {
            return;
        };

        while self.outgoing.len() < MAX_OUTGOING_FRAMES {
            let Ok(ev) = rx.try_recv() else {
                break;
            };
            match ev {
                HostWorkerEvent::Ready { conn_id, rule_id } => {
                    if self.conns.contains_key(&conn_id) {
                        self.enqueue(&PortMapWireMsg::ConnReady { conn_id, rule_id });
                    }
                }
                HostWorkerEvent::Error {
                    conn_id,
                    rule_id,
                    message,
                } => {
                    if let Some(mut conn) = self.conns.remove(&conn_id) {
                        conn.abort_and_reap(&mut self.reaper);
                        self.enqueue(&PortMapWireMsg::ConnError {
                            conn_id,
                            rule_id,
                            message,
                        });
                    }
                }
                HostWorkerEvent::Data { conn_id, data } => {
                    if self.conns.contains_key(&conn_id) {
                        self.enqueue(&PortMapWireMsg::ConnData { conn_id, data });
                    }
                }
                HostWorkerEvent::HalfClose { conn_id } => {
                    let mut should_send = false;
                    let mut both_eof = false;
                    if let Some(conn) = self.conns.get_mut(&conn_id) {
                        if !conn.local_eof {
                            conn.local_eof = true;
                            should_send = true;
                            both_eof = conn.local_eof && conn.remote_eof;
                        }
                    }
                    if should_send {
                        self.enqueue(&PortMapWireMsg::ConnHalfClose { conn_id });
                    }
                    if both_eof {
                        if let Some(mut conn) = self.conns.remove(&conn_id) {
                            conn.finish_graceful_and_reap(&mut self.reaper);
                        }
                    }
                }
                HostWorkerEvent::Window {
                    conn_id,
                    delta_bytes,
                } => {
                    if self.conns.contains_key(&conn_id) {
                        self.enqueue(&PortMapWireMsg::ConnWindow {
                            conn_id,
                            delta_bytes,
                        });
                    }
                }
                HostWorkerEvent::Closed { conn_id } => {
                    if let Some(mut conn) = self.conns.remove(&conn_id) {
                        conn.abort_and_reap(&mut self.reaper);
                        self.enqueue(&PortMapWireMsg::ConnClose { conn_id });
                    }
                }
            }
        }

        self.event_rx = Some(rx);
    }

    /// 拉取最多 `max_frames` 个待发线帧。
    pub fn poll_outgoing(&mut self, max_frames: usize) -> Vec<Vec<u8>> {
        self.drain_worker_events();
        if max_frames == 0 {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(max_frames.min(self.outgoing.len()));
        while out.len() < max_frames {
            let Some(frame) = self.outgoing.pop_front() else {
                break;
            };
            out.push(frame);
        }
        out
    }
}

impl Drop for PortMapHostEngine {
    fn drop(&mut self) {
        // 先丢弃接收端，确保任何阻塞在 `event_tx.send` 的工作线程立刻唤醒退出。
        self.event_rx.take();
        let conns = std::mem::take(&mut self.conns);
        for (_, mut conn) in conns {
            conn.abort_and_reap(&mut self.reaper);
        }
        for worker in std::mem::take(&mut self.reaper) {
            worker.force_abort_and_join();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn host_conn_worker(
    conn_id: u64,
    rule_id: u64,
    target_addr: &str,
    target_port: u16,
    abort: Arc<AtomicBool>,
    send_window: Arc<SendWindow>,
    stream_slot: Arc<Mutex<Option<TcpStream>>>,
    write_rx: Receiver<Vec<u8>>,
    event_tx: SyncSender<HostWorkerEvent>,
    _worker_guard: WorkerCountGuard,
) {
    let connect_res = connect_target_with_timeout(target_addr, target_port, &abort);

    let stream = match connect_res {
        Ok(s) => s,
        Err(err) => {
            if !abort.load(Ordering::Acquire) {
                let _ = send_cancellable(
                    &event_tx,
                    HostWorkerEvent::Error {
                        conn_id,
                        rule_id,
                        message: err,
                    },
                    &abort,
                );
            }
            return;
        }
    };

    if abort.load(Ordering::Acquire) {
        let _ = stream.shutdown(Shutdown::Both);
        return;
    }

    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(IO_POLL_TIMEOUT));

    let Ok(writer_stream) = stream.try_clone() else {
        let _ = stream.shutdown(Shutdown::Both);
        let _ = send_cancellable(
            &event_tx,
            HostWorkerEvent::Error {
                conn_id,
                rule_id,
                message: "failed to clone proxy TcpStream".to_string(),
            },
            &abort,
        );
        return;
    };
    let _ = writer_stream.set_write_timeout(Some(TCP_WRITE_TIMEOUT));

    if let Ok(mut slot) = stream_slot.lock() {
        if let Ok(shutdown_clone) = stream.try_clone() {
            *slot = Some(shutdown_clone);
        }
    }

    let writer_abort = Arc::clone(&abort);
    let writer_event_tx = event_tx.clone();
    let writer_handle = match thread::Builder::new()
        .name(format!("rc-pm-hw-{conn_id}"))
        .spawn(move || {
            let writer_abort_cb = Arc::clone(&writer_abort);
            let clean = tcp_writer_loop(writer_stream, write_rx, writer_abort, |delta_bytes| {
                send_cancellable(
                    &writer_event_tx,
                    HostWorkerEvent::Window {
                        conn_id,
                        delta_bytes,
                    },
                    &writer_abort_cb,
                )
            });
            if !clean && !writer_abort_cb.load(Ordering::Acquire) {
                let _ = send_cancellable(
                    &writer_event_tx,
                    HostWorkerEvent::Closed { conn_id },
                    &writer_abort_cb,
                );
            }
        }) {
        Ok(h) => h,
        Err(e) => {
            let _ = stream.shutdown(Shutdown::Both);
            let _ = send_cancellable(
                &event_tx,
                HostWorkerEvent::Error {
                    conn_id,
                    rule_id,
                    message: format!("failed to spawn proxy writer thread: {e}"),
                },
                &abort,
            );
            return;
        }
    };

    if !send_cancellable(
        &event_tx,
        HostWorkerEvent::Ready { conn_id, rule_id },
        &abort,
    ) {
        abort.store(true, Ordering::Release);
        send_window.notify_all();
        let _ = stream.shutdown(Shutdown::Both);
        let _ = writer_handle.join();
        return;
    }

    let mut read_stream = stream;
    let mut buf = vec![0u8; PORTMAP_CHUNK_SIZE];
    let mut clean_eof = false;
    while !abort.load(Ordering::Acquire) {
        let budget = send_window.wait_for_budget(PORTMAP_CHUNK_SIZE, &abort);
        if budget == 0 {
            break;
        }
        match read_stream.read(&mut buf[..budget]) {
            Ok(0) => {
                clean_eof = true;
                break;
            }
            Ok(n) => {
                send_window.consume(n);
                if !send_cancellable(
                    &event_tx,
                    HostWorkerEvent::Data {
                        conn_id,
                        data: buf[..n].to_vec(),
                    },
                    &abort,
                ) {
                    break;
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) => {}
            Err(_) => break,
        }
    }

    if clean_eof && !abort.load(Ordering::Acquire) {
        let _ = send_cancellable(&event_tx, HostWorkerEvent::HalfClose { conn_id }, &abort);
        let _ = writer_handle.join();
    } else {
        let _ = read_stream.shutdown(Shutdown::Both);
        if !abort.load(Ordering::Acquire) {
            let _ = send_cancellable(&event_tx, HostWorkerEvent::Closed { conn_id }, &abort);
        }
        abort.store(true, Ordering::Release);
        let _ = writer_handle.join();
    }
}

fn connect_target_with_timeout(
    target_addr: &str,
    target_port: u16,
    stop: &AtomicBool,
) -> Result<TcpStream, String> {
    let deadline = Instant::now() + HOST_CONNECT_TIMEOUT;
    let addrs: Vec<_> = (target_addr, target_port)
        .to_socket_addrs()
        .map_err(|e| format!("failed to resolve {target_addr}:{target_port}: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!(
            "no socket addresses resolved for {target_addr}:{target_port}"
        ));
    }

    let mut last_err = String::new();
    for addr in addrs {
        if stop.load(Ordering::Acquire) {
            return Err("connection cancelled".to_string());
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(format!(
                "connect {target_addr}:{target_port} timed out after {:?}",
                HOST_CONNECT_TIMEOUT
            ));
        };
        if remaining.is_zero() {
            return Err(format!(
                "connect {target_addr}:{target_port} timed out after {:?}",
                HOST_CONNECT_TIMEOUT
            ));
        }
        match TcpStream::connect_timeout(&addr, remaining) {
            Ok(s) => return Ok(s),
            Err(e) => last_err = format!("connect {addr} failed: {e}"),
        }
    }
    Err(last_err)
}

fn tcp_writer_loop<F>(
    mut stream: TcpStream,
    write_rx: Receiver<Vec<u8>>,
    abort: Arc<AtomicBool>,
    mut on_window_credit: F,
) -> bool
where
    F: FnMut(u32) -> bool,
{
    let mut write_failed = false;
    let mut pending_credit: usize = 0;

    while let Ok(chunk) = write_rx.recv() {
        if abort.load(Ordering::Acquire) {
            let _ = stream.shutdown(Shutdown::Both);
            return false;
        }
        if stream.write_all(&chunk).is_err() {
            write_failed = true;
            break;
        }
        pending_credit = pending_credit.saturating_add(chunk.len());
        while pending_credit >= CONN_WINDOW_UPDATE_THRESHOLD {
            let grant = pending_credit.min(u32::MAX as usize) as u32;
            pending_credit -= grant as usize;
            if !on_window_credit(grant) {
                let _ = stream.shutdown(Shutdown::Both);
                return false;
            }
        }
    }

    if abort.load(Ordering::Acquire) || write_failed {
        let _ = stream.shutdown(Shutdown::Both);
        false
    } else {
        // 写通道被优雅关闭（ConnHalfClose），所有剩余数据块均已刷盘，发送 TCP FIN。
        let _ = stream.shutdown(Shutdown::Write);
        true
    }
}

enum ClientListenerEvent {
    Accepted { rule_id: u64, stream: TcpStream },
}

enum ClientConnEvent {
    Data { conn_id: u64, data: Vec<u8> },
    HalfClose { conn_id: u64 },
    Window { conn_id: u64, delta_bytes: u32 },
    Closed { conn_id: u64 },
}

struct ReapedClientWorker {
    abort: Arc<AtomicBool>,
    send_window: Arc<SendWindow>,
    shutdown_stream: Option<TcpStream>,
    handle: JoinHandle<()>,
}

impl ReapedClientWorker {
    fn force_abort_and_join(self) {
        if !self.handle.is_finished() {
            self.abort.store(true, Ordering::Release);
            self.send_window.notify_all();
            if let Some(stream) = self.shutdown_stream {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
        let _ = self.handle.join();
    }
}

struct ListenerHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ListenerHandle {
    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

struct ClientRule {
    snapshot: PortMapRuleSnapshot,
    listener: Option<ListenerHandle>,
}

enum ClientConnState {
    Pending {
        stream: TcpStream,
        created_at: Instant,
    },
    Active {
        abort: Arc<AtomicBool>,
        send_window: Arc<SendWindow>,
        shutdown_stream: TcpStream,
        write_tx: Option<SyncSender<Vec<u8>>>,
        reader_thread: Option<JoinHandle<()>>,
        writer_thread: Option<JoinHandle<()>>,
        local_eof: bool,
        remote_eof: bool,
    },
}

struct ClientConn {
    rule_id: u64,
    state: ClientConnState,
}

impl ClientConn {
    fn abort_and_reap(&mut self, reaper: &mut Vec<ReapedClientWorker>) {
        match &mut self.state {
            ClientConnState::Pending { stream, .. } => {
                let _ = stream.shutdown(Shutdown::Both);
            }
            ClientConnState::Active {
                abort,
                send_window,
                shutdown_stream,
                write_tx,
                reader_thread,
                writer_thread,
                ..
            } => {
                abort.store(true, Ordering::Release);
                send_window.notify_all();
                write_tx.take();
                let _ = shutdown_stream.shutdown(Shutdown::Both);
                if let Some(handle) = reader_thread.take() {
                    reaper.push(ReapedClientWorker {
                        abort: Arc::clone(abort),
                        send_window: Arc::clone(send_window),
                        shutdown_stream: shutdown_stream.try_clone().ok(),
                        handle,
                    });
                }
                if let Some(handle) = writer_thread.take() {
                    reaper.push(ReapedClientWorker {
                        abort: Arc::clone(abort),
                        send_window: Arc::clone(send_window),
                        shutdown_stream: shutdown_stream.try_clone().ok(),
                        handle,
                    });
                }
            }
        }
    }

    fn finish_graceful_and_reap(&mut self, reaper: &mut Vec<ReapedClientWorker>) {
        match &mut self.state {
            ClientConnState::Pending { stream, .. } => {
                let _ = stream.shutdown(Shutdown::Both);
            }
            ClientConnState::Active {
                abort,
                send_window,
                shutdown_stream,
                write_tx,
                reader_thread,
                writer_thread,
                ..
            } => {
                // 不设 abort、不调 Shutdown::Both，允许写线程刷完 write_rx 中所有尾部字节后发送 FIN。
                write_tx.take();
                if let Some(handle) = reader_thread.take() {
                    reaper.push(ReapedClientWorker {
                        abort: Arc::clone(abort),
                        send_window: Arc::clone(send_window),
                        shutdown_stream: shutdown_stream.try_clone().ok(),
                        handle,
                    });
                }
                if let Some(handle) = writer_thread.take() {
                    reaper.push(ReapedClientWorker {
                        abort: Arc::clone(abort),
                        send_window: Arc::clone(send_window),
                        shutdown_stream: shutdown_stream.try_clone().ok(),
                        handle,
                    });
                }
            }
        }
    }
}

/// 控制端（Client）TCP 端口映射引擎。
///
/// 管理端口映射规则生命周期、本地 `127.0.0.1:<local_port>` 监听器、活跃连接及流量统计。
/// 析构时自动关闭全部监听线程与代理连接线程。
pub struct PortMapClientEngine {
    next_rule_id: u64,
    next_conn_id: u64,
    rules: BTreeMap<u64, ClientRule>,
    conns: BTreeMap<u64, ClientConn>,
    outgoing: VecDeque<Vec<u8>>,
    pending_accepted: VecDeque<ClientListenerEvent>,
    reaper: Vec<ReapedClientWorker>,
    listener_tx: SyncSender<ClientListenerEvent>,
    listener_rx: Option<Receiver<ClientListenerEvent>>,
    conn_tx: SyncSender<ClientConnEvent>,
    conn_rx: Option<Receiver<ClientConnEvent>>,
}

impl Default for PortMapClientEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl PortMapClientEngine {
    /// 创建新的控制端 TCP 端口映射引擎。
    pub fn new() -> Self {
        let (listener_tx, listener_rx) = mpsc::sync_channel(128);
        let (conn_tx, conn_rx) = mpsc::sync_channel(EVENT_CHANNEL_CAP);
        Self {
            next_rule_id: 1,
            next_conn_id: 1,
            rules: BTreeMap::new(),
            conns: BTreeMap::new(),
            outgoing: VecDeque::new(),
            pending_accepted: VecDeque::new(),
            reaper: Vec::new(),
            listener_tx,
            listener_rx: Some(listener_rx),
            conn_tx,
            conn_rx: Some(conn_rx),
        }
    }

    fn prune_reaper(&mut self) {
        self.reaper.retain(|w| !w.handle.is_finished());
    }

    fn enqueue(&mut self, msg: &PortMapWireMsg) {
        if let Ok(frame) = msg.encode() {
            self.outgoing.push_back(frame);
        }
    }

    /// 添加一条端口映射规则并立即尝试绑定 `127.0.0.1:<local_port>`。
    ///
    /// 参数校验失败（含本地端口已被已有规则占用）返回 `Err`；若参数合法但本地端口被外部进程占用，
    /// 仍会记录该规则且状态置为 [`PortMapStatus::Fail`] 并写入具体 `error`。
    pub fn add_rule(
        &mut self,
        name: String,
        local_port: u16,
        target_addr: String,
        target_port: u16,
    ) -> Result<PortMapRuleSnapshot, String> {
        self.add_rule_with_enabled(name, local_port, target_addr, target_port, true)
    }

    /// 添加一条端口映射规则并指定初始启用状态；`enabled == false` 时仅登记规则而不绑定本地端口。
    pub fn add_rule_with_enabled(
        &mut self,
        name: String,
        local_port: u16,
        target_addr: String,
        target_port: u16,
        enabled: bool,
    ) -> Result<PortMapRuleSnapshot, String> {
        let trimmed_name = name.trim();
        if trimmed_name.is_empty() {
            return Err("rule name must not be empty".to_string());
        }
        if local_port == 0 {
            return Err("local_port must be in 1..=65535".to_string());
        }
        if self
            .rules
            .values()
            .any(|r| r.snapshot.local_port == local_port)
        {
            return Err("本地端口已被其他规则占用".to_string());
        }
        if !validate_target_addr(&target_addr) {
            return Err("target_addr is invalid".to_string());
        }
        if target_port == 0 {
            return Err("target_port must be in 1..=65535".to_string());
        }

        let id = self.next_rule_id;
        self.next_rule_id = self.next_rule_id.saturating_add(1);

        let (listener, status, error) = if enabled {
            match self.spawn_listener(id, local_port) {
                Ok(handle) => (Some(handle), PortMapStatus::Ok, None),
                Err(err) => (None, PortMapStatus::Fail, Some(err)),
            }
        } else {
            (None, PortMapStatus::Pending, None)
        };

        let snapshot = PortMapRuleSnapshot {
            id,
            name: trimmed_name.to_string(),
            local_port,
            target_addr,
            target_port,
            enabled,
            status,
            active_conns: 0,
            bytes_tx: 0,
            bytes_rx: 0,
            error,
        };
        let ret = snapshot.clone();
        self.rules.insert(id, ClientRule { snapshot, listener });
        Ok(ret)
    }

    /// 启用或禁用指定规则；禁用时停止本地监听、丢弃该规则积压连接并关闭所有活跃连接。
    pub fn set_rule_enabled(
        &mut self,
        id: u64,
        enabled: bool,
    ) -> Result<PortMapRuleSnapshot, String> {
        if !self.rules.contains_key(&id) {
            return Err(format!("portmap rule {id} not found"));
        }

        if !enabled {
            if let Some(rule) = self.rules.get_mut(&id) {
                if let Some(mut l) = rule.listener.take() {
                    l.stop_and_join();
                }
                rule.snapshot.enabled = false;
                rule.snapshot.status = PortMapStatus::Pending;
                rule.snapshot.error = None;
            }
            self.discard_queued_accepted_for_rule(id);
            self.close_rule_connections(id);
            return Ok(self.rules[&id].snapshot.clone());
        }

        let (local_port, already_listening) = {
            let rule = &self.rules[&id];
            (rule.snapshot.local_port, rule.listener.is_some())
        };

        if !already_listening {
            match self.spawn_listener(id, local_port) {
                Ok(handle) => {
                    if let Some(rule) = self.rules.get_mut(&id) {
                        rule.listener = Some(handle);
                        rule.snapshot.enabled = true;
                        rule.snapshot.status = PortMapStatus::Ok;
                        rule.snapshot.error = None;
                    }
                }
                Err(err) => {
                    if let Some(rule) = self.rules.get_mut(&id) {
                        rule.snapshot.enabled = true;
                        rule.snapshot.status = PortMapStatus::Fail;
                        rule.snapshot.error = Some(err);
                    }
                }
            }
        } else if let Some(rule) = self.rules.get_mut(&id) {
            rule.snapshot.enabled = true;
            rule.snapshot.status = PortMapStatus::Ok;
            rule.snapshot.error = None;
        }

        Ok(self.rules[&id].snapshot.clone())
    }

    /// 删除指定规则，释放本地监听端口、丢弃积压连接并关闭所有关联连接。
    pub fn delete_rule(&mut self, id: u64) -> Result<(), String> {
        let Some(mut rule) = self.rules.remove(&id) else {
            return Err(format!("portmap rule {id} not found"));
        };
        if let Some(mut l) = rule.listener.take() {
            l.stop_and_join();
        }
        self.discard_queued_accepted_for_rule(id);
        self.close_rule_connections(id);
        Ok(())
    }

    /// 列出全部端口映射规则快照（按 ID 升序）。
    pub fn list_rules(&self) -> Vec<PortMapRuleSnapshot> {
        self.rules.values().map(|r| r.snapshot.clone()).collect()
    }

    fn discard_queued_accepted_for_rule(&mut self, rule_id: u64) {
        if let Some(rx) = &self.listener_rx {
            while let Ok(ev) = rx.try_recv() {
                self.pending_accepted.push_back(ev);
            }
        }
        let mut kept = VecDeque::with_capacity(self.pending_accepted.len());
        while let Some(ev) = self.pending_accepted.pop_front() {
            match ev {
                ClientListenerEvent::Accepted {
                    rule_id: rid,
                    stream,
                } if rid == rule_id => {
                    let _ = stream.shutdown(Shutdown::Both);
                }
                other => kept.push_back(other),
            }
        }
        self.pending_accepted = kept;
    }

    fn spawn_listener(&self, rule_id: u64, local_port: u16) -> Result<ListenerHandle, String> {
        let listener = TcpListener::bind(("127.0.0.1", local_port))
            .map_err(|e| format!("bind 127.0.0.1:{local_port} failed: {e}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("set_nonblocking on 127.0.0.1:{local_port} failed: {e}"))?;

        let stop = Arc::new(AtomicBool::new(false));
        let l_stop = Arc::clone(&stop);
        let tx = self.listener_tx.clone();

        let thread = thread::Builder::new()
            .name(format!("rc-pm-listen-{rule_id}"))
            .spawn(move || {
                while !l_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _peer)) => {
                            let _ = stream.set_nodelay(true);
                            if !send_cancellable(
                                &tx,
                                ClientListenerEvent::Accepted { rule_id, stream },
                                &l_stop,
                            ) {
                                break;
                            }
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {
                            thread::sleep(LISTENER_POLL_INTERVAL);
                        }
                        Err(e) if e.kind() == ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
            })
            .map_err(|e| format!("spawn listener thread failed: {e}"))?;

        Ok(ListenerHandle {
            stop,
            thread: Some(thread),
        })
    }

    fn close_rule_connections(&mut self, rule_id: u64) {
        let ids: Vec<u64> = self
            .conns
            .iter()
            .filter(|(_, c)| c.rule_id == rule_id)
            .map(|(&cid, _)| cid)
            .collect();
        for cid in ids {
            if let Some(mut conn) = self.conns.remove(&cid) {
                conn.abort_and_reap(&mut self.reaper);
            }
            self.enqueue(&PortMapWireMsg::ConnClose { conn_id: cid });
        }
        if let Some(rule) = self.rules.get_mut(&rule_id) {
            rule.snapshot.active_conns = 0;
        }
    }

    fn expire_stale_pending_conns(&mut self) -> bool {
        let expired: Vec<u64> = self
            .conns
            .iter()
            .filter_map(|(&cid, conn)| match &conn.state {
                ClientConnState::Pending { created_at, .. }
                    if created_at.elapsed() >= CLIENT_PENDING_TIMEOUT =>
                {
                    Some(cid)
                }
                _ => None,
            })
            .collect();

        if expired.is_empty() {
            return false;
        }

        for cid in expired {
            if let Some(mut conn) = self.conns.remove(&cid) {
                let rule_id = conn.rule_id;
                conn.abort_and_reap(&mut self.reaper);
                if let Some(rule) = self.rules.get_mut(&rule_id) {
                    rule.snapshot.active_conns = rule.snapshot.active_conns.saturating_sub(1);
                }
                self.enqueue(&PortMapWireMsg::ConnClose { conn_id: cid });
            }
        }
        true
    }

    fn drain_internal_events(&mut self) -> bool {
        self.prune_reaper();
        let mut changed = self.expire_stale_pending_conns();

        while self.outgoing.len() < MAX_OUTGOING_FRAMES {
            let ev = if let Some(ev) = self.pending_accepted.pop_front() {
                ev
            } else if let Some(rx) = &self.listener_rx {
                match rx.try_recv() {
                    Ok(ev) => ev,
                    Err(_) => break,
                }
            } else {
                break;
            };

            let ClientListenerEvent::Accepted { rule_id, stream } = ev;
            let pending_count = self
                .conns
                .values()
                .filter(|c| matches!(c.state, ClientConnState::Pending { .. }))
                .count();
            if pending_count >= MAX_CLIENT_PENDING_CONNS {
                let _ = stream.shutdown(Shutdown::Both);
                continue;
            }

            let target_info = self.rules.get_mut(&rule_id).and_then(|rule| {
                if !rule.snapshot.enabled {
                    return None;
                }
                rule.snapshot.active_conns = rule.snapshot.active_conns.saturating_add(1);
                Some((rule.snapshot.target_addr.clone(), rule.snapshot.target_port))
            });
            let Some((target_addr, target_port)) = target_info else {
                let _ = stream.shutdown(Shutdown::Both);
                continue;
            };

            let conn_id = self.next_conn_id;
            self.next_conn_id = self.next_conn_id.saturating_add(1);
            self.conns.insert(
                conn_id,
                ClientConn {
                    rule_id,
                    state: ClientConnState::Pending {
                        stream,
                        created_at: Instant::now(),
                    },
                },
            );
            self.enqueue(&PortMapWireMsg::ConnOpen {
                conn_id,
                rule_id,
                target_addr,
                target_port,
            });
            changed = true;
        }

        if let Some(rx) = self.conn_rx.take() {
            while self.outgoing.len() < MAX_OUTGOING_FRAMES {
                let Ok(ev) = rx.try_recv() else {
                    break;
                };
                match ev {
                    ClientConnEvent::Data { conn_id, data } => {
                        if let Some(conn) = self.conns.get(&conn_id) {
                            let rule_id = conn.rule_id;
                            if let Some(rule) = self.rules.get_mut(&rule_id) {
                                rule.snapshot.bytes_tx =
                                    rule.snapshot.bytes_tx.saturating_add(data.len() as u64);
                                changed = true;
                            }
                            self.enqueue(&PortMapWireMsg::ConnData { conn_id, data });
                        }
                    }
                    ClientConnEvent::HalfClose { conn_id } => {
                        let mut should_send = false;
                        let mut both_eof = false;
                        if let Some(conn) = self.conns.get_mut(&conn_id) {
                            if let ClientConnState::Active {
                                local_eof,
                                remote_eof,
                                ..
                            } = &mut conn.state
                            {
                                if !*local_eof {
                                    *local_eof = true;
                                    should_send = true;
                                    both_eof = *local_eof && *remote_eof;
                                }
                            }
                        }
                        if should_send {
                            self.enqueue(&PortMapWireMsg::ConnHalfClose { conn_id });
                        }
                        if both_eof {
                            if let Some(mut conn) = self.conns.remove(&conn_id) {
                                let rule_id = conn.rule_id;
                                conn.finish_graceful_and_reap(&mut self.reaper);
                                if let Some(rule) = self.rules.get_mut(&rule_id) {
                                    rule.snapshot.active_conns =
                                        rule.snapshot.active_conns.saturating_sub(1);
                                    changed = true;
                                }
                            }
                        }
                    }
                    ClientConnEvent::Window {
                        conn_id,
                        delta_bytes,
                    } => {
                        if self.conns.contains_key(&conn_id) {
                            self.enqueue(&PortMapWireMsg::ConnWindow {
                                conn_id,
                                delta_bytes,
                            });
                        }
                    }
                    ClientConnEvent::Closed { conn_id } => {
                        if let Some(mut conn) = self.conns.remove(&conn_id) {
                            let rule_id = conn.rule_id;
                            conn.abort_and_reap(&mut self.reaper);
                            if let Some(rule) = self.rules.get_mut(&rule_id) {
                                rule.snapshot.active_conns =
                                    rule.snapshot.active_conns.saturating_sub(1);
                                changed = true;
                            }
                            self.enqueue(&PortMapWireMsg::ConnClose { conn_id });
                        }
                    }
                }
            }
            self.conn_rx = Some(rx);
        }

        changed
    }

    /// 处理一条来自被控端的线帧；若任意规则状态、错误或流量计数变化则返回 `true`。
    pub fn handle_incoming(&mut self, frame: &[u8]) -> bool {
        let mut changed = self.drain_internal_events();
        let msg = match PortMapWireMsg::decode(frame) {
            Ok(m) => m,
            Err(_) => return changed,
        };

        match msg {
            PortMapWireMsg::ConnReady { conn_id, .. } => {
                // PM-5: 使用本地 conn.rule_id 作为权威规则 ID，不信任远端字段。
                if let Some(auth_rule_id) = self.activate_pending_conn(conn_id) {
                    if let Some(rule) = self.rules.get_mut(&auth_rule_id) {
                        if rule.listener.is_some() {
                            rule.snapshot.status = PortMapStatus::Ok;
                            rule.snapshot.error = None;
                        }
                    }
                    changed = true;
                }
            }
            PortMapWireMsg::ConnError {
                conn_id, message, ..
            } => {
                // PM-5: 忽略未知或已关闭 conn_id 的迟到 ConnError，防止污染规则错误状态。
                if let Some(mut conn) = self.conns.remove(&conn_id) {
                    conn.abort_and_reap(&mut self.reaper);
                    if let Some(rule) = self.rules.get_mut(&conn.rule_id) {
                        rule.snapshot.active_conns = rule.snapshot.active_conns.saturating_sub(1);
                        rule.snapshot.error = Some(message);
                        changed = true;
                    }
                }
            }
            PortMapWireMsg::ConnData { conn_id, data } => {
                let mut broken_rule: Option<(u64, Option<&'static str>)> = None;
                let data_len = data.len() as u64;
                if let Some(conn) = self.conns.get(&conn_id) {
                    let rule_id = conn.rule_id;
                    if let ClientConnState::Active {
                        write_tx: Some(tx), ..
                    } = &conn.state
                    {
                        match tx.try_send(data) {
                            Ok(()) => {
                                if let Some(rule) = self.rules.get_mut(&rule_id) {
                                    rule.snapshot.bytes_rx =
                                        rule.snapshot.bytes_rx.saturating_add(data_len);
                                    changed = true;
                                }
                            }
                            Err(TrySendError::Disconnected(_)) => {
                                broken_rule = Some((rule_id, None));
                            }
                            Err(TrySendError::Full(_)) => {
                                broken_rule = Some((rule_id, Some("local write queue full")));
                            }
                        }
                    }
                }
                if let Some((rule_id, err_msg)) = broken_rule {
                    if let Some(mut conn) = self.conns.remove(&conn_id) {
                        conn.abort_and_reap(&mut self.reaper);
                    }
                    if let Some(rule) = self.rules.get_mut(&rule_id) {
                        rule.snapshot.active_conns = rule.snapshot.active_conns.saturating_sub(1);
                        if let Some(msg) = err_msg {
                            rule.snapshot.error = Some(msg.to_string());
                        }
                        changed = true;
                    }
                    self.enqueue(&PortMapWireMsg::ConnClose { conn_id });
                }
            }
            PortMapWireMsg::ConnHalfClose { conn_id } => {
                let mut teardown_graceful = false;
                let mut teardown_pending = false;
                if let Some(conn) = self.conns.get_mut(&conn_id) {
                    match &mut conn.state {
                        ClientConnState::Active {
                            write_tx,
                            local_eof,
                            remote_eof,
                            ..
                        } => {
                            *remote_eof = true;
                            write_tx.take();
                            teardown_graceful = *local_eof && *remote_eof;
                        }
                        ClientConnState::Pending { .. } => {
                            teardown_pending = true;
                        }
                    }
                }
                if teardown_graceful {
                    if let Some(mut conn) = self.conns.remove(&conn_id) {
                        let rule_id = conn.rule_id;
                        conn.finish_graceful_and_reap(&mut self.reaper);
                        if let Some(rule) = self.rules.get_mut(&rule_id) {
                            rule.snapshot.active_conns =
                                rule.snapshot.active_conns.saturating_sub(1);
                            changed = true;
                        }
                    }
                } else if teardown_pending {
                    if let Some(mut conn) = self.conns.remove(&conn_id) {
                        let rule_id = conn.rule_id;
                        conn.abort_and_reap(&mut self.reaper);
                        if let Some(rule) = self.rules.get_mut(&rule_id) {
                            rule.snapshot.active_conns =
                                rule.snapshot.active_conns.saturating_sub(1);
                            changed = true;
                        }
                    }
                }
            }
            PortMapWireMsg::ConnWindow {
                conn_id,
                delta_bytes,
            } => {
                if delta_bytes > 0 {
                    if let Some(conn) = self.conns.get(&conn_id) {
                        if let ClientConnState::Active { send_window, .. } = &conn.state {
                            send_window.add_credit(delta_bytes);
                        }
                    }
                }
            }
            PortMapWireMsg::ConnClose { conn_id } => {
                if let Some(mut conn) = self.conns.remove(&conn_id) {
                    let rule_id = conn.rule_id;
                    conn.abort_and_reap(&mut self.reaper);
                    if let Some(rule) = self.rules.get_mut(&rule_id) {
                        rule.snapshot.active_conns = rule.snapshot.active_conns.saturating_sub(1);
                        changed = true;
                    }
                }
            }
            // Client 忽略主机侧专用上行帧。
            PortMapWireMsg::ConnOpen { .. } => {}
        }

        changed
    }

    fn activate_pending_conn(&mut self, conn_id: u64) -> Option<u64> {
        let conn = self.conns.remove(&conn_id)?;
        let rule_id = conn.rule_id;
        let stream = match conn.state {
            ClientConnState::Pending { stream, .. } => stream,
            active @ ClientConnState::Active { .. } => {
                self.conns.insert(
                    conn_id,
                    ClientConn {
                        rule_id,
                        state: active,
                    },
                );
                return None;
            }
        };

        let _ = stream.set_nonblocking(false);
        let _ = stream.set_nodelay(true);
        let _ = stream.set_read_timeout(Some(IO_POLL_TIMEOUT));

        let Ok(writer_stream) = stream.try_clone() else {
            let _ = stream.shutdown(Shutdown::Both);
            if let Some(rule) = self.rules.get_mut(&rule_id) {
                rule.snapshot.active_conns = rule.snapshot.active_conns.saturating_sub(1);
            }
            self.enqueue(&PortMapWireMsg::ConnClose { conn_id });
            return None;
        };
        let _ = writer_stream.set_write_timeout(Some(TCP_WRITE_TIMEOUT));
        let Ok(shutdown_stream) = stream.try_clone() else {
            let _ = stream.shutdown(Shutdown::Both);
            if let Some(rule) = self.rules.get_mut(&rule_id) {
                rule.snapshot.active_conns = rule.snapshot.active_conns.saturating_sub(1);
            }
            self.enqueue(&PortMapWireMsg::ConnClose { conn_id });
            return None;
        };

        let abort = Arc::new(AtomicBool::new(false));
        let send_window = Arc::new(SendWindow::new(INITIAL_CONN_WINDOW_BYTES));
        let (write_tx, write_rx) = mpsc::sync_channel::<Vec<u8>>(WRITE_CHANNEL_CAP);

        let r_abort = Arc::clone(&abort);
        let r_window = Arc::clone(&send_window);
        let r_tx = self.conn_tx.clone();
        let reader_handle = match thread::Builder::new()
            .name(format!("rc-pm-cr-{conn_id}"))
            .spawn(move || {
                let mut read_stream = stream;
                let mut buf = vec![0u8; PORTMAP_CHUNK_SIZE];
                let mut clean_eof = false;
                while !r_abort.load(Ordering::Acquire) {
                    let budget = r_window.wait_for_budget(PORTMAP_CHUNK_SIZE, &r_abort);
                    if budget == 0 {
                        break;
                    }
                    match read_stream.read(&mut buf[..budget]) {
                        Ok(0) => {
                            clean_eof = true;
                            break;
                        }
                        Ok(n) => {
                            r_window.consume(n);
                            if !send_cancellable(
                                &r_tx,
                                ClientConnEvent::Data {
                                    conn_id,
                                    data: buf[..n].to_vec(),
                                },
                                &r_abort,
                            ) {
                                break;
                            }
                        }
                        Err(e)
                            if matches!(
                                e.kind(),
                                ErrorKind::WouldBlock
                                    | ErrorKind::TimedOut
                                    | ErrorKind::Interrupted
                            ) => {}
                        Err(_) => break,
                    }
                }
                if !r_abort.load(Ordering::Acquire) {
                    if clean_eof {
                        let _ = send_cancellable(
                            &r_tx,
                            ClientConnEvent::HalfClose { conn_id },
                            &r_abort,
                        );
                    } else {
                        let _ =
                            send_cancellable(&r_tx, ClientConnEvent::Closed { conn_id }, &r_abort);
                    }
                }
            }) {
            Ok(h) => h,
            Err(e) => {
                let _ = shutdown_stream.shutdown(Shutdown::Both);
                if let Some(rule) = self.rules.get_mut(&rule_id) {
                    rule.snapshot.active_conns = rule.snapshot.active_conns.saturating_sub(1);
                    rule.snapshot.error = Some(format!("failed to spawn local reader thread: {e}"));
                }
                self.enqueue(&PortMapWireMsg::ConnClose { conn_id });
                return None;
            }
        };

        let w_abort = Arc::clone(&abort);
        let w_tx = self.conn_tx.clone();
        let writer_handle = match thread::Builder::new()
            .name(format!("rc-pm-cw-{conn_id}"))
            .spawn(move || {
                let w_abort_cb = Arc::clone(&w_abort);
                let clean = tcp_writer_loop(writer_stream, write_rx, w_abort, |delta_bytes| {
                    send_cancellable(
                        &w_tx,
                        ClientConnEvent::Window {
                            conn_id,
                            delta_bytes,
                        },
                        &w_abort_cb,
                    )
                });
                if !clean && !w_abort_cb.load(Ordering::Acquire) {
                    let _ =
                        send_cancellable(&w_tx, ClientConnEvent::Closed { conn_id }, &w_abort_cb);
                }
            }) {
            Ok(h) => h,
            Err(e) => {
                abort.store(true, Ordering::Release);
                send_window.notify_all();
                let _ = shutdown_stream.shutdown(Shutdown::Both);
                let _ = reader_handle.join();
                if let Some(rule) = self.rules.get_mut(&rule_id) {
                    rule.snapshot.active_conns = rule.snapshot.active_conns.saturating_sub(1);
                    rule.snapshot.error = Some(format!("failed to spawn local writer thread: {e}"));
                }
                self.enqueue(&PortMapWireMsg::ConnClose { conn_id });
                return None;
            }
        };

        self.conns.insert(
            conn_id,
            ClientConn {
                rule_id,
                state: ClientConnState::Active {
                    abort,
                    send_window,
                    shutdown_stream,
                    write_tx: Some(write_tx),
                    reader_thread: Some(reader_handle),
                    writer_thread: Some(writer_handle),
                    local_eof: false,
                    remote_eof: false,
                },
            },
        );
        Some(rule_id)
    }

    /// 轮询本地监听器与代理连接事件，最多返回 `max_frames` 个待发线帧。
    pub fn poll_outgoing(&mut self, max_frames: usize) -> Vec<Vec<u8>> {
        let _ = self.drain_internal_events();
        if max_frames == 0 {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(max_frames.min(self.outgoing.len()));
        while out.len() < max_frames {
            let Some(frame) = self.outgoing.pop_front() else {
                break;
            };
            out.push(frame);
        }
        out
    }
}

impl Drop for PortMapClientEngine {
    fn drop(&mut self) {
        // 先丢弃事件接收端，确保任何阻塞在 `send` 的监听/读取线程立刻返回。
        self.listener_rx.take();
        self.conn_rx.take();

        let rules = std::mem::take(&mut self.rules);
        for (_, mut rule) in rules {
            if let Some(mut l) = rule.listener.take() {
                l.stop_and_join();
            }
        }
        let conns = std::mem::take(&mut self.conns);
        for (_, mut conn) in conns {
            conn.abort_and_reap(&mut self.reaper);
        }
        for worker in std::mem::take(&mut self.reaper) {
            worker.force_abort_and_join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portmap_status_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&PortMapStatus::Ok).unwrap(), "\"ok\"");
        assert_eq!(
            serde_json::to_string(&PortMapStatus::Fail).unwrap(),
            "\"fail\""
        );
        assert_eq!(
            serde_json::to_string(&PortMapStatus::Pending).unwrap(),
            "\"pending\""
        );
    }

    #[test]
    fn portmap_wire_roundtrip_and_bounds() {
        let msgs = [
            PortMapWireMsg::ConnOpen {
                conn_id: 1,
                rule_id: 2,
                target_addr: "127.0.0.1".to_string(),
                target_port: 8080,
            },
            PortMapWireMsg::ConnReady {
                conn_id: 1,
                rule_id: 2,
            },
            PortMapWireMsg::ConnError {
                conn_id: 1,
                rule_id: 2,
                message: "connection refused".to_string(),
            },
            PortMapWireMsg::ConnData {
                conn_id: 1,
                data: vec![0x5A; PORTMAP_CHUNK_SIZE],
            },
            PortMapWireMsg::ConnClose { conn_id: 1 },
            PortMapWireMsg::ConnHalfClose { conn_id: 1 },
            PortMapWireMsg::ConnWindow {
                conn_id: 1,
                delta_bytes: 256 * 1024,
            },
        ];
        for m in msgs {
            let encoded = m.encode().unwrap();
            assert!(encoded.len() <= MAX_PORTMAP_WIRE_FRAME_BYTES);
            assert_eq!(PortMapWireMsg::decode(&encoded).unwrap(), m);
        }

        let too_big_chunk = PortMapWireMsg::ConnData {
            conn_id: 1,
            data: vec![0u8; PORTMAP_CHUNK_SIZE + 1],
        };
        assert!(too_big_chunk.encode().is_err());
    }

    #[test]
    fn add_rule_validates_and_records_bind_conflict() {
        let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
        let occupied_port = occupied.local_addr().unwrap().port();

        let mut client = PortMapClientEngine::new();
        assert!(client
            .add_rule("".to_string(), 8080, "127.0.0.1".to_string(), 80)
            .is_err());
        assert!(client
            .add_rule("web".to_string(), 0, "127.0.0.1".to_string(), 80)
            .is_err());
        assert!(client
            .add_rule("web".to_string(), 8080, "bad host".to_string(), 80)
            .is_err());
        assert!(client
            .add_rule("web".to_string(), 8080, "127.0.0.1\n".to_string(), 80)
            .is_err());
        assert!(client
            .add_rule("web".to_string(), 8080, "127.0.0.1\0".to_string(), 80)
            .is_err());
        assert!(client
            .add_rule("web".to_string(), 8080, "127.0.0.1".to_string(), 0)
            .is_err());

        let snap = client
            .add_rule(
                "conflict".to_string(),
                occupied_port,
                "127.0.0.1".to_string(),
                9000,
            )
            .unwrap();
        assert_eq!(snap.status, PortMapStatus::Fail);
        assert!(snap.error.is_some());
        assert_eq!(client.list_rules().len(), 1);

        // Duplicate local_port among existing rules must return exact Chinese error.
        let dup_err = client
            .add_rule(
                "dup".to_string(),
                occupied_port,
                "127.0.0.1".to_string(),
                9001,
            )
            .unwrap_err();
        assert_eq!(dup_err, "本地端口已被其他规则占用");
    }

    #[test]
    fn host_rejects_control_and_whitespace_target_addr() {
        let mut host = PortMapHostEngine::new();
        for bad in [
            "",
            " 127.0.0.1",
            "127.0.0.1 ",
            "bad host",
            "127.0.0.1\n",
            "127.0.0.1\0",
        ] {
            let f = PortMapWireMsg::ConnOpen {
                conn_id: 1,
                rule_id: 1,
                target_addr: bad.to_string(),
                target_port: 80,
            }
            .encode()
            .unwrap();
            host.handle_incoming(&f);
            let out = host.poll_outgoing(4);
            assert_eq!(out.len(), 1);
            assert!(matches!(
                PortMapWireMsg::decode(&out[0]).unwrap(),
                PortMapWireMsg::ConnError { conn_id: 1, .. }
            ));
        }
    }

    #[test]
    fn send_cancellable_exits_promptly_when_channel_full_and_stop_set() {
        let (tx, _rx) = mpsc::sync_channel::<u32>(1);
        tx.try_send(1).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop);

        let started = std::time::Instant::now();
        let handle = thread::spawn(move || send_cancellable(&tx, 2, &stop_clone));
        thread::sleep(Duration::from_millis(20));
        stop.store(true, Ordering::Release);
        let sent = handle.join().unwrap();
        assert!(!sent);
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn host_handle_incoming_closes_stalled_conn_without_blocking() {
        let mut host = PortMapHostEngine::new();
        let (write_tx, _write_rx) = mpsc::sync_channel::<Vec<u8>>(1);
        write_tx.try_send(vec![1, 2, 3]).unwrap();

        host.conns.insert(
            77,
            HostConn {
                rule_id: 9,
                abort: Arc::new(AtomicBool::new(false)),
                send_window: Arc::new(SendWindow::new(INITIAL_CONN_WINDOW_BYTES)),
                stream_slot: Arc::new(Mutex::new(None)),
                write_tx: Some(write_tx),
                main_thread: None,
                local_eof: false,
                remote_eof: false,
            },
        );

        let frame = PortMapWireMsg::ConnData {
            conn_id: 77,
            data: vec![4, 5, 6],
        }
        .encode()
        .unwrap();

        let t0 = std::time::Instant::now();
        host.handle_incoming(&frame);
        assert!(t0.elapsed() < Duration::from_millis(100));
        assert_eq!(host.active_conn_count(), 0);

        let out: Vec<PortMapWireMsg> = host
            .poll_outgoing(8)
            .into_iter()
            .map(|f| PortMapWireMsg::decode(&f).unwrap())
            .collect();
        assert!(out.iter().any(|m| matches!(
            m,
            PortMapWireMsg::ConnError {
                conn_id: 77,
                rule_id: 9,
                ..
            }
        )));
        assert!(out
            .iter()
            .any(|m| matches!(m, PortMapWireMsg::ConnClose { conn_id: 77 })));
    }

    #[test]
    fn host_and_client_cap_outgoing_at_max_outgoing_frames_for_backpressure() {
        // Host side backpressure cap
        let mut host = PortMapHostEngine::new();
        host.conns.insert(
            1,
            HostConn {
                rule_id: 1,
                abort: Arc::new(AtomicBool::new(false)),
                send_window: Arc::new(SendWindow::new(INITIAL_CONN_WINDOW_BYTES)),
                stream_slot: Arc::new(Mutex::new(None)),
                write_tx: None,
                main_thread: None,
                local_eof: false,
                remote_eof: false,
            },
        );
        for _ in 0..(MAX_OUTGOING_FRAMES + 50) {
            host.event_tx
                .try_send(HostWorkerEvent::Data {
                    conn_id: 1,
                    data: vec![0xAA; 64],
                })
                .unwrap();
        }
        host.drain_worker_events();
        assert_eq!(
            host.outgoing.len(),
            MAX_OUTGOING_FRAMES,
            "host outgoing must stop draining at MAX_OUTGOING_FRAMES"
        );
        // Remaining 50 events stay in event_rx until outgoing is polled.
        let first_batch = host.poll_outgoing(50);
        assert_eq!(first_batch.len(), 50);
        host.drain_worker_events();
        assert_eq!(host.outgoing.len(), MAX_OUTGOING_FRAMES);

        // Client side backpressure cap
        let mut client = PortMapClientEngine::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let snap = client
            .add_rule("bp".to_string(), port, "127.0.0.1".to_string(), 9999)
            .unwrap();
        let s = TcpListener::bind("127.0.0.1:0").unwrap();
        let sp = s.local_addr().unwrap().port();
        let c_stream = TcpStream::connect(("127.0.0.1", sp)).unwrap();
        let _s_stream = s.accept().unwrap();
        client.conns.insert(
            5,
            ClientConn {
                rule_id: snap.id,
                state: ClientConnState::Pending {
                    stream: c_stream,
                    created_at: Instant::now(),
                },
            },
        );
        for _ in 0..(MAX_OUTGOING_FRAMES + 40) {
            client
                .conn_tx
                .try_send(ClientConnEvent::Data {
                    conn_id: 5,
                    data: vec![0xBB; 64],
                })
                .unwrap();
        }
        let _ = client.drain_internal_events();
        assert_eq!(
            client.outgoing.len(),
            MAX_OUTGOING_FRAMES,
            "client outgoing must stop draining at MAX_OUTGOING_FRAMES"
        );
    }

    #[test]
    fn host_atomic_worker_cap_blocks_conn_open_after_conn_close_while_workers_alive() {
        let mut host = PortMapHostEngine::new();
        // Simulate MAX_HOST_PROXY_CONNS workers still alive (e.g. dialing/reaping after ConnClose),
        // even though self.conns is completely empty!
        let guards: Vec<WorkerCountGuard> = (0..MAX_HOST_PROXY_CONNS)
            .map(|_| {
                host.live_workers.fetch_add(1, Ordering::AcqRel);
                WorkerCountGuard(Arc::clone(&host.live_workers))
            })
            .collect();
        assert_eq!(host.active_conn_count(), 0);
        assert_eq!(host.live_worker_count(), MAX_HOST_PROXY_CONNS);

        let open = PortMapWireMsg::ConnOpen {
            conn_id: 999,
            rule_id: 1,
            target_addr: "127.0.0.1".to_string(),
            target_port: 80,
        }
        .encode()
        .unwrap();
        host.handle_incoming(&open);
        assert_eq!(host.active_conn_count(), 0);
        let out = host.poll_outgoing(4);
        assert_eq!(out.len(), 1);
        match PortMapWireMsg::decode(&out[0]).unwrap() {
            PortMapWireMsg::ConnError {
                conn_id: 999,
                message,
                ..
            } => assert!(message.contains("maximum concurrent proxy connections")),
            other => panic!("expected ConnError, got {other:?}"),
        }

        // Dropping one guard releases a slot atomically.
        drop(guards);
        assert_eq!(host.live_worker_count(), 0);
    }

    #[test]
    fn stale_conn_error_is_ignored_and_host_suppresses_error_for_closed_conn() {
        // Host must NOT emit ConnError for a conn_id already removed by ConnClose.
        let mut host = PortMapHostEngine::new();
        host.event_tx
            .try_send(HostWorkerEvent::Error {
                conn_id: 404,
                rule_id: 1,
                message: "late connect failure".to_string(),
            })
            .unwrap();
        assert!(host.poll_outgoing(8).is_empty());

        // Client must ignore ConnError for unknown conn_id and not pollute rule.error.
        let mut client = PortMapClientEngine::new();
        let tmp = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = tmp.local_addr().unwrap().port();
        drop(tmp);
        let rule = client
            .add_rule("clean".to_string(), port, "127.0.0.1".to_string(), 8080)
            .unwrap();
        assert_eq!(rule.error, None);

        let stale_err = PortMapWireMsg::ConnError {
            conn_id: 12345,
            rule_id: rule.id,
            message: "stale error".to_string(),
        }
        .encode()
        .unwrap();
        assert!(!client.handle_incoming(&stale_err));
        assert_eq!(client.list_rules()[0].error, None);
    }

    #[test]
    fn stale_accepted_sockets_are_drained_on_rule_disable_keeping_other_rules() {
        let mut client = PortMapClientEngine::new();
        let l1 = TcpListener::bind("127.0.0.1:0").unwrap();
        let p1 = l1.local_addr().unwrap().port();
        drop(l1);
        let l2 = TcpListener::bind("127.0.0.1:0").unwrap();
        let p2 = l2.local_addr().unwrap().port();
        drop(l2);

        let r1 = client
            .add_rule("r1".to_string(), p1, "127.0.0.1".to_string(), 8001)
            .unwrap();
        let r2 = client
            .add_rule("r2".to_string(), p2, "127.0.0.1".to_string(), 8002)
            .unwrap();

        // Connect to BOTH local listeners BEFORE draining events so both sit in listener_rx.
        let _c1 = TcpStream::connect(("127.0.0.1", p1)).unwrap();
        let _c2 = TcpStream::connect(("127.0.0.1", p2)).unwrap();
        thread::sleep(Duration::from_millis(30));

        // Disable r1 -> must discard stale Accepted for r1 while keeping r2's Accepted!
        let _ = client.set_rule_enabled(r1.id, false).unwrap();
        // Even if r1 is re-enabled before polling, the old stale socket for r1 must NOT produce ConnOpen!
        let _ = client.set_rule_enabled(r1.id, true).unwrap();

        let frames: Vec<PortMapWireMsg> = client
            .poll_outgoing(16)
            .into_iter()
            .map(|f| PortMapWireMsg::decode(&f).unwrap())
            .collect();
        assert!(
            !frames.iter().any(
                |m| matches!(m, PortMapWireMsg::ConnOpen { rule_id, .. } if *rule_id == r1.id)
            ),
            "stale Accepted socket for disabled rule r1 must have been drained: {frames:?}"
        );
        assert!(
            frames.iter().any(
                |m| matches!(m, PortMapWireMsg::ConnOpen { rule_id, .. } if *rule_id == r2.id)
            ),
            "Accepted socket for unaffected rule r2 must be preserved: {frames:?}"
        );
    }

    #[test]
    fn add_rule_with_enabled_false_does_not_bind_local_port() {
        let mut client = PortMapClientEngine::new();
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let free_port = probe.local_addr().unwrap().port();
        drop(probe);

        let rule = client
            .add_rule_with_enabled(
                "paused-rule".to_string(),
                free_port,
                "127.0.0.1".to_string(),
                8080,
                false,
            )
            .unwrap();
        assert!(!rule.enabled);
        assert_eq!(rule.status, PortMapStatus::Pending);
        assert_eq!(rule.error, None);

        // 禁用规则未短暂占用本地端口，外部仍可立即绑定同一端口。
        let rebound = TcpListener::bind(("127.0.0.1", free_port));
        assert!(
            rebound.is_ok(),
            "disabled rule must not bind local port: {:?}",
            rebound.err()
        );
    }
}
