//! 应用级偏好设置与设备桌面壁纸快照的原子持久化。
//!
//! 本模块全纯 Rust 实现，不依赖 Tauri 事件循环，支持离线确定性单测；
//! 全模块 `#![deny(unsafe_code)]` 与 `#![deny(missing_docs)]`。

#![deny(unsafe_code)]
#![deny(missing_docs)]

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::display_ctrl::base64_encode;

/// 默认码率预设名称。
pub const DEFAULT_BITRATE_PRESET: &str = "14m";

/// 默认码率（bps：14 Mbps）。
pub const DEFAULT_BITRATE_BPS: u32 = 14_000_000;

/// 默认目标帧率（fps）。
pub const DEFAULT_FPS: u32 = 144;

/// 允许的码率下限（1 Mbps）。
pub const MIN_BITRATE_BPS: u32 = 1_000_000;

/// 允许的码率上限（60 Mbps）。
pub const MAX_BITRATE_BPS: u32 = 60_000_000;

/// 允许的帧率下限（15 fps）。
pub const MIN_FPS: u32 = 15;

/// 允许的帧率上限（240 fps）。
pub const MAX_FPS: u32 = 240;

/// 允许的最小自定义分辨率宽度（像素）。
pub const MIN_RESOLUTION_WIDTH: u32 = 320;

/// 允许的最小自定义分辨率高度（像素）。
pub const MIN_RESOLUTION_HEIGHT: u32 = 240;

/// 单张设备桌面壁纸缩略图的最大字节数（96 KiB）。
pub const MAX_WALLPAPER_BYTES: usize = 96 * 1024;

/// 默认是否开启端到端加密（mTLS）连接（待 Wave 4 实测 mTLS 延迟损耗确认 ≤ 3ms 后最终定稿）。
pub const DEFAULT_ENCRYPT_CONNECTION: bool = true;

/// 持久化于本地的单个远端设备快照记录。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DeviceSnapshotRecord {
    /// 设备可读名称。
    pub name: String,
    /// 最近一次连接或刷新时间戳（Unix 秒或毫秒，由调用方统一注入）。
    pub last_connected_ts: u64,
    /// 保存在壁纸目录中的安全文件名（不含路径分隔符）。
    pub wallpaper_file: Option<String>,
}

/// 面向前端 IPC 展示的设备快照视图（含内联 `data:image/jpeg;base64,...` 壁纸 URL）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceSnapshotView {
    /// 设备索引键（如地址、设备码或规范化标识）。
    pub key: String,
    /// 设备可读名称。
    pub name: String,
    /// 最近一次连接时间戳。
    pub last_connected_ts: u64,
    /// 内联 Data URL（壁纸缺失或校验失败时为 `None`）。
    pub wallpaper_data_url: Option<String>,
}

/// 持久化于本地的单个远端设备 TCP 端口映射规则配置。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SavedPortMapRule {
    /// 规则显示名称。
    pub name: String,
    /// 本机监听端口（`1..=65535`）。
    pub local_port: u16,
    /// 远端目标地址（IP 或主机名）。
    pub target_addr: String,
    /// 远端目标端口（`1..=65535`）。
    pub target_port: u16,
    /// 是否处于启用状态。
    pub enabled: bool,
}

