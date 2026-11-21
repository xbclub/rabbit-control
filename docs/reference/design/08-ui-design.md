# 08 — UI 设计规范（rc-ui）

> 状态：设计稿（未实现）。实现载体：**GPUI（zed 官方 crate v0.2.2）+ gpui-kit（longbridge，v0.10.0）**，框架决策记录见 TASK.md §13。
> 本文是 rc-ui（M2-6 控制端 UI）的唯一视觉与交互依据；**本文自包含：实现者不需要参考图即可实现**，全部 mock 数据见 §6.0。
> 视觉风格来源：UU 远控 macOS 客户端 12 张截图（2026-11-21 收集，留档于设计轮记录）。下文以 P1–P3（主窗口页面）、O1–O3（浮层）、W1–W4（独立窗口）、S1–S4（会话中）编号引用。

---

## 1. 设计原则

1. **macOS 原生质感**：白/浅灰底、轻描边卡片、极轻阴影、系统字体，不做重装饰。
2. **状态语义色恒定**：绿=在线/成功，蓝=主操作/本机，琥珀=需关注，红=危险/退出。全 App 不混用。
3. **大数字作视觉锚点**：设备 ID、验证码用超大号灰字，是页面的视觉主角。
4. **中文优先**：所有界面文案为简体中文；西文走系统回退（SF Pro）。
5. **浮层贴近原生**：popover/菜单尽量模仿 macOS 原生 NSMenu / NSPopover 观感（半透明白底、蓝色高亮、勾选态、红色破坏项）。
6. **本期为静态原型**：不接真实网络/会话/文件系统，一切数据来自 §6.0 mock 清单；交互以「本地状态变化 + Toast 反馈」模拟。

---

## 2. 信息架构与窗口清单

| 编号 | 名称 | 形态 | 尺寸（默认 / 最小） | 出处 |
|---|---|---|---|---|
| 主窗口 | 设备管理主窗 | 单窗多页（侧栏路由） | 1440×900 / 1120×720 | P1–P3 |
| O1 | 更多工具 | 居中模态（挂主窗口） | 卡片宽 440 | P1「⋮」出口 |
| O2 | 右上下拉菜单 | popover（右上汉堡锚点） | 宽 200 | 全局 |
| O3 | 账户气泡 | popover（账户图标锚点，顶部箭头朝上） | 宽 300×高 220 | 全局 |
| W1 | 设置 | 独立窗口 | 660×640，**不可调** | O2 出口 |
| W2 | 文件传输 | 独立窗口 | 1400×860 / 1120×720 | P1 工具条出口 |
| W3 | 终端 | 独立窗口 | 1200×760 / 720×480 | P1 工具条出口 |
| W4 | 端口映射 | 独立窗口 | 1280×780 / 960×600 | P1 工具条出口 |
| S1 | 远控会话窗口 | 全屏优先/窗口化两态（含只读「观看模式」变体） | 随远端分辨率，窗口态 1280×800 | P1「进入桌面」/工具条出口 |
| S2 | 安全状态气泡 | popover（S1 工具栏盾图标锚点，顶部箭头朝上） | 宽 300 | S1 |
| S3 | 控制中心菜单 | 原生风菜单+子菜单 | 主 220 / 子 300 | S1 |
| S4 | 性能 HUD | 常驻悬浮块（S1 右下角，可隐藏） | 自适应约 150×170 | S1 |

导航状态机（主窗口侧栏 → 页面）：

```
[我的设备] 设备行点击 ──→ P1 设备主页（该设备）
          全部设备行 ──→ P3 全部设备
[远程协助] 开始协助行 ──→ P2 远程协助
          收藏设备行 ──→ P3 全部设备（收藏过滤，本期同 P3）
```

**App 初始状态**：启动即打开主窗口；侧栏两组全部展开；默认选中设备「yii's MacBook Pro」→ 显示 P1；无任何浮层打开。

---

## 3. 设计规范（Design Tokens）

实现位置约定：`crates/rc-ui/src/theme.rs`，本文 token 名即代码常量名。**业务代码禁止裸写色值/字号，一律引用 token。**

### 3.1 色彩

| Token | 值 | 用途 |
|---|---|---|
| `BG_WINDOW` | `#FFFFFF` | 内容区底 |
| `BG_SIDEBAR` | `#F5F5F7` | 侧栏底 |
| `BG_HOVER` | `#E8E8EA` | 侧栏/列表选中与悬停 |
| `BG_CARD` | `#F8F9FA` | 分组卡片底（W1 设置卡、P2 卡） |
| `BG_TOOL_TILE` | `#EEF3FF` | O1 前四项浅蓝工具卡 |
| `BG_ZEBRA` | `#FAFAFA` | W2 文件行斑马纹 |
| `ACCENT` | `#3370FF` | 主蓝：链接、选中、主按钮、图标方块、进度条 |
| `ACCENT_PRESSED` | `#2A5FD9` | 主按钮按压 |
| `TEXT_PRIMARY` | `#1D2129` | 主文本 |
| `TEXT_SECONDARY` | `#86909C` | 次文本/说明/表头/「连接中」 |
| `TEXT_DISPLAY_NUM` | `#4E5969` | 大 ID/验证码数字 |
| `TEXT_DISABLED` | `#C9CDD4` | 禁用文本 |
| `ON_ACCENT` | `#FFFFFF` | 蓝底上的文字 |
| `ONLINE` | `#23C343` | 在线绿点/成功勾 |
| `SUCCESS_BG` / `SUCCESS_FG` | `#E8FFEA` / `#00B42A` | 「远端」绿 pill /「已发送」 |
| `LOCAL_PILL_BG` / `LOCAL_PILL_FG` | `#E8F0FF` / `#3370FF` | 「本机」蓝 pill |
| `DANGER` | `#F53F3F` | 退出远控、删除、通知角标、错误描边、失败 |
| `WARNING` / `WARNING_BG` | `#FF7D00` / `#FFF7E8` | 「需关注」琥珀 pill |
| `PILL_ONLINE_BG` / `PILL_ONLINE_FG` | `#1D2129` / `#FFFFFF` | 「在线」黑 pill |
| `DIVIDER` | `#F2F3F5` | 1px 分隔线 |
| `STROKE` | `#E5E6EB` | 输入框/次按钮描边/Switch 关态 |
| `TERMINAL_BG` | `#1E1E20` | W3 终端底 |
| `TERMINAL_FG` | `#FFFFFF` | 终端正文 |
| `TERMINAL_MUTED` | `#98989D` | 终端时间戳/提示符灰/HUD 次行 |
| `HUD_BG` | `rgba(28,28,30,0.82)` | S4 HUD 半透明黑底 |
| `POPOVER_BG` | `rgba(255,255,255,0.88)` | O3/S2/S3 毛玻璃近似底 |
| `STAGE_BG` | `#2B2B2E` | S1 画面占位底 |
| `MASK` | `rgba(0,0,0,0.30)` | Modal 遮罩 |

