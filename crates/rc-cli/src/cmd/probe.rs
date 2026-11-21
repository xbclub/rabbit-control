//! `rc-cli probe`：采集/编码/解码后端探测。
//!
//! macOS 上通过 preflight 权限 API（零弹窗）与 `rc-pipeline` 编解码工厂如实探测
//! ScreenCaptureKit 权限与 VideoToolbox 硬件编解码可用性，同时保留 `fake-loopback`
//! 兜底条目；其他平台优雅退回 `fake-loopback`。

use std::process::ExitCode;

use super::{CliResult, ProbeArgs};

/// 平台权限探测明细（目前用于 macOS 三重授权，其他平台为 `None`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CapturePermissions {
    screen_recording: bool,
    accessibility: bool,
    input_monitoring: bool,
}

/// 采集后端探测结果。
#[derive(Debug, Clone, PartialEq, Eq)]
struct CaptureProbe {
    available: bool,
    backend: String,
    reason: Option<String>,
    permissions: Option<CapturePermissions>,
}

/// 编码 / 解码后端探测条目。
#[derive(Debug, Clone, PartialEq, Eq)]
struct CodecProbe {
    codec: String,
    hardware: bool,
    backend: String,
    available: bool,
}

/// 完整探测报告（数据模型平台无关，与 JSON/文本渲染逻辑解耦）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProbeReport {
    capture: CaptureProbe,
    encoders: Vec<CodecProbe>,
    decoders: Vec<CodecProbe>,
    platform: &'static str,
    arch: &'static str,
}

/// 执行 `probe` 子命令。探测本身不会失败（兜底假后端恒可用）。
pub fn run(args: &ProbeArgs) -> CliResult<ExitCode> {
    let report = probe_backends();
    if args.json {
        println!("{}", render_json(&report));
    } else {
        println!("{}", render_human(&report));
    }
    Ok(ExitCode::SUCCESS)
}

/// 探测当前平台的采集与编解码后端。
///
/// 按铁律 5，`#[cfg(target_os = "macos")]` 只集中在本探测工厂内，
/// 其余业务逻辑与 JSON/文本格式化均保持平台无关。
fn probe_backends() -> ProbeReport {
    let fake_codec = || CodecProbe {
        codec: "h264".to_string(),
        hardware: false,
        backend: "fake-loopback".to_string(),
        available: true,
    };

    #[cfg(target_os = "macos")]
    {
        // 绝不启动 SCStream、绝不触发系统权限弹窗：只走 preflight API。
        let perms = rc_capture_macos::PermissionStatus::check();
        let reason = (!perms.screen_recording)
            .then(|| "screen recording permission not granted".to_string());
        let capture = CaptureProbe {
            available: perms.screen_recording,
            backend: "sck".to_string(),
            reason,
            permissions: Some(CapturePermissions {
                screen_recording: perms.screen_recording,
                accessibility: perms.accessibility,
                input_monitoring: perms.input_monitoring,
            }),
        };

        let cfg = rc_pipeline::PipelineConfig {
            prefer_hardware: true,
            ..rc_pipeline::PipelineConfig::default()
        };
        let vt_encoder_ok = matches!(
            rc_pipeline::select_encoder_labeled(&cfg),
            Ok((_, "videotoolbox"))
        );
        let vt_decoder_ok = matches!(
            rc_pipeline::select_decoder_labeled(&cfg),
            Ok((_, "videotoolbox"))
        );

        ProbeReport {
            capture,
            encoders: vec![
                CodecProbe {
                    codec: "h264".to_string(),
                    hardware: true,
                    backend: "videotoolbox".to_string(),
                    available: vt_encoder_ok,
                },
                fake_codec(),
            ],
            decoders: vec![
                CodecProbe {
                    codec: "h264".to_string(),
                    hardware: true,
                    backend: "videotoolbox".to_string(),
                    available: vt_decoder_ok,
                },
                fake_codec(),
            ],
            platform: platform(),
            arch: arch(),
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        ProbeReport {
            capture: CaptureProbe {
                available: true,
                backend: "fake-loopback".to_string(),
                reason: None,
                permissions: None,
            },
            encoders: vec![fake_codec()],
            decoders: vec![fake_codec()],
            platform: platform(),
            arch: arch(),
        }
    }
}

/// 当前平台名（与任务卡示例一致的小写字符串）。
fn platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "unknown"
    }
}

/// 当前架构名。
fn arch() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "unknown"
    }
}

