# T01 工程基座

> 状态：✅ 已完成。本文档作为记录与验收依据保留。

## 背景

AI 主导编码时，「改代码 → 看到结果」的回路长度是乘性影响，不是加性影响。
编译等 3 分钟 vs 0.09 秒，差的是每个任务能试错的轮次。
所以工程基座必须先做，且指标必须实测。

## 交付物

| 文件 | 说明 |
|---|---|
| `Cargo.toml` | workspace + profile（dev `debug=1`、依赖 `opt-level=2`） |
| `.cargo/config.toml` | 链接器加速（lld/mold，默认注释） |
| `justfile` | 日常命令（`just c <crate>` 等） |
| `.config/nextest.toml` | 测试并行与超时配置 |
| `.github/workflows/ci.yml` | 三平台 fmt + clippy + test |
| `scripts/clean-disk.sh` | 磁盘清理 |
| 6 个 crate 参考实现 | types / diff / sched / fec / proto / trace |

## 实测结果

| 指标 | 实测 | 门槛 | 结论 |
|---|---|---|---|
| 单文件增量 `cargo check -p rc-sched` | **0.09s** | ≤3s | ✅ 优于门槛 33× |
| `cargo test --workspace` | 1.4s / 107 测试 | ≤90s | ✅ |
| 冷编译（含依赖拉取） | 52s | ≤8min | ✅ |
| `target/` 体积 | 151MB（增量 123MB 可清，清后 29MB） | ≤10GB | ✅ |
| clippy `-D warnings` | 无警告 | 无警告 | ✅ |

### 关键配置（不要改）

```toml
# Cargo.toml
[profile.dev]
debug = 1          # debug=2 会让 target/ 涨到 20-40GB

[profile.dev.package."*"]
opt-level = 2      # 依赖以优化级别编译一次，运行快 10-50x 且不再重编
```

`debug=1` 是磁盘占用的最大单点优化（-60~80%），`opt-level=2` 是运行速度的最大单点优化。

## 验收

```bash
cargo check --workspace && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings
```

## 为什么达不到门槛也不换语言

见 [ADR-0003](../adr/0003-final-rust-decision.md)：
低延迟要求媒体管线 in-process，排除了子进程方案；而 Go 走 cgo 会把风险转移到
「AI 最弱的一环：运行时段错误排查」。