### 3.2 字体

| Token | 值 | 用途 |
|---|---|---|
| `FONT_SANS` | `PingFang SC`（系统回退 SF Pro Text） | 全部正文 |
| `FONT_MONO` | `SF Mono, Menlo` | HUD、终端、路径、计时器 |
| `SIZE_DISPLAY` | 44px / semibold | 设备 ID、验证码大数字（`TEXT_DISPLAY_NUM`） |
| `SIZE_H1` | 20px / semibold | 窗口标题区（W2/W4 设备名）、Modal 标题（O1）、账户手机号 |
| `SIZE_H2` | 16px / semibold | 卡片标题（本设备/远控伙伴设备/传输列表/映射列表）、P3 分组头 |
| `SIZE_TITLE` | 15px / semibold | 主窗口标题栏设备名、P3 行名称 |
| `SIZE_BODY` | 14px / regular | 正文、列表行、菜单项 |
| `SIZE_SECONDARY` | 13px / regular | 说明文字 |
| `SIZE_CAPTION` | 12px / regular | pill、表头、HUD、错误文案 |
| `SIZE_TERMINAL` | 13px / mono | 终端 |

### 3.3 间距 / 尺寸

| Token | 值 | 用途 |
|---|---|---|
| `SP_UNIT` | 4px | 间距基数（全部间距为 4 的倍数） |
| `SP_PAGE` | 24px | 内容区左右边距 |
| `SP_CARD_IN` | 20px | 卡片内边距 |
| `SIDEBAR_W` | 216px | 侧栏宽（本期固定，不做折叠宽度） |
| `TITLEBAR_H` | 52px | 主窗口/独立窗口标题栏 |
| `SESSION_BAR_H` | 48px | 会话窗口顶部工具栏 |
| `ROW_H` | 44px | 列表行高 |
| `TOOLBAR_H` | 56px | P1 底部工具条 |

### 3.4 圆角 / 阴影

| Token | 值 | 用途 |
|---|---|---|
| `R_CARD` | 12px | 大卡片、预览卡、窗口内分组卡 |
| `R_ROW` | 8px | 列表行、按钮、输入框、工具卡 |
| `R_POPOVER` | 16px | 模态/气泡 |
| `R_PILL` | 999px | 胶囊、进度条 |
| `SHADOW_CARD` | y=1 blur=3 rgba(0,0,0,0.06) | 轻卡片 |
| `SHADOW_POPOVER` | y=8 blur=24 rgba(0,0,0,0.16) | 弹层 |

### 3.5 图标

gpui-kit 内置 **Lucide** 图标集（`gpui-kit-assets`），本期全部用 Lucide，不引自定义 SVG。**注意：Lucide 无品牌图标（没有 apple / Android 机器人 / Windows 徽标），设备类型一律用形状图标表达**；§3.5 名称按 Lucide 现行命名书写，实现时以 gpui-kit `IconName` 实际枚举为准（如个别改名以枚举为准并在实现记录中回填本文）。

| 界面语义 | Lucide 图标名 |
|---|---|
| 我的设备 / 全部设备 | `monitor-smartphone` / `layout-grid` |
| Windows 设备 / Mac 设备 / 手机 / 平板 | `app-window` / `laptop` / `smartphone` / `tablet` |
| 远程协助 / 开始协助 / 收藏设备 | `users` / `handshake` / `bookmark` |
| 文件传输 / 观看模式 / 终端 / 端口映射 | `folder-symlink` / `square-play` / `square-terminal` / `unplug` |
| 重启 / 关机 | `rotate-ccw` / `power` |
| 通知 / 账户 / 菜单 / 更多 | `bell` / `circle-user` / `menu` / `ellipsis` |
| 复制 / 分享 / 可见性 / 设置 | `copy` / `share` / `eye` / `settings` |
| 添加 / 删除 / 信息 / 展开 | `circle-plus` / `trash-2` / `info` / `chevron-down` |
| 安全锁定 / 盾 / 全屏 / 退出 | `lock` / `shield` / `maximize` / `log-out` |
| 画质 / 窗口 / 声音 / 外设 / 工具 | `image` / `app-window` / `volume-2` / `joystick` / `box` |
| 控制中心 / 发送 / 折叠分组 | `layout-dashboard` / `send` / `chevron-up` |
| 返回 / 上级 / 刷新 / 新建文件夹 | `arrow-left` / `arrow-up` / `rotate-cw` / `folder-plus` |
| 开始 / 暂停 / 取消 / 清除 / 编辑 | `play` / `pause` / `x` / `eraser` / `pencil` |
| 警示 / 静音 / 键盘 / 网络 | `triangle-alert` / `volume-x` / `keyboard` / `globe` |

### 3.6 布局定位参数（元素级）

**本节数值为全局默认；逐屏规格未另行标注的定位/间距/图标尺寸一律以本节为准**——任何元素的「位置」由此节 + 该屏线框唯一确定。

