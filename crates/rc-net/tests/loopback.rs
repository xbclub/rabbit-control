//! 本地回环 socket 集成测试（无需外网，只占 127.0.0.1 的临时端口）。
//!
//! 八条链路覆盖任务卡 T09 的验收面与三条审计遗留：
//! 1. 端到端分片 → 重组（datagram 通道）；
//! 2. reliable 通道的有序性（输入通道语义）；
//! 3. 3% 丢包下连接存活 + FEC 维持 >95% 重组成功率（代理注入真实丢包，
//!    QUIC 自己的握手/ACK 流量同样经历丢包）；
//! 4. 100KB 帧跨 ~92 个数据报的逐字节重组（N2 尺寸预算下的真实分片数）；
//! 5. 控制流被大流量灌满时 urgent（输入）消息仍即时到达（独立流）；
//! 6. urgent 与 control 消息的边界完整性（交错变长消息）；
//! 7. `Endpoint::rebind` 模拟 NAT rebinding：迁移后连接存活、媒体不中断
//!    （N9，`PathChanged` 一并断言）；
//! 8. 丢包握手下 control / urgent 流角色永不互换（按流 ID 序号判定，
//!    与 accept_bi 的交付顺序无关）。

mod common;

use std::collections::HashSet;
use std::net::UdpSocket;
use std::thread;
use std::time::{Duration, Instant};

use rc_fec::FecConfig;
use rc_net::fec_adapter::FecSender;
use rc_net::quic::{max_media_payload, max_media_payload_with_fec, QuicEndpoint, QuicTransport};
use rc_net::reassemble::{AssembledFrame, FrameAssembler, DEFAULT_MAX_CONCURRENT_FRAMES};
use rc_net::{NetConfig, NetEvent, Role, Transport};
use rc_proto::{split_frame, MediaHeader};
use rc_trace::Outcome;
use rc_types::{FrameKind, FrameSeq, Timestamp};

/// 一帧的发送路径：分片 → FEC 包装 → 逐个 send_datagram。
/// 生产代码里这段在 rc-pipeline；测试里直接驱动，验证的是 rc-net 自身。
/// 分片预算用 FEC 版（冗余包 = 最长数据报 + 自身包头，必须预留下来）。
fn send_frame(
    tx: &mut QuicTransport,
    fec: &mut FecSender,
    seq: u64,
    kind: FrameKind,
    payload: &[u8],
    loss_rate: f32,
) -> usize {
    let pkts = split_frame(
        FrameSeq(seq),
        kind,
        Timestamp(1_000),
        Timestamp(2_000),
        0,
        payload,
        max_media_payload_with_fec(rc_net::quic::DEFAULT_MTU),
    );
    let wire = fec.wrap_frame(FrameSeq(seq), pkts, loss_rate);
    let n = wire.len();
    for p in wire {
        tx.send_datagram(&p).expect("send_datagram");
    }
    n
}

/// 在 host 侧收帧：poll → 解析 → push 重组 → 每轮尝试 FEC 恢复与 gc。
struct Receiver<'a> {
    asm: FrameAssembler,
    host: &'a mut QuicTransport,
    groups_seen: HashSet<u16>,
    /// 相对接收起点的微秒（注入给 gc 的时间轴）
    start: Instant,
}

impl<'a> Receiver<'a> {
    fn new(host: &'a mut QuicTransport) -> Self {
        Self {
            asm: FrameAssembler::new(DEFAULT_MAX_CONCURRENT_FRAMES),
            host,
            groups_seen: HashSet::new(),
            start: Instant::now(),
        }
    }

    fn now_us(&self) -> u32 {
        self.start.elapsed().as_micros() as u32
    }

