#!/usr/bin/env bash
# 磁盘清理。用法：./scripts/clean-disk.sh
#
# 为什么需要：Rust 的 target/ 会随时间膨胀到 20-40GB。
# 本项目通过 debug=1 + 依赖 opt-level=2 已把常态压到 ~100MB，
# 但长期开发仍会积累多版本编译器的产物。
set -euo pipefail

cd "$(dirname "$0")/.."

before=$(du -sh target 2>/dev/null | cut -f1 || echo "0")
echo "清理前: $before"

# 1. 移除 14 天前的产物（需要 cargo-sweep，可选）
if command -v cargo-sweep >/dev/null 2>&1; then
    echo "→ cargo sweep --time 14"
    cargo sweep --time 14 || true
else
    echo "⚠ cargo-sweep 未安装，跳过时间清理"
    echo "  安装: cargo install cargo-sweep"
fi

# 2. 增量缓存（安全删除，下次重建）
if [ -d target/debug/incremental ]; then
    incr=$(du -sh target/debug/incremental | cut -f1)
    echo "→ 删除增量缓存 ($incr)"
    rm -rf target/debug/incremental
fi

# 3. 旧工具链的产物
if command -v cargo-sweep >/dev/null 2>&1; then
    echo "→ cargo sweep --installed"
    cargo sweep --installed || true
fi

after=$(du -sh target 2>/dev/null | cut -f1 || echo "0")
echo "清理后: $after"
