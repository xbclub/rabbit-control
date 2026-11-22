//! 输入通道接线（I1）：批处理 ⇆ rc-net urgent reliable stream ⇆ 注入。
//!
//! # 通道选择（以 rc-net 实际代码为准）
//!
//! rc-net 的 [`Transport`](rc_net::Transport) 提供三条互不共享队列的
//! 通道：媒体走 `send_datagram`（不可靠、经 pacing 队列）、控制消息走
//! `send_reliable`（可靠有序流），**输入走 `send_reliable_urgent`**
//! （独立第二条可靠流，专职写出线程）——输入消息既不排在视频帧之后，
//! 也不排在控制消息之后（同连接控制流量再大也队头阻塞不了输入）。
//! 两端用法：
//!
//! - 控制端：[`InputChannel::pump_sender`] —— 捕获 → 批处理 →
//!   [`encode_events`](crate::encode_events) → `send_reliable_urgent`；
//! - 受控端：[`InputChannel::pump_receiver`] —— `poll` 取
//!   [`NetEvent::ReliableUrgent`] → [`decode_events`](crate::decode_events)
//!   → [`InputInjector::inject`]，单条解码/注入失败只记错不弃批
//!   （错误隔离见该方法文档，松键绝不随批蒸发）。控制通道的
//!   `NetEvent::Reliable`
//!   归会话层消费，本模块**不碰**（混进来也不会被注入）。
//!
//! # 成帧关系（与任务卡线格式的出入）
//!
//! 任务卡的线格式自带 `[len(u16 LE)]` 消息前缀；rc-net 的
//! `send_reliable` 在传输层**又**用 `u32 LE` 长度前缀成帧并按消息
//! 边界交付（`NetEvent::Reliable` 一条 = 一条消息）。两层前缀叠加
//! 无害且各有职责：u16 前缀让事件段可以脱离 rc-net 独立校验长度
//! （录制 / 回放 / 其他传输），u32 前缀保证 QUIC 流上的消息边界。
//!
//! 批的事件段超过单条消息 u16 上限（65535 字节）时，发送路径按事件
//! 边界**自动分片**为多条消息（[`send_events`]），每条消息独立可解，
//! 接收端逐消息解码注入即可无损还原——超长批不再产生长度前缀饱和、
//! 对端必拒的消息。
//!
//! # 历史坑存档（已闭环）
//!
//! 早期 rc-net 每连接只有一条 reliable 流，控制消息与输入共享、互相
//! 队头阻塞。修复轮（2026-11-21）给 rc-net 加了独立的 urgent 流并把
//! 本模块接线切过去；接缝由 `input_goes_over_urgent_stream_not_control`
//! 测试钉死——输入若退回共享流，该测试当场红。
//!
//! 同轮审计还发现 `pump_receiver` 用 `?` 提前返回会丢弃 `poll` 局部
//! `Vec` 里同批剩余消息（松键被吞 = 用户端卡键）。修法取「逐条处理、
//! 批尾返首个错误」而非缓存重试：失败消息本身不可恢复，缓存只会让
//! 毒消息无限重试。行为由
//! `dirty_message_does_not_drop_sibling_messages_in_batch` 与
//! `inject_failure_does_not_drop_sibling_messages_in_batch` 钉死。

use crate::{
    decode_events, encode_events, event_wire_size, InputBatcher, InputCapture, InputEvent,
    InputInjector,
};
use rc_net::{NetEvent, Transport};
use rc_types::Result;
use std::time::{Duration, Instant};

/// 单条消息事件段的最大字节数：`len` 前缀是 u16，事件段必须 ≤ 65535。
const MAX_EVENT_SEGMENT: usize = u16::MAX as usize;

/// 控制端 / 受控端共用的输入通道会话半边。
///
/// 本结构只持批处理器状态（无 IO、无线程）；transport / capture /
/// injector 由调用方持有并按引用传入，便于上层自由组装会话。
#[derive(Debug)]
pub struct InputChannel {
    batcher: InputBatcher,
}