    /// 轮询一轮：返回 (凑齐的帧, gc 判丢的帧序号)。
    /// 交付的帧记 `Displayed`、gc 判丢的帧记 `LostInNetwork`（埋点闭环）。
    fn step(&mut self, poll_timeout: Duration) -> (Vec<AssembledFrame>, Vec<FrameSeq>) {
        let mut out = Vec::new();
        for ev in self.host.poll(poll_timeout).expect("poll") {
            if let NetEvent::Datagram(b) = ev {
                if let Ok((h, body)) = MediaHeader::parse_datagram(&b) {
                    self.groups_seen.insert(h.fec_group);
                    if let Some(f) = self.asm.push(&h, body) {
                        out.push(f);
                    }
                }
            }
        }
        // 对见过的组尝试恢复（缺 ≥2 的组内部直接放弃，不会自旋）。
        let groups: Vec<u16> = self.groups_seen.iter().copied().collect();
        for g in groups {
            if let Some(f) = self.asm.try_fec_recover(g) {
                out.push(f);
            }
        }
        for f in &out {
            self.host.mark_frame_outcome(f.seq, Outcome::Displayed);
        }
        // N6：超时帧判丢并记 LostInNetwork（传输层埋点）。
        let dropped = self.asm.gc(self.now_us());
        for seq in &dropped {
            self.host.mark_frame_outcome(*seq, Outcome::LostInNetwork);
        }
        (out, dropped)
    }
}

#[test]
fn loopback_media_stream_passes_through() {
    let (mut client, mut host) = common::pair();

    // 首个事件必须是 Connected（连接就绪对 poll 可见）。
    let first = host.poll(Duration::from_millis(500)).expect("poll");
    assert!(
        first.contains(&NetEvent::Connected),
        "首个事件应包含 Connected，got {first:?}"
    );

    let mut fec = FecSender::new(FecConfig::default());
    // 5.6KB 帧，FEC 版媒体预算 1088B → 6 片（零丢包不加冗余）。
    let payload: Vec<u8> = (0..5600u32).map(|i| (i % 253) as u8).collect();
    let sent = send_frame(&mut client, &mut fec, 1, FrameKind::Idr, &payload, 0.0);
    assert_eq!(sent, 6, "5600/1088 向上取整 = 6 片，零丢包不加冗余");

    let mut rx = Receiver::new(&mut host);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut got = None;
    while got.is_none() {
        assert!(Instant::now() < deadline, "帧未在 10s 内到齐");
        let (frames, _) = rx.step(Duration::from_millis(50));
        if let Some(f) = frames.into_iter().next() {
            got = Some(f);
        }
    }
    drop(rx);

    let f = got.unwrap();
    assert_eq!(f.seq, FrameSeq(1));
    assert_eq!(f.kind, FrameKind::Idr);
    assert_eq!(f.data, payload, "端到端分片→重组必须逐字节相同");
    assert_eq!(f.capture_ts, Timestamp(1_000));
    assert_eq!(f.encode_done_ts, Timestamp(2_000));

    // N4：空队列时 poll(20ms) 必须按时返回，不得长时间阻塞。
    let t = Instant::now();
    let evs = host.poll(Duration::from_millis(20)).expect("poll");
    assert!(
        t.elapsed() < Duration::from_millis(1500),
        "poll(20ms) 阻塞了 {:?}（违反 N4）",
        t.elapsed()
    );
    assert!(
        evs.iter().all(|e| !matches!(e, NetEvent::Datagram(_))),
        "不应再有媒体数据报"
    );
}

#[test]
fn reliable_stream_preserves_order() {
    let (mut client, mut host) = common::pair();

    const N: usize = 300;
    for i in 0..N {
        client
            .send_reliable(format!("msg-{i:05}").as_bytes())
            .expect("send_reliable");
    }

    let mut msgs: Vec<Vec<u8>> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while msgs.len() < N {
        assert!(Instant::now() < deadline, "reliable 消息未在 10s 内收齐");
        for ev in host.poll(Duration::from_millis(50)).expect("poll") {
            if let NetEvent::Reliable(m) = ev {
                msgs.push(m);
            }
        }
    }
    for (i, m) in msgs.iter().enumerate() {
        assert_eq!(
            m,
            format!("msg-{i:05}").as_bytes(),
            "第 {i} 条消息顺序或内容错误（输入通道语义：可靠有序）"
        );
    }

    // 反向也走一遍：host → client（控制回传方向）。
    host.send_reliable(b"ack").expect("host send");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut got = false;
    while !got {
        assert!(Instant::now() < deadline);
        for ev in client.poll(Duration::from_millis(50)).expect("poll") {
            if let NetEvent::Reliable(m) = ev {
                assert_eq!(m, b"ack");
                got = true;
            }
        }
    }
}

