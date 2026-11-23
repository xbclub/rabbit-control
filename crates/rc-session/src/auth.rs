//! M2-5 鉴权：手写 SHA-256 / HMAC-SHA256 / PBKDF2-HMAC-SHA256、密码挑战
//! 应答纯状态机、与 M2-7 接受弹窗的轮询联动位。
//!
//! # 为什么手写哈希而不是引 ring/rustls/sha2
//!
//! 本项目纪律「纯 Rust 不引重型依赖」，为单个哈希函数拉进 TLS 栈不值得；
//! SHA-256 核心约百行，以 FIPS 180-4 / RFC 4231 / 公开 PBKDF2 向量逐字节
//! 钉死正确性。
//!
//! # 安全边界（文档化取舍）
//!
//! - 密码以 `String` 常驻内存、不做内存擦除（zeroize）：零依赖优先的取舍，
//!   M4 服务化引入 Keychain 时一并处理。
//! - 信道加密与 host 身份由 QUIC TLS 1.3 + 固定信任证书负责；本模块不做
//!   PAKE/SRP，也不做失败限频/锁定（属 M4 服务化）。
//! - 盐生成是本模块唯一的随机性来源（OS 熵 16 字节）；核心 proof 函数纯
//!   确定性，便于离线测试。

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rand::rngs::OsRng;
use rand::RngCore;
use rc_types::Result;

use crate::proto::{
    AuthChallenge, AuthMethod, AuthResponse, AuthStatus, Hello, SessionParams, Welcome,
    AUTH_PROOF_LEN, AUTH_SALT_LEN, MAX_AUTH_ITERATIONS, MAX_OWNER_SIGNATURE_LEN,
    MAX_OWNER_TICKET_PAYLOAD_LEN,
};

/// 默认 PBKDF2 迭代次数（写入 AuthChallenge）。release 单次计算毫秒级，
/// 握手一次性成本；防离线字典攻击的最低强度线。
pub const DEFAULT_AUTH_ITERATIONS: u32 = 10_000;

/// 默认鉴权/审批超时（微秒）：host 侧人工确认的默认等待窗口（30s）。
pub const DEFAULT_AUTH_TIMEOUT_US: u64 = 30_000_000;

/// client 侧默认鉴权全流程超时（微秒）：`DEFAULT_AUTH_TIMEOUT_US + 15s`（45s，
/// T22 U6），确保 host 30s 审批弹窗等待期间 client 不会先于 host 超时。
pub const DEFAULT_CLIENT_AUTH_TIMEOUT_US: u64 = DEFAULT_AUTH_TIMEOUT_US + 15_000_000;

// ─────────────────────────────────────────────────────────────────
// 纯 Rust SHA-256（FIPS 180-4）/ HMAC-SHA256（RFC 4231）/
// PBKDF2-HMAC-SHA256（RFC 2898 / RFC 8018）
// ─────────────────────────────────────────────────────────────────

/// SHA-256 输出字节数。
const SHA256_LEN: usize = 32;
/// SHA-256 分组字节数。
const SHA256_BLOCK_LEN: usize = 64;

/// FIPS 180-4 §5.3.3 初始哈希值。
const SHA256_H0: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

/// FIPS 180-4 §4.2.2 64 轮常量。
const SHA256_K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// 流式 SHA-256 状态（零堆分配，便于 HMAC 预吸收 ipad/opad 后克隆复用）。
#[derive(Clone)]
struct Sha256 {
    state: [u32; 8],
    buf: [u8; SHA256_BLOCK_LEN],
    buf_len: usize,
    total_len: u64,
}

impl Sha256 {
    fn new() -> Self {
        Self {
            state: SHA256_H0,
            buf: [0u8; SHA256_BLOCK_LEN],
            buf_len: 0,
            total_len: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u64);

        if self.buf_len > 0 {
            let take = (SHA256_BLOCK_LEN - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == SHA256_BLOCK_LEN {
                let block = self.buf;
                self.compress(&block);
                self.buf_len = 0;
            }
        }

        while data.len() >= SHA256_BLOCK_LEN {
            let mut block = [0u8; SHA256_BLOCK_LEN];
            block.copy_from_slice(&data[..SHA256_BLOCK_LEN]);
            self.compress(&block);
            data = &data[SHA256_BLOCK_LEN..];
        }

        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    fn finalize(mut self) -> [u8; SHA256_LEN] {
        let bit_len = self.total_len.wrapping_mul(8);
        self.buf[self.buf_len] = 0x80;
        self.buf_len += 1;

        if self.buf_len > SHA256_BLOCK_LEN - 8 {
            for b in &mut self.buf[self.buf_len..] {
                *b = 0;
            }
            let block = self.buf;
            self.compress(&block);
            self.buf = [0u8; SHA256_BLOCK_LEN];
            self.buf_len = 0;
        } else {
            for b in &mut self.buf[self.buf_len..SHA256_BLOCK_LEN - 8] {
                *b = 0;
            }
        }

        self.buf[SHA256_BLOCK_LEN - 8..].copy_from_slice(&bit_len.to_be_bytes());
        let block = self.buf;
        self.compress(&block);

        let mut out = [0u8; SHA256_LEN];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; SHA256_BLOCK_LEN]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
        self.state[5] = self.state[5].wrapping_add(f);
        self.state[6] = self.state[6].wrapping_add(g);
        self.state[7] = self.state[7].wrapping_add(h);
    }
}

/// 一次性计算 SHA-256。
fn sha256(data: &[u8]) -> [u8; SHA256_LEN] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize()
}

/// 预吸收了 ipad/opad 的 HMAC-SHA256，供 PBKDF2 迭代复用。
struct HmacSha256 {
    inner_init: Sha256,
    outer_init: Sha256,
}

impl HmacSha256 {
    fn new(key: &[u8]) -> Self {
        let mut key_block = [0u8; SHA256_BLOCK_LEN];
        if key.len() > SHA256_BLOCK_LEN {
            let digest = sha256(key);
            key_block[..SHA256_LEN].copy_from_slice(&digest);
        } else {
            key_block[..key.len()].copy_from_slice(key);
        }

        let mut ipad = [0x36u8; SHA256_BLOCK_LEN];
        let mut opad = [0x5cu8; SHA256_BLOCK_LEN];
        for i in 0..SHA256_BLOCK_LEN {
            ipad[i] ^= key_block[i];
            opad[i] ^= key_block[i];
        }

        let mut inner_init = Sha256::new();
        inner_init.update(&ipad);
        let mut outer_init = Sha256::new();
        outer_init.update(&opad);

        Self {
            inner_init,
            outer_init,
        }
    }

    fn mac(&self, data: &[u8]) -> [u8; SHA256_LEN] {
        let mut inner = self.inner_init.clone();
        inner.update(data);
        let inner_hash = inner.finalize();

        let mut outer = self.outer_init.clone();
        outer.update(&inner_hash);
        outer.finalize()
    }

    fn mac_two(&self, part1: &[u8], part2: &[u8]) -> [u8; SHA256_LEN] {
        let mut inner = self.inner_init.clone();
        inner.update(part1);
        inner.update(part2);
        let inner_hash = inner.finalize();

        let mut outer = self.outer_init.clone();
        outer.update(&inner_hash);
        outer.finalize()
    }
}

/// PBKDF2-HMAC-SHA256（固定输出 32 字节 = 单块 `INT_32_BE(1)`）。
fn pbkdf2_hmac_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; AUTH_PROOF_LEN] {
    if iterations == 0 {
        return [0u8; AUTH_PROOF_LEN];
    }
    let hmac = HmacSha256::new(password);
    let mut u = hmac.mac_two(salt, &1u32.to_be_bytes());
    let mut out = u;
    for _ in 1..iterations {
        u = hmac.mac(&u);
        for (o, &ui) in out.iter_mut().zip(u.iter()) {
            *o ^= ui;
        }
    }
    out
}

