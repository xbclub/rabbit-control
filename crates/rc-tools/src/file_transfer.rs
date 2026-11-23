//! 双向文件传输引擎（W2）：本地目录浏览、目录创建、分块流式上传/下载与临时 `.rcpart` 原子落盘。
//!
//! 线协议采用紧凑二进制信封 `[kind: u8][header_len: u32 LE][json_header][raw_bytes]`，
//! 单个线帧严格限制在 [`MAX_FILE_WIRE_FRAME_BYTES`]（192 KiB）以内，可直接接入
//! `rc-session` 的 `ToolChannel`（上限 256 KiB）。

#![deny(missing_docs)]
#![deny(unsafe_code)]

use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

static PART_NONCE_SEQ: AtomicU64 = AtomicU64::new(1);

/// 单个文件传输线帧的最大字节数（192 KiB，远低于 ToolChannel 256 KiB 上限）。
pub const MAX_FILE_WIRE_FRAME_BYTES: usize = 192 * 1024;

/// 单个上传/下载数据块的默认大小（64 KiB）。
pub const FILE_CHUNK_SIZE: usize = 64 * 1024;

/// 主机端允许的最大并发上传或并发下载任务数。
const MAX_CONCURRENT_HOST_TRANSFERS: usize = 16;

/// 被控端文件引擎待发队列上限（帧数）。
pub const MAX_FILE_HOST_OUTGOING_FRAMES: usize = 64;

/// 控制端允许的最大并发上传或并发下载任务数。
pub const MAX_CONCURRENT_CLIENT_TRANSFERS: usize = 16;

/// 控制端保留的已结束传输历史上限（超出时按最旧 ID 淘汰）。
pub const MAX_FINISHED_HISTORY: usize = 200;

const FRAME_KIND_CONTROL: u8 = 1;
const FRAME_KIND_UPLOAD_CHUNK: u8 = 2;
const FRAME_KIND_DOWNLOAD_CHUNK: u8 = 3;
const FRAME_HEADER_PREFIX_LEN: usize = 5;

/// 目录条目元数据（用于 UI 展示与远程目录响应）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    /// 文件或目录名（不含父路径）。
    pub name: String,
    /// 完整规范路径。
    pub path: String,
    /// 是否为目录。
    pub is_dir: bool,
    /// 文件字节大小（目录固定为 0）。
    pub size: u64,
    /// 最近修改时间（Unix 秒时间戳）。
    pub modified_sec: u64,
    /// 便于 UI 展示的文件类型标签（如 `"dir"`、`"image"`、`"code"`、`"file"`）。
    pub kind: String,
}

/// 传输方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TransferDirection {
    /// 本地上传至远端主机。
    Upload,
    /// 从远端主机下载至本地。
    Download,
}

/// 传输状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TransferStatus {
    /// 传输进行中。
    Active,
    /// 已成功完成并落盘。
    Done,
    /// 传输失败。
    Failed,
    /// 已被取消。
    Cancelled,
}

/// 单条传输任务快照。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferItem {
    /// 传输任务唯一 ID。
    pub id: u64,
    /// 文件名。
    pub name: String,
    /// 传输方向。
    pub direction: TransferDirection,
    /// 当前状态。
    pub status: TransferStatus,
    /// 总字节数。
    pub total_bytes: u64,
    /// 已完成字节数。
    pub done_bytes: u64,
    /// 源路径。
    pub from_path: String,
    /// 目标路径。
    pub to_path: String,
    /// 失败原因（若有）。
    pub error: Option<String>,
}

/// 文件传输客户端向外发出的 UI 事件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum FileEvent {
    /// 远端目录列表结果。
    RemoteDir {
        /// 规范化后的远端目录路径。
        path: String,
        /// 目录下的条目列表。
        entries: Vec<FileEntry>,
        /// 错误信息（成功时为 `None`）。
        error: Option<String>,
    },
    /// 远端新建目录结果。
    MkdirResult {
        /// 新建目录的完整路径。
        path: String,
        /// 是否创建成功。
        ok: bool,
        /// 错误信息（成功时为 `None`）。
        error: Option<String>,
    },
    /// 传输任务状态或进度更新。
    TransferUpdated(TransferItem),
}