#[test]
fn connection_survives_packet_loss() {
    // 3% 双向丢包（种子固定）：QUIC 握手/ACK 经历同样的丢包。
    let (mut client, mut host, proxy) = common::pair_with_proxy(0.03, 42);
    let mut fec = FecSender::new(FecConfig::default());

    // 300 帧 × 4 片（4352B，FEC 版预算 1088B/片）+ 每帧 1 冗余。
    // 组越小「组内缺 ≥2」越难：实测链路丢率 ~2.6% 时 4 包组失败率 ~0.2%，
    // 即便有效丢率翻倍到 6% 也只有 ~1.6%——验收线 95% 有充分裕量。
    // （曾用 6 包 × 100 帧：丢包落点跨运行重排，帧失败数 1–6 波动，贴线 flaky。）
    const FRAMES: u64 = 300;
    const FRAME_BYTES: usize = 4 * 1088;
    let payloads: Vec<Vec<u8>> = (0..FRAMES)
        .map(|k| {
            let salt = (k % 251) as u8;
            (0..FRAME_BYTES)
                .map(|i| ((i % 251) as u8).wrapping_add(salt))
                .collect()
        })
        .collect();

    // 发送端窗口 = 2（与 rc-sched 的 max_in_flight=2、接收端 N10 同一口径）：
    // 一帧结案（交付或判丢）才发下一帧。全量倾泻会被接收端的并发上限
    // 正确拒绝——那不是 bug，是 Q3 在起作用。
    let mut next_seq: u64 = 1;
    let mut in_flight: HashSet<u64> = HashSet::new();
    let mut assembled: HashSet<u64> = HashSet::new();
    let mut lost: HashSet<u64> = HashSet::new();

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut rx = Receiver::new(&mut host);
    while (assembled.len() + lost.len()) < FRAMES as usize {
        assert!(Instant::now() < deadline, "30s 内未收齐（连接可能已死）");
        while in_flight.len() < 2 && next_seq <= FRAMES {
            send_frame(
                &mut client,
                &mut fec,
                next_seq,
                FrameKind::P,
                &payloads[(next_seq - 1) as usize],
                0.03,
            );
            in_flight.insert(next_seq);
            next_seq += 1;
        }
        let (frames, dropped) = rx.step(Duration::from_millis(50));
        for f in frames {
            // 校验内容确实来自对应帧（防止张冠李戴的成功率）。
            assert_eq!(f.data, payloads[(f.seq.0 - 1) as usize]);
            assembled.insert(f.seq.0);
            in_flight.remove(&f.seq.0);
        }
        for seq in dropped {
            lost.insert(seq.0);
            in_flight.remove(&seq.0);
        }
    }

    let rate = assembled.len() as f64 / FRAMES as f64;
    let (fwd, dropped, total) = *proxy.counters.lock().unwrap();
    let s = client.stats();
    eprintln!(
        "3% 丢包（seed=42）：{}/{} 帧重组成功（rate={rate:.3}），{} 帧判丢；代理转发 {fwd}/丢 {dropped}/总 {total}（实测丢率 {:.3}），cwnd={} rtt_us={} loss_est={:.3}",
        assembled.len(),
        FRAMES,
        lost.len(),
        dropped as f64 / total as f64,
        s.cwnd_bytes,
        s.rtt_us,
        s.loss_rate
    );
    assert!(
        rate > 0.95,
        "3% 丢包下重组成功率 {rate:.3}（丢失 {}/{}，FEC 必须把成功率维持在 95% 以上）",
        lost.len(),
        FRAMES
    );

    // 连接确实存活（不只是没报错）：丢包仍在持续时还能继续传帧。
    // probe 与常规媒体同规格（带 FEC 冗余），允许换序号重发——
    // 真实管线丢帧后的行为就是「请求 IDR 重来」，而不是无限等死。
    let probe: Vec<u8> = (0..1120 * 3).map(|i| (i % 249) as u8).collect();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut probe_seq = FRAMES + 1;
    let mut attempts = 0;
    let mut got = false;
    while !got {
        assert!(
            Instant::now() < deadline,
            "丢包期后连接应继续可用（probe 尝试 {attempts} 次未达）"
        );
        send_frame(
            &mut client,
            &mut fec,
            probe_seq,
            FrameKind::Idr,
            &probe,
            0.03,
        );
        attempts += 1;
        probe_seq += 1;
        // 每 600ms（> gc 的 500ms 放弃线）没等到就换序号重发。
        let attempt_deadline = Instant::now() + Duration::from_millis(600);
        while !got && Instant::now() < attempt_deadline {
            let (frames, _) = rx.step(Duration::from_millis(50));
            for f in frames {
                if f.data == probe {
                    got = true; // 内容匹配即成功（不绑死具体序号）
                }
            }
        }
    }

    // N8：统计可用且有内容（rtt/cwnd 来自 quinn，loss 来自 NACK 估计）。
    let s = host.stats();
    assert!(s.bytes_recv > 0, "必须统计到接收字节");
    assert!(s.pacing_rate_bps > 0);
    // 审计修复（loss_rate 失真）：主动结算后，真实丢包开始后几秒内
    // loss_rate 必须非零且在实测丢率的量级内（代理丢 3%，实测 UDP 丢率
    // ~2-3%）。修复前恒 0.0——要等跟踪表攒满 4096 帧（60fps 下 30-60
    // 分钟）。缺片帧在 500ms 放弃线后结算，这里固定再等 2s 让全部媒体
    // 帧结算完，然后校验量级。
    let settle_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < settle_deadline {
        let _ = host.poll(Duration::from_millis(50)).expect("poll");
    }
    let host_loss = host.stats().loss_rate;
    eprintln!(
        "host 侧 loss_rate={host_loss:.4}（实测 UDP 丢率 {:.3}）",
        dropped as f64 / total as f64
    );
    assert!(
        host_loss > 0.01 && host_loss < 0.06,
        "loss_rate={host_loss:.4} 必须反映真实丢包且量级正确（实测丢率 ~2%，\
         修复前恒 0.0，双倍计错会 >6%）"
    );
    client.stats(); // 双侧都能取
}