/// 常数时间定长字节数组比较（无短路，防时序侧信道，A10）。
fn ct_eq(a: &[u8; AUTH_PROOF_LEN], b: &[u8; AUTH_PROOF_LEN]) -> bool {
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// client 侧证明计算（纯函数、确定性——测试可固定盐逐字节断言）。
pub fn compute_proof(
    password: &[u8],
    salt: &[u8; AUTH_SALT_LEN],
    iterations: u32,
) -> [u8; AUTH_PROOF_LEN] {
    pbkdf2_hmac_sha256(password, salt, iterations)
}

/// host 侧校验：同参重算 + **常数时间比较**（不泄比较结果的时间信息）。
pub fn verify_proof(
    password: &[u8],
    salt: &[u8; AUTH_SALT_LEN],
    iterations: u32,
    proof: &[u8; AUTH_PROOF_LEN],
) -> bool {
    if iterations == 0 {
        return false;
    }
    let expected = compute_proof(password, salt, iterations);
    ct_eq(&expected, proof)
}

// ─────────────────────────────────────────────────────────────────
// 自控免密签名票据（OwnerTicket PoP）抽象（密码学签验经 trait 注入）
// ─────────────────────────────────────────────────────────────────

/// 已消费 `ticket_id` 防重放表容量上限（FIFO 有界淘汰 + O(1) 判重）。
const OWNER_REPLAY_CACHE_CAP: usize = 256;

/// 默认生产 Unix 秒时钟（仅在 `HostAuthConfig::unix_time_fn` 为 `None` 时使用）。
fn default_unix_time_sec() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 经 host 验签通过的自控免密票据元数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedOwnerTicket {
    /// 票据唯一 ID（防重放键）。
    pub ticket_id: String,
    /// 签发用户 ID。
    pub user_id: String,
    /// 主控端设备 ID。
    pub caller_device_id: String,
    /// 主控端设备公钥十六进制表示。
    pub caller_pubkey_hex: String,
    /// 被控端目标设备 ID。
    pub target_device_id: String,
    /// 关联会话 ID 字符串。
    pub session_id_str: String,
    /// 票据过期时间戳（Unix 秒）。
    pub expires_at_unix_sec: u64,
}

/// client 侧自控免密票据签名器（密码学操作通过 trait 注入）。
pub trait OwnerTicketSigner: Send + Sync {
    /// 返回序列化后的 `owner_grant` 票据载荷（放入 `Hello.auth_payload`）。
    fn ticket_payload(&self) -> Vec<u8>;
    /// 使用主控端私钥对挑战 `(hello_session_id, salt)` 签名（1..=128 字节）。
    fn sign_challenge(&self, hello_session_id: u64, salt: &[u8; AUTH_SALT_LEN]) -> Result<Vec<u8>>;
}

/// host 侧自控免密票据校验器（密码学操作通过 trait 注入）。
pub trait OwnerTicketVerifier: Send + Sync {
    /// 校验票据签名、受众与过期时间（`now_unix_sec`），成功返回 [`VerifiedOwnerTicket`]。
    fn verify_ticket(&self, payload: &[u8], now_unix_sec: u64) -> Result<VerifiedOwnerTicket>;
    /// 校验 client 侧 `OwnerAuthResponse` 对挑战 `(hello_session_id, salt)` 的 PoP 签名。
    fn verify_challenge_signature(
        &self,
        ticket: &VerifiedOwnerTicket,
        hello_session_id: u64,
        salt: &[u8; AUTH_SALT_LEN],
        signature: &[u8],
    ) -> Result<()>;
}

/// 有界已消费 `ticket_id` 防重放表（`VecDeque` 维持插入序 + `HashSet` O(1) 判重）。
#[derive(Debug, Default)]
pub struct OwnerReplayCache {
    order: VecDeque<String>,
    seen: HashSet<String>,
}

impl OwnerReplayCache {
    /// 构造空的自控票据防重放缓存（可跨 `HostSession` 实例共享）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 若 `ticket_id` 尚未消费则写入并返回 `true`；已消费过则返回 `false`。
    pub fn insert_if_new(&mut self, ticket_id: &str) -> bool {
        if self.seen.contains(ticket_id) {
            return false;
        }
        if self.order.len() >= OWNER_REPLAY_CACHE_CAP {
            if let Some(oldest) = self.order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
        let owned = ticket_id.to_owned();
        self.seen.insert(owned.clone());
        self.order.push_back(owned);
        true
    }
}

// ─────────────────────────────────────────────────────────────────
// 配置与审批抽象（全项目唯一审批 trait，T22 直接消费）
// ─────────────────────────────────────────────────────────────────

/// host 侧鉴权配置。
#[derive(Clone)]
pub struct HostAuthConfig {
    /// 共享密码。`None` = 不启用密码鉴权（此时 `Hello.auth_method == Password` 一律拒绝）。
    pub password: Option<String>,
    /// 挑战迭代次数（进 `AuthChallenge`；测试可用小值如 64 加速状态机用例）。
    pub iterations: u32,
    /// 人工确认超时（微秒，会话钟）；超时按拒绝处理。
    pub approval_timeout_us: u64,
    /// 是否允许远程协助（`AuthMethod::None` / `AuthMethod::Password`）。
    /// 默认 `true` 以保持现有 CLI/单测行为不变；UI 层应按开关传入实际值。
    pub allow_remote_assist: bool,
    /// 自控免密票据校验器（`Some` 时接受 `AuthMethod::OwnerTicket`）。
    pub owner_verifier: Option<Arc<dyn OwnerTicketVerifier>>,
    /// 跨多轮 `HostSession` 实例共享的已消费 `ticket_id` 防重放缓存（`None` 时每实例独立构造）。
    pub replay_cache: Option<Arc<Mutex<OwnerReplayCache>>>,
    /// 可注入的当前 Unix 秒时钟（`None` 时默认读取系统 Unix 秒）。
    pub unix_time_fn: Option<Arc<dyn Fn() -> u64 + Send + Sync>>,
}

impl fmt::Debug for HostAuthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostAuthConfig")
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("iterations", &self.iterations)
            .field("approval_timeout_us", &self.approval_timeout_us)
            .field("allow_remote_assist", &self.allow_remote_assist)
            .field("owner_verifier", &self.owner_verifier.is_some())
            .field("replay_cache", &self.replay_cache.is_some())
            .field("unix_time_fn", &self.unix_time_fn.is_some())
            .finish()
    }
}

impl Default for HostAuthConfig {
    fn default() -> Self {
        Self {
            password: None,
            iterations: DEFAULT_AUTH_ITERATIONS,
            approval_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
            allow_remote_assist: true,
            owner_verifier: None,
            replay_cache: None,
            unix_time_fn: None,
        }
    }
}

/// client 侧鉴权配置。
#[derive(Clone)]
pub struct ClientAuthConfig {
    /// 共享密码。`None` = 以 `AuthMethod::None` 握手（T14 原行为，不变）。
    pub password: Option<String>,
    /// 鉴权全流程超时（微秒，会话钟）。
    pub auth_timeout_us: u64,
    /// 自控免密票据签名器（`Some` 时优先以 `AuthMethod::OwnerTicket` 握手）。
    pub owner_signer: Option<Arc<dyn OwnerTicketSigner>>,
}

impl fmt::Debug for ClientAuthConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientAuthConfig")
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("auth_timeout_us", &self.auth_timeout_us)
            .field("owner_signer", &self.owner_signer.is_some())
            .finish()
    }
}

impl Default for ClientAuthConfig {
    fn default() -> Self {
        Self {
            password: None,
            auth_timeout_us: DEFAULT_CLIENT_AUTH_TIMEOUT_US,
            owner_signer: None,
        }
    }
}

/// 一次连接审批请求：审批 UI 需要展示的全部信息（T22 弹窗文案的唯一
/// 信息源）。由 [`HostAuthenticator`] 在首次进入等待确认时组装，经
/// [`ConnectionApprover::submit`] 递给实现方。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    /// 回显 `Hello.session_id`。
    pub session_id: u64,
    /// 对端传输层地址（`QuicEndpoint` accept 得到，非 client 自报；经
    /// `on_hello` 的 `remote_addr` 参数进入状态机）。
    pub remote_addr: SocketAddr,
    /// Hello 请求参数（弹窗展示「请求 1920x1080@60」）。
    pub requested: SessionParams,
    /// Hello 声明的鉴权方式。
    pub auth_method: AuthMethod,
    /// 密码校验是否已通过（未启用密码 = `false`）。先鉴权后审批：密码
    /// 失败根本不会走到审批，能到达 submit 的只有「已通过」或「无密码
    /// 要求」两种（T22 弹窗据此显示「密码已验证」/「未启用密码」）。
    pub password_verified: bool,
}

