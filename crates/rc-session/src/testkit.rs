//! 进程内传输对接件：记录发送、脚本化接收、可互连（T14 测试基建）。
//!
//! # 为什么不用真 QUIC 做单测
//!
//! 会话状态机（握手校验、三通道分发、停机顺序）的全部逻辑都不依赖
//! 网络时序：给它一个**确定性的** [`Transport`](rc_net::Transport) 替身，
//! 事件按投递序即刻交付，测试才能逐条断言。真 QUIC 回环由
//! `tests/quic_loopback.rs` 单独覆盖。
//!
//! # 与铁律的关系
//!
//! 容量不设界（铁律 1 管管线队列，不管测试件）；`poll` **忽略 timeout
//! 立即返回**已就绪事件（确定性优先；N4 的超时语义由 rc-net 自己的
//! 测试覆盖，别拿它验证）。
//!
//! # T18 追加件
//!
//! [`ScriptedCapture`]（client 脚本输入源）、[`RecordingInjector`]（host
//! 记录型注入器）、[`batch_is_key`]（InputAck「关键批」共享口径函数）。
//! 定位对齐 rc-loopback 的 `WallEncoder`：它们是**测量基础设施**，处在
//! IO 边界上，允许读墙钟（真实睡眠模拟硬件）；纯逻辑判定（如关键批）
//! 不得读钟。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rc_input::InputEvent;
use rc_net::{NetEvent, NetStats, Transport};
use rc_types::{Error, Result};

use crate::auth::{ApprovalRequest, ApprovalVerdict, ConnectionApprover};
use crate::clipboard::ClipboardBackend;
use crate::proto::ControlMessage;

/// 一端的私有状态：发送记录 + 脚本化投递队列。
#[derive(Default)]
struct OwnState {
    /// `send_datagram` 逐条记录（按发送序）。
    datagrams_sent: Vec<Vec<u8>>,
    /// `send_reliable` 逐条记录（按发送序）。
    reliable_sent: Vec<Vec<u8>>,
    /// `send_reliable_urgent` 逐条记录（按发送序）。
    urgent_sent: Vec<Vec<u8>>,
    /// 脚本化投递队列：`deliver` 预置、`poll` 取走（先于互连事件交付）。
    deliver: VecDeque<NetEvent>,
}

/// 互连链路：A→B 与 B→A 两条单向事件队列（无损无延迟、不设容量界）。
#[derive(Default)]
struct Link {
    to_b: VecDeque<NetEvent>,
    to_a: VecDeque<NetEvent>,
}

/// 进程内传输对接件：记录发送、脚本化接收、可互连。
///
/// # 两种用法
///
/// - **脚本模式**（单测主流）：[`MockTransport::new`] 创建独立实例，
///   [`MockTransport::deliver`] / [`MockTransport::deliver_reliable`] 预置
///   下一拍 poll 要交付的事件，发送记录用于事后断言；
/// - **互连模式**（端到端闭环）：[`pair`] 创建一对互连实例，A 的
///   `send_datagram` / `send_reliable` / `send_reliable_urgent` 依次出现
///   在 B 的 `poll` 里，反之亦然。
///
/// # 句柄约定（测试断言用）
///
/// 本类型实现 `Clone`：克隆体与原实例共享**同一端**的记录与投递队列
/// （同一把锁）。把原实例 `Box` 进会话后，测试仍持克隆体断言
/// `reliable_sent` / `datagrams_sent`、继续 `deliver` 事件——这是单测
/// 能透视被装箱传输的唯一通道。克隆 `pair` 的一端得到的仍是该端的
/// 视图，不会变成对端。
#[derive(Clone)]
pub struct MockTransport {
    own: Arc<Mutex<OwnState>>,
    link: Option<Arc<Mutex<Link>>>,
    /// `true` = pair 的 A 端（向 `to_b` 写、从 `to_a` 读）。
    is_a: bool,
}

impl Default for MockTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl MockTransport {
    /// 独立实例（脚本模式，未互连）。
    pub fn new() -> Self {
        Self {
            own: Arc::new(Mutex::new(OwnState::default())),
            link: None,
            is_a: true,
        }
    }

    /// 预置一条下一拍 poll 要交付的事件（按投递序）。
    ///
    /// 互连模式下本端脚本事件**先于**对端发来的互连事件交付（脚本 =
    /// 「更早到达」的确定性替身）。
    pub fn deliver(&mut self, ev: NetEvent) {
        self.own
            .lock()
            .expect("mock own lock")
            .deliver
            .push_back(ev);
    }

    /// 编码一条控制消息并入箱（等价 `deliver(NetEvent::Reliable(msg))`）。
    pub fn deliver_reliable(&mut self, msg: &ControlMessage) {
        let mut buf = Vec::new();
        msg.encode(&mut buf);
        self.deliver(NetEvent::Reliable(buf));
    }