#[test]
fn large_frame_spans_many_packets() {
    let (mut client, mut host) = common::pair();

    // 100KB 帧：1120B/片 → ceil(102400/1120) = 92 片。
    let payload: Vec<u8> = (0..102_400u32).map(|i| (i % 256) as u8).collect();
    let pkts = split_frame(
        FrameSeq(9),
        FrameKind::Idr,
        Timestamp(0),
        Timestamp(5_000),
        0,
        &payload,
        max_media_payload(rc_net::quic::DEFAULT_MTU),
    );
    assert_eq!(pkts.len(), 92, "100KB / 1120B 媒体预算 = 92 片");
    for p in &pkts {
        client.send_datagram(p).expect("send_datagram");
    }

    let mut rx = Receiver::new(&mut host);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut got = None;
    while got.is_none() {
        assert!(Instant::now() < deadline, "100KB 帧未在 10s 内重组完成");
        let (frames, _) = rx.step(Duration::from_millis(50));
        if let Some(f) = frames.into_iter().next() {
            got = Some(f);
        }
    }
    let f = got.unwrap();
    assert_eq!(f.seq, FrameSeq(9));
    assert_eq!(f.data.len(), 102_400);
    assert_eq!(f.data, payload, "100KB 帧跨 92 包必须逐字节重组相同");

    // N2 防线：超出 mtu - QUIC_OVERHEAD 预算的数据报立即被拒。
    let too_big = vec![0u8; rc_net::quic::DEFAULT_MTU - rc_net::quic::QUIC_OVERHEAD + 1];
    assert!(client.send_datagram(&too_big).is_err());
    // 边界内接受（1152 = 预算上限，此包仅用于尺寸检查，不期望送达语义）。
    let edge = vec![0u8; rc_net::quic::DEFAULT_MTU - rc_net::quic::QUIC_OVERHEAD];
    assert!(client.send_datagram(&edge).is_ok());
}