/// 文件传输线协议消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileWireMsg {
    /// 请求列举远端目录。
    ListDir {
        /// 目标路径；为 `None` 时使用远端默认主目录。
        path: Option<String>,
    },
    /// 远端目录列举响应。
    ListDirResp {
        /// 目录规范路径。
        path: String,
        /// 条目列表。
        entries: Vec<FileEntry>,
        /// 错误信息。
        error: Option<String>,
    },
    /// 请求在远端创建目录。
    Mkdir {
        /// 父目录路径。
        parent: String,
        /// 新目录名（单段合法名称）。
        name: String,
    },
    /// 远端创建目录响应。
    MkdirResp {
        /// 目录完整路径。
        path: String,
        /// 是否成功。
        ok: bool,
        /// 错误信息。
        error: Option<String>,
    },
    /// 客户端请求开始上传文件。
    UploadStart {
        /// 传输 ID。
        id: u64,
        /// 远端目标目录。
        remote_dir: String,
        /// 目标文件名（单段合法名称）。
        file_name: String,
        /// 文件总大小（字节）。
        total_bytes: u64,
    },
    /// 主机对上传请求的确认响应。
    UploadAck {
        /// 传输 ID。
        id: u64,
        /// 是否接受上传。
        ok: bool,
        /// 拒绝或失败原因。
        error: Option<String>,
    },
    /// 上传数据块。
    UploadChunk {
        /// 传输 ID。
        id: u64,
        /// 当前数据块在文件中的字节偏移。
        offset: u64,
        /// 原始二进制数据。
        data: Vec<u8>,
    },
    /// 客户端通知上传数据已全部发送完毕。
    UploadFinish {
        /// 传输 ID。
        id: u64,
    },
    /// 主机通知上传落盘结果。
    UploadDone {
        /// 传输 ID。
        id: u64,
        /// 是否成功落盘。
        ok: bool,
        /// 错误信息。
        error: Option<String>,
    },
    /// 客户端请求开始下载远端文件。
    DownloadStart {
        /// 传输 ID。
        id: u64,
        /// 远端文件路径。
        remote_path: String,
    },
    /// 主机返回待下载文件的元数据。
    DownloadMeta {
        /// 传输 ID。
        id: u64,
        /// 文件名。
        file_name: String,
        /// 文件总字节数。
        total_bytes: u64,
        /// 是否可以开始下载。
        ok: bool,
        /// 错误信息。
        error: Option<String>,
    },
    /// 下载数据块。
    DownloadChunk {
        /// 传输 ID。
        id: u64,
        /// 当前数据块在文件中的字节偏移。
        offset: u64,
        /// 原始二进制数据。
        data: Vec<u8>,
    },
    /// 主机通知下载数据流结束。
    DownloadFinish {
        /// 传输 ID。
        id: u64,
    },
    /// 取消进行中的上传或下载，触发 `.rcpart` 清理。
    CancelTransfer {
        /// 传输 ID。
        id: u64,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case")]
enum ControlPayload {
    ListDir {
        path: Option<String>,
    },
    ListDirResp {
        path: String,
        entries: Vec<FileEntry>,
        error: Option<String>,
    },
    Mkdir {
        parent: String,
        name: String,
    },
    MkdirResp {
        path: String,
        ok: bool,
        error: Option<String>,
    },
    UploadStart {
        id: u64,
        remote_dir: String,
        file_name: String,
        total_bytes: u64,
    },
    UploadAck {
        id: u64,
        ok: bool,
        error: Option<String>,
    },
    UploadFinish {
        id: u64,
    },
    UploadDone {
        id: u64,
        ok: bool,
        error: Option<String>,
    },
    DownloadStart {
        id: u64,
        remote_path: String,
    },
    DownloadMeta {
        id: u64,
        file_name: String,
        total_bytes: u64,
        ok: bool,
        error: Option<String>,
    },
    DownloadFinish {
        id: u64,
    },
    CancelTransfer {
        id: u64,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct ChunkHeader {
    id: u64,
    offset: u64,
}

impl FileWireMsg {
    /// 将消息编码为长度受控的二进制线帧（`<= 192 KiB`）。
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        match self {
            FileWireMsg::UploadChunk { id, offset, data } => {
                encode_chunk_frame(FRAME_KIND_UPLOAD_CHUNK, *id, *offset, data)
            }
            FileWireMsg::DownloadChunk { id, offset, data } => {
                encode_chunk_frame(FRAME_KIND_DOWNLOAD_CHUNK, *id, *offset, data)
            }
            FileWireMsg::ListDirResp {
                path,
                entries,
                error,
            } => encode_list_dir_resp(path, entries, error),
            other => {
                let payload = match other {
                    FileWireMsg::ListDir { path } => ControlPayload::ListDir { path: path.clone() },
                    FileWireMsg::Mkdir { parent, name } => ControlPayload::Mkdir {
                        parent: parent.clone(),
                        name: name.clone(),
                    },
                    FileWireMsg::MkdirResp { path, ok, error } => ControlPayload::MkdirResp {
                        path: path.clone(),
                        ok: *ok,
                        error: error.clone(),
                    },
                    FileWireMsg::UploadStart {
                        id,
                        remote_dir,
                        file_name,
                        total_bytes,
                    } => ControlPayload::UploadStart {
                        id: *id,
                        remote_dir: remote_dir.clone(),
                        file_name: file_name.clone(),
                        total_bytes: *total_bytes,
                    },
                    FileWireMsg::UploadAck { id, ok, error } => ControlPayload::UploadAck {
                        id: *id,
                        ok: *ok,
                        error: error.clone(),
                    },
                    FileWireMsg::UploadFinish { id } => ControlPayload::UploadFinish { id: *id },
                    FileWireMsg::UploadDone { id, ok, error } => ControlPayload::UploadDone {
                        id: *id,
                        ok: *ok,
                        error: error.clone(),
                    },
                    FileWireMsg::DownloadStart { id, remote_path } => {
                        ControlPayload::DownloadStart {
                            id: *id,
                            remote_path: remote_path.clone(),
                        }
                    }
                    FileWireMsg::DownloadMeta {
                        id,
                        file_name,
                        total_bytes,
                        ok,
                        error,
                    } => ControlPayload::DownloadMeta {
                        id: *id,
                        file_name: file_name.clone(),
                        total_bytes: *total_bytes,
                        ok: *ok,
                        error: error.clone(),
                    },
                    FileWireMsg::DownloadFinish { id } => {
                        ControlPayload::DownloadFinish { id: *id }
                    }
                    FileWireMsg::CancelTransfer { id } => {
                        ControlPayload::CancelTransfer { id: *id }
                    }
                    FileWireMsg::UploadChunk { .. }
                    | FileWireMsg::DownloadChunk { .. }
                    | FileWireMsg::ListDirResp { .. } => unreachable!(),
                };
                encode_control_frame(&payload)
            }
        }
    }

    /// 严格校验并解码二进制线帧。
    pub fn decode(frame: &[u8]) -> Result<Self, String> {
        if frame.len() < FRAME_HEADER_PREFIX_LEN {
            return Err("file wire frame too short".to_string());
        }
        if frame.len() > MAX_FILE_WIRE_FRAME_BYTES {
            return Err(format!(
                "file wire frame exceeds {} bytes limit ({} bytes)",
                MAX_FILE_WIRE_FRAME_BYTES,
                frame.len()
            ));
        }

        let kind = frame[0];
        let hdr_len = u32::from_le_bytes([frame[1], frame[2], frame[3], frame[4]]) as usize;
        let hdr_end = FRAME_HEADER_PREFIX_LEN
            .checked_add(hdr_len)
            .ok_or_else(|| "file wire header length overflow".to_string())?;
        if hdr_end > frame.len() {
            return Err("file wire header length out of bounds".to_string());
        }

        let header_bytes = &frame[FRAME_HEADER_PREFIX_LEN..hdr_end];
        let raw_bytes = &frame[hdr_end..];

        match kind {
            FRAME_KIND_CONTROL => {
                if !raw_bytes.is_empty() {
                    return Err("file control frame must not contain trailing payload".to_string());
                }
                let payload: ControlPayload = serde_json::from_slice(header_bytes)
                    .map_err(|e| format!("invalid file control json: {e}"))?;
                Ok(match payload {
                    ControlPayload::ListDir { path } => FileWireMsg::ListDir { path },
                    ControlPayload::ListDirResp {
                        path,
                        entries,
                        error,
                    } => FileWireMsg::ListDirResp {
                        path,
                        entries,
                        error,
                    },
                    ControlPayload::Mkdir { parent, name } => FileWireMsg::Mkdir { parent, name },
                    ControlPayload::MkdirResp { path, ok, error } => {
                        FileWireMsg::MkdirResp { path, ok, error }
                    }
                    ControlPayload::UploadStart {
                        id,
                        remote_dir,
                        file_name,
                        total_bytes,
                    } => FileWireMsg::UploadStart {
                        id,
                        remote_dir,
                        file_name,
                        total_bytes,
                    },
                    ControlPayload::UploadAck { id, ok, error } => {
                        FileWireMsg::UploadAck { id, ok, error }
                    }
                    ControlPayload::UploadFinish { id } => FileWireMsg::UploadFinish { id },
                    ControlPayload::UploadDone { id, ok, error } => {
                        FileWireMsg::UploadDone { id, ok, error }
                    }
                    ControlPayload::DownloadStart { id, remote_path } => {
                        FileWireMsg::DownloadStart { id, remote_path }
                    }
                    ControlPayload::DownloadMeta {
                        id,
                        file_name,
                        total_bytes,
                        ok,
                        error,
                    } => FileWireMsg::DownloadMeta {
                        id,
                        file_name,
                        total_bytes,
                        ok,
                        error,
                    },
                    ControlPayload::DownloadFinish { id } => FileWireMsg::DownloadFinish { id },
                    ControlPayload::CancelTransfer { id } => FileWireMsg::CancelTransfer { id },
                })
            }
            FRAME_KIND_UPLOAD_CHUNK => {
                let hdr: ChunkHeader = serde_json::from_slice(header_bytes)
                    .map_err(|e| format!("invalid upload chunk header: {e}"))?;
                Ok(FileWireMsg::UploadChunk {
                    id: hdr.id,
                    offset: hdr.offset,
                    data: raw_bytes.to_vec(),
                })
            }
            FRAME_KIND_DOWNLOAD_CHUNK => {
                let hdr: ChunkHeader = serde_json::from_slice(header_bytes)
                    .map_err(|e| format!("invalid download chunk header: {e}"))?;
                Ok(FileWireMsg::DownloadChunk {
                    id: hdr.id,
                    offset: hdr.offset,
                    data: raw_bytes.to_vec(),
                })
            }
            _ => Err(format!("unknown file wire frame kind: {kind}")),
        }
    }
}

fn encode_control_frame(payload: &ControlPayload) -> Result<Vec<u8>, String> {
    let header =
        serde_json::to_vec(payload).map_err(|e| format!("serialize control failed: {e}"))?;
    let total = FRAME_HEADER_PREFIX_LEN + header.len();
    if total > MAX_FILE_WIRE_FRAME_BYTES {
        return Err(format!(
            "control frame too large ({total} > {MAX_FILE_WIRE_FRAME_BYTES})"
        ));
    }
    let mut out = Vec::with_capacity(total);
    out.push(FRAME_KIND_CONTROL);
    out.extend_from_slice(&(header.len() as u32).to_le_bytes());
    out.extend_from_slice(&header);
    Ok(out)
}

fn encode_list_dir_resp(
    path: &str,
    entries: &[FileEntry],
    error: &Option<String>,
) -> Result<Vec<u8>, String> {
    let payload = ControlPayload::ListDirResp {
        path: path.to_string(),
        entries: entries.to_vec(),
        error: error.clone(),
    };
    if let Ok(frame) = encode_control_frame(&payload) {
        return Ok(frame);
    }

    // 超大目录自动截断到线帧上限以内，确保永不违反 192 KiB 约束。
    let mut lo = 0usize;
    let mut hi = entries.len();
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        let candidate = ControlPayload::ListDirResp {
            path: path.to_string(),
            entries: entries[..mid].to_vec(),
            error: error.clone(),
        };
        if encode_control_frame(&candidate).is_ok() {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let trimmed = ControlPayload::ListDirResp {
        path: path.to_string(),
        entries: entries[..lo].to_vec(),
        error: error.clone(),
    };
    encode_control_frame(&trimmed)
}

fn encode_chunk_frame(kind: u8, id: u64, offset: u64, data: &[u8]) -> Result<Vec<u8>, String> {
    let header = serde_json::to_vec(&ChunkHeader { id, offset })
        .map_err(|e| format!("serialize chunk header failed: {e}"))?;
    let total = FRAME_HEADER_PREFIX_LEN + header.len() + data.len();
    if total > MAX_FILE_WIRE_FRAME_BYTES {
        return Err(format!(
            "chunk frame too large ({total} > {MAX_FILE_WIRE_FRAME_BYTES})"
        ));
    }
    let mut out = Vec::with_capacity(total);
    out.push(kind);
    out.extend_from_slice(&(header.len() as u32).to_le_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(data);
    Ok(out)
}

/// 返回当前用户的默认主目录（依次检查 `HOME`、`USERPROFILE`、当前工作目录、`/`）。
pub fn default_home_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(home);
    }
    if let Some(profile) = std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()) {
        return PathBuf::from(profile);
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
}

/// 校验单个文件或目录名：严禁空串、`.`、`..`、路径分隔符 `/` `\` 与 `\0`。
fn validate_single_name(name: &str) -> Result<&str, String> {
    if name.is_empty() || name.trim().is_empty() {
        return Err("name must not be empty".to_string());
    }
    if name == "." || name == ".." {
        return Err("name must not be '.' or '..'".to_string());
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return Err("name must not contain path separators or NUL bytes".to_string());
    }
    let mut comps = Path::new(name).components();
    match (comps.next(), comps.next()) {
        (Some(Component::Normal(_)), None) => Ok(name),
        _ => Err("name must be a single normal path segment".to_string()),
    }
}

fn expand_tilde(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "~" {
        return default_home_dir();
    }
    if let Some(rest) = trimmed
        .strip_prefix("~/")
        .or_else(|| trimmed.strip_prefix("~\\"))
    {
        return default_home_dir().join(rest);
    }
    PathBuf::from(trimmed)
}

fn classify_file_kind(path: &Path, is_dir: bool) -> String {
    if is_dir {
        return "dir".to_string();
    }
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase());
    let kind = match ext.as_deref() {
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "ico" | "heic" | "tiff") => {
            "image"
        }
        Some("mp4" | "mov" | "mkv" | "avi" | "webm" | "flv" | "wmv") => "video",
        Some("mp3" | "wav" | "flac" | "aac" | "ogg" | "m4a" | "opus") => "audio",
        Some("zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "7z" | "rar" | "zst") => "archive",
        Some("pdf") => "pdf",
        Some(
            "rs" | "ts" | "tsx" | "js" | "jsx" | "py" | "go" | "c" | "cc" | "cpp" | "h" | "hpp"
            | "java" | "kt" | "swift" | "rb" | "sh" | "zsh" | "bash" | "toml" | "yaml" | "yml"
            | "json" | "xml" | "html" | "css" | "scss" | "sql" | "md",
        ) => "code",
        Some("txt" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "csv" | "rtf") => "doc",
        _ => "file",
    };
    kind.to_string()
}

/// 列举本地目录内容：返回规范化后的目录路径以及按「目录优先、名称字母序」排列的条目。
pub fn list_local_dir(path: &str) -> Result<(String, Vec<FileEntry>), String> {
    let target = expand_tilde(path);
    let canonical = fs::canonicalize(&target)
        .map_err(|e| format!("failed to resolve directory '{}': {e}", target.display()))?;
    if !canonical.is_dir() {
        return Err(format!("'{}' is not a directory", canonical.display()));
    }

    let read_dir = fs::read_dir(&canonical)
        .map_err(|e| format!("failed to read directory '{}': {e}", canonical.display()))?;

    let mut entries = Vec::new();
    for entry_res in read_dir {
        let entry = match entry_res {
            Ok(e) => e,
            Err(_) => continue,
        };
        let entry_path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let meta = entry
            .metadata()
            .or_else(|_| fs::symlink_metadata(&entry_path));
        let (is_dir, size, modified_sec) = match meta {
            Ok(m) => {
                let is_dir = m.is_dir();
                let size = if is_dir { 0 } else { m.len() };
                let modified_sec = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                (is_dir, size, modified_sec)
            }
            Err(_) => (false, 0, 0),
        };
        let kind = classify_file_kind(&entry_path, is_dir);
        entries.push(FileEntry {
            name,
            path: entry_path.to_string_lossy().into_owned(),
            is_dir,
            size,
            modified_sec,
            kind,
        });
    }

    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });

    Ok((canonical.to_string_lossy().into_owned(), entries))
}

