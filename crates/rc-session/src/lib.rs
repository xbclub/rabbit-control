//! RabbitControl 会话层：M2 双机直连的控制平面。
//!
//! # 为什么存在
//!
//! M1 的 rc-pipeline 是**本机**管线驱动（host.rs / controller.rs 只认
//! tick / push_datagram，不认连接生命周期）；rc-net 有 T09 红线「不碰
//! 采集/编码/解码」只做传输；rc-cli 必须薄。会话握手、IDR 请求、探测、
//! 码率设定、重连恢复需要一条**控制消息协议**与一个**会话状态机**，
//! 它们的家在这里。T13 定义了「控制消息长什么样」（[`proto`]）；
//! T14 在其上落地两侧会话（[`host_session`]/[`client_session`]）与
//! 进程内测试对接件（[`testkit`]）。
//!
//! # 模块地图（随任务卡增量落地，骨架期不预建空壳）
//!
//! | 模块 | 职责 | 落地卡 |
//! |---|---|---|
//! | [`proto`] | 控制消息线格式：类型号、枚举、encode/decode、版本校验 | T13 |
//! | [`host_session`] / [`client_session`] | 会话状态机与握手（Init→Connected→Streaming→Closed）、驱动循环与停机 | T14 |
//! | [`testkit`] | 进程内 MockTransport + pair 对接件（离线确定性测试） | T14 |
//! | [`probe`] | RTT / 时钟偏移 / 带宽探测算法与初始码率（v0 粗 heuristic，M3-3 废弃） | T15 |
//! | [`watchdog`] | IDR 停滞看门狗与请求去抖 | T16 |
//! | [`reconnect`] | 断线重连纯逻辑：指数退避、保持窗口、时钟饱和守卫 | T17 |
//! | [`report`] | client 会话报告与 Gate（跨钟域换算、JSON schema） | T18 |
//! | [`dev_cert`] | 内嵌开发证书：localhost 双进程直连的零配置身份 | T18 |
//! | [`auth`] | 鉴权：手写 SHA-256/HMAC/PBKDF2 密码挑战状态机与连接审批抽象 | T19 |
//! | [`clipboard`] | 剪贴板 UTF-8 纯文本双向同步引擎（回声抑制、节流合并、大小上限） | T20 |
//! | `clipboard_macos` | macOS CarbonCore Pasteboard 纯 C 后端（`cfg(target_os = "macos")`，本 crate 唯一 FFI 边界模块） | T20 |
//!
//! # 纪律
//!
//! 纯 Rust、除 `clipboard_macos` 外全域 `deny(unsafe_code)`、时间注入（铁律 4）：
//! 会话钟由调用方以 [`SessionClock`] 闭包注入，两侧管线共享同一闭包；
//! 唯一的例外是 `client_session` 的 `run()` 用 `Instant::now()` 作驱动钟（坑 5 认可，
//! 见该模块文档）与 `testkit` 的脚本化 IO 边界件。线格式纪律对齐 rc-input：
//! 拒绝未知类型、错误带偏移、绝不 panic、绝不静默跳过。

#![deny(missing_docs)]
#![deny(unsafe_code)]

pub mod auth;
pub mod clipboard;
#[cfg(target_os = "macos")]
pub mod clipboard_macos;
pub mod dev_cert;
pub mod probe;
pub mod proto;
pub mod reconnect;
pub mod report;
pub mod watchdog;

mod client_session;
mod host_session;
pub mod testkit;