/// 审计遗留 1（输入独立流）：控制流被大流量灌满时，urgent 消息仍即时到达。
///
/// 序列：灌一半控制消息（100 × 64KB = 6.4MB backlog 在途）→ 中途插入
/// 1 条 urgent → 再灌另一半。共享单流时 urgent 必须排在已缓冲控制字节
/// 之后（QUIC 流内严格有序）；独立流 + stream priority 下，urgent 交付时
/// 后半批控制消息必然还有大量在路上。
///
/// 断言用「urgent 之前交付的控制消息数」（事件队列的通道序，与 poll
/// 时机无关）度量队头阻塞，另加发送调用本身不阻塞的时长断言。
#[test]
fn urgent_input_beats_control_backlog() {
    let (mut client, mut host) = common::pair();

    const N: usize = 200;
    const HALF: usize = N / 2;
    const MSG_BYTES: usize = 64 * 1024;
    // 允许在 urgent 之前交付的控制消息数超出前半批的余量：流控信用
    // 授予的竞争窗口里可能夹带少量控制消息（2MB），再多就不是「即时」。
    const GRANT_SLACK: usize = 32;

    let mk = |i: usize| -> Vec<u8> {
        let mut m = Vec::with_capacity(MSG_BYTES);
        m.extend_from_slice(&(i as u32).to_le_bytes());
        m.resize(MSG_BYTES, (i % 251) as u8);
        m
    };

    let mut urgent_send_took = None;
    let flood_deadline = Instant::now() + Duration::from_secs(30);
    let mut msgs = Vec::with_capacity(N);
    for i in 0..N {
        if i == HALF {
            // 此刻前半批 6.4MB 大部分仍在 quinn 发送缓冲 / 链路上。
            // 发送本身不得被控制流的反压挡住（专职写线程语义）。
            let t = Instant::now();
            client
                .send_reliable_urgent(b"input-event-urgent")
                .expect("send_reliable_urgent");
            urgent_send_took = Some(t.elapsed());
        }
        let m = mk(i);
        client.send_reliable(&m).expect("send_reliable");
        msgs.push(m);
        assert!(Instant::now() < flood_deadline, "灌流 30s 未完成");
    }
    let urgent_send_took = urgent_send_took.expect("HALF < N，必然已发送");
    let flood_end = Instant::now();

    // 排空 backlog，直到看见 urgent。`before` 在 urgent 事件处快照：
    // 只数排在它之前的控制消息（poll 批次内其后的消息属于「被超越」的证据）。
    let mut control_msgs: Vec<Vec<u8>> = Vec::with_capacity(N);
    let mut urgent_at_after_flood = None;
    let mut before = 0usize;
    let deadline = Instant::now() + Duration::from_secs(30);
    while urgent_at_after_flood.is_none() {
        assert!(Instant::now() < deadline, "urgent 30s 内未到达");
        for ev in host.poll(Duration::from_millis(20)).expect("poll") {
            match ev {
                NetEvent::ReliableUrgent(m) => {
                    assert_eq!(m, b"input-event-urgent", "urgent 消息必须逐字节完整");
                    before = control_msgs.len();
                    urgent_at_after_flood = Some(flood_end.elapsed());
                }
                NetEvent::Reliable(m) => control_msgs.push(m),
                _ => {}
            }
        }
    }

    eprintln!(
        "urgent 发送耗时 {urgent_send_took:?}（backlog 灌满中），灌流结束后 {:?} 可见，\
         此前只交付 {before}/{N} 条控制消息",
        urgent_at_after_flood.unwrap(),
    );
    assert!(
        urgent_send_took < Duration::from_millis(100),
        "send_reliable_urgent 本身耗时 {urgent_send_took:?}：输入发送路径被控制流反压挡住了"
    );
    assert!(
        urgent_at_after_flood.unwrap() < Duration::from_millis(500),
        "灌流结束后 urgent 仍迟迟不可见"
    );
    assert!(
        before < HALF + GRANT_SLACK,
        "urgent 交付前已有 {before}/{N} 条控制消息：它排在了控制 backlog 之后"
    );

    // backlog 排空：剩余控制消息全部到齐，逐条有序且逐字节完整。
    while control_msgs.len() < N {
        assert!(Instant::now() < deadline, "剩余控制消息未在 30s 内收齐");
        for ev in host.poll(Duration::from_millis(50)).expect("poll") {
            if let NetEvent::Reliable(m) = ev {
                control_msgs.push(m);
            }
        }
    }
    assert_eq!(control_msgs, msgs, "控制消息必须按序逐字节完整交付");
}