/// 最小 JSON 字符串转义，返回带引号的合法 JSON 字符串字面量。
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// 渲染编解码后端 JSON 数组（无尾逗号）。
fn render_codec_array(items: &[CodecProbe]) -> String {
    if items.is_empty() {
        return "[]".to_string();
    }
    let entries: Vec<String> = items
        .iter()
        .map(|item| {
            format!(
                "    {{ \"codec\": {}, \"hardware\": {}, \"backend\": {}, \"available\": {} }}",
                json_string(&item.codec),
                item.hardware,
                json_string(&item.backend),
                item.available,
            )
        })
        .collect();
    format!("[\n{}\n  ]", entries.join(",\n"))
}

/// 机器可读 JSON 输出（手写拼接，不引入 serde）。
fn render_json(report: &ProbeReport) -> String {
    let mut capture_fields = vec![
        format!("\"available\": {}", report.capture.available),
        format!("\"backend\": {}", json_string(&report.capture.backend)),
    ];
    if let Some(reason) = &report.capture.reason {
        capture_fields.push(format!("\"reason\": {}", json_string(reason)));
    }
    if let Some(perms) = report.capture.permissions {
        capture_fields.push(format!(
            "\"permissions\": {{ \"screen_recording\": {}, \"accessibility\": {}, \"input_monitoring\": {} }}",
            perms.screen_recording, perms.accessibility, perms.input_monitoring
        ));
    }
    let capture_json = format!("{{ {} }}", capture_fields.join(", "));

    format!(
        concat!(
            "{{\n",
            "  \"capture\": {capture},\n",
            "  \"encoders\": {encoders},\n",
            "  \"decoders\": {decoders},\n",
            "  \"platform\": {platform},\n",
            "  \"arch\": {arch}\n",
            "}}"
        ),
        capture = capture_json,
        encoders = render_codec_array(&report.encoders),
        decoders = render_codec_array(&report.decoders),
        platform = json_string(report.platform),
        arch = json_string(report.arch),
    )
}

fn yes_no(v: bool) -> &'static str {
    if v {
        "yes"
    } else {
        "no"
    }
}