| 参数 | 值 | 适用 |
|---|---|---|
| `PAD_TITLEBAR_X` | 12px | 交通灯距窗口左缘；灯直径 12、组内间距 8 |
| `GAP_ICONBAR` | 8px | 标题栏右上图标组内间距；组距窗口右缘 16 |
| 标题栏上下文缩进 | 距侧栏分界线 16px | P1 的 pill+设备名+`ellipsis`、P2/P3 的页面标题 |
| 侧栏 | 内边距：左右 12 / 顶 12；分组头高 40（图标 20px `ACCENT` + 文字 14 semibold + `chevron-up` 16px `TEXT_SECONDARY`）；组头与首行间距 4；行间距 2 | 行内：`StatusDot` → 6px → 图标方块 22×22（圆角 6）→ 8px → 名称；选中行左右各缩进 4 |
| 图标默认尺寸 | 按钮内/表格操作列 16px；工具条与分组头 20px；菜单项 18px | 全局；逐屏另有标注以屏为准 |
| ghost 图标默认色 | `TEXT_SECONDARY`，悬停 `TEXT_PRIMARY` | 全局 |
| P1 预览卡 | 距标题栏 48px **顶对齐**、水平居中；工具条紧贴其下（间距 0）；「进入桌面」文字与箭头间距 8 | |
| P2 布局 | 内容区距标题栏 32；卡间距 24；卡内边距 20；卡 1 左 ID 栏 flex(1)、右验证栏固定宽 460、列间距 48；验证码行内各元素间距 12；卡 2 表单行 = Input 320 + 24 + 按钮 152，行距分隔线 24，整行左对齐 | |
| P3 布局 | 分组头高 36；组头与首行间距 12；行宽 = 内容区−2×24 | |
| O1 | 卡内边距 24；行间距 16；底部按钮区距列表 16、高 56 | |
| Popover 定位 | 距锚点垂直 8px；水平：O2/S3 右缘对齐锚点右缘，O3/S2 水平居中于锚点 | |
| Modal / ConfirmDialog | 垂直居中于窗口；卡内边距 24 | |
| Toast | 距窗口底缘 24、水平居中 | |
| W2 布局 | 顶部区左右内边距 24；双栏与队列卡间距 16；队列卡内边距 20 | |
| W1 布局 | 内容区距 tab 分隔线 24 | |
| W4 布局 | 内容卡距标题区 16、内边距 20 | |

---

## 4. 基础组件规格

| 组件 | 规格 | 状态 |
|---|---|---|
| `Pill` | 高 24，`R_PILL`，13px，左右内边距 10；五款语义：在线（`PILL_ONLINE_BG` 底白字+前置 `ONLINE` 绿点 8px）、本机（蓝）、远端（绿）、需关注（琥珀）、观看模式（灰：`BG_HOVER` 底 `TEXT_SECONDARY` 字） | 静态 |
| `StatusDot` | 8px 圆点；`ONLINE`/`TEXT_DISABLED` 两态 | 静态 |
| `Badge` | 图标右上角角标：16×16 `DANGER` 圆 + 10px 白字数字，偏移 (-4,-4)；无通知时隐藏 | 数字≥1/隐藏 |
| `ButtonPrimary` | 高 36，`R_ROW`，`ACCENT` 底 `ON_ACCENT` 字；按压 `ACCENT_PRESSED`；禁用=`STROKE` 底 `TEXT_DISABLED` 字 | 正常/悬停/按压/禁用 |
| `ButtonSecondary` | 高 36，白底 `STROKE` 描边，`TEXT_PRIMARY` 字 | 同上 |
| `ButtonDanger` | 高 36，`DANGER` 底白字（ConfirmDialog 确认键） | 正常/悬停/禁用 |
| `ButtonGhost` | 无底，悬停 `BG_HOVER`，用于图标按钮（40×40 命中区） | 同上 |
| `Switch` | iOS 风：52×32，开=`ACCENT`，关=`STROKE`；动画 150ms | 开/关/禁用 |
| `Checkbox` | 16×16，`R_ROW`/2（4px）圆角；未选=白底 `STROKE` 描边，选中=`ACCENT` 底白✓ | 未选/选中/禁用 |
| `ProgressBar` | 高 6 `R_PILL`，底 `STROKE`，填充 `ACCENT`；右侧 `SIZE_CAPTION` 百分比文字 | 0–100 |
| `Input` | 高 36，白底 `STROKE` 描边 `R_ROW`，聚焦描边 `ACCENT`，错误描边 `DANGER`；右内嵌下拉箭头变体；错误态行下方 12px `DANGER` 文案 | 聚焦/禁用/占位/错误 |
| `Select` | 同 Input 外观 + 右侧 `chevron-down`；点开 popover 单选列表，选中项前置 ✓ | 展开态 |
| `TabsTop` | 顶部居中图标+文字 tab；选中=白底 `SHADOW_CARD` 胶囊卡+`ACCENT` 图标，未选=`TEXT_SECONDARY` 图标+`TEXT_PRIMARY` 字（W1） | 选中/未选 |
| `ListRow` | 高 44（P3 卡片化行高 64），左图标方块（24×24 / P3 36×36，`R_ROW`，`ACCENT` 底白图标，可简化为单色）+名称+右侧徽标/操作；选中 `BG_HOVER` | 悬停/选中 |
| `DataTable` | 表头 `SIZE_CAPTION` `TEXT_SECONDARY` 底部 1px `DIVIDER`；行高 44（W2 文件行 40），行间仅 `DIVIDER` 分隔；右对齐数字列；列头可带排序 chevron | 排序/勾选 |
| `Popover` | `POPOVER_BG` 半透明白+`SHADOW_POPOVER`+`R_POPOVER`；箭头变体：箭头位于 popover 边缘指向锚点（锚点在上方→箭头在顶部朝上） | 开/关 |
| `Modal` | 居中卡 `R_POPOVER`+`SHADOW_POPOVER`+`MASK` 遮罩；底部左次按钮/右次按钮槽位 | — |
| `ConfirmDialog` | 小号 Modal 宽 320：标题 `SIZE_H2`、正文 `SIZE_SECONDARY`、底部右对齐 [取消 ButtonSecondary][确认 ButtonDanger] | — |
| `Toast` | 底部居中：`HUD_BG` 底白字 13px，高 36 `R_ROW`，左右内边距 16；显示 1.8s 后自动消失；同一时刻仅一条，新的顶替旧的 | — |
| `NativeMenu` | 模仿 NSMenu：`POPOVER_BG` 底、行高 36、悬停整行 `ACCENT` 白字、单选组选中项前置 ✓、破坏项 `DANGER` 色、快捷键右对齐 `TERMINAL_MUTED` mono；子菜单贴主菜单右侧展开 | 悬停/展开 |
| `Hud` | `HUD_BG` 底、`FONT_MONO` 12px 白（首行）/`TERMINAL_MUTED`（其余），右下角悬浮；圆角 8；行距 4 | — |
| `ToggleRow` | 设置行：左标题（+可选灰说明）、右控件（Switch 或 Select 或按钮组）；行间 1px `DIVIDER` | — |

---

## 5. 全局框架

### 5.1 主窗口（P1–P3 共用骨架）

```
┌──────────────────────────────────────────────────────────────┐
│ ●●●   │  [●在线] 设备名 ⋮                    ⧉ 🔔¹ 👤 ☰      │ ← 52px 标题栏
├───────┼──────────────────────────────────────────────────────┤
│ 侧栏   │                                                      │
│ 216px │                     内容区（按路由切换）              │
│       │                                                      │
│ 我的设备∧│                                                    │
│  · 设备A│                                                    │
│  · 设备B│                                                    │
│  · 全部设备│                                                 │
│ 远程协助∧│                                                    │
│  · 开始协助│                                                 │
│  · 收藏设备│                                                 │
└───────┴──────────────────────────────────────────────────────┘
```