    /// 本端发出的媒体数据报（按发送序）。
    pub fn datagrams_sent(&self) -> Vec<Vec<u8>> {
        self.own
            .lock()
            .expect("mock own lock")
            .datagrams_sent
            .clone()
    }

    /// 本端经 reliable 流发出的原始消息（按发送序）。
    pub fn reliable_sent(&self) -> Vec<Vec<u8>> {
        self.own
            .lock()
            .expect("mock own lock")
            .reliable_sent
            .clone()
    }

    /// 本端经 urgent 流发出的原始消息（按发送序）。
    pub fn urgent_sent(&self) -> Vec<Vec<u8>> {
        self.own.lock().expect("mock own lock").urgent_sent.clone()
    }

    /// 可靠流上收到的控制消息（按序解码，测试断言用）。
    ///
    /// 解码失败的消息以 `panic` 暴露——这是测试断言件，调用方只应对
    /// 「本端只会发合法控制消息」的会话使用它。
    pub fn reliable_decoded(&self) -> Vec<ControlMessage> {
        self.reliable_sent()
            .iter()
            .map(|m| ControlMessage::decode(m).expect("mock reliable_sent 里的消息必须可解"))
            .collect()
    }
}

impl Transport for MockTransport {
    fn send_datagram(&mut self, data: &[u8]) -> Result<()> {
        let mut own = self.own.lock().expect("mock own lock");
        own.datagrams_sent.push(data.to_vec());
        if let Some(link) = &self.link {
            let mut l = link.lock().expect("mock link lock");
            let q = if self.is_a { &mut l.to_b } else { &mut l.to_a };
            q.push_back(NetEvent::Datagram(data.to_vec()));
        }
        Ok(())
    }

    fn send_reliable(&mut self, data: &[u8]) -> Result<()> {
        let mut own = self.own.lock().expect("mock own lock");
        own.reliable_sent.push(data.to_vec());
        if let Some(link) = &self.link {
            let mut l = link.lock().expect("mock link lock");
            let q = if self.is_a { &mut l.to_b } else { &mut l.to_a };
            q.push_back(NetEvent::Reliable(data.to_vec()));
        }
        Ok(())
    }

    /// 覆写而不吃默认实现：默认会委托 `send_reliable`，记录就分不清
    /// 输入到底走了哪条流（S4 的通道纪律断言会失效）。
    fn send_reliable_urgent(&mut self, data: &[u8]) -> Result<()> {
        let mut own = self.own.lock().expect("mock own lock");
        own.urgent_sent.push(data.to_vec());
        if let Some(link) = &self.link {
            let mut l = link.lock().expect("mock link lock");
            let q = if self.is_a { &mut l.to_b } else { &mut l.to_a };
            q.push_back(NetEvent::ReliableUrgent(data.to_vec()));
        }
        Ok(())
    }

    /// 忽略 `timeout`，立即返回已就绪事件（确定性优先，见模块文档）。
    fn poll(&mut self, _timeout: std::time::Duration) -> Result<Vec<NetEvent>> {
        let mut out: Vec<NetEvent> = {
            let mut own = self.own.lock().expect("mock own lock");
            own.deliver.drain(..).collect()
        };
        if let Some(link) = &self.link {
            let mut l = link.lock().expect("mock link lock");
            let q = if self.is_a { &mut l.to_a } else { &mut l.to_b };
            out.extend(q.drain(..));
        }
        Ok(out)
    }

    fn stats(&self) -> NetStats {
        NetStats::default()
    }
}

/// 双工对接：一对互连的 [`MockTransport`]——A 的 `send_datagram` /
/// `send_reliable` / `send_reliable_urgent` 依次出现在 B 的 `poll` 里，
/// 反之亦然。无损无延迟；容量不设界（铁律 1 管管线队列，不管测试件）。
pub fn pair() -> (MockTransport, MockTransport) {
    let link = Arc::new(Mutex::new(Link::default()));
    let a = MockTransport {
        own: Arc::new(Mutex::new(OwnState::default())),
        link: Some(Arc::clone(&link)),
        is_a: true,
    };
    let b = MockTransport {
        own: Arc::new(Mutex::new(OwnState::default())),
        link: Some(Arc::clone(&link)),
        is_a: false,
    };
    (a, b)
}

// ─────────────────────────────────────────────────────────────────
// T18 追加：脚本输入源 / 记录型注入器 / 关键批共享口径
// ─────────────────────────────────────────────────────────────────

/// 批内是否含关键事件：按下/抬起（MouseButton/Key）、滚轮（MouseWheel）
/// ——即 MouseMove 与 Sync 之外的全部。
///
/// **共享口径函数**：InputAck「关键批」的判定（client 发送侧统计与
/// host 回执侧计数）必须与 testkit/真实 InputBridge 用同一个实现，
/// 不得各写一套（设计师风险 6 / 坑 6）。host 与 client 会话均从这里
/// 调用，两侧天然同源。
pub fn batch_is_key(events: &[InputEvent]) -> bool {
    events
        .iter()
        .any(|ev| !matches!(ev, InputEvent::MouseMove { .. } | InputEvent::Sync))
}

