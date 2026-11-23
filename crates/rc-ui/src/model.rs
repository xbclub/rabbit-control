//! 纯逻辑校验（设计文档 §7.1 / T21 API 契约 §4）。
//!
//! 全部纯函数，不依赖 Tauri 或窗口系统，可离线确定性单测。

#![deny(unsafe_code)]

use std::net::{IpAddr, Ipv6Addr, SocketAddr};

/// 连接目标：支持直连 `ip:port` 或 9 位数字设备码（经由信令/中继服务器）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectTarget {
    /// 局域网 / 公网 `ip:port` 直连（QUIC）。
    Direct(SocketAddr),
    /// 9 位纯数字设备码（如 `"604928235"`）。
    DeviceId(String),
}

/// 解析连接输入框内容：同时接受 9 位设备码（如 `"604 928 235"` / `"604928235"`）或 `ip:port` 直连地址。
pub fn parse_connect_input(s: &str) -> Result<ConnectTarget, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("连接目标不能为空".to_string());
    }
    if let Some(device_id) = crate::server_config::parse_device_id_input(trimmed) {
        return Ok(ConnectTarget::DeviceId(device_id));
    }
    match parse_connect_target(trimmed) {
        Ok(addr) => Ok(ConnectTarget::Direct(addr)),
        Err(err) if !err.contains("暂仅支持 IP:端口直连") => Err(err),
        Err(_) => {
            Err("请输入 9 位设备码（如 604 928 235）或 IP:端口（如 192.168.1.20:9901）".to_string())
        }
    }
}

/// 解析连接目标。
///
/// M2 直连只收 `ip:port`（对齐 T18 `--connect` 的 `SocketAddr` 口径）；
/// 域名或设备 ID 体系属 M4/M5 信令，返回 `Err` 提示「暂仅支持 IP:端口直连」。
/// 端口范围 `1..=65535`，拒绝 `0`、`>65535`、非数字、空串、缺少端口、多余字段。
pub fn parse_connect_target(s: &str) -> Result<SocketAddr, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("连接目标不能为空".to_string());
    }
    if trimmed.split_whitespace().count() > 1 {
        return Err("连接目标包含多余字段，暂仅支持 IP:端口直连".to_string());
    }

    let (host, port_str) = if let Some(rest) = trimmed.strip_prefix('[') {
        // IPv6 方括号格式：[::1]:9901（方括号内必须是合法 IPv6 地址，拒绝 [127.0.0.1]:9901）
        let Some((ipv6_host, after_bracket)) = rest.split_once(']') else {
            return Err("IPv6 地址格式无效，缺少右括号".to_string());
        };
        if ipv6_host.parse::<Ipv6Addr>().is_err() {
            return Err("方括号内必须是合法的 IPv6 地址".to_string());
        }
        let Some(port_part) = after_bracket.strip_prefix(':') else {
            return Err("缺少端口号，请填写 IP:端口".to_string());
        };
        (ipv6_host, port_part)
    } else if let Some((h, p)) = trimmed.rsplit_once(':') {
        // 非方括号形式下若主机段仍含冒号，属于多余字段或未加方括号的 IPv6
        if h.contains(':') {
            return Err("连接目标格式无效（多余字段或 IPv6 未加方括号）".to_string());
        }
        (h, p)
    } else {
        // 无冒号：若本身是合法 IP 则提示缺端口，否则视为域名 / 设备 ID
        if trimmed.parse::<IpAddr>().is_ok() {
            return Err("缺少端口号，请填写 IP:端口".to_string());
        }
        return Err("暂仅支持 IP:端口直连（如 127.0.0.1:9901）".to_string());
    };

    if host.is_empty() {
        return Err("缺少主机地址".to_string());
    }

    let ip: IpAddr = host
        .parse()
        .map_err(|_| "暂仅支持 IP:端口直连（如 127.0.0.1:9901）".to_string())?;
    let port = validate_port(port_str)?;
    Ok(SocketAddr::new(ip, port))
}

/// 校验端口号字符串：必须是 `1..=65535` 范围内的纯数字整数。
pub fn validate_port(s: &str) -> Result<u16, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("端口不能为空".to_string());
    }
    if !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return Err("端口必须为 1–65535 的整数".to_string());
    }
    let port: u16 = trimmed
        .parse()
        .map_err(|_| "端口范围必须为 1–65535".to_string())?;
    if port == 0 {
        return Err("端口不能为 0，范围必须为 1–65535".to_string());
    }
    Ok(port)
}

/// 校验 W4 端口映射名称：非空且不超过 64 个字符。
pub fn validate_map_name(s: &str) -> Result<(), String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("映射名称不能为空".to_string());
    }
    if trimmed.chars().count() > 64 {
        return Err("映射名称不能超过 64 个字符".to_string());
    }
    Ok(())
}