- 标题栏左右分区以侧栏分界为界：左区含 macOS 交通灯（8px 间距），中左为当前页上下文——P1：`在线` Pill + 设备名（`SIZE_TITLE`）+ `ellipsis` ghost；P2：标题「远程协助」；P3：标题「全部设备」。右侧图标组四个（`copy` 多窗占位、`bell`+Badge、`circle-user`、`menu`），间距 8。
- 侧栏行规格：分组头 = 图标+14px semibold+右 `chevron-up`，点击可折叠/展开（折叠后仅显分组头）；设备行 = `StatusDot`+图标方块+名称（14px，超长尾部截断 `…`）。
- 图标方块可简化为 `ACCENT` 单色（参考来源为轻微上浅下深蓝渐变，不强制）。
- 交互：右上图标组 → O2/O3 浮层。

---

## 6. 逐屏规格

### 6.0 Mock 数据总表（自包含，实现不得引用外部参考图）

**设备（侧栏 3 台，全部在线=绿点）**：

| 名称 | 类型 | 图标 |
|---|---|---|
| DESKTOP-Q5Q8E8R | Windows | `app-window` |
| yii's MacBook Pro | Mac（默认选中） | `laptop` |
| Y 的 Mac mini | Mac | `laptop` |

**P3 全部设备（电脑 4）**：Y的 MacBook Pro（+「本机」pill）、DESKTOP-Q5Q8E8R、yii's MacBook Pro、Y 的 Mac mini。
**P3（手机/平板 4）**：iPhone 17 Pro Max（`smartphone`）、iQOO Pad2 Pro（`tablet`）、OnePlusPGP110（`smartphone`）、一加 Ace Pro 全网通版（`smartphone`）。

**W2 左栏（本机，路径 `/Users/yii/Downloads`，8 行）**：

| 名称 | 修改日期 | 类型 | 大小 |
|---|---|---|---|
| openssl-3.5.tar.gz | 2026年11月21日 | GZip 归档 | 9.44 MB |
| Antigravity.dmg | 2026年11月21日 | 磁盘映像 | 225.02 MB |
| GUI-Server.exe | 2026年11月19日 | Windows 程序 | 24.35 MB |
| logs（文件夹） | 2026年11月19日 | 文件夹 | -- |
| logs.zip | 2026年11月19日 | Zip 归档 | 7.44 MB |
| expo-report.json | 2026年11月18日 | JSON | 1.24 KB |
| notes.md | 2026年11月18日 | Markdown | 3.10 KB |
| dump-1121.log | 2026年11月17日 | 日志 | 56.72 MB |

**W2 右栏（远端，路径 `/Users/yii/Documents/mycode/data`，8 行）**：

| 名称 | 修改日期 | 类型 | 大小 |
|---|---|---|---|
| BiliBiliLiveRecorder.7z | 2026年11月07日 | 7z | 10.80 MB |
| blivedm-go（文件夹） | 2026年11月05日 | 文件夹 | -- |
| browserclient（文件夹） | 2026年11月05日 | 文件夹 | -- |
| danmaku-coder.data | 2026年11月06日 | data | 21.29 MB |
| danmaku-renderer.exe | 2026年11月07日 | exe | 15.85 MB |
| docs（文件夹） | 2026年11月07日 | 文件夹 | -- |
| README.md | 2026年11月07日 | Markdown | 2.42 KB |
| stream-config.toml | 2026年11月06日 | TOML | 1.05 KB |

**W2 传输队列（4 条）**：

| 名称 | 状态 | 大小 | 发送路径 | 接收路径 |
|---|---|---|---|---|
| BiliBiliLiveRecorder-gui-master.7z | ✓ 已发送（绿） | 10.80 MB | 本机 `/Users/yii/Downloads/…ter.7z` | 远端 `\Users\yii\data\…coder` |
| Stream-ug.na…22:11:18.har | ✓ 已发送（绿） | 6.94 KB | 本机 `/Users/yii/Downloads/…18.har` | 远端 `\Users\yii\Downloads` |
| danmaku-coder.data | 进行中 47%（蓝进度条） | 21.29 MB | 本机 `/Users/yii/Downloads/…data` | 远端 `\Users\yii\data` |
| GUI-Server.exe | ✕ 失败（红，悬浮说明「连接中断」） | 24.35 MB | 本机 `/Users/yii/Downloads/…exe` | 远端 `\Users\yii\data` |

**W2 顶部**：左「Y的 MacBook Pro」+「本机」pill；右「远端」pill +「yii's MacBook Pro」。

**W4 映射（3 行）**：

| 名称 | 本地端口 | 目标地址 | 目标端口 | 状态 |
|---|---|---|---|---|
| deepseek harness | 3080 | 127.0.0.1 | 3080 | ✓ 成功（绿） |
| ssh-tunnel | 2222 | 10.0.0.8 | 22 | ✕ 失败（红） |
| jupyter-lab | 8888 | 127.0.0.1 | 8888 | … 连接中（`TEXT_SECONDARY`） |

**S1 会话**：设备=yii's MacBook Pro；屏幕 tab 文案=`显示屏1 (Built-in Retina Display)`；计时从 `00:00:00` 起每秒 +1（注入时钟）；HUD 数据=§6.6 所列静态值。
**P2 本设备**：ID=`183 435 140`（三位一组空格分隔）；验证码=`9ZGQ30ES`。
**O3 账户**：手机号=`151******21`。

---

### P1 设备主页（侧栏选中任一设备时）

```
│         ┌──────────────────────────────────────────┐        │
│         │                                          │        │
│         │            预览卡（占位图填充）           │        │
│         │            中央：「进入桌面  →」          │        │
│         │                                          │        │
│         └──────────────────────────────────────────┘        │
│         ┌──────────────────────────────────────────┐        │
│         │ ⧉ 文件传输 │ ▶ 观看模式 │ ⌗ 终端 │ ⛓ 端口映射 │ ⋮ │
│         └──────────────────────────────────────────┘        │
```