/// 记录型注入器（host `--inject-mode record`）的内态：克隆共享。
#[derive(Default)]
struct RecordingInner {
    /// 已记录事件（按到达序；`take_events` 取走后清空）。有界队列：
    /// 超过 [`MAX_RECORDED_EVENTS`] 丢最旧（修复卡D-5，CLI host 默认
    /// 注入器不能无界增长），故用 `VecDeque`。
    events: VecDeque<InputEvent>,
    /// 注入的事件总数（取走事件不清零——stderr 摘要与双子进程测试断言
    /// 用；也不受容量丢弃影响，始终是「到达过」的全量计数）。
    injected: u64,
    /// 因容量上限丢弃的最旧事件数（修复卡D-5：`events` 有界后的溢出
    /// 观测口，测试与诊断断言用）。
    dropped: u64,
    /// 含关键事件的批数（与 InputAck `key_batch_seq` 同一口径）。
    key_batches: u64,
    /// 最近一次 `set_screen_size` 记录的尺寸（诊断）。
    screen: Option<(u32, u32)>,
}

/// 记录型注入器（host `--inject-mode record`）：实现
/// [`rc_input::InputInjector`]，不真注入——localhost 实测无需辅助功能
/// 权限（S7：CI 免权限复跑指标）。
///
/// 记录**有界**（修复卡D-5）：本注入器是 CLI host 的默认装配件，跑在
/// 真会话里——长会话下脚本输入（8ms 一批移动）会让无界的 events 向量
/// 无限膨胀。上限 [`MAX_RECORDED_EVENTS`] 条，超限丢最旧并计入
/// [`RecordingInjector::dropped_events`]（`injected_count` 仍是全量
/// 计数，不受丢弃影响）。
///
/// `Clone` 与 [`MockTransport`] 同一约定：克隆体与原实例共享同一份
/// 记录（同一把锁）。原实例 `Box` 进会话后，调用方仍持克隆体读计数、
/// 取事件做断言。
#[derive(Clone, Default)]
pub struct RecordingInjector {
    inner: Arc<Mutex<RecordingInner>>,
}

/// 记录容量上限（修复卡D-5）：10 万条事件 ≈ 24B×100k ≈ 2.4MB，足够
/// 覆盖 30 分钟级 localhost 实测的断言取样，又封死无界增长。
pub const MAX_RECORDED_EVENTS: usize = 100_000;

impl RecordingInjector {
    /// 空记录构造。
    pub fn new() -> Self {
        Self::default()
    }

    /// 已记录事件总数（host stderr 摘要与双子进程测试断言用）。
    pub fn injected_count(&self) -> u64 {
        self.inner.lock().expect("recorder lock").injected
    }

    /// 因容量上限丢弃的最旧事件数（修复卡D-5 的溢出观测口）。
    pub fn dropped_events(&self) -> u64 {
        self.inner.lock().expect("recorder lock").dropped
    }

    /// 当前留存的记录条数（≤ [`MAX_RECORDED_EVENTS`]）。
    pub fn recorded_len(&self) -> usize {
        self.inner.lock().expect("recorder lock").events.len()
    }

    /// 含关键事件的批数。与 InputAck 的 `key_batch_seq` 同一口径计数：
    /// 两侧判定不一致会让输入单向延迟采样错位（见 [`batch_is_key`]）。
    pub fn key_batches(&self) -> u64 {
        self.inner.lock().expect("recorder lock").key_batches
    }

    /// 取走全部已记录事件（测试断言用，取走后清空；计数器不清零）。
    pub fn take_events(&mut self) -> Vec<InputEvent> {
        self.inner
            .lock()
            .expect("recorder lock")
            .events
            .drain(..)
            .collect()
    }

    /// 最近一次 `set_screen_size` 记录的尺寸（None = 未收到）。
    pub fn screen_size(&self) -> Option<(u32, u32)> {
        self.inner.lock().expect("recorder lock").screen
    }
}

impl rc_input::InputInjector for RecordingInjector {
    fn inject(&mut self, events: &[InputEvent]) -> Result<()> {
        let mut inner = self.inner.lock().expect("recorder lock");
        if batch_is_key(events) {
            inner.key_batches += 1;
        }
        inner.injected += events.len() as u64;
        inner.events.extend(events.iter().copied());
        // 修复卡D-5：超限丢最旧并计数（injected 是全量计数，不动）。
        if inner.events.len() > MAX_RECORDED_EVENTS {
            let overflow = inner.events.len() - MAX_RECORDED_EVENTS;
            inner.events.drain(..overflow);
            inner.dropped += overflow as u64;
        }
        Ok(())
    }