/// 校验 W4 端口映射目标地址：非空且不能包含空白或控制字符（trim 后与后端规则一致）。
pub fn validate_target_addr(s: &str) -> Result<(), String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("目标地址不能为空".to_string());
    }
    if trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("目标地址不能包含空白或控制字符".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn parse_connect_target_accepts_ip_port() {
        assert_eq!(
            parse_connect_target("127.0.0.1:9901").unwrap(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9901)
        );
        assert_eq!(
            parse_connect_target("  192.168.1.20:65535  ").unwrap(),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)), 65535)
        );
        assert_eq!(
            parse_connect_target("[::1]:9901").unwrap(),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 9901)
        );

        // 拒绝端口 0 / >65535 / 非数字 / 空端口
        assert!(parse_connect_target("127.0.0.1:0").is_err());
        assert!(parse_connect_target("127.0.0.1:70000").is_err());
        assert!(parse_connect_target("127.0.0.1:abc").is_err());
        assert!(parse_connect_target("127.0.0.1:").is_err());

        // 拒绝空串 / 无端口 / 多余字段 / 方括号内非 IPv6
        assert!(parse_connect_target("").is_err());
        assert!(parse_connect_target("   ").is_err());
        assert!(parse_connect_target("127.0.0.1").is_err());
        assert!(parse_connect_target("127.0.0.1:9901 extra").is_err());
        assert!(parse_connect_target("127.0.0.1:9901:123").is_err());
        assert!(parse_connect_target("[127.0.0.1]:9901").is_err());
        assert!(parse_connect_target("[not-ipv6]:9901").is_err());

        // 域名 / 设备 ID 必须返回明确区分文案
        for bad in ["example.com:9901", "example.com", "RC-8492-3310"] {
            let err = parse_connect_target(bad).unwrap_err();
            assert!(
                err.contains("暂仅支持 IP:端口直连"),
                "输入 {bad:?} 错误文案应包含「暂仅支持 IP:端口直连」，实际：{err:?}"
            );
        }
    }

    #[test]
    fn validate_map_form_rejects_bad_ports() {
        // 端口校验
        assert_eq!(validate_port("1"), Ok(1));
        assert_eq!(validate_port("8080"), Ok(8080));
        assert_eq!(validate_port("65535"), Ok(65535));
        for bad in [
            "0", "70000", "999999", "abc", "-1", "+80", "", "   ", "80.5",
        ] {
            assert!(validate_port(bad).is_err(), "非法端口 {bad:?} 应被拒绝");
        }

        // 名称校验：非空且 ≤64 字符
        assert!(validate_map_name("").is_err());
        assert!(validate_map_name("   ").is_err());
        let name_64 = "a".repeat(64);
        let name_65 = "a".repeat(65);
        assert!(validate_map_name(&name_64).is_ok());
        assert!(validate_map_name(&name_65).is_err());
        let cjk_64 = "映".repeat(64);
        let cjk_65 = "映".repeat(65);
        assert!(validate_map_name(&cjk_64).is_ok());
        assert!(validate_map_name(&cjk_65).is_err());

        // 目标地址校验
        assert!(validate_target_addr("").is_err());
        assert!(validate_target_addr("   ").is_err());
        assert!(validate_target_addr("127.0.0.1").is_ok());
        assert!(validate_target_addr("  127.0.0.1  ").is_ok());
        assert!(validate_target_addr("127.0.0.1 bad").is_err());
        assert!(validate_target_addr("127.0.0.1\n").is_ok());
        assert!(validate_target_addr("127.0.0.\x001").is_err());
    }

    #[test]
    fn parse_connect_input_accepts_device_id_and_direct_addr() {
        assert_eq!(
            parse_connect_input("604928235").unwrap(),
            ConnectTarget::DeviceId("604928235".to_string())
        );
        assert_eq!(
            parse_connect_input("  604 928 235  ").unwrap(),
            ConnectTarget::DeviceId("604928235".to_string())
        );
        assert_eq!(
            parse_connect_input("604-928-235").unwrap(),
            ConnectTarget::DeviceId("604928235".to_string())
        );
        assert_eq!(
            parse_connect_input("604\u{00A0}928\t235").unwrap(),
            ConnectTarget::DeviceId("604928235".to_string())
        );
        assert_eq!(
            parse_connect_input("604\u{3000}928\u{3000}235").unwrap(),
            ConnectTarget::DeviceId("604928235".to_string())
        );
        assert_eq!(
            parse_connect_input("192.168.1.20:9901").unwrap(),
            ConnectTarget::Direct(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)),
                9901
            ))
        );
        assert_eq!(
            parse_connect_input("[::1]:9901").unwrap(),
            ConnectTarget::Direct(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 9901))
        );

        for bad in [
            "",
            "   ",
            "000000000",
            "000 000 000",
            "012345678",
            "012 345 678",
            "12345678",
            "1234567890",
            "127.0.0.1",
            "127.0.0.1:0",
            "127.0.0.1:70000",
            "[127.0.0.1]:9901",
            "example.com:9901",
            "RC-8492-3310",
        ] {
            assert!(
                parse_connect_input(bad).is_err(),
                "expected error for {bad:?}"
            );
        }
    }
}
