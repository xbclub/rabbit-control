//! 信令与中继服务器配置模型、地址/设备码解析与原子持久化。
//!
//! 本模块全纯 Rust 实现，不依赖 Tauri 事件循环，支持离线确定性单测；
//! 全模块 `#![deny(unsafe_code)]` 与 `#![deny(missing_docs)]`。

#![deny(unsafe_code)]
#![deny(missing_docs)]

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{IpAddr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};

/// 全局单调递增的服务器状态快照版本号生成器（跨重启单调递增，供前端过滤乱序状态事件）。
static STATUS_REVISION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 分配下一个全局单调递增的状态快照版本号（从 `1` 开始）。
pub fn next_status_revision() -> u64 {
    STATUS_REVISION_COUNTER.fetch_add(1, Ordering::AcqRel) + 1
}

/// 默认原生 TCP/UDP 信令端口（见 `server/docs/02-protocol-and-api.md` §1）。
pub const DEFAULT_SIGNAL_PORT: u16 = 21116;

/// 默认零拷贝 TCP 中继端口（见 `server/docs/02-protocol-and-api.md` §1）。
pub const DEFAULT_RELAY_PORT: u16 = 21117;

/// 默认服务端 HTTP/HTTPS API 管理与认证端口。
pub const DEFAULT_API_PORT: u16 = 21118;

/// 设备密钥随机字节长度（32 字节 = 64 位小写十六进制）。
const DEVICE_KEY_BYTES: usize = 32;

/// 持久化于本地磁盘的信令/中继服务器配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    /// 是否启用信令与中继服务器。
    #[serde(default)]
    pub enabled: bool,
    /// 服务器主机名或 IP（不含端口，IPv6 不含方括号）。
    #[serde(default)]
    pub server_host: String,
    /// 原生 TCP/UDP 信令端口（默认 `21116`）。
    #[serde(default = "default_signal_port")]
    pub signal_port: u16,
    /// TCP 中继端口（默认 `21117`）。
    #[serde(default = "default_relay_port")]
    pub relay_port: u16,
    /// HTTP/HTTPS API 端口（默认 `21118`）。
    #[serde(default = "default_api_port")]
    pub api_port: u16,
    /// 是否默认通过 HTTPS 访问 API 端口（默认 `true`）。
    #[serde(default = "default_use_https")]
    pub use_https: bool,
    /// 锚定的服务器 Root CA SHA-256 指纹（64 位小写十六进制）。
    #[serde(default)]
    pub ca_fingerprint_sha256: Option<String>,
    /// 锚定的服务器 Root CA 证书 PEM（用于 HTTPS 与 mTLS 信任根）。
    #[serde(default)]
    pub ca_cert_pem: Option<String>,
    /// 锚定的服务端 Ed25519 验签公钥（64 位小写十六进制，用于离线验签 `OwnerGrantTicket`）。
    #[serde(default)]
    pub server_auth_pubkey_hex: Option<String>,
    /// 本机唯一稳定密钥（64 位小写十六进制，用于向服务器幂等换取永久 9 位设备码）。
    #[serde(default = "generate_device_key")]
    pub device_key: String,
    /// 服务器签发并缓存的 9 位数字设备码（纯 9 位数字，不含空格）。
    #[serde(default)]
    pub cached_device_id: Option<String>,
}

fn default_signal_port() -> u16 {
    DEFAULT_SIGNAL_PORT
}

fn default_relay_port() -> u16 {
    DEFAULT_RELAY_PORT
}

fn default_api_port() -> u16 {
    DEFAULT_API_PORT
}

fn default_use_https() -> bool {
    true
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            server_host: String::new(),
            signal_port: DEFAULT_SIGNAL_PORT,
            relay_port: DEFAULT_RELAY_PORT,
            api_port: DEFAULT_API_PORT,
            use_https: true,
            ca_fingerprint_sha256: None,
            ca_cert_pem: None,
            server_auth_pubkey_hex: None,
            device_key: generate_device_key(),
            cached_device_id: None,
        }
    }
}

impl ServerConfig {
    /// 判断服务器配置是否已启用且所有关键端点字段均非空有效。
    pub fn is_configured(&self) -> bool {
        self.enabled
            && !self.server_host.trim().is_empty()
            && self.signal_port > 0
            && self.relay_port > 0
            && self.api_port > 0
            && (!self.use_https
                || self
                    .ca_cert_pem
                    .as_deref()
                    .map(str::trim)
                    .map(|s| !s.is_empty())
                    .unwrap_or(false))
    }