| 区域 | 规格 |
|---|---|
| 预览卡 | 内容区水平居中，宽 = 内容区−2×48，高 480；`R_CARD`+`SHADOW_CARD`；填充 `STAGE_BG` 纯色+居中 `TEXT_SECONDARY` 13px 设备名水印（占位图来源未决，见 §10）；中央白色 16px semibold「进入桌面」+右箭头，整体可点击（悬停时箭头右移 4px）→ 打开 S1 |
| 底部工具条 | 紧贴预览卡底部，白底 `R_CARD`+`SHADOW_CARD`，高 56；五组「20px 图标+14px 文字」等宽均分，组间 1px `DIVIDER` 竖线；第五组仅 `ellipsis` 图标 → 打开 O1 |
| 交互 | 文件传输→W2；观看模式→S1 只读变体；终端→W3；端口映射→W4 |

### P2 远程协助（侧栏「开始协助」选中）

内容区两张竖排卡片，宽 = 内容区−2×48，间距 24：

**卡 1「本设备」**（`BG_CARD`，`R_CARD`，内边距 20）：

```
│ 本设备                                允许他人远程协助 [开关] │
│ ────────────────────────────────────────────────────────────│
│ 本设备 ID                    验证方式：仅使用临时验证码 ⌄     │
│                              9ZGQ30ES   👁  ⚙  [复制并分享]  │
│ 183 435 140                  每次远控结束刷新                │
```

- 「本设备」`SIZE_H2`；「允许他人远程协助」`SIZE_BODY`+Switch（默认开；关闭时卡 1 其余内容整体 40% 透明且不可交互）。
- 大 ID 与验证码均 `SIZE_DISPLAY` `TEXT_DISPLAY_NUM`；验证码行右侧 `eye`、`settings` 两个 ghost（`eye`=明文/掩码切换，`settings`=Toast 占位）+「复制并分享」ButtonSecondary → 剪贴板写入 `183 435 140 · 9ZGQ30ES` + Toast「已复制设备 ID 与验证码」。
- 「验证方式」Select（选项：仅使用临时验证码 / 自定义验证码；切换仅改标签，本期无真实逻辑）。
- 「每次远控结束刷新」`SIZE_SECONDARY` `TEXT_SECONDARY`。

**卡 2「远控伙伴设备」**：

```
│ 远控伙伴设备                                                 │
│ 通过其他设备的【设备 ID】及【设备验证码】启动远程控制          │
│ ────────────────────────────────────────────────────────────│
│ 伙伴的设备 ID                                                │
│ [ 请输入设备 ID            ⌄ ]        [    连接    ]        │
```

- 说明 `SIZE_SECONDARY` `TEXT_SECONDARY`；Input 内嵌下拉变体宽 320（下拉=历史记录，本期空列表显示「暂无历史记录」）；「连接」ButtonPrimary 宽 152，输入为空时禁用；输入非空点击 → Toast「功能开发中」（本期无真实连接）。

### P3 全部设备（侧栏「全部设备」/「收藏设备」选中）

| 区域 | 规格 |
|---|---|
| 分组头 | 「⌄ 电脑 4」：`chevron-down` + `SIZE_H2` + 计数 `TEXT_SECONDARY`；点击折叠/展开；上下分组间距 32 |
| 设备行 | ListRow 卡片化：白底 `R_ROW`+`SHADOW_CARD`，行高 64，图标方块 36×36；名称 `SIZE_TITLE`；「本机」Pill 紧随名称（仅 Y的 MacBook Pro）；行尾固定 `info` ghost + `chevron-right` ghost |
| 行间距 | 8 |
| 交互 | 行悬停 `BG_HOVER`；`info` → Toast「功能开发中」；`chevron-right`/行点击 → 选中该设备并路由到 P1 |

### O1 更多工具弹窗（P1「⋮」与 S1「更多」出口）

- Modal 宽 440，标题「更多工具」`SIZE_H1` 左上。
- 六个工具行（行高 72，`R_ROW`，间距 16）：左 24px 图标 + 16px 文字 + 右 `chevron-right`。
  - `BG_TOOL_TILE` 底：文件传输 / 观看模式 / 终端 / 端口映射（行为同 P1 工具条，点击后关闭 O1）
  - 白底 `STROKE` 描边：重启 / 关机 → ConfirmDialog（标题「{重启|关机}设备」，正文「确认要{重启|关机}「yii's MacBook Pro」吗？」，确认键 ButtonDanger）→ 确认后 Toast「已发送{重启|关机}指令」（mock）并关闭
- 底部：左「调整顺序」ButtonSecondary（Toast 占位）、右「关闭」ButtonSecondary（关闭 Modal）。

### O2 右上下拉菜单（标题栏 `menu` 锚点）

Popover 宽 200，行高 40，`SIZE_BODY`，左 20px 图标：

```
🔒 安全锁定        → Toast「功能开发中」
⚙ 设置            → 打开 W1
？ 帮助与反馈       → Toast「功能开发中」
🎧 在线客服        → Toast「功能开发中」
ⓘ 关于            → Toast「功能开发中」
──────────────
⌃ 检查更新         → Toast「当前已是最新版本」
```

### O3 账户气泡（标题栏 `circle-user` 锚点）

Popover 宽 300 高 220，**顶部箭头朝上指向锚点**，`POPOVER_BG` 毛玻璃近似：

- 右上 `log-out` ghost（→ Toast「已退出登录（mock）」）。
- 中央：56px 圆头像占位（`ACCENT` 2px 圆环 + `circle-user` 40px `ACCENT` 色图标）；下方手机号 `SIZE_H1`（`151******21`）；蓝链接「我的卡券」14px（Toast 占位）。

### W1 设置窗口（O2「设置」出口）

独立窗口，交通灯，标题「设置」居中，不可调。内容整体宽 600 居中：

- **顶部 TabsTop**：常规（`settings`）/ 安全（`lock`）/ 键盘（`keyboard`）/ 网络（`globe`）；仅「常规」有内容，其余 tab 显示居中灰字空态「暂无可配置项」。tab 区高 88，底部 1px `DIVIDER` 通栏。
- **分组卡**（`BG_CARD` `R_CARD`，组间 16，内为 ToggleRow 列表，行间 1px `DIVIDER`）：

| 组 | 行 | 右侧控件 |
|---|---|---|
| G1 | 开机自启动 | Switch（默认开） |
| | 让这台 Mac 保持唤醒状态（说明：防止电脑进入睡眠状态，导致无法远程访问（合盖状态下建议连接电源适配器）。） | Select「永不」（选项：永不/10 分钟/30 分钟/1 小时） |
| | 关闭窗口时 | Select「隐藏到菜单栏」（选项：隐藏到菜单栏/退出程序） |
| G2 | 被控时拖拽文件显示发送浮窗（说明：开启后，可在被控时向主控端发送文件。） | Switch（默认开） |
| G3 | 移动设备传输文件存储路径（下方 mono 13px `TEXT_SECONDARY`：`/Users/yii/Library/Application Support/RabbitControl/Download`） | [修改路径][打开文件夹] 两枚 ButtonSecondary（Toast 占位） |
| G4 | 实名认证 | 灰字「已通过账号实名认证」（`TEXT_DISABLED`，禁用样式） |