/// 审批裁决（M2-7 接受弹窗的应答值）。超时**不**在此枚举——无人应答
/// 的超时由 [`HostAuthenticator::poll_approval`] 按注入时钟判成
/// `Rejected`，实现方不感知超时。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalVerdict {
    /// 接受本次连接。
    Accepted,
    /// 拒绝（含弹窗点「拒绝」）。
    Rejected,
}

/// 连接审批人——M2-7 接受弹窗的**联动位**（全项目唯一审批抽象，
/// T22 实装托盘弹窗版，本卡交付 trait 与内置替身）。
///
/// 为什么三方法全部非阻塞而不是回调/阻塞：quinn 的 accept/connect
/// 本就是阻塞式（`quic.rs:253/303`），会话驱动循环靠短超时 poll 推进——
/// 审批人若在循环里阻塞等人，心跳、采集、编码全部停摆。实现方（T22
/// 的托盘弹窗、测试替身）在独立线程更新共享状态，会话循环每拍来问
/// 一次；超时判定收敛在会话层（[`HostAuthenticator::poll_approval`] 的
/// 注入 `now_us`），trait 方法不携带时间（铁律 4）。
pub trait ConnectionApprover: Send {
    /// 提交一次审批请求（弹窗出现）。同一时刻至多一个在途请求；
    /// 在途期间再次 submit 属实现方 bug，实现可 panic 于 debug、
    /// 覆盖于 release。
    fn submit(&mut self, req: &ApprovalRequest);
    /// 非阻塞取裁决：`None` = 仍在等待（弹窗未答 / 无在途请求）；
    /// `Some` = 终局。
    fn poll_verdict(&mut self) -> Option<ApprovalVerdict>;
    /// 撤销在途审批（client 在等待期断开 / 会话被终止）；弹窗应随之
    /// 关闭。无在途请求时为幂等 no-op。
    fn cancel(&mut self);
}

/// 即时接受的审批人：无头 CLI 默认 / 测试基线（submit 即 Accepted，
/// cancel no-op）。
#[derive(Debug, Default, Clone, Copy)]
pub struct AutoApprover;

impl ConnectionApprover for AutoApprover {
    fn submit(&mut self, _req: &ApprovalRequest) {}

    fn poll_verdict(&mut self) -> Option<ApprovalVerdict> {
        Some(ApprovalVerdict::Accepted)
    }

    fn cancel(&mut self) {}
}

/// 一律拒绝的审批人：远程运维锁主机 / CI 验证拒绝路径（T22 的
/// `--approval deny` 消费）。
#[derive(Debug, Default, Clone, Copy)]
pub struct DenyApprover;

impl ConnectionApprover for DenyApprover {
    fn submit(&mut self, _req: &ApprovalRequest) {}

    fn poll_verdict(&mut self) -> Option<ApprovalVerdict> {
        Some(ApprovalVerdict::Rejected)
    }

    fn cancel(&mut self) {}
}

/// 脚本化审批人：按给定序列回放（`None` = 等待，`Some` = 终局）；序列
/// 耗尽后恒返回末项，测试不会悬死。T22 落地弹窗时的行为参考样板。
#[derive(Debug, Clone)]
pub struct ScriptedApprover {
    responses: Vec<Option<ApprovalVerdict>>,
    cursor: usize,
    in_flight: bool,
}

impl ScriptedApprover {
    /// 以给定的非阻塞裁决序列构造脚本化审批人。
    pub fn new(responses: Vec<Option<ApprovalVerdict>>) -> Self {
        Self {
            responses,
            cursor: 0,
            in_flight: false,
        }
    }
}

impl ConnectionApprover for ScriptedApprover {
    fn submit(&mut self, _req: &ApprovalRequest) {
        debug_assert!(
            !self.in_flight,
            "ScriptedApprover::submit called while another request is already in flight"
        );
        self.in_flight = true;
    }

    fn poll_verdict(&mut self) -> Option<ApprovalVerdict> {
        if !self.in_flight || self.responses.is_empty() {
            return None;
        }
        let v = self.responses[self.cursor];
        if self.cursor + 1 < self.responses.len() {
            self.cursor += 1;
        }
        if v.is_some() {
            self.in_flight = false;
        }
        v
    }

    fn cancel(&mut self) {
        self.in_flight = false;
    }
}

// ─────────────────────────────────────────────────────────────────
// HostAuthenticator（host 侧纯状态机）
// ─────────────────────────────────────────────────────────────────

/// host 侧动作（由 `HostSession` 翻译成线上消息与断开）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostAuthAction {
    /// 发 `Welcome{auth_status=ChallengeRequired}`（过程消息：granted 填
    /// 预裁定真值但 client 必须忽略）。仅在「确认器首次返回等待」且本次
    /// 握手尚未发过过程消息时出现一次——密码路径 Challenge 已自带过程
    /// 消息，不会重复出现。
    NotifyPending,
    /// 依次发 `Welcome{ChallengeRequired}` + `AuthChallenge`（顺序固定）。
    Challenge(AuthChallenge),
    /// 终局：发 `Welcome{auth_status}`；`Rejected` 在发送后**立即断开**传输层。
    Conclude(AuthStatus),
    /// 本拍无事（等待 `AuthResponse` / 人工确认）。
    Wait,
}

enum HostState {
    /// 尚未收到 Hello。
    Init,
    /// 已发密码 Challenge，等待 AuthResponse。
    AwaitingResponse {
        salt: [u8; AUTH_SALT_LEN],
        iterations: u32,
        pending_request: ApprovalRequest,
        /// 首次轮询时锚定的挑战计时起点（微秒，会话钟），防未认证对端永久占用连接（H2）。
        started_at_us: Option<u64>,
    },
    /// 已发自控挑战，等待 OwnerAuthResponse。
    AwaitingOwnerResponse {
        session_id: u64,
        salt: [u8; AUTH_SALT_LEN],
        ticket: VerifiedOwnerTicket,
        /// 首次轮询时锚定的挑战计时起点（微秒，会话钟）。
        started_at_us: Option<u64>,
    },
    /// 等待人工确认（无密码路径行 1，或密码校验通过后的行 5）。
    PendingApproval {
        request: ApprovalRequest,
        submitted: bool,
        started_at_us: Option<u64>,
        process_notified: bool,
    },
    /// 鉴权已终局。
    Concluded,
}

/// host 侧鉴权纯状态机（transport 无关；`HostSession` 消费其动作，
/// 不自己发明任何鉴权分支）。
pub struct HostAuthenticator {
    cfg: HostAuthConfig,
    state: HostState,
    authenticated_owner: Option<VerifiedOwnerTicket>,
    replay_cache: Arc<Mutex<OwnerReplayCache>>,
}

impl HostAuthenticator {
    /// 按配置构造 host 侧鉴权状态机。
    pub fn new(cfg: HostAuthConfig) -> Self {
        Self::with_replay_cache(cfg, Arc::new(Mutex::new(OwnerReplayCache::default())))
    }

    /// 以共享的防重放缓存构造（供 `HostSession` 跨候选/重连持久化已消费 `ticket_id`）。
    pub(crate) fn with_replay_cache(
        cfg: HostAuthConfig,
        replay_cache: Arc<Mutex<OwnerReplayCache>>,
    ) -> Self {
        Self {
            cfg,
            state: HostState::Init,
            authenticated_owner: None,
            replay_cache,
        }
    }

    /// 收到 Hello 的裁定（使用配置注入的 `unix_time_fn` 或默认系统 Unix 秒）。
    /// `remote_addr` 为 accept 得到的对端传输层地址（进入 [`ApprovalRequest`]
    /// 的唯一途径——弹窗不得展示 client 自报地址）。
    pub fn on_hello(&mut self, hello: &Hello, remote_addr: SocketAddr) -> HostAuthAction {
        let now_unix_sec = self
            .cfg
            .unix_time_fn
            .as_ref()
            .map(|f| f())
            .unwrap_or_else(default_unix_time_sec);
        self.on_hello_at(hello, remote_addr, now_unix_sec)
    }