/// 在本地 `parent` 目录下安全创建单个子目录 `name`，拒绝路径穿越与非法分隔符。
pub fn create_local_dir(parent: &str, name: &str) -> Result<String, String> {
    let valid_name = validate_single_name(name)?;
    let parent_path = expand_tilde(parent);
    let canonical_parent = fs::canonicalize(&parent_path).map_err(|e| {
        format!(
            "failed to resolve parent directory '{}': {e}",
            parent_path.display()
        )
    })?;
    if !canonical_parent.is_dir() {
        return Err(format!(
            "'{}' is not a directory",
            canonical_parent.display()
        ));
    }
    let target = canonical_parent.join(valid_name);
    fs::create_dir(&target)
        .map_err(|e| format!("failed to create directory '{}': {e}", target.display()))?;
    Ok(target.to_string_lossy().into_owned())
}

fn part_file_path(dir: &Path, id: u64) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = PART_NONCE_SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    dir.join(format!(".rcpart.{id}.{pid}.{nanos:x}.{seq:x}"))
}

#[cfg(unix)]
fn open_regular_file_safe(path: &Path) -> Result<(File, fs::Metadata), String> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| format!("failed to open file '{}': {e}", path.display()))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("failed to stat '{}': {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("'{}' is not a regular file", path.display()));
    }
    crate::pty_unix::clear_nonblock_fd(file.as_raw_fd())
        .map_err(|e| format!("failed to clear O_NONBLOCK on '{}': {e}", path.display()))?;
    Ok((file, meta))
}

#[cfg(not(unix))]
fn open_regular_file_safe(path: &Path) -> Result<(File, fs::Metadata), String> {
    let file =
        File::open(path).map_err(|e| format!("failed to open file '{}': {e}", path.display()))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("failed to stat '{}': {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("'{}' is not a regular file", path.display()));
    }
    Ok((file, meta))
}

fn read_retry_eintr(file: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    loop {
        match file.read(buf) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            other => return other,
        }
    }
}

struct HostActiveUpload {
    file: File,
    part_path: PathBuf,
    final_path: PathBuf,
    total_bytes: u64,
    received_bytes: u64,
}

struct HostActiveDownload {
    file: File,
    offset: u64,
    total_bytes: u64,
}

/// 被控端（Host）文件传输引擎。
///
/// 负责响应目录查询、创建目录、接收分块上传并原子落盘，以及平滑节奏的流式下载。
/// 析构时自动清理所有未完成的 `.rcpart` 临时文件。
pub struct FileHostEngine {
    outgoing: VecDeque<Vec<u8>>,
    uploads: BTreeMap<u64, HostActiveUpload>,
    downloads: BTreeMap<u64, HostActiveDownload>,
    download_rr_cursor: u64,
}

impl Default for FileHostEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl FileHostEngine {
    /// 创建新的被控端文件传输引擎。
    pub fn new() -> Self {
        Self {
            outgoing: VecDeque::new(),
            uploads: BTreeMap::new(),
            downloads: BTreeMap::new(),
            download_rr_cursor: 0,
        }
    }

    /// 返回当前进行中的上传任务数量。
    pub fn active_upload_count(&self) -> usize {
        self.uploads.len()
    }

    /// 返回当前进行中的下载任务数量。
    pub fn active_download_count(&self) -> usize {
        self.downloads.len()
    }

    fn enqueue_critical(&mut self, msg: &FileWireMsg) {
        if let Ok(frame) = msg.encode() {
            while self.outgoing.len() >= MAX_FILE_HOST_OUTGOING_FRAMES {
                if let Some(pos) = self
                    .outgoing
                    .iter()
                    .position(|f| f.first().copied() == Some(FRAME_KIND_DOWNLOAD_CHUNK))
                {
                    self.outgoing.remove(pos);
                } else {
                    self.outgoing.pop_front();
                }
            }
            self.outgoing.push_back(frame);
        }
    }

