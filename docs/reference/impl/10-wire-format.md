# 10 线格式规格（逐字节）

> **这是契约文档。** 任何改动字段含义、顺序、宽度的行为都必须同步更新本文档
> 与 `crates/rc-proto/src/lib.rs` 的 `field_offsets_are_stable` 测试。
> 不一致会导致：与本项目的其他实现、或其他 agent 的实现无法互通。

## 1. 总则

| 项 | 规定 |
|---|---|
| 字节序 | **小端（little-endian）**，所有多字节字段 |
| 包头长度 | **固定 32 字节**，无扩展、无 varint |
| 数据报总长 | 包头 + 负载 ≤ 1232 字节（`32 + 1200`） |
| 负载上限 | 1200 字节（避免 IP 分片） |
| 校验 | 无包头校验和（QUIC 已提供完整性） |
| 保留字段 | 必须写 0，读时忽略 |

### 为什么定长、不用 protobuf

- 1080p60 每帧 10–40 包 → 每秒最多 2400 次包头解析
- 目标 **≤200ns/包**
- protobuf 的 varint + 字段匹配约 100–300ns，且会分配
- 定长布局是按偏移直接读，实测 ~30ns，零分配

## 2. 媒体包头（32 字节）

### 2.1 字段表

| 偏移 | 长度 | 类型 | 字段名 | 说明 |
|---|---|---|---|---|
| 0 | 4 | u32 | `magic` | 固定 `0x31564352`，内存字节序为 ASCII `"RCV1"` |
| 4 | 8 | u64 | `frame_seq` | 帧序号，会话内单调递增，从 0 开始 |
| 12 | 4 | u32 | `capture_ts_us` | 采集时刻（屏幕内容**实际变化**的时刻），相对会话起点，微秒 |
| 16 | 4 | u32 | `encode_done_us` | 编码完成时刻，相对会话起点，微秒 |
| 20 | 2 | u16 | `packet_idx` | 本包在该帧内的序号，从 0 开始 |
| 22 | 2 | u16 | `packet_cnt` | 该帧的总包数，≥1 |
| 24 | 2 | u16 | `fec_group` | FEC 组号，同一组的包共享此值 |
| 26 | 1 | u8 | `flags` | 见 §2.2 |
| 27 | 1 | u8 | `layer_id` | 分层编码预留，P0 恒为 0 |
| 28 | 2 | u16 | `payload_len` | 负载字节数，0–1200 |
| 30 | 2 | u16 | `reserved` | 保留，写 0，读时忽略 |

### 2.2 `flags` 位定义

| 位 | 掩码 | 名称 | 含义 |
|---|---|---|---|
| 0 | `0x01` | `IDR` | 该帧是关键帧。**该帧的每一个包都必须置此位**（不是只有第一个） |
| 1 | `0x02` | `LAST` | 该帧的最后一个包。**只有 `packet_idx == packet_cnt - 1` 的包置位** |
| 2 | `0x04` | `HAS_DIRTY` | 携带脏区信息（P1，P0 不置位） |
| 3 | `0x08` | `PARITY` | 该包是 FEC 冗余包（负载不是媒体数据） |
| 4–7 | — | 保留 | 必须为 0 |

**为什么 IDR 标志放在每个包上**：数据报可能乱序到达。接收端看到一个
`packet_idx=5` 的包时，必须能独立判断「这是关键帧的一部分」，
不能依赖先收到 `packet_idx=0`。

### 2.3 不变量（解码端必须校验）

| 编号 | 不变量 | 违反时 |
|---|---|---|
| I1 | `magic == 0x31564352` | 拒绝，`Protocol { offset: 0 }` |
| I2 | `packet_cnt >= 1` | 拒绝，`Protocol { offset: 22 }` |
| I3 | `packet_idx < packet_cnt` | 拒绝，`Protocol { offset: 20 }` |
| I4 | `payload_len <= 1200` | 拒绝，`Protocol { offset: 28 }` |
| I5 | `payload_len == 实际剩余字节数` | 拒绝，`Protocol { offset: 28 }`（仅在 `parse_datagram` 校验） |