    fn set_screen_size(&mut self, w: u32, h: u32) {
        // 记录尺寸（真实注入器用它反归一化坐标；记录型只留诊断信息）。
        self.inner.lock().expect("recorder lock").screen = Some((w, h));
    }
}

/// 脚本输入源（client `--input-mode scripted`）：实现
/// [`rc_input::InputCapture`]，按墙钟回放时间线。
///
/// 测量基础设施定位对齐 rc-loopback 的 `WallEncoder`（真实睡眠模拟硬件）：
/// IO 边界允许读墙钟（`next_events` 以构造时刻为零点判断脚本事件是否
/// 到期）；纯逻辑判定见 [`batch_is_key`]。
pub struct ScriptedCapture {
    /// 构造时刻（脚本时间零点）。
    start: Instant,
    /// 已回放到的下标。
    cursor: usize,
    /// `(距开始的微秒, 事件)` 列表（时刻非降）。
    timeline: Vec<(u64, InputEvent)>,
}

impl ScriptedCapture {
    /// 内置标准脚本：8ms 间隔 MouseMove + 每 500ms 一次 MouseButton
    /// 按下/抬起（关键事件强制成批）。覆盖「移动合并」与「关键事件
    /// 立即 flush」两条批处理路径，并给 InputAck 提供持续样本。
    ///
    /// 总时长 10 分钟（30s 级实测足够；播完后返回空，会话继续）。
    pub fn standard() -> Self {
        const MOVE_INTERVAL_US: u64 = 8_000;
        const CLICK_PERIOD_US: u64 = 500_000;
        const TOTAL_US: u64 = 600_000_000;
        let mut timeline = Vec::with_capacity((TOTAL_US / MOVE_INTERVAL_US) as usize + 4);
        for t in (0..TOTAL_US).step_by(MOVE_INTERVAL_US as usize) {
            timeline.push((
                t,
                InputEvent::MouseMove {
                    x: 32_768,
                    y: 32_768,
                },
            ));
        }
        // 每 500ms：按下 + 1ms 后抬起（两个关键批；避开同刻合并成一批）。
        for t in (0..TOTAL_US).step_by(CLICK_PERIOD_US as usize) {
            timeline.push((
                t + CLICK_PERIOD_US / 2,
                InputEvent::MouseButton {
                    button: rc_input::MouseButton::Left,
                    pressed: true,
                },
            ));
            timeline.push((
                t + CLICK_PERIOD_US / 2 + 1_000,
                InputEvent::MouseButton {
                    button: rc_input::MouseButton::Left,
                    pressed: false,
                },
            ));
        }
        timeline.sort_by_key(|(t, _)| *t); // 同刻稳定合序
        Self::from_timeline(timeline).expect("内置脚本时刻必非降")
    }

    /// 自定义时间线：`(距开始的微秒, 事件)` 列表，时刻必须非降
    /// （违反返回 `Error::InvalidConfig`）。脚本播完后返回空（会话继续）。
    pub fn from_timeline(timeline: Vec<(u64, InputEvent)>) -> Result<Self> {
        for pair in timeline.windows(2) {
            if pair[0].0 > pair[1].0 {
                return Err(Error::InvalidConfig(format!(
                    "scripted capture timeline must be non-decreasing: {} > {}",
                    pair[0].0, pair[1].0
                )));
            }
        }
        Ok(Self {
            start: Instant::now(),
            cursor: 0,
            timeline,
        })
    }

    /// 脚本总时长（微秒；末条事件时刻，空脚本为 0）。
    pub fn duration_us(&self) -> u64 {
        self.timeline.last().map(|(t, _)| *t).unwrap_or(0)
    }
}

impl rc_input::InputCapture for ScriptedCapture {
    /// 到期事件返回（可多条同拍到期），未到期返回空。
    fn next_events(&mut self, _timeout: Duration) -> Result<Vec<InputEvent>> {
        let now_us = u64::try_from(self.start.elapsed().as_micros()).unwrap_or(u64::MAX);
        let mut out = Vec::new();
        while self.cursor < self.timeline.len() && self.timeline[self.cursor].0 <= now_us {
            out.push(self.timeline[self.cursor].1);
            self.cursor += 1;
        }
        Ok(out)
    }
}

// ─────────────────────────────────────────────────────────────────
// T20 追加：可编程假剪贴板 / 脚本化剪贴板
// ─────────────────────────────────────────────────────────────────

/// 可编程假剪贴板（单测与集成测试直接操纵）：确定性、不读时钟。
#[derive(Debug, Default)]
pub struct FakeClipboard {
    count: u64,
    text: Option<String>,
    written: Vec<String>,
}

impl FakeClipboard {
    /// 创建空假剪贴板（初始变更计数为 0，无文本内容）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 模拟用户在本机复制内容（递增变更计数）。
    ///
    /// `None` 表示复制了非文本内容（图片 / 文件：`read_text` 将返回 `Ok(None)`）。
    pub fn simulate_local_copy(&mut self, text: Option<&str>) {
        self.count = self.count.wrapping_add(1);
        self.text = text.map(str::to_string);
    }