/// 审计遗留 1（输入独立流）：urgent 与 control 交错发送变长消息时，
/// 两条流各自逐条完整（边界不被拆串）、各自有序（互不重排）。
#[test]
fn urgent_and_control_message_boundaries_are_intact() {
    let (mut client, mut host) = common::pair();

    const N: usize = 40;
    let fill = |i: usize, len: usize, salt: u8| -> Vec<u8> {
        let mut m = Vec::with_capacity(len);
        m.extend_from_slice(&(i as u32).to_le_bytes());
        m.resize(len, salt.wrapping_add(i as u8));
        m
    };
    let control: Vec<Vec<u8>> = (0..N)
        .map(|i| fill(i, (i * 37 + 1) % 3000 + 1, 0x5A))
        .collect();
    let urgent: Vec<Vec<u8>> = (0..N)
        .map(|i| fill(i, (i * 53 + 1) % 2000 + 1, 0xA5))
        .collect();

    for i in 0..N {
        client.send_reliable(&control[i]).expect("control");
        client.send_reliable_urgent(&urgent[i]).expect("urgent");
    }

    let (mut got_c, mut got_u) = (0usize, 0usize);
    let deadline = Instant::now() + Duration::from_secs(10);
    while got_c < N || got_u < N {
        assert!(Instant::now() < deadline, "交错消息未在 10s 内收齐");
        for ev in host.poll(Duration::from_millis(50)).expect("poll") {
            match ev {
                NetEvent::Reliable(m) => {
                    assert_eq!(m, control[got_c], "第 {got_c} 条控制消息边界或内容错误");
                    got_c += 1;
                }
                NetEvent::ReliableUrgent(m) => {
                    assert_eq!(m, urgent[got_u], "第 {got_u} 条 urgent 消息边界或内容错误");
                    got_u += 1;
                }
                _ => {}
            }
        }
    }
}

