//! 显示控制协议与分辨率预设生成（`TOOL_CHANNEL_DISPLAY_CTRL = 5`）。
//!
//! 承载控制端与被控端之间的实时流参数能力通告（[`DisplayControlMsg::Capabilities`]）
//! 与热切换请求（[`DisplayControlMsg::SetStream`]），并按被控端主显示器宽高比生成
//! 严格等比、偶数对齐、由高到低排列的分辨率预设列表（[`build_resolution_presets`]）。
//!
//! 全模块纯安全 Rust（`#![deny(unsafe_code)]`），不依赖 Tauri 或平台 FFI，便于确定性单测。

#![deny(unsafe_code)]

/// `TOOL_CHANNEL_DISPLAY_CTRL` 单条 JSON 载荷的最大允许字节数（96 KiB，支撑桌面壁纸与能力通告）。
pub const MAX_DISPLAY_CTRL_BYTES: usize = 96 * 1024;

/// 热切换流分辨率允许的最大宽度上限（4K UHD）。
pub const MAX_STREAM_WIDTH: u32 = 3840;

/// 热切换流分辨率允许的最大高度上限（4K UHD）。
pub const MAX_STREAM_HEIGHT: u32 = 2160;

/// 降采样预设保留的最小宽度（像素）。
const MIN_PRESET_WIDTH: u32 = 640;

/// 降采样预设保留的最小高度（像素）。
const MIN_PRESET_HEIGHT: u32 = 360;

/// 去重与「接近原生」判定的像素容差（避免出现高度几乎一致的重复档位）。
const PRESET_PROXIMITY_PX: u32 = 24;

/// 一条可供前端分辨率菜单展示的等比分辨率预设。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DisplayPreset {
    /// 预设画面宽度（像素，偶数对齐）。
    pub width: u32,
    /// 预设画面高度（像素，偶数对齐）。
    pub height: u32,
    /// 菜单展示标签（如 `"1512×982 (HiDPI)"` 或 `"1280×800"`）。
    pub label: String,
    /// 是否为被控端显示器原生逻辑分辨率。
    pub is_native: bool,
    /// 是否为 macOS HiDPI / Retina 缩放模式。
    #[serde(default)]
    pub is_hidpi: bool,
}