/// 持久化于本地磁盘的应用偏好设置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppPrefs {
    /// 是否开启「允许其他设备远控本机」（默认 `false`）。
    #[serde(default)]
    pub allow_remote_assist: bool,
    /// 自定义访问验证码（`None` = 临时验证码模式；`Some("")` = 无密码；`Some(pw)` = 自定义密码）。
    #[serde(default)]
    pub custom_host_password: Option<String>,
    /// 当前选择的码率档位（`"auto"` / `"30m"` / `"14m"` / `"8m"`，默认 `"14m"`）。
    #[serde(default = "default_bitrate_preset")]
    pub bitrate_preset: String,
    /// 当前目标码率（bps，默认 `14_000_000`）。
    #[serde(default = "default_bitrate_bps")]
    pub bitrate_bps: u32,
    /// 当前目标帧率（fps，默认 `144`）。
    #[serde(default = "default_fps")]
    pub fps: u32,
    /// 记住的远控流分辨率宽度（`None` 表示跟随自动协商）。
    #[serde(default)]
    pub resolution_width: Option<u32>,
    /// 记住的远控流分辨率高度（`None` 表示跟随自动协商）。
    #[serde(default)]
    pub resolution_height: Option<u32>,
    /// S1 会话窗口是否显示网络状态 HUD（默认 `true`）。
    #[serde(default = "default_show_hud")]
    pub show_hud: bool,
    /// 退出远控时是否自动锁定被控端屏幕（默认 `false`）。
    #[serde(default)]
    pub lock_on_exit: bool,
    /// 是否默认开启端到端加密（mTLS）连接（默认 `true`）。
    #[serde(default = "default_encrypt_connection")]
    pub default_encrypt_connection: bool,
    /// 是否在设备列表展示局域网发现设备（默认 `false`）。
    #[serde(default)]
    pub show_lan_devices: bool,
    /// 是否允许同账号下自己的设备免密自控本机（默认 `true`）。
    #[serde(default = "default_allow_owner_self_control")]
    pub allow_owner_self_control: bool,
    /// 按设备标识索引的历史连接与壁纸快照映射。
    #[serde(default)]
    pub device_snapshots: BTreeMap<String, DeviceSnapshotRecord>,
    /// 按设备标识（或连接目标）持久化的 W4 TCP 端口映射规则列表。
    #[serde(default)]
    pub saved_portmaps: BTreeMap<String, Vec<SavedPortMapRule>>,
}

fn default_bitrate_preset() -> String {
    DEFAULT_BITRATE_PRESET.to_string()
}

fn default_bitrate_bps() -> u32 {
    DEFAULT_BITRATE_BPS
}

fn default_fps() -> u32 {
    DEFAULT_FPS
}

fn default_show_hud() -> bool {
    true
}

fn default_encrypt_connection() -> bool {
    DEFAULT_ENCRYPT_CONNECTION
}

fn default_allow_owner_self_control() -> bool {
    true
}

impl Default for AppPrefs {
    fn default() -> Self {
        Self {
            allow_remote_assist: false,
            custom_host_password: None,
            bitrate_preset: default_bitrate_preset(),
            bitrate_bps: DEFAULT_BITRATE_BPS,
            fps: DEFAULT_FPS,
            resolution_width: None,
            resolution_height: None,
            show_hud: true,
            lock_on_exit: false,
            default_encrypt_connection: DEFAULT_ENCRYPT_CONNECTION,
            show_lan_devices: false,
            allow_owner_self_control: true,
            device_snapshots: BTreeMap::new(),
            saved_portmaps: BTreeMap::new(),
        }
    }
}

/// 安全、连接加密与设备可见性偏好的增量更新补丁（经 Tauri IPC 反序列化）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SecurityPrefsPatch {
    /// 可选「允许其他设备远程协助本机」开关。
    pub allow_remote_assist: Option<bool>,
    /// 可选「默认开启加密连接（mTLS）」开关。
    pub default_encrypt_connection: Option<bool>,
    /// 可选「显示局域网设备」开关。
    pub show_lan_devices: Option<bool>,
    /// 可选「允许同账号免密自控」开关。
    pub allow_owner_self_control: Option<bool>,
    /// 可选自定义访问验证码（`Some("")` 表示无密码；`Some(pw)` 长度不超过 64 字符且无控制字符）。
    pub custom_host_password: Option<String>,
    /// 为 `Some(true)` 时清除自定义密码并切回临时验证码模式。
    pub clear_custom_host_password: Option<bool>,
}

/// S1 远控流与 HUD 偏好的增量更新补丁（经 Tauri IPC 反序列化）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StreamPrefsPatch {
    /// 可选码率档位（`"auto"` / `"30m"` / `"14m"` / `"8m"`）。
    pub bitrate_preset: Option<String>,
    /// 可选目标码率（bps，范围 `1_000_000..=60_000_000`）。
    pub bitrate_bps: Option<u32>,
    /// 可选目标帧率（fps，范围 `15..=240`）。
    pub fps: Option<u32>,
    /// 可选目标分辨率宽度（需与 `resolution_height` 成对提供，且 `>= 320`）。
    pub resolution_width: Option<u32>,
    /// 可选目标分辨率高度（需与 `resolution_width` 成对提供，且 `>= 240`）。
    pub resolution_height: Option<u32>,
    /// 为 `Some(true)` 时清除已保存的自定义分辨率。
    pub clear_resolution: Option<bool>,
    /// 可选 HUD 显隐开关。
    pub show_hud: Option<bool>,
    /// 可选「退出时锁定被控端」开关。
    pub lock_on_exit: Option<bool>,
}