    /// 处理一条来自控制端的二进制线帧。
    pub fn handle_incoming(&mut self, frame: &[u8]) {
        let msg = match FileWireMsg::decode(frame) {
            Ok(m) => m,
            Err(_) => return,
        };

        match msg {
            FileWireMsg::ListDir { path } => {
                let target = path.as_deref().unwrap_or("");
                match list_local_dir(target) {
                    Ok((canonical, entries)) => {
                        self.enqueue_critical(&FileWireMsg::ListDirResp {
                            path: canonical,
                            entries,
                            error: None,
                        });
                    }
                    Err(err) => {
                        self.enqueue_critical(&FileWireMsg::ListDirResp {
                            path: target.to_string(),
                            entries: Vec::new(),
                            error: Some(err),
                        });
                    }
                }
            }
            FileWireMsg::Mkdir { parent, name } => match create_local_dir(&parent, &name) {
                Ok(created) => {
                    self.enqueue_critical(&FileWireMsg::MkdirResp {
                        path: created,
                        ok: true,
                        error: None,
                    });
                }
                Err(err) => {
                    self.enqueue_critical(&FileWireMsg::MkdirResp {
                        path: parent,
                        ok: false,
                        error: Some(err),
                    });
                }
            },
            FileWireMsg::UploadStart {
                id,
                remote_dir,
                file_name,
                total_bytes,
            } => {
                self.handle_upload_start(id, &remote_dir, &file_name, total_bytes);
            }
            FileWireMsg::UploadChunk { id, offset, data } => {
                self.handle_upload_chunk(id, offset, &data);
            }
            FileWireMsg::UploadFinish { id } => {
                self.handle_upload_finish(id);
            }
            FileWireMsg::DownloadStart { id, remote_path } => {
                self.handle_download_start(id, &remote_path);
            }
            FileWireMsg::CancelTransfer { id } => {
                self.cancel_host_transfer(id);
            }
            // Host 忽略客户端侧专用响应帧。
            FileWireMsg::ListDirResp { .. }
            | FileWireMsg::MkdirResp { .. }
            | FileWireMsg::UploadAck { .. }
            | FileWireMsg::UploadDone { .. }
            | FileWireMsg::DownloadMeta { .. }
            | FileWireMsg::DownloadChunk { .. }
            | FileWireMsg::DownloadFinish { .. } => {}
        }
    }

    fn handle_upload_start(
        &mut self,
        id: u64,
        remote_dir: &str,
        file_name: &str,
        total_bytes: u64,
    ) {
        if self.uploads.len() >= MAX_CONCURRENT_HOST_TRANSFERS {
            self.enqueue_critical(&FileWireMsg::UploadAck {
                id,
                ok: false,
                error: Some("too many concurrent uploads".to_string()),
            });
            return;
        }
        if self.uploads.contains_key(&id) || self.downloads.contains_key(&id) {
            self.enqueue_critical(&FileWireMsg::UploadAck {
                id,
                ok: false,
                error: Some("duplicate transfer id".to_string()),
            });
            return;
        }
        let valid_name = match validate_single_name(file_name) {
            Ok(n) => n,
            Err(err) => {
                self.enqueue_critical(&FileWireMsg::UploadAck {
                    id,
                    ok: false,
                    error: Some(err),
                });
                return;
            }
        };

        let dir_path = expand_tilde(remote_dir);
        let canonical_dir = match fs::canonicalize(&dir_path) {
            Ok(p) if p.is_dir() => p,
            Ok(p) => {
                self.enqueue_critical(&FileWireMsg::UploadAck {
                    id,
                    ok: false,
                    error: Some(format!("'{}' is not a directory", p.display())),
                });
                return;
            }
            Err(e) => {
                self.enqueue_critical(&FileWireMsg::UploadAck {
                    id,
                    ok: false,
                    error: Some(format!("invalid remote_dir '{}': {e}", dir_path.display())),
                });
                return;
            }
        };

        let part_path = part_file_path(&canonical_dir, id);
        let final_path = canonical_dir.join(valid_name);
        let file = match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&part_path)
        {
            Ok(f) => f,
            Err(e) => {
                self.enqueue_critical(&FileWireMsg::UploadAck {
                    id,
                    ok: false,
                    error: Some(format!("failed to create temp file: {e}")),
                });
                return;
            }
        };

        self.uploads.insert(
            id,
            HostActiveUpload {
                file,
                part_path,
                final_path,
                total_bytes,
                received_bytes: 0,
            },
        );
        self.enqueue_critical(&FileWireMsg::UploadAck {
            id,
            ok: true,
            error: None,
        });
    }

    fn handle_upload_chunk(&mut self, id: u64, offset: u64, data: &[u8]) {
        let err_msg = match self.uploads.get_mut(&id) {
            None => return,
            Some(up) => {
                if offset != up.received_bytes {
                    Some(format!(
                        "upload offset mismatch: expected {}, got {offset}",
                        up.received_bytes
                    ))
                } else if up
                    .received_bytes
                    .checked_add(data.len() as u64)
                    .map_or(true, |next| next > up.total_bytes)
                {
                    Some("upload exceeded declared total_bytes".to_string())
                } else if let Err(e) = up.file.write_all(data) {
                    Some(format!("write upload chunk failed: {e}"))
                } else {
                    up.received_bytes += data.len() as u64;
                    None
                }
            }
        };

        if let Some(err) = err_msg {
            if let Some(up) = self.uploads.remove(&id) {
                drop(up.file);
                let _ = fs::remove_file(&up.part_path);
            }
            self.enqueue_critical(&FileWireMsg::UploadDone {
                id,
                ok: false,
                error: Some(err),
            });
        }
    }

    fn handle_upload_finish(&mut self, id: u64) {
        let Some(up) = self.uploads.remove(&id) else {
            self.enqueue_critical(&FileWireMsg::UploadDone {
                id,
                ok: false,
                error: Some("upload session not found".to_string()),
            });
            return;
        };

        if up.received_bytes != up.total_bytes {
            drop(up.file);
            let _ = fs::remove_file(&up.part_path);
            self.enqueue_critical(&FileWireMsg::UploadDone {
                id,
                ok: false,
                error: Some(format!(
                    "incomplete upload: received {} of {} bytes",
                    up.received_bytes, up.total_bytes
                )),
            });
            return;
        }

        if let Err(e) = up.file.sync_all() {
            drop(up.file);
            let _ = fs::remove_file(&up.part_path);
            self.enqueue_critical(&FileWireMsg::UploadDone {
                id,
                ok: false,
                error: Some(format!("sync temp file failed: {e}")),
            });
            return;
        }
        drop(up.file);

        if let Err(e) = fs::rename(&up.part_path, &up.final_path) {
            let _ = fs::remove_file(&up.part_path);
            self.enqueue_critical(&FileWireMsg::UploadDone {
                id,
                ok: false,
                error: Some(format!("atomic rename failed: {e}")),
            });
            return;
        }

        self.enqueue_critical(&FileWireMsg::UploadDone {
            id,
            ok: true,
            error: None,
        });
    }

    fn handle_download_start(&mut self, id: u64, remote_path: &str) {
        if self.downloads.len() >= MAX_CONCURRENT_HOST_TRANSFERS {
            self.enqueue_critical(&FileWireMsg::DownloadMeta {
                id,
                file_name: String::new(),
                total_bytes: 0,
                ok: false,
                error: Some("too many concurrent downloads".to_string()),
            });
            return;
        }
        if self.downloads.contains_key(&id) || self.uploads.contains_key(&id) {
            self.enqueue_critical(&FileWireMsg::DownloadMeta {
                id,
                file_name: String::new(),
                total_bytes: 0,
                ok: false,
                error: Some("duplicate transfer id".to_string()),
            });
            return;
        }

        let target = expand_tilde(remote_path);
        // 使用请求路径的 basename（而非符号链接规范化后的目标名），避免远端意外改写客户端落盘文件名。
        let file_name = match target
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| "invalid UTF-8 file name".to_string())
            .and_then(|n| validate_single_name(n).map(str::to_string))
        {
            Ok(n) => n,
            Err(err) => {
                self.enqueue_critical(&FileWireMsg::DownloadMeta {
                    id,
                    file_name: String::new(),
                    total_bytes: 0,
                    ok: false,
                    error: Some(err),
                });
                return;
            }
        };

        let canonical = match fs::canonicalize(&target) {
            Ok(p) => p,
            Err(e) => {
                self.enqueue_critical(&FileWireMsg::DownloadMeta {
                    id,
                    file_name: String::new(),
                    total_bytes: 0,
                    ok: false,
                    error: Some(format!(
                        "failed to resolve file '{}': {e}",
                        target.display()
                    )),
                });
                return;
            }
        };

        let (file, meta) = match open_regular_file_safe(&canonical) {
            Ok(pair) => pair,
            Err(err) => {
                self.enqueue_critical(&FileWireMsg::DownloadMeta {
                    id,
                    file_name: String::new(),
                    total_bytes: 0,
                    ok: false,
                    error: Some(err),
                });
                return;
            }
        };

        let total_bytes = meta.len();
        self.enqueue_critical(&FileWireMsg::DownloadMeta {
            id,
            file_name,
            total_bytes,
            ok: true,
            error: None,
        });
        self.downloads.insert(
            id,
            HostActiveDownload {
                file,
                offset: 0,
                total_bytes,
            },
        );
    }

    fn cancel_host_transfer(&mut self, id: u64) {
        if let Some(up) = self.uploads.remove(&id) {
            drop(up.file);
            let _ = fs::remove_file(&up.part_path);
        }
        self.downloads.remove(&id);
    }

    /// 最多拉取 `max_frames` 个待发送线帧，并对下载数据块实施平滑轮询节流。
    pub fn poll_outgoing(&mut self, max_frames: usize) -> Vec<Vec<u8>> {
        if max_frames == 0 {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(max_frames.min(16));

        while out.len() < max_frames {
            if let Some(frame) = self.outgoing.pop_front() {
                out.push(frame);
                continue;
            }
            if self.downloads.is_empty() {
                break;
            }
            if !self.pump_one_download_chunk(&mut out) {
                break;
            }
        }
        out
    }

    fn pump_one_download_chunk(&mut self, out: &mut Vec<Vec<u8>>) -> bool {
        let next_id = self
            .downloads
            .range((
                std::ops::Bound::Excluded(self.download_rr_cursor),
                std::ops::Bound::Unbounded,
            ))
            .next()
            .or_else(|| self.downloads.iter().next())
            .map(|(&id, _)| id);

        let Some(id) = next_id else {
            return false;
        };
        self.download_rr_cursor = id;

        let mut buf = vec![0u8; FILE_CHUNK_SIZE];
        let mut finished = false;
        let mut failed = false;
        let mut chunk_msg = None;

        if let Some(dl) = self.downloads.get_mut(&id) {
            if dl.offset >= dl.total_bytes {
                finished = true;
            } else {
                let to_read = (dl.total_bytes - dl.offset).min(FILE_CHUNK_SIZE as u64) as usize;
                match read_retry_eintr(&mut dl.file, &mut buf[..to_read]) {
                    Ok(0) => {
                        finished = true;
                    }
                    Ok(n) => {
                        buf.truncate(n);
                        let offset = dl.offset;
                        dl.offset += n as u64;
                        chunk_msg = Some(FileWireMsg::DownloadChunk {
                            id,
                            offset,
                            data: buf,
                        });
                    }
                    Err(_) => {
                        failed = true;
                    }
                }
            }
        }

        if let Some(msg) = chunk_msg {
            if let Ok(frame) = msg.encode() {
                out.push(frame);
            }
            return true;
        }

        if finished {
            self.downloads.remove(&id);
            if let Ok(frame) = (FileWireMsg::DownloadFinish { id }).encode() {
                out.push(frame);
            }
            return true;
        }

        if failed {
            self.downloads.remove(&id);
            if let Ok(frame) = (FileWireMsg::CancelTransfer { id }).encode() {
                out.push(frame);
            }
            return true;
        }

        false
    }
}