/// 人类可读输出。
fn render_human(report: &ProbeReport) -> String {
    let mut lines = Vec::new();
    lines.push("RabbitControl 后端探测".to_string());
    lines.push(format!("platform : {} ({})", report.platform, report.arch));

    let mut capture_line = format!(
        "capture  : available={}  backend={}",
        yes_no(report.capture.available),
        report.capture.backend
    );
    if let Some(reason) = &report.capture.reason {
        capture_line.push_str(&format!("  reason={reason}"));
    }
    lines.push(capture_line);

    if let Some(perms) = report.capture.permissions {
        lines.push(format!(
            "  permissions: screen_recording={}  accessibility={}  input_monitoring={}",
            yes_no(perms.screen_recording),
            yes_no(perms.accessibility),
            yes_no(perms.input_monitoring),
        ));
    }

    lines.push("encoders :".to_string());
    for enc in &report.encoders {
        lines.push(format!(
            "  - codec={}  hardware={}  backend={}  available={}",
            enc.codec,
            yes_no(enc.hardware),
            enc.backend,
            yes_no(enc.available),
        ));
    }

    lines.push("decoders :".to_string());
    for dec in &report.decoders {
        lines.push(format!(
            "  - codec={}  hardware={}  backend={}  available={}",
            dec.codec,
            yes_no(dec.hardware),
            dec.backend,
            yes_no(dec.available),
        ));
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 轻量递归下降 JSON 语法校验器（无 serde 依赖，严格检查尾逗号、引号转义与结构闭合）。
    struct JsonValidator<'a> {
        bytes: &'a [u8],
        pos: usize,
    }

    impl<'a> JsonValidator<'a> {
        fn new(input: &'a str) -> Self {
            Self {
                bytes: input.as_bytes(),
                pos: 0,
            }
        }

        fn validate_all(mut self) -> Result<(), String> {
            self.skip_ws();
            self.parse_value()?;
            self.skip_ws();
            if self.pos != self.bytes.len() {
                return Err(format!("trailing bytes at offset {}", self.pos));
            }
            Ok(())
        }

        fn skip_ws(&mut self) {
            while let Some(&b) = self.bytes.get(self.pos) {
                if matches!(b, b' ' | b'\n' | b'\r' | b'\t') {
                    self.pos += 1;
                } else {
                    break;
                }
            }
        }

        fn peek(&self) -> Option<u8> {
            self.bytes.get(self.pos).copied()
        }

        fn expect_byte(&mut self, expected: u8) -> Result<(), String> {
            match self.peek() {
                Some(b) if b == expected => {
                    self.pos += 1;
                    Ok(())
                }
                other => Err(format!(
                    "expected {:?} at offset {}, got {:?}",
                    expected as char, self.pos, other
                )),
            }
        }

        fn parse_value(&mut self) -> Result<(), String> {
            self.skip_ws();
            match self.peek() {
                Some(b'{') => self.parse_object(),
                Some(b'[') => self.parse_array(),
                Some(b'"') => self.parse_string(),
                Some(b't') => self.parse_literal(b"true"),
                Some(b'f') => self.parse_literal(b"false"),
                Some(b'n') => self.parse_literal(b"null"),
                other => Err(format!(
                    "unexpected token {:?} at offset {}",
                    other, self.pos
                )),
            }
        }

        fn parse_literal(&mut self, lit: &[u8]) -> Result<(), String> {
            if self.bytes.get(self.pos..self.pos + lit.len()) == Some(lit) {
                self.pos += lit.len();
                Ok(())
            } else {
                Err(format!("invalid literal at offset {}", self.pos))
            }
        }

        fn parse_string(&mut self) -> Result<(), String> {
            self.expect_byte(b'"')?;
            while let Some(b) = self.peek() {
                self.pos += 1;
                match b {
                    b'"' => return Ok(()),
                    b'\\' => {
                        let esc = self
                            .peek()
                            .ok_or_else(|| "unterminated escape sequence".to_string())?;
                        self.pos += 1;
                        match esc {
                            b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {}
                            b'u' => {
                                for _ in 0..4 {
                                    let h = self
                                        .peek()
                                        .ok_or_else(|| "short \\u escape".to_string())?;
                                    if !h.is_ascii_hexdigit() {
                                        return Err(format!("invalid hex digit at {}", self.pos));
                                    }
                                    self.pos += 1;
                                }
                            }
                            other => return Err(format!("invalid escape \\{}", other as char)),
                        }
                    }
                    b if b < 0x20 => {
                        return Err(format!("unescaped control byte 0x{b:02x} at {}", self.pos));
                    }
                    _ => {}
                }
            }
            Err("unterminated string".to_string())
        }

        fn parse_object(&mut self) -> Result<(), String> {
            self.expect_byte(b'{')?;
            self.skip_ws();
            if self.peek() == Some(b'}') {
                self.pos += 1;
                return Ok(());
            }
            loop {
                self.skip_ws();
                self.parse_string()?;
                self.skip_ws();
                self.expect_byte(b':')?;
                self.parse_value()?;
                self.skip_ws();
                match self.peek() {
                    Some(b',') => {
                        self.pos += 1;
                        self.skip_ws();
                        if self.peek() == Some(b'}') {
                            return Err(format!("trailing comma in object at {}", self.pos));
                        }
                    }
                    Some(b'}') => {
                        self.pos += 1;
                        return Ok(());
                    }
                    other => {
                        return Err(format!(
                            "expected ',' or '}}' in object at {}, got {:?}",
                            self.pos, other
                        ));
                    }
                }
            }
        }

        fn parse_array(&mut self) -> Result<(), String> {
            self.expect_byte(b'[')?;
            self.skip_ws();
            if self.peek() == Some(b']') {
                self.pos += 1;
                return Ok(());
            }
            loop {
                self.parse_value()?;
                self.skip_ws();
                match self.peek() {
                    Some(b',') => {
                        self.pos += 1;
                        self.skip_ws();
                        if self.peek() == Some(b']') {
                            return Err(format!("trailing comma in array at {}", self.pos));
                        }
                    }
                    Some(b']') => {
                        self.pos += 1;
                        return Ok(());
                    }
                    other => {
                        return Err(format!(
                            "expected ',' or ']' in array at {}, got {:?}",
                            self.pos, other
                        ));
                    }
                }
            }
        }
    }

    fn sample_macos_report(screen_recording: bool, vt_enc: bool, vt_dec: bool) -> ProbeReport {
        ProbeReport {
            capture: CaptureProbe {
                available: screen_recording,
                backend: "sck".to_string(),
                reason: (!screen_recording)
                    .then(|| "screen recording permission not granted".to_string()),
                permissions: Some(CapturePermissions {
                    screen_recording,
                    accessibility: true,
                    input_monitoring: false,
                }),
            },
            encoders: vec![
                CodecProbe {
                    codec: "h264".to_string(),
                    hardware: true,
                    backend: "videotoolbox".to_string(),
                    available: vt_enc,
                },
                CodecProbe {
                    codec: "h264".to_string(),
                    hardware: false,
                    backend: "fake-loopback".to_string(),
                    available: true,
                },
            ],
            decoders: vec![
                CodecProbe {
                    codec: "h264".to_string(),
                    hardware: true,
                    backend: "videotoolbox".to_string(),
                    available: vt_dec,
                },
                CodecProbe {
                    codec: "h264".to_string(),
                    hardware: false,
                    backend: "fake-loopback".to_string(),
                    available: true,
                },
            ],
            platform: "macos",
            arch: "aarch64",
        }
    }

    #[test]
    fn render_json_macos_report_is_valid_json_with_expected_fields() {
        let report = sample_macos_report(false, true, true);
        let json = render_json(&report);
        JsonValidator::new(&json)
            .validate_all()
            .unwrap_or_else(|e| panic!("生成的 JSON 非法：{e}\n{json}"));

        assert!(json.contains("\"backend\": \"sck\""));
        assert!(json.contains("\"available\": false"));
        assert!(json.contains("\"reason\": \"screen recording permission not granted\""));
        assert!(json.contains("\"permissions\": { \"screen_recording\": false, \"accessibility\": true, \"input_monitoring\": false }"));
        assert!(json.contains("\"backend\": \"videotoolbox\", \"available\": true"));
        assert!(json.contains("\"backend\": \"fake-loopback\", \"available\": true"));
        assert!(json.contains("\"platform\": \"macos\""));
        assert!(json.contains("\"arch\": \"aarch64\""));
    }

    #[test]
    fn render_json_macos_granted_omits_reason_and_handles_vt_unavailable() {
        let report = sample_macos_report(true, false, false);
        let json = render_json(&report);
        JsonValidator::new(&json)
            .validate_all()
            .unwrap_or_else(|e| panic!("生成的 JSON 非法：{e}\n{json}"));

        assert!(!json.contains("\"reason\""));
        assert!(json.contains("\"available\": true, \"backend\": \"sck\""));
        assert!(json.contains("\"backend\": \"videotoolbox\", \"available\": false"));
        assert!(json.contains("\"backend\": \"fake-loopback\", \"available\": true"));
    }

    #[test]
    fn render_json_fallback_matches_cross_platform_schema() {
        let report = ProbeReport {
            capture: CaptureProbe {
                available: true,
                backend: "fake-loopback".to_string(),
                reason: None,
                permissions: None,
            },
            encoders: vec![CodecProbe {
                codec: "h264".to_string(),
                hardware: false,
                backend: "fake-loopback".to_string(),
                available: true,
            }],
            decoders: vec![CodecProbe {
                codec: "h264".to_string(),
                hardware: false,
                backend: "fake-loopback".to_string(),
                available: true,
            }],
            platform: "linux",
            arch: "x86_64",
        };
        let json = render_json(&report);
        JsonValidator::new(&json)
            .validate_all()
            .unwrap_or_else(|e| panic!("生成的 JSON 非法：{e}\n{json}"));

        assert!(!json.contains("\"permissions\""));
        assert!(!json.contains("\"reason\""));
        assert!(json.contains("\"backend\": \"fake-loopback\""));
        assert!(json.contains("\"platform\": \"linux\""));
        assert!(json.contains("\"arch\": \"x86_64\""));
    }

    #[test]
    fn render_json_escapes_special_characters_safely() {
        let report = ProbeReport {
            capture: CaptureProbe {
                available: false,
                backend: "sck".to_string(),
                reason: Some("quote\"slash\\line\nctrl\x07".to_string()),
                permissions: None,
            },
            encoders: vec![],
            decoders: vec![],
            platform: "macos",
            arch: "aarch64",
        };
        let json = render_json(&report);
        JsonValidator::new(&json)
            .validate_all()
            .unwrap_or_else(|e| panic!("含转义字符的 JSON 非法：{e}\n{json}"));

        assert!(json.contains("quote\\\"slash\\\\line\\nctrl\\u0007"));
        assert!(json.contains("\"encoders\": []"));
        assert!(json.contains("\"decoders\": []"));
    }

    #[test]
    fn render_human_displays_permissions_and_all_backends() {
        let report = sample_macos_report(false, true, false);
        let text = render_human(&report);

        assert!(text.contains("platform : macos (aarch64)"));
        assert!(text.contains(
            "capture  : available=no  backend=sck  reason=screen recording permission not granted"
        ));
        assert!(text
            .contains("permissions: screen_recording=no  accessibility=yes  input_monitoring=no"));
        assert!(text.contains("codec=h264  hardware=yes  backend=videotoolbox  available=yes"));
        assert!(text.contains("codec=h264  hardware=yes  backend=videotoolbox  available=no"));
        assert!(text.contains("codec=h264  hardware=no  backend=fake-loopback  available=yes"));
    }

    #[test]
    fn live_probe_backends_renders_valid_json_and_keeps_fake_fallback() {
        let report = probe_backends();
        let json = render_json(&report);
        JsonValidator::new(&json)
            .validate_all()
            .unwrap_or_else(|e| panic!("真实探测生成的 JSON 非法：{e}\n{json}"));
        assert!(json.contains("\"backend\": \"fake-loopback\", \"available\": true"));
    }
}