fn is_valid_bitrate_preset(preset: &str) -> bool {
    matches!(preset, "auto" | "30m" | "14m" | "8m")
}

impl AppPrefs {
    /// 校验并原子应用远控流与 HUD 偏好增量补丁。
    ///
    /// 若任一字段校验失败则返回 `Err` 且保持原对象完全不变：
    /// - `bitrate_preset` 须属于 `["auto", "30m", "14m", "8m"]`；
    /// - `bitrate_bps` 须在 `1_000_000..=60_000_000`；
    /// - `fps` 须在 `15..=240`；
    /// - 若 `clear_resolution == Some(true)` 则清空分辨率；否则若传入分辨率宽高，
    ///   必须成对提供且满足 `width >= 320 && height >= 240`。
    pub fn apply_stream_patch(&mut self, patch: StreamPrefsPatch) -> Result<(), String> {
        if let Some(ref preset) = patch.bitrate_preset {
            if !is_valid_bitrate_preset(preset) {
                return Err(format!(
                    "无效码率档位：{preset:?}（允许 auto / 30m / 14m / 8m）"
                ));
            }
        }
        if let Some(bps) = patch.bitrate_bps {
            if !(MIN_BITRATE_BPS..=MAX_BITRATE_BPS).contains(&bps) {
                return Err(format!(
                    "码率超出允许范围：{bps}（允许 {MIN_BITRATE_BPS}..={MAX_BITRATE_BPS} bps）"
                ));
            }
        }
        if let Some(fps) = patch.fps {
            if !(MIN_FPS..=MAX_FPS).contains(&fps) {
                return Err(format!(
                    "帧率超出允许范围：{fps}（允许 {MIN_FPS}..={MAX_FPS} fps）"
                ));
            }
        }

        let clear_res = patch.clear_resolution.unwrap_or(false);
        let next_res = if clear_res {
            Some((None, None))
        } else {
            match (patch.resolution_width, patch.resolution_height) {
                (None, None) => None,
                (Some(w), Some(h)) => {
                    if w < MIN_RESOLUTION_WIDTH || h < MIN_RESOLUTION_HEIGHT {
                        return Err(format!(
                            "分辨率过小：{w}×{h}（最小 {MIN_RESOLUTION_WIDTH}×{MIN_RESOLUTION_HEIGHT}）"
                        ));
                    }
                    Some((Some(w), Some(h)))
                }
                _ => {
                    return Err("resolutionWidth 与 resolutionHeight 必须同时提供".to_string());
                }
            }
        };

        if let Some(preset) = patch.bitrate_preset {
            self.bitrate_preset = preset;
        }
        if let Some(bps) = patch.bitrate_bps {
            self.bitrate_bps = bps;
        }
        if let Some(fps) = patch.fps {
            self.fps = fps;
        }
        if let Some((w, h)) = next_res {
            self.resolution_width = w;
            self.resolution_height = h;
        }
        if let Some(show_hud) = patch.show_hud {
            self.show_hud = show_hud;
        }
        if let Some(lock_on_exit) = patch.lock_on_exit {
            self.lock_on_exit = lock_on_exit;
        }
        Ok(())
    }

    /// 校验并原子应用安全与加密偏好增量补丁。
    ///
    /// 若任一字段校验失败则返回 `Err` 且保持原对象完全不变：
    /// - 若 `clear_custom_host_password == Some(true)` 则清空自定义密码；
    /// - 否则若传入 `custom_host_password`，须满足字符数 `<= 64` 且不含控制字符或 NUL。
    pub fn apply_security_patch(&mut self, patch: SecurityPrefsPatch) -> Result<(), String> {
        let clear_pw = patch.clear_custom_host_password.unwrap_or(false);
        if !clear_pw {
            if let Some(ref pw) = patch.custom_host_password {
                if pw.chars().count() > 64 || pw.chars().any(|ch| ch.is_control() || ch == '\0') {
                    return Err(
                        "自定义访问验证码长度不能超过 64 个字符，且不能包含控制字符".to_string()
                    );
                }
            }
        }

        if let Some(v) = patch.allow_remote_assist {
            self.allow_remote_assist = v;
        }
        if let Some(v) = patch.default_encrypt_connection {
            self.default_encrypt_connection = v;
        }
        if let Some(v) = patch.show_lan_devices {
            self.show_lan_devices = v;
        }
        if let Some(v) = patch.allow_owner_self_control {
            self.allow_owner_self_control = v;
        }
        if clear_pw {
            self.custom_host_password = None;
        } else if let Some(pw) = patch.custom_host_password {
            self.custom_host_password = Some(pw);
        }
        Ok(())
    }