impl Drop for FileHostEngine {
    fn drop(&mut self) {
        let uploads = std::mem::take(&mut self.uploads);
        for (_, up) in uploads {
            drop(up.file);
            let _ = fs::remove_file(&up.part_path);
        }
        self.downloads.clear();
    }
}

struct ClientActiveUpload {
    file: File,
    offset: u64,
    total_bytes: u64,
    acked: bool,
    finish_sent: bool,
}

struct ClientActiveDownload {
    local_dir: PathBuf,
    expected_file_name: String,
    file: Option<File>,
    part_path: Option<PathBuf>,
    final_path: Option<PathBuf>,
    total_bytes: u64,
    received_bytes: u64,
}

/// 控制端（Client）文件传输引擎。
///
/// 管理目录浏览请求、上传/下载任务队列与状态快照，产出面向 UI 的 [`FileEvent`]。
/// 析构时自动清理所有未完成的本地下载 `.rcpart` 临时文件。
pub struct FileClientEngine {
    next_id: u64,
    outgoing: VecDeque<Vec<u8>>,
    local_events: Vec<FileEvent>,
    transfers: BTreeMap<u64, TransferItem>,
    uploads: BTreeMap<u64, ClientActiveUpload>,
    downloads: BTreeMap<u64, ClientActiveDownload>,
    upload_rr_cursor: u64,
}

impl Default for FileClientEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl FileClientEngine {
    /// 创建新的控制端文件传输引擎。
    pub fn new() -> Self {
        Self {
            next_id: 1,
            outgoing: VecDeque::new(),
            local_events: Vec::new(),
            transfers: BTreeMap::new(),
            uploads: BTreeMap::new(),
            downloads: BTreeMap::new(),
            upload_rr_cursor: 0,
        }
    }

    /// 拉取本地状态变更产出的事件（例如上传读盘失败时立即触发的 [`FileEvent::TransferUpdated`]）。
    pub fn drain_local_events(&mut self) -> Vec<FileEvent> {
        std::mem::take(&mut self.local_events)
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    fn enqueue(&mut self, msg: &FileWireMsg) {
        if let Ok(frame) = msg.encode() {
            self.outgoing.push_back(frame);
        }
    }

    /// 请求列举远端目录内容。
    pub fn request_list_dir(&mut self, path: Option<String>) {
        self.enqueue(&FileWireMsg::ListDir { path });
    }

    /// 请求在远端 `parent` 目录下创建子目录 `name`。
    pub fn request_mkdir(&mut self, parent: String, name: String) {
        self.enqueue(&FileWireMsg::Mkdir { parent, name });
    }

    fn prune_finished_history(&mut self) {
        let finished_ids: Vec<u64> = self
            .transfers
            .iter()
            .filter(|(_, item)| item.status != TransferStatus::Active)
            .map(|(&id, _)| id)
            .collect();
        if finished_ids.len() > MAX_FINISHED_HISTORY {
            let excess = finished_ids.len() - MAX_FINISHED_HISTORY;
            for id in &finished_ids[..excess] {
                self.transfers.remove(id);
            }
        }
    }

    /// 启动一次本地文件上传，返回分配的传输任务 ID。
    pub fn start_upload(&mut self, local_path: &str, remote_dir: &str) -> Result<u64, String> {
        if self.uploads.len() >= MAX_CONCURRENT_CLIENT_TRANSFERS {
            return Err(format!(
                "too many concurrent uploads (max {MAX_CONCURRENT_CLIENT_TRANSFERS})"
            ));
        }

        let local = expand_tilde(local_path);
        let canonical = fs::canonicalize(&local)
            .map_err(|e| format!("failed to resolve local file '{}': {e}", local.display()))?;

        let file_name = canonical
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| "invalid UTF-8 local file name".to_string())?;
        let valid_name = validate_single_name(file_name)?.to_string();

        let (file, meta) = open_regular_file_safe(&canonical)?;
        let total_bytes = meta.len();
        let id = self.alloc_id();

        let to_path = if remote_dir.is_empty() {
            valid_name.clone()
        } else {
            let trimmed = remote_dir.trim_end_matches(['/', '\\']);
            format!("{trimmed}/{valid_name}")
        };

        self.transfers.insert(
            id,
            TransferItem {
                id,
                name: valid_name.clone(),
                direction: TransferDirection::Upload,
                status: TransferStatus::Active,
                total_bytes,
                done_bytes: 0,
                from_path: canonical.to_string_lossy().into_owned(),
                to_path,
                error: None,
            },
        );
        self.uploads.insert(
            id,
            ClientActiveUpload {
                file,
                offset: 0,
                total_bytes,
                acked: false,
                finish_sent: false,
            },
        );
        self.enqueue(&FileWireMsg::UploadStart {
            id,
            remote_dir: remote_dir.to_string(),
            file_name: valid_name,
            total_bytes,
        });
        Ok(id)
    }