### 2.4 字节布局图

```
字节  0    1    2    3    4                11   12        15   16        19
     ┌────┬────┬────┬────┬─────────────────┬────┬─────────┬────┬─────────┐
     │ 'R'│ 'C'│ 'V'│ '1'│   frame_seq     │    │capture  │    │encode   │
     │    magic (u32 LE) │      (u64 LE)    │    │ts (u32) │    │done(u32)│
     └────┴────┴────┴────┴─────────────────┴────┴─────────┴────┴─────────┘

字节 20   21   22   23   24   25   26   27   28   29   30   31
    ┌────┬────┬────┬────┬────┬────┬────┬────┬────┬────┬────┬────┐
    │pkt │pkt │pkt │pkt │fec │fec │flag│lay │pay │pay │rsv │rsv │
    │idx │idx │cnt │cnt │grp │grp │    │id  │len │len │    │    │
    └────┴────┴────┴────┴────┴────┴────┴────┴────┴────┴────┴────┘
     └── u16 ──┘└── u16 ──┘└── u16 ──┘     └── u16 ──┘└── u16 ──┘
```

### 2.5 已固定的字节级测试向量

输入：

```rust
MediaHeader {
    frame_seq: 0x0807060504030201,
    capture_ts: Timestamp(0x11112222),
    encode_done_ts: Timestamp(0x33334444),
    packet_idx: 0x5555,
    packet_cnt: 0x6666,
    fec_group: 0x7777,
    flags: HeaderFlags(0x88),
    layer_id: 0x99,
    payload_len: 0xAAAA,
}
```

期望输出（32 字节）：

```
52 43 56 31                                     # "RCV1"
01 02 03 04 05 06 07 08                         # frame_seq
22 22 11 11                                     # capture_ts
44 44 33 33                                     # encode_done_ts
55 55                                           # packet_idx
66 66                                           # packet_cnt
77 77                                           # fec_group
88                                              # flags
99                                              # layer_id
AA AA                                           # payload_len
00 00                                           # reserved
```

**任何实现都必须能复现这个向量。** 测试见 `rc-proto::tests::field_offsets_are_stable`。

## 3. 帧分片规则

一个编码后的帧（可能几十 KB）被切成多个数据报。

### 3.1 规则

| 规则 | 说明 |
|---|---|
| F1 | 每片负载 ≤ 1200 字节 |
| F2 | 切片顺序固定：第 i 片是 `payload[i*1200 .. (i+1)*1200]` |
| F3 | 空负载也产生 **1 个**包（`payload_len = 0`, `packet_idx = 0`, `packet_cnt = 1`） |
| F4 | 只有最后一片置 `LAST` |
| F5 | 所有片共享相同的 `frame_seq` / `packet_cnt` / `fec_group` / `capture_ts` / `encode_done_ts` |
| F6 | IDR 帧的所有片都置 `IDR` |

### 3.2 分片大小选择的理由

1200 字节来自 QUIC 的约束链：

```
以太网 MTU            1500
- IPv6 头              40
- UDP 头                8
- QUIC 包头+AEAD 开销  ~48
────────────────────────────
可用负载            ≈1404 → 保守取 1200
```

超过 MTU 会触发 IP 分片。**IP 分片丢失一片就等于整包丢失**，
在弱网下会显著放大丢包影响，必须避免。

项目里断言：`HEADER_LEN + MAX_PAYLOAD <= 1400`（见 `rc-proto` 测试）。

## 4. FEC 冗余包格式

冗余包复用同一套 32 字节包头，但：

| 字段 | 值 |
|---|---|
| `flags.PARITY` | 置位（`0x08`） |
| `packet_idx` | 恒为 0 |
| `packet_cnt` | 恒为 1（冗余包自身不分组） |
| `fec_group` | **被保护的数据包组的组号** |
| `payload_len` | 组内最长的数据包负载长度（已补零对齐） |
| `payload` | 组内所有数据包的逐字节异或 |

### 4.1 编码端算法