    /// 导出包含内联 Data URL 的设备快照视图列表。
    pub fn snapshot_views(&self, wallpapers_dir: &Path) -> Vec<DeviceSnapshotView> {
        self.device_snapshots
            .iter()
            .map(|(key, rec)| DeviceSnapshotView {
                key: key.clone(),
                name: rec.name.clone(),
                last_connected_ts: rec.last_connected_ts,
                wallpaper_data_url: rec
                    .wallpaper_file
                    .as_deref()
                    .and_then(|file| load_device_wallpaper_data_url(wallpapers_dir, file)),
            })
            .collect()
    }

    /// 更新指定设备的 W4 持久化端口映射规则列表（过滤无效项并截断至最多 32 条；空列表移除该设备键）。
    pub fn set_device_portmaps(&mut self, device_key: &str, rules: Vec<SavedPortMapRule>) {
        let key = device_key.trim();
        if key.is_empty() {
            return;
        }
        let mut clean = Vec::new();
        for mut rule in rules {
            rule.name = rule.name.trim().to_string();
            rule.target_addr = rule.target_addr.trim().to_string();
            if rule.name.is_empty()
                || rule.target_addr.is_empty()
                || rule
                    .target_addr
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control())
                || rule.local_port == 0
                || rule.target_port == 0
            {
                continue;
            }
            if clean.len() >= 32 {
                break;
            }
            clean.push(rule);
        }
        if clean.is_empty() {
            self.saved_portmaps.remove(key);
        } else {
            self.saved_portmaps.insert(key.to_string(), clean);
        }
    }
}

pub(crate) fn config_root_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".config").join("rabbitcontrol")
}

/// 返回默认应用偏好文件路径：`$HOME/.config/rabbitcontrol/prefs.json`。
pub fn default_prefs_path() -> PathBuf {
    config_root_dir().join("prefs.json")
}

/// 返回默认设备壁纸缓存目录：`$HOME/.config/rabbitcontrol/wallpapers`。
pub fn default_wallpapers_dir() -> PathBuf {
    config_root_dir().join("wallpapers")
}

pub(crate) fn encode_hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0F) as usize] as char);
    }
    out
}

fn sanitize_prefs(prefs: &mut AppPrefs) {
    if let Some(ref pw) = prefs.custom_host_password {
        if pw.chars().count() > 64 || pw.chars().any(|ch| ch.is_control() || ch == '\0') {
            prefs.custom_host_password = None;
        }
    }
    if !is_valid_bitrate_preset(&prefs.bitrate_preset) {
        prefs.bitrate_preset = default_bitrate_preset();
    }
    if !(MIN_BITRATE_BPS..=MAX_BITRATE_BPS).contains(&prefs.bitrate_bps) {
        prefs.bitrate_bps = DEFAULT_BITRATE_BPS;
    }
    if !(MIN_FPS..=MAX_FPS).contains(&prefs.fps) {
        prefs.fps = DEFAULT_FPS;
    }
    let valid_res = matches!(
        (prefs.resolution_width, prefs.resolution_height),
        (Some(w), Some(h)) if w >= MIN_RESOLUTION_WIDTH && h >= MIN_RESOLUTION_HEIGHT
    );
    if !valid_res {
        prefs.resolution_width = None;
        prefs.resolution_height = None;
    }
    for rec in prefs.device_snapshots.values_mut() {
        if let Some(ref file) = rec.wallpaper_file {
            if !is_safe_wallpaper_filename(file) {
                rec.wallpaper_file = None;
            }
        }
    }
    prefs.saved_portmaps.retain(|k, rules| {
        if k.trim().is_empty() {
            return false;
        }
        for r in rules.iter_mut() {
            r.name = r.name.trim().to_string();
            r.target_addr = r.target_addr.trim().to_string();
        }
        rules.retain(|r| {
            !r.name.is_empty()
                && !r.target_addr.is_empty()
                && !r
                    .target_addr
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control())
                && r.local_port > 0
                && r.target_port > 0
        });
        rules.truncate(32);
        !rules.is_empty()
    });
}