### W2 文件传输（P1 工具条出口）

独立窗口，交通灯+标题「文件传输」。上中下三段：

**顶部对开区**（高 56）：左 = 「Y的 MacBook Pro」`SIZE_H1` + 「本机」Pill；中央两枚 ButtonSecondary「发送 →」「← 发送」（任一栏有勾选文件时激活，否则禁用）；右 = 「远端」Pill +「yii's MacBook Pro」。

**双栏文件浏览器**（等宽，间距 16，每栏结构相同）：

```
│ [←] [↑] [⟳] [📁+]  [ 📁 /Users/yii/Downloads          ⌄ ] │
│ 名称(flex)     修改日期(140⌄)  类型(90)     大小(90,右对齐) │
│ ☐ 📄 openssl-3.5.tar.gz  2026年11月21日  GZip 归档  9.44 MB │
```

- 工具行：4 枚 ghost（`arrow-left`/`arrow-up`/`rotate-cw`/`folder-plus`，均 Toast 占位）+ 路径 Select（`folder` 前缀 `ACCENT` 色图标，mono 13px）。
- 行：Checkbox（列宽 32）+ 类型图标（文件夹=蓝 `folder`，文件=`file`）+ 名称（超长中截断）；行高 40；偶数行 `BG_ZEBRA`，悬停 `BG_HOVER`；点击行=切换勾选。
- 数据：§6.0 左右栏各 8 行。
- 「发送 →」：将左栏勾选文件加入队列（每文件插入一条状态=已发送 的行，名称取自勾选行）+ Toast「已加入传输队列」；「← 发送」同理反向（mock 不区分方向数据）。

**底部传输队列卡**（高 240，白底 `R_CARD`）：

- 头行：「传输列表」`SIZE_H2` + 灰字「已传输 2 个文件」（=已发送条数，动态）；右侧按钮组四枚 ButtonSecondary：`play` 全部开始 / `pause` 全部暂停 / `x` 全部取消 / `eraser` 清除完结任务（mock：全部 Toast 占位，仅「清除完结任务」实际移除所有已发送/失败行）。
- 队列表六列：名称(flex，类型图标+全名)/状态(140)/大小(100，右对齐)/发送路径(flex，「本机」Pill+mono 截断)/接收路径(flex，「远端」Pill+mono 截断)/操作(48，`trash-2` ghost 移除该行)。
- 数据：§6.0 队列 4 条。

### W3 终端窗口（P1 工具条出口）

独立窗口「session1」，`TERMINAL_BG` 铺满。**静态壳**（真实 PTY 属 M2-6 后）：

- 左上 prompt：两枚 12px 灰描边小方块标签（文字 `[host]`、`[path]`）+ `~`（`ACCENT`）+ `>`（`ONLINE`）+ 10×18 `ACCENT` 光标块（800ms 周期闪烁）。
- 右上 `TERMINAL_MUTED` mono 14px 计时（每秒刷新，注入时钟，从 00:00:00 起）。

### W4 端口映射（P1 工具条出口）

独立窗口「端口映射」。标题区：「yii's MacBook Pro」`SIZE_H1` + `ONLINE` 点 + 「已连接」13px `TEXT_SECONDARY`。

内容卡（白底 `R_CARD`，占满余高，内边距 20）：

- 头行：「映射列表」`SIZE_H2`；右上 `circle-plus`+「添加」ButtonSecondary → 打开添加 Modal。
- 表六列：名称(flex，可排序)/本地端口(120)/目标地址(160)/目标端口(120)/映射状态(140)/操作(120：Switch + `ellipsis` ghost)。
- 行间仅 `DIVIDER`；数据：§6.0 映射 3 行。Switch 关=该行文字变 `TEXT_DISABLED`。
- **添加 Modal**（宽 400）：四行表单（名称 Input / 本地端口 Input / 目标地址 Input / 目标端口 Input）+ 底部右对齐 [取消 ButtonSecondary][保存 ButtonPrimary]；校验见 §7.1，通过后插入一条「连接中」新行。

### S1 会话窗口（P1「进入桌面」/「观看模式」出口）

**顶部会话工具栏**（48px，白底，替代标题栏）：

```
●●● │ 盾 | [▣ 显示屏1 (Built-in Retina Display)] [+] │ │⏱ 00:00:11│      [≡ 控制中心]
```

- 交通灯；`shield` ghost（→ S2）；屏幕 tab（选中白底胶囊卡+`SHADOW_CARD`，文字=屏幕名，超宽 320 尾部截断）；`+` ghost（禁用态）；
- 计时器 `FONT_MONO` HH:MM:SS（注入时钟）；
- 右侧「控制中心」ButtonGhost（`layout-dashboard`）→ S3。

**内容区**：`STAGE_BG` 铺满 + 居中 `TEXT_SECONDARY` 13px「画面接入将在 M2-6 桥接 MetalRenderer」。真实帧接入路径预留：`rc-render-macos::MetalRenderer::from_raw_layer` 与 GPUI 窗口原生层桥接（本期不实现）。

**观看模式变体**：同一 S1 的只读态——工具栏相同，画面区左上角 16px 边距处常驻「观看模式」Pill（灰）；本变体下无鼠标/键盘注入（本期本来也无注入，仅状态标记）。

**S2 安全状态气泡**（`shield` 锚点，顶部箭头朝上，宽 300）：

```
│ 安全状态                        [⚡ 需关注]                  │
│ ──────────────────────────────────────────────              │
│ ⛶ 防窥模式        隐私屏保 ⌄                                │
│    被控端屏幕已防护，遮挡屏幕内容（两行灰说明）               │
│ 🔇 被控静音  已外放                              [开关]      │
│ 🔒 自动锁屏  未开启                              [开关]      │
```

- 标题 `SIZE_H2`；「需关注」琥珀 Pill 的**判定规则**：（防窥模式 Select 值 =「关闭」）或（自动锁屏 Switch = 关）时显示，否则隐藏。mock 默认：防窥=隐私屏保、静音=关（说明文字「已外放」）、自动锁屏=关 → 显示「需关注」。
- 说明文字随状态切换：防窥=「被控端屏幕已防护，遮挡屏幕内容」/静音开=「已静音」关=「已外放」/自动锁屏开=「已开启」关=「未开启」。
- 「防窥模式」Select 选项：隐私屏保/黑屏/关闭。