/// 经 `TOOL_CHANNEL_DISPLAY_CTRL`（可靠 Stream A）双向传递的显示控制消息。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DisplayControlMsg {
    /// 被控端 → 控制端：通告当前生效分辨率/帧率以及原生/最高显示能力。
    Capabilities {
        /// 当前生效流宽度（像素）。
        current_width: u32,
        /// 当前生效流高度（像素）。
        current_height: u32,
        /// 当前生效帧率（fps）。
        current_fps: u32,
        /// 被控端主显示器原生逻辑宽度（像素）。
        native_width: u32,
        /// 被控端主显示器原生逻辑高度（像素）。
        native_height: u32,
        /// 被控端允许的最高输出宽度（Retina 物理像素或能力上界，封顶 3840）。
        max_width: u32,
        /// 被控端允许的最高输出高度（Retina 物理像素或能力上界，封顶 2160）。
        max_height: u32,
        /// 被控端支持的最高帧率（fps）。
        max_fps: u32,
        /// 被控端权威计算或上报的可选分辨率预设列表（为空时由控制端按原生宽高比回退生成）。
        #[serde(default)]
        presets: Vec<DisplayPreset>,
    },
    /// 控制端 → 被控端：请求热切换视频流的分辨率与帧率。
    SetStream {
        /// 目标视频宽度（像素；`0` 表示保持当前分辨率）。
        width: u32,
        /// 目标视频高度（像素；`0` 表示保持当前分辨率）。
        height: u32,
        /// 目标帧率（fps；`0` 表示保持当前帧率）。
        fps: u32,
    },
    /// 被控端 → 控制端：推送当前桌面壁纸缩略图（JPEG/PNG Base64）。
    Wallpaper {
        /// MIME 类型（如 `"image/jpeg"` 或 `"image/png"`）。
        mime: String,
        /// Base64 编码的图片字节数据。
        data_base64: String,
    },
    /// 控制端 → 被控端：请求立即锁定被控端屏幕。
    LockScreen,
}

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// 将原始字节切片编码为标准 RFC 4648 Base64 字符串（含 `=` 填充）。
pub fn base64_encode(input: &[u8]) -> String {
    if input.is_empty() {
        return String::new();
    }
    let out_len = input.len().div_ceil(3) * 4;
    let mut out = String::with_capacity(out_len);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(*chunk.get(1).unwrap_or(&0));
        let b2 = u32::from(*chunk.get(2).unwrap_or(&0));
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(BASE64_ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(BASE64_ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(BASE64_ALPHABET[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(BASE64_ALPHABET[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn base64_decode_val(b: u8) -> Option<u8> {
    match b {
        b'A'..=b'Z' => Some(b - b'A'),
        b'a'..=b'z' => Some(b - b'a' + 26),
        b'0'..=b'9' => Some(b - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// 将标准 RFC 4648 Base64 字符串解码为原始字节；长度、字符或填充非法时返回 `None`。
pub fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    if bytes.is_empty() {
        return Some(Vec::new());
    }
    if bytes.len() % 4 != 0 {
        return None;
    }
    let num_groups = bytes.len() / 4;
    let mut out = Vec::with_capacity(num_groups * 3);
    for (idx, chunk) in bytes.chunks_exact(4).enumerate() {
        let is_last = idx + 1 == num_groups;
        let a = base64_decode_val(chunk[0])?;
        let b = base64_decode_val(chunk[1])?;
        match (chunk[2], chunk[3]) {
            (b'=', b'=') => {
                if !is_last || (b & 0x0F) != 0 {
                    return None;
                }
                out.push((a << 2) | (b >> 4));
            }
            (c_ch, b'=') => {
                if !is_last {
                    return None;
                }
                let c = base64_decode_val(c_ch)?;
                if (c & 0x03) != 0 {
                    return None;
                }
                out.push((a << 2) | (b >> 4));
                out.push((b << 4) | (c >> 2));
            }
            (c_ch, d_ch) => {
                let c = base64_decode_val(c_ch)?;
                let d = base64_decode_val(d_ch)?;
                out.push((a << 2) | (b >> 4));
                out.push((b << 4) | (c >> 2));
                out.push((c << 6) | d);
            }
        }
    }
    Some(out)
}

/// 将正浮点数按最接近的偶数对齐（不低于 2）。
fn round_to_even(v: f64) -> u32 {
    if !v.is_finite() || v < 2.0 {
        return 2;
    }
    (((v / 2.0).round() as u32).saturating_mul(2)).max(2)
}

/// 在最大包络 `(bound_w, bound_h)` 内按 `(aspect_w, aspect_h)` 等比缩放并偶数对齐。
pub fn fit_within_cap(bound_w: u32, bound_h: u32, aspect_w: u32, aspect_h: u32) -> (u32, u32) {
    let max_even_w = bound_w & !1;
    let max_even_h = bound_h & !1;
    if max_even_w < 2 || max_even_h < 2 || aspect_w == 0 || aspect_h == 0 {
        return (max_even_w.max(2), max_even_h.max(2));
    }
    if u64::from(max_even_w) * u64::from(aspect_h) <= u64::from(max_even_h) * u64::from(aspect_w) {
        let w = max_even_w;
        let h = round_to_even(f64::from(w) * f64::from(aspect_h) / f64::from(aspect_w))
            .clamp(2, max_even_h);
        (w, h)
    } else {
        let h = max_even_h;
        let w = round_to_even(f64::from(h) * f64::from(aspect_w) / f64::from(aspect_h))
            .clamp(2, max_even_w);
        (w, h)
    }
}

/// 将控制端请求的目标 `(target_w, target_h)` 限制在 `(max_w, max_h)`（并封顶 `3840×2160`）内，
/// 同时按 `(aspect_w, aspect_h)` 保持原生宽高比并偶数对齐。
pub fn fit_target_preserving_aspect(
    target_w: u32,
    target_h: u32,
    aspect_w: u32,
    aspect_h: u32,
    max_w: u32,
    max_h: u32,
) -> (u32, u32) {
    let cap_w = max_w.clamp(2, MAX_STREAM_WIDTH) & !1;
    let cap_h = max_h.clamp(2, MAX_STREAM_HEIGHT) & !1;
    let clamped_w = target_w.clamp(2, cap_w);
    let clamped_h = target_h.clamp(2, cap_h);
    fit_within_cap(clamped_w, clamped_h, aspect_w, aspect_h)
}

/// 根据显示器原生逻辑分辨率 `(native_w, native_h)` 与最大能力 `(max_w, max_h)` 生成
/// 去重、严格保持原生宽高比、偶数对齐且按由高到低排序的分辨率预设列表。
///
/// 规则：
/// 1. 若 `max_w > native_w`（如 Retina 2x 屏），加入封顶 `3840×2160` 且等比适配的高分上限，
///    并在中间存在明显档位时补充 `1440p` 等比过渡档；
/// 2. 始终包含原生逻辑分辨率，标记 `is_native: true`，文案为 `format!("原生 {nw}×{nh}")`；
/// 3. 按目标高度 `[1440, 1080, 900, 720, 540]` 生成等比降采样档位（跳过距原生或已有档位
///    24px 以内的条目，且仅保留 `width >= 640 && height >= 360`）。
pub fn build_resolution_presets(
    native_w: u32,
    native_h: u32,
    max_w: u32,
    max_h: u32,
) -> Vec<DisplayPreset> {
    let nw = native_w & !1;
    let nh = native_h & !1;
    if nw < 2 || nh < 2 {
        return Vec::new();
    }

    let eff_max_w = max_w.max(nw).min(MAX_STREAM_WIDTH);
    let eff_max_h = max_h.max(nh).min(MAX_STREAM_HEIGHT);
    let (hi_w, hi_h) = fit_within_cap(eff_max_w, eff_max_h, nw, nh);

    let mut presets: Vec<DisplayPreset> = Vec::new();
    let is_too_close_to = |w: u32, h: u32, list: &[DisplayPreset]| -> bool {
        list.iter().any(|p| {
            (p.width == w && p.height == h)
                || p.height.abs_diff(h) <= PRESET_PROXIMITY_PX
                || (p.width.abs_diff(w) <= PRESET_PROXIMITY_PX
                    && p.height.abs_diff(h) <= PRESET_PROXIMITY_PX)
        })
    };

    // 1) Retina / 高分档位（高于原生且差距 > 24px）
    if max_w > native_w && hi_w > nw + PRESET_PROXIMITY_PX && hi_h > nh + PRESET_PROXIMITY_PX {
        presets.push(DisplayPreset {
            width: hi_w,
            height: hi_h,
            label: format!("{hi_w}×{hi_h}"),
            is_native: false,
            is_hidpi: false,
        });

        // 中间过渡档（1440p 高度）：落在原生与高分上限之间且距两端均 > 24px
        let mid_h = 1440u32;
        if mid_h > nh + PRESET_PROXIMITY_PX && mid_h + PRESET_PROXIMITY_PX < hi_h {
            let mid_w = round_to_even(f64::from(mid_h) * f64::from(nw) / f64::from(nh));
            if mid_w >= MIN_PRESET_WIDTH
                && mid_w <= hi_w
                && !is_too_close_to(mid_w, mid_h, &presets)
            {
                presets.push(DisplayPreset {
                    width: mid_w,
                    height: mid_h,
                    label: format!("{mid_w}×{mid_h}"),
                    is_native: false,
                    is_hidpi: false,
                });
            }
        }
    }

    // 2) 原生逻辑分辨率（必选）
    presets.push(DisplayPreset {
        width: nw,
        height: nh,
        label: format!("原生 {nw}×{nh}"),
        is_native: true,
        is_hidpi: false,
    });

    // 3) 降采样等比档位：[1440, 1080, 900, 720, 540]
    for target_h in [1440u32, 1080, 900, 720, 540] {
        if target_h >= nh || nh.abs_diff(target_h) <= PRESET_PROXIMITY_PX {
            continue;
        }
        let h = target_h & !1;
        let w = round_to_even(f64::from(h) * f64::from(nw) / f64::from(nh));
        if w < MIN_PRESET_WIDTH || h < MIN_PRESET_HEIGHT || w > nw {
            continue;
        }
        if is_too_close_to(w, h, &presets) {
            continue;
        }
        presets.push(DisplayPreset {
            width: w,
            height: h,
            label: format!("{w}×{h}"),
            is_native: false,
            is_hidpi: false,
        });
    }

    // 按分辨率由高到低排序，并按 (width, height) 去重保底
    presets.sort_by_key(|a| std::cmp::Reverse((a.height, a.width)));
    presets.dedup_by(|a, b| a.width == b.width && a.height == b.height);
    presets
}

/// 将 [`DisplayControlMsg`] 序列化为 JSON 字节；超限（`> MAX_DISPLAY_CTRL_BYTES`）或序列化失败返回 `None`。
pub fn encode_display_ctrl(msg: &DisplayControlMsg) -> Option<Vec<u8>> {
    let bytes = serde_json::to_vec(msg).ok()?;
    (bytes.len() <= MAX_DISPLAY_CTRL_BYTES).then_some(bytes)
}

/// 从 JSON 字节反序列化 [`DisplayControlMsg`]；空载荷、超限（`> MAX_DISPLAY_CTRL_BYTES`）或格式错误返回 `None`。
pub fn decode_display_ctrl(bytes: &[u8]) -> Option<DisplayControlMsg> {
    if bytes.is_empty() || bytes.len() > MAX_DISPLAY_CTRL_BYTES {
        return None;
    }
    serde_json::from_slice(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_presets_valid(presets: &[DisplayPreset], native_w: u32, native_h: u32) {
        assert!(!presets.is_empty(), "预设列表不能为空");
        let native_count = presets.iter().filter(|p| p.is_native).count();
        assert_eq!(native_count, 1, "必须且仅有一条原生预设: {presets:?}");

        let native_aspect = f64::from(native_w) / f64::from(native_h);
        for (i, p) in presets.iter().enumerate() {
            assert_eq!(p.width % 2, 0, "宽必须偶数对齐: {p:?}");
            assert_eq!(p.height % 2, 0, "高必须偶数对齐: {p:?}");
            assert!(p.width <= MAX_STREAM_WIDTH && p.height <= MAX_STREAM_HEIGHT);
            let aspect = f64::from(p.width) / f64::from(p.height);
            let rel_err = (aspect - native_aspect).abs() / native_aspect;
            assert!(
                rel_err < 0.005,
                "预设 {p:?} 宽高比({aspect:.4}) 偏离原生 {native_w}×{native_h}({native_aspect:.4})"
            );
            if p.is_native {
                assert_eq!(p.width, native_w & !1);
                assert_eq!(p.height, native_h & !1);
                assert_eq!(p.label, format!("原生 {}×{}", native_w & !1, native_h & !1));
            } else {
                assert!(p.width >= MIN_PRESET_WIDTH && p.height >= MIN_PRESET_HEIGHT);
            }
            if i > 0 {
                let prev = &presets[i - 1];
                assert!(
                    (prev.height, prev.width) > (p.height, p.width),
                    "预设必须严格由高到低去重排序: prev={prev:?}, cur={p:?}"
                );
            }
        }
    }

    #[test]
    fn presets_for_14inch_mbp_retina() {
        let presets = build_resolution_presets(1512, 982, 3024, 1964);
        assert_presets_valid(&presets, 1512, 982);

        let dims: Vec<(u32, u32, bool)> = presets
            .iter()
            .map(|p| (p.width, p.height, p.is_native))
            .collect();
        assert_eq!(
            dims,
            vec![
                (3024, 1964, false),
                (2218, 1440, false),
                (1512, 982, true),
                (1386, 900, false),
                (1108, 720, false),
                (832, 540, false),
            ]
        );
    }

    #[test]
    fn presets_for_16_9_1080p() {
        let presets = build_resolution_presets(1920, 1080, 1920, 1080);
        assert_presets_valid(&presets, 1920, 1080);

        let dims: Vec<(u32, u32, bool)> = presets
            .iter()
            .map(|p| (p.width, p.height, p.is_native))
            .collect();
        assert_eq!(
            dims,
            vec![
                (1920, 1080, true),
                (1600, 900, false),
                (1280, 720, false),
                (960, 540, false),
            ]
        );
    }

    #[test]
    fn presets_for_5k_imac_capped_at_4k() {
        // 无论上界传入 3840x2160 还是 5120x2880，均封顶 3840x2160
        for (max_w, max_h) in [(3840, 2160), (5120, 2880)] {
            let presets = build_resolution_presets(2560, 1440, max_w, max_h);
            assert_presets_valid(&presets, 2560, 1440);

            let dims: Vec<(u32, u32, bool)> = presets
                .iter()
                .map(|p| (p.width, p.height, p.is_native))
                .collect();
            assert_eq!(
                dims,
                vec![
                    (3840, 2160, false),
                    (2560, 1440, true),
                    (1920, 1080, false),
                    (1600, 900, false),
                    (1280, 720, false),
                    (960, 540, false),
                ]
            );
        }
    }

    #[test]
    fn encode_decode_roundtrip_and_size_limit() {
        assert_eq!(MAX_DISPLAY_CTRL_BYTES, 96 * 1024);

        let caps = DisplayControlMsg::Capabilities {
            current_width: 1512,
            current_height: 982,
            current_fps: 120,
            native_width: 1512,
            native_height: 982,
            max_width: 3024,
            max_height: 1964,
            max_fps: 144,
            presets: build_resolution_presets(1512, 982, 3024, 1964),
        };
        let encoded_caps = encode_display_ctrl(&caps).expect("encode capabilities");
        assert_eq!(decode_display_ctrl(&encoded_caps), Some(caps));

        // 兼容旧版缺少 `presets` 字段的 JSON
        let legacy_json = serde_json::json!({
            "kind": "capabilities",
            "current_width": 1920,
            "current_height": 1080,
            "current_fps": 60,
            "native_width": 1920,
            "native_height": 1080,
            "max_width": 1920,
            "max_height": 1080,
            "max_fps": 144,
        });
        let decoded_legacy =
            decode_display_ctrl(&serde_json::to_vec(&legacy_json).unwrap()).expect("legacy caps");
        assert_eq!(
            decoded_legacy,
            DisplayControlMsg::Capabilities {
                current_width: 1920,
                current_height: 1080,
                current_fps: 60,
                native_width: 1920,
                native_height: 1080,
                max_width: 1920,
                max_height: 1080,
                max_fps: 144,
                presets: Vec::new(),
            }
        );

        let set_stream = DisplayControlMsg::SetStream {
            width: 2218,
            height: 1440,
            fps: 144,
        };
        let encoded_set = encode_display_ctrl(&set_stream).expect("encode set_stream");
        assert_eq!(decode_display_ctrl(&encoded_set), Some(set_stream));

        let wallpaper = DisplayControlMsg::Wallpaper {
            mime: "image/jpeg".to_string(),
            data_base64: base64_encode(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10]),
        };
        let encoded_wp = encode_display_ctrl(&wallpaper).expect("encode wallpaper");
        assert_eq!(decode_display_ctrl(&encoded_wp), Some(wallpaper));

        let encoded_lock =
            encode_display_ctrl(&DisplayControlMsg::LockScreen).expect("encode lock");
        assert_eq!(
            decode_display_ctrl(&encoded_lock),
            Some(DisplayControlMsg::LockScreen)
        );

        // 空载荷、非法 JSON、超过 96 KiB 均拒绝
        assert_eq!(decode_display_ctrl(&[]), None);
        assert_eq!(decode_display_ctrl(b"{not-json"), None);
        let oversized = vec![b' '; MAX_DISPLAY_CTRL_BYTES + 1];
        assert_eq!(decode_display_ctrl(&oversized), None);
    }

    #[test]
    fn base64_rfc4648_vectors_roundtrip_and_rejects_invalid() {
        let vectors: &[(&[u8], &str)] = &[
            (b"", ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
        ];
        for (raw, expected) in vectors {
            let enc = base64_encode(raw);
            assert_eq!(enc, *expected);
            assert_eq!(base64_decode(&enc).as_deref(), Some(*raw));
        }

        // 全字节值 0..=255 往返校验
        let all_bytes: Vec<u8> = (0..=255).collect();
        let enc_all = base64_encode(&all_bytes);
        assert_eq!(base64_decode(&enc_all), Some(all_bytes));

        // 非法长度、非法字符、错误填充位置、非规范末位比特均应拒绝
        for bad in [
            "A", "AB", "ABC", "ABCDE", "Zm9$", "Zm 9v", "====", "A=AA", "AA=A", "Zm9v====",
            "Zm8=Zm8=",
            // 非零未使用位：'Z','h' -> 0x66 + low bits 0x1 != 0 for "Zh=="
            "Zh==", "Zm9=",
        ] {
            assert_eq!(base64_decode(bad), None, "expected None for {bad:?}");
        }
    }

    #[test]
    fn fit_target_preserving_aspect_clamps_and_aligns() {
        assert_eq!(
            fit_target_preserving_aspect(1920, 1080, 1512, 982, 3024, 1964),
            (1662, 1080)
        );
        assert_eq!(
            fit_target_preserving_aspect(5120, 2880, 2560, 1440, 5120, 2880),
            (3840, 2160)
        );
        assert_eq!(
            fit_target_preserving_aspect(1280, 720, 1920, 1080, 3840, 2160),
            (1280, 720)
        );
    }
}