/// 从指定路径读取应用偏好设置；文件不存在或内容损坏时回退默认值，并对越界字段自动修正。
pub fn load_from_path(path: &Path) -> AppPrefs {
    let Ok(bytes) = fs::read(path) else {
        return AppPrefs::default();
    };
    let Ok(mut prefs) = serde_json::from_slice::<AppPrefs>(&bytes) else {
        return AppPrefs::default();
    };
    sanitize_prefs(&mut prefs);
    prefs
}

pub(crate) fn atomic_write_bytes(
    path: &Path,
    bytes: &[u8],
    append_newline: bool,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("创建目录 {} 失败：{e}", parent.display()))?;
        }
    }

    let mut rand_suffix = [0u8; 6];
    OsRng.fill_bytes(&mut rand_suffix);
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("prefs.tmp");
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
        file.write_all(bytes)?;
        if append_newline {
            file.write_all(b"\n")?;
        }
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
        return Err(format!("写入临时文件失败：{err}"));
    }

    if let Err(err) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(format!("原子替换文件 {} 失败：{err}", path.display()));
    }

    Ok(())
}

/// 将应用偏好原子写入指定路径（先写同目录临时文件并落盘，再 `rename` 覆盖）。
///
/// 会自动创建缺失的父目录；在 Unix 平台上将文件权限收紧为 `0600`。
pub fn save_to_path(path: &Path, prefs: &AppPrefs) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(prefs).map_err(|e| format!("序列化应用偏好失败：{e}"))?;
    atomic_write_bytes(path, &json, true)
}

fn sanitize_device_key_for_filename(device_key: &str) -> String {
    let mut cleaned = String::with_capacity(device_key.len().min(64));
    let mut prev_underscore = false;
    for ch in device_key.trim().chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            cleaned.push(ch);
            prev_underscore = ch == '_';
        } else if !prev_underscore && !cleaned.is_empty() {
            cleaned.push('_');
            prev_underscore = true;
        }
        if cleaned.len() >= 64 {
            break;
        }
    }
    let trimmed = cleaned.trim_matches('_');
    if trimmed.is_empty() {
        "device".to_string()
    } else {
        trimmed.to_string()
    }
}

