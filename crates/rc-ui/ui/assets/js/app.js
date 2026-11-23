/* app.js —— 主窗口 P1 / P2 / P3：侧栏路由 + 页面交互
 *
 * 唯一依据：docs/reference/design/08-ui-design.md
 *   §2 初始状态：默认选中 yii's MacBook Pro → P1、侧栏两组展开、无浮层
 *   §2 导航状态机：[我的设备] 设备行 → P1；全部设备行 → P3；
 *                  [远程协助] 开始协助 → P2；收藏设备 → P3（本期同 P3）
 *   §5.1 标题栏上下文 / 侧栏折叠
 *   §6 P1 / P2 / P3 逐屏交互
 *   §7.2 侧栏加载骨架、P3 空态
 *   §7.4 Toast 统一出口；Esc 关闭浮层（浮层本身由 overlay.js 管理）
 *
 * mock 数据统一从 window.RCMock（assets/js/mock.js，§6.0 单一数据源）取，
 * 本文件不内联任何展示数据。
 */
(function () {
  "use strict";

  var MOCK = window.RCMock;
  var Overlay = window.RCOverlay;
  var Bridge = window.RCBridge;
  var IS_TAURI = Boolean(Bridge && Bridge.isTauri);

  /* ── 小工具 ─────────────────────────────────────────────────── */
  function $(sel, root) {
    return (root || document).querySelector(sel);
  }

  function el(tag, cls, text) {
    var node = document.createElement(tag);
    if (cls) node.className = cls;
    if (text != null) node.textContent = text;
    return node;
  }

  /* sprite 引用（内联 Lucide，离线可用；§3.5） */
  function useIcon(name, size) {
    var svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
    svg.setAttribute("class", "ic ic--" + (size || 16));
    svg.setAttribute("aria-hidden", "true");
    svg.setAttribute("focusable", "false");
    var use = document.createElementNS("http://www.w3.org/2000/svg", "use");
    use.setAttribute("href", "#i-" + name);
    /* 兼容旧 WebKit：xlink:href */
    use.setAttributeNS("http://www.w3.org/1999/xlink", "xlink:href", "#i-" + name);
    svg.appendChild(use);
    return svg;
  }

  function toast(text) {
    if (Overlay && typeof Overlay.toast === "function") {
      Overlay.toast(text);
    }
  }

  function createPasswordField(opts) {
    var options = opts || {};
    var wrap = el("div", "rc-modal__input-wrap");
    var input = el("input", "rc-modal__input rc-modal__input--secret");
    input.type = "password";
    input.autocomplete = "new-password";
    input.spellcheck = false;
    input.autocapitalize = "off";
    if (options.value != null) input.value = options.value;
    if (options.placeholder) input.placeholder = options.placeholder;
    if (options.ariaLabel) input.setAttribute("aria-label", options.ariaLabel);

    var eyeBtn = el("button", "rc-modal__input-eye");
    eyeBtn.type = "button";
    var eyeSvg = useIcon("eye-off", 16);
    var useEl = eyeSvg.querySelector("use");
    eyeBtn.appendChild(eyeSvg);
    var visible = false;
    function syncEye() {
      input.type = visible ? "text" : "password";
      var href = visible ? "#i-eye" : "#i-eye-off";
      if (useEl) {
        useEl.setAttribute("href", href);
        useEl.setAttributeNS("http://www.w3.org/1999/xlink", "xlink:href", href);
      }
      var label = visible ? "隐藏验证码" : "显示验证码";
      eyeBtn.setAttribute("aria-pressed", visible ? "true" : "false");
      eyeBtn.setAttribute("aria-label", label);
      eyeBtn.title = label;
    }
    syncEye();
    eyeBtn.addEventListener("click", function (e) {
      e.preventDefault();
      visible = !visible;
      syncEye();
      input.focus();
    });

    wrap.appendChild(input);
    wrap.appendChild(eyeBtn);
    return { wrap: wrap, input: input };
  }

  /* 打开工具窗口：Tauri 内走 Rust 侧 WebviewWindowBuilder（§8.3，原生窗口、可聚焦复用），
   * 浏览器预览时回落到 window.open（便于逐屏比对设计稿）。 */
  function openWindow(kind) {
    if (IS_TAURI && Bridge && typeof Bridge.openToolWindow === "function") {
      Bridge.openToolWindow(kind).catch(function (err) {
        toast("打开窗口失败：" + err);
      });
      return;
    }
    var internals = window.__TAURI_INTERNALS__;
    if (internals && typeof internals.invoke === "function") {
      internals.invoke("open_tool_window", { kind: kind }).catch(function (err) {
        toast("打开窗口失败：" + err);
      });
      return;
    }
    var spec = WIN[kind];
    if (spec) {
      window.open(spec[0], spec[1], spec[2]);
    }
  }

  var WIN = {
    transfer: ["transfer.html", "rc-transfer", "width=1400,height=860"],
    terminal: ["terminal.html", "rc-terminal", "width=1200,height=760"],
    portmap: ["portmap.html", "rc-portmap", "width=1280,height=780"],
    settings: ["settings.html", "rc-settings", "width=660,height=640"]
  };

  /* ── 路由状态 ───────────────────────────────────────────────── */
  var state = {
    /* route: 'p1' | 'p2' | 'p3' */
    route: "p1",
    /* 当前选中设备 id（P1 使用；P3 行点击也会更新它） */
    deviceId: MOCK.defaultDeviceId,
    /* 侧栏选中项 key：device:<id> / all / assist / favorites */
    sidebarKey: "device:" + MOCK.defaultDeviceId,
    /* P2 状态（桌面端默认关闭远程协助，浏览器静态预览保持开启） */
    allowAssist: !IS_TAURI,
    codeVisible: true,
    verifyMode: 0,
    /* Tauri 一体化真实状态（非 Tauri 下保持空，不影响 mock） */
    hostStatus: null,
    serverStatus: null,
    lastDeviceCode: null,
    myDevices: [],
    gateStatus: null,
    permBannerDismissed: false,
    lanPeers: [],
    deviceSnapshots: {},
    activeSession: {
      active: false,
      target: "",
      deviceName: "",
      watchOnly: false,
      connected: false,
      transport: null
    },
    /* P3 折叠状态 */
    p3Collapsed: { lan: false, computers: false, mobiles: false }
  };

  function iconForOs(os) {
    var lower = String(os || "").toLowerCase();
    if (lower.indexOf("win") !== -1) return "app-window";
    if (lower.indexOf("ipad") !== -1 || lower.indexOf("tablet") !== -1) return "tablet";
    if (lower.indexOf("ios") !== -1 || lower.indexOf("android") !== -1 || lower.indexOf("phone") !== -1) {
      return "smartphone";
    }
    return "laptop";
  }

  function groupForOs(os) {
    var ic = iconForOs(os);
    return ic === "smartphone" || ic === "tablet" ? "mobiles" : "computers";
  }

  function normalizeMyDevice(raw) {
    var did = String((raw && (raw.deviceId || raw.device_id)) || "");
    var formattedId = String((raw && (raw.formattedId || raw.formatted_id)) || formatDeviceId(did));
    var alias = String((raw && raw.alias) || "");
    var hostname = String((raw && raw.hostname) || "");
    var displayName = String(
      (raw && (raw.displayName || raw.display_name)) ||
        alias.trim() ||
        hostname.trim() ||
        formattedId ||
        did
    );
    var os = String((raw && raw.os) || "");
    return {
      id: did,
      deviceId: did,
      formattedId: formattedId,
      hostname: hostname,
      alias: alias,
      name: displayName,
      os: os,
      icon: iconForOs(os),
      group: groupForOs(os),
      online: Boolean(raw && raw.online),
      inSession: Boolean(raw && (raw.inSession || raw.in_session)),
      isLocal: Boolean(raw && (raw.isLocal || raw.is_local)),
      allowRemoteAssist: Boolean(raw && (raw.allowRemoteAssist || raw.allow_remote_assist)),
      isMine: true
    };
  }

  function findDevice(id) {
    if (IS_TAURI) {
      for (var m = 0; m < state.myDevices.length; m++) {
        if (state.myDevices[m].id === id) return state.myDevices[m];
      }
      return state.myDevices.length > 0 ? state.myDevices[0] : null;
    }
    for (var i = 0; i < MOCK.devices.length; i++) {
      if (MOCK.devices[i].id === id) return MOCK.devices[i];
    }
    /* P3 独有设备（本机 / 手机）也需支持路由回 P1 */
    var groups = [MOCK.p3.computers, MOCK.p3.mobiles];
    for (var g = 0; g < groups.length; g++) {
      for (var j = 0; j < groups[g].length; j++) {
        if (groups[g][j].id === id) return groups[g][j];
      }
    }
    return MOCK.devices[1];
  }

  /* ── 标题栏上下文（§5.1）─────────────────────────────────────── */
  var titlebarContext = $("#titlebarContext");

  function renderTitlebar() {
    titlebarContext.textContent = "";

    if (state.route === "p1") {
      var device = findDevice(state.deviceId);
      if (!device) {
        var fallbackTitle = el("h1", "titlebar__page", "我的设备");
        fallbackTitle.setAttribute("data-tauri-drag-region", "");
        titlebarContext.appendChild(fallbackTitle);
        return;
      }
      /* 在线 Pill（PILL_ONLINE_BG 底白字 + 前置在线绿点/离线灰点） */
      var pill = el("span", "pill pill--online");
      pill.setAttribute("data-tauri-drag-region", "");
      var dot = el("span", "dot " + (device.online ? "dot--online" : "dot--offline"));
      dot.setAttribute("data-tauri-drag-region", "");
      pill.appendChild(dot);
      pill.appendChild(document.createTextNode(device.online ? "在线" : "离线"));
      titlebarContext.appendChild(pill);

      var name = el("span", "titlebar__device", device.name);
      name.title = device.name;
      name.setAttribute("data-tauri-drag-region", "");
      titlebarContext.appendChild(name);

      var more = el("button", "rc-btn rc-btn--ghost titlebar__more");
      more.type = "button";
      more.setAttribute("aria-label", "更多");
      more.appendChild(useIcon("ellipsis", 16));
      more.addEventListener("click", function () {
        if (Overlay) Overlay.openO1();
      });
      titlebarContext.appendChild(more);
      return;
    }

    var title = el("h1", "titlebar__page", state.route === "p2" ? "远程协助" : "全部设备");
    title.setAttribute("data-tauri-drag-region", "");
    titlebarContext.appendChild(title);
  }

  /* ── 侧栏（§5.1 / §3.6）────────────────────────────────────── */
  var sidebarDevices = $("#sidebarDevices");
  var sidebarAssist = $("#sidebarAssist");

  function buildSidebarRow(opts) {
    var row = el("button", "sidebar__row");
    row.type = "button";
    row.dataset.key = opts.key;

    var dot = el("span", "dot " + (opts.dot ? "dot--" + opts.dot : "dot--none"));
    row.appendChild(dot);

    var icon = el("span", "sidebar__icon");
    icon.appendChild(useIcon(opts.icon, 14));
    row.appendChild(icon);

    var name = el("span", "sidebar__name", opts.name);
    name.title = opts.name;
    row.appendChild(name);

    row.addEventListener("click", opts.onClick);
    return row;
  }

  function renderSidebar() {
    /* §7.2 首帧骨架：BG_HOVER R_ROW 高 36 ×3（真实数据同步可用时立即替换） */
    sidebarDevices.textContent = "";
    for (var s = 0; s < 3; s++) {
      sidebarDevices.appendChild(el("div", "sidebar__skeleton"));
    }
    sidebarAssist.textContent = "";

    /* 数据同步可得（本期 mock），立即落真实行；
     * `?loading=sidebar` 保留骨架 1.2s，便于单独验收 §7.2 加载态。 */
    var holdMs = /(?:^|[?&])loading=sidebar(?:&|$)/.test(window.location.search) ? 1200 : 0;
    if (holdMs) {
      window.setTimeout(paintSidebarRows, holdMs);
    } else {
      paintSidebarRows();
    }
  }

  function paintSidebarRows() {
    sidebarDevices.textContent = "";
    sidebarAssist.textContent = "";

    var appendControlledBadge = function (row, d) {
      if (!isDeviceControlled(d)) return;
      var ctrlBadge = el("span", "sidebar__ctrl-badge");
      ctrlBadge.setAttribute("aria-label", "正在远程控制");
      ctrlBadge.title = "正在远程控制";
      ctrlBadge.appendChild(useIcon("cast", 12));
      ctrlBadge.appendChild(el("span", null, "1"));
      row.appendChild(ctrlBadge);
    };

    if (IS_TAURI) {
      state.myDevices.forEach(function (d) {
        var row = buildSidebarRow({
          key: "device:" + d.id,
          dot: d.online ? "online" : "offline",
          icon: d.icon,
          name: d.name,
          onClick: function () {
            selectDevice(d.id, "p1");
          }
        });
        var nameNode = row.querySelector(".sidebar__name");
        if (nameNode && d.formattedId) {
          nameNode.title = d.name + " (" + d.formattedId + ")" + (d.isLocal ? " · 本机" : "");
        }
        row.addEventListener("contextmenu", function (e) {
          e.preventDefault();
          openDeviceContextMenu(e.clientX, e.clientY, d);
        });
        appendControlledBadge(row, d);
        sidebarDevices.appendChild(row);
      });
    } else {
      MOCK.devices.forEach(function (d) {
        var row = buildSidebarRow({
          key: "device:" + d.id,
          dot: d.online ? "online" : "offline",
          icon: d.icon,
          name: d.name,
          onClick: function () {
            selectDevice(d.id, "p1");
          }
        });
        appendControlledBadge(row, d);
        sidebarDevices.appendChild(row);
      });
    }

    sidebarDevices.appendChild(
      buildSidebarRow({
        key: "all",
        dot: null,
        icon: "layout-grid",
        name: "全部设备",
        onClick: function () {
          go("p3", "all");
        }
      })
    );

    sidebarAssist.appendChild(
      buildSidebarRow({
        key: "assist",
        dot: null,
        icon: "handshake",
        name: "开始协助",
        onClick: function () {
          go("p2", "assist");
        }
      })
    );

    sidebarAssist.appendChild(
      buildSidebarRow({
        key: "favorites",
        dot: null,
        icon: "bookmark",
        name: "收藏设备",
        onClick: function () {
          go("p3", "favorites");
        }
      })
    );

    syncSidebarSelection();
  }

  function syncSidebarSelection() {
    var rows = document.querySelectorAll(".sidebar__row");
    Array.prototype.forEach.call(rows, function (row) {
      var on = row.dataset.key === state.sidebarKey;
      row.classList.toggle("is-selected", on);
      if (on) {
        row.setAttribute("aria-current", "page");
      } else {
        row.removeAttribute("aria-current");
      }
    });
  }

  /* 侧栏分组折叠 */
  function wireSidebarGroups() {
    var heads = document.querySelectorAll(".sidebar__head");
    Array.prototype.forEach.call(heads, function (head) {
      head.addEventListener("click", function () {
        var group = head.parentNode;
        var collapsed = group.classList.toggle("is-collapsed");
        head.setAttribute("aria-expanded", collapsed ? "false" : "true");
      });
    });
  }

  /* ── 路由 ───────────────────────────────────────────────────── */
  var pages = {
    p1: $("#page-p1"),
    p2: $("#page-p2"),
    p3: $("#page-p3")
  };

  function go(route, sidebarKey) {
    state.route = route;
    if (sidebarKey) state.sidebarKey = sidebarKey;

    Object.keys(pages).forEach(function (key) {
      pages[key].classList.toggle("is-active", key === route);
    });

    if (route === "p1") renderP1();
    if (route === "p3") renderP3();
    renderTitlebar();
    syncSidebarSelection();

    $("#content").scrollTop = 0;
  }

  /* 侧栏是否收录该设备（P3 独有的本机/手机不在侧栏，选中态保持原样） */
  function inSidebar(id) {
    if (IS_TAURI) {
      for (var m = 0; m < state.myDevices.length; m++) {
        if (state.myDevices[m].id === id) return true;
      }
      return false;
    }
    for (var i = 0; i < MOCK.devices.length; i++) {
      if (MOCK.devices[i].id === id) return true;
    }
    return false;
  }

  function selectDevice(id, route) {
    state.deviceId = id;
    /* 侧栏无该设备行时保留原选中项（避免主窗出现「无任何高亮」的破态） */
    var key = inSidebar(id) ? "device:" + id : state.sidebarKey;
    go(route || "p1", key);
  }

  /* ── P1 设备主页（§6 P1）───────────────────────────────────── */
  var p1Card = $("#p1Card");
  var p1Watermark = $("#p1Watermark");
  var p1Preview = $("#p1Preview");
  var p1PreviewName = $("#p1PreviewName");
  var p1PreviewHint = $("#p1PreviewHint");
  var p1Wallpaper = $("#p1Wallpaper");
  var p1SessionBanner = $("#p1SessionBanner");
  var p1BannerDisconnect = $("#p1BannerDisconnect");
  var p1ActiveOverlay = $("#p1ActiveOverlay");
  var p1FocusSession = $("#p1FocusSession");

  function stripDeviceIdSep(val) {
    return String(val || "").replace(/[\s-]+/g, "");
  }

  function applyDeviceSnapshots(raw) {
    var map = {};
    var addEntry = function (entry, fallbackKey) {
      if (!entry || typeof entry !== "object") return;
      var keys = [];
      if (entry.key) keys.push(String(entry.key));
      if (fallbackKey) keys.push(String(fallbackKey));
      if (entry.addr) keys.push(String(entry.addr));
      if (entry.deviceId) keys.push(String(entry.deviceId));
      if (entry.name) keys.push(String(entry.name));
      keys.forEach(function (k) {
        var norm = String(k).trim();
        if (norm) map[norm] = entry;
      });
    };
    if (Array.isArray(raw)) {
      raw.forEach(function (item) {
        addEntry(item, null);
      });
    } else if (raw && typeof raw === "object") {
      Object.keys(raw).forEach(function (k) {
        addEntry(raw[k], k);
      });
    }
    state.deviceSnapshots = map;
    if (state.route === "p1") renderP1();
  }

  function findDeviceSnapshot(device) {
    if (!device || !state.deviceSnapshots) return null;
    var candidates = [];
    if (device.id) {
      candidates.push(String(device.id));
      if (String(device.id).indexOf("lan:") === 0) {
        candidates.push(String(device.id).slice(4));
      }
    }
    if (device.addr) candidates.push(String(device.addr));
    if (device.deviceId) candidates.push(String(device.deviceId));
    if (device.name) candidates.push(String(device.name));
    for (var i = 0; i < candidates.length; i++) {
      var k = candidates[i].trim();
      if (k && state.deviceSnapshots[k]) return state.deviceSnapshots[k];
    }
    return null;
  }

  function isDeviceControlled(device) {
    if (!device || !state.activeSession || !state.activeSession.active) return false;
    var target = state.activeSession.target;
    var deviceName = state.activeSession.deviceName;
    if (target && device.addr && device.addr === target) return true;
    if (target && device.deviceId) {
      if (device.deviceId === target) return true;
      var strippedA = stripDeviceIdSep(device.deviceId);
      var strippedB = stripDeviceIdSep(target);
      if (strippedA && strippedA === strippedB) return true;
    }
    if (deviceName && device.name && device.name === deviceName) return true;
    return false;
  }

  var pendingToolAfterConnect = null;

  function matchesPendingDevice(pending, info) {
    if (!pending || !pending.deviceKey) return true;
    var keyA = stripDeviceIdSep(pending.deviceKey);
    var tB = stripDeviceIdSep(info.target || "");
    var kB = stripDeviceIdSep(info.deviceKey || "");
    if (pending.deviceKey === info.target || pending.deviceKey === info.deviceKey) return true;
    if (keyA && (keyA === tB || keyA === kB)) return true;
    return false;
  }

  function applyActiveSessionState(raw) {
    var s = raw && typeof raw === "object" ? raw : {};
    state.activeSession = {
      active: Boolean(s.active),
      target: s.target ? String(s.target) : "",
      deviceKey: s.deviceKey
        ? String(s.deviceKey)
        : s.device_key
          ? String(s.device_key)
          : "",
      deviceName: s.deviceName
        ? String(s.deviceName)
        : s.device_name
          ? String(s.device_name)
          : "",
      watchOnly: Boolean(s.watchOnly !== undefined ? s.watchOnly : s.watch_only),
      connected: Boolean(s.connected),
      transport: s.transport ? String(s.transport) : null
    };
    if (pendingToolAfterConnect) {
      if (Date.now() > pendingToolAfterConnect.expiresAt) {
        pendingToolAfterConnect = null;
      } else if (state.activeSession.active) {
        if (matchesPendingDevice(pendingToolAfterConnect, state.activeSession)) {
          pendingToolAfterConnect.sawTargetActive = true;
          if (state.activeSession.connected) {
            var readyTool = pendingToolAfterConnect;
            pendingToolAfterConnect = null;
            if (!state.activeSession.watchOnly) {
              openWindow(readyTool.tool);
            }
          }
        }
      } else if (pendingToolAfterConnect.sawTargetActive) {
        pendingToolAfterConnect = null;
      }
    }
    paintSidebarRows();
    if (state.route === "p1") renderP1();
    renderP2SessionBar();
  }

  function focusSessionWindow() {
    if (IS_TAURI && typeof Bridge.focusSessionWindow === "function") {
      Bridge.focusSessionWindow().catch(function (err) {
        toast("显示远控窗口失败：" + err);
      });
      return;
    }
    if (IS_TAURI && typeof Bridge.openSessionWindow === "function") {
      Bridge.openSessionWindow(Boolean(state.activeSession && state.activeSession.watchOnly)).catch(function (err) {
        toast("显示远控窗口失败：" + err);
      });
      return;
    }
    toast("已切换至远控窗口");
  }

  function disconnectActiveSession() {
    if (IS_TAURI && typeof Bridge.sessionCommand === "function") {
      Bridge.sessionCommand({ type: "disconnect" })
        .then(function () {
          toast("已断开远程控制");
        })
        .catch(function (err) {
          toast("断开失败：" + err);
        });
      return;
    }
    applyActiveSessionState({ active: false });
    toast("已断开远程控制");
  }

  function renderP1() {
    var device = findDevice(state.deviceId);
    if (!device) {
      p1Watermark.textContent = "RabbitControl";
      p1Card.setAttribute("aria-label", "暂无选中设备");
      p1Card.classList.remove("is-controlling");
      if (p1SessionBanner) p1SessionBanner.hidden = true;
      if (p1ActiveOverlay) p1ActiveOverlay.hidden = true;
      if (p1PreviewName) p1PreviewName.textContent = "暂无绑定设备";
      if (p1PreviewHint) p1PreviewHint.textContent = "请在「远程协助」输入设备码，或在其他设备登录同一账号";
      if (p1Wallpaper) {
        p1Wallpaper.hidden = true;
        p1Wallpaper.removeAttribute("src");
      }
      if (p1Preview) {
        p1Preview.classList.remove("has-wallpaper");
        p1Preview.classList.add("p1__preview--empty");
        p1Preview.classList.add("p1__preview--offline");
      }
      return;
    }
    p1Watermark.textContent = device.name;

    var snap = findDeviceSnapshot(device);
    var wallpaperUrl =
      snap && snap.wallpaperDataUrl
        ? String(snap.wallpaperDataUrl)
        : device.snapshotUrl && String(device.snapshotUrl).length
          ? String(device.snapshotUrl)
          : "";
    var hasWallpaper = Boolean(wallpaperUrl);
    var offline = !device.online;

    if (p1PreviewName) p1PreviewName.textContent = device.name;
    if (p1Preview) {
      var iconUse = p1Preview.querySelector(".p1__preview-icon use");
      if (iconUse && device.icon) {
        var iconHref = "#i-" + device.icon;
        iconUse.setAttribute("href", iconHref);
        iconUse.setAttributeNS("http://www.w3.org/1999/xlink", "xlink:href", iconHref);
      }
      if (p1Wallpaper) {
        if (hasWallpaper) {
          p1Wallpaper.src = wallpaperUrl;
          p1Wallpaper.hidden = false;
        } else {
          p1Wallpaper.hidden = true;
          p1Wallpaper.removeAttribute("src");
        }
      }
      p1Preview.classList.toggle("has-wallpaper", hasWallpaper);
      p1Preview.classList.toggle("p1__preview--empty", !hasWallpaper);
      p1Preview.classList.toggle("p1__preview--offline", offline);
      if (!hasWallpaper && p1PreviewHint) {
        p1PreviewHint.textContent =
          snap && Number(snap.lastConnectedTs) > 0 ? "最近已连接" : "尚未连接过该设备";
      }
    }

    var controlling = isDeviceControlled(device);
    p1Card.classList.toggle("is-controlling", controlling);
    p1Card.setAttribute(
      "aria-label",
      (controlling ? "显示远控窗口：" : "进入桌面：") + device.name
    );
    if (p1SessionBanner) p1SessionBanner.hidden = !controlling;
    if (p1ActiveOverlay) p1ActiveOverlay.hidden = !controlling;
  }

  function openSession(watchOnly, targetDevice) {
    var device = targetDevice || findDevice(state.deviceId);
    if (device && isDeviceControlled(device)) {
      focusSessionWindow();
      return;
    }
    if (IS_TAURI) {
      if (device && device.isMine) {
        if (device.isLocal) {
          toast("当前为本机设备，请选择其他在线设备连接");
          return;
        }
        if (!device.online) {
          toast("设备「" + device.name + "」当前离线");
          return;
        }
        connectToTarget(device.deviceId, {
          watchOnly: Boolean(watchOnly),
          promptPassword: true,
          placeholder: "请输入设备验证码（已开启免密自控可直接留空）",
          displayName: device.name
        });
        return;
      }
      if (device && device.isLan && device.addr) {
        connectToTarget(device.addr, {
          watchOnly: Boolean(watchOnly),
          promptPassword: Boolean(device.requiresPassword),
          displayName: device.name
        });
        return;
      }
      toast("请先选择在线设备或在「远程协助」输入 9 位设备码连接");
      go("p2", "assist");
      return;
    }
    var params = [];
    params.push("device=" + encodeURIComponent((device && device.name) || ""));
    if (watchOnly) params.push("watch=1");
    window.open("session.html?" + params.join("&"), "rc-session", "width=1280,height=800");
  }

  function openTool(tool, targetDevice) {
    if (tool === "watch") return openSession(true, targetDevice);
    if (tool === "transfer" || tool === "terminal" || tool === "portmap") {
      if (!IS_TAURI) return openWindow(tool);
      var device = targetDevice || findDevice(state.deviceId);
      var active = state.activeSession;
      if (active && active.active && active.connected) {
        if (!device || isDeviceControlled(device)) {
          if (active.watchOnly) {
            toast("观看模式为只读，请切换完整控制模式后再使用工具");
            return;
          }
          return openWindow(tool);
        }
      }
      if (!device) {
        toast("请先选择在线设备或在「远程协助」连接后再打开工具");
        go("p2", "assist");
        return;
      }
      if (device.isLocal) {
        toast("当前为本机设备，请选择其他在线设备");
        return;
      }
      if (!device.online) {
        toast("设备「" + device.name + "」当前离线");
        return;
      }
      var devKey = device.deviceId || device.addr || device.id || "";
      pendingToolAfterConnect = {
        tool: tool,
        deviceKey: devKey,
        sawTargetActive: false,
        expiresAt: Date.now() + 20000
      };
      toast("正在连接「" + device.name + "」，建连后自动打开工具…");
      openSession(false, device);
      return;
    }
    toast("功能开发中");
  }

  function wireP1() {
    /* 预览卡整体可点击 → S1 会话（若当前正在远控此设备则直接聚焦远控窗） */
    p1Card.addEventListener("click", function () {
      openSession(false);
    });
    p1Card.addEventListener("keydown", function (e) {
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        openSession(false);
      }
    });

    if (p1BannerDisconnect) {
      p1BannerDisconnect.addEventListener("click", function (e) {
        e.stopPropagation();
        disconnectActiveSession();
      });
    }

    if (p1FocusSession) {
      p1FocusSession.addEventListener("click", function (e) {
        e.stopPropagation();
        focusSessionWindow();
      });
    }

    /* 工具条：文件传输 → W2 / 观看模式 → S1 只读 / 终端 → W3 / 端口映射 → W4 */
    var tools = document.querySelectorAll(".p1__tool[data-tool]");
    Array.prototype.forEach.call(tools, function (btn) {
      btn.addEventListener("click", function () {
        openTool(btn.dataset.tool);
      });
    });
  }

  /* ── P2 远程协助（§6 P2）───────────────────────────────────── */
  var p2Allow = $("#p2Allow");
  var p2Body = $("#p2Body");
  var p2DeviceId = $("#p2DeviceId");
  var p2RefreshId = $("#p2RefreshId");
  var p2DeviceSub = $("#p2DeviceSub");
  var p2SessionBar = $("#p2SessionBar");
  var p2SessionText = $("#p2SessionText");
  var p2FocusSession = $("#p2FocusSession");
  var p2DisconnectSession = $("#p2DisconnectSession");
  var p2OpenTransfer = $("#p2OpenTransfer");
  var p2OpenTerminal = $("#p2OpenTerminal");
  var p2OpenPortmap = $("#p2OpenPortmap");
  var p2VerifyMode = $("#p2VerifyMode");
  var p2Code = $("#p2Code");
  var p2Eye = $("#p2Eye");
  var p2Copy = $("#p2Copy");
  var p2Refresh = $(".p2__refresh");
  var p2PeerInput = $("#p2PeerInput");
  var p2Connect = $("#p2Connect");
  var p2HistoryBtn = $("#p2HistoryBtn");
  var p2InputWrap = p2PeerInput.parentNode;
  var p2History = null;
  var refreshingDeviceId = false;

  /* ── 应用内轻量弹窗（Tauri 一体化：密码输入 / 验证码设置 / 连接审批）── */
  var activeModal = null;
  var activeApprovalGen = null;

  function closeModal() {
    if (!activeModal) return;
    if (activeModal.onKeyDown) {
      document.removeEventListener("keydown", activeModal.onKeyDown, true);
    }
    if (activeModal.backdrop && activeModal.backdrop.parentNode) {
      activeModal.backdrop.parentNode.removeChild(activeModal.backdrop);
    }
    activeModal = null;
    activeApprovalGen = null;
  }

  function showModal(opts) {
    closeModal();
    if (Overlay && typeof Overlay.closeAll === "function") Overlay.closeAll();
    closeHistory();

    var backdrop = el("div", "rc-modal-backdrop");
    var dialog = el("div", opts.modalClass ? "rc-modal " + opts.modalClass : "rc-modal");
    dialog.setAttribute("role", "dialog");
    dialog.setAttribute("aria-modal", "true");
    if (opts.title) {
      dialog.appendChild(el("h3", "rc-modal__title", opts.title));
    }
    if (opts.desc) {
      dialog.appendChild(el("p", "rc-modal__desc", opts.desc));
    }
    if (opts.content) {
      dialog.appendChild(opts.content);
    }
    if (opts.actions) {
      dialog.appendChild(opts.actions);
    }
    backdrop.appendChild(dialog);

    var dismissible = opts.dismissible !== false;
    if (dismissible) {
      backdrop.addEventListener("click", function (e) {
        if (e.target === backdrop) {
          closeModal();
          if (typeof opts.onCancel === "function") opts.onCancel();
        }
      });
    }

    var onKeyDown = function (e) {
      if (e.key === "Escape" && dismissible) {
        e.preventDefault();
        e.stopPropagation();
        closeModal();
        if (typeof opts.onCancel === "function") opts.onCancel();
      }
    };
    document.addEventListener("keydown", onKeyDown, true);

    document.body.appendChild(backdrop);
    activeModal = { backdrop: backdrop, onKeyDown: onKeyDown };
    activeApprovalGen = opts.approvalGeneration != null ? opts.approvalGeneration : null;

    if (opts.focusEl && typeof opts.focusEl.focus === "function") {
      opts.focusEl.focus();
    }
  }

  function parseDeviceIdInput(raw) {
    var digits = String(raw || "").trim().replace(/[\s-]+/g, "");
    if (!/^[1-9]\d{8}$/.test(digits)) {
      return null;
    }
    return digits;
  }

  function formatDeviceId(raw) {
    var digits = String(raw || "").replace(/[\s-]+/g, "");
    if (/^\d{9}$/.test(digits)) {
      return digits.slice(0, 3) + " " + digits.slice(3, 6) + " " + digits.slice(6, 9);
    }
    return String(raw || "");
  }

  function getConnectedDeviceCode() {
    var srv = state.serverStatus;
    if (srv && srv.state === "connected") {
      if (srv.deviceIdFormatted) return String(srv.deviceIdFormatted);
      if (srv.deviceId) return formatDeviceId(srv.deviceId);
    }
    return null;
  }

  function getCachedDeviceCode() {
    var srv = state.serverStatus;
    if (srv) {
      if (srv.deviceIdFormatted) return String(srv.deviceIdFormatted);
      if (srv.deviceId) return formatDeviceId(srv.deviceId);
    }
    return state.lastDeviceCode || null;
  }

  function renderP2DeviceId() {
    if (!p2DeviceId || !IS_TAURI) return;
    var srv = state.serverStatus;
    var srvState = srv && srv.state ? String(srv.state) : "disabled";
    var connectedCode = getConnectedDeviceCode();
    var cachedCode = getCachedDeviceCode();

    if (srvState === "connected" && connectedCode) {
      p2DeviceId.textContent = connectedCode;
      p2DeviceId.classList.remove("p2__bigid--muted", "p2__bigid--addr");
      p2DeviceId.title = connectedCode;
    } else if (srvState === "connecting") {
      if (cachedCode) {
        p2DeviceId.textContent = cachedCode;
        p2DeviceId.classList.remove("p2__bigid--muted", "p2__bigid--addr");
        p2DeviceId.title = cachedCode + "（连接服务器中…）";
      } else {
        p2DeviceId.textContent = "连接服务器中…";
        p2DeviceId.classList.add("p2__bigid--muted");
        p2DeviceId.classList.remove("p2__bigid--addr");
        p2DeviceId.title = "正在连接服务器";
      }
    } else {
      p2DeviceId.textContent = "未连接服务器";
      p2DeviceId.classList.add("p2__bigid--muted");
      p2DeviceId.classList.remove("p2__bigid--addr");
      p2DeviceId.title =
        srv && srv.errorMessage ? "服务器连接失败：" + srv.errorMessage : "尚未连接服务器";
    }

    if (!p2DeviceSub) return;
    p2DeviceSub.textContent = "";
    var hasSub = false;

    if (srvState !== "connected" && srvState !== "connecting") {
      var cfgRow = el("div", "p2__id-sub-row");
      cfgRow.appendChild(el("span", null, "配置服务器以获取设备 ID"));
      var cfgBtn = el("button", "p2__id-sub-link", "去配置");
      cfgBtn.type = "button";
      cfgBtn.addEventListener("click", function () {
        openWindow("settings");
      });
      cfgRow.appendChild(cfgBtn);
      p2DeviceSub.appendChild(cfgRow);
      hasSub = true;
    } else if (srvState === "connecting" && cachedCode) {
      p2DeviceSub.appendChild(el("div", "p2__id-sub-row", "正在连接服务器…"));
      hasSub = true;
    }

    var lanAddr =
      state.hostStatus && state.hostStatus.enabled && state.hostStatus.displayAddr
        ? String(state.hostStatus.displayAddr)
        : "";
    if (lanAddr) {
      var lanRow = el("div", "p2__id-sub-row");
      lanRow.appendChild(el("span", "p2__id-sub-lan", "局域网：" + lanAddr));
      p2DeviceSub.appendChild(lanRow);
      hasSub = true;
    }

    p2DeviceSub.hidden = !hasSub;
  }

  var lastServerRev = -1;
  var sessionLaunching = false;

  function applyServerStatus(snap) {
    if (!snap) return;
    if (typeof snap.revision === "number" && snap.revision > 0 && snap.revision < lastServerRev) {
      return;
    }
    if (typeof snap.revision === "number" && snap.revision > lastServerRev) {
      lastServerRev = snap.revision;
    }
    var code = snap.deviceIdFormatted
      ? String(snap.deviceIdFormatted)
      : snap.deviceId
        ? formatDeviceId(snap.deviceId)
        : null;
    if (code) state.lastDeviceCode = code;
    state.serverStatus = snap;
    renderP2DeviceId();
  }

  function launchSessionWithTarget(addr, watchOnly, password, deviceName) {
    var cfg = {
      target: addr,
      watchOnly: Boolean(watchOnly),
      password: password && password.length > 0 ? password : null,
      deviceName: deviceName || null
    };
    var isDeviceCode = Boolean(parseDeviceIdInput(addr));
    sessionLaunching = true;
    updateConnectState();
    if (isDeviceCode) {
      toast("正在连接目标设备…");
    }
    return Bridge.openSession(cfg)
      .catch(function (err) {
        pendingToolAfterConnect = null;
        toast("发起连接失败：" + err);
      })
      .then(function () {
        sessionLaunching = false;
        updateConnectState();
      });
  }

  function promptAndLaunchSession(target, displayTarget, targetKindLabel, options) {
    if (options.promptPassword === false) {
      return launchSessionWithTarget(target, options.watchOnly, null, options.displayName);
    }
    var field = createPasswordField({
      placeholder: options.placeholder || "请输入对方验证码（无密码可留空）",
      ariaLabel: "对方验证码"
    });
    var input = field.input;

    var actions = el("div", "rc-modal__actions");
    var cancelBtn = el("button", "rc-btn rc-btn--secondary", "取消");
    cancelBtn.type = "button";
    var okBtn = el("button", "rc-btn rc-btn--primary", options.watchOnly ? "观看连接" : "连接");
    okBtn.type = "button";
    actions.appendChild(cancelBtn);
    actions.appendChild(okBtn);

    var submit = function () {
      var pw = input.value.trim();
      closeModal();
      launchSessionWithTarget(target, options.watchOnly, pw, options.displayName);
    };

    cancelBtn.addEventListener("click", function () {
      pendingToolAfterConnect = null;
      closeModal();
    });
    okBtn.addEventListener("click", submit);
    input.addEventListener("keydown", function (e) {
      if (e.isComposing || e.keyCode === 229) return;
      if (e.key === "Enter") {
        e.preventDefault();
        submit();
      }
    });

    var title = options.displayName
      ? "连接至 " + options.displayName
      : "连接至 " + displayTarget;
    showModal({
      title: title,
      content: field.wrap,
      actions: actions,
      focusEl: input,
      onCancel: function () {
        pendingToolAfterConnect = null;
      }
    });
  }

  function connectToTarget(rawTarget, opts) {
    var options = opts || {};
    var trimmed = String(rawTarget || "").trim();
    if (!trimmed) {
      pendingToolAfterConnect = null;
      toast("请输入 9 位设备码或 IP:端口");
      return;
    }
    var deviceId = parseDeviceIdInput(trimmed);
    if (deviceId) {
      if (!state.serverStatus || state.serverStatus.state !== "connected") {
        pendingToolAfterConnect = null;
        toast("请先在「设置 → 网络」配置并连接服务器");
        return;
      }
      promptAndLaunchSession(deviceId, formatDeviceId(deviceId), "目标设备码", options);
      return;
    }
    Bridge.parseConnectTarget(trimmed)
      .then(function (addr) {
        promptAndLaunchSession(addr, addr, "目标地址", options);
      })
      .catch(function (err) {
        pendingToolAfterConnect = null;
        var msg = String(err || "");
        if (!msg || msg.indexOf("暂仅支持 IP:端口直连") !== -1) {
          msg = "请输入 9 位设备码（如 604 928 235）或 IP:端口（如 192.168.1.20:9901）";
        }
        toast(msg);
      });
  }

  function openCodeSettingsModal(opts) {
    var options = opts || {};
    var curPw = (state.hostStatus && state.hostStatus.customPassword && state.hostStatus.password) || "";
    var field = createPasswordField({
      value: curPw,
      placeholder: "输入自定义验证码（留空则关闭密码）",
      ariaLabel: "自定义验证码"
    });
    var input = field.input;

    var actions = el("div", "rc-modal__actions rc-modal__actions--split");
    var regenBtn = el("button", "rc-btn rc-btn--secondary", "刷新临时验证码");
    regenBtn.type = "button";

    var right = el("div", "rc-modal__actions-right");
    var cancelBtn = el("button", "rc-btn rc-btn--secondary", "取消");
    cancelBtn.type = "button";
    var saveBtn = el("button", "rc-btn rc-btn--primary", "保存自定义");
    saveBtn.type = "button";
    right.appendChild(cancelBtn);
    right.appendChild(saveBtn);

    actions.appendChild(regenBtn);
    actions.appendChild(right);

    var handleCancel = function () {
      closeModal();
      if (typeof options.onCancel === "function") options.onCancel();
    };

    regenBtn.addEventListener("click", function () {
      closeModal();
      Bridge.setHostPassword(null, true)
        .then(function (snap) {
          applyHostStatus(snap);
          toast("已生成新的临时验证码");
        })
        .catch(function (err) {
          toast("刷新验证码失败：" + err);
        });
    });

    var saveCustom = function () {
      var val = input.value.trim();
      closeModal();
      Bridge.setHostPassword(val.length > 0 ? val : null, false)
        .then(function (snap) {
          applyHostStatus(snap);
          toast(val.length > 0 ? "已保存自定义验证码" : "已关闭密码（仅机主审批）");
        })
        .catch(function (err) {
          toast("保存验证码失败：" + err);
        });
    };

    cancelBtn.addEventListener("click", handleCancel);
    saveBtn.addEventListener("click", saveCustom);
    input.addEventListener("keydown", function (e) {
      if (e.isComposing || e.keyCode === 229) return;
      if (e.key === "Enter") {
        e.preventDefault();
        saveCustom();
      }
    });

    showModal({
      title: "验证码设置",
      desc: "可重新生成 8 位临时验证码，或设置固定自定义验证码。",
      content: field.wrap,
      actions: actions,
      focusEl: input,
      onCancel: options.onCancel
    });
  }

  function openApprovalModal(prompt) {
    if (!prompt || prompt.generation == null) return;
    if (activeApprovalGen === prompt.generation) return;

    var meta = el("div", "rc-modal__meta");
    var rows = [
      ["来源地址", prompt.remote_addr || "未知"],
      [
        "请求规格",
        (prompt.requested_res || "--") +
          (prompt.requested_fps ? " @ " + prompt.requested_fps + " fps" : "")
      ],
      ["密码校验", prompt.password_verified ? "已通过" : "未启用密码"]
    ];
    rows.forEach(function (pair) {
      var r = el("div", "rc-modal__meta-row");
      r.appendChild(el("span", "rc-modal__meta-label", pair[0]));
      r.appendChild(el("span", "rc-modal__meta-val", pair[1]));
      meta.appendChild(r);
    });

    var actions = el("div", "rc-modal__actions");
    var rejectBtn = el("button", "rc-btn rc-btn--secondary", "拒绝");
    rejectBtn.type = "button";
    var acceptBtn = el("button", "rc-btn rc-btn--primary", "接受");
    acceptBtn.type = "button";
    actions.appendChild(rejectBtn);
    actions.appendChild(acceptBtn);

    var gen = prompt.generation;
    var respond = function (accepted) {
      closeModal();
      Bridge.respondApproval(gen, accepted)
        .then(function () {
          toast(accepted ? "已同意远程控制请求" : "已拒绝远程控制请求");
        })
        .catch(function (err) {
          toast("审批提交失败：" + err);
        });
    };

    rejectBtn.addEventListener("click", function () {
      respond(false);
    });
    acceptBtn.addEventListener("click", function () {
      respond(true);
    });

    showModal({
      title: "收到远程控制请求",
      desc: "是否允许对方控制本机屏幕与输入？",
      content: meta,
      actions: actions,
      dismissible: false,
      approvalGeneration: gen,
      focusEl: acceptBtn
    });
  }

  function enableHostService() {
    if (p2Allow) p2Allow.disabled = true;
    return Bridge.setHostEnabled(true)
      .then(function (snap) {
        applyHostStatus(snap);
        toast(snap && snap.enabled ? "已开启本机被控服务" : "已关闭本机被控服务");
      })
      .catch(function (err) {
        if (p2Allow) p2Allow.checked = false;
        state.allowAssist = false;
        renderP2Body();
        toast("开启被控服务失败：" + err);
      })
      .then(function () {
        if (p2Allow) p2Allow.disabled = false;
      });
  }

  function openPermissionModal(initialPerms, opts) {
    var modalOpts = opts || {};
    var enableHostOnReady = modalOpts.enableHostOnReady !== false;
    var closed = false;
    var body = el("div", "rc-perm-body");
    var listEl = el("div", "rc-perm-list");
    var hintEl = el("p", "rc-perm-hint");
    body.appendChild(listEl);
    body.appendChild(hintEl);

    function renderPermContent(perms) {
      listEl.textContent = "";
      var items = [
        {
          name: "屏幕录制（必需）",
          granted: Boolean(perms && perms.screenRecording),
          required: true,
          kind: "screen_recording",
          canRequest: true
        },
        {
          name: "辅助功能（必需）",
          granted: Boolean(perms && perms.accessibility),
          required: true,
          kind: "accessibility",
          canRequest: false
        },
        {
          name: "输入监控（可选）",
          granted: Boolean(perms && perms.inputMonitoring),
          required: false,
          kind: "input_monitoring",
          canRequest: false
        }
      ];

      items.forEach(function (item) {
        var row = el("div", "rc-perm-row");
        var info = el("div", "rc-perm-row__info");
        info.appendChild(el("span", "rc-perm-row__name", item.name));

        var pillCls = item.granted
          ? "rc-perm-pill rc-perm-pill--ok"
          : item.required
            ? "rc-perm-pill rc-perm-pill--missing"
            : "rc-perm-pill rc-perm-pill--optional";
        info.appendChild(el("span", pillCls, item.granted ? "已授权" : "未授权"));
        row.appendChild(info);

        if (!item.granted) {
          var rowActions = el("div", "rc-perm-row__actions");
          if (item.canRequest) {
            var reqBtn = el("button", "rc-btn rc-btn--secondary rc-perm-btn", "请求授权");
            reqBtn.type = "button";
            reqBtn.addEventListener("click", function () {
              reqBtn.disabled = true;
              Bridge.requestScreenRecordingPermission()
                .then(function () {
                  if (closed) return null;
                  return Bridge.checkPermissions();
                })
                .then(function (latest) {
                  if (closed || !latest) return;
                  renderPermContent(latest);
                })
                .catch(function (err) {
                  toast("请求屏幕录制授权失败：" + err);
                })
                .then(function () {
                  reqBtn.disabled = false;
                });
            });
            rowActions.appendChild(reqBtn);
          }

          var openBtn = el("button", "rc-btn rc-btn--secondary rc-perm-btn", "系统设置");
          openBtn.type = "button";
          openBtn.addEventListener("click", function () {
            Bridge.openPermissionSettings(item.kind).catch(function (err) {
              toast("打开系统设置失败：" + err);
            });
          });
          rowActions.appendChild(openBtn);
          row.appendChild(rowActions);
        }

        listEl.appendChild(row);
      });

      var restartNotice =
        perms && typeof perms.restartHint === "string" && perms.restartHint.trim()
          ? perms.restartHint.trim()
          : "授权勾选后若状态未立即更新，请重启 RabbitControl 使其生效。";
      hintEl.textContent = restartNotice;
    }

    renderPermContent(initialPerms);

    var actions = el("div", "rc-modal__actions");
    var cancelBtn = el("button", "rc-btn rc-btn--secondary", "取消");
    cancelBtn.type = "button";
    var recheckBtn = el(
      "button",
      "rc-btn rc-btn--primary",
      enableHostOnReady ? "重新检测并开启" : "重新检测"
    );
    recheckBtn.type = "button";
    actions.appendChild(cancelBtn);
    actions.appendChild(recheckBtn);

    cancelBtn.addEventListener("click", function () {
      closed = true;
      closeModal();
    });

    recheckBtn.addEventListener("click", function () {
      if (recheckBtn.disabled) return;
      recheckBtn.disabled = true;
      recheckBtn.textContent = "检测中…";
      Bridge.checkPermissions()
        .then(function (latest) {
          if (closed) return;
          if (!latest || !latest.platformSupported || latest.hostReady) {
            closed = true;
            closeModal();
            if (enableHostOnReady) {
              enableHostService();
            } else {
              checkAndRenderPermBanner();
              toast("系统权限已就绪");
            }
            return;
          }
          renderPermContent(latest);
          recheckBtn.disabled = false;
          recheckBtn.textContent = enableHostOnReady ? "重新检测并开启" : "重新检测";
          toast("仍有必需权限未授权，请完成授权后再试");
        })
        .catch(function (err) {
          if (closed) return;
          recheckBtn.disabled = false;
          recheckBtn.textContent = enableHostOnReady ? "重新检测并开启" : "重新检测";
          toast("检测系统权限失败：" + err);
        });
    });

    showModal({
      title: enableHostOnReady ? "开启远程协助前需要授权" : "授予 macOS 远程控制权限",
      desc: "macOS 需要以下系统权限才能共享画面并响应远端键鼠控制：",
      content: body,
      actions: actions,
      modalClass: "rc-modal--wide",
      focusEl: recheckBtn,
      onCancel: function () {
        closed = true;
      }
    });
  }

  function applyHostStatus(snap) {
    if (!snap) return;
    state.hostStatus = snap;
    state.allowAssist = Boolean(snap.enabled);
    if (p2Allow) p2Allow.checked = state.allowAssist;

    renderP2DeviceId();

    if (p2VerifyMode) {
      var idx = snap.customPassword ? 1 : 0;
      p2VerifyMode.selectedIndex = idx;
      state.verifyMode = idx;
    }

    if (p2Refresh) {
      p2Refresh.textContent = "";
      if (snap.connectedPeer) {
        var wrap = el("span", "p2__refresh-row");
        wrap.appendChild(document.createTextNode("已连接：" + snap.connectedPeer));
        var discBtn = el("button", "rc-btn rc-btn--secondary p2__disconnect-btn", "断开");
        discBtn.type = "button";
        discBtn.addEventListener("click", function () {
          Bridge.disconnectHostClient()
            .then(function () {
              toast("已断开当前控制端");
            })
            .catch(function (err) {
              toast("断开失败：" + err);
            });
        });
        wrap.appendChild(discBtn);
        p2Refresh.appendChild(wrap);
      } else if (snap.customPassword) {
        p2Refresh.textContent = snap.password ? "已启用自定义验证码" : "当前未设置密码（仅机主审批）";
      } else {
        p2Refresh.textContent = "每次远控结束刷新";
      }
    }

    renderP2Body();
    renderP2Code();

    if (snap.pendingApproval) {
      openApprovalModal(snap.pendingApproval);
    } else if (activeApprovalGen != null) {
      closeModal();
    }
  }

  function refreshHostStatus() {
    if (!IS_TAURI || typeof Bridge.getHostStatus !== "function") {
      return Promise.resolve();
    }
    return Bridge.getHostStatus()
      .then(applyHostStatus)
      .catch(function () {});
  }

  function handleHostUpdate(ev) {
    if (!ev || !ev.kind) return;
    if (ev.kind === "approval_requested") {
      openApprovalModal(ev);
      return;
    }
    if (ev.kind === "approval_cancelled") {
      if (activeApprovalGen === ev.generation) {
        closeModal();
        toast("连接审批已取消或超时");
      }
      return;
    }
    if (ev.kind === "client_connected") {
      if (activeApprovalGen != null) closeModal();
      toast("控制端已连接" + (ev.remote_addr ? "：" + ev.remote_addr : ""));
      refreshHostStatus();
      return;
    }
    if (ev.kind === "owner_connected") {
      if (activeApprovalGen != null) closeModal();
      toast("我的设备（" + (ev.caller_device_id || "--") + "）已通过免密自控接入");
      refreshHostStatus();
      return;
    }
    if (ev.kind === "client_disconnected") {
      toast("控制端已断开" + (ev.reason ? "：" + ev.reason : ""));
      refreshHostStatus();
      return;
    }
    if (ev.kind === "listening" || ev.kind === "stopped") {
      refreshHostStatus();
      return;
    }
    if (ev.kind === "error") {
      toast("被控端错误：" + (ev.message || "未知错误"));
    }
  }

  function applyMyDevices(list) {
    var rawList = Array.isArray(list) ? list : [];
    state.myDevices = rawList.map(normalizeMyDevice);
    if (IS_TAURI) {
      var curExists = state.myDevices.some(function (d) {
        return d.id === state.deviceId;
      });
      if (!curExists) {
        var preferred = null;
        for (var i = 0; i < state.myDevices.length; i++) {
          if (!state.myDevices[i].isLocal && state.myDevices[i].online) {
            preferred = state.myDevices[i];
            break;
          }
        }
        if (!preferred && state.myDevices.length > 0) {
          preferred = state.myDevices[0];
        }
        if (preferred) {
          state.deviceId = preferred.id;
          if (state.route === "p1" || state.route === "p2") {
            go("p1", "device:" + preferred.id);
          }
        } else {
          state.deviceId = null;
        }
      }
    }
    paintSidebarRows();
    if (state.route === "p3") renderP3();
    if (state.route === "p1") {
      renderP1();
      renderTitlebar();
    }
  }

  function applyDeviceStatusChanged(ev) {
    if (!ev || typeof ev !== "object") return;
    var did = String(ev.device_id || ev.deviceId || "").replace(/\s+/g, "");
    if (!did) return;
    var found = false;
    for (var i = 0; i < state.myDevices.length; i++) {
      var d = state.myDevices[i];
      if (d.id === did) {
        if (typeof ev.online === "boolean") d.online = ev.online;
        if (typeof ev.hostname === "string" && ev.hostname) d.hostname = ev.hostname;
        if (typeof ev.alias === "string") d.alias = ev.alias;
        if (typeof ev.os === "string" && ev.os) {
          d.os = ev.os;
          d.icon = iconForOs(ev.os);
          d.group = groupForOs(ev.os);
        }
        if (typeof ev.allow_remote_assist === "boolean") {
          d.allowRemoteAssist = ev.allow_remote_assist;
        } else if (typeof ev.allowRemoteAssist === "boolean") {
          d.allowRemoteAssist = ev.allowRemoteAssist;
        }
        d.name = d.alias.trim() || d.hostname.trim() || d.formattedId || did;
        found = true;
        break;
      }
    }
    if (!found) {
      refreshMyDevices();
      return;
    }
    paintSidebarRows();
    if (state.route === "p3") renderP3();
    if (state.route === "p1") {
      renderP1();
      renderTitlebar();
    }
  }

  function refreshMyDevices() {
    if (!IS_TAURI || typeof Bridge.listMyDevices !== "function") {
      return Promise.resolve();
    }
    return Bridge.listMyDevices()
      .then(applyMyDevices)
      .catch(function (err) {
        if (String(err || "").indexOf("登录") !== -1) {
          refreshGateStatus();
        }
      });
  }

  /* ── 设备上下文菜单 / 重命名 / 移除 / 改密弹窗 ───────────────── */
  var activeCtxMenu = null;

  function closeDeviceContextMenu() {
    if (activeCtxMenu && activeCtxMenu.parentNode) {
      activeCtxMenu.parentNode.removeChild(activeCtxMenu);
    }
    activeCtxMenu = null;
  }

  function openRenameDeviceModal(device) {
    if (!device || !device.deviceId) return;
    var input = el("input", "rc-modal__input");
    input.type = "text";
    input.value = device.alias || device.name || "";
    input.placeholder = "请输入设备备注名称";
    input.setAttribute("aria-label", "设备备注");

    var actions = el("div", "rc-modal__actions");
    var cancelBtn = el("button", "rc-btn rc-btn--secondary", "取消");
    cancelBtn.type = "button";
    var saveBtn = el("button", "rc-btn rc-btn--primary", "保存");
    saveBtn.type = "button";
    actions.appendChild(cancelBtn);
    actions.appendChild(saveBtn);

    var submit = function () {
      var nextAlias = input.value.trim();
      saveBtn.disabled = true;
      Bridge.renameMyDevice(device.deviceId, nextAlias)
        .then(function () {
          closeModal();
          toast("已更新设备备注");
          refreshMyDevices();
        })
        .catch(function (err) {
          saveBtn.disabled = false;
          toast("修改备注失败：" + err);
        });
    };

    cancelBtn.addEventListener("click", closeModal);
    saveBtn.addEventListener("click", submit);
    input.addEventListener("keydown", function (e) {
      if (e.isComposing || e.keyCode === 229) return;
      if (e.key === "Enter") {
        e.preventDefault();
        submit();
      }
    });

    showModal({
      title: "修改设备备注",
      desc: "设备码：" + (device.formattedId || device.deviceId),
      content: input,
      actions: actions,
      focusEl: input
    });
  }

  function openRemoveDeviceModal(device) {
    if (!device || !device.deviceId) return;
    var actions = el("div", "rc-modal__actions");
    var cancelBtn = el("button", "rc-btn rc-btn--secondary", "取消");
    cancelBtn.type = "button";
    var delBtn = el("button", "rc-btn rc-btn--danger", "移除设备");
    delBtn.type = "button";
    actions.appendChild(cancelBtn);
    actions.appendChild(delBtn);

    cancelBtn.addEventListener("click", closeModal);
    delBtn.addEventListener("click", function () {
      delBtn.disabled = true;
      Bridge.removeMyDevice(device.deviceId)
        .then(function () {
          closeModal();
          toast("已移除设备「" + device.name + "」");
          refreshMyDevices();
        })
        .catch(function (err) {
          delBtn.disabled = false;
          toast("移除设备失败：" + err);
        });
    });

    showModal({
      title: "确认移除设备",
      desc: "移除后「" + device.name + "」将从您的账号设备列表解绑，如需再次使用请重新登录绑定。",
      actions: actions,
      focusEl: cancelBtn
    });
  }

  function openChangePasswordModal() {
    var wrap = el("div", "gate-form");
    var curField = createPasswordField({ placeholder: "当前密码", ariaLabel: "当前密码" });
    var newField = createPasswordField({ placeholder: "新密码（至少 8 位）", ariaLabel: "新密码" });
    var confField = createPasswordField({ placeholder: "再次输入新密码", ariaLabel: "确认新密码" });
    wrap.appendChild(curField.wrap);
    wrap.appendChild(newField.wrap);
    wrap.appendChild(confField.wrap);

    var actions = el("div", "rc-modal__actions");
    var cancelBtn = el("button", "rc-btn rc-btn--secondary", "取消");
    cancelBtn.type = "button";
    var okBtn = el("button", "rc-btn rc-btn--primary", "确认修改");
    okBtn.type = "button";
    actions.appendChild(cancelBtn);
    actions.appendChild(okBtn);

    cancelBtn.addEventListener("click", closeModal);
    okBtn.addEventListener("click", function () {
      var curPw = curField.input.value;
      var newPw = newField.input.value;
      var confPw = confField.input.value;
      if (!curPw || !newPw) {
        toast("请填写当前密码与新密码");
        return;
      }
      if (newPw.length < 8) {
        toast("新密码至少需要 8 位字符");
        return;
      }
      if (newPw !== confPw) {
        toast("两次输入的新密码不一致");
        return;
      }
      okBtn.disabled = true;
      Bridge.authChangePassword({ currentPassword: curPw, newPassword: newPw })
        .then(function (snap) {
          closeModal();
          applyGateStatus(snap);
          toast("账号密码已修改");
        })
        .catch(function (err) {
          okBtn.disabled = false;
          toast("修改密码失败：" + err);
        });
    });

    showModal({
      title: "修改账号密码",
      desc: "修改成功后将自动轮换当前登录令牌。",
      content: wrap,
      actions: actions,
      focusEl: curField.input
    });
  }

  function openDeviceContextMenu(x, y, device) {
    closeDeviceContextMenu();
    if (!IS_TAURI || !device || !device.isMine) return;
    var menu = el("div", "rc-ctx-menu");
    menu.setAttribute("role", "menu");

    var connBtn = el("button", "rc-ctx-menu__item", "连接桌面");
    connBtn.type = "button";
    connBtn.disabled = Boolean(device.isLocal || !device.online);
    connBtn.addEventListener("click", function () {
      closeDeviceContextMenu();
      openSession(false, device);
    });

    var transferBtn = el("button", "rc-ctx-menu__item", "文件传输");
    transferBtn.type = "button";
    transferBtn.disabled = Boolean(device.isLocal || !device.online);
    transferBtn.addEventListener("click", function () {
      closeDeviceContextMenu();
      openTool("transfer", device);
    });

    var terminalBtn = el("button", "rc-ctx-menu__item", "远程终端");
    terminalBtn.type = "button";
    terminalBtn.disabled = Boolean(device.isLocal || !device.online);
    terminalBtn.addEventListener("click", function () {
      closeDeviceContextMenu();
      openTool("terminal", device);
    });

    var portmapBtn = el("button", "rc-ctx-menu__item", "端口映射");
    portmapBtn.type = "button";
    portmapBtn.disabled = Boolean(device.isLocal || !device.online);
    portmapBtn.addEventListener("click", function () {
      closeDeviceContextMenu();
      openTool("portmap", device);
    });

    var renameBtn = el("button", "rc-ctx-menu__item", "修改备注");
    renameBtn.type = "button";
    renameBtn.addEventListener("click", function () {
      closeDeviceContextMenu();
      openRenameDeviceModal(device);
    });

    var removeBtn = el("button", "rc-ctx-menu__item rc-ctx-menu__item--danger", "移除设备");
    removeBtn.type = "button";
    removeBtn.addEventListener("click", function () {
      closeDeviceContextMenu();
      openRemoveDeviceModal(device);
    });

    menu.appendChild(connBtn);
    menu.appendChild(transferBtn);
    menu.appendChild(terminalBtn);
    menu.appendChild(portmapBtn);
    menu.appendChild(renameBtn);
    menu.appendChild(removeBtn);
    document.body.appendChild(menu);

    var vw = window.innerWidth || document.documentElement.clientWidth;
    var vh = window.innerHeight || document.documentElement.clientHeight;
    var left = Math.max(8, Math.min(x, vw - menu.offsetWidth - 8));
    var top = Math.max(8, Math.min(y, vh - menu.offsetHeight - 8));
    menu.style.left = left + "px";
    menu.style.top = top + "px";
    activeCtxMenu = menu;
  }

  document.addEventListener("pointerdown", function (e) {
    if (activeCtxMenu && !activeCtxMenu.contains(e.target)) {
      closeDeviceContextMenu();
    }
  }, true);

  /* 掩码字符数 = 明文长度（等宽显示下不跳动） */
  function maskedCode(code) {
    return new Array(code.length + 1).join("•");
  }

  function renderP2Body() {
    p2Body.classList.toggle("is-disabled", !state.allowAssist);
    /* 关闭时右侧验证码区不可交互；#p2DeviceSub 内的「去配置」及 #p2RefreshId 始终保持可点击 */
    var controls = p2Body.querySelectorAll("button, input, select");
    Array.prototype.forEach.call(controls, function (c) {
      if (p2DeviceSub && p2DeviceSub.contains(c)) {
        c.disabled = false;
        return;
      }
      if (p2RefreshId && c === p2RefreshId) {
        c.disabled = refreshingDeviceId;
        return;
      }
      c.disabled = !state.allowAssist;
    });
    p2Body.setAttribute("aria-disabled", state.allowAssist ? "false" : "true");
  }

  function renderP2SessionBar() {
    if (!p2SessionBar) return;
    var active = Boolean(state.activeSession && state.activeSession.active);
    p2SessionBar.hidden = !active;
    if (!active) return;
    var label = state.activeSession.deviceName || state.activeSession.target || "远端设备";
    if (p2SessionText) {
      p2SessionText.textContent = "正在远控 " + label;
    }
    var canTools = Boolean(state.activeSession.connected && !state.activeSession.watchOnly);
    [p2OpenTransfer, p2OpenTerminal, p2OpenPortmap].forEach(function (btn) {
      if (!btn) return;
      btn.hidden = !canTools;
      btn.disabled = !canTools;
    });
  }

  function renderP2Code() {
    var real = IS_TAURI && state.hostStatus ? state.hostStatus.password || "" : MOCK.assist.code;
    if (IS_TAURI && state.hostStatus && !real) {
      p2Code.textContent = "无密码";
    } else {
      p2Code.textContent = state.codeVisible ? real : maskedCode(real);
    }
    p2Eye.setAttribute("aria-pressed", state.codeVisible ? "false" : "true");
    p2Eye.setAttribute("aria-label", state.codeVisible ? "隐藏验证码" : "显示验证码");
    p2Eye.title = p2Eye.getAttribute("aria-label");
    /* eye = 当前明文（点击转为掩码）/ eye-off = 当前掩码 */
    var use = p2Eye.querySelector("use");
    if (use) {
      var href = state.codeVisible ? "#i-eye" : "#i-eye-off";
      use.setAttribute("href", href);
      use.setAttributeNS("http://www.w3.org/1999/xlink", "xlink:href", href);
    }
  }

  function closeHistory() {
    if (p2History) {
      p2History.hidden = true;
    }
    p2HistoryBtn.setAttribute("aria-expanded", "false");
  }

  function updateConnectState() {
    if (!p2Connect || !p2PeerInput) return;
    p2Connect.disabled = sessionLaunching || p2PeerInput.value.trim().length === 0;
  }

  function wireP2() {
    if (p2PeerInput) {
      p2PeerInput.placeholder = "请输入 9 位设备码或 IP:端口";
    }

    if (p2FocusSession) {
      p2FocusSession.addEventListener("click", focusSessionWindow);
    }

    if (p2DisconnectSession) {
      p2DisconnectSession.addEventListener("click", disconnectActiveSession);
    }

    if (p2OpenTransfer) {
      p2OpenTransfer.addEventListener("click", function () {
        openWindow("transfer");
      });
    }
    if (p2OpenTerminal) {
      p2OpenTerminal.addEventListener("click", function () {
        openWindow("terminal");
      });
    }
    if (p2OpenPortmap) {
      p2OpenPortmap.addEventListener("click", function () {
        openWindow("portmap");
      });
    }

    if (p2RefreshId) {
      p2RefreshId.addEventListener("click", function () {
        if (IS_TAURI) {
          if (refreshingDeviceId) return;
          if (typeof Bridge.refreshDeviceId !== "function") {
            toast("当前版本暂不支持刷新设备 ID");
            return;
          }
          refreshingDeviceId = true;
          p2RefreshId.disabled = true;
          p2RefreshId.classList.add("is-spinning");
          Bridge.refreshDeviceId()
            .then(
              function (snap) {
                state.lastDeviceCode = null;
                applyServerStatus(snap);
                toast("已更换新的设备 ID");
              },
              function (err) {
                toast("刷新设备 ID 失败：" + err);
              }
            )
            .then(function () {
              refreshingDeviceId = false;
              p2RefreshId.disabled = false;
              p2RefreshId.classList.remove("is-spinning");
            });
          return;
        }
        var randId = String(Math.floor(100000000 + Math.random() * 900000000));
        var formatted = formatDeviceId(randId);
        if (p2DeviceId) {
          p2DeviceId.textContent = formatted;
          p2DeviceId.title = formatted;
        }
        toast("已更换新的设备 ID");
      });
    }

    p2Allow.addEventListener("change", function () {
      if (IS_TAURI) {
        var desired = p2Allow.checked;
        if (desired) {
          p2Allow.disabled = true;
          var permCheck =
            typeof Bridge.checkPermissions === "function"
              ? Bridge.checkPermissions()
              : Promise.resolve(null);
          permCheck
            .then(function (perms) {
              if (perms && perms.platformSupported && !perms.hostReady) {
                p2Allow.checked = false;
                state.allowAssist = false;
                renderP2Body();
                p2Allow.disabled = false;
                openPermissionModal(perms);
                return;
              }
              return enableHostService();
            })
            .catch(function (err) {
              p2Allow.checked = false;
              state.allowAssist = false;
              renderP2Body();
              p2Allow.disabled = false;
              toast("检查系统权限失败：" + err);
            });
          return;
        }
        p2Allow.disabled = true;
        Bridge.setHostEnabled(false)
          .then(function (snap) {
            applyHostStatus(snap);
            toast(snap.enabled ? "已开启本机被控服务" : "已关闭本机被控服务");
          })
          .catch(function (err) {
            p2Allow.checked = true;
            state.allowAssist = true;
            renderP2Body();
            toast("切换被控服务失败：" + err);
          })
          .then(function () {
            p2Allow.disabled = false;
          });
        return;
      }
      state.allowAssist = p2Allow.checked;
      renderP2Body();
    });

    p2Eye.addEventListener("click", function () {
      state.codeVisible = !state.codeVisible;
      renderP2Code();
    });

    $("#p2CodeSettings").addEventListener("click", function () {
      if (IS_TAURI) {
        openCodeSettingsModal();
        return;
      }
      toast("功能开发中");
    });

    /* 「复制并分享」→ 优先复制 9 位设备 ID + 验证码；仅无服务器设备 ID 且有局域网地址时回退局域网地址 */
    p2Copy.addEventListener("click", function () {
      var text;
      var doneMsg;
      var deviceCode = IS_TAURI ? getConnectedDeviceCode() : null;
      if (IS_TAURI) {
        var addr = state.hostStatus
          ? state.hostStatus.displayAddr || state.hostStatus.listenAddr || ""
          : "";
        var pw = state.hostStatus && state.hostStatus.password ? state.hostStatus.password : "无";
        if (deviceCode) {
          text = "设备 ID: " + deviceCode + "\n验证码: " + pw;
          doneMsg = "已复制设备 ID 与验证码";
        } else if (addr) {
          text = "局域网地址: " + addr + "\n验证码: " + pw;
          doneMsg = "已复制局域网地址与验证码";
        } else {
          toast("尚未获取设备 ID 或局域网地址");
          return;
        }
      } else {
        text = MOCK.assist.deviceId + " · " + MOCK.assist.code;
        doneMsg = "已复制设备 ID 与验证码";
      }
      var done = function () {
        toast(doneMsg);
      };
      var webCopy = function () {
        if (navigator.clipboard && navigator.clipboard.writeText) {
          navigator.clipboard.writeText(text).then(done, function () {
            fallbackCopy(text);
            done();
          });
        } else {
          fallbackCopy(text);
          done();
        }
      };
      if (IS_TAURI) {
        Bridge.copyText(text).then(done, webCopy);
      } else {
        webCopy();
      }
    });

    /* 验证方式 Select：Tauri 下切换临时/自定义验证码；浏览器预览保持原 mock */
    p2VerifyMode.addEventListener("change", function (e) {
      var idx = e.target.selectedIndex;
      if (IS_TAURI) {
        if (idx === 0) {
          Bridge.setHostPassword(null, true)
            .then(function (snap) {
              applyHostStatus(snap);
              toast("已切换为临时验证码");
            })
            .catch(function (err) {
              toast("切换验证方式失败：" + err);
              refreshHostStatus();
            });
        } else {
          openCodeSettingsModal({
            onCancel: function () {
              p2VerifyMode.selectedIndex =
                state.hostStatus && state.hostStatus.customPassword ? 1 : 0;
            }
          });
        }
        return;
      }
      state.verifyMode = idx;
    });

    /* Input 内嵌下拉 = 历史记录，本期空列表 */
    p2HistoryBtn.addEventListener("click", function (e) {
      e.stopPropagation();
      if (p2History && !p2History.hidden) {
        closeHistory();
        return;
      }
      /* §7.4：同时只允许一个 popover —— 开历史面板前先收起其余浮层 */
      if (Overlay && typeof Overlay.closeAll === "function") Overlay.closeAll();
      if (!p2History) {
        p2History = el("div", "p2__history");
        p2History.setAttribute("role", "listbox");
        p2History.setAttribute("aria-label", "历史记录");
        p2History.appendChild(el("div", "p2__history-empty", "暂无历史记录"));
        p2InputWrap.appendChild(p2History);
      }
      p2History.hidden = false;
      p2HistoryBtn.setAttribute("aria-expanded", "true");
    });

    document.addEventListener("click", function (e) {
      if (!p2History || p2History.hidden) return;
      if (p2History.contains(e.target) || p2HistoryBtn.contains(e.target)) return;
      closeHistory();
    });

    document.addEventListener("keydown", function (e) {
      if (e.key !== "Escape") return;
      if (p2History && !p2History.hidden) {
        closeHistory();
        e.stopPropagation();
      }
    });

    p2PeerInput.addEventListener("input", updateConnectState);
    p2PeerInput.addEventListener("keydown", function (e) {
      if (e.isComposing || e.keyCode === 229) return;
      if (e.key === "Enter" && !p2Connect.disabled) {
        e.preventDefault();
        p2Connect.click();
      }
    });

    /* 连接：Tauri 内走设备码或 parse_connect_target + 可选密码弹窗 + open_session（由后端成功后开会话窗）；
     * 浏览器预览保持 mock Toast（§6 P2）。 */
    p2Connect.addEventListener("click", function () {
      if (p2Connect.disabled) return;
      if (IS_TAURI) {
        var target = p2PeerInput.value.trim();
        connectToTarget(target, { watchOnly: false, promptPassword: true });
        return;
      }
      toast("功能开发中");
    });
  }

  function fallbackCopy(text) {
    var ta = document.createElement("textarea");
    ta.value = text;
    ta.setAttribute("readonly", "");
    ta.style.position = "fixed";
    ta.style.top = "-1000px";
    ta.style.opacity = "0";
    document.body.appendChild(ta);
    ta.select();
    try {
      document.execCommand("copy");
    } catch (err) {
      /* 忽略：file:// 无剪贴板权限时静默（Toast 仍提示已复制，属 mock） */
    }
    document.body.removeChild(ta);
  }

  /* ── P3 全部设备（§6 P3）───────────────────────────────────── */
  var p3Groups = $("#p3Groups");
  var p3Empty = $("#p3Empty");

  function buildP3Group(key, title, list) {
    var group = el("div", "p3__group");
    if (state.p3Collapsed[key]) group.classList.add("is-collapsed");

    var head = el("button", "p3__group-head");
    head.type = "button";
    head.setAttribute("aria-expanded", state.p3Collapsed[key] ? "false" : "true");

    var chev = el("span", "p3__group-head-icon");
    chev.appendChild(useIcon("chevron-down", 16));
    head.appendChild(chev);

    head.appendChild(el("span", "p3__group-title", title));
    head.appendChild(el("span", "p3__group-count", String(list.length)));

    head.addEventListener("click", function () {
      state.p3Collapsed[key] = !state.p3Collapsed[key];
      group.classList.toggle("is-collapsed", state.p3Collapsed[key]);
      head.setAttribute("aria-expanded", state.p3Collapsed[key] ? "false" : "true");
    });
    group.appendChild(head);

    var rows = el("div", "p3__rows");
    list.forEach(function (device) {
      rows.appendChild(buildP3Row(device));
    });
    group.appendChild(rows);
    return group;
  }

  function buildP3Row(device) {
    var row = el("div", "p3__row");
    row.setAttribute("role", "button");
    row.tabIndex = 0;
    row.setAttribute("aria-label", device.name);

    var icon = el("span", "p3__icon");
    icon.appendChild(useIcon(device.icon, 20));
    row.appendChild(icon);

    var ident = el("span", "p3__ident");
    ident.appendChild(el("span", "dot " + (device.online ? "dot--online" : "dot--offline")));
    ident.appendChild(el("span", "p3__name", device.name));
    if (device.isLocal) {
      ident.appendChild(el("span", "pill pill--local", "本机"));
    }
    if (device.formattedId) {
      ident.appendChild(el("span", "p3__device-id", device.formattedId));
    }
    row.appendChild(ident);

    var actions = el("span", "p3__actions");

    if (device.isMine) {
      if (!device.isLocal) {
        var connectBtn = el("button", "rc-btn rc-btn--secondary p3__action-pill", "连接桌面");
        connectBtn.type = "button";
        connectBtn.disabled = !device.online;
        connectBtn.addEventListener("click", function (e) {
          e.stopPropagation();
          openSession(false, device);
        });
        actions.appendChild(connectBtn);
      }

      var renameBtn = el("button", "rc-btn rc-btn--ghost p3__row-btn");
      renameBtn.type = "button";
      renameBtn.setAttribute("aria-label", "修改备注");
      renameBtn.title = "修改备注";
      renameBtn.appendChild(useIcon("pencil", 16));
      renameBtn.addEventListener("click", function (e) {
        e.stopPropagation();
        openRenameDeviceModal(device);
      });
      actions.appendChild(renameBtn);

      var removeBtn = el("button", "rc-btn rc-btn--ghost p3__row-btn p3__row-btn--danger");
      removeBtn.type = "button";
      removeBtn.setAttribute("aria-label", "移除设备");
      removeBtn.title = "移除设备";
      removeBtn.appendChild(useIcon("trash-2", 16));
      removeBtn.addEventListener("click", function (e) {
        e.stopPropagation();
        openRemoveDeviceModal(device);
      });
      actions.appendChild(removeBtn);

      row.addEventListener("contextmenu", function (e) {
        e.preventDefault();
        openDeviceContextMenu(e.clientX, e.clientY, device);
      });
    } else {
      var infoBtn = el("button", "rc-btn rc-btn--ghost p3__row-btn");
      infoBtn.type = "button";
      infoBtn.setAttribute("aria-label", "设备信息");
      infoBtn.appendChild(useIcon("info", 16));
      infoBtn.addEventListener("click", function (e) {
        e.stopPropagation();
        toast("功能开发中");
      });
      actions.appendChild(infoBtn);
    }

    var enterBtn = el("button", "rc-btn rc-btn--ghost p3__row-btn");
    enterBtn.type = "button";
    enterBtn.setAttribute("aria-label", "进入设备");
    enterBtn.appendChild(useIcon("chevron-right", 16));
    enterBtn.addEventListener("click", function (e) {
      e.stopPropagation();
      selectDevice(device.id, "p1");
    });
    actions.appendChild(enterBtn);

    row.appendChild(actions);

    row.addEventListener("click", function () {
      selectDevice(device.id, "p1");
    });
    row.addEventListener("keydown", function (e) {
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        selectDevice(device.id, "p1");
      }
    });

    return row;
  }

  function renderP3() {
    p3Groups.textContent = "";
    if (IS_TAURI) {
      var computers = [];
      var mobiles = [];
      state.myDevices.forEach(function (d) {
        if (d.group === "mobiles") mobiles.push(d);
        else computers.push(d);
      });
      var totalMine = state.myDevices.length;
      p3Empty.hidden = totalMine > 0;
      if (totalMine === 0) return;
      if (computers.length > 0) {
        p3Groups.appendChild(buildP3Group("computers", "电脑", computers));
      }
      if (mobiles.length > 0) {
        p3Groups.appendChild(buildP3Group("mobiles", "手机/平板", mobiles));
      }
      return;
    }

    var total = MOCK.p3.computers.length + MOCK.p3.mobiles.length;

    /* §7.2 无设备空态：40px monitor-smartphone + 「暂无设备」+ 开始协助 */
    p3Empty.hidden = total > 0;

    if (total === 0) return;

    p3Groups.appendChild(buildP3Group("computers", "电脑", MOCK.p3.computers));
    p3Groups.appendChild(buildP3Group("mobiles", "手机/平板", MOCK.p3.mobiles));
  }

  function wireP3() {
    $("#p3EmptyAssist").addEventListener("click", function () {
      go("p2", "assist");
    });
  }

  /* ── 权限引导横幅 & 双阶强门禁控制器（仅 IS_TAURI 激活）──────── */
  var permBanner = $("#permBanner");
  var permBannerText = $("#permBannerText");
  var btnGrantPerms = $("#btnGrantPerms");
  var btnRecheckPerms = $("#btnRecheckPerms");
  var btnDismissPerms = $("#btnDismissPerms");
  var lastPermSnapshot = null;

  function checkAndRenderPermBanner() {
    if (!IS_TAURI || !permBanner || typeof Bridge.checkPermissions !== "function") return;
    if (!state.gateStatus || state.gateStatus.gate !== "authenticated") {
      permBanner.hidden = true;
      return;
    }
    Bridge.checkPermissions()
      .then(function (perms) {
        lastPermSnapshot = perms;
        if (!perms || !perms.platformSupported || perms.hostReady || state.permBannerDismissed) {
          permBanner.hidden = true;
          return;
        }
        var missing = Array.isArray(perms.missingRequired) && perms.missingRequired.length > 0
          ? perms.missingRequired.join("、")
          : "屏幕录制、辅助功能";
        if (permBannerText) {
          permBannerText.textContent =
            "macOS 缺少「" + missing + "」权限，本人远程自控与远程协助暂无法共享画面或注入键鼠";
        }
        permBanner.hidden = false;
      })
      .catch(function () {});
  }

  function wirePermBanner() {
    if (btnGrantPerms) {
      btnGrantPerms.addEventListener("click", function () {
        var openModalWith = function (perms) {
          openPermissionModal(perms, { enableHostOnReady: false });
        };
        if (lastPermSnapshot) {
          openModalWith(lastPermSnapshot);
          return;
        }
        Bridge.checkPermissions().then(openModalWith).catch(function (err) {
          toast("检查权限失败：" + err);
        });
      });
    }
    if (btnRecheckPerms) {
      btnRecheckPerms.addEventListener("click", function () {
        state.permBannerDismissed = false;
        checkAndRenderPermBanner();
      });
    }
    if (btnDismissPerms) {
      btnDismissPerms.addEventListener("click", function () {
        state.permBannerDismissed = true;
        if (permBanner) permBanner.hidden = true;
      });
    }
  }

  var gateOverlay = $("#gateOverlay");
  var gateTitle = $("#gateTitle");
  var gateSubtitle = $("#gateSubtitle");
  var gateStepLoading = $("#gateStepLoading");
  var gateStepServer = $("#gateStepServer");
  var gateStepAuth = $("#gateStepAuth");
  var gateStepPasswordChange = $("#gateStepPasswordChange");
  var gateServerHost = $("#gateServerHost");
  var gateAdvToggle = $("#gateAdvToggle");
  var gateAdvPanel = $("#gateAdvPanel");
  var gateUseHttps = $("#gateUseHttps");
  var gateApiPort = $("#gateApiPort");
  var gateSignalPort = $("#gateSignalPort");
  var gateRelayPort = $("#gateRelayPort");
  var gateProbeResult = $("#gateProbeResult");
  var gateProbeBadge = $("#gateProbeBadge");
  var gateProbeMeta = $("#gateProbeMeta");
  var gateCaFingerprint = $("#gateCaFingerprint");
  var gateProbeReg = $("#gateProbeReg");
  var gateServerError = $("#gateServerError");
  var gateBackToLoginBtn = $("#gateBackToLoginBtn");
  var gateProbeBtn = $("#gateProbeBtn");
  var gateConfirmServerBtn = $("#gateConfirmServerBtn");
  var gateServerSummary = $("#gateServerSummary");
  var gateChangeServerBtn = $("#gateChangeServerBtn");
  var gateTabLogin = $("#gateTabLogin");
  var gateTabRegister = $("#gateTabRegister");
  var gateRegDisabledNotice = $("#gateRegDisabledNotice");
  var gateLoginForm = $("#gateLoginForm");
  var gateLoginEmail = $("#gateLoginEmail");
  var gateLoginPassword = $("#gateLoginPassword");
  var gateLoginError = $("#gateLoginError");
  var gateLoginBtn = $("#gateLoginBtn");
  var gateRegForm = $("#gateRegForm");
  var gateRegEmail = $("#gateRegEmail");
  var gateRegName = $("#gateRegName");
  var gateRegPassword = $("#gateRegPassword");
  var gateRegConfirm = $("#gateRegConfirm");
  var gateRegError = $("#gateRegError");
  var gateRegBtn = $("#gateRegBtn");
  var gateCpForm = $("#gateCpForm");
  var gateCpCurrent = $("#gateCpCurrent");
  var gateCpNew = $("#gateCpNew");
  var gateCpConfirm = $("#gateCpConfirm");
  var gateCpError = $("#gateCpError");
  var gateCpLogoutBtn = $("#gateCpLogoutBtn");
  var gateCpSubmitBtn = $("#gateCpSubmitBtn");

  var gateLastProbe = null;
  var gateServerViewOverride = false;
  var gateAuthedInitialized = false;

  function setGateError(elErr, msg) {
    if (!elErr) return;
    var text = msg ? String(msg).trim() : "";
    elErr.textContent = text;
    elErr.hidden = !text;
  }

  function showGateStep(stepName) {
    if (gateStepLoading) gateStepLoading.hidden = stepName !== "loading";
    if (gateStepServer) gateStepServer.hidden = stepName !== "server";
    if (gateStepAuth) gateStepAuth.hidden = stepName !== "auth";
    if (gateStepPasswordChange) gateStepPasswordChange.hidden = stepName !== "cp";

    if (stepName === "loading") {
      if (gateTitle) gateTitle.textContent = "正在启动 RabbitControl";
      if (gateSubtitle) gateSubtitle.textContent = "正在检查服务器锚定与账号会话状态…";
    } else if (stepName === "server") {
      if (gateTitle) gateTitle.textContent = "连接 RabbitControl 服务器";
      if (gateSubtitle) gateSubtitle.textContent = "首次使用请填写服务器地址并验证 CA 指纹";
    } else if (stepName === "auth") {
      if (gateTitle) gateTitle.textContent = "登录 RabbitControl";
      if (gateSubtitle) gateSubtitle.textContent = "登录后将自动签发 mTLS 设备证书并同步您的设备";
    } else if (stepName === "cp") {
      if (gateTitle) gateTitle.textContent = "首次登录修改密码";
      if (gateSubtitle) gateSubtitle.textContent = "请为您的账号设置新密码以完成安全激活";
    }
  }

  function setAuthMode(mode) {
    var isReg = mode === "register";
    if (gateTabLogin) {
      gateTabLogin.classList.toggle("is-active", !isReg);
      gateTabLogin.setAttribute("aria-selected", !isReg ? "true" : "false");
    }
    if (gateTabRegister) {
      gateTabRegister.classList.toggle("is-active", isReg);
      gateTabRegister.setAttribute("aria-selected", isReg ? "true" : "false");
    }
    if (gateLoginForm) gateLoginForm.hidden = isReg;
    if (gateRegForm) gateRegForm.hidden = !isReg;
    setGateError(gateLoginError, "");
    setGateError(gateRegError, "");
  }

  function syncOverlayAccountFromGate(snap) {
    if (!Overlay || typeof Overlay.setAccount !== "function") return;
    if (!snap || !snap.user) {
      Overlay.setAccount(null);
      return;
    }
    Overlay.setAccount({
      id: snap.user.id,
      email: snap.user.email,
      displayName: snap.user.displayName || snap.user.email,
      role: snap.user.role,
      certIssued: Boolean(snap.certStatus && snap.certStatus.issued),
      certSerialHex: snap.certStatus && snap.certStatus.serialHex,
      certExpiresAtUnixSec: snap.certStatus && snap.certStatus.expiresAtUnixSec
    });
  }

  function syncRegistrationVisibility(allowReg) {
    var allowed = allowReg !== false;
    if (gateTabRegister) gateTabRegister.hidden = !allowed;
    if (gateRegDisabledNotice) gateRegDisabledNotice.hidden = allowed;
    if (!allowed) setAuthMode("login");
  }

  function applyGateStatus(snap) {
    if (!IS_TAURI || !snap) return;
    state.gateStatus = snap;
    syncOverlayAccountFromGate(snap);

    if (snap.gate === "authenticated") {
      gateServerViewOverride = false;
      if (gateOverlay) gateOverlay.hidden = true;
      if (!gateAuthedInitialized) {
        gateAuthedInitialized = true;
        refreshHostStatus();
        if (typeof Bridge.getServerConfig === "function") {
          Bridge.getServerConfig().then(applyServerStatus).catch(function () {});
        }
        if (typeof Bridge.getDeviceSnapshots === "function") {
          Bridge.getDeviceSnapshots().then(applyDeviceSnapshots).catch(function () {});
        }
        if (typeof Bridge.getActiveSessionInfo === "function") {
          Bridge.getActiveSessionInfo().then(applyActiveSessionState).catch(function () {});
        }
      }
      refreshMyDevices();
      checkAndRenderPermBanner();
      return;
    }

    gateAuthedInitialized = false;
    state.myDevices = [];
    paintSidebarRows();
    if (permBanner) permBanner.hidden = true;
    if (gateOverlay) gateOverlay.hidden = false;

    if (snap.gate === "unconfigured" || gateServerViewOverride) {
      if (gateServerHost && snap.serverHost && !gateServerHost.value) {
        gateServerHost.value = snap.serverHost;
      }
      if (gateApiPort && snap.apiPort) gateApiPort.value = String(snap.apiPort);
      if (gateSignalPort && snap.signalPort) gateSignalPort.value = String(snap.signalPort);
      if (gateRelayPort && snap.relayPort) gateRelayPort.value = String(snap.relayPort);
      if (gateUseHttps && typeof snap.useHttps === "boolean") {
        gateUseHttps.checked = snap.useHttps;
      }
      if (gateBackToLoginBtn) {
        gateBackToLoginBtn.hidden = !snap.serverConfigured;
      }
      showGateStep("server");
      return;
    }

    if (snap.gate === "password_change_required") {
      setGateError(gateCpError, "");
      showGateStep("cp");
      return;
    }

    // Unauthenticated
    if (gateServerSummary) {
      var scheme = snap.useHttps ? "https://" : "http://";
      gateServerSummary.textContent = "服务器：" + scheme + (snap.serverHost || "--") + ":" + (snap.apiPort || 21118);
    }
    syncRegistrationVisibility(snap.allowRegistration);
    showGateStep("auth");
    if (typeof Bridge.fetchAuthConfig === "function") {
      Bridge.fetchAuthConfig()
        .then(function (cfg) {
          if (cfg && typeof cfg.allowRegistration === "boolean") {
            syncRegistrationVisibility(cfg.allowRegistration);
          }
        })
        .catch(function () {});
    }
  }

  function refreshGateStatus() {
    if (!IS_TAURI || typeof Bridge.getGateStatus !== "function") {
      return Promise.resolve();
    }
    return Bridge.getGateStatus()
      .then(applyGateStatus)
      .catch(function () {});
  }

  function wireGateOverlay() {
    if (!IS_TAURI || !gateOverlay) return;

    Array.prototype.forEach.call(
      gateOverlay.querySelectorAll("[data-pw-toggle]"),
      function (btn) {
        btn.addEventListener("click", function () {
          var targetId = btn.getAttribute("data-pw-toggle");
          var input = targetId ? document.getElementById(targetId) : null;
          if (!input) return;
          var nextShow = input.type === "password";
          input.type = nextShow ? "text" : "password";
          var use = btn.querySelector("use");
          if (use) {
            var href = nextShow ? "#i-eye" : "#i-eye-off";
            use.setAttribute("href", href);
            use.setAttributeNS("http://www.w3.org/1999/xlink", "xlink:href", href);
          }
          btn.setAttribute("aria-label", nextShow ? "隐藏密码" : "显示密码");
        });
      }
    );

    if (gateAdvToggle && gateAdvPanel) {
      gateAdvToggle.addEventListener("click", function () {
        var nextHidden = !gateAdvPanel.hidden;
        gateAdvPanel.hidden = nextHidden;
        gateAdvToggle.setAttribute("aria-expanded", nextHidden ? "false" : "true");
      });
    }

    function invalidateProbe() {
      gateLastProbe = null;
      if (gateProbeResult) gateProbeResult.hidden = true;
      if (gateConfirmServerBtn) gateConfirmServerBtn.disabled = true;
      setGateError(gateServerError, "");
    }

    [gateServerHost, gateApiPort, gateSignalPort, gateRelayPort, gateUseHttps].forEach(function (inp) {
      if (!inp) return;
      inp.addEventListener("input", invalidateProbe);
      inp.addEventListener("change", invalidateProbe);
    });

    function readGateServerForm() {
      var host = gateServerHost ? gateServerHost.value.trim() : "";
      if (!host) {
        setGateError(gateServerError, "请输入服务器域名或 IP 地址");
        if (gateServerHost) gateServerHost.focus();
        return null;
      }
      var apiPort = Number(gateApiPort ? gateApiPort.value.trim() : "21118") || 21118;
      var signalPort = Number(gateSignalPort ? gateSignalPort.value.trim() : "21116") || 21116;
      var relayPort = Number(gateRelayPort ? gateRelayPort.value.trim() : "21117") || 21117;
      if (
        !Number.isInteger(apiPort) || apiPort < 1 || apiPort > 65535 ||
        !Number.isInteger(signalPort) || signalPort < 1 || signalPort > 65535 ||
        !Number.isInteger(relayPort) || relayPort < 1 || relayPort > 65535
      ) {
        setGateError(gateServerError, "端口必须为 1–65535 的整数");
        return null;
      }
      return {
        serverHost: host,
        apiPort: apiPort,
        signalPort: signalPort,
        relayPort: relayPort,
        useHttps: Boolean(gateUseHttps && gateUseHttps.checked)
      };
    }

    if (gateProbeBtn) {
      gateProbeBtn.addEventListener("click", function () {
        var form = readGateServerForm();
        if (!form) return;
        setGateError(gateServerError, "");
        gateProbeBtn.disabled = true;
        gateProbeBtn.textContent = "检测中…";
        Bridge.probeAndSaveServer({
          serverHost: form.serverHost,
          apiPort: form.apiPort,
          signalPort: form.signalPort,
          relayPort: form.relayPort,
          useHttps: form.useHttps,
          save: false
        })
          .then(function (res) {
            gateLastProbe = res;
            if (gateProbeResult) gateProbeResult.hidden = false;
            if (gateProbeBadge) gateProbeBadge.textContent = res.status === "ok" ? "连接正常" : res.status;
            if (gateProbeMeta) {
              gateProbeMeta.textContent =
                (res.version ? "v" + res.version + " · " : "") + res.latencyMs + " ms";
            }
            if (gateCaFingerprint) {
              gateCaFingerprint.textContent =
                res.caFingerprintSha256 || res.tlsLeafFingerprintSha256 || "未启用 TLS（明文 HTTP）";
            }
            if (gateProbeReg) {
              gateProbeReg.textContent = res.allowRegistration
                ? "注册策略：开放自助注册"
                : "注册策略：管理员已关闭自助注册";
            }
            if (gateConfirmServerBtn) gateConfirmServerBtn.disabled = false;
          })
          .catch(function (err) {
            invalidateProbe();
            setGateError(gateServerError, String(err || "连接服务器失败"));
          })
          .then(function () {
            gateProbeBtn.disabled = false;
            gateProbeBtn.textContent = "检测连接";
          });
      });
    }

    if (gateConfirmServerBtn) {
      gateConfirmServerBtn.addEventListener("click", function () {
        var form = readGateServerForm();
        if (!form) return;
        setGateError(gateServerError, "");
        gateConfirmServerBtn.disabled = true;
        gateConfirmServerBtn.textContent = "保存中…";
        Bridge.probeAndSaveServer({
          serverHost: form.serverHost,
          apiPort: form.apiPort,
          signalPort: form.signalPort,
          relayPort: form.relayPort,
          useHttps: form.useHttps,
          expectedCaFingerprintSha256:
            gateLastProbe && gateLastProbe.caFingerprintSha256
              ? gateLastProbe.caFingerprintSha256
              : null,
          save: true
        })
          .then(function (res) {
            gateServerViewOverride = false;
            if (res && res.gateStatus) {
              applyGateStatus(res.gateStatus);
            } else {
              refreshGateStatus();
            }
          })
          .catch(function (err) {
            setGateError(gateServerError, String(err || "保存服务器配置失败"));
            gateConfirmServerBtn.disabled = false;
          })
          .then(function () {
            gateConfirmServerBtn.textContent = "确认并进入登录";
          });
      });
    }

    if (gateBackToLoginBtn) {
      gateBackToLoginBtn.addEventListener("click", function () {
        gateServerViewOverride = false;
        if (state.gateStatus) applyGateStatus(state.gateStatus);
      });
    }

    if (gateChangeServerBtn) {
      gateChangeServerBtn.addEventListener("click", function () {
        gateServerViewOverride = true;
        if (state.gateStatus) applyGateStatus(state.gateStatus);
      });
    }

    if (gateTabLogin) {
      gateTabLogin.addEventListener("click", function () {
        setAuthMode("login");
      });
    }
    if (gateTabRegister) {
      gateTabRegister.addEventListener("click", function () {
        setAuthMode("register");
      });
    }

    if (gateLoginForm) {
      gateLoginForm.addEventListener("submit", function (e) {
        e.preventDefault();
        var email = gateLoginEmail ? gateLoginEmail.value.trim() : "";
        var password = gateLoginPassword ? gateLoginPassword.value : "";
        if (!email || !password) {
          setGateError(gateLoginError, "请输入邮箱与密码");
          return;
        }
        setGateError(gateLoginError, "");
        gateLoginBtn.disabled = true;
        gateLoginBtn.textContent = "登录中…";
        Bridge.authLogin({ email: email, password: password })
          .then(function (snap) {
            if (gateLoginPassword) gateLoginPassword.value = "";
            applyGateStatus(snap);
            if (snap && snap.gate === "authenticated") {
              toast("登录成功");
            }
          })
          .catch(function (err) {
            setGateError(gateLoginError, String(err || "登录失败"));
          })
          .then(function () {
            gateLoginBtn.disabled = false;
            gateLoginBtn.textContent = "登录";
          });
      });
    }

    if (gateRegForm) {
      gateRegForm.addEventListener("submit", function (e) {
        e.preventDefault();
        var email = gateRegEmail ? gateRegEmail.value.trim() : "";
        var displayName = gateRegName ? gateRegName.value.trim() : "";
        var password = gateRegPassword ? gateRegPassword.value : "";
        var confirm = gateRegConfirm ? gateRegConfirm.value : "";
        if (!email || !password) {
          setGateError(gateRegError, "请输入注册邮箱与密码");
          return;
        }
        if (password.length < 8) {
          setGateError(gateRegError, "密码长度至少需要 8 位字符");
          return;
        }
        if (password !== confirm) {
          setGateError(gateRegError, "两次输入的密码不一致");
          return;
        }
        setGateError(gateRegError, "");
        gateRegBtn.disabled = true;
        gateRegBtn.textContent = "注册中…";
        Bridge.authRegister({
          email: email,
          password: password,
          displayName: displayName || null
        })
          .then(function (snap) {
            if (gateRegPassword) gateRegPassword.value = "";
            if (gateRegConfirm) gateRegConfirm.value = "";
            applyGateStatus(snap);
            if (snap && snap.gate === "authenticated") {
              toast("注册并登录成功");
            }
          })
          .catch(function (err) {
            setGateError(gateRegError, String(err || "注册失败"));
          })
          .then(function () {
            gateRegBtn.disabled = false;
            gateRegBtn.textContent = "注册并登录";
          });
      });
    }

    if (gateCpForm) {
      gateCpForm.addEventListener("submit", function (e) {
        e.preventDefault();
        var cur = gateCpCurrent ? gateCpCurrent.value : "";
        var next = gateCpNew ? gateCpNew.value : "";
        var conf = gateCpConfirm ? gateCpConfirm.value : "";
        if (!cur || !next) {
          setGateError(gateCpError, "请填写当前密码与新密码");
          return;
        }
        if (next.length < 8) {
          setGateError(gateCpError, "新密码至少需要 8 位字符");
          return;
        }
        if (next !== conf) {
          setGateError(gateCpError, "两次输入的新密码不一致");
          return;
        }
        setGateError(gateCpError, "");
        gateCpSubmitBtn.disabled = true;
        gateCpSubmitBtn.textContent = "提交中…";
        Bridge.authChangePassword({ currentPassword: cur, newPassword: next })
          .then(function (snap) {
            if (gateCpCurrent) gateCpCurrent.value = "";
            if (gateCpNew) gateCpNew.value = "";
            if (gateCpConfirm) gateCpConfirm.value = "";
            applyGateStatus(snap);
            toast("密码修改成功，欢迎使用 RabbitControl");
          })
          .catch(function (err) {
            setGateError(gateCpError, String(err || "修改密码失败"));
          })
          .then(function () {
            gateCpSubmitBtn.disabled = false;
            gateCpSubmitBtn.textContent = "确认修改并进入";
          });
      });
    }

    if (gateCpLogoutBtn) {
      gateCpLogoutBtn.addEventListener("click", function () {
        Bridge.authLogout()
          .then(applyGateStatus)
          .catch(function (err) {
            toast("退出登录失败：" + err);
          });
      });
    }
  }

  /* ── 标题栏图标（§5.1：copy 多窗占位 / bell / account / menu）──── */
  function wireTitlebar() {
    $("#btnCopyWindow").addEventListener("click", function () {
      closeHistory();
      toast("功能开发中");
    });
    $("#btnBell").addEventListener("click", function () {
      closeHistory();
      toast("功能开发中");
    });

    /* O1 工具行出口由本页接管（§6 O1：行为同 P1 工具条） */
    if (Overlay) {
      Overlay.onOpenTool = function (tool) {
        openTool(tool);
      };
      Overlay.onOpenSettings = function () {
        openWindow("settings");
      };
      Overlay.onChangePassword = function () {
        if (!IS_TAURI) {
          toast("功能开发中");
          return;
        }
        openChangePasswordModal();
      };
      Overlay.onLogout = function () {
        if (!IS_TAURI || typeof Bridge.authLogout !== "function") {
          toast("已退出登录（mock）");
          return;
        }
        Bridge.authLogout()
          .then(function (snap) {
            applyGateStatus(snap);
            toast("已退出登录");
          })
          .catch(function (err) {
            toast("退出登录失败：" + err);
          });
      };
      Overlay.toastHost = document.body;
    }
  }

  /* ── 启动（§2 初始状态）─────────────────────────────────────── */
  /* 直达预览：index.html#p1 / #p2 / #p3 / #favorites（file:// 直接打开亦可），
   * 便于逐屏验收；不带 hash 时按 §2 初始状态落到 P1。 */
  function applyHashRoute() {
    var h = (window.location.hash || "").replace(/^#/, "");
    if (h === "p1" || h === "device") {
      go("p1");
      return true;
    }
    if (h === "p2" || h === "assist") {
      go("p2", "assist");
      return true;
    }
    if (h === "p3" || h === "all") {
      go("p3", "all");
      return true;
    }
    if (h === "favorites") {
      go("p3", "favorites");
      return true;
    }
    return false;
  }

  function boot() {
    /* Tauri 真窗口：原生标题栏（titleBarStyle: Overlay）已提供交通灯，
     * 打 .is-tauri 让自绘交通灯隐藏（overlay.css），浏览器预览时保留以便比对设计稿。 */
    if (window.__TAURI_INTERNALS__ || window.__TAURI__) {
      document.body.classList.add("is-tauri");
    }

    wireTitlebar();
    wireSidebarGroups();
    wireP1();
    wireP2();
    wireP3();
    wirePermBanner();
    wireGateOverlay();
    renderSidebar();

    if (IS_TAURI) {
      /* 真实桌面端：启动期间激活门禁加载态，待 getGateStatus() 回填后决定解锁主界面或显示门禁 1/2 */
      if (gateOverlay) {
        gateOverlay.hidden = false;
        showGateStep("loading");
      }
      if (pages.p2) pages.p2.classList.add("is-booting");
      state.allowAssist = false;
      if (p2Allow) p2Allow.checked = false;
      state.deviceId = null;
      state.sidebarKey = "all";
      go("p3", "all");
      renderP2DeviceId();
    } else {
      /* 浏览器静态预览：默认开启远程协助并选中 yii's MacBook Pro → P1（08 §2） */
      if (gateOverlay) gateOverlay.hidden = true;
      state.allowAssist = true;
      if (p2Allow) p2Allow.checked = true;
      state.deviceId = MOCK.defaultDeviceId;
      state.sidebarKey = "device:" + MOCK.defaultDeviceId;
      go("p1");
    }

    renderP2Body();
    renderP2Code();
    renderP2SessionBar();
    updateConnectState();

    if (IS_TAURI) {
      var finishBootSync = function () {
        if (!pages.p2 || !pages.p2.classList.contains("is-booting")) return;
        window.requestAnimationFrame(function () {
          if (pages.p2) pages.p2.classList.remove("is-booting");
        });
      };
      window.setTimeout(finishBootSync, 350);

      if (typeof Bridge.onGateStatus === "function") {
        Bridge.onGateStatus(applyGateStatus);
      }
      refreshGateStatus().then(finishBootSync, finishBootSync);

      Bridge.onHostUpdate(handleHostUpdate);
      if (typeof Bridge.onServerStatus === "function") {
        Bridge.onServerStatus(applyServerStatus);
      }
      if (typeof Bridge.onDeviceSnapshots === "function") {
        Bridge.onDeviceSnapshots(applyDeviceSnapshots);
      }
      if (typeof Bridge.onActiveSessionState === "function") {
        Bridge.onActiveSessionState(applyActiveSessionState);
      }
      if (typeof Bridge.onDeviceStatusChanged === "function") {
        Bridge.onDeviceStatusChanged(applyDeviceStatusChanged);
      }
      if (typeof Bridge.onAuthKicked === "function") {
        Bridge.onAuthKicked(function (payload) {
          var reason = (payload && payload.reason) || "请重新登录";
          toast("账号会话已强制注销：" + reason);
          refreshGateStatus();
        });
      }

      if (typeof Bridge.onDockConnectDevice === "function") {
        Bridge.onDockConnectDevice(function (payload) {
          if (!payload || !payload.target) return;
          var target = String(payload.target);
          var name = payload.name ? String(payload.name) : "";
          var alreadyControlling = isDeviceControlled({
            addr: target,
            deviceId: target,
            name: name
          });
          if (alreadyControlling) {
            focusSessionWindow();
            return;
          }
          connectToTarget(target, {
            watchOnly: false,
            promptPassword: Boolean(payload.requiresPassword),
            displayName: name || undefined
          });
        });
      }
    }

    applyHashRoute();
    window.addEventListener("hashchange", applyHashRoute);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", boot);
  } else {
    boot();
  }
})();