```
输入：一组数据包 P[0..n]（长度可以不同）
输出：parity

max_len = max(len(p) for p in P)
parity = [0u8; max_len]
for p in P:
    for i in 0..len(p):
        parity[i] ^= p[i]
    # p 比 max_len 短的部分等价于补 0，异或后不影响结果
```

### 4.2 解码端算法（缺 1 个包）

```
输入：已收到的包、缺失位置 k、parity
输出：P[k]

recovered = parity.clone()
for i in 0..n, i != k:
    for j in 0..len(P[i]):
        recovered[j] ^= P[i][j]
P[k] = recovered[0..原始长度]    # 尾部是补零产物，按需截断
```

### 4.3 能力边界

| 缺失包数 | 能否恢复 |
|---|---|
| 0 | 不需要（组完整） |
| 1 | ✅ 可恢复 |
| ≥2 | ❌ 不可恢复（XOR 固有局限） |

**缺失 ≥2 时不要重试**，直接通知上层请求关键帧。
Reed-Solomon 留作 P1 优化（见 `docs/03-protocol.md` §2.2）。

## 5. 可靠性分类

**不同数据走不同通道**，这是协议设计的核心决策：

| 数据 | 通道 | 丢包处理 |
|---|---|---|
| 输入事件（键鼠） | QUIC reliable stream | 重传至成功 |
| 控制消息（会话/鉴权） | QUIC reliable stream | 重传至成功 |
| IDR 帧 | QUIC reliable stream | 重传至成功 |
| P 帧 | QUIC datagram | FEC 恢复；失败则丢弃并请求 IDR |
| FEC 冗余包 | QUIC datagram | 丢了就算了 |
| 光标形状 | QUIC reliable stream | 重传 |

**为什么 IDR 走可靠通道**：IDR 丢失会导致解码器长时间花屏（参考链断裂）。
IDR 是低频事件（会话开始、按需请求），走可靠通道的延迟代价可接受。

**为什么 P 帧不走可靠通道**：P 帧丢失时，等待重传的时间远大于「直接丢帧 + 请求 IDR」。
这就是「宁可花屏一帧，不要冻结 50ms」。

## 6. 错误码与错误偏移

所有解析错误都带 `offset`（相对包起始），便于定位。这是调试的关键：

| 错误 | offset | 含义 |
|---|---|---|
| 缓冲不足 | 0 | `len < 32` |
| 魔数错误 | 0 | 不是 RC 包 |
| `packet_idx >= packet_cnt` | 20 | 包内序号非法 |
| `packet_cnt == 0` | 22 | 总包数为 0 |
| `payload_len` 超限 | 28 | 超过 1200 |
| `payload_len` 与实长不符 | 28 | 只有 `parse_datagram` 校验 |

## 7. 版本演进

| 规则 | 规定 |
|---|---|
| P0 版本号 | 无独立版本字段。`magic` 的最后一字节 `'1'` 即版本（`RCV1`） |
| 不兼容变更 | 改 `magic` 为 `RCV2`，旧端直接拒绝 |
| 兼容变更 | 用 `reserved` 字段或 `flags` 的保留位 |
| 能力协商 | 在 QUIC stream 上的控制通道用 protobuf 交换 `Capability` 位图 |

**禁止**在 32 字节包头里塞新字段——定长是性能前提。
新需求优先考虑放进控制通道的 protobuf 消息。

## 8. 与其他实现的互操作检查清单

如果你实现了另一个语言的对端，逐项验证：

- [ ] 能生成 §2.5 的精确 32 字节向量
- [ ] 能解析自己生成的包，字段全部正确
- [ ] `magic` 字节序正确（内存中是 `R C V 1`，不是 `1 V C R`）
- [ ] 分片后重组得到原始字节流（含 5000 字节以上的大帧）
- [ ] 空负载产生 1 个包且 `payload_len = 0`
- [ ] 只有最后一片带 `LAST`
- [ ] IDR 的每片都带 `IDR`
- [ ] 缺 1 包时 FEC 能恢复出**逐字节相同**的内容
- [ ] 缺失包在首位、中间、末尾三种情况都能恢复
- [ ] 非法的 `packet_idx`/`packet_cnt`/`payload_len` 被拒绝且 offset 正确