    /// 返回配置中的信令端点字符串（如 `"172.16.1.2:21116"` 或 `"[::1]:21116"`）；
    /// 当 `server_host` 为空时返回 `None`。
    pub fn signal_addr(&self) -> Option<String> {
        let host = self.server_host.trim();
        if host.is_empty() || self.signal_port == 0 {
            return None;
        }
        Some(format_host_port(host, self.signal_port))
    }

    /// 返回配置中的中继端点字符串（如 `"172.16.1.2:21117"` 或 `"[::1]:21117"`）；
    /// 当 `server_host` 为空时返回 `None`。
    pub fn relay_addr(&self) -> Option<String> {
        let host = self.server_host.trim();
        if host.is_empty() || self.relay_port == 0 {
            return None;
        }
        Some(format_host_port(host, self.relay_port))
    }

    /// 返回配置中的 HTTP/HTTPS API 端点字符串（如 `"172.16.1.2:21118"` 或 `"[::1]:21118"`）；
    /// 当 `server_host` 为空或 `api_port == 0` 时返回 `None`。
    pub fn api_addr(&self) -> Option<String> {
        let host = self.server_host.trim();
        if host.is_empty() || self.api_port == 0 {
            return None;
        }
        Some(format_host_port(host, self.api_port))
    }

    /// 返回配置中的 API 基础 URL（如 `"https://172.16.1.2:21118"`）；
    /// 当 `server_host` 为空或 `api_port == 0` 时返回 `None`。
    pub fn api_base_url(&self) -> Option<String> {
        self.api_addr().map(|addr| {
            let scheme = if self.use_https { "https" } else { "http" };
            format!("{scheme}://{addr}")
        })
    }

    /// 构造一份与当前配置对应的禁用态快照。
    pub fn disabled_snapshot(&self) -> ServerStatusSnapshot {
        let device_id = self.cached_device_id.clone();
        let device_id_formatted = device_id.as_deref().map(format_device_id);
        ServerStatusSnapshot {
            enabled: self.enabled,
            server_host: self.server_host.clone(),
            signal_port: self.signal_port,
            relay_port: self.relay_port,
            state: ServerConnectionState::Disabled,
            device_id,
            device_id_formatted,
            error_message: None,
            signal_addr: self.signal_addr(),
            relay_addr: self.relay_addr(),
            revision: next_status_revision(),
        }
    }
}

/// 客户端与信令服务器的连接状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerConnectionState {
    /// 未启用或未配置服务器地址。
    Disabled,
    /// 正在建立 TCP 连接或注册设备码。
    Connecting,
    /// 已完成注册，保持心跳与入站信令监听。
    Connected,
    /// 连接失败或被服务器断开（具体原因见 `error_message`）。
    Error,
}

/// 面向前端 IPC 的服务器状态快照（`camelCase` JSON 序列化）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerStatusSnapshot {
    /// 当前是否启用了信令服务器。
    pub enabled: bool,
    /// 已配置的服务器主机名或 IP。
    pub server_host: String,
    /// 已配置的信令端口。
    pub signal_port: u16,
    /// 已配置的中继端口。
    pub relay_port: u16,
    /// 当前连接状态。
    pub state: ServerConnectionState,
    /// 服务器签发的 9 位纯数字设备码（如 `"604928235"`）。
    pub device_id: Option<String>,
    /// 格式化展示的 9 位设备码（如 `"604 928 235"`）。
    pub device_id_formatted: Option<String>,
    /// 最近一次错误说明（正常时为 `None`）。
    pub error_message: Option<String>,
    /// 实际使用的信令端点地址（如 `"172.16.1.2:21116"`）。
    pub signal_addr: Option<String>,
    /// 实际使用的中继端点地址（如 `"172.16.1.2:21117"`）。
    pub relay_addr: Option<String>,
    /// 全局单调递增的状态快照版本号（跨信令客户端重启严格递增，前端用于丢弃乱序旧快照）。
    #[serde(default)]
    pub revision: u64,
}