pub use auth::{
    ApprovalRequest, ApprovalVerdict, AutoApprover, ClientAuthAction, ClientAuthConfig,
    ClientAuthenticator, ConnectionApprover, DenyApprover, HostAuthAction, HostAuthConfig,
    HostAuthenticator, OwnerReplayCache, OwnerTicketSigner, OwnerTicketVerifier, ScriptedApprover,
    VerifiedOwnerTicket, DEFAULT_AUTH_ITERATIONS, DEFAULT_AUTH_TIMEOUT_US,
};
pub use client_session::{
    ClientRunSummary, ClientSession, ClientSessionConfig, ClientTickReport, ProbeSnapshot,
    ReconnectOutcome,
};
pub use clipboard::{
    ClipboardAction, ClipboardBackend, ClipboardChannel, ClipboardConfig, ClipboardStats,
    CLIPBOARD_MIN_NOTIFY_INTERVAL_US,
};
#[cfg(target_os = "macos")]
pub use clipboard_macos::NspasteboardBackend;
pub use host_session::{
    DetachOutcome, HostRunSummary, HostSession, HostSessionConfig, HostTickReport,
};
pub use proto::{
    AudioFramePacket, AuthMethod, ControlMessage, AUDIO_CODEC_IMA_ADPCM, AUDIO_CODEC_PCM16,
    MAX_AUDIO_FRAME_BYTES, MAX_AUDIO_SAMPLE_RATE, MAX_OWNER_SIGNATURE_LEN,
    MAX_OWNER_TICKET_PAYLOAD_LEN, MAX_TOOL_PAYLOAD_BYTES, MIN_AUDIO_SAMPLE_RATE,
    TOOL_CHANNEL_AUDIO_CTRL, TOOL_CHANNEL_DISPLAY_CTRL, TOOL_CHANNEL_FILE, TOOL_CHANNEL_PORTMAP,
    TOOL_CHANNEL_TERMINAL, TYPE_OWNER_AUTH_RESPONSE,
};
pub use reconnect::{
    Backoff, ReconnectConfig, ReconnectStats, RefreshGuard, ResumeWindow, DEFAULT_BACKOFF_INITIAL,
    DEFAULT_BACKOFF_MAX, DEFAULT_BACKOFF_MAX_ATTEMPTS, DEFAULT_RESUME_TTL, REFRESH_GUARD_US,
};
pub use report::ClipboardReport;
pub use testkit::{pair, MockTransport};

/// 会话默认路径 MTU 预算（rc-net 常量的薄转发：T18 的 CLI 组合根不直接
/// 依赖 rc-net，经这里取同一常量）。
pub const DEFAULT_SESSION_MTU: usize = rc_net::quic::DEFAULT_MTU;

use crate::proto::ByeReason;

/// 会话时钟：微秒，相对会话零点。照抄 run_real 的 clock Arc 模式
/// （`crates/rc-loopback/src/lib.rs` 的 `run_real`）：由构造方注入，两侧
/// 管线共享同一闭包；会话内部除驱动循环的 poll 预算外不读任何时钟。
///
/// 闭包返回值用 `u32::try_from(...).unwrap_or(u32::MAX)` 饱和，不 `as`
/// 截断（坑 6；饱和守卫本身是 T17 的 `Bye(Refresh)`）。
pub type SessionClock = std::sync::Arc<dyn Fn() -> u32 + Send + Sync>;

/// 会话生命周期（严格单向：Init→Connected→Streaming→Closed；`Detached` 与
/// `Reconnecting` 是 T17 的重连中间态——传输层断开不再等价于会话结束）。
///
/// - `Init`：端点已创建，传输未建立；
/// - `Connected`：QUIC 握手完成，Hello/Welcome 参数协商进行中；
/// - `Streaming`：参数已裁定，媒体/输入双向流动；
/// - `Detached`（host）：传输层已断，管线保留、保持窗口计时中，
///   短超时 accept 轮询等 `ReconnectRequest` 续传或全新 Hello；
/// - `Reconnecting`（client）：传输层已断，管线保留，按指数退避自动重连；
/// - `Closed`：Bye 已发/收或传输断开，且停机序列已完成。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// 端点已创建，传输未建立。
    Init,
    /// QUIC 握手完成，Hello/Welcome 参数协商进行中。
    Connected,
    /// 参数已裁定，媒体/输入双向流动。
    Streaming,
    /// host：传输层已断，管线保留、保持窗口计时中，短超时 accept 轮询
    /// （T17 增补；client 不进此态）。
    Detached,
    /// client：传输层已断，管线保留，按指数退避自动重连（T17 增补；
    /// host 不进此态）。
    Reconnecting,
    /// Bye 已发/收或传输断开，且停机序列已完成。
    Closed,
}