impl InputChannel {
    /// 构造。`window` 是批窗口，建议 [`DEFAULT_WINDOW`](crate::DEFAULT_WINDOW)（4ms）。
    pub fn new(window: Duration) -> Self {
        Self {
            batcher: InputBatcher::new(window),
        }
    }

    /// 控制端一拍：捕获一批事件 → 批处理 → 编码 → `send_reliable_urgent`。
    ///
    /// `now` 是本拍时刻（批窗口判断依据，注入式时间）。返回发出的
    /// 全部消息字节总数（超长批被 [`send_events`] 分片时为多条消息
    /// 之和）；本拍没有到期批次返回 0 且**不发送任何消息**
    /// （空批不产生消息，见 `empty_batch_produces_no_message`）。
    /// 注入失败类错误（I9）在此路径上是发送错误，原样上抛。
    pub fn pump_sender(
        &mut self,
        capture: &mut dyn InputCapture,
        transport: &mut dyn Transport,
        now: Instant,
        timeout: Duration,
    ) -> Result<usize> {
        for ev in capture.next_events(timeout)? {
            self.batcher.push(ev, now);
        }
        let events = self.batcher.poll(now);
        send_events(transport, &events)
    }

    /// 受控端一拍：`poll` 传输层 → 解码每条 `ReliableUrgent` 消息 → 注入。
    ///
    /// 只消费输入专用的 urgent 通道消息；控制通道的 `Reliable`、媒体
    /// `Datagram` 等本拍忽略——前者归会话层，后者归视频管线。
    ///
    /// # 错误隔离：单条失败不丢同批兄弟消息
    ///
    /// `poll` 一次取回一整批消息（局部 `Vec`，提前返回即整批蒸发）。
    /// 任一条消息解码或注入失败都**不提前返回**：失败只记录，本批
    /// 剩余消息（可能含松键——丢了就是用户端永久卡键）继续解码注入，
    /// 批处理完后返回**首个**错误（I9：返回错误，不 panic）。失败的
    /// 那条消息本身不可恢复（脏数据解不出、注入已试过），就地丢弃、
    /// 只报这一次错：不缓存重试，永久性脏消息不会无限阻塞流。部分
    /// 成功的批返回 `Err` 时成功计数不可用，调用方按需补发 `Sync` 校准。
    pub fn pump_receiver(
        &mut self,
        transport: &mut dyn Transport,
        injector: &mut dyn InputInjector,
        timeout: Duration,
    ) -> Result<usize> {
        let mut injected = 0usize;
        let mut first_err: Option<rc_types::Error> = None;
        for ev in transport.poll(timeout)? {
            if let NetEvent::ReliableUrgent(msg) = ev {
                // 逐条注入：任一失败 → 记首个错误，继续下一条（语义与
                // [`inject_message`] 完全一致，见其文档）。
                match inject_message(&msg, injector) {
                    Ok(n) => injected += n,
                    Err(e) => {
                        first_err.get_or_insert(e);
                    }
                }
            }
        }
        first_err.map_or(Ok(injected), Err)
    }

    /// 内部批处理器的只读访问（诊断 / 测试）。
    pub fn batcher(&self) -> &InputBatcher {
        &self.batcher
    }
}

/// 解码并注入**一条** urgent 消息（[`InputChannel::pump_receiver`] 循环体
/// 的提取）。
///
/// 为什么提取：`pump_receiver` 内部自己 `transport.poll`，同批到达的非
/// urgent 事件（控制消息 / `Disconnected`）会被静默丢弃——会话层必须
/// **独占** poll 做三通道分发，urgent 注入改走本函数，逐条调用。错误
/// 隔离语义与 `pump_receiver` 完全一致：单条消息失败返回 `Err`、**不
/// 注入半批**（解码失败时整条消息的事件都不注入），成功时返回注入的
/// 事件数。`pump_receiver` 内部循环调用本函数，行为不变。
pub fn inject_message(msg: &[u8], injector: &mut dyn InputInjector) -> Result<usize> {
    let events = decode_events(msg)?;
    injector.inject(&events)?;
    Ok(events.len())
}

