//! 集成测试的公共设施：TLS 身份、连接对、丢包代理。
//!
//! 证书是 2026-11-21 用 openssl 生成的自签 EC P-256 证书（有效期到 2126 年，
//! SAN：`rabbitcontrol` / `localhost` / `127.0.0.1`）。**仅测试用**：私钥
//! 公开在仓库里，不构成任何安全边界；真实会话的证书体系是 M5 的事。

#![allow(dead_code)]

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use rc_net::quic::{Identity, QuicEndpoint, QuicTransport};
use rc_net::{NetConfig, Role};

pub const SERVER_CERT_PEM: &str = include_str!("../certs/server-cert.pem");
pub const SERVER_KEY_PEM: &str = include_str!("../certs/server-key.pem");

/// 握手 / 收帧的宽松上限：本地回环上真实耗时都在毫秒级，
/// 这里只防挂死，不做性能断言。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

pub fn identity() -> Identity {
    Identity::from_pem(SERVER_CERT_PEM, SERVER_KEY_PEM).expect("embedded test identity")
}

pub fn server_certs() -> Vec<rustls::pki_types::CertificateDer<'static>> {
    Identity::certs_from_pem(SERVER_CERT_PEM).expect("embedded test cert")
}

/// 建立一对直连的 (client, host) 传输。
pub fn pair() -> (QuicTransport, QuicTransport) {
    let ident = identity();
    let certs = server_certs();

    let host_ep = QuicEndpoint::new(
        &NetConfig::new(Role::Host, "127.0.0.1:0".parse().unwrap()),
        &ident,
    )
    .expect("host endpoint");
    let host_addr = host_ep.local_addr().expect("host addr");

    let client_ep = QuicEndpoint::new(
        &NetConfig::new(Role::Controller, "127.0.0.1:0".parse().unwrap()),
        &ident,
    )
    .expect("client endpoint");

    let accepter = thread::spawn(move || host_ep.accept(CONNECT_TIMEOUT).expect("host accept"));
    let client = client_ep
        .connect(host_addr, &certs, "rabbitcontrol", CONNECT_TIMEOUT)
        .expect("client connect");
    let host = accepter.join().expect("accepter thread");
    (client, host)
}

/// 可停的丢包代理（进程内、本地回环、无需外网）。
///
/// 语义对齐 rc-loopback 的 `DamageInjector`（T02）：`StdRng::seed_from_u64`
/// 伯努利丢包、`gen::<f32>() < loss` 判丢。因为真实 UDP 包的到达顺序
/// 受调度影响，**逐包的丢法**跨运行不完全一致，但丢包率与恢复概率
/// 的统计性质稳定——这正是断言所依赖的（成功率 > 95%，不是 == 某个值）。
pub struct LossyProxy {
    pub addr: std::net::SocketAddr,
    /// (转发成功, 被丢弃, 收到的总包数)——诊断用
    pub counters: Arc<std::sync::Mutex<(u64, u64, u64)>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl LossyProxy {
    /// 在 `target` 前面放一个双向按 `loss` 概率丢包的 UDP 代理。
    pub fn spawn(target: std::net::SocketAddr, loss: f32, seed: u64) -> Self {
        let sock = UdpSocket::bind("127.0.0.1:0").expect("proxy bind");
        sock.set_read_timeout(Some(Duration::from_millis(100)))
            .expect("set_read_timeout");
        let addr = sock.local_addr().expect("proxy addr");
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let counters = Arc::new(std::sync::Mutex::new((0u64, 0u64, 0u64)));
        let counters2 = counters.clone();

        let handle = thread::Builder::new()
            .name("rc-net-test-proxy".into())
            .spawn(move || {
                let mut rng = StdRng::seed_from_u64(seed);
                let mut buf = [0u8; 65_535];
                let mut client_addr: Option<std::net::SocketAddr> = None;
                loop {
                    if stop2.load(Ordering::Relaxed) {
                        return;
                    }
                    match sock.recv_from(&mut buf) {
                        Ok((n, from)) => {
                            let mut c = counters2.lock().unwrap();
                            c.2 += 1;
                            if from == target {
                                // 服务端 → 客户端
                                if let Some(cl) = client_addr {
                                    if rng.gen::<f32>() >= loss {
                                        c.0 += 1;
                                        drop(c);
                                        let _ = sock.send_to(&buf[..n], cl);
                                    } else {
                                        c.1 += 1;
                                    }
                                }
                            } else {
                                // 客户端 → 服务端（学习客户端地址）
                                client_addr = Some(from);
                                if rng.gen::<f32>() >= loss {
                                    c.0 += 1;
                                    drop(c);
                                    let _ = sock.send_to(&buf[..n], target);
                                } else {
                                    c.1 += 1;
                                }
                            }
                        }
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                || e.kind() == std::io::ErrorKind::TimedOut =>
                        {
                            continue
                        }
                        Err(_) => return,
                    }
                }
            })
            .expect("spawn proxy");

        Self {
            addr,
            counters,
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for LossyProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// 经丢包代理建立的一对传输：QUIC 自己的控制/握手流量同样经历丢包，
/// 检验的是「连接在丢包下存活 + 媒体靠 FEC 维持成功率」。
pub fn pair_with_proxy(loss: f32, seed: u64) -> (QuicTransport, QuicTransport, LossyProxy) {
    let ident = identity();
    let certs = server_certs();

    let host_ep = QuicEndpoint::new(
        &NetConfig::new(Role::Host, "127.0.0.1:0".parse().unwrap()),
        &ident,
    )
    .expect("host endpoint");
    let host_addr = host_ep.local_addr().expect("host addr");

    let proxy = LossyProxy::spawn(host_addr, loss, seed);

    let client_ep = QuicEndpoint::new(
        &NetConfig::new(Role::Controller, "127.0.0.1:0".parse().unwrap()),
        &ident,
    )
    .expect("client endpoint");

    let accepter = thread::spawn(move || host_ep.accept(CONNECT_TIMEOUT).expect("host accept"));
    let client = client_ep
        .connect(proxy.addr, &certs, "rabbitcontrol", CONNECT_TIMEOUT)
        .expect("client connect through lossy proxy");
    let host = accepter.join().expect("accepter thread");
    (client, host, proxy)
}
