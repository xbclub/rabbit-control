# 日常开发命令。
#
# 用法：`just` 列出所有命令；`just c rc-sched` 检查单个 crate。
#
# 核心纪律（见 docs/07-engineering-velocity.md）：
#   内循环一律用 `c`（cargo check），不要用 `cargo build`。

# 默认：列出命令
default:
    @just --list

# ── 内循环（最常用）──────────────────────────────────────────

# 类型检查单个 crate（最快，1-3 秒）
c crate:
    cargo check -p {{crate}}

# 检查测试代码能否编译（提前发现测试里的错误）
ct crate:
    cargo check -p {{crate}} --tests

# 跑单个 crate 的测试
t crate:
    cargo test -p {{crate}}

# 检查整个 workspace
cw:
    cargo check --workspace

# ── 外循环（提交前）────────────────────────────────────────

# 全部测试
ta:
    cargo test --workspace

# 格式化 + lint（CI 会跑同样的命令）
f:
    cargo fmt --all
    cargo clippy --workspace --all-targets -- -D warnings

# 完整验证：格式 + lint + 全部测试
v: f ta
    @echo "✓ all checks passed"

# ── 性能 ───────────────────────────────────────────────────

# 基准测试（改了算法才需要跑）
bench crate:
    cargo bench -p {{crate}}

# ── 维护 ───────────────────────────────────────────────────

# 查看磁盘占用
size:
    @du -sh target 2>/dev/null || echo "target/ not found"
    @du -sh target/debug/* 2>/dev/null | sort -rh | head -5

# 清理磁盘（保留 14 天内的产物）
clean-disk:
    ./scripts/clean-disk.sh

# 彻底重置（谨慎，下次全量重编）
clean:
    cargo clean

# 列出所有 crate 及其大小
crates:
    @find crates -maxdepth 2 -name Cargo.toml | sed 's|/Cargo.toml||' | while read d; do \
        printf "%-32s %s\n" "$(basename $d)" "$(wc -l < $d/src/lib.rs 2>/dev/null || echo 0) 行"; \
    done

# ── macOS 固定签名真机测试（RabbitControl Dev 证书）────────────

# 检查 macOS 开发证书与 App 签名需求
dev-cert:
    ./scripts/macos-dev-run.sh check-cert

# 用已授权的固定签名 App 探测 macOS 权限与硬件后端
dev-probe:
    ./scripts/macos-dev-run.sh cli -- probe --json

# 用已授权的固定签名 App 运行 rc-cli（示例：just dev-cli --release -- loopback --backend real --duration 15s）
dev-cli *args:
    ./scripts/macos-dev-run.sh cli {{args}}

# 用已授权的固定签名 App 运行指定 crate 的真机测试（示例：just dev-test rc-capture-macos -- --ignored --nocapture）
dev-test crate *args:
    ./scripts/macos-dev-run.sh test {{crate}} {{args}}

# ── rc-ui（Tauri v2 桌面端 + macOS 打包）──────────────────────

# 用固定开发签名证书（RabbitControl Dev）启动桌面端 App（复用 macOS 屏幕录制/辅助功能权限）
dev-ui *args:
    ./scripts/macos-dev-run.sh ui {{args}}

# 打包 macOS 签名 .app 与 .dmg 安装镜像（产物在 target/dist/）
package-macos *args:
    ./scripts/package-macos.sh {{args}}

# 直接以 debug 模式运行桌面端（适合纯 UI 调试，无固定签名）
ui:
    cargo run -p rc-ui

# 在浏览器里预览静态前端（逐屏比对设计稿用，默认端口 8791；纯静态文件）
ui-preview port="8791":
    cd crates/rc-ui/ui && python3 -m http.server {{port}}