/// 把一批事件编码成一个或多个消息并通过输入专用的 urgent reliable
/// 流发送（I1：不与媒体 pacing 队列、也不与控制消息共享流）。
///
/// 一批事件的事件段可能超过单条消息的 u16 长度上限（如积压后一次性
/// flush 出两万条按键，事件段 ≈100KB）：此时**自动分片**——按事件
/// 边界切成多个事件段 ≤ 65535 字节的 `[len][events]` 消息逐条发送，
/// 每条消息都能被 [`decode_events`](crate::decode_events) 独立解码，
/// 接收端（[`InputChannel::pump_receiver`]）逐消息解码注入即可无损
/// 还原，顺序不变。批未超限时仍是恰好一条消息，单事件/小批的消息
/// 边界不受影响。
///
/// 空批直接返回 0，不产生消息。返回值是发出的全部消息字节总数。
pub fn send_events(transport: &mut dyn Transport, events: &[InputEvent]) -> Result<usize> {
    if events.is_empty() {
        return Ok(0);
    }
    // 单事件线格式最大 5 字节，恒 ≤ MAX_EVENT_SEGMENT，
    // 因此「片已装 1 条事件装不下下一条」⇒ 先发当前片即可，不会卡死。
    let mut total = 0usize;
    let mut buf = Vec::with_capacity(2 + events.len() * 5);
    let mut start = 0usize; // 当前片在 events 里的起点
    let mut seg = 0usize; // 当前片的事件段字节数
    for (i, ev) in events.iter().enumerate() {
        let size = event_wire_size(ev);
        debug_assert!(
            size <= MAX_EVENT_SEGMENT,
            "单事件线格式 {size} 字节超过消息事件段上限（不可能）"
        );
        if seg > 0 && seg + size > MAX_EVENT_SEGMENT {
            buf.clear();
            encode_events(&events[start..i], &mut buf);
            transport.send_reliable_urgent(&buf)?;
            total += buf.len();
            start = i;
            seg = 0;
        }
        seg += size;
    }
    buf.clear();
    encode_events(&events[start..], &mut buf);
    transport.send_reliable_urgent(&buf)?;
    total += buf.len();
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MouseButton, DEFAULT_WINDOW};
    use rc_net::NetStats;
    use rc_types::Error;

    /// 记录型假传输：不碰网络，回放脚本化事件。
    #[derive(Default)]
    struct MockTransport {
        reliable_sent: Vec<Vec<u8>>,
        urgent_sent: Vec<Vec<u8>>,
        datagrams_sent: Vec<Vec<u8>>,
        deliver: Vec<NetEvent>,
    }

    impl Transport for MockTransport {
        fn send_datagram(&mut self, data: &[u8]) -> Result<()> {
            self.datagrams_sent.push(data.to_vec());
            Ok(())
        }
        fn send_reliable(&mut self, data: &[u8]) -> Result<()> {
            self.reliable_sent.push(data.to_vec());
            Ok(())
        }
        // 覆写而不是吃默认实现：默认会委托 send_reliable，测试就证明
        // 不了输入真的走了独立 urgent 流（接缝测试的关键）。
        fn send_reliable_urgent(&mut self, data: &[u8]) -> Result<()> {
            self.urgent_sent.push(data.to_vec());
            Ok(())
        }
        fn poll(&mut self, _timeout: Duration) -> Result<Vec<NetEvent>> {
            Ok(std::mem::take(&mut self.deliver))
        }
        fn stats(&self) -> NetStats {
            NetStats::default()
        }
    }

    /// 记录型假注入器；可切换为必失败（验证 I9 传播）。
    #[derive(Default)]
    struct MockInjector {
        events: Vec<InputEvent>,
        fail: bool,
        screen: (u32, u32),
    }

    impl InputInjector for MockInjector {
        fn inject(&mut self, events: &[InputEvent]) -> Result<()> {
            if self.fail {
                return Err(Error::InvalidConfig("inject failed".into()));
            }
            self.events.extend_from_slice(events);
            Ok(())
        }
        fn set_screen_size(&mut self, w: u32, h: u32) {
            self.screen = (w, h);
        }
    }

    /// 脚本型假捕获器：按队列吐事件。
    struct MockCapture {
        scripted: Vec<InputEvent>,
    }

    impl InputCapture for MockCapture {
        fn next_events(&mut self, _timeout: Duration) -> Result<Vec<InputEvent>> {
            Ok(std::mem::take(&mut self.scripted))
        }
    }

    /// I1+I2+I7 端到端：一拍内 8 次移动 + 1 次点击 → 恰好 1 条
    /// reliable 消息（含最新移动 + 点击），接收端解码注入零丢失。
    #[test]
    fn controller_batch_flows_over_reliable_stream_to_injector() {
        let mut scripted = Vec::new();
        for i in 0..8u16 {
            scripted.push(InputEvent::MouseMove {
                x: 1000 + i,
                y: 2000 + i,
            });
        }
        scripted.push(InputEvent::MouseButton {
            button: MouseButton::Left,
            pressed: true,
        });
        let mut capture = MockCapture { scripted };
        let mut transport = MockTransport::default();
        let mut injector = MockInjector::default();
        let mut channel = InputChannel::new(DEFAULT_WINDOW);

        let t0 = Instant::now();
        let sent = channel
            .pump_sender(&mut capture, &mut transport, t0, Duration::from_millis(0))
            .expect("send pump");
        // 点击是关键事件 → 立即成批：1 条消息，绝不发 8 条
        assert_eq!(transport.urgent_sent.len(), 1, "一批必须只发一条消息");
        assert!(transport.reliable_sent.is_empty(), "输入绝不走共享控制流");
        assert_eq!(transport.datagrams_sent.len(), 0, "输入绝不走媒体 datagram");
        assert_eq!(sent, transport.urgent_sent[0].len());

        // 回环交付：同一条消息解码注入
        transport.deliver = transport
            .urgent_sent
            .drain(..)
            .map(NetEvent::ReliableUrgent)
            .collect();
        let injected = channel
            .pump_receiver(&mut transport, &mut injector, Duration::from_millis(0))
            .expect("recv pump");
        assert_eq!(injected, 2);
        assert_eq!(
            injector.events,
            vec![
                InputEvent::MouseMove { x: 1007, y: 2007 },
                InputEvent::MouseButton {
                    button: MouseButton::Left,
                    pressed: true,
                },
            ],
            "移动合并为最新值且点击保序在后"
        );
    }

    /// 空捕获不产生任何消息（空批边界）。
    #[test]
    fn channel_sends_nothing_when_capture_is_idle() {
        let mut capture = MockCapture {
            scripted: Vec::new(),
        };
        let mut transport = MockTransport::default();
        let mut channel = InputChannel::new(DEFAULT_WINDOW);
        let t0 = Instant::now();
        let n = channel
            .pump_sender(&mut capture, &mut transport, t0, Duration::from_millis(0))
            .unwrap();
        assert_eq!(n, 0);
        assert!(transport.urgent_sent.is_empty());
        assert!(transport.reliable_sent.is_empty());
        assert!(transport.datagrams_sent.is_empty());
    }

    /// 验收标准「1000Hz 鼠标 ≤ 300 包/秒」的离线确定性证明：
    /// 模拟 1000Hz 移动持续 1 秒、控制端每 1ms 一拍，消息数必须
    /// ≤ 300（4ms 窗口理论值 250）。时间全部注入，无真实等待。
    #[test]
    fn thousand_hz_mouse_stays_under_300_msgs_per_second() {
        let mut capture = MockCapture {
            scripted: Vec::new(),
        };
        let mut transport = MockTransport::default();
        let mut channel = InputChannel::new(DEFAULT_WINDOW);
        let t0 = Instant::now();
        let tick = Duration::from_millis(1);
        // 1000 拍 = 1 秒，每拍 1 条移动（等效 1000Hz 鼠标）
        for i in 0..1000u16 {
            capture.scripted = vec![InputEvent::MouseMove { x: i, y: i }];
            channel
                .pump_sender(&mut capture, &mut transport, t0 + tick * u32::from(i), tick)
                .expect("pump");
        }
        assert!(
            !transport.urgent_sent.is_empty(),
            "必须有消息发出（否则没在发送输入）"
        );
        assert!(
            transport.urgent_sent.len() <= 300,
            "1000Hz 鼠标 1 秒发出 {} 条消息，超过 300 上限（I2/I6 失效）",
            transport.urgent_sent.len()
        );
    }

    /// 超长批自动分片（审计遗留修复）：约 2 万条 5 字节事件
    /// （事件段 100000 字节 ≈ 100KB，远超单消息 u16::MAX 上限）一批
    /// → 切成多条事件段 ≤ 65535 字节的消息，每条独立可解；逐消息解码
    /// 合并后与原序列逐项相等、顺序不变。修复前一批会编码出 len 饱和
    /// 为 u16::MAX 的单条消息，长度前缀与实际字节数对不上，接收端必拒。
    #[test]
    fn oversized_batch_is_split_into_independently_decodable_messages() {
        // 20000 × 5 字节 = 100000 字节事件段 > 65535
        let events: Vec<InputEvent> = (0..20000u32)
            .map(|i| InputEvent::MouseMove {
                x: i as u16,
                y: (i * 7 % 65536) as u16,
            })
            .collect();
        let mut transport = MockTransport::default();
        let sent = send_events(&mut transport, &events).expect("send");

        assert!(
            transport.urgent_sent.len() >= 2,
            "≈100KB 事件段必须分片为多条消息，实际 {} 条",
            transport.urgent_sent.len()
        );
        let mut merged = Vec::with_capacity(events.len());
        let mut total = 0usize;
        for msg in &transport.urgent_sent {
            let seg = u16::from_le_bytes([msg[0], msg[1]]) as usize;
            assert_eq!(
                msg.len(),
                2 + seg,
                "长度前缀必须等于实际事件段字节数（不饱和、不缺斤短两）"
            );
            merged.extend(decode_events(msg).expect("分片后每条消息必须独立可解"));
            total += msg.len();
        }
        assert_eq!(total, sent, "返回值必须是全部消息字节总数");
        assert_eq!(
            merged, events,
            "逐消息解码合并后必须与原序列逐项相等、顺序不变"
        );

        // 边界：恰好 65535 字节（13107 × 5）仍是单条消息；再 +1 事件才分片
        let exact: Vec<InputEvent> = (0..13107u32)
            .map(|i| InputEvent::MouseMove { x: i as u16, y: 0 })
            .collect();
        let mut t_exact = MockTransport::default();
        send_events(&mut t_exact, &exact).expect("send");
        assert_eq!(
            t_exact.urgent_sent.len(),
            1,
            "恰好 65535 字节事件段应单条消息"
        );

        let one_more: Vec<InputEvent> = (0..13108u32)
            .map(|i| InputEvent::MouseMove { x: i as u16, y: 0 })
            .collect();
        let mut t_over = MockTransport::default();
        send_events(&mut t_over, &one_more).expect("send");
        assert_eq!(t_over.urgent_sent.len(), 2, "超出 1 字节即分 2 条");

        // 单事件 / 小批的消息边界不受分片影响：仍是恰好一条消息
        let single = vec![InputEvent::Key {
            scancode: 0x1E,
            pressed: true,
            modifiers: 0,
        }];
        let mut t_single = MockTransport::default();
        let n = send_events(&mut t_single, &single).expect("send");
        assert_eq!(t_single.urgent_sent.len(), 1);
        assert_eq!(n, 7, "2 前缀 + 5 字节 Key 事件");
        assert_eq!(
            decode_events(&t_single.urgent_sent[0]).unwrap(),
            single,
            "单事件消息原样可解"
        );
    }

    /// 全链版本：pump_sender 一拍积压两万条按键（关键事件，批处理全保留，
    /// 事件段 ≈100KB）→ 发送路径自动分片 → pump_receiver 逐消息解码
    /// 注入，零丢失、顺序不变。
    #[test]
    fn oversized_batch_roundtrips_through_channel_pumps() {
        // scancode 0x1E/0x1F 都不是修饰键，modifiers 快照恒 0，
        // 预期序列即脚本原样。
        let scripted: Vec<InputEvent> = (0..20000u32)
            .map(|i| InputEvent::Key {
                scancode: 0x1E + (i % 2) as u16,
                pressed: i % 2 == 0,
                modifiers: 0,
            })
            .collect();
        let mut capture = MockCapture {
            scripted: scripted.clone(),
        };
        let mut transport = MockTransport::default();
        let mut injector = MockInjector::default();
        let mut channel = InputChannel::new(DEFAULT_WINDOW);

        let t0 = Instant::now();
        let sent = channel
            .pump_sender(&mut capture, &mut transport, t0, Duration::from_millis(0))
            .expect("send pump");
        assert!(
            transport.urgent_sent.len() >= 2,
            "两万条按键必须分片，实际 {} 条消息",
            transport.urgent_sent.len()
        );
        assert_eq!(
            sent,
            transport.urgent_sent.iter().map(|m| m.len()).sum::<usize>(),
            "pump_sender 返回全部消息字节总数"
        );

        // 回环交付：逐消息解码注入
        transport.deliver = transport
            .urgent_sent
            .drain(..)
            .map(NetEvent::ReliableUrgent)
            .collect();
        let injected = channel
            .pump_receiver(&mut transport, &mut injector, Duration::from_millis(0))
            .expect("recv pump");
        assert_eq!(injected, 20000, "分片重组后零丢失");
        assert_eq!(
            injector.events, scripted,
            "注入序列与原序列逐项相等、顺序不变"
        );
    }

    /// I9 端到端：注入失败 / 脏消息都以 `Err` 返回，不 panic。
    #[test]
    fn channel_receiver_errors_are_returned_not_panicked() {
        let mut transport = MockTransport {
            deliver: vec![NetEvent::ReliableUrgent(vec![1, 0, 0x99])], // 未知类型
            ..Default::default()
        };
        let mut injector = MockInjector::default();
        let mut channel = InputChannel::new(DEFAULT_WINDOW);
        let err = channel
            .pump_receiver(&mut transport, &mut injector, Duration::from_millis(0))
            .expect_err("脏消息必须报错");
        assert!(matches!(err, Error::Protocol { .. }));
        assert!(injector.events.is_empty(), "解码失败时不得注入任何事件");

        // 注入器自身失败同样上抛（合法 Sync 消息 + 必失败注入器）
        transport.deliver = vec![NetEvent::ReliableUrgent(vec![1, 0, crate::TYPE_SYNC])];
        let mut failing = MockInjector {
            fail: true,
            ..Default::default()
        };
        assert!(channel
            .pump_receiver(&mut transport, &mut failing, Duration::from_millis(0))
            .is_err());
    }

    /// 接缝钉死（复审发现的修复）：输入生产路径必须走独立 urgent 流，
    /// 绝不退回共享控制流；接收端对控制通道消息（`Reliable`）视而不见。
    #[test]
    fn input_goes_over_urgent_stream_not_control() {
        let mut capture = MockCapture {
            scripted: vec![
                InputEvent::MouseMove { x: 123, y: 456 },
                InputEvent::MouseButton {
                    button: MouseButton::Left,
                    pressed: true,
                },
            ],
        };
        let mut transport = MockTransport::default();
        let mut channel = InputChannel::new(DEFAULT_WINDOW);
        let t0 = Instant::now();
        channel
            .pump_sender(&mut capture, &mut transport, t0, Duration::from_millis(0))
            .expect("send pump");
        assert!(
            !transport.urgent_sent.is_empty(),
            "输入消息必须走 send_reliable_urgent"
        );
        assert!(
            transport.reliable_sent.is_empty(),
            "输入消息退回共享控制流 = 队头阻塞回归（N6/I1）"
        );

        // 接收侧：控制通道消息不被当输入注入（哪怕字节恰好像事件消息）
        let mut injector = MockInjector::default();
        let mut ctl_transport = MockTransport {
            deliver: vec![NetEvent::Reliable(vec![1, 0, crate::TYPE_SYNC])],
            ..Default::default()
        };
        let n = channel
            .pump_receiver(&mut ctl_transport, &mut injector, Duration::from_millis(0))
            .expect("控制消息必须被忽略而不是报错");
        assert_eq!(n, 0, "控制通道不产出注入");
        assert!(injector.events.is_empty());
    }

    /// 第 N 次调用必失败的注入器（验证单条注入失败不吞兄弟消息）。
    struct FlakyInjector {
        events: Vec<InputEvent>,
        fail_on_call: usize,
        calls: usize,
    }

    impl InputInjector for FlakyInjector {
        fn inject(&mut self, events: &[InputEvent]) -> Result<()> {
            self.calls += 1;
            if self.calls == self.fail_on_call {
                return Err(Error::InvalidConfig("flaky inject".into()));
            }
            self.events.extend_from_slice(events);
            Ok(())
        }
        fn set_screen_size(&mut self, _w: u32, _h: u32) {}
    }

    /// 审计修复：一批 3 条消息中第 2 条是脏消息（未知事件类型）时，
    /// 第 1、3 条必须仍被处理——尤其第 3 条是松键，被 `?` 提前返回
    /// 吞掉就是用户端永久卡键——且函数返回错误。修复前第 2 条的
    /// `?` 会丢弃局部 Vec 里剩余全部消息。
    #[test]
    fn dirty_message_does_not_drop_sibling_messages_in_batch() {
        let key_down = InputEvent::Key {
            scancode: 0x1E,
            pressed: true,
            modifiers: 0,
        };
        let key_up = InputEvent::Key {
            scancode: 0x1E,
            pressed: false,
            modifiers: 0,
        };
        let mut m1 = Vec::new();
        encode_events(&[key_down], &mut m1);
        let dirty = vec![1u8, 0, 0x99]; // 未知事件类型，解码必拒
        let mut m3 = Vec::new();
        encode_events(&[key_up], &mut m3);
        let mut transport = MockTransport {
            deliver: vec![
                NetEvent::ReliableUrgent(m1),
                NetEvent::ReliableUrgent(dirty),
                NetEvent::ReliableUrgent(m3),
            ],
            ..Default::default()
        };
        let mut injector = MockInjector::default();
        let mut channel = InputChannel::new(DEFAULT_WINDOW);

        let err = channel
            .pump_receiver(&mut transport, &mut injector, Duration::from_millis(0))
            .expect_err("脏消息必须让本拍返回错误");
        assert!(
            matches!(err, Error::Protocol { .. }),
            "脏消息应报 Protocol 错误，实际 {err:?}"
        );
        assert_eq!(
            injector.events,
            vec![key_down, key_up],
            "第 1、3 条必须照常注入：松键随批蒸发 = 永久卡键"
        );
    }

    /// 注入失败同理：第 2 条消息注入失败时，第 1、3 条（含松键）仍被
    /// 注入，函数返回错误。
    #[test]
    fn inject_failure_does_not_drop_sibling_messages_in_batch() {
        let key_down = InputEvent::Key {
            scancode: 0x2C,
            pressed: true,
            modifiers: 0,
        };
        let key_up = InputEvent::Key {
            scancode: 0x2C,
            pressed: false,
            modifiers: 0,
        };
        let mut msgs = Vec::new();
        for ev in [key_down, InputEvent::Sync, key_up] {
            let mut buf = Vec::new();
            encode_events(&[ev], &mut buf);
            msgs.push(NetEvent::ReliableUrgent(buf));
        }
        let mut transport = MockTransport {
            deliver: msgs,
            ..Default::default()
        };
        let mut injector = FlakyInjector {
            events: Vec::new(),
            fail_on_call: 2, // 第 2 条（Sync）注入必失败
            calls: 0,
        };
        let mut channel = InputChannel::new(DEFAULT_WINDOW);

        let err = channel
            .pump_receiver(&mut transport, &mut injector, Duration::from_millis(0))
            .expect_err("注入失败必须让本拍返回错误");
        assert_eq!(injector.calls, 3, "三条消息都必须尝试注入");
        assert_eq!(
            injector.events,
            vec![key_down, key_up],
            "失败前后的兄弟消息（含松键）必须照常注入"
        );
        assert!(
            err.to_string().contains("flaky inject"),
            "应返回注入器原始错误: {err}"
        );
    }
}
