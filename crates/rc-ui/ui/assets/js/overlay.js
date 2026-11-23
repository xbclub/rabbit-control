/* overlay.js —— O1 更多工具 / O2 右上下拉菜单 / O3 账户气泡
 *
 * 唯一依据：docs/reference/design/08-ui-design.md
 *   §6 O1 / O2 / O3 逐屏规格（尺寸、行高、图标尺寸、文案、Toast 出口）
 *   §7.3 Esc = 关闭当前浮层/模态/ConfirmDialog
 *   §7.4 Popover 点击锚点开合 / 点击外部关闭 / 同时只允许一个；
 *        Modal 遮罩点击不关闭，仅按钮/Esc；Toast 统一出口
 *   §3.6 Popover 距锚点垂直 8px；O2 右缘对齐锚点右缘；O3 水平居中于锚点
 *
 * 对外 API（挂 window.RCOverlay）：
 *   toast(text)                      —— 底部 Toast（1.8s，单条顶替）
 *   icon(name, size)                 —— Lucide 内联 SVG（离线，无 CDN）
 *   openO1() / openO2(anchor?) / openO3(anchor?) / closeAll()
 *   onOpenSettings = fn()            —— 宿主可覆盖「设置」跳转（默认新开 settings.html）
 *   onOpenTool = fn(tool)            —— 宿主可覆盖 O1 工具行跳转
 *
 * 自动接线：文档里带 data-rc-anchor="more|menu|account" 的元素会被绑定；
 * index.html 的 P1 工具条第五组请加 data-rc-anchor="more"，标题栏
 * menu / circle-user 图标请加 data-rc-anchor="menu" / "account"。
 *
 * 独立预览：任意加载本脚本的页面加 ?preview=o1|o2|o3（或 #o1|#o2|#o3）
 * 会挂出 §5.1 主窗口骨架并自动打开对应浮层（不注入产品 UI，仅预览台）。
 */