/// 将主机名/IP 与端口拼接为 `host:port`（裸 IPv6 自动补全方括号 `[ipv6]:port`）。
pub fn format_host_port(host: &str, port: u16) -> String {
    let trimmed = host.trim();
    if trimmed.contains(':') && !trimmed.starts_with('[') {
        format!("[{trimmed}]:{port}")
    } else {
        format!("{trimmed}:{port}")
    }
}

/// 解析用户输入的服务器地址字符串。
///
/// 支持剥离可选的 `http://`、`https://`、`tcp://` 前缀（不区分大小写）以及末尾斜杠；
/// 支持 `ip`、`ip:port`、`host`、`host:port`、`[ipv6]`、`[ipv6]:port` 与裸 `ipv6`；
/// 拒绝空字符串、内部空白、路径段、非法主机名及非法端口（`0` / `>65535` / 非数字）。
pub fn parse_server_input(raw: &str, default_signal_port: u16) -> Result<(String, u16), String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("服务器地址不能为空".to_string());
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err("服务器地址不能包含空白字符".to_string());
    }
    if trimmed.chars().any(char::is_control) {
        return Err("服务器地址包含非法控制字符".to_string());
    }

    let without_scheme = strip_optional_scheme(trimmed);
    let cleaned = without_scheme.trim_end_matches('/');
    if cleaned.is_empty() {
        return Err("服务器地址不能为空".to_string());
    }
    if cleaned.contains('/') {
        return Err("服务器地址不应包含路径，请填写主机名或 IP（可选 :端口）".to_string());
    }

    let (host, port) = if let Some(rest) = cleaned.strip_prefix('[') {
        let Some((ipv6_host, after_bracket)) = rest.split_once(']') else {
            return Err("IPv6 地址格式无效，缺少右括号".to_string());
        };
        if ipv6_host.parse::<Ipv6Addr>().is_err() {
            return Err("方括号内必须是合法的 IPv6 地址".to_string());
        }
        if after_bracket.is_empty() {
            (
                ipv6_host.to_string(),
                validate_default_port(default_signal_port)?,
            )
        } else if let Some(port_part) = after_bracket.strip_prefix(':') {
            let port = parse_port_str(port_part)?;
            (ipv6_host.to_string(), port)
        } else {
            return Err("IPv6 地址右括号后仅允许跟 :端口".to_string());
        }
    } else {
        let colon_count = cleaned.bytes().filter(|&b| b == b':').count();
        match colon_count {
            0 => (
                cleaned.to_string(),
                validate_default_port(default_signal_port)?,
            ),
            1 => {
                let (h, p) = cleaned.split_once(':').expect("colon_count == 1");
                if h.is_empty() {
                    return Err("缺少服务器主机地址".to_string());
                }
                let port = parse_port_str(p)?;
                (h.to_string(), port)
            }
            _ => {
                // 多个冒号：仅当整体是合法裸 IPv6 地址时接受（如 `::1`）
                if cleaned.parse::<Ipv6Addr>().is_ok() {
                    (
                        cleaned.to_string(),
                        validate_default_port(default_signal_port)?,
                    )
                } else {
                    return Err(
                        "服务器地址格式无效（IPv6 带端口请使用 [ipv6]:端口 格式）".to_string()
                    );
                }
            }
        }
    };

    validate_host(&host)?;
    Ok((host, port))
}

fn strip_optional_scheme(s: &str) -> &str {
    for prefix in ["http://", "https://", "tcp://"] {
        if s.get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        {
            return &s[prefix.len()..];
        }
    }
    s
}

fn validate_default_port(port: u16) -> Result<u16, String> {
    if port == 0 {
        return Err("端口不能为 0，范围必须为 1–65535".to_string());
    }
    Ok(port)
}

fn parse_port_str(s: &str) -> Result<u16, String> {
    if s.is_empty() {
        return Err("端口不能为空".to_string());
    }
    if !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err("端口必须为 1–65535 的整数".to_string());
    }
    let port: u16 = s
        .parse()
        .map_err(|_| "端口范围必须为 1–65535".to_string())?;
    if port == 0 {
        return Err("端口不能为 0，范围必须为 1–65535".to_string());
    }
    Ok(port)
}