    /// 引擎历次 `write_text` 的记录（按写入序，断言用）。
    pub fn writes(&self) -> &[String] {
        &self.written
    }
}

impl ClipboardBackend for FakeClipboard {
    fn change_count(&mut self) -> Result<u64> {
        Ok(self.count)
    }

    fn read_text(&mut self) -> Result<Option<String>> {
        Ok(self.text.clone())
    }

    fn write_text(&mut self, text: &str) -> Result<()> {
        let owned = text.to_string();
        self.count = self.count.wrapping_add(1);
        self.text = Some(owned.clone());
        self.written.push(owned);
        Ok(())
    }
}

/// 可克隆共享句柄包装的 [`FakeClipboard`]：装箱进会话的 `enable_clipboard`
/// 后，测试仍可通过克隆句柄触发 `simulate_local_copy` 并断言 `writes()`
/// （与 [`MockTransport`] / [`RecordingInjector`] 同一句柄约定）。
#[derive(Clone, Default)]
pub struct SharedFakeClipboard {
    inner: Arc<Mutex<FakeClipboard>>,
}

impl SharedFakeClipboard {
    /// 创建空的共享假剪贴板句柄。
    pub fn new() -> Self {
        Self::default()
    }

    /// 模拟用户在本机复制内容（递增变更计数）。
    pub fn simulate_local_copy(&self, text: Option<&str>) {
        self.inner
            .lock()
            .expect("fake clipboard lock")
            .simulate_local_copy(text);
    }

    /// 返回历次 `write_text` 记录的快照（按写入序）。
    pub fn writes(&self) -> Vec<String> {
        self.inner
            .lock()
            .expect("fake clipboard lock")
            .writes()
            .to_vec()
    }
}

impl ClipboardBackend for SharedFakeClipboard {
    fn change_count(&mut self) -> Result<u64> {
        self.inner
            .lock()
            .expect("fake clipboard lock")
            .change_count()
    }

    fn read_text(&mut self) -> Result<Option<String>> {
        self.inner.lock().expect("fake clipboard lock").read_text()
    }

    fn write_text(&mut self, text: &str) -> Result<()> {
        self.inner
            .lock()
            .expect("fake clipboard lock")
            .write_text(text)
    }
}

/// CLI `--clipboard scripted` 用：**IO 边界墙钟**（定位与 [`ScriptedCapture`]
/// 同款——测量设施允许读墙钟，引擎逻辑不允许）。
///
/// 构造后 500ms 模拟一次固定文本复制（`"rabbitcontrol-clipboard-test"`），
/// 之后保持不变——给双子进程 CI 测试提供确定可断言的流量，全程不碰真实剪贴板。
pub struct ScriptedClipboard {
    start: Instant,
    delay: Duration,
    scripted_text: String,
    fired: bool,
    count: u64,
    text: Option<String>,
    written: Vec<String>,
}

/// [`ScriptedClipboard::standard`] 注入的固定测试文本。
pub const SCRIPTED_CLIPBOARD_TEXT: &str = "rabbitcontrol-clipboard-test";

impl ScriptedClipboard {
    /// 标准脚本：构造后 500ms 触发一次 `"rabbitcontrol-clipboard-test"` 复制，
    /// 之后不再主动变更。
    pub fn standard() -> Self {
        Self::with_text(SCRIPTED_CLIPBOARD_TEXT)
    }

    /// 自定义文本脚本：构造后 500ms 触发一次指定文本复制，之后不再主动变更。
    ///
    /// 双端同时跑 `--clipboard scripted` 时让 host 与 client 使用不同文案，
    /// 避免先发一端的文本经回声抑制吞掉另一端的同文案脚本事件。
    pub fn with_text(text: impl Into<String>) -> Self {
        Self {
            start: Instant::now(),
            delay: Duration::from_millis(500),
            scripted_text: text.into(),
            fired: false,
            count: 0,
            text: None,
            written: Vec::new(),
        }
    }

    /// 历次 `write_text` 记录（断言与诊断用）。
    pub fn writes(&self) -> &[String] {
        &self.written
    }

    /// 若墙钟已越过触发延迟且尚未触发，则推进一次本地复制事件。
    fn maybe_fire(&mut self) {
        if !self.fired && self.start.elapsed() >= self.delay {
            self.fired = true;
            self.count = self.count.wrapping_add(1);
            self.text = Some(self.scripted_text.clone());
        }
    }
}

impl ClipboardBackend for ScriptedClipboard {
    fn change_count(&mut self) -> Result<u64> {
        self.maybe_fire();
        Ok(self.count)
    }

    fn read_text(&mut self) -> Result<Option<String>> {
        self.maybe_fire();
        Ok(self.text.clone())
    }

