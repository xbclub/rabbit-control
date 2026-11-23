//! 远程真实 PTY 终端引擎（W3）：支持多标签并发 Shell、窗口尺寸同步与优雅回收。
//!
//! 线协议采用 `[kind: u8][header_len: u32 LE][json_header][raw_bytes]`，
//! 单帧上限 [`MAX_TERMINAL_WIRE_FRAME_BYTES`]（64 KiB）。

#![deny(missing_docs)]
#![deny(unsafe_code)]

use std::collections::{BTreeMap, HashMap, VecDeque};
#[cfg(unix)]
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[cfg(unix)]
use crate::pty_unix::PtyProcess;

/// 单个终端线帧最大字节数（64 KiB）。
pub const MAX_TERMINAL_WIRE_FRAME_BYTES: usize = 64 * 1024;

/// 单条终端输入/输出数据块最大负载（32 KiB）。
pub const TERMINAL_DATA_CHUNK_SIZE: usize = 32 * 1024;

/// 被控端允许同时打开的最大 PTY 数量。
pub const MAX_CONCURRENT_TERMINALS: usize = 4;

/// 被控端终端引擎待发输出队列上限（帧数），防止慢网络下内存无界增长。
pub const MAX_TERMINAL_OUTGOING_FRAMES: usize = 64;

const FRAME_KIND_CONTROL: u8 = 1;
const FRAME_KIND_INPUT: u8 = 2;
const FRAME_KIND_OUTPUT: u8 = 3;
const FRAME_PREFIX_LEN: usize = 5;

#[cfg(unix)]
const EXIT_DRAIN_GRACE: Duration = Duration::from_millis(250);

