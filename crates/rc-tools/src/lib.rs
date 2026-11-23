//! RabbitControl 远控外围工具引擎 crate：
//!
//! - [`file_transfer`]：W2 双向文件传输引擎（本地/远端目录浏览、创建目录、分块流式上传与下载、`.rcpart` 原子落盘）。
//! - [`terminal`]（及 Unix 底层 [`pty_unix`]）：W3 真实 PTY 远程终端引擎（多标签并发 Shell、窗口尺寸同步、优雅退出回收）。
//! - [`portmap`]：W4 TCP 端口映射引擎（本地端口监听、隧道多路复用代理、实时流量统计）。
//!
//! 所有引擎均完全解耦于传输层，仅以 `handle_incoming(&mut self, frame: &[u8])` 与
//! `poll_outgoing(&mut self, max_frames: usize) -> Vec<Vec<u8>>` 交换二进制线帧，
//! 每帧均严格控制在 192 KiB 以内（终端与端口映射 `<= 64 KiB`），可直接接入 `rc-session` 的 `ToolChannel`。

#![deny(missing_docs)]
#![deny(unsafe_code)]

pub mod file_transfer;
pub mod portmap;
#[cfg(unix)]
pub mod pty_unix;
pub mod terminal;

pub use file_transfer::{
    create_local_dir, default_home_dir, list_local_dir, FileClientEngine, FileEntry, FileEvent,
    FileHostEngine, FileWireMsg, TransferDirection, TransferItem, TransferStatus, FILE_CHUNK_SIZE,
    MAX_FILE_WIRE_FRAME_BYTES,
};
pub use portmap::{
    PortMapClientEngine, PortMapHostEngine, PortMapRuleSnapshot, PortMapStatus, PortMapWireMsg,
    HOST_CONNECT_TIMEOUT, MAX_HOST_PROXY_CONNS, MAX_PORTMAP_WIRE_FRAME_BYTES, PORTMAP_CHUNK_SIZE,
};
#[cfg(unix)]
pub use pty_unix::{PtyProcess, PTY_OUTPUT_BUFFER_CAP, PTY_PENDING_WRITE_CAP};
pub use terminal::{
    TerminalClientEngine, TerminalEvent, TerminalHostEngine, TerminalWireMsg,
    MAX_CONCURRENT_TERMINALS, MAX_TERMINAL_WIRE_FRAME_BYTES, TERMINAL_DATA_CHUNK_SIZE,
};