    /// 启动一次远端文件下载到本地目录，返回分配的传输任务 ID。
    pub fn start_download(&mut self, remote_path: &str, local_dir: &str) -> Result<u64, String> {
        if self.downloads.len() >= MAX_CONCURRENT_CLIENT_TRANSFERS {
            return Err(format!(
                "too many concurrent downloads (max {MAX_CONCURRENT_CLIENT_TRANSFERS})"
            ));
        }
        if remote_path.trim().is_empty() {
            return Err("remote_path must not be empty".to_string());
        }
        let raw_basename = remote_path.trim().rsplit(['/', '\\']).next().unwrap_or("");
        let expected_file_name = validate_single_name(raw_basename)
            .map_err(|e| format!("invalid remote file name in '{remote_path}': {e}"))?
            .to_string();

        let dir = expand_tilde(local_dir);
        let canonical_dir = fs::canonicalize(&dir)
            .map_err(|e| format!("failed to resolve local_dir '{}': {e}", dir.display()))?;
        if !canonical_dir.is_dir() {
            return Err(format!("'{}' is not a directory", canonical_dir.display()));
        }

        let id = self.alloc_id();
        let to_path = canonical_dir.join(&expected_file_name);

        self.transfers.insert(
            id,
            TransferItem {
                id,
                name: expected_file_name.clone(),
                direction: TransferDirection::Download,
                status: TransferStatus::Active,
                total_bytes: 0,
                done_bytes: 0,
                from_path: remote_path.to_string(),
                to_path: to_path.to_string_lossy().into_owned(),
                error: None,
            },
        );
        self.downloads.insert(
            id,
            ClientActiveDownload {
                local_dir: canonical_dir,
                expected_file_name,
                file: None,
                part_path: None,
                final_path: None,
                total_bytes: 0,
                received_bytes: 0,
            },
        );
        self.enqueue(&FileWireMsg::DownloadStart {
            id,
            remote_path: remote_path.to_string(),
        });
        Ok(id)
    }

    /// 取消进行中的传输任务，清理本地临时文件并通知远端主机。
    pub fn cancel_transfer(&mut self, id: u64) {
        let mut was_active = false;
        if let Some(item) = self.transfers.get_mut(&id) {
            if item.status == TransferStatus::Active {
                item.status = TransferStatus::Cancelled;
                was_active = true;
            }
        }
        let removed_up = self.uploads.remove(&id).is_some();
        let removed_dl = if let Some(dl) = self.downloads.remove(&id) {
            drop(dl.file);
            if let Some(part) = dl.part_path {
                let _ = fs::remove_file(part);
            }
            true
        } else {
            false
        };

        if was_active || removed_up || removed_dl {
            self.enqueue(&FileWireMsg::CancelTransfer { id });
        }
        if was_active {
            self.prune_finished_history();
        }
    }

    /// 清除所有已结束（Done / Failed / Cancelled）的传输记录。
    pub fn clear_finished(&mut self) {
        self.transfers
            .retain(|_, item| item.status == TransferStatus::Active);
    }

    /// 移除单条传输记录（若仍在进行中则先取消）。
    pub fn remove_transfer(&mut self, id: u64) {
        if let Some(item) = self.transfers.get(&id) {
            if item.status == TransferStatus::Active {
                self.cancel_transfer(id);
            }
        }
        self.transfers.remove(&id);
    }

    /// 返回当前所有传输任务快照（按 ID 升序）。
    pub fn transfers(&self) -> Vec<TransferItem> {
        self.transfers.values().cloned().collect()
    }

    /// 处理一条来自远端主机的线帧，返回需要推送给 UI 的事件列表。
    pub fn handle_incoming(&mut self, frame: &[u8]) -> Vec<FileEvent> {
        let msg = match FileWireMsg::decode(frame) {
            Ok(m) => m,
            Err(_) => return Vec::new(),
        };

        let mut events = Vec::new();
        match msg {
            FileWireMsg::ListDirResp {
                path,
                entries,
                error,
            } => {
                events.push(FileEvent::RemoteDir {
                    path,
                    entries,
                    error,
                });
            }
            FileWireMsg::MkdirResp { path, ok, error } => {
                events.push(FileEvent::MkdirResult { path, ok, error });
            }
            FileWireMsg::UploadAck { id, ok, error } => {
                let is_active_upload = self.uploads.contains_key(&id)
                    && matches!(
                        self.transfers.get(&id),
                        Some(item)
                            if item.direction == TransferDirection::Upload
                                && item.status == TransferStatus::Active
                    );
                if is_active_upload {
                    if ok {
                        if let Some(up) = self.uploads.get_mut(&id) {
                            up.acked = true;
                        }
                        if let Some(item) = self.transfers.get(&id) {
                            events.push(FileEvent::TransferUpdated(item.clone()));
                        }
                    } else {
                        self.uploads.remove(&id);
                        if let Some(item) = self.transfers.get_mut(&id) {
                            item.status = TransferStatus::Failed;
                            item.error = error.or_else(|| Some("upload rejected".to_string()));
                            events.push(FileEvent::TransferUpdated(item.clone()));
                        }
                        self.prune_finished_history();
                    }
                }
            }
            FileWireMsg::UploadDone { id, ok, error } => {
                let is_active_upload = matches!(
                    self.transfers.get(&id),
                    Some(item)
                        if item.direction == TransferDirection::Upload
                            && item.status == TransferStatus::Active
                );
                if is_active_upload && self.uploads.remove(&id).is_some() {
                    if let Some(item) = self.transfers.get_mut(&id) {
                        if ok {
                            item.status = TransferStatus::Done;
                            item.done_bytes = item.total_bytes;
                            item.error = None;
                        } else {
                            item.status = TransferStatus::Failed;
                            item.error = error.or_else(|| Some("upload failed".to_string()));
                        }
                        events.push(FileEvent::TransferUpdated(item.clone()));
                    }
                    self.prune_finished_history();
                }
            }
            FileWireMsg::DownloadMeta {
                id,
                file_name,
                total_bytes,
                ok,
                error,
            } => {
                self.handle_download_meta(id, file_name, total_bytes, ok, error, &mut events);
            }
            FileWireMsg::DownloadChunk { id, offset, data } => {
                self.handle_download_chunk(id, offset, &data, &mut events);
            }
            FileWireMsg::DownloadFinish { id } => {
                self.handle_download_finish(id, &mut events);
            }
            FileWireMsg::CancelTransfer { id } => {
                if let Some(dl) = self.downloads.remove(&id) {
                    drop(dl.file);
                    if let Some(part) = dl.part_path {
                        let _ = fs::remove_file(part);
                    }
                }
                self.uploads.remove(&id);
                let mut updated = false;
                if let Some(item) = self.transfers.get_mut(&id) {
                    if item.status == TransferStatus::Active {
                        item.status = TransferStatus::Failed;
                        item.error = Some("cancelled by remote host".to_string());
                        events.push(FileEvent::TransferUpdated(item.clone()));
                        updated = true;
                    }
                }
                if updated {
                    self.prune_finished_history();
                }
            }
            // Client 忽略主机侧专用请求帧。
            FileWireMsg::ListDir { .. }
            | FileWireMsg::Mkdir { .. }
            | FileWireMsg::UploadStart { .. }
            | FileWireMsg::UploadChunk { .. }
            | FileWireMsg::UploadFinish { .. }
            | FileWireMsg::DownloadStart { .. } => {}
        }
        events
    }

    fn handle_download_meta(
        &mut self,
        id: u64,
        file_name: String,
        total_bytes: u64,
        ok: bool,
        error: Option<String>,
        events: &mut Vec<FileEvent>,
    ) {
        if !self.downloads.contains_key(&id) {
            if ok {
                self.enqueue(&FileWireMsg::CancelTransfer { id });
            }
            return;
        }
        // 严禁重复下发 DownloadMeta（否则会导致前一次 .rcpart 泄漏或覆盖状态）。
        if self.downloads[&id].file.is_some() {
            if let Some(dl) = self.downloads.remove(&id) {
                drop(dl.file);
                if let Some(part) = dl.part_path {
                    let _ = fs::remove_file(part);
                }
            }
            self.enqueue(&FileWireMsg::CancelTransfer { id });
            if let Some(item) = self.transfers.get_mut(&id) {
                item.status = TransferStatus::Failed;
                item.error = Some("duplicate download metadata".to_string());
                events.push(FileEvent::TransferUpdated(item.clone()));
            }
            self.prune_finished_history();
            return;
        }
        if !ok {
            self.downloads.remove(&id);
            if let Some(item) = self.transfers.get_mut(&id) {
                item.status = TransferStatus::Failed;
                item.error = error.or_else(|| Some("download failed".to_string()));
                events.push(FileEvent::TransferUpdated(item.clone()));
            }
            self.prune_finished_history();
            return;
        }

        let expected_name = self.downloads[&id].expected_file_name.clone();
        if validate_single_name(&file_name).is_err() || file_name != expected_name {
            self.downloads.remove(&id);
            self.enqueue(&FileWireMsg::CancelTransfer { id });
            if let Some(item) = self.transfers.get_mut(&id) {
                item.status = TransferStatus::Failed;
                item.error = Some(format!(
                    "unsafe or mismatched remote file name: expected '{expected_name}', got '{file_name}'"
                ));
                events.push(FileEvent::TransferUpdated(item.clone()));
            }
            self.prune_finished_history();
            return;
        }

        let local_dir = self.downloads[&id].local_dir.clone();
        let part_path = part_file_path(&local_dir, id);
        let final_path = local_dir.join(&expected_name);

        let file = match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&part_path)
        {
            Ok(f) => f,
            Err(e) => {
                self.downloads.remove(&id);
                self.enqueue(&FileWireMsg::CancelTransfer { id });
                if let Some(item) = self.transfers.get_mut(&id) {
                    item.status = TransferStatus::Failed;
                    item.error = Some(format!("failed to create local temp file: {e}"));
                    events.push(FileEvent::TransferUpdated(item.clone()));
                }
                self.prune_finished_history();
                return;
            }
        };