    /// 收到 Hello 的确定性裁定（显式注入 `now_unix_sec`，供离线单测与状态机直测使用）。
    pub fn on_hello_at(
        &mut self,
        hello: &Hello,
        remote_addr: SocketAddr,
        now_unix_sec: u64,
    ) -> HostAuthAction {
        self.authenticated_owner = None;

        if hello.auth_method == AuthMethod::OwnerTicket {
            if hello.auth_payload.is_empty()
                || hello.auth_payload.len() > MAX_OWNER_TICKET_PAYLOAD_LEN
            {
                return self.reject(format!(
                    "Hello.auth_payload length {} invalid for AuthMethod::OwnerTicket",
                    hello.auth_payload.len()
                ));
            }
            let Some(verifier) = self.cfg.owner_verifier.as_ref() else {
                return self.reject(
                    "client requested OwnerTicket auth, but host has no owner_verifier configured"
                        .into(),
                );
            };
            let ticket = match verifier.verify_ticket(&hello.auth_payload, now_unix_sec) {
                Ok(t) => t,
                Err(e) => {
                    return self.reject(format!("owner ticket verification failed: {e}"));
                }
            };
            let is_fresh = {
                let mut cache = self.replay_cache.lock().unwrap_or_else(|e| e.into_inner());
                cache.insert_if_new(&ticket.ticket_id)
            };
            if !is_fresh {
                return self.reject(format!(
                    "owner ticket replay rejected (ticket_id={})",
                    ticket.ticket_id
                ));
            }
            let mut salt = [0u8; AUTH_SALT_LEN];
            OsRng.fill_bytes(&mut salt);
            self.state = HostState::AwaitingOwnerResponse {
                session_id: hello.session_id,
                salt,
                ticket,
                started_at_us: None,
            };
            return HostAuthAction::Challenge(AuthChallenge {
                salt,
                iterations: 1,
            });
        }

        if !self.cfg.allow_remote_assist {
            return self.reject(format!(
                "remote assist is disabled; {:?} auth is not allowed",
                hello.auth_method
            ));
        }

        let has_password = self.cfg.password.is_some();

        match (
            has_password,
            hello.auth_method,
            hello.auth_payload.is_empty(),
        ) {
            // 行 1：无密码 + None + 空载荷 → 进入等待确认（不发过程消息，AutoApprover 首拍即 Ok）
            (false, AuthMethod::None, true) => {
                let request = ApprovalRequest {
                    session_id: hello.session_id,
                    remote_addr,
                    requested: hello.requested,
                    auth_method: hello.auth_method,
                    password_verified: false,
                };
                self.state = HostState::PendingApproval {
                    request,
                    submitted: false,
                    started_at_us: None,
                    process_notified: false,
                };
                HostAuthAction::Wait
            }
            // 无密码 + None + 非空载荷 → 协议违规拒绝
            (false, AuthMethod::None, false) => self.reject(format!(
                "Hello.auth_payload must be empty for AuthMethod::None (got {} bytes)",
                hello.auth_payload.len()
            )),
            // 行 2：无密码 + Password（任意载荷）→ 不支持该方式
            (false, AuthMethod::Password, _) => self.reject(
                "client requested Password auth, but host has no password configured".into(),
            ),
            // 行 3：有密码 + None → host 要求凭据，client 拒给
            (true, AuthMethod::None, _) => {
                self.reject("host requires Password auth, but client offered None".into())
            }
            // 行 4：有密码 + Password + 非空载荷 → 密码绝不进 Hello
            (true, AuthMethod::Password, false) => self.reject(format!(
                "Hello.auth_payload must be empty for AuthMethod::Password (got {} bytes)",
                hello.auth_payload.len()
            )),
            // 行 5：有密码 + Password + 空载荷 → 生成一次性随机盐并发挑战
            (true, AuthMethod::Password, true) => {
                let mut salt = [0u8; AUTH_SALT_LEN];
                OsRng.fill_bytes(&mut salt);
                let iterations = self.cfg.iterations.clamp(1, MAX_AUTH_ITERATIONS);
                let pending_request = ApprovalRequest {
                    session_id: hello.session_id,
                    remote_addr,
                    requested: hello.requested,
                    auth_method: hello.auth_method,
                    password_verified: true,
                };
                self.state = HostState::AwaitingResponse {
                    salt,
                    iterations,
                    pending_request,
                    started_at_us: None,
                };
                HostAuthAction::Challenge(AuthChallenge { salt, iterations })
            }
            (_, AuthMethod::OwnerTicket, _) => unreachable!("handled above"),
        }
    }

    /// 收到 AuthResponse 的裁定（只应出现在密码 Challenge 之后；否则视为
    /// 协议违规并 `Conclude(Rejected)`——宁拒不猜）。
    pub fn on_auth_response(&mut self, response: &AuthResponse) -> HostAuthAction {
        let prev = std::mem::replace(&mut self.state, HostState::Init);
        match prev {
            HostState::AwaitingResponse {
                salt,
                iterations,
                pending_request,
                ..
            } => {
                let verified = self.cfg.password.as_deref().is_some_and(|pwd| {
                    verify_proof(pwd.as_bytes(), &salt, iterations, &response.proof)
                });
                if verified {
                    self.state = HostState::PendingApproval {
                        request: pending_request,
                        submitted: false,
                        started_at_us: None,
                        // 密码路径的 Challenge 已自带过程 Welcome{ChallengeRequired}，
                        // 等待人工确认期绝不再重复发 NotifyPending。
                        process_notified: true,
                    };
                    HostAuthAction::Wait
                } else {
                    self.reject("password proof verification failed".into())
                }
            }
            _ => self.reject("unexpected AuthResponse without prior AuthChallenge".into()),
        }
    }

    /// 收到 `OwnerAuthResponse` 的裁定（仅应出现在 `OwnerTicket` Challenge 之后；
    /// PoP 验签通过即直接 `Conclude(AuthStatus::Ok)`，免密码免审批弹窗）。
    pub fn on_owner_auth_response(&mut self, signature: &[u8]) -> HostAuthAction {
        let prev = std::mem::replace(&mut self.state, HostState::Init);
        match prev {
            HostState::AwaitingOwnerResponse {
                session_id,
                salt,
                ticket,
                ..
            } => {
                if signature.is_empty() || signature.len() > MAX_OWNER_SIGNATURE_LEN {
                    return self.reject(format!(
                        "invalid OwnerAuthResponse signature length: {}",
                        signature.len()
                    ));
                }
                let Some(verifier) = self.cfg.owner_verifier.as_ref() else {
                    return self.reject("owner_verifier missing during OwnerAuthResponse".into());
                };
                match verifier.verify_challenge_signature(&ticket, session_id, &salt, signature) {
                    Ok(()) => {
                        self.authenticated_owner = Some(ticket);
                        self.state = HostState::Concluded;
                        HostAuthAction::Conclude(AuthStatus::Ok)
                    }
                    Err(e) => self.reject(format!(
                        "owner challenge signature verification failed: {e}"
                    )),
                }
            }
            _ => self
                .reject("unexpected OwnerAuthResponse without prior OwnerTicket challenge".into()),
        }
    }

    /// 本次握手已通过自控 PoP 认证的票据元数据（仅 `OwnerTicket` 认证成功后为 `Some`）。
    pub fn authenticated_owner(&self) -> Option<&VerifiedOwnerTicket> {
        self.authenticated_owner.as_ref()
    }