fn validate_host(host: &str) -> Result<(), String> {
    if host.is_empty() {
        return Err("缺少服务器主机地址".to_string());
    }
    if host.parse::<IpAddr>().is_ok() {
        return Ok(());
    }
    if host.len() > 253 {
        return Err("服务器主机名过长（超过 253 字节）".to_string());
    }
    if host.starts_with('.') || host.ends_with('.') || host.contains("..") {
        return Err("服务器主机名格式无效".to_string());
    }
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err("服务器主机名格式无效".to_string());
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err("服务器主机名格式无效".to_string());
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err("服务器主机名包含非法字符".to_string());
        }
    }
    Ok(())
}

/// 将 9 位纯数字设备码格式化为 `"604 928 235"` 分组形式；
/// 若输入不是 9 位 ASCII 数字则原样返回。
pub fn format_device_id(id: &str) -> String {
    let trimmed = id.trim();
    if trimmed.len() == 9 && trimmed.bytes().all(|b| b.is_ascii_digit()) {
        format!("{} {} {}", &trimmed[0..3], &trimmed[3..6], &trimmed[6..9])
    } else {
        id.to_string()
    }
}

/// 解析用户输入的 9 位设备码。
///
/// 忽略前后空白及中间的任意 Unicode 空白与连字符（`-`）；
/// 当且仅当剥离后恰好为 9 位 ASCII 数字且首位不为 `'0'`（`100000000..=999999999`，对齐服务端签发区间）时返回 `Some(9位数字)`。
pub fn parse_device_id_input(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut digits = String::with_capacity(9);
    for ch in trimmed.chars() {
        if ch.is_whitespace() || ch == '-' {
            continue;
        }
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else {
            return None;
        }
    }
    if digits.len() == 9 && !digits.starts_with('0') {
        Some(digits)
    } else {
        None
    }
}

/// 使用操作系统加密熵源 (`OsRng`) 生成 32 字节随机设备密钥，编码为 64 位小写十六进制。
pub fn generate_device_key() -> String {
    let mut bytes = [0u8; DEVICE_KEY_BYTES];
    OsRng.fill_bytes(&mut bytes);
    encode_hex_lower(&bytes)
}

fn encode_hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0F) as usize] as char);
    }
    out
}

/// 返回默认配置文件路径：`$HOME/.config/rabbitcontrol/server.json`。
pub fn default_config_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".config")
        .join("rabbitcontrol")
        .join("server.json")
}

/// 从指定路径读取服务器配置；文件不存在或内容损坏时回退默认配置（自动生成新 `device_key`）。
pub fn load_from_path(path: &Path) -> ServerConfig {
    let Ok(bytes) = fs::read(path) else {
        return ServerConfig::default();
    };
    let Ok(mut cfg) = serde_json::from_slice::<ServerConfig>(&bytes) else {
        return ServerConfig::default();
    };

    let trimmed_key = cfg.device_key.trim();
    let key_invalid = trimmed_key.is_empty()
        || trimmed_key.len() > 256
        || trimmed_key.chars().any(|ch| ch.is_control() || ch == '\0');
    if key_invalid {
        cfg.device_key = generate_device_key();
        cfg.cached_device_id = None;
    } else {
        cfg.device_key = trimmed_key.to_string();
        cfg.cached_device_id = cfg
            .cached_device_id
            .as_deref()
            .and_then(parse_device_id_input);
    }
    if cfg.signal_port == 0 {
        cfg.signal_port = DEFAULT_SIGNAL_PORT;
    }
    if cfg.relay_port == 0 {
        cfg.relay_port = DEFAULT_RELAY_PORT;
    }
    if cfg.api_port == 0 {
        cfg.api_port = DEFAULT_API_PORT;
    }
    cfg.ca_fingerprint_sha256 = cfg
        .ca_fingerprint_sha256
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty());
    cfg.ca_cert_pem = cfg
        .ca_cert_pem
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    cfg.server_auth_pubkey_hex = cfg
        .server_auth_pubkey_hex
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty());
    cfg.server_host = cfg.server_host.trim().to_string();
    cfg
}