        if let Some(dl) = self.downloads.get_mut(&id) {
            dl.file = Some(file);
            dl.part_path = Some(part_path);
            dl.final_path = Some(final_path.clone());
            dl.total_bytes = total_bytes;
        }
        if let Some(item) = self.transfers.get_mut(&id) {
            item.name = expected_name;
            item.total_bytes = total_bytes;
            item.to_path = final_path.to_string_lossy().into_owned();
            events.push(FileEvent::TransferUpdated(item.clone()));
        }
    }

    fn handle_download_chunk(
        &mut self,
        id: u64,
        offset: u64,
        data: &[u8],
        events: &mut Vec<FileEvent>,
    ) {
        let mut fail_reason: Option<String> = None;
        let mut new_done = 0u64;

        match self.downloads.get_mut(&id) {
            None => return,
            Some(dl) => {
                if offset != dl.received_bytes {
                    fail_reason = Some(format!(
                        "download offset mismatch: expected {}, got {offset}",
                        dl.received_bytes
                    ));
                } else if dl
                    .received_bytes
                    .checked_add(data.len() as u64)
                    .map_or(true, |next| next > dl.total_bytes)
                {
                    fail_reason = Some("download exceeded declared total_bytes".to_string());
                } else if let Some(file) = dl.file.as_mut() {
                    if let Err(e) = file.write_all(data) {
                        fail_reason = Some(format!("write download chunk failed: {e}"));
                    } else {
                        dl.received_bytes += data.len() as u64;
                        new_done = dl.received_bytes;
                    }
                } else {
                    fail_reason = Some("received chunk before download metadata".to_string());
                }
            }
        }

        if let Some(err) = fail_reason {
            if let Some(dl) = self.downloads.remove(&id) {
                drop(dl.file);
                if let Some(part) = dl.part_path {
                    let _ = fs::remove_file(part);
                }
            }
            self.enqueue(&FileWireMsg::CancelTransfer { id });
            if let Some(item) = self.transfers.get_mut(&id) {
                item.status = TransferStatus::Failed;
                item.error = Some(err);
                events.push(FileEvent::TransferUpdated(item.clone()));
            }
            self.prune_finished_history();
            return;
        }

        if let Some(item) = self.transfers.get_mut(&id) {
            item.done_bytes = new_done;
            events.push(FileEvent::TransferUpdated(item.clone()));
        }
    }

    fn handle_download_finish(&mut self, id: u64, events: &mut Vec<FileEvent>) {
        let Some(dl) = self.downloads.remove(&id) else {
            return;
        };

        let finish_res: Result<(), String> = (|| {
            if dl.received_bytes != dl.total_bytes {
                return Err(format!(
                    "incomplete download: received {} of {} bytes",
                    dl.received_bytes, dl.total_bytes
                ));
            }
            let file = dl
                .file
                .ok_or_else(|| "download file handle missing".to_string())?;
            file.sync_all()
                .map_err(|e| format!("sync local temp file failed: {e}"))?;
            drop(file);

            let part = dl
                .part_path
                .as_ref()
                .ok_or_else(|| "temp path missing".to_string())?;
            let final_path = dl
                .final_path
                .as_ref()
                .ok_or_else(|| "final path missing".to_string())?;
            fs::rename(part, final_path).map_err(|e| format!("atomic rename failed: {e}"))?;
            Ok(())
        })();

        if finish_res.is_err() {
            if let Some(part) = dl.part_path {
                let _ = fs::remove_file(part);
            }
        }

        if let Some(item) = self.transfers.get_mut(&id) {
            match finish_res {
                Ok(()) => {
                    item.status = TransferStatus::Done;
                    item.done_bytes = item.total_bytes;
                    item.error = None;
                }
                Err(err) => {
                    item.status = TransferStatus::Failed;
                    item.error = Some(err);
                }
            }
            events.push(FileEvent::TransferUpdated(item.clone()));
        }
        self.prune_finished_history();
    }

    /// 最多拉取 `max_frames` 个待发送线帧，平滑节流已确认的上传任务。
    pub fn poll_outgoing(&mut self, max_frames: usize) -> Vec<Vec<u8>> {
        if max_frames == 0 {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(max_frames.min(16));

        while out.len() < max_frames {
            if let Some(frame) = self.outgoing.pop_front() {
                out.push(frame);
                continue;
            }
            if !self.pump_one_upload_chunk(&mut out) {
                break;
            }
        }
        out
    }

    fn pump_one_upload_chunk(&mut self, out: &mut Vec<Vec<u8>>) -> bool {
        let next_id = self
            .uploads
            .range((
                std::ops::Bound::Excluded(self.upload_rr_cursor),
                std::ops::Bound::Unbounded,
            ))
            .find(|(_, u)| u.acked && !u.finish_sent)
            .or_else(|| self.uploads.iter().find(|(_, u)| u.acked && !u.finish_sent))
            .map(|(&id, _)| id);

        let Some(id) = next_id else {
            return false;
        };
        self.upload_rr_cursor = id;

        let mut buf = vec![0u8; FILE_CHUNK_SIZE];
        let mut send_finish = false;
        let mut read_err: Option<String> = None;
        let mut chunk_msg = None;
        let mut new_done = 0u64;

        if let Some(up) = self.uploads.get_mut(&id) {
            if up.offset >= up.total_bytes {
                up.finish_sent = true;
                send_finish = true;
            } else {
                let to_read = (up.total_bytes - up.offset).min(FILE_CHUNK_SIZE as u64) as usize;
                match read_retry_eintr(&mut up.file, &mut buf[..to_read]) {
                    Ok(0) => {
                        up.finish_sent = true;
                        send_finish = true;
                    }
                    Ok(n) => {
                        buf.truncate(n);
                        let offset = up.offset;
                        up.offset += n as u64;
                        new_done = up.offset;
                        chunk_msg = Some(FileWireMsg::UploadChunk {
                            id,
                            offset,
                            data: buf,
                        });
                    }
                    Err(e) => {
                        read_err = Some(format!("read local file failed: {e}"));
                    }
                }
            }
        }

        if let Some(msg) = chunk_msg {
            if let Some(item) = self.transfers.get_mut(&id) {
                item.done_bytes = new_done;
            }
            if let Ok(frame) = msg.encode() {
                out.push(frame);
            }
            return true;
        }

        if send_finish {
            if let Ok(frame) = (FileWireMsg::UploadFinish { id }).encode() {
                out.push(frame);
            }
            return true;
        }

        if let Some(err) = read_err {
            self.uploads.remove(&id);
            if let Some(item) = self.transfers.get_mut(&id) {
                item.status = TransferStatus::Failed;
                item.error = Some(err);
                self.local_events
                    .push(FileEvent::TransferUpdated(item.clone()));
            }
            self.prune_finished_history();
            if let Ok(frame) = (FileWireMsg::CancelTransfer { id }).encode() {
                out.push(frame);
            }
            return true;
        }

        false
    }
}