    /// 每拍轮询人工确认（会话循环对未终局鉴权恒调；`now_us` 注入驱动
    /// `approval_timeout_us` 超时判定，铁律 4）。首次进入等待确认时向
    /// approver `submit` 一条 [`ApprovalRequest`]（先鉴权后审批：密码
    /// 失败的路径不会走到 submit），此后每拍 `poll_verdict`，裁决或
    /// 超时出终局；client 等待期断开时由会话层调 `approver.cancel()`。
    pub fn poll_approval(
        &mut self,
        now_us: u64,
        approver: &mut dyn ConnectionApprover,
    ) -> HostAuthAction {
        if let HostState::AwaitingResponse { started_at_us, .. }
        | HostState::AwaitingOwnerResponse { started_at_us, .. } = &mut self.state
        {
            let start = *started_at_us.get_or_insert(now_us);
            if now_us.saturating_sub(start) >= self.cfg.approval_timeout_us {
                let timeout_us = self.cfg.approval_timeout_us;
                return self.reject(format!(
                    "challenge response timed out after {timeout_us} us"
                ));
            }
            return HostAuthAction::Wait;
        }
        let HostState::PendingApproval {
            request,
            submitted,
            started_at_us,
            process_notified,
        } = &mut self.state
        else {
            return HostAuthAction::Wait;
        };

        if !*submitted {
            approver.submit(request);
            *submitted = true;
            *started_at_us = Some(now_us);
        }

        match approver.poll_verdict() {
            Some(ApprovalVerdict::Accepted) => {
                self.state = HostState::Concluded;
                HostAuthAction::Conclude(AuthStatus::Ok)
            }
            Some(ApprovalVerdict::Rejected) => {
                self.reject("connection rejected by approver".into())
            }
            None => {
                let start = started_at_us.unwrap_or(now_us);
                if now_us.saturating_sub(start) >= self.cfg.approval_timeout_us {
                    approver.cancel();
                    let timeout_us = self.cfg.approval_timeout_us;
                    return self.reject(format!("approval timed out after {timeout_us} us"));
                }
                if !*process_notified {
                    *process_notified = true;
                    HostAuthAction::NotifyPending
                } else {
                    HostAuthAction::Wait
                }
            }
        }
    }

    /// 鉴权是否已终局（终局后会话层停止调用上面三个方法）。
    pub fn concluded(&self) -> bool {
        matches!(self.state, HostState::Concluded)
    }

    /// 统一拒绝出口：仅向本端 stderr 记录原因（A5：绝不把原因文本送上网络），
    /// 并置终局为 `AuthStatus::Rejected`。
    fn reject(&mut self, reason: String) -> HostAuthAction {
        eprintln!("[rc-session/auth] host rejected connection: {reason}");
        self.state = HostState::Concluded;
        HostAuthAction::Conclude(AuthStatus::Rejected)
    }
}

// ─────────────────────────────────────────────────────────────────
// ClientAuthenticator（client 侧纯状态机）
// ─────────────────────────────────────────────────────────────────

/// client 侧动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientAuthAction {
    /// 终局成功（终局 `Welcome{Ok}`）：进入 Connected。
    Done,
    /// 发送 `AuthResponse`。
    Respond(AuthResponse),
    /// 发送 `OwnerAuthResponse`。
    RespondOwner(Vec<u8>),
    /// 过程通知（收到 `Welcome{ChallengeRequired}`）：等待挑战或终局，不发消息。
    Waiting,
    /// 终局失败：断开。字符串为本端呈现原因（stderr/日志用，**不上线**）。
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientState {
    Initial,
    Waiting,
    Responded,
    Done,
    Failed,
}

/// client 侧鉴权纯状态机（transport 无关）。
pub struct ClientAuthenticator {
    cfg: ClientAuthConfig,
    hello_session_id: u64,
    state: ClientState,
    hello_sent_at_us: Option<u64>,
}

impl ClientAuthenticator {
    /// 按配置构造 client 侧鉴权状态机（`hello_session_id` 默认 0；需要 PoP 签名时
    /// 可用 [`Self::with_session_id`] 或 [`Self::set_session_id`]）。
    pub fn new(cfg: ClientAuthConfig) -> Self {
        Self::with_session_id(cfg, 0)
    }

    /// 按配置与 `Hello.session_id` 构造 client 侧鉴权状态机。
    pub fn with_session_id(cfg: ClientAuthConfig, hello_session_id: u64) -> Self {
        Self {
            cfg,
            hello_session_id,
            state: ClientState::Initial,
            hello_sent_at_us: None,
        }
    }

    /// 更新本次握手的 `Hello.session_id`（供 PoP 签名绑定本次握手会话）。
    pub fn set_session_id(&mut self, hello_session_id: u64) {
        self.hello_session_id = hello_session_id;
    }

    /// 构造 Hello 的鉴权位：配置 `owner_signer` 时优先发 `(AuthMethod::OwnerTicket, payload)`；
    /// 否则有密码发 `(AuthMethod::Password, 空)`，无密码发 `(AuthMethod::None, 空)`。
    /// **密码绝不进 Hello。**
    pub fn hello_auth(&self) -> (AuthMethod, Vec<u8>) {
        if let Some(ref signer) = self.cfg.owner_signer {
            return (AuthMethod::OwnerTicket, signer.ticket_payload());
        }
        match self.cfg.password {
            Some(_) => (AuthMethod::Password, Vec::new()),
            None => (AuthMethod::None, Vec::new()),
        }
    }

    /// 记录 Hello 发出时刻（auth 超时窗口起点，会话钟注入）。
    pub fn on_hello_sent(&mut self, now_us: u64) {
        self.hello_sent_at_us = Some(now_us);
    }

    /// 收到 Welcome 的处置：`Ok` → `Done`（仅握手期）、`ChallengeRequired` →
    /// `Waiting`（进入 Authenticating，显式忽略 `granted`）、`Rejected` → `Failed`。
    pub fn on_welcome(&mut self, welcome: &Welcome) -> ClientAuthAction {
        if matches!(self.state, ClientState::Done | ClientState::Failed) {
            self.state = ClientState::Failed;
            return ClientAuthAction::Failed(
                "protocol violation: received Welcome after authentication concluded".into(),
            );
        }

        match welcome.auth_status {
            AuthStatus::Ok => {
                self.state = ClientState::Done;
                ClientAuthAction::Done
            }
            AuthStatus::ChallengeRequired => {
                // 显式忽略过程 Welcome 的 granted 字段（坑 5：其为预裁定值，
                // client 只能以终局 Welcome{Ok}.granted 为准）。
                let _ = welcome.granted;
                self.state = ClientState::Waiting;
                ClientAuthAction::Waiting
            }
            AuthStatus::Rejected => {
                self.state = ClientState::Failed;
                ClientAuthAction::Failed("authentication rejected by host".into())
            }
        }
    }

    /// 收到 `AuthChallenge`：仅在 `Waiting` 态接受，否则 `Failed`（协议违规）；
    /// 若配置了 `owner_signer` 则调用 PoP 签名返回 `RespondOwner`，否则同步计算密码 proof 返回 `Respond`。
    pub fn on_challenge(&mut self, challenge: &AuthChallenge) -> ClientAuthAction {
        if self.state != ClientState::Waiting {
            self.state = ClientState::Failed;
            return ClientAuthAction::Failed(
                "protocol violation: received AuthChallenge before Welcome{ChallengeRequired}"
                    .into(),
            );
        }

        if let Some(ref signer) = self.cfg.owner_signer {
            match signer.sign_challenge(self.hello_session_id, &challenge.salt) {
                Ok(sig) => {
                    if sig.is_empty() || sig.len() > MAX_OWNER_SIGNATURE_LEN {
                        self.state = ClientState::Failed;
                        return ClientAuthAction::Failed(format!(
                            "owner_signer produced invalid signature length {}",
                            sig.len()
                        ));
                    }
                    self.state = ClientState::Responded;
                    return ClientAuthAction::RespondOwner(sig);
                }
                Err(e) => {
                    self.state = ClientState::Failed;
                    return ClientAuthAction::Failed(format!(
                        "owner challenge signing failed: {e}"
                    ));
                }
            }
        }

        let Some(ref password) = self.cfg.password else {
            self.state = ClientState::Failed;
            return ClientAuthAction::Failed(
                "host requested password challenge, but client has no password configured".into(),
            );
        };

        let proof = compute_proof(password.as_bytes(), &challenge.salt, challenge.iterations);
        self.state = ClientState::Responded;
        ClientAuthAction::Respond(AuthResponse { proof })
    }

    /// 超时复核：`true` = 已越 `auth_timeout_us` 窗口，应断开。
    /// 终局（`Done`/`Failed`）后恒 `false`。
    pub fn poll_timeout(&mut self, now_us: u64) -> bool {
        if matches!(self.state, ClientState::Done | ClientState::Failed) {
            return false;
        }
        let Some(t0) = self.hello_sent_at_us else {
            return false;
        };
        if now_us < t0 {
            // 时钟纪元回跳（如 resume 被拒降级后驱动器归零会话钟）：重锚起点，
            // 避免 saturating_sub 恒为 0 导致鉴权超时失效（M7）。
            self.hello_sent_at_us = Some(now_us);
            return false;
        }
        if now_us - t0 >= self.cfg.auth_timeout_us {
            self.state = ClientState::Failed;
            return true;
        }
        false
    }
}