**S3 控制中心菜单**（「控制中心」锚点，NativeMenu 规格）：

主菜单（宽 220）：

```
▣  显示屏1 (Built-in…)            ›
⇄  切换下一个屏幕            ⌥⌘N
────────────────────────────
🖼 画质                           ›   ← 悬停展开子菜单
▣ 窗口                            ›
🔊 声音                           ›
🛡 安全                           ›
🎛 外设                            ›
📦 工具                           ›
⊞ 更多                            ›
────────────────────────────
⛶ 进入全屏幕                    F
────────────────────────────
⎋ 退出远控（DANGER 红）
```

- 除「画质」外其余子菜单入口均 Toast「功能开发中」；「切换下一个屏幕」Toast 占位；「进入全屏幕」切换窗口全屏态（快捷键 `F` 同效）；**「退出远控」= 关闭会话窗口并回到主窗口 P1**（`DANGER` 色）。
- 「画质」子菜单（宽 300）：

```
自动
原画 30M
✓ 超清 14M
高清 8M
────────────
✓ 144 帧
90 帧
60 帧
30 帧
────────────
使用安全中转连接 [开关]
画面卡顿时，可提升流畅度，仅本次远控生效。
```

- 两个单选组（码率四选一、帧率四选一），mock 默认：超清 14M / 144 帧；切换仅改勾选态+Toast「画质将在下次连接生效」（mock）。
- 「使用安全中转连接」独立 Switch（默认关），说明文字 12px `TEXT_SECONDARY`。

**S4 HUD**（S1 右下角 24px 边距常驻，`Hud` 组件；左下偏移 8px 处一枚 `pencil` ghost 小按钮切换显隐）：

```
00:00:11          ← 白色（随计时同步），其余 TERMINAL_MUTED
UDP LAN
2 fps
227.2 Kbps
110 ms
20 ms frm.
0.0% loss
2K
```

- 数据本期全部为上述静态 mock；字段与 `LoopbackReport` 语义对齐（fps/码率/延迟/帧时间/丢包/分辨率），M2-6 接真数据。

---

## 7. 交互与状态规格

### 7.1 输入校验（唯一纯逻辑校验，W4 添加映射；可单测，放 `model.rs`）

- 本地端口/目标端口：1–65535 整数；非法（0、70000、非数字、空）→ 该 Input 错误态（`DANGER` 描边）+ 行下方 12px `DANGER`「端口需为 1–65535 的整数」。
- 目标地址：非空，否则「目标地址不能为空」；IPv4/IPv6/域名严格校验留 TODO。
- 名称：非空且 ≤64 字符，否则「名称需为 1–64 个字符」。
- 全部通过才允许「保存」。

### 7.2 加载/空态

| 场景 | 表现 |
|---|---|
| 侧栏设备列表加载中 | 行骨架条（`BG_HOVER` `R_ROW` 高 36）×3 |
| P3 无设备 | 居中 `monitor-smartphone` 40px `TEXT_DISABLED` + 「暂无设备」+ ButtonPrimary「开始协助」 |
| W2 列表为空 | 居中灰字「此文件夹为空」 |
| W2 队列为空 | 居中灰字「暂无传输任务」 |
| W4 无映射 | 居中灰字「暂无映射，点击右上角添加」 |

### 7.3 快捷键（本期注册表，行为可占位）

| 快捷键 | 作用 | 界面 |
|---|---|---|
| `⌥⌘N` | 切换下一个屏幕 | S1/S3 |
| `F` | 进入/退出全屏幕 | S1/S3 |
| `Esc` | 关闭当前浮层/模态/ConfirmDialog | 全局 |

### 7.4 浮层行为

- Popover 统一：点击锚点开合、点击外部关闭、`Esc` 关闭；同时只允许一个 popover。
- Modal：居中弹出 + `MASK` 遮罩；遮罩点击**不**关闭（防误触），仅按钮/`Esc` 关闭。
- ConfirmDialog 同 Modal 规则。
- Toast（§4）：任何 Toast 占位交互的统一出口，文案除特别标注外一律「功能开发中」。

---

## 8. GPUI / gpui-kit 实现映射建议

> 仅为映射建议；gpui-kit API 迭代快，实现时以其实际 examples 为准（遵 onboarding §5「照抄风格」）。

| 设计组件 | gpui-kit 对应（建议） |
|---|---|
| Button 系 | `Button`（`.primary()` / secondary / 自定义 danger 色） |
| Switch | `Switch` |
| Checkbox | `Checkbox`（若无则自绘 div） |
| Input / Select | `Input` / `SelectInput`（或 Input+Popover 组合） |
| DataTable | `Table`（虚拟滚动、列宽/排序） |
| TabsTop | `Tab`/`TabBar` 变体 + 自定义白卡胶囊样式 |
| ListRow | `List`/`ListItem` |
| Modal / ConfirmDialog / Popover | `Modal` / `Popover` / `PopupCard` |
| NativeMenu | `ContextMenu` 风格定制（或自绘 div 栈） |
| 图标 | `Icon`（`IconName`，Lucide） |
| Pill / StatusDot / Toast / ProgressBar / Hud / Badge | 无对应，自绘（div + 文本，theme token 直映） |

实现注意：

1. **毛玻璃近似**：GPUI 无 backdrop-filter。`POPOVER_BG` 用 `rgba(255,255,255,0.88)` 半透明 + `SHADOW_POPOVER`；若观感不足，备选=对锚点背景预烘焙高斯模糊贴底（本期不做）。
2. **theme.rs**：§3 全部 token 一比一落为常量；禁止在业务代码写裸色值。
3. **时钟**：HUD/终端计时器的时间由外部注入（onboarding R7），mock 数据走逻辑时钟。
4. **测试边界**：仅纯逻辑可测（§7.1 校验、路由状态机、token 不变量）；窗口渲染不进单测，走 §9 截图验收。
5. **窗口管理**：主窗口+四个独立工具窗口各自 `open_window`；macOS 交通灯走原生 titlebar；S1 用自绘工具栏替代。
6. **视频接入预留**：S1 画面区后续经 `rc-render-macos::MetalRenderer::from_raw_layer` 与 GPUI 窗口原生层桥接（M2-6，本期 mock）。