impl Drop for FileClientEngine {
    fn drop(&mut self) {
        let downloads = std::mem::take(&mut self.downloads);
        for (_, dl) in downloads {
            drop(dl.file);
            if let Some(part) = dl.part_path {
                let _ = fs::remove_file(part);
            }
        }
        self.uploads.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(1);

    struct TempDirGuard(PathBuf);

    impl TempDirGuard {
        fn new(prefix: &str) -> Self {
            let id = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "rc_tools_ft_{prefix}_{}_{}",
                std::process::id(),
                id
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn list_local_dir_sorts_dirs_first_then_alphabetical() {
        let tmp = TempDirGuard::new("list");
        fs::create_dir(tmp.path().join("z_dir")).unwrap();
        fs::create_dir(tmp.path().join("a_dir")).unwrap();
        fs::write(tmp.path().join("b.rs"), b"fn main() {}").unwrap();
        fs::write(tmp.path().join("a.png"), b"png").unwrap();

        let (canonical, entries) = list_local_dir(tmp.path().to_str().unwrap()).unwrap();
        assert!(!canonical.is_empty());
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["a_dir", "z_dir", "a.png", "b.rs"]);
        assert!(entries[0].is_dir && entries[0].kind == "dir");
        assert!(!entries[2].is_dir && entries[2].kind == "image");
        assert!(!entries[3].is_dir && entries[3].kind == "code");
    }

    #[test]
    fn create_local_dir_rejects_traversal_and_invalid_names() {
        let tmp = TempDirGuard::new("mkdir");
        let parent = tmp.path().to_str().unwrap();

        for bad in ["", "   ", ".", "..", "a/b", "a\\b", "../evil", "foo\0bar"] {
            assert!(
                create_local_dir(parent, bad).is_err(),
                "expected rejection for {bad:?}"
            );
        }

        let created = create_local_dir(parent, "sub_ok").unwrap();
        assert!(Path::new(&created).is_dir());

        // Double dots inside a normal filename/dir name are valid.
        let dotted = create_local_dir(parent, "report..v2").unwrap();
        assert!(Path::new(&dotted).is_dir());
        assert_eq!(
            validate_single_name("report..v2.pdf").unwrap(),
            "report..v2.pdf"
        );
    }

    #[test]
    fn part_file_path_includes_unique_entropy_and_short_length() {
        let dir = Path::new("/tmp");
        let p1 = part_file_path(dir, 7);
        let p2 = part_file_path(dir, 7);
        assert_ne!(p1, p2);
        let name1 = p1.file_name().unwrap().to_str().unwrap();
        assert!(name1.starts_with(".rcpart.7."));
        assert!(
            name1.len() < 64,
            "temp filename must stay < 64 bytes, got {} ({name1})",
            name1.len()
        );
    }

    #[test]
    fn host_outgoing_is_capped_and_rejects_cross_map_duplicate_id() {
        let tmp = TempDirGuard::new("host_cap");
        let src = tmp.path().join("sample.bin");
        fs::write(&src, b"hello").unwrap();

        let mut host = FileHostEngine::new();
        // Start upload with id 10
        let up_start = FileWireMsg::UploadStart {
            id: 10,
            remote_dir: tmp.path().to_str().unwrap().to_string(),
            file_name: "up.bin".to_string(),
            total_bytes: 5,
        }
        .encode()
        .unwrap();
        host.handle_incoming(&up_start);
        let _ = host.poll_outgoing(4);
        assert_eq!(host.active_upload_count(), 1);

        // Starting download with same id 10 must be rejected across maps!
        let dl_start = FileWireMsg::DownloadStart {
            id: 10,
            remote_path: src.to_str().unwrap().to_string(),
        }
        .encode()
        .unwrap();
        host.handle_incoming(&dl_start);
        let frames = host.poll_outgoing(4);
        assert_eq!(frames.len(), 1);
        match FileWireMsg::decode(&frames[0]).unwrap() {
            FileWireMsg::DownloadMeta { id, ok, .. } => {
                assert_eq!(id, 10);
                assert!(!ok);
            }
            other => panic!("unexpected msg: {other:?}"),
        }
        assert_eq!(host.active_download_count(), 0);

        // Flood ListDir frames without polling -> outgoing capped at MAX_FILE_HOST_OUTGOING_FRAMES
        let list = FileWireMsg::ListDir {
            path: Some(tmp.path().to_str().unwrap().to_string()),
        }
        .encode()
        .unwrap();
        for _ in 0..(MAX_FILE_HOST_OUTGOING_FRAMES + 20) {
            host.handle_incoming(&list);
        }
        assert_eq!(host.outgoing.len(), MAX_FILE_HOST_OUTGOING_FRAMES);

        // Critical DownloadMeta/UploadDone must still be queued when outgoing is full of ListDirResp
        let dl = FileWireMsg::DownloadStart {
            id: 99,
            remote_path: tmp.path().join("missing.bin").to_str().unwrap().to_string(),
        }
        .encode()
        .unwrap();
        host.handle_incoming(&dl);
        assert_eq!(host.outgoing.len(), MAX_FILE_HOST_OUTGOING_FRAMES);
        let tail = host.outgoing.back().unwrap();
        assert!(matches!(
            FileWireMsg::decode(tail).unwrap(),
            FileWireMsg::DownloadMeta {
                id: 99,
                ok: false,
                ..
            }
        ));
    }

    #[test]
    fn client_caps_active_transfers_and_prunes_finished_history() {
        let tmp = TempDirGuard::new("client_cap");
        let mut client = FileClientEngine::new();

        // Cap active downloads at MAX_CONCURRENT_CLIENT_TRANSFERS (16)
        for i in 0..MAX_CONCURRENT_CLIENT_TRANSFERS {
            let res = client.start_download(
                &format!("/remote/file_{i}.bin"),
                tmp.path().to_str().unwrap(),
            );
            assert!(res.is_ok());
        }
        assert!(client
            .start_download("/remote/overflow.bin", tmp.path().to_str().unwrap())
            .is_err());

        // Cancel them all, then generate > MAX_FINISHED_HISTORY finished transfers
        let ids: Vec<u64> = client.transfers().into_iter().map(|t| t.id).collect();
        for id in ids {
            client.cancel_transfer(id);
        }
        for i in 0..(MAX_FINISHED_HISTORY + 15) {
            let id = client
                .start_download(
                    &format!("/remote/hist_{i}.bin"),
                    tmp.path().to_str().unwrap(),
                )
                .unwrap();
            client.cancel_transfer(id);
        }
        assert_eq!(client.transfers().len(), MAX_FINISHED_HISTORY);
    }

    #[cfg(unix)]
    #[test]
    fn upload_local_read_error_emits_drainable_local_event() {
        let tmp = TempDirGuard::new("read_err");
        let mut client = FileClientEngine::new();
        // Opening a directory succeeds on Unix, but reading from it deterministically fails with EISDIR.
        let dir_handle = File::open(tmp.path()).unwrap();
        let id = 42;
        client.transfers.insert(
            id,
            TransferItem {
                id,
                name: "broken.bin".to_string(),
                direction: TransferDirection::Upload,
                status: TransferStatus::Active,
                total_bytes: 128,
                done_bytes: 0,
                from_path: tmp.path().to_string_lossy().into_owned(),
                to_path: "/remote/broken.bin".to_string(),
                error: None,
            },
        );
        client.uploads.insert(
            id,
            ClientActiveUpload {
                file: dir_handle,
                offset: 0,
                total_bytes: 128,
                acked: true,
                finish_sent: false,
            },
        );

        let frames = client.poll_outgoing(4);
        assert_eq!(frames.len(), 1);
        assert_eq!(
            FileWireMsg::decode(&frames[0]).unwrap(),
            FileWireMsg::CancelTransfer { id }
        );

        let events = client.drain_local_events();
        assert_eq!(events.len(), 1);
        match &events[0] {
            FileEvent::TransferUpdated(item) => {
                assert_eq!(item.id, id);
                assert_eq!(item.status, TransferStatus::Failed);
                assert!(item.error.is_some());
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert!(client.drain_local_events().is_empty());
    }

    #[test]
    fn wire_frame_roundtrip_and_bounds() {
        let chunk = FileWireMsg::UploadChunk {
            id: 42,
            offset: 65536,
            data: vec![0xAB; FILE_CHUNK_SIZE],
        };
        let encoded = chunk.encode().unwrap();
        assert!(encoded.len() <= MAX_FILE_WIRE_FRAME_BYTES);
        assert_eq!(FileWireMsg::decode(&encoded).unwrap(), chunk);

        let oversized = vec![0u8; MAX_FILE_WIRE_FRAME_BYTES + 1];
        assert!(FileWireMsg::decode(&oversized).is_err());

        let event = FileEvent::TransferUpdated(TransferItem {
            id: 1,
            name: "demo.bin".to_string(),
            direction: TransferDirection::Upload,
            status: TransferStatus::Done,
            total_bytes: 10,
            done_bytes: 10,
            from_path: "/a".to_string(),
            to_path: "/b".to_string(),
            error: None,
        });
        let json = serde_json::to_string(&event).unwrap();
        let parsed: FileEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, event);
    }
}