// ─────────────────────────────────────────────────────────────────
// 内联单测（纯函数公开向量 + 状态机全决策表）
// ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{HostFlags, PROTOCOL_VERSION};

    fn hex_decode_32(hex: &str) -> [u8; 32] {
        assert_eq!(hex.len(), 64, "expected 64 hex chars");
        let mut out = [0u8; 32];
        for i in 0..32 {
            out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap();
        }
        out
    }

    fn sample_addr() -> SocketAddr {
        "192.168.1.42:54321".parse().unwrap()
    }

    fn sample_params() -> SessionParams {
        SessionParams {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_bps: 8_000_000,
        }
    }

    fn sample_hello(method: AuthMethod, payload: Vec<u8>) -> Hello {
        Hello {
            version: PROTOCOL_VERSION,
            session_id: 0xDEAD_BEEF_1234_5678,
            requested: sample_params(),
            auth_method: method,
            auth_payload: payload,
        }
    }

    fn sample_welcome(status: AuthStatus) -> Welcome {
        Welcome {
            session_id: 0xDEAD_BEEF_1234_5678,
            granted: sample_params(),
            auth_status: status,
            host_flags: HostFlags(0),
        }
    }

    // ── 1-5：密码学纯函数与标准公开向量 ─────────────────────────

    #[test]
    fn sha256_fips_vectors() {
        // FIPS 180-4 公开向量：空串、"abc"、448bit（56 字节跨双块填充）
        assert_eq!(
            sha256(b""),
            hex_decode_32("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
        assert_eq!(
            sha256(b"abc"),
            hex_decode_32("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        assert_eq!(
            sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            hex_decode_32("248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1")
        );
    }

    #[test]
    fn hmac_sha256_rfc4231_vectors() {
        // RFC 4231 Test Case 1
        let k1 = [0x0bu8; 20];
        assert_eq!(
            HmacSha256::new(&k1).mac(b"Hi There"),
            hex_decode_32("b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7")
        );
        // RFC 4231 Test Case 2
        assert_eq!(
            HmacSha256::new(b"Jefe").mac(b"what do ya want for nothing?"),
            hex_decode_32("5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843")
        );
        // RFC 4231 Test Case 6（覆盖 key > 64 字节先哈希分支）
        let k6 = [0xaau8; 131];
        assert_eq!(
            HmacSha256::new(&k6).mac(b"Test Using Larger Than Block-Size Key - Hash Key First"),
            hex_decode_32("60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54")
        );
    }

    #[test]
    fn pbkdf2_hmac_sha256_vectors() {
        // RFC 7914 / 标准公开 PBKDF2-HMAC-SHA256 向量（dkLen=32）
        assert_eq!(
            pbkdf2_hmac_sha256(b"password", b"salt", 1),
            hex_decode_32("120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b")
        );
        assert_eq!(
            pbkdf2_hmac_sha256(b"password", b"salt", 2),
            hex_decode_32("ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43")
        );
        assert_eq!(
            pbkdf2_hmac_sha256(b"password", b"salt", 4096),
            hex_decode_32("c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a")
        );
    }

    #[test]
    fn compute_proof_deterministic_salt_sensitive() {
        let salt = [0x11u8; AUTH_SALT_LEN];
        let p1 = compute_proof(b"hunter2", &salt, 64);
        let p2 = compute_proof(b"hunter2", &salt, 64);
        assert_eq!(p1, p2);

        let mut flipped_salt = salt;
        flipped_salt[0] ^= 0x01;
        let p3 = compute_proof(b"hunter2", &flipped_salt, 64);
        assert_ne!(p1, p3);
    }

    #[test]
    fn verify_proof_rejects_single_bit_flip() {
        let salt = [0x42u8; AUTH_SALT_LEN];
        let proof = compute_proof(b"hunter2", &salt, 64);
        assert!(verify_proof(b"hunter2", &salt, 64, &proof));

        for idx in [0usize, 15, 31] {
            let mut bad = proof;
            bad[idx] ^= 0x01;
            assert!(
                !verify_proof(b"hunter2", &salt, 64, &bad),
                "flipped byte {idx} must fail verification"
            );
        }
    }

    // ── 6-15：HostAuthenticator 决策表与审批流 ───────────────────

    #[test]
    fn host_decision_table_full_matrix() {
        let addr = sample_addr();

        // 行 1：无密码 + None + 空载荷 → Wait（未终局）
        let mut h1 = HostAuthenticator::new(HostAuthConfig {
            password: None,
            iterations: 64,
            approval_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
            ..HostAuthConfig::default()
        });
        assert_eq!(
            h1.on_hello(&sample_hello(AuthMethod::None, vec![]), addr),
            HostAuthAction::Wait
        );
        assert!(!h1.concluded());

        // 行 2：无密码 + Password（任意载荷）→ Conclude(Rejected) + concluded
        let mut h2 = HostAuthenticator::new(HostAuthConfig {
            password: None,
            iterations: 64,
            approval_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
            ..HostAuthConfig::default()
        });
        assert_eq!(
            h2.on_hello(&sample_hello(AuthMethod::Password, vec![]), addr),
            HostAuthAction::Conclude(AuthStatus::Rejected)
        );
        assert!(h2.concluded());

        // 行 3：有密码 + None + 空载荷 → Conclude(Rejected) + concluded
        let mut h3 = HostAuthenticator::new(HostAuthConfig {
            password: Some("hunter2".into()),
            iterations: 64,
            approval_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
            ..HostAuthConfig::default()
        });
        assert_eq!(
            h3.on_hello(&sample_hello(AuthMethod::None, vec![]), addr),
            HostAuthAction::Conclude(AuthStatus::Rejected)
        );
        assert!(h3.concluded());

        // 行 4：有密码 + Password + 非空载荷 → Conclude(Rejected) + concluded
        let mut h4 = HostAuthenticator::new(HostAuthConfig {
            password: Some("hunter2".into()),
            iterations: 64,
            approval_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
            ..HostAuthConfig::default()
        });
        assert_eq!(
            h4.on_hello(
                &sample_hello(AuthMethod::Password, b"hunter2".to_vec()),
                addr
            ),
            HostAuthAction::Conclude(AuthStatus::Rejected)
        );
        assert!(h4.concluded());

        // 行 5：有密码 + Password + 空载荷 → Challenge(ch)（iterations=64，未终局）
        let mut h5 = HostAuthenticator::new(HostAuthConfig {
            password: Some("hunter2".into()),
            iterations: 64,
            approval_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
            ..HostAuthConfig::default()
        });
        match h5.on_hello(&sample_hello(AuthMethod::Password, vec![]), addr) {
            HostAuthAction::Challenge(ch) => assert_eq!(ch.iterations, 64),
            other => panic!("expected Challenge, got {other:?}"),
        }
        assert!(!h5.concluded());
    }

    #[test]
    fn challenge_flow_correct_password_auto_approver() {
        let mut host = HostAuthenticator::new(HostAuthConfig {
            password: Some("hunter2".into()),
            iterations: 64,
            approval_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
            ..HostAuthConfig::default()
        });
        let ch = match host.on_hello(&sample_hello(AuthMethod::Password, vec![]), sample_addr()) {
            HostAuthAction::Challenge(ch) => ch,
            other => panic!("expected Challenge, got {other:?}"),
        };

        let proof = compute_proof(b"hunter2", &ch.salt, ch.iterations);
        assert_eq!(
            host.on_auth_response(&AuthResponse { proof }),
            HostAuthAction::Wait
        );
        assert!(!host.concluded());

        let mut approver = AutoApprover;
        // 首拍即 Conclude(Ok)，全程无 NotifyPending
        assert_eq!(
            host.poll_approval(1_000, &mut approver),
            HostAuthAction::Conclude(AuthStatus::Ok)
        );
        assert!(host.concluded());
    }

    #[test]
    fn challenge_flow_wrong_password_rejected() {
        let mut host = HostAuthenticator::new(HostAuthConfig {
            password: Some("hunter2".into()),
            iterations: 64,
            approval_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
            ..HostAuthConfig::default()
        });
        let ch = match host.on_hello(&sample_hello(AuthMethod::Password, vec![]), sample_addr()) {
            HostAuthAction::Challenge(ch) => ch,
            other => panic!("expected Challenge, got {other:?}"),
        };

        let wrong_proof = compute_proof(b"wrong-password", &ch.salt, ch.iterations);
        assert_eq!(
            host.on_auth_response(&AuthResponse { proof: wrong_proof }),
            HostAuthAction::Conclude(AuthStatus::Rejected)
        );
        assert!(host.concluded());
    }

    #[test]
    fn deny_approver_rejects_immediately() {
        let mut approver = DenyApprover;
        let req = ApprovalRequest {
            session_id: 1,
            remote_addr: sample_addr(),
            requested: sample_params(),
            auth_method: AuthMethod::None,
            password_verified: false,
        };
        approver.submit(&req);
        assert_eq!(approver.poll_verdict(), Some(ApprovalVerdict::Rejected));
        // cancel 幂等 no-op
        approver.cancel();
        approver.cancel();
    }

    #[test]
    fn approver_request_carries_hello_and_addr() {
        struct RecordingApprover {
            submitted: Vec<ApprovalRequest>,
        }
        impl ConnectionApprover for RecordingApprover {
            fn submit(&mut self, req: &ApprovalRequest) {
                self.submitted.push(req.clone());
            }
            fn poll_verdict(&mut self) -> Option<ApprovalVerdict> {
                Some(ApprovalVerdict::Accepted)
            }
            fn cancel(&mut self) {}
        }

        let addr: SocketAddr = "10.0.0.99:44332".parse().unwrap();
        let hello = sample_hello(AuthMethod::Password, vec![]);
        let mut host = HostAuthenticator::new(HostAuthConfig {
            password: Some("secret".into()),
            iterations: 64,
            approval_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
            ..HostAuthConfig::default()
        });

        let ch = match host.on_hello(&hello, addr) {
            HostAuthAction::Challenge(ch) => ch,
            other => panic!("expected Challenge, got {other:?}"),
        };
        let proof = compute_proof(b"secret", &ch.salt, ch.iterations);
        assert_eq!(
            host.on_auth_response(&AuthResponse { proof }),
            HostAuthAction::Wait
        );

        let mut approver = RecordingApprover {
            submitted: Vec::new(),
        };
        assert_eq!(
            host.poll_approval(5_000, &mut approver),
            HostAuthAction::Conclude(AuthStatus::Ok)
        );
        assert_eq!(approver.submitted.len(), 1);
        let req = &approver.submitted[0];
        assert_eq!(req.session_id, hello.session_id);
        assert_eq!(req.remote_addr, addr);
        assert_eq!(req.requested, hello.requested);
        assert_eq!(req.auth_method, AuthMethod::Password);
        assert!(req.password_verified);
    }

    #[test]
    fn no_password_immediate_accept_no_process_welcome() {
        let mut host = HostAuthenticator::new(HostAuthConfig::default());
        assert_eq!(
            host.on_hello(&sample_hello(AuthMethod::None, vec![]), sample_addr()),
            HostAuthAction::Wait
        );

        let mut approver = AutoApprover;
        // 首拍 poll 即 Conclude(Ok)，全程无 NotifyPending（T14 兼容钉子）
        assert_eq!(
            host.poll_approval(1_000, &mut approver),
            HostAuthAction::Conclude(AuthStatus::Ok)
        );
        assert!(host.concluded());
    }

    #[test]
    fn no_password_pending_approver_notifies_then_concludes() {
        let mut host = HostAuthenticator::new(HostAuthConfig::default());
        assert_eq!(
            host.on_hello(&sample_hello(AuthMethod::None, vec![]), sample_addr()),
            HostAuthAction::Wait
        );

        let mut approver = ScriptedApprover::new(vec![None, None, Some(ApprovalVerdict::Accepted)]);
        assert_eq!(
            host.poll_approval(1_000, &mut approver),
            HostAuthAction::NotifyPending
        );
        assert_eq!(
            host.poll_approval(2_000, &mut approver),
            HostAuthAction::Wait
        );
        assert_eq!(
            host.poll_approval(3_000, &mut approver),
            HostAuthAction::Conclude(AuthStatus::Ok)
        );
        assert!(host.concluded());
    }

    #[test]
    fn approval_timeout_rejects() {
        let mut host = HostAuthenticator::new(HostAuthConfig {
            password: None,
            iterations: 64,
            approval_timeout_us: 10_000,
            ..HostAuthConfig::default()
        });
        assert_eq!(
            host.on_hello(&sample_hello(AuthMethod::None, vec![]), sample_addr()),
            HostAuthAction::Wait
        );

        let mut approver = ScriptedApprover::new(vec![None]);
        // t0 = 1_000 首次进入等待 → NotifyPending
        assert_eq!(
            host.poll_approval(1_000, &mut approver),
            HostAuthAction::NotifyPending
        );
        // 窗口内仍 Wait
        assert_eq!(
            host.poll_approval(10_999, &mut approver),
            HostAuthAction::Wait
        );
        assert!(!host.concluded());
        // 越过 approval_timeout_us → Conclude(Rejected)
        assert_eq!(
            host.poll_approval(11_001, &mut approver),
            HostAuthAction::Conclude(AuthStatus::Rejected)
        );
        assert!(host.concluded());
    }

    #[test]
    fn auth_response_without_challenge_is_rejected() {
        let mut host = HostAuthenticator::new(HostAuthConfig {
            password: Some("hunter2".into()),
            iterations: 64,
            approval_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
            ..HostAuthConfig::default()
        });
        assert_eq!(
            host.on_auth_response(&AuthResponse {
                proof: [0u8; AUTH_PROOF_LEN]
            }),
            HostAuthAction::Conclude(AuthStatus::Rejected)
        );
        assert!(host.concluded());
    }

    #[test]
    fn fresh_salt_per_handshake() {
        let mut host = HostAuthenticator::new(HostAuthConfig {
            password: Some("hunter2".into()),
            iterations: 64,
            approval_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
            ..HostAuthConfig::default()
        });
        let ch1 = match host.on_hello(&sample_hello(AuthMethod::Password, vec![]), sample_addr()) {
            HostAuthAction::Challenge(ch) => ch,
            other => panic!("expected Challenge, got {other:?}"),
        };
        let ch2 = match host.on_hello(&sample_hello(AuthMethod::Password, vec![]), sample_addr()) {
            HostAuthAction::Challenge(ch) => ch,
            other => panic!("expected Challenge, got {other:?}"),
        };
        assert_ne!(ch1.salt, ch2.salt);
    }

    // ── 16-20：ClientAuthenticator 状态机与超时 ──────────────────

    #[test]
    fn client_hello_auth_never_carries_password() {
        let c_none = ClientAuthenticator::new(ClientAuthConfig::default());
        assert_eq!(c_none.hello_auth(), (AuthMethod::None, Vec::new()));

        let c_pwd = ClientAuthenticator::new(ClientAuthConfig {
            password: Some("super-secret-password".into()),
            auth_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
            ..ClientAuthConfig::default()
        });
        assert_eq!(c_pwd.hello_auth(), (AuthMethod::Password, Vec::new()));
    }

    #[test]
    fn client_welcome_lifecycle() {
        let mut c1 = ClientAuthenticator::new(ClientAuthConfig::default());
        assert_eq!(
            c1.on_welcome(&sample_welcome(AuthStatus::Ok)),
            ClientAuthAction::Done
        );

        let mut c2 = ClientAuthenticator::new(ClientAuthConfig {
            password: Some("hunter2".into()),
            auth_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
            ..ClientAuthConfig::default()
        });
        // 过程 Welcome 携带的 granted 被显式忽略，Waiting 不携带参数
        let mut proc_welcome = sample_welcome(AuthStatus::ChallengeRequired);
        proc_welcome.granted = SessionParams {
            width: 0,
            height: 0,
            fps: 0,
            bitrate_bps: 0,
        };
        assert_eq!(c2.on_welcome(&proc_welcome), ClientAuthAction::Waiting);

        let mut c3 = ClientAuthenticator::new(ClientAuthConfig::default());
        match c3.on_welcome(&sample_welcome(AuthStatus::Rejected)) {
            ClientAuthAction::Failed(msg) => assert!(!msg.is_empty()),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn client_challenge_requires_process_welcome_first() {
        let mut client = ClientAuthenticator::new(ClientAuthConfig {
            password: Some("hunter2".into()),
            auth_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
            ..ClientAuthConfig::default()
        });
        let ch = AuthChallenge {
            salt: [0x55u8; AUTH_SALT_LEN],
            iterations: 64,
        };
        match client.on_challenge(&ch) {
            ClientAuthAction::Failed(msg) => {
                assert!(
                    msg.contains("protocol violation"),
                    "expected protocol violation message, got: {msg}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn client_challenge_computes_expected_proof() {
        let mut client = ClientAuthenticator::new(ClientAuthConfig {
            password: Some("hunter2".into()),
            auth_timeout_us: DEFAULT_AUTH_TIMEOUT_US,
            ..ClientAuthConfig::default()
        });
        assert_eq!(
            client.on_welcome(&sample_welcome(AuthStatus::ChallengeRequired)),
            ClientAuthAction::Waiting
        );
        let ch = AuthChallenge {
            salt: [0xABu8; AUTH_SALT_LEN],
            iterations: 64,
        };
        let expected = compute_proof(b"hunter2", &ch.salt, ch.iterations);
        assert_eq!(
            client.on_challenge(&ch),
            ClientAuthAction::Respond(AuthResponse { proof: expected })
        );
    }

    #[test]
    fn client_timeout_window() {
        let t0 = 10_000u64;
        let window = 30_000u64;

        let mut c1 = ClientAuthenticator::new(ClientAuthConfig {
            password: None,
            auth_timeout_us: window,
            ..ClientAuthConfig::default()
        });
        c1.on_hello_sent(t0);
        assert!(!c1.poll_timeout(t0 + window - 1));
        assert!(c1.poll_timeout(t0 + window + 1));

        // Done 后恒 false
        let mut c2 = ClientAuthenticator::new(ClientAuthConfig {
            password: None,
            auth_timeout_us: window,
            ..ClientAuthConfig::default()
        });
        c2.on_hello_sent(t0);
        assert_eq!(
            c2.on_welcome(&sample_welcome(AuthStatus::Ok)),
            ClientAuthAction::Done
        );
        assert!(!c2.poll_timeout(t0 + window + 1_000_000));
    }

    #[test]
    fn scripted_approver_clears_in_flight_on_terminal_verdict() {
        let req = ApprovalRequest {
            session_id: 1,
            remote_addr: sample_addr(),
            requested: sample_params(),
            auth_method: AuthMethod::None,
            password_verified: false,
        };
        let mut approver = ScriptedApprover::new(vec![
            None,
            Some(ApprovalVerdict::Rejected),
            Some(ApprovalVerdict::Accepted),
        ]);

        // 首次提交：None 保持 in_flight，终局 Rejected 清除 in_flight。
        approver.submit(&req);
        assert_eq!(approver.poll_verdict(), None);
        assert_eq!(approver.poll_verdict(), Some(ApprovalVerdict::Rejected));
        // 终局后未重新 submit 时再次轮询应返回 None。
        assert_eq!(approver.poll_verdict(), None);

        // 无需先调用 cancel()，下一候选连接再次 submit 不得触发 debug_assert。
        approver.submit(&req);
        assert_eq!(approver.poll_verdict(), Some(ApprovalVerdict::Accepted));
        assert_eq!(approver.poll_verdict(), None);
    }

    /// H2 回归：密码挑战已发出（AwaitingResponse）但对端不回 AuthResponse，
    /// `poll_approval` 在 `approval_timeout_us` 后必须拒绝终局，防连接被永久霸占。
    #[test]
    fn host_awaiting_response_times_out_in_poll_approval() {
        let mut host = HostAuthenticator::new(HostAuthConfig {
            password: Some("secret".into()),
            iterations: 64,
            approval_timeout_us: 10_000,
            ..HostAuthConfig::default()
        });
        let mut approver = AutoApprover;
        let hello = sample_hello(AuthMethod::Password, Vec::new());
        assert!(matches!(
            host.on_hello(&hello, sample_addr()),
            HostAuthAction::Challenge(_)
        ));
        // 首次轮询锚定起点 t=1_000
        assert_eq!(
            host.poll_approval(1_000, &mut approver),
            HostAuthAction::Wait
        );
        assert!(!host.concluded());
        // 未到期继续等
        assert_eq!(
            host.poll_approval(10_999, &mut approver),
            HostAuthAction::Wait
        );
        assert!(!host.concluded());
        // 到期拒绝并终局
        assert_eq!(
            host.poll_approval(11_000, &mut approver),
            HostAuthAction::Conclude(AuthStatus::Rejected)
        );
        assert!(host.concluded());
    }

    /// M7 回归：resume 被拒降级为全新 Hello 后驱动器把会话钟归零（`now_us < t0`），
    /// `ClientAuthenticator::poll_timeout` 应重锚起点而非因 saturating_sub 恒为 0 永不超时。
    #[test]
    fn client_poll_timeout_reanchors_on_clock_epoch_reset() {
        let window = 30_000u64;
        let mut client = ClientAuthenticator::new(ClientAuthConfig {
            password: None,
            auth_timeout_us: window,
            ..ClientAuthConfig::default()
        });
        // 降级 Hello 在旧纪元末尾 t=4_000_000_000 发出
        client.on_hello_sent(4_000_000_000);
        // 驱动器重锚时钟到 0 后首拍（now_us=100 < t0）→ 重锚到 100 并返回 false
        assert!(!client.poll_timeout(100));
        // 距新锚点未满 window → false
        assert!(!client.poll_timeout(100 + window - 1));
        // 距新锚点满 window → true（超时生效）
        assert!(client.poll_timeout(100 + window));
    }

    #[test]
    fn owner_replay_cache_bounded_eviction() {
        let mut cache = OwnerReplayCache::default();
        for i in 0..OWNER_REPLAY_CACHE_CAP {
            let id = format!("t-{i}");
            assert!(cache.insert_if_new(&id));
            assert!(!cache.insert_if_new(&id));
        }
        // 插入第 257 个，淘汰最早的 t-0
        assert!(cache.insert_if_new("t-new"));
        assert!(!cache.insert_if_new("t-1"));
        assert!(cache.insert_if_new("t-0"));
    }

    #[test]
    fn host_awaiting_owner_response_times_out_in_poll_approval() {
        struct StubVerifier;
        impl OwnerTicketVerifier for StubVerifier {
            fn verify_ticket(
                &self,
                payload: &[u8],
                now_unix_sec: u64,
            ) -> Result<VerifiedOwnerTicket> {
                if payload == b"good" && now_unix_sec < 1_000 {
                    Ok(VerifiedOwnerTicket {
                        ticket_id: "t-timeout".into(),
                        user_id: "u1".into(),
                        caller_device_id: "c1".into(),
                        caller_pubkey_hex: "aa".into(),
                        target_device_id: "h1".into(),
                        session_id_str: "s1".into(),
                        expires_at_unix_sec: 1_000,
                    })
                } else {
                    Err(rc_types::Error::Protocol {
                        offset: 0,
                        msg: "bad".into(),
                    })
                }
            }
            fn verify_challenge_signature(
                &self,
                _ticket: &VerifiedOwnerTicket,
                _hello_session_id: u64,
                _salt: &[u8; AUTH_SALT_LEN],
                _signature: &[u8],
            ) -> Result<()> {
                Ok(())
            }
        }

        let mut host = HostAuthenticator::new(HostAuthConfig {
            allow_remote_assist: false,
            owner_verifier: Some(Arc::new(StubVerifier)),
            unix_time_fn: Some(Arc::new(|| 500)),
            approval_timeout_us: 5_000,
            ..HostAuthConfig::default()
        });
        let hello = sample_hello(AuthMethod::OwnerTicket, b"good".to_vec());
        assert!(matches!(
            host.on_hello(&hello, sample_addr()),
            HostAuthAction::Challenge(AuthChallenge { iterations: 1, .. })
        ));
        let mut approver = DenyApprover;
        assert_eq!(
            host.poll_approval(1_000, &mut approver),
            HostAuthAction::Wait
        );
        assert_eq!(
            host.poll_approval(6_000, &mut approver),
            HostAuthAction::Conclude(AuthStatus::Rejected)
        );
        assert!(host.concluded());
        assert!(host.authenticated_owner().is_none());
    }
}