fn is_safe_wallpaper_filename(filename: &str) -> bool {
    if filename.is_empty()
        || filename.contains('/')
        || filename.contains('\\')
        || filename.contains("..")
        || filename.contains('\0')
    {
        return false;
    }
    let mut components = Path::new(filename).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

/// 将远端桌面壁纸 JPEG 字节原子写入 `wallpapers_dir`，返回安全文件名（如 `192_168_1_10_9901.jpg`）。
///
/// 拒绝空载荷及超过 `96 KiB`（[`MAX_WALLPAPER_BYTES`]）的图像，并清洗 `device_key` 防止路径穿越。
pub fn save_device_wallpaper(
    wallpapers_dir: &Path,
    device_key: &str,
    jpeg_bytes: &[u8],
) -> Result<String, String> {
    if jpeg_bytes.is_empty() {
        return Err("壁纸数据不能为空".to_string());
    }
    if jpeg_bytes.len() > MAX_WALLPAPER_BYTES {
        return Err(format!(
            "壁纸体积超限：{} 字节（上限 {} 字节）",
            jpeg_bytes.len(),
            MAX_WALLPAPER_BYTES
        ));
    }

    let safe_stem = sanitize_device_key_for_filename(device_key);
    let filename = format!("{safe_stem}.jpg");
    if !is_safe_wallpaper_filename(&filename) {
        return Err("生成安全壁纸文件名失败".to_string());
    }

    let dest = wallpapers_dir.join(&filename);
    atomic_write_bytes(&dest, jpeg_bytes, false)?;
    Ok(filename)
}

/// 从 `wallpapers_dir` 读取指定壁纸文件并编码为 `data:image/jpeg;base64,...`；
/// 文件名非法（含 `/`、`\`、`..`）、文件不存在或超限时返回 `None`。
pub fn load_device_wallpaper_data_url(wallpapers_dir: &Path, filename: &str) -> Option<String> {
    if !is_safe_wallpaper_filename(filename) {
        return None;
    }
    let path = wallpapers_dir.join(filename);
    let bytes = fs::read(&path).ok()?;
    if bytes.is_empty() || bytes.len() > MAX_WALLPAPER_BYTES {
        return None;
    }
    let b64 = base64_encode(&bytes);
    Some(format!("data:image/jpeg;base64,{b64}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_test_dir(tag: &str) -> PathBuf {
        let mut suffix = [0u8; 6];
        OsRng.fill_bytes(&mut suffix);
        std::env::temp_dir().join(format!("rc-ui-{tag}-{}", encode_hex_lower(&suffix)))
    }

    #[test]
    fn default_paths_and_prefs_match_spec() {
        let prefs_path = default_prefs_path();
        assert!(prefs_path.ends_with(".config/rabbitcontrol/prefs.json"));
        let wp_dir = default_wallpapers_dir();
        assert!(wp_dir.ends_with(".config/rabbitcontrol/wallpapers"));

        let prefs = AppPrefs::default();
        assert!(!prefs.allow_remote_assist);
        assert_eq!(prefs.bitrate_preset, "14m");
        assert_eq!(prefs.bitrate_bps, 14_000_000);
        assert_eq!(prefs.fps, 144);
        assert_eq!(prefs.resolution_width, None);
        assert_eq!(prefs.resolution_height, None);
        assert!(prefs.show_hud);
        assert!(prefs.device_snapshots.is_empty());

        let json_val = serde_json::to_value(&prefs).unwrap();
        assert_eq!(json_val["allowRemoteAssist"], false);
        assert_eq!(json_val["bitratePreset"], "14m");
        assert_eq!(json_val["bitrateBps"], 14_000_000);
        assert_eq!(json_val["fps"], 144);
        assert!(json_val["resolutionWidth"].is_null());
        assert!(json_val["resolutionHeight"].is_null());
        assert_eq!(json_val["showHud"], true);
        assert_eq!(json_val["deviceSnapshots"], serde_json::json!({}));

        // 空 JSON `{}` 反序列化必须恢复各项非零默认值
        let parsed_empty: AppPrefs = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed_empty, AppPrefs::default());
    }

    #[test]
    fn apply_stream_patch_validates_all_fields_atomically() {
        let mut prefs = AppPrefs::default();

        // 合法修改
        prefs
            .apply_stream_patch(StreamPrefsPatch {
                bitrate_preset: Some("30m".to_string()),
                bitrate_bps: Some(30_000_000),
                fps: Some(120),
                resolution_width: Some(1920),
                resolution_height: Some(1080),
                clear_resolution: None,
                show_hud: Some(false),
                lock_on_exit: Some(true),
            })
            .expect("valid patch");
        assert_eq!(prefs.bitrate_preset, "30m");
        assert_eq!(prefs.bitrate_bps, 30_000_000);
        assert_eq!(prefs.fps, 120);
        assert_eq!(prefs.resolution_width, Some(1920));
        assert_eq!(prefs.resolution_height, Some(1080));
        assert!(!prefs.show_hud);
        assert!(prefs.lock_on_exit);

        // 清除分辨率
        prefs
            .apply_stream_patch(StreamPrefsPatch {
                clear_resolution: Some(true),
                ..Default::default()
            })
            .expect("clear resolution");
        assert_eq!(prefs.resolution_width, None);
        assert_eq!(prefs.resolution_height, None);

        // 非法补丁不得部分修改状态
        let snapshot_before = prefs.clone();
        for bad in [
            StreamPrefsPatch {
                bitrate_preset: Some("100m".to_string()),
                fps: Some(60),
                ..Default::default()
            },
            StreamPrefsPatch {
                bitrate_bps: Some(500_000),
                ..Default::default()
            },
            StreamPrefsPatch {
                bitrate_bps: Some(60_000_001),
                ..Default::default()
            },
            StreamPrefsPatch {
                fps: Some(14),
                ..Default::default()
            },
            StreamPrefsPatch {
                fps: Some(241),
                ..Default::default()
            },
            StreamPrefsPatch {
                resolution_width: Some(318),
                resolution_height: Some(240),
                ..Default::default()
            },
            StreamPrefsPatch {
                resolution_width: Some(640),
                resolution_height: Some(238),
                ..Default::default()
            },
            StreamPrefsPatch {
                resolution_width: Some(1280),
                resolution_height: None,
                ..Default::default()
            },
        ] {
            assert!(
                prefs.apply_stream_patch(bad).is_err(),
                "expected validation error"
            );
            assert_eq!(prefs, snapshot_before);
        }
    }

    #[test]
    fn load_and_save_prefs_atomic_roundtrip_and_sanitization() {
        let dir = temp_test_dir("app-prefs");
        let path = dir.join("nested").join("prefs.json");

        // 缺失文件回退默认值
        assert_eq!(load_from_path(&path), AppPrefs::default());

        let mut prefs = AppPrefs {
            allow_remote_assist: true,
            custom_host_password: Some("MYCODE".to_string()),
            bitrate_preset: "8m".to_string(),
            bitrate_bps: 8_000_000,
            fps: 60,
            resolution_width: Some(1512),
            resolution_height: Some(982),
            show_hud: false,
            lock_on_exit: true,
            default_encrypt_connection: false,
            show_lan_devices: true,
            allow_owner_self_control: false,
            device_snapshots: BTreeMap::new(),
            saved_portmaps: BTreeMap::new(),
        };
        prefs.device_snapshots.insert(
            "604928235".to_string(),
            DeviceSnapshotRecord {
                name: "Studio Mac".to_string(),
                last_connected_ts: 1_732_000_000,
                wallpaper_file: Some("604928235.jpg".to_string()),
            },
        );

        save_to_path(&path, &prefs).expect("save prefs");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        assert_eq!(load_from_path(&path), prefs);

        // 越界字段与非法壁纸文件名在读取时被清洗
        let tainted = serde_json::json!({
            "allowRemoteAssist": true,
            "bitratePreset": "invalid",
            "bitrateBps": 123,
            "fps": 999,
            "resolutionWidth": 100,
            "resolutionHeight": 100,
            "showHud": false,
            "deviceSnapshots": {
                "evil": {
                    "name": "Evil",
                    "lastConnectedTs": 42,
                    "wallpaperFile": "../secret.jpg"
                }
            }
        });
        fs::write(&path, serde_json::to_vec(&tainted).unwrap()).unwrap();
        let cleaned = load_from_path(&path);
        assert!(cleaned.allow_remote_assist);
        assert_eq!(cleaned.bitrate_preset, DEFAULT_BITRATE_PRESET);
        assert_eq!(cleaned.bitrate_bps, DEFAULT_BITRATE_BPS);
        assert_eq!(cleaned.fps, DEFAULT_FPS);
        assert_eq!(cleaned.resolution_width, None);
        assert_eq!(cleaned.resolution_height, None);
        assert!(!cleaned.show_hud);
        assert!(!cleaned.lock_on_exit);
        assert_eq!(cleaned.device_snapshots["evil"].wallpaper_file, None);

        // 损坏 JSON 安全回退默认
        fs::write(&path, b"{corrupt").unwrap();
        assert_eq!(load_from_path(&path), AppPrefs::default());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn device_wallpaper_save_load_sanitizes_and_enforces_limits() {
        let dir = temp_test_dir("wallpapers");
        let wp_dir = dir.join("wallpapers");
        let jpeg = vec![0xFFu8, 0xD8, 0xFF, 0xE0, 0x01, 0x02, 0x03];

        // 拒绝空与超大载荷
        assert!(save_device_wallpaper(&wp_dir, "dev1", &[]).is_err());
        let oversized = vec![0xABu8; MAX_WALLPAPER_BYTES + 1];
        assert!(save_device_wallpaper(&wp_dir, "dev1", &oversized).is_err());

        // 清洗危险设备键为安全文件名
        let file1 = save_device_wallpaper(&wp_dir, "../../192.168.1.10:9901", &jpeg)
            .expect("save sanitized wallpaper");
        assert!(!file1.contains('/'));
        assert!(!file1.contains('\\'));
        assert!(!file1.contains(".."));
        assert!(file1.ends_with(".jpg"));

        let data_url = load_device_wallpaper_data_url(&wp_dir, &file1).expect("load data url");
        assert_eq!(
            data_url,
            format!("data:image/jpeg;base64,{}", base64_encode(&jpeg))
        );

        // 路径穿越文件名全部拒绝
        for bad_name in [
            "",
            "../a.jpg",
            "sub/a.jpg",
            "sub\\a.jpg",
            "a..b.jpg",
            "missing.jpg",
        ] {
            assert_eq!(
                load_device_wallpaper_data_url(&wp_dir, bad_name),
                None,
                "expected None for {bad_name:?}"
            );
        }

        // DeviceSnapshotView 序列化字段名校验
        let mut prefs = AppPrefs::default();
        prefs.device_snapshots.insert(
            "192.168.1.10:9901".to_string(),
            DeviceSnapshotRecord {
                name: "MacBook Pro".to_string(),
                last_connected_ts: 123456,
                wallpaper_file: Some(file1),
            },
        );
        let views = prefs.snapshot_views(&wp_dir);
        assert_eq!(views.len(), 1);
        let v = serde_json::to_value(&views[0]).unwrap();
        assert_eq!(v["key"], "192.168.1.10:9901");
        assert_eq!(v["name"], "MacBook Pro");
        assert_eq!(v["lastConnectedTs"], 123456);
        assert_eq!(v["wallpaperDataUrl"], data_url);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn security_prefs_defaults_and_patch_validation() {
        let mut prefs = AppPrefs::default();
        assert_eq!(prefs.default_encrypt_connection, DEFAULT_ENCRYPT_CONNECTION);
        assert!(prefs.default_encrypt_connection);
        assert!(!prefs.show_lan_devices);
        assert!(prefs.allow_owner_self_control);

        let json_val = serde_json::to_value(&prefs).unwrap();
        assert_eq!(json_val["defaultEncryptConnection"], true);
        assert_eq!(json_val["showLanDevices"], false);
        assert_eq!(json_val["allowOwnerSelfControl"], true);

        // 合法更新
        prefs
            .apply_security_patch(SecurityPrefsPatch {
                allow_remote_assist: Some(true),
                default_encrypt_connection: Some(false),
                show_lan_devices: Some(true),
                allow_owner_self_control: Some(false),
                custom_host_password: Some("PIN99".to_string()),
                clear_custom_host_password: None,
            })
            .expect("valid security patch");
        assert!(prefs.allow_remote_assist);
        assert!(!prefs.default_encrypt_connection);
        assert!(prefs.show_lan_devices);
        assert!(!prefs.allow_owner_self_control);
        assert_eq!(prefs.custom_host_password.as_deref(), Some("PIN99"));

        // 清除密码
        prefs
            .apply_security_patch(SecurityPrefsPatch {
                clear_custom_host_password: Some(true),
                ..Default::default()
            })
            .expect("clear custom password");
        assert_eq!(prefs.custom_host_password, None);

        // 非法密码不得部分修改其他字段
        let snapshot_before = prefs.clone();
        for bad_pw in ["a".repeat(65), "bad\npw".to_string(), "bad\0pw".to_string()] {
            assert!(prefs
                .apply_security_patch(SecurityPrefsPatch {
                    show_lan_devices: Some(false),
                    custom_host_password: Some(bad_pw),
                    ..Default::default()
                })
                .is_err());
            assert_eq!(prefs, snapshot_before);
        }
    }

    #[test]
    fn saved_portmaps_roundtrip_and_limits() {
        let dir = temp_test_dir("portmaps");
        let path = dir.join("prefs.json");
        let mut prefs = AppPrefs::default();
        prefs.set_device_portmaps(
            "dev-1",
            vec![
                SavedPortMapRule {
                    name: "SSH".to_string(),
                    local_port: 2222,
                    target_addr: "127.0.0.1".to_string(),
                    target_port: 22,
                    enabled: true,
                },
                SavedPortMapRule {
                    name: String::new(),
                    local_port: 0,
                    target_addr: "127.0.0.1".to_string(),
                    target_port: 80,
                    enabled: true,
                },
            ],
        );
        save_to_path(&path, &prefs).unwrap();
        let loaded = load_from_path(&path);
        let list = loaded.saved_portmaps.get("dev-1").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "SSH");
        assert_eq!(list[0].local_port, 2222);

        let mut cleared = loaded;
        cleared.set_device_portmaps("dev-1", Vec::new());
        assert!(!cleared.saved_portmaps.contains_key("dev-1"));

        let _ = fs::remove_dir_all(&dir);
    }
}