/// 会话结束原因（run/close 的摘要字段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// stop 闭包要求退出（到时 / 上层指令），本端随后发 Bye(Normal)。
    Requested,
    /// 对端发来 Bye（`Refresh` 语义的自动重连由 T17 在 tick 分发内接管，
    /// 不再走到停机；此变体只承载 `Normal` 关闭）。
    ByeReceived(ByeReason),
    /// 传输层断开（本卡不做重连，直接走停机序列）。
    ///
    /// 字符串也复用于「协议/注入错误导致的停机」的自由文本原因——
    /// 错误停机与断连对上层（T17 重连决策）是同一类输入。
    Disconnected(String),
}

#[cfg(test)]
mod tool_audio_pair_tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use rc_capture::{DisplayInfo, FakeBackend, FakeBackendConfig, FakeContent};
    use rc_decode::{FakeDecoder, FakeDecoderConfig};
    use rc_encode::{EncoderConfig, FakeEncoder};
    use rc_input::{InputCapture, InputEvent, InputInjector};
    use rc_pipeline::PipelineConfig;
    use rc_render::{FakeRenderer, FakeRendererConfig};
    use rc_types::{Error, Resolution, Result};

    use crate::proto::{
        AuthMethod, ControlMessage, Hello, SessionParams, PROTOCOL_VERSION, TYPE_AUDIO_FRAME,
        TYPE_TOOL_CHANNEL,
    };
    use crate::{
        pair, AudioFramePacket, ClientSession, ClientSessionConfig, HostSession, HostSessionConfig,
        MockTransport, SessionClock, SessionState, AUDIO_CODEC_IMA_ADPCM, AUDIO_CODEC_PCM16,
        MAX_AUDIO_FRAME_BYTES, MAX_AUDIO_SAMPLE_RATE, MAX_TOOL_PAYLOAD_BYTES,
        MIN_AUDIO_SAMPLE_RATE, TOOL_CHANNEL_AUDIO_CTRL, TOOL_CHANNEL_DISPLAY_CTRL,
        TOOL_CHANNEL_FILE, TOOL_CHANNEL_PORTMAP, TOOL_CHANNEL_TERMINAL,
    };

    struct NoopInjector;
    impl InputInjector for NoopInjector {
        fn inject(&mut self, _events: &[InputEvent]) -> Result<()> {
            Ok(())
        }
        fn set_screen_size(&mut self, _w: u32, _h: u32) {}
    }

    struct EmptyCapture;
    impl InputCapture for EmptyCapture {
        fn next_events(&mut self, _timeout: Duration) -> Result<Vec<InputEvent>> {
            Ok(Vec::new())
        }
    }

    fn test_params() -> SessionParams {
        SessionParams {
            width: 64,
            height: 64,
            fps: 30,
            bitrate_bps: 1_000_000,
        }
    }

    fn test_pcfg() -> PipelineConfig {
        PipelineConfig {
            resolution: Resolution::new(64, 64),
            fps: 30,
            bitrate_bps: 1_000_000,
            prefer_hardware: false,
            target_latency_us: 22_000,
            trace_capacity: 256,
            diff: rc_pipeline::DiffMode::TrustPlatform,
            clock_origin_us: None,
            max_packet_payload: rc_proto::MAX_PAYLOAD,
        }
    }

    fn zero_clock() -> SessionClock {
        Arc::new(|| 0)
    }

    fn fake_display() -> DisplayInfo {
        DisplayInfo {
            id: rc_types::DisplayId(0),
            resolution: Resolution::new(64, 64),
            scale: 1.0,
            hdr: false,
        }
    }

    fn make_host(transport: MockTransport) -> HostSession {
        let mut cfg = HostSessionConfig::new(
            "127.0.0.1:9901".parse().unwrap(),
            test_params(),
            Box::new(NoopInjector),
        );
        cfg.pipeline = test_pcfg();
        cfg.handshake_timeout = Duration::from_millis(200);
        cfg.pace_capture = true;

        let capture = Box::new(
            FakeBackend::new(FakeBackendConfig {
                display: fake_display(),
                fps: 30,
                content: FakeContent::Static,
                dirty_rect: None,
            })
            .expect("fake capture"),
        );
        let encoder = Box::new(
            FakeEncoder::with_tuning(
                EncoderConfig {
                    resolution: Resolution::new(64, 64),
                    fps: 30,
                    bitrate_bps: 1_000_000,
                    ..EncoderConfig::default()
                },
                500,
                128,
            )
            .expect("fake encoder"),
        );
        HostSession::with_parts(cfg, Box::new(transport), capture, encoder, zero_clock())
            .expect("host with_parts")
    }

    fn make_client(transport: MockTransport) -> ClientSession {
        let mut cfg = ClientSessionConfig::new(
            "127.0.0.1:9901".parse().unwrap(),
            test_params(),
            4242,
            Box::new(EmptyCapture),
        );
        cfg.pipeline = test_pcfg();
        cfg.handshake_timeout = Duration::from_millis(200);

        let decoder =
            Box::new(FakeDecoder::new(FakeDecoderConfig::default()).expect("fake decoder"));
        let renderer =
            Box::new(FakeRenderer::new(FakeRendererConfig::default()).expect("fake renderer"));
        ClientSession::with_parts(cfg, Box::new(transport), decoder, renderer, zero_clock())
            .expect("client with_parts")
    }

    fn handshaken_pair() -> (HostSession, ClientSession, MockTransport, MockTransport) {
        let (mut host_t, client_t) = pair();
        let host_handle = host_t.clone();
        let client_handle = client_t.clone();
        // 预置 Hello 使单线程下 host.handshake() 立即可产出 Welcome 给 client.handshake() 消费
        host_t.deliver_reliable(&ControlMessage::Hello(Hello {
            version: PROTOCOL_VERSION,
            session_id: 4242,
            requested: test_params(),
            auth_method: AuthMethod::None,
            auth_payload: vec![],
        }));

        let mut host = make_host(host_t);
        host.handshake().expect("host handshake");
        let mut client = make_client(client_t);
        client.handshake().expect("client handshake");
        // 排空 client.handshake() 实发到 host 的重复 Hello
        host.tick(Duration::ZERO)
            .expect("drain client hello after handshake");
        assert_eq!(host.state(), SessionState::Streaming);
        assert_eq!(client.state(), SessionState::Streaming);
        (host, client, host_handle, client_handle)
    }

    #[test]
    fn tool_channel_bidirectional_all_four_channels_over_pair() {
        let (mut host, mut client, host_handle, client_handle) = handshaken_pair();
        let now = Instant::now();

        let channels = [
            (TOOL_CHANNEL_FILE, b"file-chunk-001".to_vec()),
            (TOOL_CHANNEL_TERMINAL, b"pty-input-ls\n".to_vec()),
            (TOOL_CHANNEL_PORTMAP, vec![0x01, 0x00, 0xFF, 0x42]),
            (TOOL_CHANNEL_DISPLAY_CTRL, b"display-caps".to_vec()),
            (TOOL_CHANNEL_AUDIO_CTRL, b"mute=false".to_vec()),
        ];

        // Client -> Host across all 5 channels
        for (ch, payload) in &channels {
            client
                .send_tool_message(*ch, payload.clone())
                .expect("client send_tool_message");
        }
        // Host -> Client across all 5 channels
        for (ch, payload) in &channels {
            let mut reply = b"ack:".to_vec();
            reply.extend_from_slice(payload);
            host.send_tool_message(*ch, reply)
                .expect("host send_tool_message");
        }

        host.tick(Duration::ZERO).expect("host tick");
        client.tick(Duration::ZERO, now).expect("client tick");

        let host_got = host.drain_tool_messages();
        assert_eq!(host_got, channels.to_vec());
        assert!(host.drain_tool_messages().is_empty());

        let expected_client_got: Vec<(u8, Vec<u8>)> = channels
            .iter()
            .map(|(ch, p)| {
                let mut reply = b"ack:".to_vec();
                reply.extend_from_slice(p);
                (*ch, reply)
            })
            .collect();
        let client_got = client.drain_tool_messages();
        assert_eq!(client_got, expected_client_got);
        assert!(client.drain_tool_messages().is_empty());

        // 确认全走 reliable（Stream A），未污染 datagram / urgent
        assert!(client_handle
            .reliable_sent()
            .iter()
            .any(|b| b.first() == Some(&TYPE_TOOL_CHANNEL)));
        assert!(host_handle
            .reliable_sent()
            .iter()
            .any(|b| b.first() == Some(&TYPE_TOOL_CHANNEL)));
        assert!(client_handle
            .datagrams_sent()
            .iter()
            .all(|b| b.first() != Some(&TYPE_TOOL_CHANNEL)));
        assert!(client_handle
            .urgent_sent()
            .iter()
            .all(|b| b.first() != Some(&TYPE_TOOL_CHANNEL)));

        // 发送侧校验：非法通道 / 超大载荷
        assert!(matches!(
            client.send_tool_message(0, vec![1]),
            Err(Error::InvalidConfig(_))
        ));
        assert!(matches!(
            host.send_tool_message(6, vec![1]),
            Err(Error::InvalidConfig(_))
        ));
        assert!(matches!(
            client.send_tool_message(TOOL_CHANNEL_FILE, vec![0u8; MAX_TOOL_PAYLOAD_BYTES + 1]),
            Err(Error::InvalidConfig(_))
        ));

        // 接收队列 2048 有界验证（520 条全部保留不丢帧）+ 关闭时清空不泄漏
        for i in 0..520u32 {
            client
                .send_tool_message(TOOL_CHANNEL_FILE, i.to_le_bytes().to_vec())
                .expect("send within cap test");
        }
        host.tick(Duration::ZERO).expect("host tick 520");
        let drained = host.drain_tool_messages();
        assert_eq!(drained.len(), 520);
        assert_eq!(drained[0].1, 0u32.to_le_bytes().to_vec());
        assert_eq!(drained[519].1, 519u32.to_le_bytes().to_vec());
        assert_eq!(host.tool_rx_overflow_dropped(), 0);

        // 关闭后队列清空且不可发送
        client
            .send_tool_message(TOOL_CHANNEL_TERMINAL, b"pending".to_vec())
            .expect("send before close");
        host.tick(Duration::ZERO).expect("receive before close");
        host.close(crate::proto::ByeReason::Normal)
            .expect("host close");
        assert!(host.drain_tool_messages().is_empty());
        assert!(matches!(
            host.send_tool_message(TOOL_CHANNEL_FILE, vec![1]),
            Err(Error::InvalidConfig(_))
        ));
        client
            .close(crate::proto::ByeReason::Normal)
            .expect("client close");
    }

    #[test]
    fn audio_frame_host_to_client_with_drop_oldest_cap_32() {
        let (mut host, mut client, host_handle, _client_handle) = handshaken_pair();
        let now = Instant::now();

        // 发送 40 帧（超过 32 上限），验证 drop-oldest 仅保留最新 32 帧（seq 8..40）
        for seq in 0..40u32 {
            let codec = if seq % 2 == 0 {
                AUDIO_CODEC_PCM16
            } else {
                AUDIO_CODEC_IMA_ADPCM
            };
            host.send_audio_frame(AudioFramePacket {
                seq,
                timestamp_us: seq * 20_000,
                sample_rate: 48_000,
                channels: 2,
                codec,
                data: vec![seq as u8; 16],
            })
            .expect("host send_audio_frame");
        }

        client.tick(Duration::ZERO, now).expect("client tick audio");
        let frames = client.drain_audio_frames();
        assert_eq!(frames.len(), 32, "audio_rx 必须封顶 32 帧（drop-oldest）");
        for (idx, frame) in frames.iter().enumerate() {
            let expected_seq = 8 + idx as u32;
            assert_eq!(frame.seq, expected_seq);
            assert_eq!(frame.timestamp_us, expected_seq * 20_000);
            assert_eq!(frame.sample_rate, 48_000);
            assert_eq!(frame.channels, 2);
            assert_eq!(
                frame.codec,
                if expected_seq % 2 == 0 {
                    AUDIO_CODEC_PCM16
                } else {
                    AUDIO_CODEC_IMA_ADPCM
                }
            );
            assert_eq!(frame.data, vec![expected_seq as u8; 16]);
        }
        assert!(client.drain_audio_frames().is_empty());

        assert!(host_handle
            .reliable_sent()
            .iter()
            .any(|b| b.first() == Some(&TYPE_AUDIO_FRAME)));

        // 发送侧校验：sample_rate 越界 / channels=0 / codec=2 / PCM16 未对齐 / oversized
        for bad_rate in [
            0u32,
            4_000,
            MIN_AUDIO_SAMPLE_RATE - 1,
            MAX_AUDIO_SAMPLE_RATE + 1,
            u32::MAX,
        ] {
            assert!(matches!(
                host.send_audio_frame(AudioFramePacket {
                    seq: 1,
                    timestamp_us: 0,
                    sample_rate: bad_rate,
                    channels: 2,
                    codec: AUDIO_CODEC_PCM16,
                    data: vec![],
                }),
                Err(Error::InvalidConfig(_))
            ));
        }
        assert!(matches!(
            host.send_audio_frame(AudioFramePacket {
                seq: 1,
                timestamp_us: 0,
                sample_rate: 48_000,
                channels: 2,
                codec: AUDIO_CODEC_PCM16,
                data: vec![1, 2, 3],
            }),
            Err(Error::InvalidConfig(_))
        ));
        assert!(matches!(
            host.send_audio_frame(AudioFramePacket {
                seq: 1,
                timestamp_us: 0,
                sample_rate: 48_000,
                channels: 0,
                codec: AUDIO_CODEC_PCM16,
                data: vec![],
            }),
            Err(Error::InvalidConfig(_))
        ));
        assert!(matches!(
            host.send_audio_frame(AudioFramePacket {
                seq: 1,
                timestamp_us: 0,
                sample_rate: 48_000,
                channels: 2,
                codec: 2,
                data: vec![],
            }),
            Err(Error::InvalidConfig(_))
        ));
        assert!(matches!(
            host.send_audio_frame(AudioFramePacket {
                seq: 1,
                timestamp_us: 0,
                sample_rate: 48_000,
                channels: 2,
                codec: AUDIO_CODEC_PCM16,
                data: vec![0u8; MAX_AUDIO_FRAME_BYTES + 1],
            }),
            Err(Error::InvalidConfig(_))
        ));

        // 关闭后音频队列清空不泄漏
        host.send_audio_frame(AudioFramePacket {
            seq: 99,
            timestamp_us: 99,
            sample_rate: 48_000,
            channels: 1,
            codec: AUDIO_CODEC_PCM16,
            data: vec![1, 2],
        })
        .expect("send before close");
        client.tick(Duration::ZERO, now).expect("recv before close");
        client
            .close(crate::proto::ByeReason::Normal)
            .expect("client close");
        assert!(client.drain_audio_frames().is_empty());
        host.close(crate::proto::ByeReason::Normal)
            .expect("host close");
    }

    #[test]
    fn tool_channel_and_audio_frame_before_streaming_rejected_with_protocol_error() {
        let tool_msg = ControlMessage::ToolChannel {
            channel: TOOL_CHANNEL_FILE,
            payload: b"early".to_vec(),
        };
        let audio_msg = ControlMessage::AudioFrame {
            seq: 1,
            timestamp_us: 100,
            sample_rate: 48_000,
            channels: 2,
            codec: AUDIO_CODEC_PCM16,
            data: vec![0u8; 8],
        };

        // 1) Host 在握手期（Hello 前）收到 ToolChannel / AudioFrame -> Error::Protocol
        for early in [&tool_msg, &audio_msg] {
            let (mut host_t, _client_t) = pair();
            host_t.deliver_reliable(early);
            let mut host = make_host(host_t);
            let err = host
                .handshake()
                .expect_err("host must reject early tool/audio before Hello");
            assert!(
                matches!(err, Error::Protocol { .. }),
                "expected Error::Protocol, got {err:?}"
            );
            assert_eq!(host.state(), SessionState::Closed);
        }

        // 2) Host 在密码鉴权挑战期（已收 Hello，等待 AuthResponse）收到 ToolChannel / AudioFrame -> Error::Protocol
        for early in [&tool_msg, &audio_msg] {
            let (mut host_t, _client_t) = pair();
            host_t.deliver_reliable(&ControlMessage::Hello(Hello {
                version: PROTOCOL_VERSION,
                session_id: 4242,
                requested: test_params(),
                auth_method: AuthMethod::Password,
                auth_payload: vec![],
            }));
            host_t.deliver_reliable(early);

            let mut cfg = HostSessionConfig::new(
                "127.0.0.1:9901".parse().unwrap(),
                test_params(),
                Box::new(NoopInjector),
            );
            cfg.pipeline = test_pcfg();
            cfg.handshake_timeout = Duration::from_millis(200);
            cfg.pace_capture = true;
            cfg.auth.password = Some("s3cr3t".into());

            let capture = Box::new(
                FakeBackend::new(FakeBackendConfig {
                    display: fake_display(),
                    fps: 30,
                    content: FakeContent::Static,
                    dirty_rect: None,
                })
                .expect("fake capture"),
            );
            let encoder = Box::new(
                FakeEncoder::with_tuning(
                    EncoderConfig {
                        resolution: Resolution::new(64, 64),
                        fps: 30,
                        bitrate_bps: 1_000_000,
                        ..EncoderConfig::default()
                    },
                    500,
                    128,
                )
                .expect("fake encoder"),
            );
            let mut host =
                HostSession::with_parts(cfg, Box::new(host_t), capture, encoder, zero_clock())
                    .expect("host with_parts");
            let err = host
                .handshake()
                .expect_err("host must reject early tool/audio during auth");
            assert!(
                matches!(err, Error::Protocol { .. }),
                "expected Error::Protocol, got {err:?}"
            );
            assert_eq!(host.state(), SessionState::Closed);
        }

        // 3) Client 在握手期（等待 Welcome）收到 ToolChannel / AudioFrame -> Error::Protocol
        for early in [&tool_msg, &audio_msg] {
            let (_host_t, mut client_t) = pair();
            client_t.deliver_reliable(early);
            let mut client = make_client(client_t);
            let err = client
                .handshake()
                .expect_err("client must reject early tool/audio before Welcome");
            assert!(
                matches!(err, Error::Protocol { .. }),
                "expected Error::Protocol, got {err:?}"
            );
            assert_eq!(client.state(), SessionState::Closed);
        }
    }

    #[test]
    fn audio_ctrl_channel_isolated_latest_wins_under_bulk_tool_burst() {
        let (mut host, mut client, _host_handle, _client_handle) = handshaken_pair();
        let now = Instant::now();

        // Fill reliable tool_rx right up to the 2048 cap with bulk FILE & PORTMAP frames.
        for i in 0..2048u32 {
            let ch = if i % 2 == 0 {
                TOOL_CHANNEL_FILE
            } else {
                TOOL_CHANNEL_PORTMAP
            };
            client
                .send_tool_message(ch, i.to_le_bytes().to_vec())
                .expect("send bulk within 2048 cap");
        }
        // Send multiple AUDIO_CTRL updates while tool_rx is full: must NOT overflow, latest wins.
        client
            .send_tool_message(TOOL_CHANNEL_AUDIO_CTRL, b"mute=true".to_vec())
            .expect("send audio ctrl 1");
        client
            .send_tool_message(TOOL_CHANNEL_AUDIO_CTRL, b"mute=false".to_vec())
            .expect("send audio ctrl 2");

        host.tick(Duration::ZERO)
            .expect("host tick with full 2048 bulk + audio ctrl must succeed");
        assert_eq!(host.tool_rx_overflow_dropped(), 0);

        let drained = host.drain_tool_messages();
        assert_eq!(drained.len(), 2049, "2048 bulk + 1 latest audio ctrl");
        assert_eq!(
            drained.last(),
            Some(&(TOOL_CHANNEL_AUDIO_CTRL, b"mute=false".to_vec()))
        );
        assert!(host.drain_tool_messages().is_empty());

        // Symmetric check on ClientSession side.
        for i in 0..2048u32 {
            host.send_tool_message(TOOL_CHANNEL_FILE, i.to_le_bytes().to_vec())
                .expect("host send bulk");
        }
        host.send_tool_message(TOOL_CHANNEL_AUDIO_CTRL, b"vol=0.2".to_vec())
            .expect("host audio ctrl 1");
        host.send_tool_message(TOOL_CHANNEL_AUDIO_CTRL, b"vol=0.8".to_vec())
            .expect("host audio ctrl 2");

        client
            .tick(Duration::ZERO, now)
            .expect("client tick with full 2048 bulk + audio ctrl must succeed");
        assert_eq!(client.tool_rx_overflow_dropped(), 0);
        let c_drained = client.drain_tool_messages();
        assert_eq!(c_drained.len(), 2049);
        assert_eq!(
            c_drained.last(),
            Some(&(TOOL_CHANNEL_AUDIO_CTRL, b"vol=0.8".to_vec()))
        );
    }

    #[test]
    fn tool_rx_overflow_returns_protocol_error_and_increments_counter() {
        // Host side overflow
        let (mut host, mut client, _host_handle, _client_handle) = handshaken_pair();
        for i in 0..2049u32 {
            client
                .send_tool_message(TOOL_CHANNEL_FILE, i.to_le_bytes().to_vec())
                .expect("send bulk");
        }
        let err = host
            .tick(Duration::ZERO)
            .expect_err("2049th reliable tool frame must trigger Error::Protocol");
        assert!(
            matches!(err, Error::Protocol { ref msg, .. } if msg.contains("tool channel queue overflow")),
            "unexpected error: {err:?}"
        );
        assert_eq!(host.tool_rx_overflow_dropped(), 1);
        assert_eq!(host.state(), SessionState::Closed);
        assert!(host.drain_tool_messages().is_empty());

        // Client side overflow
        let (mut host, mut client, _host_handle, _client_handle) = handshaken_pair();
        let now = Instant::now();
        for i in 0..2049u32 {
            host.send_tool_message(TOOL_CHANNEL_PORTMAP, i.to_le_bytes().to_vec())
                .expect("host send bulk");
        }
        let err = client
            .tick(Duration::ZERO, now)
            .expect_err("2049th reliable tool frame on client must trigger Error::Protocol");
        assert!(
            matches!(err, Error::Protocol { ref msg, .. } if msg.contains("tool channel queue overflow")),
            "unexpected error: {err:?}"
        );
        assert_eq!(client.tool_rx_overflow_dropped(), 1);
        assert_eq!(client.state(), SessionState::Closed);
        assert!(client.drain_tool_messages().is_empty());
    }

    #[test]
    fn tool_rx_and_audio_ctrl_cleared_on_disconnect() {
        let (mut host, mut client, _host_handle, _client_handle) = handshaken_pair();
        let now = Instant::now();

        client
            .send_tool_message(TOOL_CHANNEL_FILE, b"stale-file".to_vec())
            .expect("client send file");
        client
            .send_tool_message(TOOL_CHANNEL_AUDIO_CTRL, b"mute=true".to_vec())
            .expect("client send audio ctrl");
        host.send_tool_message(TOOL_CHANNEL_TERMINAL, b"stale-tty".to_vec())
            .expect("host send terminal");
        host.send_tool_message(TOOL_CHANNEL_AUDIO_CTRL, b"mute=false".to_vec())
            .expect("host send audio ctrl");

        host.tick(Duration::ZERO).expect("host tick");
        client.tick(Duration::ZERO, now).expect("client tick");

        // Disconnect before draining: both tool_rx and audio_ctrl_rx must be cleared.
        host.on_disconnected(1_000_000);
        assert_eq!(host.state(), SessionState::Detached);
        assert!(
            host.drain_tool_messages().is_empty(),
            "host tool_rx and audio_ctrl_rx must be cleared on disconnect"
        );

        client.on_disconnected(1_000_000);
        assert_eq!(client.state(), SessionState::Reconnecting);
        assert!(
            client.drain_tool_messages().is_empty(),
            "client tool_rx and audio_ctrl_rx must be cleared on disconnect"
        );
    }
}