/// 远程终端线协议消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalWireMsg {
    /// 客户端请求开启新的 PTY 会话。
    Open {
        /// 终端会话 ID。
        term_id: u32,
        /// 初始列数。
        cols: u16,
        /// 初始行数。
        rows: u16,
    },
    /// 主机返回 PTY 开启结果。
    Opened {
        /// 终端会话 ID。
        term_id: u32,
        /// Shell 名称标题。
        title: String,
        /// 初始工作目录。
        cwd: String,
        /// 是否启动成功。
        ok: bool,
        /// 错误信息（失败时填充）。
        error: Option<String>,
    },
    /// 客户端发送至 PTY 的输入字节流。
    Input {
        /// 终端会话 ID。
        term_id: u32,
        /// 原始键盘/粘贴字节。
        data: Vec<u8>,
    },
    /// 主机从 PTY 读取的输出字节流。
    Output {
        /// 终端会话 ID。
        term_id: u32,
        /// 原始终端输出字节（含 ANSI 转义序列）。
        data: Vec<u8>,
    },
    /// 调整终端窗口尺寸。
    Resize {
        /// 终端会话 ID。
        term_id: u32,
        /// 新列数。
        cols: u16,
        /// 新行数。
        rows: u16,
    },
    /// 客户端请求关闭终端会话。
    Close {
        /// 终端会话 ID。
        term_id: u32,
    },
    /// 主机通知终端进程已退出。
    Exited {
        /// 终端会话 ID。
        term_id: u32,
        /// 退出码。
        code: i32,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
enum TermControlPayload {
    Open {
        term_id: u32,
        cols: u16,
        rows: u16,
    },
    Opened {
        term_id: u32,
        title: String,
        cwd: String,
        ok: bool,
        error: Option<String>,
    },
    Resize {
        term_id: u32,
        cols: u16,
        rows: u16,
    },
    Close {
        term_id: u32,
    },
    Exited {
        term_id: u32,
        code: i32,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct TermDataHeader {
    term_id: u32,
}

impl TerminalWireMsg {
    /// 编码为严格受限（`<= 64 KiB`）的二进制线帧。
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        match self {
            TerminalWireMsg::Input { term_id, data } => {
                encode_data_frame(FRAME_KIND_INPUT, *term_id, data)
            }
            TerminalWireMsg::Output { term_id, data } => {
                encode_data_frame(FRAME_KIND_OUTPUT, *term_id, data)
            }
            other => {
                let ctrl = match other {
                    TerminalWireMsg::Open {
                        term_id,
                        cols,
                        rows,
                    } => TermControlPayload::Open {
                        term_id: *term_id,
                        cols: *cols,
                        rows: *rows,
                    },
                    TerminalWireMsg::Opened {
                        term_id,
                        title,
                        cwd,
                        ok,
                        error,
                    } => TermControlPayload::Opened {
                        term_id: *term_id,
                        title: title.clone(),
                        cwd: cwd.clone(),
                        ok: *ok,
                        error: error.clone(),
                    },
                    TerminalWireMsg::Resize {
                        term_id,
                        cols,
                        rows,
                    } => TermControlPayload::Resize {
                        term_id: *term_id,
                        cols: *cols,
                        rows: *rows,
                    },
                    TerminalWireMsg::Close { term_id } => {
                        TermControlPayload::Close { term_id: *term_id }
                    }
                    TerminalWireMsg::Exited { term_id, code } => TermControlPayload::Exited {
                        term_id: *term_id,
                        code: *code,
                    },
                    TerminalWireMsg::Input { .. } | TerminalWireMsg::Output { .. } => {
                        unreachable!()
                    }
                };
                encode_control_frame(&ctrl)
            }
        }
    }

    /// 严格校验并解码二进制线帧。
    pub fn decode(frame: &[u8]) -> Result<Self, String> {
        if frame.len() < FRAME_PREFIX_LEN {
            return Err("terminal wire frame too short".to_string());
        }
        if frame.len() > MAX_TERMINAL_WIRE_FRAME_BYTES {
            return Err(format!(
                "terminal wire frame exceeds {} bytes limit ({} bytes)",
                MAX_TERMINAL_WIRE_FRAME_BYTES,
                frame.len()
            ));
        }

        let kind = frame[0];
        let hdr_len = u32::from_le_bytes([frame[1], frame[2], frame[3], frame[4]]) as usize;
        let hdr_end = FRAME_PREFIX_LEN
            .checked_add(hdr_len)
            .ok_or_else(|| "terminal wire header length overflow".to_string())?;
        if hdr_end > frame.len() {
            return Err("terminal wire header length out of bounds".to_string());
        }

        let hdr_bytes = &frame[FRAME_PREFIX_LEN..hdr_end];
        let raw_bytes = &frame[hdr_end..];

        match kind {
            FRAME_KIND_CONTROL => {
                if !raw_bytes.is_empty() {
                    return Err(
                        "terminal control frame must not contain trailing bytes".to_string()
                    );
                }
                let ctrl: TermControlPayload = serde_json::from_slice(hdr_bytes)
                    .map_err(|e| format!("invalid terminal control json: {e}"))?;
                Ok(match ctrl {
                    TermControlPayload::Open {
                        term_id,
                        cols,
                        rows,
                    } => TerminalWireMsg::Open {
                        term_id,
                        cols,
                        rows,
                    },
                    TermControlPayload::Opened {
                        term_id,
                        title,
                        cwd,
                        ok,
                        error,
                    } => TerminalWireMsg::Opened {
                        term_id,
                        title,
                        cwd,
                        ok,
                        error,
                    },
                    TermControlPayload::Resize {
                        term_id,
                        cols,
                        rows,
                    } => TerminalWireMsg::Resize {
                        term_id,
                        cols,
                        rows,
                    },
                    TermControlPayload::Close { term_id } => TerminalWireMsg::Close { term_id },
                    TermControlPayload::Exited { term_id, code } => {
                        TerminalWireMsg::Exited { term_id, code }
                    }
                })
            }
            FRAME_KIND_INPUT => {
                let hdr: TermDataHeader = serde_json::from_slice(hdr_bytes)
                    .map_err(|e| format!("invalid terminal input header: {e}"))?;
                Ok(TerminalWireMsg::Input {
                    term_id: hdr.term_id,
                    data: raw_bytes.to_vec(),
                })
            }
            FRAME_KIND_OUTPUT => {
                let hdr: TermDataHeader = serde_json::from_slice(hdr_bytes)
                    .map_err(|e| format!("invalid terminal output header: {e}"))?;
                Ok(TerminalWireMsg::Output {
                    term_id: hdr.term_id,
                    data: raw_bytes.to_vec(),
                })
            }
            _ => Err(format!("unknown terminal wire frame kind: {kind}")),
        }
    }
}

fn encode_control_frame(ctrl: &TermControlPayload) -> Result<Vec<u8>, String> {
    let header = serde_json::to_vec(ctrl).map_err(|e| format!("serialize control failed: {e}"))?;
    let total = FRAME_PREFIX_LEN + header.len();
    if total > MAX_TERMINAL_WIRE_FRAME_BYTES {
        return Err(format!(
            "terminal control frame too large ({total} > {MAX_TERMINAL_WIRE_FRAME_BYTES})"
        ));
    }
    let mut out = Vec::with_capacity(total);
    out.push(FRAME_KIND_CONTROL);
    out.extend_from_slice(&(header.len() as u32).to_le_bytes());
    out.extend_from_slice(&header);
    Ok(out)
}

fn encode_data_frame(kind: u8, term_id: u32, data: &[u8]) -> Result<Vec<u8>, String> {
    let header = serde_json::to_vec(&TermDataHeader { term_id })
        .map_err(|e| format!("serialize data header failed: {e}"))?;
    let total = FRAME_PREFIX_LEN + header.len() + data.len();
    if total > MAX_TERMINAL_WIRE_FRAME_BYTES {
        return Err(format!(
            "terminal data frame too large ({total} > {MAX_TERMINAL_WIRE_FRAME_BYTES})"
        ));
    }
    let mut out = Vec::with_capacity(total);
    out.push(kind);
    out.extend_from_slice(&(header.len() as u32).to_le_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(data);
    Ok(out)
}

/// 终端客户端面向 UI 产出的事件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TerminalEvent {
    /// 远端终端会话打开结果。
    Opened {
        /// 终端会话 ID。
        #[serde(rename = "termId")]
        term_id: u32,
        /// Shell 名称标题。
        title: String,
        /// 初始工作目录。
        cwd: String,
        /// 是否打开成功。
        ok: bool,
        /// 错误详情（失败时填充）。
        error: Option<String>,
    },
    /// 远端终端新到达的输出内容。
    Output {
        /// 终端会话 ID。
        #[serde(rename = "termId")]
        term_id: u32,
        /// 宽松 UTF-8 解码后的文本（适合轻量 UI 渲染）。
        #[serde(rename = "textUtf8Lossy")]
        text_utf8_lossy: String,
        /// 原始输出字节（适合完整 VT100/xterm 终端模拟器）。
        bytes: Vec<u8>,
    },
    /// 远端终端进程已退出。
    Exited {
        /// 终端会话 ID。
        #[serde(rename = "termId")]
        term_id: u32,
        /// 进程退出码。
        code: i32,
    },
}

#[cfg(unix)]
struct HostPtySession {
    pty: PtyProcess,
    pending_exit: Option<(i32, Instant)>,
}

/// 被控端（Host）远程终端引擎，最多同时托管 [`MAX_CONCURRENT_TERMINALS`]（4）个真实 PTY。
///
/// 析构时自动终止并回收所有存活的子 Shell 进程。
pub struct TerminalHostEngine {
    outgoing: VecDeque<Vec<u8>>,
    #[cfg(unix)]
    sessions: BTreeMap<u32, HostPtySession>,
    #[cfg(not(unix))]
    sessions: BTreeMap<u32, ()>,
}

impl Default for TerminalHostEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalHostEngine {
    /// 创建新的被控端远程终端引擎。
    pub fn new() -> Self {
        Self {
            outgoing: VecDeque::new(),
            sessions: BTreeMap::new(),
        }
    }

    /// 返回当前活跃的 PTY 会话数。
    pub fn active_count(&self) -> usize {
        self.sessions.len()
    }

    fn enqueue(&mut self, msg: &TerminalWireMsg) {
        if let Ok(frame) = msg.encode() {
            self.outgoing.push_back(frame);
        }
    }

    /// 处理一条来自控制端的二进制线帧。
    pub fn handle_incoming(&mut self, frame: &[u8]) {
        let msg = match TerminalWireMsg::decode(frame) {
            Ok(m) => m,
            Err(_) => return,
        };

        match msg {
            TerminalWireMsg::Open {
                term_id,
                cols,
                rows,
            } => {
                self.handle_open(term_id, cols, rows);
            }
            TerminalWireMsg::Input { term_id, data } => {
                #[cfg(unix)]
                {
                    let mut fatal_exit: Option<(Option<Vec<u8>>, i32)> = None;
                    if let Some(sess) = self.sessions.get_mut(&term_id) {
                        if let Err(err) = sess.pty.write_input(&data) {
                            // 仅在底层 PTY 写入出现致命 I/O 错误（非临时缓冲满反压）时回收会话。
                            if !err.contains("pending write buffer full") {
                                let remaining_slots = MAX_TERMINAL_OUTGOING_FRAMES
                                    .saturating_sub(self.outgoing.len());
                                let tail = if remaining_slots > 0 {
                                    let bytes = sess.pty.try_read_output_up_to(
                                        remaining_slots.saturating_mul(TERMINAL_DATA_CHUNK_SIZE),
                                    );
                                    if bytes.is_empty() {
                                        None
                                    } else {
                                        Some(bytes)
                                    }
                                } else {
                                    None
                                };
                                let code = sess.pty.try_wait().unwrap_or(-1);
                                fatal_exit = Some((tail, code));
                            }
                        }
                    }
                    if let Some((tail, code)) = fatal_exit {
                        if let Some(bytes) = tail {
                            for chunk in bytes.chunks(TERMINAL_DATA_CHUNK_SIZE) {
                                self.enqueue(&TerminalWireMsg::Output {
                                    term_id,
                                    data: chunk.to_vec(),
                                });
                            }
                        }
                        self.sessions.remove(&term_id);
                        self.enqueue(&TerminalWireMsg::Exited { term_id, code });
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = (term_id, data);
                }
            }
            TerminalWireMsg::Resize {
                term_id,
                cols,
                rows,
            } => {
                #[cfg(unix)]
                if let Some(sess) = self.sessions.get_mut(&term_id) {
                    let _ = sess.pty.resize(cols, rows);
                }
                #[cfg(not(unix))]
                {
                    let _ = (term_id, cols, rows);
                }
            }
            TerminalWireMsg::Close { term_id } => {
                self.sessions.remove(&term_id);
            }
            // Host 忽略控制端专用下行帧。
            TerminalWireMsg::Opened { .. }
            | TerminalWireMsg::Output { .. }
            | TerminalWireMsg::Exited { .. } => {}
        }
    }

    fn handle_open(&mut self, term_id: u32, cols: u16, rows: u16) {
        if self.sessions.len() >= MAX_CONCURRENT_TERMINALS {
            self.enqueue(&TerminalWireMsg::Opened {
                term_id,
                title: String::new(),
                cwd: String::new(),
                ok: false,
                error: Some(format!(
                    "maximum concurrent terminals ({MAX_CONCURRENT_TERMINALS}) reached"
                )),
            });
            return;
        }
        if self.sessions.contains_key(&term_id) {
            self.enqueue(&TerminalWireMsg::Opened {
                term_id,
                title: String::new(),
                cwd: String::new(),
                ok: false,
                error: Some(format!("terminal id {term_id} already exists")),
            });
            return;
        }

        #[cfg(unix)]
        {
            match PtyProcess::spawn(cols, rows) {
                Ok(pty) => {
                    let title = pty.title().to_string();
                    let cwd = pty.cwd().to_string();
                    self.sessions.insert(
                        term_id,
                        HostPtySession {
                            pty,
                            pending_exit: None,
                        },
                    );
                    self.enqueue(&TerminalWireMsg::Opened {
                        term_id,
                        title,
                        cwd,
                        ok: true,
                        error: None,
                    });
                }
                Err(err) => {
                    self.enqueue(&TerminalWireMsg::Opened {
                        term_id,
                        title: String::new(),
                        cwd: String::new(),
                        ok: false,
                        error: Some(err),
                    });
                }
            }
        }

        #[cfg(not(unix))]
        {
            let _ = (cols, rows);
            self.enqueue(&TerminalWireMsg::Opened {
                term_id,
                title: String::new(),
                cwd: String::new(),
                ok: false,
                error: Some("PTY is not supported on this platform".to_string()),
            });
        }
    }

    /// 轮询所有活跃 PTY 的输出与退出事件，并最多返回 `max_frames` 个待发线帧。
    pub fn poll_outgoing(&mut self, max_frames: usize) -> Vec<Vec<u8>> {
        if max_frames == 0 {
            return Vec::new();
        }

        #[cfg(unix)]
        {
            let mut exited_ids: Vec<(u32, i32)> = Vec::new();
            let mut pending_outputs: Vec<(u32, Vec<u8>)> = Vec::new();
            let mut queued_frames = self.outgoing.len();

            for (&term_id, sess) in &mut self.sessions {
                if queued_frames < MAX_TERMINAL_OUTGOING_FRAMES {
                    let remaining_slots = MAX_TERMINAL_OUTGOING_FRAMES - queued_frames;
                    let bytes = sess.pty.try_read_output_up_to(
                        remaining_slots.saturating_mul(TERMINAL_DATA_CHUNK_SIZE),
                    );
                    if !bytes.is_empty() {
                        let chunks = bytes.len().div_ceil(TERMINAL_DATA_CHUNK_SIZE);
                        queued_frames = queued_frames.saturating_add(chunks);
                        pending_outputs.push((term_id, bytes));
                    }
                }

                if sess.pending_exit.is_none() {
                    if let Some(code) = sess.pty.try_wait() {
                        sess.pending_exit = Some((code, Instant::now()));
                    }
                }

                if let Some((code, detected_at)) = sess.pending_exit {
                    if (sess.pty.is_reader_eof() || detected_at.elapsed() >= EXIT_DRAIN_GRACE)
                        && queued_frames < MAX_TERMINAL_OUTGOING_FRAMES
                    {
                        let remaining_slots = MAX_TERMINAL_OUTGOING_FRAMES - queued_frames;
                        let tail = sess.pty.try_read_output_up_to(
                            remaining_slots.saturating_mul(TERMINAL_DATA_CHUNK_SIZE),
                        );
                        if !tail.is_empty() {
                            let chunks = tail.len().div_ceil(TERMINAL_DATA_CHUNK_SIZE);
                            queued_frames = queued_frames.saturating_add(chunks);
                            pending_outputs.push((term_id, tail));
                        }
                        exited_ids.push((term_id, code));
                    }
                }
            }

            for (term_id, bytes) in pending_outputs {
                for chunk in bytes.chunks(TERMINAL_DATA_CHUNK_SIZE) {
                    self.enqueue(&TerminalWireMsg::Output {
                        term_id,
                        data: chunk.to_vec(),
                    });
                }
            }

            for (term_id, code) in exited_ids {
                self.sessions.remove(&term_id);
                self.enqueue(&TerminalWireMsg::Exited { term_id, code });
            }
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

impl Drop for TerminalHostEngine {
    fn drop(&mut self) {
        self.sessions.clear();
    }
}

fn decode_utf8_streaming(carry: &mut Vec<u8>, chunk: &[u8]) -> String {
    if carry.is_empty() && chunk.is_empty() {
        return String::new();
    }
    let combined;
    let mut slice: &[u8] = if carry.is_empty() {
        chunk
    } else {
        combined = {
            let mut buf = Vec::with_capacity(carry.len() + chunk.len());
            buf.extend_from_slice(carry);
            buf.extend_from_slice(chunk);
            buf
        };
        carry.clear();
        &combined
    };

    let mut out = String::new();
    while !slice.is_empty() {
        match std::str::from_utf8(slice) {
            Ok(valid) => {
                out.push_str(valid);
                break;
            }
            Err(err) => {
                let valid_up_to = err.valid_up_to();
                if valid_up_to > 0 {
                    if let Ok(valid) = std::str::from_utf8(&slice[..valid_up_to]) {
                        out.push_str(valid);
                    }
                }
                let rest = &slice[valid_up_to..];
                match err.error_len() {
                    Some(invalid_len) => {
                        out.push('\u{FFFD}');
                        slice = &rest[invalid_len..];
                    }
                    None => {
                        // 末尾 1..=3 字节为合法多字节 UTF-8 前缀，暂存至下一帧继续解码。
                        debug_assert!(rest.len() <= 3);
                        carry.extend_from_slice(rest);
                        break;
                    }
                }
            }
        }
    }
    out
}

/// 控制端（Client）远程终端引擎。
pub struct TerminalClientEngine {
    next_term_id: u32,
    outgoing: VecDeque<Vec<u8>>,
    utf8_carry: HashMap<u32, Vec<u8>>,
}

impl Default for TerminalClientEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalClientEngine {
    /// 创建新的控制端远程终端引擎。
    pub fn new() -> Self {
        Self {
            next_term_id: 1,
            outgoing: VecDeque::new(),
            utf8_carry: HashMap::new(),
        }
    }

    fn enqueue(&mut self, msg: &TerminalWireMsg) {
        if let Ok(frame) = msg.encode() {
            self.outgoing.push_back(frame);
        }
    }

    /// 请求打开一个新的远程终端会话，返回分配的 `term_id`。
    pub fn open(&mut self, cols: u16, rows: u16) -> u32 {
        let term_id = self.next_term_id;
        self.next_term_id = self.next_term_id.saturating_add(1);
        self.enqueue(&TerminalWireMsg::Open {
            term_id,
            cols: cols.max(1),
            rows: rows.max(1),
        });
        term_id
    }

    /// 向指定远程终端发送输入字节（超长输入自动按 32 KiB 切片）。
    pub fn send_input(&mut self, term_id: u32, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        for chunk in data.chunks(TERMINAL_DATA_CHUNK_SIZE) {
            self.enqueue(&TerminalWireMsg::Input {
                term_id,
                data: chunk.to_vec(),
            });
        }
    }

    /// 调整指定远程终端窗口尺寸。
    pub fn resize(&mut self, term_id: u32, cols: u16, rows: u16) {
        self.enqueue(&TerminalWireMsg::Resize {
            term_id,
            cols: cols.max(1),
            rows: rows.max(1),
        });
    }

    /// 关闭指定远程终端会话。
    pub fn close(&mut self, term_id: u32) {
        self.utf8_carry.remove(&term_id);
        self.enqueue(&TerminalWireMsg::Close { term_id });
    }

    /// 处理一条来自被控端的线帧，返回面向 UI 的事件列表。
    pub fn handle_incoming(&mut self, frame: &[u8]) -> Vec<TerminalEvent> {
        let msg = match TerminalWireMsg::decode(frame) {
            Ok(m) => m,
            Err(_) => return Vec::new(),
        };

        let mut events = Vec::new();
        match msg {
            TerminalWireMsg::Opened {
                term_id,
                title,
                cwd,
                ok,
                error,
            } => {
                events.push(TerminalEvent::Opened {
                    term_id,
                    title,
                    cwd,
                    ok,
                    error,
                });
            }
            TerminalWireMsg::Output { term_id, data } => {
                let carry = self.utf8_carry.entry(term_id).or_default();
                let text_utf8_lossy = decode_utf8_streaming(carry, &data);
                if carry.is_empty() {
                    self.utf8_carry.remove(&term_id);
                }
                events.push(TerminalEvent::Output {
                    term_id,
                    text_utf8_lossy,
                    bytes: data,
                });
            }
            TerminalWireMsg::Exited { term_id, code } => {
                self.utf8_carry.remove(&term_id);
                events.push(TerminalEvent::Exited { term_id, code });
            }
            // Client 忽略主机侧专用上行帧。
            TerminalWireMsg::Open { .. }
            | TerminalWireMsg::Input { .. }
            | TerminalWireMsg::Resize { .. }
            | TerminalWireMsg::Close { .. } => {}
        }
        events
    }

    /// 最多拉取 `max_frames` 个待发送线帧。
    pub fn poll_outgoing(&mut self, max_frames: usize) -> Vec<Vec<u8>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_wire_roundtrip_and_bounds() {
        let msgs = [
            TerminalWireMsg::Open {
                term_id: 1,
                cols: 120,
                rows: 40,
            },
            TerminalWireMsg::Opened {
                term_id: 1,
                title: "zsh".to_string(),
                cwd: "/tmp".to_string(),
                ok: true,
                error: None,
            },
            TerminalWireMsg::Input {
                term_id: 1,
                data: b"ls -la\n".to_vec(),
            },
            TerminalWireMsg::Output {
                term_id: 1,
                data: vec![b'x'; TERMINAL_DATA_CHUNK_SIZE],
            },
            TerminalWireMsg::Resize {
                term_id: 1,
                cols: 80,
                rows: 24,
            },
            TerminalWireMsg::Close { term_id: 1 },
            TerminalWireMsg::Exited {
                term_id: 1,
                code: 0,
            },
        ];
        for m in msgs {
            let encoded = m.encode().unwrap();
            assert!(encoded.len() <= MAX_TERMINAL_WIRE_FRAME_BYTES);
            assert_eq!(TerminalWireMsg::decode(&encoded).unwrap(), m);
        }

        let oversized = vec![0u8; MAX_TERMINAL_WIRE_FRAME_BYTES + 1];
        assert!(TerminalWireMsg::decode(&oversized).is_err());
    }

    #[test]
    fn split_multibyte_utf8_across_output_chunks_decodes_without_fffd() {
        let mut client = TerminalClientEngine::new();
        let bytes_cjk = "中".as_bytes(); // [0xE4, 0xB8, 0xAD]
        assert_eq!(bytes_cjk.len(), 3);

        // Split "中" across two chunks: [0xE4, 0xB8] then [0xAD]
        let f1 = TerminalWireMsg::Output {
            term_id: 1,
            data: vec![b'A', bytes_cjk[0], bytes_cjk[1]],
        }
        .encode()
        .unwrap();
        let f2 = TerminalWireMsg::Output {
            term_id: 1,
            data: vec![bytes_cjk[2], b'B'],
        }
        .encode()
        .unwrap();

        let ev1 = client.handle_incoming(&f1);
        let ev2 = client.handle_incoming(&f2);

        let mut acc = String::new();
        for ev in ev1.into_iter().chain(ev2) {
            if let TerminalEvent::Output {
                text_utf8_lossy, ..
            } = ev
            {
                assert!(
                    !text_utf8_lossy.contains('\u{FFFD}'),
                    "unexpected U+FFFD in chunk: {text_utf8_lossy:?}"
                );
                acc.push_str(&text_utf8_lossy);
            }
        }
        assert_eq!(acc, "A中B");
        assert!(!client.utf8_carry.contains_key(&1));

        // 4-byte emoji split byte-by-byte across 4 chunks
        let emoji = "🦀".as_bytes();
        assert_eq!(emoji.len(), 4);
        let mut emoji_acc = String::new();
        for &b in emoji {
            let f = TerminalWireMsg::Output {
                term_id: 2,
                data: vec![b],
            }
            .encode()
            .unwrap();
            for ev in client.handle_incoming(&f) {
                if let TerminalEvent::Output {
                    text_utf8_lossy, ..
                } = ev
                {
                    assert!(!text_utf8_lossy.contains('\u{FFFD}'));
                    emoji_acc.push_str(&text_utf8_lossy);
                }
            }
        }
        assert_eq!(emoji_acc, "🦀");

        // Leftover incomplete prefix cleared on Exited and close
        let half = TerminalWireMsg::Output {
            term_id: 3,
            data: vec![0xE4],
        }
        .encode()
        .unwrap();
        let _ = client.handle_incoming(&half);
        assert_eq!(client.utf8_carry.get(&3).map(Vec::len), Some(1));
        let exited = TerminalWireMsg::Exited {
            term_id: 3,
            code: 0,
        }
        .encode()
        .unwrap();
        let _ = client.handle_incoming(&exited);
        assert!(!client.utf8_carry.contains_key(&3));

        let half2 = TerminalWireMsg::Output {
            term_id: 4,
            data: vec![0xE4, 0xB8],
        }
        .encode()
        .unwrap();
        let _ = client.handle_incoming(&half2);
        assert_eq!(client.utf8_carry.get(&4).map(Vec::len), Some(2));
        client.close(4);
        assert!(!client.utf8_carry.contains_key(&4));
    }

    #[cfg(unix)]
    #[test]
    fn host_outgoing_queue_is_capped_at_max_terminal_outgoing_frames() {
        let mut host = TerminalHostEngine::new();
        let open = TerminalWireMsg::Open {
            term_id: 1,
            cols: 80,
            rows: 24,
        }
        .encode()
        .unwrap();
        host.handle_incoming(&open);
        let _ = host.poll_outgoing(4); // consume Opened

        // Pre-fill outgoing queue to MAX_TERMINAL_OUTGOING_FRAMES
        for _ in 0..MAX_TERMINAL_OUTGOING_FRAMES {
            host.outgoing.push_back(vec![1, 2, 3]);
        }
        assert_eq!(host.outgoing.len(), MAX_TERMINAL_OUTGOING_FRAMES);

        // Produce output on the PTY and poll 1 frame: outgoing must never exceed MAX_TERMINAL_OUTGOING_FRAMES.
        let inp = TerminalWireMsg::Input {
            term_id: 1,
            data: b"echo CAP_TEST_LINE\n".to_vec(),
        }
        .encode()
        .unwrap();
        host.handle_incoming(&inp);
        std::thread::sleep(Duration::from_millis(80));

        // Polling 0 returns nothing and does not grow outgoing.
        assert!(host.poll_outgoing(0).is_empty());
        assert_eq!(host.outgoing.len(), MAX_TERMINAL_OUTGOING_FRAMES);

        // Polling 1 pops 1 frame without calling try_read_output while full (so len becomes MAX - 1).
        let polled = host.poll_outgoing(1);
        assert_eq!(polled.len(), 1);
        assert!(host.outgoing.len() <= MAX_TERMINAL_OUTGOING_FRAMES);
    }
}
