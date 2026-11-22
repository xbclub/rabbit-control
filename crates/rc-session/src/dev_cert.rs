//! 内嵌开发证书：localhost 双进程直连的零配置身份（T18）。
//!
//! # 仅开发用（S3）
//!
//! 这对材料与 `crates/rc-net/tests/certs/` 是**同一对**自签证书（EC P-256，
//! 2026-11-21 生成，有效期到 2126 年，SAN：`rabbitcontrol` / `localhost` /
//! `127.0.0.1`）。私钥公开在仓库里，**不构成任何安全边界**；真实会话的
//! 证书体系（受控端身份、指纹校验、用户信任决策）属 M5。
//!
//! # 再生成命令（来源可追溯）
//!
//! ```bash
//! openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
//!     -keyout dev-key.pem -out dev-cert.pem -days 36500 \
//!     -subj "/CN=rabbitcontrol" \
//!     -addext "subjectAltName=DNS:rabbitcontrol,DNS:localhost,IP:127.0.0.1"
//! ```
//!
//! # 依赖纪律
//!
//! rc-session 不依赖 rustls：[`dev_identity`] 返回
//! `rc_net::quic::Identity`（rustls 类型只在 rc-net 边界内出现）；
//! client 侧固定信任直接传 [`DEV_CERT_PEM`] 文本给 `ClientSession::new`
//! （其内部经 `rc_net::quic::Identity::certs_from_pem` 解析）。

use rc_types::Result;

/// 内嵌开发证书的 PEM 文本（`include_str!`，照
/// `crates/rc-net/tests/common/mod.rs` 的模式；文件头带「仅开发用」注释，
/// PEM 解析器忽略非 BEGIN/END 行）。
pub const DEV_CERT_PEM: &str = include_str!("../certs/dev-cert.pem");

/// 内嵌开发私钥的 PEM 文本。
pub const DEV_KEY_PEM: &str = include_str!("../certs/dev-key.pem");

/// 证书 SAN 内的服务名（与 `rc_net::quic::ALPN` 一样是两端约定）。
/// client 默认用它做 `QuicEndpoint::connect` 的 `server_name`。
pub const DEV_SERVER_NAME: &str = "rabbitcontrol";

/// 默认身份（host 与 client 的 QuicEndpoint 都要一份 Identity——
/// `QuicEndpoint::new` 无论角色都构建服务端配置）。
pub fn dev_identity() -> Result<rc_net::quic::Identity> {
    rc_net::quic::Identity::from_pem(DEV_CERT_PEM, DEV_KEY_PEM)
}

/// client 固定信任的服务端证书（PEM 文本，即 [`DEV_CERT_PEM`]）。
///
/// 用法：直接传 `ClientSession::new(cfg, cert, key, server_cert_pem,
/// server_name, clock)`，由会话层内部解析——本 crate 的公开 API 不出现
/// rustls 类型（返回 `Vec<CertificateDer>` 会让 rc-session 直接依赖
/// rustls；若未来确需直接持有证书链对象，先由 rc-net re-export）。
pub fn dev_server_cert_pem() -> &'static str {
    DEV_CERT_PEM
}

#[cfg(test)]
mod tests {
    use super::*;

    /// S3：内嵌材料可直接构造 Identity（文件头注释不破坏 PEM 解析），
    /// client 固定信任证书可解析出证书链。
    #[test]
    fn dev_cert_material_is_parseable() {
        dev_identity().expect("内嵌开发证书 + 私钥必须能构造 Identity");
        let certs = rc_net::quic::Identity::certs_from_pem(dev_server_cert_pem())
            .expect("内嵌开发证书必须能解析出证书链");
        assert!(!certs.is_empty(), "证书链不能为空");
    }
}