/// 审计遗留 2（N9 迁移测试）：`Endpoint::rebind` 换掉客户端底层 socket
/// （模拟 NAT rebinding 路径迁移）后：连接存活、媒体数据报继续收发
/// 不中断、`PathChanged` 上报、reliable 双向仍可用。
#[test]
fn connection_survives_rebind_migration() {
    // rebind 需要 client 端点，common::pair 不返还端点——手动搭一对。
    let ident = common::identity();
    let certs = common::server_certs();
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
    let accepter = thread::spawn(move || {
        host_ep
            .accept(common::CONNECT_TIMEOUT)
            .expect("host accept")
    });
    let mut client = client_ep
        .connect(host_addr, &certs, "rabbitcontrol", common::CONNECT_TIMEOUT)
        .expect("client connect");
    let mut host = accepter.join().expect("accepter thread");

    let mk_payload = |k: u64| -> Vec<u8> {
        (0..4352u32)
            .map(|i| ((i % 251) as u8).wrapping_add((k % 251) as u8))
            .collect()
    };
    let mut fec = FecSender::new(FecConfig::default());

    // 迁移前基线：2 帧媒体（4 片/帧）正常到达。
    let mut asm = FrameAssembler::new(DEFAULT_MAX_CONCURRENT_FRAMES);
    let mut assembled: HashSet<u64> = HashSet::new();
    let baseline_deadline = Instant::now() + Duration::from_secs(10);
    for seq in 1..=2u64 {
        send_frame(
            &mut client,
            &mut fec,
            seq,
            FrameKind::P,
            &mk_payload(seq),
            0.0,
        );
    }
    while assembled.len() < 2 {
        assert!(Instant::now() < baseline_deadline, "迁移前基线帧未到齐");
        for ev in host.poll(Duration::from_millis(50)).expect("poll") {
            if let NetEvent::Datagram(b) = ev {
                if let Ok((h, body)) = MediaHeader::parse_datagram(&b) {
                    if let Some(f) = asm.push(&h, body) {
                        assert_eq!(f.data, mk_payload(f.seq.0), "迁移前帧内容错误");
                        assembled.insert(f.seq.0);
                    }
                }
            }
        }
    }

    // 迁移：客户端换底层 socket（新端口），连接 ID 不变。
    let new_sock = UdpSocket::bind("127.0.0.1:0").expect("bind new socket");
    client_ep.rebind(new_sock).expect("rebind");

    // 迁移后：继续发 6 帧（发送窗口 ≤ 2，与 Q3 同口径），全部必须到齐；
    // PathChanged 必须上报；控制流双向 round-trip 必须仍然可用。
    const POST_FRAMES: u64 = 6;
    let mut next_seq: u64 = 3;
    let mut in_flight: HashSet<u64> = HashSet::new();
    let mut path_changed = false;
    let mut got_roundtrip = false;
    client
        .send_reliable(b"post-migration-control")
        .expect("post-migration control");

    let deadline = Instant::now() + Duration::from_secs(20);
    while assembled.len() < 2 + POST_FRAMES as usize || !path_changed || !got_roundtrip {
        assert!(
            Instant::now() < deadline,
            "迁移后 20s 内未完成：assembled={} path_changed={path_changed} roundtrip={got_roundtrip}",
            assembled.len()
        );
        while in_flight.len() < 2 && next_seq <= 2 + POST_FRAMES {
            send_frame(
                &mut client,
                &mut fec,
                next_seq,
                FrameKind::P,
                &mk_payload(next_seq),
                0.0,
            );
            in_flight.insert(next_seq);
            next_seq += 1;
        }
        for ev in host.poll(Duration::from_millis(50)).expect("poll") {
            match ev {
                NetEvent::Datagram(b) => {
                    if let Ok((h, body)) = MediaHeader::parse_datagram(&b) {
                        if let Some(f) = asm.push(&h, body) {
                            assert_eq!(f.data, mk_payload(f.seq.0), "迁移后帧内容错误");
                            assembled.insert(f.seq.0);
                            in_flight.remove(&f.seq.0);
                        }
                    }
                }
                NetEvent::PathChanged => path_changed = true,
                NetEvent::Reliable(m) => {
                    assert_eq!(m, b"post-migration-control");
                    got_roundtrip = true;
                    // 反向：host → client 也走一遍迁移后的新路径。
                    host.send_reliable(b"ack-after-migration")
                        .expect("host ack");
                }
                _ => {}
            }
        }
    }
    assert!(path_changed, "rebind 后必须上报 PathChanged");

    // host → client 方向的 reliable 到达（服务端已迁移到客户端新地址）。
    let ack_deadline = Instant::now() + Duration::from_secs(10);
    let mut got_ack = false;
    while !got_ack {
        assert!(Instant::now() < ack_deadline, "host→client ack 未到达");
        for ev in client.poll(Duration::from_millis(50)).expect("poll") {
            if let NetEvent::Reliable(m) = ev {
                assert_eq!(m, b"ack-after-migration");
                got_ack = true;
            }
        }
    }

    let s = client.stats();
    assert!(s.bytes_sent > 0, "迁移后统计仍可用");
    eprintln!(
        "rebind 迁移后：{} 帧全部到齐，PathChanged 已上报，双向 reliable 正常，rtt_us={}",
        2 + POST_FRAMES,
        s.rtt_us
    );
}