### 8.1 接线点清单（并行开工策略：mock 先行 → 后接线）

本文界面可与任务书其他工作**并行开工**（rc-ui 本期零 `rc-*` 依赖，单测不碰窗口）；接线时按下表逐项替换 mock，无需重新设计：

| 界面元素 | 本期 mock | 接线目标 | 前置条件 |
|---|---|---|---|
| 侧栏/P3 设备列表 | §6.0 mock 设备 | 设备注册与发现 | 会话层卡（待建） |
| P2 本设备 ID/验证码 | 静态值 | 鉴权/信令（临时码生成与刷新） | 鉴权卡（待建） |
| P1 进入桌面 / P2 连接 | 打开静态 S1 / Toast | 发起会话（打洞/中继；传输层 T09 已就绪） | 会话层卡 |
| **S1 画面** | `STAGE_BG` 占位 | `MetalRenderer::from_raw_layer` ↔ GPUI 窗口层桥接 | **桥接 spike（应最先做）** |
| S1 计时器 / S4 HUD | 注入时钟/静态值 | 会话开始时间与统计（字段已对齐 `LoopbackReport` 语义） | 会话层卡 |
| S3 画质/帧率/中转 | 勾选+Toast | 编码参数下发 | 会话层卡或 M3 |
| S2 安全状态 | mock | 被控端状态上报 | 会话层卡 |
| W2 文件传输 | mock 列表+假队列 | 文件通道（rc-net reliable stream） | 需建卡（目前没有） |
| W3 终端 | 静态壳 | PTY 会话 | 需建卡 |
| W4 端口映射 | mock | QUIC 端口转发 | 需建卡 |
| W1 设置 | 内存态 | 本地持久化 | 低优先 |

> 并行开工的唯一例外：**S1 视频桥接是全项目技术未知数**（GPUI 自带 Metal 渲染器与外部 CAMetalLayer 的合成方式未验证），UI 开工第一件事应做一个最小 spike（只验证层桥接、不做界面），确认可行后再按本文落地 S1，避免接线期返工布局。次要影响：gpui 冷编译较重，首次接入吃一次冷构建预算；此后 `just c rc-ui` 增量独立，不拖累其他 crate。

---

## 9. 视觉验收清单

实现后逐窗截图（`screencapture`）与本文规格比对：

- [ ] 初始状态：主窗口打开即 P1、选中 yii's MacBook Pro、无浮层
- [ ] P1：侧栏选中切换正确；工具条五组等分；预览卡比例与「进入桌面」悬停动效
- [ ] P2：大 ID 排版、验证码四件套、复制 Toast、禁用态连接按钮、开关关闭置灰
- [ ] P3：两组 4+4 行、mock 名称逐字一致、「本机」pill 仅一台、info/chevron 对齐
- [ ] O1：浅蓝四项+白底两项、重启/关机 ConfirmDialog、底部双按钮
- [ ] O2/O3：锚点定位正确、O3 箭头朝上、退出/卡券 Toast
- [ ] W1：tab 选中白卡、四分组卡、Select 选项、路径行 mono
- [ ] W2：双栏 8+8 行逐字一致、勾选激活发送按钮、发送入队 mock、队列 4 状态齐全（含 47% 进度条）、清除完结任务生效
- [ ] W3：prompt 元素、光标闪烁、计时刷新
- [ ] W4：三态映射行、Switch 关闭置灰、添加 Modal 校验（0/70000/空名称均红框）
- [ ] S1：工具栏五区、观看模式 Pill 变体、S2 气泡（含箭头与「需关注」判定：防窥=关闭 或 自动锁屏=关 时显示）、S3 菜单勾选态与快捷键对齐、退出远控回 P1、S4 HUD 七行
- [ ] 全局：`just f` 与 `just t rc-ui` 通过（实现阶段）；本文 token 与 theme.rs 无漂移

**元素级抽查法（验收人自测用）**：对任意元素追问四要素——位置/颜色/尺寸/内容——都应能在本文直接查出，无需猜测。样例（P2「连接」按钮）：位置=卡 2 表单行、Input 右侧间距 24、整行左对齐（§3.6 P2 布局）；颜色=`ACCENT`，禁用=`STROKE` 底 `TEXT_DISABLED`（§4 ButtonPrimary）；尺寸=高 36×宽 152、圆角 `R_ROW`（§4/§6 P2）；内容=「连接」（§6 P2）。验收时随机抽 5 个元素重复此抽查，查不到的视为文档缺陷并回填。

---

## 10. 未决事项

| # | 事项 | 影响 | 倾向 |
|---|---|---|---|
| 1 | 品牌主色是否沿用参考来源蓝 `#3370FF` | 全局 token | 暂沿用；品牌色确定后只改 `ACCENT` 一处 |
| 2 | 深色模式 | token 全集 | 来源全浅色，本期不做，token 已按语义命名预留 |
| 3 | Windows 端 chrome 差异（交通灯→Win 标题栏） | W1–W4/S1 框架 | P1 平台再定，本文只覆盖 macOS |
| 4 | P1 预览卡占位图（现为纯色+水印） | P1 | 后续定静态本地图片或渐变 |
| 5 | W2 双栏联动滚动/排序 | W2 | 先独立后联动（M2-6 前不必做） |
| 6 | 设备类型品牌徽标（Lucide 无 apple/Android） | 侧栏/P3 图标 | 本期形状图标；如需品牌观感再引入自定义 SVG 资产 |
| 7 | gpui-kit 具体组件 API 版本差 | 实现细节 | 实现首轮以 examples 校准后回填本文 §8 |

---

## 变更记录

| 日期 | 变更 |
|---|---|
| 2026-11-21 | 初版（12 屏 + tokens + 组件规格） |
| 2026-11-21 | 自查修订：mock 数据全量内联（§6.0，清除对参考图的依赖）；补 Checkbox/ProgressBar/Toast/ConfirmDialog/ButtonDanger/Badge 五组件规格；修正 Lucide 无品牌图标的设备映射；统一字号 token（SIZE_H1=20、新增 SIZE_TITLE）；补 App 初始状态、S2「需关注」判定规则、发送/退出远控等交互定义；§2 窗口表补 S4 与最小尺寸；修正 O3/S2 气泡箭头方向 |
| 2026-11-21 | 元素级审查修订：新增 §3.6 布局定位参数（标题栏/侧栏/各屏容器间距、图标默认尺寸与颜色、popover/modal/toast 定位规则），补齐「位置」要素；§9 增元素级抽查法（四要素：位置/颜色/尺寸/内容） |