    fn write_text(&mut self, text: &str) -> Result<()> {
        let owned = text.to_string();
        self.count = self.count.wrapping_add(1);
        self.text = Some(owned.clone());
        self.written.push(owned);
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────
// T22 追加：可编排连接审批人（离线测试 / 集成测试复用）
// ─────────────────────────────────────────────────────────────────

/// [`MockApprover`] 的共享内态（克隆共享，装箱进 `HostSessionConfig` 后仍可观测）。
#[derive(Default)]
struct MockApproverInner {
    verdict: Option<ApprovalVerdict>,
    request: Option<ApprovalRequest>,
    in_flight: bool,
    submit_count: u32,
    cancel_count: u32,
}

/// 可编排的审批人（离线测试 / scripted 模式复用）。
///
/// `Clone` 与 [`MockTransport`] / [`RecordingInjector`] 同一句柄约定：
/// 克隆体与原实例共享同一份内态。把原实例装箱进 `HostSessionConfig::approver`
/// 后，测试仍可通过克隆体调用 [`preset`](Self::preset)、
/// [`take_request`](Self::take_request)、[`submit_count`](Self::submit_count)、
/// [`cancel_count`](Self::cancel_count) 编排与观测。
#[derive(Clone, Default)]
pub struct MockApprover {
    inner: Arc<Mutex<MockApproverInner>>,
}

impl MockApprover {
    /// 构造默认不自动裁决（`poll_verdict` 恒返回 `None`）的审批人。
    pub fn new() -> Self {
        Self::default()
    }

    /// 预置裁决：`submit` 后 `poll_verdict` 立即返回它（也可在等待期间经克隆句柄动态写入）。
    pub fn preset(&mut self, verdict: ApprovalVerdict) {
        self.inner.lock().expect("mock approver lock").verdict = Some(verdict);
    }

    /// 观测：取走最近一次 `submit` 收到的审批请求（断言字段完整性用；取走后清空）。
    pub fn take_request(&mut self) -> Option<ApprovalRequest> {
        self.inner
            .lock()
            .expect("mock approver lock")
            .request
            .take()
    }

    /// 观测：`submit` 调用次数。
    pub fn submit_count(&self) -> u32 {
        self.inner.lock().expect("mock approver lock").submit_count
    }

    /// 观测：`cancel` 实际撤销在途审批的次数（无在途请求时的幂等 no-op 不计数）。
    pub fn cancel_count(&self) -> u32 {
        self.inner.lock().expect("mock approver lock").cancel_count
    }
}

impl ConnectionApprover for MockApprover {
    fn submit(&mut self, req: &ApprovalRequest) {
        let mut inner = self.inner.lock().expect("mock approver lock");
        inner.request = Some(req.clone());
        inner.in_flight = true;
        inner.submit_count += 1;
    }

    fn poll_verdict(&mut self) -> Option<ApprovalVerdict> {
        let mut inner = self.inner.lock().expect("mock approver lock");
        if !inner.in_flight {
            return None;
        }
        let verdict = inner.verdict;
        if verdict.is_some() {
            inner.in_flight = false;
        }
        verdict
    }

    fn cancel(&mut self) {
        let mut inner = self.inner.lock().expect("mock approver lock");
        if inner.in_flight {
            inner.in_flight = false;
            inner.cancel_count += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_input::{InputCapture, MouseButton};

    /// `batch_is_key` 的输入语义：MouseMove/Sync 非关键；按下/抬起/滚轮/
    /// 按键关键。
    #[test]
    fn batch_is_key_matches_input_semantics() {
        let mv = || InputEvent::MouseMove { x: 1, y: 2 };
        assert!(!batch_is_key(&[]), "空批非关键");
        assert!(!batch_is_key(&[mv()]), "纯移动批非关键");
        assert!(!batch_is_key(&[InputEvent::Sync]), "Sync 非关键");
        assert!(!batch_is_key(&[mv(), InputEvent::Sync, mv()]));
        for ev in [
            InputEvent::MouseButton {
                button: MouseButton::Left,
                pressed: true,
            },
            InputEvent::MouseButton {
                button: MouseButton::Right,
                pressed: false,
            },
            InputEvent::MouseWheel {
                delta: 1,
                horizontal: false,
            },
            InputEvent::Key {
                scancode: 0x1E,
                pressed: true,
                modifiers: 0,
            },
        ] {
            assert!(batch_is_key(&[mv(), ev]), "{ev:?} 必须判关键");
            assert!(batch_is_key(&[ev]), "{ev:?} 单条也判关键");
        }
    }

    /// 3 批（纯移动/含点击/含按键）→ injected_count=事件总数、key_batches=2；
    /// take_events 取走后清空但计数器保留；克隆体共享同一记录。
    #[test]
    fn recording_injector_counts_key_batches() {
        let mut inj = RecordingInjector::new();
        let observer = inj.clone();

        rc_input::InputInjector::inject(
            &mut inj,
            &[
                InputEvent::MouseMove { x: 1, y: 1 },
                InputEvent::MouseMove { x: 2, y: 2 },
            ],
        )
        .expect("inject");
        rc_input::InputInjector::inject(
            &mut inj,
            &[
                InputEvent::MouseMove { x: 3, y: 3 },
                InputEvent::MouseButton {
                    button: MouseButton::Left,
                    pressed: true,
                },
            ],
        )
        .expect("inject");
        rc_input::InputInjector::inject(
            &mut inj,
            &[InputEvent::Key {
                scancode: 0x1E,
                pressed: false,
                modifiers: 0,
            }],
        )
        .expect("inject");

        assert_eq!(inj.injected_count(), 5, "事件总数");
        assert_eq!(inj.key_batches(), 2, "只有含关键事件的两批");
        assert_eq!(observer.injected_count(), 5, "克隆体共享记录");
        let events = inj.take_events();
        assert_eq!(events.len(), 5, "取走全部事件");
        assert!(inj.take_events().is_empty(), "取走后清空");
        assert_eq!(inj.injected_count(), 5, "取走事件不动计数器");
        rc_input::InputInjector::set_screen_size(&mut inj, 64, 64);
        assert_eq!(inj.screen_size(), Some((64, 64)), "set_screen_size 记尺寸");
    }

    /// 修复卡D-5：记录容量有界（[`MAX_RECORDED_EVENTS`] = 10 万条），
    /// 超限丢**最旧**、`dropped_events` 计数正确；`injected_count` 是
    /// 全量计数不受丢弃影响；留存首条是被丢后的最新一段。
    #[test]
    fn recording_injector_caps_events_at_limit() {
        let mut inj = RecordingInjector::new();
        // 两批各 6 万条移动事件（y 标记批次，x 标记批内序号），共 12 万
        // > 10 万上限：应丢最旧 2 万（第一批前 2 万条）。
        let batch = |y: u16| -> Vec<InputEvent> {
            (0..60_000u32)
                .map(|i| InputEvent::MouseMove { x: i as u16, y })
                .collect()
        };
        rc_input::InputInjector::inject(&mut inj, &batch(0)).expect("inject 批1");
        rc_input::InputInjector::inject(&mut inj, &batch(1)).expect("inject 批2");
        assert_eq!(inj.recorded_len(), MAX_RECORDED_EVENTS, "留存封顶");
        assert_eq!(inj.dropped_events(), 20_000, "超限丢最旧 2 万条");
        assert_eq!(inj.injected_count(), 120_000, "injected 是全量计数");

        // 再注入 3 条：又溢出 3 条（批 1 的第 20000..20003 被丢）。
        rc_input::InputInjector::inject(
            &mut inj,
            &[
                InputEvent::MouseMove { x: 0, y: 2 },
                InputEvent::MouseMove { x: 1, y: 2 },
                InputEvent::MouseMove { x: 2, y: 2 },
            ],
        )
        .expect("inject 追加");
        assert_eq!(inj.recorded_len(), MAX_RECORDED_EVENTS, "留存仍有界");
        assert_eq!(inj.dropped_events(), 20_003, "丢弃计数累加");
        assert_eq!(inj.injected_count(), 120_003);

        // 留存窗口的首尾：首条是批 1 存活下来的第 20003 条；末条是追加
        // 的最后一条（证明丢的是最旧、保的是最新）。
        let events = inj.take_events();
        assert_eq!(events.len(), MAX_RECORDED_EVENTS);
        assert_eq!(
            events.first(),
            Some(&InputEvent::MouseMove { x: 20_003, y: 0 }),
            "首条应为批 1 第 20003 条（更旧的已丢）"
        );
        assert_eq!(
            events.last(),
            Some(&InputEvent::MouseMove { x: 2, y: 2 }),
            "末条应为最新追加的事件"
        );
        assert_eq!(inj.recorded_len(), 0, "取走后清空");
        assert_eq!(inj.dropped_events(), 20_003, "取走不动 dropped 计数");
    }

    /// from_timeline 首拍返回 t=0 事件、未到期返回空、播完返回空；
    /// 时间线降序被拒。
    #[test]
    fn scripted_capture_replays_in_order() {
        // 降序时间线必须被拒。
        assert!(ScriptedCapture::from_timeline(vec![
            (1_000_000, InputEvent::Sync),
            (500_000, InputEvent::Sync),
        ])
        .is_err());

        let mut cap = ScriptedCapture::from_timeline(vec![
            (0, InputEvent::MouseMove { x: 0, y: 0 }),
            (0, InputEvent::MouseMove { x: 1, y: 1 }),
            (600_000_000, InputEvent::Sync), // 10 分钟后才到期：本轮恒未到期
        ])
        .expect("非降时间线合法");
        assert_eq!(cap.duration_us(), 600_000_000);

        // 首拍：两条 t=0 事件按序返回。
        let first = cap.next_events(Duration::ZERO).expect("next_events");
        assert_eq!(
            first,
            vec![
                InputEvent::MouseMove { x: 0, y: 0 },
                InputEvent::MouseMove { x: 1, y: 1 },
            ],
            "t=0 事件即刻到期"
        );
        // 未到期（10 分钟远处）：空。
        assert!(cap
            .next_events(Duration::ZERO)
            .expect("next_events")
            .is_empty());

        // 播完：构造一个已播尽的时间线验证返回空。
        let mut done = ScriptedCapture::from_timeline(vec![(0, InputEvent::Sync)]).expect("合法");
        let _ = done.next_events(Duration::ZERO);
        assert!(
            done.next_events(Duration::ZERO)
                .expect("next_events")
                .is_empty(),
            "播完返回空（会话继续）"
        );

        // standard()：时刻非降、覆盖移动与点击两条路径（≈10 分钟）。
        let std = ScriptedCapture::standard();
        assert!(std.duration_us() >= 599_000_000, "标准脚本应覆盖约 10 分钟");
        let has_click = std
            .timeline
            .iter()
            .any(|(_, ev)| matches!(ev, InputEvent::MouseButton { .. }));
        assert!(has_click, "标准脚本必须含关键事件");
        for w in std.timeline.windows(2) {
            assert!(w[0].0 <= w[1].0, "内置脚本时刻必须非降");
        }
    }

    /// `testkit::pair()`（T14 交付符号）双向投递、urgent 与 reliable 分道
    /// （urgent 不进 reliable 队列）。
    #[test]
    fn mock_transport_pair_roundtrips() {
        let (mut a, mut b) = pair();
        let mut a_handle = a.clone();

        a.send_reliable(b"ctrl").expect("send reliable");
        a.send_reliable_urgent(b"input").expect("send urgent");
        a.send_datagram(b"media").expect("send datagram");

        // B 侧 poll：三条分道到达。
        let got = b.poll(Duration::ZERO).expect("poll");
        assert_eq!(
            got,
            vec![
                NetEvent::Reliable(b"ctrl".to_vec()),
                NetEvent::ReliableUrgent(b"input".to_vec()),
                NetEvent::Datagram(b"media".to_vec()),
            ],
            "A 的三类发送必须按原通道出现在 B 的 poll 里"
        );

        // 反向：B→A 同样投递；A 句柄可透视自己发出的记录。
        b.send_reliable(b"pong").expect("send reliable");
        let got_a = a_handle.poll(Duration::ZERO).expect("poll");
        assert_eq!(got_a, vec![NetEvent::Reliable(b"pong".to_vec())]);
        assert_eq!(a_handle.reliable_sent(), vec![b"ctrl".to_vec()]);
        assert_eq!(
            a_handle.urgent_sent(),
            vec![b"input".to_vec()],
            "urgent 与 reliable 分道记录"
        );
    }

    /// T22：`MockApprover` 预置裁决、请求观测与 submit/cancel 计数及幂等性。
    #[test]
    fn mock_approver_preset_and_observation() {
        let req = ApprovalRequest {
            session_id: 0x1234_5678,
            remote_addr: "127.0.0.1:54321".parse().unwrap(),
            requested: crate::proto::SessionParams {
                width: 1920,
                height: 1080,
                fps: 60,
                bitrate_bps: 8_000_000,
            },
            auth_method: crate::proto::AuthMethod::Password,
            password_verified: true,
        };

        let mut approver = MockApprover::new();
        let mut observer = approver.clone();
        assert_eq!(approver.submit_count(), 0);
        assert_eq!(approver.cancel_count(), 0);
        assert!(approver.take_request().is_none());

        // 无在途请求时 cancel 为幂等 no-op，不 panic、不计数。
        approver.cancel();
        approver.cancel();
        assert_eq!(observer.cancel_count(), 0);

        // preset(Accepted) 后 submit → poll_verdict 立即返回 Some(Accepted)。
        approver.preset(ApprovalVerdict::Accepted);
        approver.submit(&req);
        assert_eq!(observer.submit_count(), 1);
        assert_eq!(approver.poll_verdict(), Some(ApprovalVerdict::Accepted));
        // 终局后再次 poll 返回 None；take_request 取到完整字段后清空。
        assert_eq!(approver.poll_verdict(), None);
        assert_eq!(observer.take_request(), Some(req.clone()));
        assert!(observer.take_request().is_none());

        // 未预置裁决的在途请求：poll 返回 None，cancel 计数恰好 +1 且二次 cancel 幂等。
        let mut pending = MockApprover::new();
        let pending_obs = pending.clone();
        pending.submit(&req);
        assert_eq!(pending.poll_verdict(), None);
        pending.cancel();
        pending.cancel();
        assert_eq!(pending_obs.submit_count(), 1);
        assert_eq!(pending_obs.cancel_count(), 1);
    }
}