/// 审计修复（流角色可能互换）：弱网丢包重传会让 accept_bi 的交付顺序与
/// 打开顺序颠倒——按到达顺序分配角色会把 control / urgent 两条流静默互换。
/// 角色必须按流 ID 序号判定（先开的 = control，见 `control_is_first`）。
///
/// 经丢包代理建立连接（握手与两条流的 STREAM 帧都经历丢包重传，交付
/// 顺序可能颠倒——即便本轮碰巧没颠倒，角色正确性断言同样成立），随后
/// 在两条流上交错发送消息：control 消息必须以 [`NetEvent::Reliable`]
/// 交付、urgent 消息必须以 [`NetEvent::ReliableUrgent`] 交付，且各自
/// 有序——角色互换会立刻暴露为消息错位。
#[test]
fn stream_roles_never_swap_under_lossy_handshake() {
    let ident = common::identity();
    let certs = common::server_certs();
    let host_ep = QuicEndpoint::new(
        &NetConfig::new(Role::Host, "127.0.0.1:0".parse().unwrap()),
        &ident,
    )
    .expect("host endpoint");
    let host_addr = host_ep.local_addr().expect("host addr");
    // 5% 丢包：握手 + 流打开的重传窗口足够大，乱序概率显著。
    let proxy = common::LossyProxy::spawn(host_addr, 0.05, 7);
    let client_ep = QuicEndpoint::new(
        &NetConfig::new(Role::Controller, "127.0.0.1:0".parse().unwrap()),
        &ident,
    )
    .expect("client endpoint");

    let accepter = thread::spawn(move || {
        host_ep
            .accept(common::CONNECT_TIMEOUT)
            .expect("host accept through lossy proxy")
    });
    let mut client = client_ep
        .connect(proxy.addr, &certs, "rabbitcontrol", common::CONNECT_TIMEOUT)
        .expect("client connect");
    let mut host = accepter.join().expect("accepter thread");

    // 交错发送：control 上是 C 系列、urgent 上是 U 系列。
    const N: u32 = 20;
    for i in 0..N {
        client
            .send_reliable(format!("C{i}").as_bytes())
            .expect("control send");
        client
            .send_reliable_urgent(format!("U{i}").as_bytes())
            .expect("urgent send");
    }

    let (mut cs, mut us) = (0u32, 0u32);
    let deadline = Instant::now() + Duration::from_secs(15);
    while cs < N || us < N {
        assert!(
            Instant::now() < deadline,
            "消息未在 15s 内收齐（cs={cs} us={us}）：流角色互换或连接死亡"
        );
        for ev in host.poll(Duration::from_millis(50)).expect("poll") {
            match ev {
                NetEvent::Reliable(m) => {
                    assert_eq!(
                        m,
                        format!("C{cs}").as_bytes(),
                        "control 消息错位：流角色被互换了"
                    );
                    cs += 1;
                }
                NetEvent::ReliableUrgent(m) => {
                    assert_eq!(
                        m,
                        format!("U{us}").as_bytes(),
                        "urgent 消息错位：流角色被互换了"
                    );
                    us += 1;
                }
                _ => {}
            }
        }
    }
    drop(client);
    drop(host);
    drop(proxy);
}