/// 将服务器配置原子写入指定路径（先写同目录临时文件并落盘，再 `rename` 覆盖）。
///
/// 会自动创建缺失的父目录；在 Unix 平台上将文件权限收紧为 `0600`（保护 `device_key`）。
pub fn save_to_path(path: &Path, cfg: &ServerConfig) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("创建配置目录 {} 失败：{e}", parent.display()))?;
        }
    }

    let json = serde_json::to_vec_pretty(cfg).map_err(|e| format!("序列化服务器配置失败：{e}"))?;

    let mut rand_suffix = [0u8; 6];
    OsRng.fill_bytes(&mut rand_suffix);
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("server.json");
    let tmp_name = format!(
        ".{file_name}.tmp.{}.{}",
        std::process::id(),
        encode_hex_lower(&rand_suffix)
    );
    let tmp_path = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(tmp_name),
        _ => PathBuf::from(tmp_name),
    };

    let write_res = (|| -> std::io::Result<()> {
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp_path)?;
        file.write_all(&json)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600));
        }
        Ok(())
    })();

    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(format!("写入临时配置文件失败：{err}"));
    }

    if let Err(err) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(format!("原子替换配置文件 {} 失败：{err}", path.display()));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_server_input_accepts_valid_variants() {
        assert_eq!(
            parse_server_input("172.16.1.2", DEFAULT_SIGNAL_PORT).unwrap(),
            ("172.16.1.2".to_string(), 21116)
        );
        assert_eq!(
            parse_server_input("172.16.1.2:22222", DEFAULT_SIGNAL_PORT).unwrap(),
            ("172.16.1.2".to_string(), 22222)
        );
        assert_eq!(
            parse_server_input("  http://172.16.1.2:21116/  ", DEFAULT_SIGNAL_PORT).unwrap(),
            ("172.16.1.2".to_string(), 21116)
        );
        assert_eq!(
            parse_server_input("HTTPS://signal.example.com", DEFAULT_SIGNAL_PORT).unwrap(),
            ("signal.example.com".to_string(), 21116)
        );
        assert_eq!(
            parse_server_input("tcp://signal.example.com:30000", DEFAULT_SIGNAL_PORT).unwrap(),
            ("signal.example.com".to_string(), 30000)
        );
        assert_eq!(
            parse_server_input("[::1]:21116", DEFAULT_SIGNAL_PORT).unwrap(),
            ("::1".to_string(), 21116)
        );
        assert_eq!(
            parse_server_input("[2001:db8::1]", DEFAULT_SIGNAL_PORT).unwrap(),
            ("2001:db8::1".to_string(), 21116)
        );
        assert_eq!(
            parse_server_input("::1", DEFAULT_SIGNAL_PORT).unwrap(),
            ("::1".to_string(), 21116)
        );
    }

    #[test]
    fn parse_server_input_rejects_invalid_inputs() {
        for bad in [
            "",
            "   ",
            "http://",
            "172.16.1.2 21116",
            "172.16.1.2:0",
            "172.16.1.2:70000",
            "172.16.1.2:abc",
            "172.16.1.2:",
            ":21116",
            "http://172.16.1.2/api/v1",
            "[::1",
            "[not-ipv6]:21116",
            "[::1]extra",
            "bad host",
            "-bad.example.com",
            "bad..example.com",
            "服务器",
            "测试服务器",
            "12。34。56",
        ] {
            assert!(
                parse_server_input(bad, DEFAULT_SIGNAL_PORT).is_err(),
                "expected error for {bad:?}"
            );
        }
        assert!(parse_server_input("172.16.1.2", 0).is_err());
    }

    #[test]
    fn strip_optional_scheme_handles_multibyte_utf8_without_panic() {
        assert_eq!(strip_optional_scheme("服务器"), "服务器");
        assert_eq!(strip_optional_scheme("测试服务器"), "测试服务器");
        assert_eq!(strip_optional_scheme("12。34。56"), "12。34。56");
        assert_eq!(strip_optional_scheme("HTTP://服务器"), "服务器");
        assert_eq!(strip_optional_scheme("https://测试"), "测试");
        assert_eq!(strip_optional_scheme("TCP://12。34"), "12。34");
    }

    #[test]
    fn format_and_parse_device_id_roundtrip() {
        assert_eq!(format_device_id("604928235"), "604 928 235");
        assert_eq!(format_device_id("short"), "short");

        assert_eq!(
            parse_device_id_input("604928235"),
            Some("604928235".to_string())
        );
        assert_eq!(
            parse_device_id_input("604 928 235"),
            Some("604928235".to_string())
        );
        assert_eq!(
            parse_device_id_input("  604-928-235  "),
            Some("604928235".to_string())
        );
        // Unicode 空白（NBSP / Tab / 全角空格）与前端 JS 保持一致
        assert_eq!(
            parse_device_id_input("604\u{00A0}928\t235"),
            Some("604928235".to_string())
        );
        assert_eq!(
            parse_device_id_input("604\u{3000}928\u{3000}235"),
            Some("604928235".to_string())
        );
        assert_eq!(
            parse_device_id_input("100000000"),
            Some("100000000".to_string())
        );
        assert_eq!(
            parse_device_id_input("999999999"),
            Some("999999999".to_string())
        );

        for bad in [
            "",
            "   ",
            "000000000",
            "000 000 000",
            "000-000-000",
            "012345678",
            "012 345 678",
            "099999999",
            "12345678",
            "1234567890",
            "60492823a",
            "127.0.0.1:9901",
            "RC-8492-3310",
        ] {
            assert_eq!(
                parse_device_id_input(bad),
                None,
                "expected None for {bad:?}"
            );
        }
    }

    #[test]
    fn generate_device_key_produces_unique_64_hex_chars() {
        let k1 = generate_device_key();
        let k2 = generate_device_key();
        assert_eq!(k1.len(), 64);
        assert_eq!(k2.len(), 64);
        assert!(k1.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        assert_ne!(k1, k2);
    }

    #[test]
    fn load_and_save_config_atomic_roundtrip() {
        let mut suffix = [0u8; 6];
        OsRng.fill_bytes(&mut suffix);
        let dir = std::env::temp_dir().join(format!(
            "rc-ui-server-cfg-test-{}",
            encode_hex_lower(&suffix)
        ));
        let path = dir.join("nested").join("server.json");

        // Missing file falls back to sane default with fresh key.
        let initial = load_from_path(&path);
        assert!(!initial.enabled);
        assert_eq!(initial.signal_port, DEFAULT_SIGNAL_PORT);
        assert_eq!(initial.relay_port, DEFAULT_RELAY_PORT);
        assert_eq!(initial.device_key.len(), 64);

        let cfg = ServerConfig {
            enabled: true,
            server_host: "172.16.1.2".to_string(),
            signal_port: 21116,
            relay_port: 21117,
            api_port: 21118,
            use_https: true,
            ca_fingerprint_sha256: Some("aa".repeat(32)),
            ca_cert_pem: Some(
                "-----BEGIN CERTIFICATE-----\nTEST\n-----END CERTIFICATE-----".to_string(),
            ),
            server_auth_pubkey_hex: Some("bb".repeat(32)),
            device_key: generate_device_key(),
            cached_device_id: Some("604928235".to_string()),
        };
        save_to_path(&path, &cfg).expect("save_to_path must succeed");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        let loaded = load_from_path(&path);
        assert_eq!(loaded, cfg);

        // Corrupted JSON falls back cleanly without panicking.
        fs::write(&path, b"{not valid json").unwrap();
        let recovered = load_from_path(&path);
        assert!(!recovered.enabled);
        assert_eq!(recovered.device_key.len(), 64);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_path_regenerates_invalid_device_key_and_clears_cached_id() {
        let mut suffix = [0u8; 6];
        OsRng.fill_bytes(&mut suffix);
        let dir =
            std::env::temp_dir().join(format!("rc-ui-server-cfg-m5-{}", encode_hex_lower(&suffix)));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("server.json");

        // 1) 空白 device_key -> 重新生成并清空 cached_device_id
        let cases = [
            "   ".to_string(),
            "a".repeat(257),
            "abc\ndef".to_string(),
            "abc\0def".to_string(),
            "abc\x1bdef".to_string(),
        ];
        for bad_key in cases {
            let payload = serde_json::json!({
                "enabled": true,
                "server_host": "172.16.1.2",
                "signal_port": 21116,
                "relay_port": 21117,
                "device_key": bad_key,
                "cached_device_id": "604928235",
            });
            fs::write(&path, serde_json::to_vec(&payload).unwrap()).unwrap();
            let loaded = load_from_path(&path);
            assert_eq!(loaded.device_key.len(), 64);
            assert_eq!(loaded.cached_device_id, None);
        }

        // 2) 合法 device_key 带首尾空格 -> 裁剪后保留并保留 cached_device_id
        let ok_key = generate_device_key();
        let payload = serde_json::json!({
            "enabled": true,
            "server_host": "172.16.1.2",
            "signal_port": 21116,
            "relay_port": 21117,
            "device_key": format!("  {ok_key}  "),
            "cached_device_id": "604 928 235",
        });
        fs::write(&path, serde_json::to_vec(&payload).unwrap()).unwrap();
        let loaded = load_from_path(&path);
        assert_eq!(loaded.device_key, ok_key);
        assert_eq!(loaded.cached_device_id.as_deref(), Some("604928235"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_snapshot_serializes_with_camel_case_and_snake_case_state() {
        let rev = next_status_revision();
        let snap = ServerStatusSnapshot {
            enabled: true,
            server_host: "172.16.1.2".to_string(),
            signal_port: 21116,
            relay_port: 21117,
            state: ServerConnectionState::Connected,
            device_id: Some("604928235".to_string()),
            device_id_formatted: Some("604 928 235".to_string()),
            error_message: None,
            signal_addr: Some("172.16.1.2:21116".to_string()),
            relay_addr: Some("172.16.1.2:21117".to_string()),
            revision: rev,
        };
        let v = serde_json::to_value(&snap).unwrap();
        assert_eq!(v["enabled"], true);
        assert_eq!(v["serverHost"], "172.16.1.2");
        assert_eq!(v["signalPort"], 21116);
        assert_eq!(v["relayPort"], 21117);
        assert_eq!(v["state"], "connected");
        assert_eq!(v["deviceId"], "604928235");
        assert_eq!(v["deviceIdFormatted"], "604 928 235");
        assert!(v["errorMessage"].is_null());
        assert_eq!(v["signalAddr"], "172.16.1.2:21116");
        assert_eq!(v["relayAddr"], "172.16.1.2:21117");
        assert_eq!(v["revision"], rev);

        // 旧 JSON 缺省 revision 时反序列化为 0
        let legacy = serde_json::json!({
            "enabled": false,
            "serverHost": "",
            "signalPort": 21116,
            "relayPort": 21117,
            "state": "disabled",
            "deviceId": null,
            "deviceIdFormatted": null,
            "errorMessage": null,
            "signalAddr": null,
            "relayAddr": null,
        });
        let parsed: ServerStatusSnapshot = serde_json::from_value(legacy).unwrap();
        assert_eq!(parsed.revision, 0);

        // disabled_snapshot 分配严格单调递增的 revision
        let cfg = ServerConfig::default();
        let d1 = cfg.disabled_snapshot();
        let d2 = cfg.disabled_snapshot();
        assert!(d1.revision > rev);
        assert!(d2.revision > d1.revision);
    }

    #[test]
    fn legacy_server_json_deserializes_with_new_defaults_and_helpers() {
        let legacy_json = serde_json::json!({
            "enabled": true,
            "server_host": "172.16.1.2",
            "signal_port": 21116,
            "relay_port": 21117,
            "device_key": generate_device_key(),
            "cached_device_id": "604928235"
        });
        let mut cfg: ServerConfig = serde_json::from_value(legacy_json).unwrap();
        assert_eq!(cfg.api_port, DEFAULT_API_PORT);
        assert!(cfg.use_https);
        assert_eq!(cfg.ca_fingerprint_sha256, None);
        assert_eq!(cfg.ca_cert_pem, None);
        assert_eq!(cfg.server_auth_pubkey_hex, None);
        assert!(
            !cfg.is_configured(),
            "HTTPS config without anchored ca_cert_pem must require server probe"
        );
        cfg.ca_cert_pem =
            Some("-----BEGIN CERTIFICATE-----\nTEST\n-----END CERTIFICATE-----\n".to_string());
        assert!(cfg.is_configured());
        assert_eq!(cfg.api_addr().as_deref(), Some("172.16.1.2:21118"));
        assert_eq!(
            cfg.api_base_url().as_deref(),
            Some("https://172.16.1.2:21118")
        );

        let disabled = ServerConfig {
            enabled: false,
            server_host: "172.16.1.2".to_string(),
            ..ServerConfig::default()
        };
        assert!(!disabled.is_configured());

        let blank_host = ServerConfig {
            enabled: true,
            server_host: "   ".to_string(),
            ..ServerConfig::default()
        };
        assert!(!blank_host.is_configured());
        assert_eq!(blank_host.api_addr(), None);
        assert_eq!(blank_host.api_base_url(), None);
    }
}
