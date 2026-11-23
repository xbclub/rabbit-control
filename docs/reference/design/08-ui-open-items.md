# rc-ui 未完成项清单

> 状态：截至 2026-11-21 第三轮验证。逐条带证据，完成后请就地标记闭环并写明日期与验证方式。
> 设计依据：[docs/reference/design/08-ui-design.md](08-ui-design.md)。过程记录见 TASK.md §13。

---

## 一、必须做（发布前的硬门槛）

### 1. 应用打包未做

- **证据**：`crates/rc-ui/tauri.conf.json` 中 `"bundle": { "active": false }`；`ls target/debug/bundle` 无产物。
- **影响**：无法分发 `.app` / `.dmg`，用户拿不到可安装的产物。
- **做法**：
  1. `bundle.active = true`，配置 `bundle.targets`（macOS: `["app", "dmg"]`）。
  2. 补齐 `bundle.icon` 多尺寸图标（现仅有一个 32×32 纯色 PNG，`icons/icon.png`）。
  3. macOS 需签名 —— 项目已有 `RabbitControl Dev` 固定证书（AGENTS.md §八），发布需换正式证书或做 notarization。
  4. 验证：`cargo tauri build`（或 `tauri build`）产出 `target/release/bundle/`，双击 `.app` 能启动且界面正常。
- **注意**：打包后会重走一次 release 编译，gpui/tauri 依赖树较大，需预留构建时间。

### 2. CSP 未配置

- **证据**：`"security": { "csp": null }`。
- **影响**：WebView 无内容安全策略，属安全加固缺失（XSS 面放大）。
- **做法**：设为 `"default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:"`（内联样式目前是必需的，因为样式写在 HTML 内联 `style=` 属性与 `<style>` 块里，需实测确认是否要保留 `unsafe-inline`）。
- **验证**：配置后真窗口能正常渲染（`cargo run -p rc-ui` + 截图），控制台无 CSP violation。

---

## 二、属 M2-6 后续里程碑（本期范围外）

### 3. 真实后端接线未做（§8.4 十二项）

- **证据**：全部数据来自 `crates/rc-ui/ui/assets/js/mock.js`（设计文档 §6.0 mock 总表）。
- **清单**（对应设计文档 §8.4）：
  | 接线点 | 当前 mock | 目标 |
  |---|---|---|
  | 侧栏/P3 设备列表 | mock 设备表 | 设备注册与发现 |
  | P2 本设备 ID/验证码 | 静态 `183 435 140` / `9ZGQ30ES` | 鉴权/信令（临时码生成与刷新） |
  | P1 进入桌面 / P2 连接 | 打开静态 S1 / Toast | 发起会话（打洞/中继） |
  | S1 画面 | `STAGE_BG` 占位 | 见第 4 项 |
  | S1 计时器 / S4 HUD | 逻辑时钟 / 静态值 | 会话统计（字段已对齐 `LoopbackReport`） |
  | S3 画质/帧率/中转 | 勾选 + Toast | 编码参数下发 |
  | S2 安全状态 | mock | 被控端状态上报 |
  | W2 文件传输 | mock 列表 + 假队列 | rc-net reliable stream |
  | W3 终端 | 静态壳 | PTY 会话 |
  | W4 端口映射 | mock | QUIC 端口转发 |
  | W1 设置 | `localStorage` | 本地持久化（`tauri-plugin-store`） |
  | O2 检查更新 | Toast | `tauri-plugin-updater` |
  | W1 开机自启 | 内存态 | `tauri-plugin-autostart`（可立即接真） |

### 4. S1 视频画面为占位（需 §8.5 spike）

- **证据**：`session.html` 画面区为 `STAGE_BG` + 居中灰字「画面接入将在 M2-6 桥接视频层」。
- **两条候选路径**（设计文档 §8.5）：
  - **A 原生层叠加**（推荐先验证）：取窗口原生句柄，把 `rc-render-macos::MetalRenderer` 的 `CAMetalLayer` 作为子层插入视频容器区域。零拷贝、延迟最低，与 M1 Gate 呈现路径一致；需处理层叠加顺序与 resize 同步。
  - **B 帧转 Web**（fallback）：解码帧经 IPC/共享内存送前端，Canvas/WebGL/WebGPU 上屏。实现简单但多一次拷贝与 IPC 开销。
- **要求**：开工第一件事做**最小 spike**，只验证「原生 Metal 层能否正确嵌入 Tauri 窗口并随窗口缩放」，不做界面。

---

## 三、低优先级 / 环境项

### 5. `just` 命令本机不可用

- **证据**：`which just` → not found。
- **影响**：`justfile` 里已写好的 `c`/`ct`/`t`/`cw`/`ta`/`f`/`v`/`ui`/`ui-build`/`ui-preview` 等 recipe 无法执行。
- **说明**：项目既有环境问题，非本次引入。装 `just` 后即可用（`brew install just`）。

### 6. 1120×720 最小尺寸布局未实测

- **证据**：`assets/css/pages/main.css:87` 与 `transfer.css:31` 有 `min-width: 1120px; min-height: 720px`，但未实际缩窗截图验证布局是否溢出。
- **做法**：真窗口缩到 1120×720 截图，检查侧栏/工具条/双栏是否被挤压或出现横向滚动。

### 7. W2 双栏未联动滚动

- **证据**：设计文档 §10 已列为未决项，本期两栏独立滚动。
- **倾向**：M2-6 前不必做。

### 8. O1「调整顺序」、O2 占位项为 Toast

- **证据**：点击后仅显示「功能开发中」或「当前已是最新版本」。
- **说明**：设计文档 §6 明确为占位行为，非缺陷。

---

## 四、验证过程中观察到的现象（非缺陷，但影响自动化）

### 9. 浮层打开时 AX 索引整体偏移

- **现象**：本轮用 computer use 关闭 O1 时，因未重新取 AX 树，沿用了浮层打开前的索引 `12`（原为侧栏「全部设备」），结果点到侧栏导致路由从 P1 跳到 P3，而非关闭弹窗。
- **结论**：应用行为正确（浮层关闭与路由互不干扰），但**任何自动化脚本在读元素前必须重新调用 `getAXState` 取最新索引**，不能跨状态复用索引。
- **记录目的**：给后续写 UI 自动化测试的人一条硬规则。

---

## 五、本轮已闭环（供对照，勿重复处理）

| 原未完成项 | 闭环方式 | 日期 |
|---|---|---|
| 多窗口 IPC 的 ACL 授权缺失 | 新建 `permissions/default.toml` 定义 `allow-window-open` | 2026-11-21 |
| 多窗口端到端未验证 | CG 窗口列表实测：设置 660×640 + 主窗，TOTAL=2 | 2026-11-21 |
| `backdrop-filter` 在 WKWebView 未实测 | O1/O2/O3/S3 截图确认背景真实模糊透出 | 2026-11-21 |
| 浮层真窗口验收未做 | 9 界面 computer-use 点击 + AX 断言 + 截图三重验证 | 2026-11-21 |
| Rust 侧多窗口未实现 | `src/window.rs`：`ToolWindow` 枚举 + 2 个 IPC 命令 + 5 个单测 | 2026-11-21 |
| 双交通灯缺陷 | `titleBarStyle: Overlay` + `hiddenTitle` + `.is-tauri` 隐藏自绘灯 | 2026-11-21 |
