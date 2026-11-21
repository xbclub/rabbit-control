#!/usr/bin/env bash
# macOS 开发期固定签名运行器（RabbitControl Dev）
#
# 目的：解决 Apple Silicon 上 ad-hoc 签名每次重编译 CDHash 变化导致 macOS TCC
# （屏幕录制 / 辅助功能 / 输入监控）权限反复失效的问题。
#
# 原理：
#   1. 使用登录钥匙串中固定的自签名代码签名证书 "RabbitControl Dev"；
#   2. 将待测 Mach-O（rc-cli 或任意 cargo test 二进制）封装进固定路径的
#      target/dev-app/RabbitControlDev.app（CFBundleIdentifier=com.rabbitcontrol.dev）；
#   3. 通过 LaunchServices (`open -n -W`) 启动，使 macOS TCC 将 Responsible Process
#      精确归属到 com.rabbitcontrol.dev（证书哈希 + Bundle ID），重编译重签后权限永久保持。
#
# 用法：
#   ./scripts/macos-dev-run.sh check-cert
#   ./scripts/macos-dev-run.sh cli [--release] -- <rc-cli 参数...>
#   ./scripts/macos-dev-run.sh test <crate> [--release] -- <test 二进制参数...>
#   ./scripts/macos-dev-run.sh open-settings

set -euo pipefail

CERT_NAME="${RC_DEV_CERT:-RabbitControl Dev}"
BUNDLE_ID="${RC_DEV_BUNDLE_ID:-com.rabbitcontrol.dev}"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP_DIR="$REPO_ROOT/target/dev-app/RabbitControlDev.app"
EXEC_NAME="rc-dev-runner"

die() {
    echo "错误: $*" >&2
    exit 1
}

ensure_cert() {
    if ! security find-identity -p codesigning 2>/dev/null | grep -Fq "\"$CERT_NAME\""; then
        die "登录钥匙串中未找到代码签名证书 \"$CERT_NAME\"。请先在登录钥匙串创建或导入该代码签名证书。"
    fi
}

package_and_sign() {
    local src_bin="$1"
    [[ -x "$src_bin" ]] || die "找不到可执行文件: $src_bin"

    ensure_cert

    rm -rf "$APP_DIR"
    mkdir -p "$APP_DIR/Contents/MacOS"
    cp "$src_bin" "$APP_DIR/Contents/MacOS/$EXEC_NAME"
    chmod +x "$APP_DIR/Contents/MacOS/$EXEC_NAME"

    cat > "$APP_DIR/Contents/Info.plist" << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>${EXEC_NAME}</string>
    <key>CFBundleIdentifier</key>
    <string>${BUNDLE_ID}</string>
    <key>CFBundleName</key>
    <string>RabbitControlDev</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.0</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>LSUIElement</key>
    <true/>
</dict>
</plist>
EOF

    codesign --force --deep --sign "$CERT_NAME" --identifier "$BUNDLE_ID" "$APP_DIR" >/dev/null
    codesign --verify --deep --strict "$APP_DIR"
}

# 启动封装好的 App 并透传 stdout/stderr。
# 参数 1: 模式（cli | test | test-list），用于弥补 `open -W` 不回传子进程退出码的限制。
run_packaged_app() {
    local mode="$1"
    shift

    local out_file err_file open_err status=0
    out_file="$(mktemp)"
    err_file="$(mktemp)"
    open_err="$(mktemp)"

    if [[ $# -gt 0 ]]; then
        open -n -W --stdout "$out_file" --stderr "$err_file" "$APP_DIR" --args "$@" 2>"$open_err" || status=$?
    else
        open -n -W --stdout "$out_file" --stderr "$err_file" "$APP_DIR" 2>"$open_err" || status=$?
    fi

    # macOS `open -W` 已知竞态：极短命令（如 `--list` / `probe`）在 `open` 挂上
    # 进程监听之前就已退出，会打印 `Unable to block on application...` 并返回 1，
    # 实际程序已正常跑完。此时视为正常完成，其他错误照常透传。
    if [[ "$status" -ne 0 ]] && grep -Eq "Unable to block on application" "$open_err"; then
        status=0
    elif [[ -s "$open_err" ]]; then
        cat "$open_err" >&2
    fi

    cat "$out_file"
    if [[ -s "$err_file" ]]; then
        cat "$err_file" >&2
    fi

    if [[ "$status" -eq 0 ]]; then
        case "$mode" in
            test)
                if grep -Fq "test result: FAILED" "$out_file" || ! grep -Fq "test result: ok." "$out_file"; then
                    status=1
                fi
                ;;
            cli)
                if grep -Eq "^错误：|Gate 未通过" "$err_file"; then
                    status=1
                elif grep -Fq "loopback 开始" "$err_file" && ! grep -Fq "loopback 完成" "$err_file"; then
                    # 进程在 loopback 途中异常终止（如 SIGTRAP/SIGSEGV），open -W 可能仍返回 0
                    echo "错误: rc-cli 在 loopback 过程中异常退出（未输出完成摘要）" >&2
                    status=1
                fi
                ;;
            test-list) ;;
        esac
    fi

    rm -f "$out_file" "$err_file" "$open_err"
    return "$status"
}

cmd_check_cert() {
    ensure_cert
    security find-identity -p codesigning | grep -F "\"$CERT_NAME\""
    if [[ -d "$APP_DIR" ]]; then
        echo "=== 当前 App 签名需求 ==="
        codesign -d -r- "$APP_DIR" 2>&1
    fi
}

cmd_cli() {
    local profile="debug"
    local build_args=()
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --release)
                profile="release"
                build_args+=(--release)
                shift
                ;;
            --)
                shift
                break
                ;;
            *)
                break
                ;;
        esac
    done

    (cd "$REPO_ROOT" && cargo build -p rc-cli ${build_args[@]+"${build_args[@]}"} >/dev/null)
    local bin="$REPO_ROOT/target/$profile/rc-cli"
    package_and_sign "$bin"
    run_packaged_app cli "$@"
}

cmd_test() {
    [[ $# -ge 1 ]] || die "用法: $0 test <crate> [--release] [-- <test 参数...>]"
    local crate="$1"
    shift
    local cargo_args=(-p "$crate" --no-run --message-format=json)
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --release)
                cargo_args+=(--release)
                shift
                ;;
            --)
                shift
                break
                ;;
            *)
                break
                ;;
        esac
    done

    local bin
    bin="$(cd "$REPO_ROOT" && cargo test "${cargo_args[@]}" | python3 -c '
import json, sys
bins = []
for line in sys.stdin:
    try:
        msg = json.loads(line)
    except Exception:
        continue
    if msg.get("reason") == "compiler-artifact" and msg.get("executable"):
        if msg.get("profile", {}).get("test"):
            bins.append(msg["executable"])
print(bins[-1] if bins else "")
')"
    [[ -n "$bin" ]] || die "未能找到 crate=$crate 的测试二进制"

    local mode="test"
    for arg in "$@"; do
        if [[ "$arg" == "--list" ]]; then
            mode="test-list"
            break
        fi
    done

    package_and_sign "$bin"
    run_packaged_app "$mode" "$@"
}

cmd_open_settings() {
    open "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
}

main() {
    [[ $# -ge 1 ]] || die "用法: $0 {check-cert|cli|test|open-settings}"
    local sub="$1"
    shift
    case "$sub" in
        check-cert) cmd_check_cert "$@" ;;
        cli) cmd_cli "$@" ;;
        test) cmd_test "$@" ;;
        open-settings) cmd_open_settings "$@" ;;
        *) die "未知子命令: $sub" ;;
    esac
}

main "$@"