(function () {
  "use strict";

  /* ── Lucide 图标（lucide-static v1.77.0，ISC；内联路径，离线可用）── */
  var ICONS = {
    "folder-symlink":
      '<path d="M2 9.35V5a2 2 0 0 1 2-2h3.9a2 2 0 0 1 1.69.9l.81 1.2a2 2 0 0 0 1.67.9H20a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2v-3a2 2 0 0 1 2-2h7"/><path d="m8 16 3-3-3-3"/>',
    "square-play":
      '<rect x="3" y="3" width="18" height="18" rx="2"/><path d="M9 9.003a1 1 0 0 1 1.517-.859l4.997 2.997a1 1 0 0 1 0 1.718l-4.997 2.997A1 1 0 0 1 9 14.996z"/>',
    "square-terminal":
      '<path d="m7 11 2-2-2-2"/><path d="M11 13h4"/><rect width="18" height="18" x="3" y="3" rx="2" ry="2"/>',
    unplug:
      '<path d="m19 5 3-3"/><path d="m2 22 3-3"/><path d="M6.3 20.3a2.4 2.4 0 0 0 3.4 0L12 18l-6-6-2.3 2.3a2.4 2.4 0 0 0 0 3.4Z"/><path d="M7.5 13.5 10 11"/><path d="M10.5 16.5 13 14"/><path d="m12 6 6 6 2.3-2.3a2.4 2.4 0 0 0 0-3.4l-2.6-2.6a2.4 2.4 0 0 0-3.4 0Z"/>',
    "rotate-ccw":
      '<path d="M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"/><path d="M3 3v5h5"/>',
    "rotate-cw":
      '<path d="M21 12a9 9 0 1 1-9-9c2.52 0 4.93 1 6.74 2.74L21 8"/><path d="M21 3v5h-5"/>',
    power:
      '<path d="M12 2v10"/><path d="M18.4 6.6a9 9 0 1 1-12.77.04"/>',
    "chevron-right": '<path d="m9 18 6-6-6-6"/>',
    ellipsis:
      '<circle cx="12" cy="12" r="1"/><circle cx="19" cy="12" r="1"/><circle cx="5" cy="12" r="1"/>',
    lock:
      '<rect width="18" height="11" x="3" y="11" rx="2" ry="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/>',
    settings:
      '<path d="M9.671 4.136a2.34 2.34 0 0 1 4.659 0 2.34 2.34 0 0 0 3.319 1.915 2.34 2.34 0 0 1 2.33 4.033 2.34 2.34 0 0 0 0 3.831 2.34 2.34 0 0 1-2.33 4.033 2.34 2.34 0 0 0-3.319 1.915 2.34 2.34 0 0 1-4.659 0 2.34 2.34 0 0 0-3.32-1.915 2.34 2.34 0 0 1-2.33-4.033 2.34 2.34 0 0 0 0-3.831A2.34 2.34 0 0 1 6.35 6.051a2.34 2.34 0 0 0 3.319-1.915"/><circle cx="12" cy="12" r="3"/>',
    "circle-help":
      '<circle cx="12" cy="12" r="10"/><path d="M9 9.5a3 2.5 0 016 0c0 2-2 2-3 3.5"/><path d="M12 17h.01"/>',
    headphones:
      '<path d="M3 14h3a2 2 0 0 1 2 2v3a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-7a9 9 0 0 1 18 0v7a2 2 0 0 1-2 2h-1a2 2 0 0 1-2-2v-3a2 2 0 0 1 2-2h3"/>',
    info:
      '<circle cx="12" cy="12" r="10"/><path d="M12 16v-4"/><path d="M12 8h.01"/>',
    "circle-user":
      '<circle cx="12" cy="12" r="10"/><circle cx="12" cy="10" r="3"/><path d="M7 20.662V19a2 2 0 0 1 2-2h6a2 2 0 0 1 2 2v1.662"/>',
    user:
      '<path d="M19 21v-2a4 4 0 0 0-4-4H9a4 4 0 0 0-4 4v2"/><circle cx="12" cy="7" r="4"/>',
    keyboard:
      '<path d="M10 8h.01"/><path d="M12 12h.01"/><path d="M14 8h.01"/><path d="M16 12h.01"/><path d="M18 8h.01"/><path d="M6 8h.01"/><path d="M7 16h10"/><path d="M8 12h.01"/><rect width="20" height="16" x="2" y="4" rx="2"/>',
    globe:
      '<circle cx="12" cy="12" r="10"/><path d="M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20"/><path d="M2 12h20"/>',
    "log-out":
      '<path d="m16 17 5-5-5-5"/><path d="M21 12H9"/><path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4"/>',
    copy:
      '<rect width="14" height="14" x="8" y="8" rx="2" ry="2"/><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"/>',
    bell:
      '<path d="M10.268 21a2 2 0 0 0 3.464 0"/><path d="M3.262 15.326A1 1 0 0 0 4 17h16a1 1 0 0 0 .74-1.673C19.41 13.956 18 12.499 18 8A6 6 0 0 0 6 8c0 4.499-1.411 5.956-2.738 7.326"/>',
    menu: '<path d="M4 5h16"/><path d="M4 12h16"/><path d="M4 19h16"/>',
    "shield-check":
      '<path d="M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z"/><path d="m9 12 2 2 4-4"/>',
    "key-round":
      '<path d="M2.586 17.414A2 2 0 0 0 2 18.828V21a1 1 0 0 0 1 1h3a1 1 0 0 0 1-1v-1a1 1 0 0 1 1-1h1a1 1 0 0 0 1-1v-1a1 1 0 0 1 1-1h.172a2 2 0 0 0 1.414-.586l.814-.814a6.5 6.5 0 1 0-4-4z"/><circle cx="16.5" cy="7.5" r=".5" fill="currentColor"/>'
  };

  function icon(name, size) {
    var body = ICONS[name];
    if (!body) return "";
    var s = size || 16;
    return (
      '<svg class="rc-icon" width="' +
      s +
      '" height="' +
      s +
      '" viewBox="0 0 24 24" fill="none" stroke="currentColor" ' +
      'stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" ' +
      'aria-hidden="true" focusable="false">' +
      body +
      "</svg>"
    );
  }

  /* ── §6.0 mock 数据（逐字取自设计文档，禁止自编）──────────────── */
  var MOCK = {
    /* O1 ConfirmDialog 正文里的设备名 */
    device: "yii's MacBook Pro",
    /* O3 账户手机号 */
    phone: "151******21"
  };

  /* ── Toast（§4 / §7.4：单条、1.8s、距底 24 水平居中）──────────── */
  var toastEl = null;
  var toastTimer = 0;

  /* 宿主是否真的在渲染（display:none 的祖先会让 getClientRects 为空；
   * 不可用 offsetParent 判定——固定定位元素的 offsetParent 恒为 null） */
  function isRendered(el) {
    return !!(el && el.getClientRects().length);
  }

  function toast(text) {
    var host = api.toastHost || document.body;
    /* 宿主不可见（隐藏窗口 / 预览台）时回落到 body，避免 Toast 落在不可见层 */
    if (host !== document.body && !isRendered(host)) host = document.body;
    if (!toastEl) {
      toastEl = document.createElement("div");
      toastEl.className = "rc-toast";
      toastEl.setAttribute("role", "status");
      toastEl.setAttribute("aria-live", "polite");
    }
    if (toastEl.parentNode !== host) {
      host.appendChild(toastEl);
    }
    /* 独立窗口内 Toast 用绝对定位（距窗口底缘 24，非视口） */
    toastEl.classList.toggle("rc-toast--hosted", host !== document.body);
    toastEl.textContent = text;
    /* 重启过渡：同一时刻仅一条，新的顶替旧的 */
    toastEl.classList.remove("is-visible");
    void toastEl.offsetWidth;
    toastEl.classList.add("is-visible");
    window.clearTimeout(toastTimer);
    toastTimer = window.setTimeout(function () {
      toastEl.classList.remove("is-visible");
    }, 1800);
  }

  /* ── 状态 ─────────────────────────────────────────────────────── */
  var popover = null; /* { el, anchor } —— 同时只允许一个（§7.4） */
  var modalStack = []; /* [{ el, onEscape }] —— 栈顶响应 Esc */

  function onDocPointerDown(e) {
    if (!popover) return;
    if (popover.el.contains(e.target)) return;
    if (popover.anchor && popover.anchor.contains(e.target)) return;
    closePopover();
  }

  function onDocKeyDown(e) {
    if (e.key !== "Escape") return;
    if (modalStack.length) {
      var top = modalStack[modalStack.length - 1];
      e.preventDefault();
      e.stopPropagation();
      if (typeof top.onEscape === "function") top.onEscape();
      return;
    }
    if (popover) {
      e.preventDefault();
      e.stopPropagation();
      closePopover();
    }
  }

  function onViewportChange() {
    if (popover && popover.anchor) placePopover(popover.el, popover.anchor, popover.align);
  }

  document.addEventListener("pointerdown", onDocPointerDown, true);
  document.addEventListener("keydown", onDocKeyDown, true);
  window.addEventListener("resize", onViewportChange);
  window.addEventListener("scroll", onViewportChange, true);

  /* ── Popover 定位（§3.6：距锚点垂直 8px；右缘对齐 / 水平居中）── */
  function placePopover(el, anchor, align) {
    var r = anchor.getBoundingClientRect();
    var w = el.offsetWidth;
    var h = el.offsetHeight;
    var vw = document.documentElement.clientWidth || window.innerWidth;
    var vh = document.documentElement.clientHeight || window.innerHeight;
    var gap = 8;
    var margin = 8;

    var left = align === "center" ? r.left + r.width / 2 - w / 2 : r.right - w;
    left = Math.max(margin, Math.min(left, vw - w - margin));

    var top = r.bottom + gap;
    if (top + h > vh - margin && r.top - gap - h >= margin) {
      top = r.top - gap - h; /* 下方放不下时翻到锚点上方（文档未定义，取默认行为的镜像） */
    }
    top = Math.max(margin, top);

    /* 保留 1 位小数：锚点常落在半像素上，取整会把 8px 间距吃掉 0.5px */
    el.style.left = (Math.round(left * 10) / 10) + "px";
    el.style.top = (Math.round(top * 10) / 10) + "px";
    /* 顶部箭头指向锚点中心（O3/S2 规则；O2 无箭头） */
    var arrowX = Math.round(r.left + r.width / 2 - left);
    el.style.setProperty(
      "--rc-arrow-x",
      Math.max(16, Math.min(w - 16, arrowX)) + "px"
    );
  }

  function closePopover() {
    if (!popover) return;
    if (popover.anchor && popover.anchor.setAttribute) {
      popover.anchor.setAttribute("aria-expanded", "false");
    }
    if (popover.el.parentNode) popover.el.parentNode.removeChild(popover.el);
    popover = null;
  }

  function mountPopover(el, anchor, align) {
    closePopover();
    el.style.visibility = "hidden";
    document.body.appendChild(el);
    popover = { el: el, anchor: anchor || null, align: align || "right" };
    if (anchor) {
      placePopover(el, anchor, popover.align);
      if (anchor.setAttribute) anchor.setAttribute("aria-expanded", "true");
    } else {
      /* 无锚点（预览台兜底）：贴右上角，距边 16 */
      el.style.top = "60px";
      el.style.right = "16px";
    }
    el.style.visibility = "";
  }

  /* ── Modal 栈 ─────────────────────────────────────────────────── */
  function pushModal(layer, onEscape, focusEl) {
    layer.addEventListener("pointerdown", function (e) {
      /* 遮罩点击不关闭（§7.4 防误触）；仅吞掉点击避免穿透 */
      if (e.target === layer) e.preventDefault();
    });
    document.body.appendChild(layer);
    modalStack.push({ el: layer, onEscape: onEscape || null });
    if (focusEl) focusEl.focus();
    return layer;
  }

  function popModal(layer) {
    for (var i = modalStack.length - 1; i >= 0; i--) {
      if (modalStack[i].el === layer) {
        modalStack.splice(i, 1);
        break;
      }
    }
    if (layer.parentNode) layer.parentNode.removeChild(layer);
  }

  function closeModal() {
    while (modalStack.length) popModal(modalStack[modalStack.length - 1].el);
  }

  /* ── 宿主跳转钩子（接线点，默认行为可被 app.js 覆盖）─────────── */
  function openSettings() {
    if (typeof api.onOpenSettings === "function") {
      api.onOpenSettings();
      return;
    }
    window.open("settings.html", "rc-settings", "width=660,height=640");
  }

  function openTool(tool) {
    closeModal();
    if (typeof api.onOpenTool === "function") {
      api.onOpenTool(tool);
      return;
    }
    var ev = new CustomEvent("rc:open-tool", {
      detail: { tool: tool },
      bubbles: true,
      cancelable: true
    });
    var handled = !document.dispatchEvent(ev); /* preventDefault = 已处理 */
    if (!handled) toast("功能开发中");
  }

  /* ── O1 更多工具（§6 O1）─────────────────────────────────────── */
  function openO1() {
    if (document.querySelector(".rc-tools")) return; /* 已开 */
    closePopover();

    var layer = document.createElement("div");
    layer.className = "rc-layer";

    var tools = [
      { tool: "transfer", label: "文件传输", icon: "folder-symlink" },
      { tool: "watch", label: "观看模式", icon: "square-play" },
      { tool: "terminal", label: "终端", icon: "square-terminal" },
      { tool: "portmap", label: "端口映射", icon: "unplug" }
    ];
    var powers = [
      { action: "reboot", label: "重启", icon: "rotate-ccw" },
      { action: "shutdown", label: "关机", icon: "power" }
    ];

    var rows = tools
      .map(function (t) {
        return (
          '<button type="button" class="rc-tools__item rc-tools__item--tile" ' +
          'data-rc-tool="' + t.tool + '">' +
          '<span class="rc-tools__icon">' + icon(t.icon, 20) + "</span>" +
          '<span class="rc-tools__label">' + t.label + "</span>" +
          '<span class="rc-tools__chevron">' + icon("chevron-right", 16) + "</span>" +
          "</button>"
        );
      })
      .concat(
        powers.map(function (p) {
          return (
            '<button type="button" class="rc-tools__item rc-tools__item--plain" ' +
            'data-rc-power="' + p.action + '">' +
            '<span class="rc-tools__icon">' + icon(p.icon, 20) + "</span>" +
            '<span class="rc-tools__label">' + p.label + "</span>" +
            '<span class="rc-tools__chevron">' + icon("chevron-right", 16) + "</span>" +
            "</button>"
          );
        })
      )
      .join("");

    layer.innerHTML =
      '<div class="rc-tools" role="dialog" aria-modal="true" aria-labelledby="rc-o1-title">' +
      '<h2 class="rc-tools__title" id="rc-o1-title">更多工具</h2>' +
      '<div class="rc-tools__list">' + rows + "</div>" +
      '<div class="rc-tools__footer">' +
      '<button type="button" class="rc-btn rc-btn--secondary" data-action="reorder">调整顺序</button>' +
      '<button type="button" class="rc-btn rc-btn--secondary" data-action="close">关闭</button>' +
      "</div>" +
      "</div>";

    var card = layer.querySelector(".rc-tools");
    card.tabIndex = -1;

    card.addEventListener("click", function (e) {
      var toolBtn = e.target.closest("[data-rc-tool]");
      if (toolBtn) {
        openTool(toolBtn.getAttribute("data-rc-tool"));
        return;
      }
      var powerBtn = e.target.closest("[data-rc-power]");
      if (powerBtn) {
        openConfirm(powerBtn.getAttribute("data-rc-power"));
        return;
      }
      var actBtn = e.target.closest("[data-action]");
      if (!actBtn) return;
      var action = actBtn.getAttribute("data-action");
      if (action === "reorder") {
        toast("功能开发中");
        return;
      }
      if (action === "close") closeModal();
    });

    pushModal(layer, closeModal, card);
  }

  /* ConfirmDialog（§4 / §6 O1：重启 / 关机） */
  function openConfirm(action) {
    var isReboot = action === "reboot";
    var verb = isReboot ? "重启" : "关机";

    var layer = document.createElement("div");
    /* O1 已有 MASK，确认层不再二次压暗；单独调用时用遮罩 */
    layer.className = "rc-layer rc-layer--confirm" + (modalStack.length ? " rc-layer--over" : "");
    layer.innerHTML =
      '<div class="rc-confirm" role="alertdialog" aria-modal="true" ' +
      'aria-labelledby="rc-confirm-title" aria-describedby="rc-confirm-body">' +
      '<h2 class="rc-confirm__title" id="rc-confirm-title">' + verb + "设备</h2>" +
      '<p class="rc-confirm__body" id="rc-confirm-body">确认要' + verb +
      "「" + MOCK.device + "」吗？</p>" +
      '<div class="rc-confirm__footer">' +
      '<button type="button" class="rc-btn rc-btn--secondary" data-action="cancel">取消</button>' +
      '<button type="button" class="rc-btn rc-btn--danger" data-action="ok">确认</button>' +
      "</div>" +
      "</div>";

    var card = layer.querySelector(".rc-confirm");

    card.addEventListener("click", function (e) {
      var btn = e.target.closest("[data-action]");
      if (!btn) return;
      if (btn.getAttribute("data-action") === "ok") {
        popModal(layer);
        closeModal();
        toast("已发送" + verb + "指令");
      } else {
        popModal(layer);
      }
    });

    pushModal(
      layer,
      function () {
        popModal(layer);
      },
      card.querySelector('[data-action="ok"]')
    );
  }

  /* ── O2 右上下拉菜单（§6 O2：宽 200 / 行高 40 / SIZE_BODY / 左 20px 图标）── */
  function buildO2() {
    var el = document.createElement("div");
    el.className = "rc-popover rc-menu";
    el.setAttribute("role", "menu");
    el.setAttribute("aria-label", "应用菜单");

    var items = [
      { icon: "lock", label: "安全锁定", act: function () { toast("功能开发中"); } },
      { icon: "settings", label: "设置", act: function () { openSettings(); } },
      { icon: "circle-help", label: "帮助与反馈", act: function () { toast("功能开发中"); } },
      { icon: "headphones", label: "在线客服", act: function () { toast("功能开发中"); } },
      { icon: "info", label: "关于", act: function () { toast("功能开发中"); } },
      { sep: true },
      { icon: "rotate-cw", label: "检查更新", act: function () { toast("当前已是最新版本"); } }
    ];

    el.innerHTML = items
      .map(function (it) {
        if (it.sep) return '<div class="rc-menu__sep" role="separator"></div>';
        return (
          '<button type="button" class="rc-menu__item" role="menuitem">' +
          '<span class="rc-menu__icon">' + icon(it.icon, 16) + "</span>" +
          "<span>" + it.label + "</span>" +
          "</button>"
        );
      })
      .join("");

    /* 选中即关闭菜单（NSMenu 行为），再由动作本身给 Toast / 跳转 */
    var btns = el.querySelectorAll(".rc-menu__item");
    var idx = 0;
    items.forEach(function (it, i) {
      if (it.sep) return;
      var btn = btns[idx++];
      btn.addEventListener("click", function () {
        closePopover();
        it.act();
      });
    });

    /* 上下键在菜单内移动焦点（原生菜单观感） */
    el.addEventListener("keydown", function (e) {
      if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
      e.preventDefault();
      var list = Array.prototype.slice.call(btns);
      var at = list.indexOf(document.activeElement);
      var next = e.key === "ArrowDown" ? at + 1 : at - 1;
      if (next < 0) next = list.length - 1;
      if (next >= list.length) next = 0;
      list[next].focus();
    });

    return el;
  }

  function openO2(anchor) {
    var btn = anchor || document.querySelector('[data-rc-anchor="menu"]');
    if (popover && popover.el.classList.contains("rc-menu")) {
      closePopover();
      return;
    }
    var el = buildO2();
    el.tabIndex = -1;
    mountPopover(el, btn, "right");
    /* 焦点落在菜单容器（NSMenu 打开态不高亮首项；Tab / 方向键进入项） */
    el.focus({ preventScroll: true });
  }

  /* ── O3 账户气泡（§6 O3：宽 280+、顶部箭头朝上、毛玻璃）── */
  var accountState = null;

  function setAccount(info) {
    accountState = info && typeof info === "object" ? info : null;
  }

  function escapeHtml(str) {
    return String(str == null ? "" : str)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;");
  }

  function buildO3() {
    var el = document.createElement("div");
    el.className = "rc-popover rc-account";
    el.setAttribute("role", "dialog");
    el.setAttribute("aria-label", "账户");

    if (accountState && accountState.email) {
      var displayName = (accountState.displayName && accountState.displayName.trim()) || accountState.email;
      var certValid = Boolean(accountState.certIssued);
      var certTitle = certValid && accountState.certSerialHex
        ? "序列号: " + accountState.certSerialHex
        : "";
      el.innerHTML =
        '<div class="rc-account__arrow" aria-hidden="true"></div>' +
        '<button type="button" class="rc-btn rc-btn--ghost rc-account__logout" ' +
        'aria-label="退出登录" title="退出登录">' + icon("log-out", 16) + "</button>" +
        '<div class="rc-account__avatar">' + icon("user", 24) + "</div>" +
        '<div class="rc-account__name">' + escapeHtml(displayName) + "</div>" +
        '<div class="rc-account__email">' + escapeHtml(accountState.email) + "</div>" +
        '<div class="rc-account__cert ' + (certValid ? "rc-account__cert--ok" : "rc-account__cert--warn") + '"' +
        (certTitle ? ' title="' + escapeHtml(certTitle) + '"' : "") + ">" +
        icon("shield-check", 13) +
        "<span>" + (certValid ? "mTLS 设备证书有效" : "设备证书未签发") + "</span>" +
        "</div>" +
        '<div class="rc-account__actions">' +
        '<button type="button" class="rc-btn rc-btn--secondary rc-account__action-btn" data-account-act="password">' +
        icon("key-round", 14) + "<span>修改密码</span></button>" +
        '<button type="button" class="rc-btn rc-btn--secondary rc-account__action-btn rc-account__action-btn--danger" data-account-act="logout">' +
        icon("log-out", 14) + "<span>退出登录</span></button>" +
        "</div>";

      var handleLogout = function () {
        closePopover();
        if (typeof api.onLogout === "function") {
          api.onLogout();
        } else {
          toast("已退出登录");
        }
      };
      el.querySelector(".rc-account__logout").addEventListener("click", handleLogout);
      el.querySelector('[data-account-act="logout"]').addEventListener("click", handleLogout);
      el.querySelector('[data-account-act="password"]').addEventListener("click", function () {
        closePopover();
        if (typeof api.onChangePassword === "function") {
          api.onChangePassword();
        } else {
          toast("功能开发中");
        }
      });
      return el;
    }

    el.innerHTML =
      '<div class="rc-account__arrow" aria-hidden="true"></div>' +
      '<button type="button" class="rc-btn rc-btn--ghost rc-account__logout" ' +
      'aria-label="退出登录">' + icon("log-out", 16) + "</button>" +
      '<div class="rc-account__avatar">' + icon("user", 26) + "</div>" +
      '<div class="rc-account__phone">' + MOCK.phone + "</div>" +
      '<button type="button" class="rc-account__link">我的卡券</button>';

    el.querySelector(".rc-account__logout").addEventListener("click", function () {
      closePopover();
      if (typeof api.onLogout === "function") {
        api.onLogout();
      } else {
        toast("已退出登录（mock）");
      }
    });
    el.querySelector(".rc-account__link").addEventListener("click", function () {
      toast("功能开发中");
    });
    return el;
  }

  function openO3(anchor) {
    var btn = anchor || document.querySelector('[data-rc-anchor="account"]');
    if (popover && popover.el.classList.contains("rc-account")) {
      closePopover();
      return;
    }
    var el = buildO3();
    el.tabIndex = -1;
    mountPopover(el, btn, "center");
    el.focus({ preventScroll: true });
  }

  function closeAll() {
    closeModal();
    closePopover();
  }

  /* ── 自动接线：data-rc-anchor ─────────────────────────────────── */
  function wireAnchors(root) {
    var scope = root || document;
    var more = scope.querySelectorAll('[data-rc-anchor="more"]');
    var menu = scope.querySelectorAll('[data-rc-anchor="menu"]');
    var account = scope.querySelectorAll('[data-rc-anchor="account"]');
    Array.prototype.forEach.call(more, function (b) {
      if (b.__rcWired) return;
      b.__rcWired = true;
      b.setAttribute("aria-haspopup", "dialog");
      b.addEventListener("click", function (e) {
        e.preventDefault();
        openO1();
      });
    });
    Array.prototype.forEach.call(menu, function (b) {
      if (b.__rcWired) return;
      b.__rcWired = true;
      b.setAttribute("aria-haspopup", "menu");
      b.setAttribute("aria-expanded", "false");
      b.addEventListener("click", function (e) {
        e.preventDefault();
        openO2(b);
      });
    });
    Array.prototype.forEach.call(account, function (b) {
      if (b.__rcWired) return;
      b.__rcWired = true;
      b.setAttribute("aria-haspopup", "dialog");
      b.setAttribute("aria-expanded", "false");
      b.addEventListener("click", function (e) {
        e.preventDefault();
        openO3(b);
      });
    });
  }

  /* ── 独立预览台（?preview=o1|o2|o3）───────────────────────────
   * O1–O3 是主窗口内浮层；此处按 §5.1 搭最小标题栏骨架（真实锚点位置）
   * + §7.2 骨架条，使浮层定位与毛玻璃可单独验证。不属于产品 UI。 */
  function previewKind() {
    var q = new URLSearchParams(window.location.search).get("preview");
    if (q === "o1" || q === "o2" || q === "o3") return q;
    var h = (window.location.hash || "").replace("#", "");
    return h === "o1" || h === "o2" || h === "o3" ? h : null;
  }

  function mountPreview(kind) {
    document.body.classList.add("rc-preview-mode");

    var wrap = document.createElement("div");
    wrap.className = "rc-preview";
    wrap.innerHTML =
      '<div class="rc-preview__bar">' +
      '<div class="rc-traffic">' +
      '<span class="rc-traffic__dot rc-traffic__dot--close"></span>' +
      '<span class="rc-traffic__dot rc-traffic__dot--min"></span>' +
      '<span class="rc-traffic__dot rc-traffic__dot--zoom"></span>' +
      "</div>" +
      '<div class="rc-preview__actions">' +
      '<button type="button" class="rc-btn rc-btn--ghost" aria-label="复制设备 ID">' +
      icon("copy", 20) + "</button>" +
      '<button type="button" class="rc-btn rc-btn--ghost" aria-label="通知">' +
      icon("bell", 20) + "</button>" +
      '<button type="button" class="rc-btn rc-btn--ghost" data-rc-anchor="account" aria-label="账户">' +
      icon("circle-user", 20) + "</button>" +
      '<button type="button" class="rc-btn rc-btn--ghost" data-rc-anchor="menu" aria-label="菜单">' +
      icon("menu", 20) + "</button>" +
      "</div>" +
      "</div>" +
      '<div class="rc-preview__body">' +
      '<aside class="rc-preview__sidebar">' +
      '<div class="rc-preview__skeleton"></div>' +
      '<div class="rc-preview__skeleton"></div>' +
      '<div class="rc-preview__skeleton"></div>' +
      "</aside>" +
      '<main class="rc-preview__content">' +
      '<div class="rc-preview__skeleton rc-preview__skeleton--wide"></div>' +
      '<div class="rc-preview__skeleton rc-preview__skeleton--wide"></div>' +
      "</main>" +
      "</div>" +
      /* P1 工具条位（仅预览台）：第五组 ellipsis = O1 出口 */
      '<div class="rc-preview__toolbar">' +
      '<span class="rc-preview__toolhint">P1 工具条 ⋮（O1 出口占位）</span>' +
      '<button type="button" class="rc-btn rc-btn--ghost" data-rc-anchor="more" aria-label="更多工具">' +
      icon("ellipsis", 20) + "</button>" +
      "</div>";
    document.body.appendChild(wrap);
    wireAnchors(wrap);

    if (kind === "o1") openO1();
    if (kind === "o2") openO2(wrap.querySelector('[data-rc-anchor="menu"]'));
    if (kind === "o3") openO3(wrap.querySelector('[data-rc-anchor="account"]'));
  }

  /* ── 样式自注入兜底 ──────────────────────────────────────────────
   * 宿主页若只引了本脚本、没引 overlay.css，则按脚本自身 URL 相对定位
   * 补一条 link（ui/assets/js/overlay.js → ui/assets/css/pages/overlay.css）。
   * 宿主已显式引入时不重复。 */
  function ensureStylesheet() {
    if (document.querySelector('link[href*="overlay.css"]')) return;
    var script = document.currentScript;
    if (!script || !script.src) return;
    var href = script.src.replace(/\/js\/overlay\.js(\?.*)?$/, "/css/pages/overlay.css");
    if (href === script.src) return;
    var link = document.createElement("link");
    link.rel = "stylesheet";
    link.href = href;
    /* 注入是异步的：样式就绪后重排当前 popover，否则按未加样式的尺寸定位 */
    link.addEventListener("load", onViewportChange);
    document.head.appendChild(link);
  }

  ensureStylesheet();

  /* ── 公开 API ─────────────────────────────────────────────────── */
  var api = {
    toast: toast,
    icon: icon,
    openO1: openO1,
    openO2: openO2,
    openO3: openO3,
    closeAll: closeAll,
    wireAnchors: wireAnchors,
    setAccount: setAccount,
    mock: MOCK,
    onOpenSettings: null,
    onOpenTool: null,
    onLogout: null,
    onChangePassword: null
  };
  window.RCOverlay = api;

  function boot() {
    var kind = previewKind();
    if (!kind) {
      wireAnchors(document);
      return;
    }
    /* 宿主页已有真实锚点（index.html）→ 直接在真实标题栏上开浮层；
     * 没有锚点的页面（如 settings.html）→ 注入 §5.1 最小骨架供独立预览。 */
    var realMenu = document.querySelector('[data-rc-anchor="menu"]');
    if (realMenu) {
      wireAnchors(document);
      if (kind === "o1") {
        openO1();
      } else if (kind === "o2") {
        openO2(realMenu);
      } else {
        openO3(document.querySelector('[data-rc-anchor="account"]'));
      }
      return;
    }
    mountPreview(kind);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", boot);
  } else {
    boot();
  }
})();
