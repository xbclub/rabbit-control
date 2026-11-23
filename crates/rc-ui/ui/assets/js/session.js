/* session.js —— W3 终端窗口 + S1 会话窗口 / S2 安全状态 / S3 控制中心 / S4 HUD
 *
 * 唯一依据：docs/reference/design/08-ui-design.md
 *   §6 W3 终端：prompt 两枚 12px 灰描边标签 + ~（ACCENT）+ >（ONLINE）+ 10×18
 *      ACCENT 光标块（800ms 闪烁）；右上 TERMINAL_MUTED mono 14px 计时，
 *      注入时钟，从 00:00:00 起每秒 +1
 *   §6 S1：计时从 00:00:00 起（注入时钟）；观看模式变体 = 只读态 + 左上灰 Pill
 *   §6 S2：需关注判定（防窥=关闭 或 自动锁屏=关）；说明文字随状态切换
 *   §6 S3：控制中心 NativeMenu；显示屏分辨率（含「其他分辨率」二级菜单）、画质两单选组（默认 超清 14M / 144 帧）、
 *      声音/安全（观看模式/立即锁屏/退出时锁屏）/外设（macOS 组合键）/窗口/工具/更多；进入全屏幕；退出远控回主窗口 P1
 *   §6 S4：HUD 首行随计时同步，其余 7 行静态 mock
 *   §7.3 快捷键：⌥⌘N 切换下一个屏幕、F 进入/退出全屏幕、Esc 关闭当前浮层
 *   §7.4 浮层：点锚点开合、点外部关闭、同时只允许一个
 *   §8.6.2 时钟注入：本文件的时间一律由可注入的时钟源提供（一期逻辑时钟）
 *
 * 同一文件服务两个页面：
 *   terminal.html —— W3（计时 only）
 *   session.html  —— S1 + S2 + S3 + S4
 */
(function () {
  'use strict';

  /* ── §8.6.2 时钟注入 ─────────────────────────────────────────────
   * 时间只从 clock.nowMs() 取；后续接线 Rust 侧会话时间时替换此函数即可。
   * 暴露 window.RCSession.setClock(fn) 作为注入点（返回毫秒）。
   */
  var clock = {
    nowMs: function () {
      return Date.now();
    }
  };

  function pad2(n) {
    return n < 10 ? '0' + n : String(n);
  }

  /* HH:MM:SS（<24h；与 T21 契约 HudSnapshot.elapsed 同格式） */
  function formatHms(totalSeconds) {
    var s = Math.max(0, Math.floor(totalSeconds));
    var h = Math.floor(s / 3600);
    var m = Math.floor((s % 3600) / 60);
    var sec = s % 60;
    return pad2(h) + ':' + pad2(m) + ':' + pad2(sec);
  }

  /* 统一走时器：250ms 轮询 + 仅在秒变化时写 DOM（避免每帧 reflow） */
  function startTicker(onSecond) {
    var startMs = clock.nowMs();
    var lastShown = -1;

    function tick() {
      var elapsed = Math.floor((clock.nowMs() - startMs) / 1000);
      if (elapsed === lastShown) return;
      lastShown = elapsed;
      onSecond(elapsed, formatHms(elapsed));
    }

    tick();
    var timerId = window.setInterval(tick, 250);
    return {
      stop: function () {
        window.clearInterval(timerId);
      }
    };
  }

  /* ── Toast（§4 / §3.6：HUD_BG 底白字 13px、高 36、距底 24 居中、1.8s 单条）─ */
  var toastEl = null;
  var toastTimer = 0;

  function toast(text) {
    if (!toastEl) {
      toastEl = document.getElementById('session-toast');
      if (!toastEl) return;
    }
    toastEl.textContent = text;
    toastEl.classList.remove('is-visible');
    void toastEl.offsetWidth; /* 重启过渡：同一时刻仅一条，新的顶替旧的 */
    toastEl.classList.add('is-visible');
    window.clearTimeout(toastTimer);
    toastTimer = window.setTimeout(function () {
      toastEl.classList.remove('is-visible');
    }, 1800);
  }

  /* ════════════════════════════════════════════════════════════════
   * W3 终端（terminal.html）
   * ════════════════════════════════════════════════════════════════ */
  function initTerminal() {
    var timerEl = document.getElementById('terminal-timer');
    if (!timerEl) return;
    startTicker(function (elapsed, text) {
      timerEl.textContent = text;
    });
  }

  /* ════════════════════════════════════════════════════════════════
   * S1–S4 会话窗口（session.html）
   * ════════════════════════════════════════════════════════════════ */

  /* §6.0 mock（逐字取自设计文档，禁止自编）：
   *   S1 会话：设备=yii's MacBook Pro；屏幕 tab=显示屏1 (Built-in Retina Display)
   *   S4 HUD 静态 7 行：UDP LAN / 2 fps / 227.2 Kbps / 110 ms / 20 ms frm. / 0.0% loss / 2K
   * 计时器与 HUD 首行同源（从 00:00:00 起每秒 +1）。
   */
  var MOCK = {
    screenName: '显示屏1 (Built-in Retina Display)'
  };

  function initSession() {
    var win = document.getElementById('session-window');
    var stage = document.getElementById('session-stage');
    if (!win || !stage) return;

    /* Item #3：全局捕获阶段无条件拦截 contextmenu，彻底消除 WKWebView 原生「重新加载」菜单；
     * 不调用 stopPropagation，以确保 #screen-tab 自定义右键分辨率菜单正常触发。 */
    document.addEventListener(
      'contextmenu',
      function (e) {
        e.preventDefault();
      },
      true
    );

    /* 在舞台区提前阻断鼠标右键默认行为，防止 macOS WebKit 启动原生右键跟踪 */
    stage.addEventListener('mousedown', function (e) {
      if (e.button === 2) {
        e.preventDefault();
      }
    });
    stage.addEventListener('pointerdown', function (e) {
      if (e.button === 2) {
        e.preventDefault();
      }
    });

    var timerEl = document.getElementById('session-timer');
    var hudElapsedEl = document.getElementById('hud-elapsed');
    var hudEl = document.getElementById('session-hud');
    var shieldBtn = document.getElementById('btn-shield');
    var ctrlBtn = document.getElementById('btn-control-center');
    var screenTabEl = document.getElementById('screen-tab');
    var screenResMenuEl = document.getElementById('screen-res-menu');
    var screenResListEl = document.getElementById('screen-res-list');
    var tabFitWindowBtn = document.getElementById('btn-tab-fit-window');
    var securityEl = document.getElementById('security-popover');
    var menuEl = document.getElementById('control-menu');
    var screenEl = document.getElementById('screen-menu');
    var screenItem = document.getElementById('menu-screen');
    var screenMainResListEl = document.getElementById('screen-main-res-list');
    var screenMoreEl = document.getElementById('screen-more-menu');
    var screenMoreResListEl = document.getElementById('screen-more-res-list');
    var qualityEl = document.getElementById('quality-menu');
    var qualityItem = document.getElementById('menu-quality');
    var windowEl = document.getElementById('window-menu');
    var windowItem = document.getElementById('menu-window');
    var soundEl = document.getElementById('sound-menu');
    var soundItem = document.getElementById('menu-sound');
    var soundPlayBtn = document.getElementById('opt-sound-play');
    var securitySubEl = document.getElementById('security-menu');
    var securityItem = document.getElementById('menu-security');
    var watchOnlyMenuBtn = document.getElementById('opt-watch-only');
    var lockNowBtn = document.getElementById('btn-lock-remote-now');
    var autoLockExitBtn = document.getElementById('opt-auto-lock-exit');
    var mtlsSwitchEl = document.getElementById('opt-mtls');
    var secTransportModeEl = document.getElementById('sec-transport-mode');
    var secMtlsStatusEl = document.getElementById('sec-mtls-status');
    var secPeerFpEl = document.getElementById('sec-peer-fp');
    var linkSecurityState = {
      encryptConnection: true,
      transport: 'UDP LAN',
      peerFingerprint: ''
    };
    var peripheralsEl = document.getElementById('peripherals-menu');
    var peripheralsItem = document.getElementById('menu-peripherals');
    var fitWindowBtn = document.getElementById('btn-fit-window');
    var windowFullscreenBtn = document.getElementById('btn-window-fullscreen');
    var toolsEl = document.getElementById('tools-menu');
    var toolsItem = document.getElementById('menu-tools');
    var moreEl = document.getElementById('more-menu');
    var moreItem = document.getElementById('menu-more');
    var showHudBtn = document.getElementById('opt-show-hud');
    var relayEl = document.getElementById('opt-relay');
    var clipboardItem = document.getElementById('menu-clipboard');
    var clipboardStateEl = document.getElementById('menu-clipboard-state');
    var clipboardEnabled = true;

    function syncClipboardMenuUi() {
      if (clipboardItem) {
        clipboardItem.setAttribute('aria-checked', clipboardEnabled ? 'true' : 'false');
      }
      if (clipboardStateEl) {
        clipboardStateEl.textContent = clipboardEnabled ? '已开启' : '已关闭';
      }
    }

    var BITRATE_BPS = {
      auto: 14000000,
      '30m': 30000000,
      '14m': 14000000,
      '8m': 8000000
    };

    function syncBitrateMenuUi(presetOrBps) {
      if (!qualityEl || presetOrBps == null || presetOrBps === '') return;
      var bitrateOpts = Array.prototype.slice.call(
        qualityEl.querySelectorAll('.sn-submenu__item[data-group="bitrate"]')
      );
      if (!bitrateOpts.length) return;
      var activeOpt = null;
      if (typeof presetOrBps === 'string') {
        var key = presetOrBps.trim().toLowerCase();
        bitrateOpts.forEach(function (opt) {
          if (opt.getAttribute('data-value') === key) {
            activeOpt = opt;
          }
        });
      }
      if (!activeOpt) {
        var bps = Number(presetOrBps);
        if (!isFinite(bps) || bps <= 0) return;
        var closestDiff = Infinity;
        bitrateOpts.forEach(function (opt) {
          var val = opt.getAttribute('data-value');
          if (val === 'auto') return;
          var optBps = BITRATE_BPS[val];
          if (!isFinite(optBps)) return;
          var diff = Math.abs(bps - optBps);
          if (diff < closestDiff) {
            closestDiff = diff;
            activeOpt = opt;
          }
        });
      }
      if (!activeOpt) return;
      bitrateOpts.forEach(function (opt) {
        var checked = opt === activeOpt;
        opt.setAttribute('aria-checked', checked ? 'true' : 'false');
        var check = opt.querySelector('.sn-check');
        if (check) check.hidden = !checked;
      });
    }

    function syncFpsMenuUi(fps) {
      if (!qualityEl) return;
      var target = Number(fps);
      if (!isFinite(target) || target <= 0) return;
      var fpsOpts = Array.prototype.slice.call(
        qualityEl.querySelectorAll('.sn-submenu__item[data-group="fps"]')
      );
      if (!fpsOpts.length) return;
      var matchedOpt = null;
      var closestOpt = null;
      var closestDiff = Infinity;
      fpsOpts.forEach(function (opt) {
        var v = Number(opt.getAttribute('data-value'));
        if (!isFinite(v)) return;
        if (Math.round(target) === v) {
          matchedOpt = opt;
        }
        var diff = Math.abs(target - v);
        if (diff < closestDiff) {
          closestDiff = diff;
          closestOpt = opt;
        }
      });
      var activeOpt = matchedOpt || closestOpt;
      fpsOpts.forEach(function (opt) {
        var checked = opt === activeOpt;
        opt.setAttribute('aria-checked', checked ? 'true' : 'false');
        var check = opt.querySelector('.sn-check');
        if (check) check.hidden = !checked;
      });
    }

    /* 屏幕 tab 文案以 §6.0 为准（HTML 已内联，保持一致） */
    var screenNameEl = document.getElementById('screen-name');
    if (screenNameEl) {
      screenNameEl.textContent = MOCK.screenName;
      screenNameEl.parentNode.setAttribute('title', MOCK.screenName + ' — 右键切换分辨率');
    }

    var params = new URLSearchParams(window.location.search);

    /* 窗口标题 = 会话设备名（主窗口经 ?device= 传入；缺省取 §6.0 的 yii's MacBook Pro） */
    var deviceParam = params.get('device');
    if (deviceParam) document.title = deviceParam + ' — RabbitControl';

    /* 观看模式变体（§8.3 / U-M7：支持 URL 初始参数与 SessionUpdate 动态同步） */
    var hash = (window.location.hash || '').replace('#', '');
    var watchMode = params.get('mode') === 'watch' || params.get('watch') === '1' || hash === 'watch';
    var watchPill = document.getElementById('watch-pill');
    function setWatchMode(enabled) {
      watchMode = Boolean(enabled);
      if (watchMode) {
        win.setAttribute('data-watch', '');
        if (watchPill) watchPill.hidden = false;
      } else {
        win.removeAttribute('data-watch');
        if (watchPill) watchPill.hidden = true;
      }
      if (watchOnlyMenuBtn) {
        watchOnlyMenuBtn.setAttribute('aria-checked', watchMode ? 'true' : 'false');
        var wc = watchOnlyMenuBtn.querySelector('.sn-check');
        if (wc) wc.hidden = !watchMode;
      }
      if (lockNowBtn) {
        lockNowBtn.setAttribute('aria-disabled', watchMode ? 'true' : 'false');
      }
      if (peripheralsEl) {
        Array.prototype.forEach.call(
          peripheralsEl.querySelectorAll('[data-combo]'),
          function (btn) {
            btn.setAttribute('aria-disabled', watchMode ? 'true' : 'false');
          }
        );
      }
    }
    setWatchMode(watchMode);

    /* ── 计时器 + HUD 首行（同一注入时钟，§6 S1 / §6 S4）──────────── */
    var Bridge = window.RCBridge;
    var IS_TAURI = Boolean(Bridge && Bridge.isTauri);
    var stageHintEl = stage.querySelector('.sn-stage__hint');
    var hudRows = hudEl ? Array.prototype.slice.call(hudEl.querySelectorAll('span')) : [];
    var realSessionActive = false;
    var relaySwitching = false;
    var preferredResolution = null;
    var autoAppliedSavedRes = false;
    var hasDisplayConfig = false;
    var pendingUserResTarget = null;
    var prevDisplayWidth = 0;
    var prevDisplayHeight = 0;
    var seenNegotiated = false;

    function formatTransportBadge(raw) {
      var text = String(raw || '').trim();
      if (!text) return 'UDP LAN';
      if (text === 'TCP Relay' || text === 'Relay') return 'mTLS Relay';
      return text;
    }

    function formatFingerprintAbbr(fp) {
      var clean = String(fp || '').trim();
      if (!clean) return '--';
      if (clean.length <= 14) return clean;
      return clean.slice(0, 8) + '…' + clean.slice(-6);
    }

    function syncLinkSecurityUi() {
      var on = Boolean(linkSecurityState.encryptConnection);
      if (mtlsSwitchEl) mtlsSwitchEl.checked = on;
      if (secTransportModeEl) {
        secTransportModeEl.textContent = formatTransportBadge(linkSecurityState.transport);
      }
      if (secMtlsStatusEl) {
        secMtlsStatusEl.textContent = on
          ? 'mTLS 1.3 双向证书已验证'
          : '未开启端到端 mTLS（中转外层仍受保护）';
        secMtlsStatusEl.classList.toggle('is-off', !on);
      }
      if (secPeerFpEl) {
        secPeerFpEl.textContent = on
          ? '对端证书指纹：' + formatFingerprintAbbr(linkSecurityState.peerFingerprint)
          : '对端证书指纹：未验证';
      }
    }

    function syncRelaySwitchUi(transport) {
      if (typeof transport === 'string' && transport) {
        linkSecurityState.transport = transport;
        syncLinkSecurityUi();
      }
      if (relaySwitching || !relayEl) return;
      if (typeof transport !== 'string' || !transport) return;
      relayEl.checked = transport === 'TCP Relay';
    }

    function applyHudVisibility(show, persist) {
      var visible = Boolean(show);
      if (hudEl) {
        hudEl.hidden = !visible;
      }
      if (showHudBtn) {
        showHudBtn.setAttribute('aria-checked', visible ? 'true' : 'false');
        var check = showHudBtn.querySelector('.sn-check');
        if (check) check.hidden = !visible;
      }
      if (persist && IS_TAURI && typeof Bridge.saveStreamPrefs === 'function') {
        Bridge.saveStreamPrefs({ showHud: visible }).catch(function () {});
      }
    }
    var localTicker = startTicker(function (elapsed, text) {
      if (timerEl) timerEl.textContent = text;
      if (hudElapsedEl) hudElapsedEl.textContent = text;
    });

    function stopLocalTicker() {
      if (localTicker) {
        localTicker.stop();
        localTicker = null;
      }
    }

    function formatHudBitrate(kbps) {
      if (!isFinite(kbps) || kbps < 0) return '0.0 Kbps';
      if (kbps >= 1000) return (kbps / 1000).toFixed(1) + ' Mbps';
      return kbps.toFixed(1) + ' Kbps';
    }

    function applyHudSnapshot(snap) {
      if (!snap) return;
      if (snap.elapsed) {
        if (timerEl) timerEl.textContent = snap.elapsed;
        if (hudElapsedEl) hudElapsedEl.textContent = snap.elapsed;
      }
      if (snap.transport != null) {
        linkSecurityState.transport = String(snap.transport);
        syncLinkSecurityUi();
      }
      if (hudRows.length >= 8) {
        if (snap.transport != null) hudRows[1].textContent = String(snap.transport);
        hudRows[2].textContent = (isFinite(snap.fps) ? Math.round(snap.fps) : 0) + ' fps';
        hudRows[3].textContent = formatHudBitrate(snap.bitrate_kbps);
        hudRows[4].textContent =
          snap.rtt_ms != null && isFinite(snap.rtt_ms) ? snap.rtt_ms.toFixed(1) + ' ms' : '-- ms';
        hudRows[5].textContent =
          snap.frame_ms != null && isFinite(snap.frame_ms)
            ? snap.frame_ms.toFixed(1) + ' ms frm.'
            : '-- ms frm.';
        hudRows[6].textContent =
          (isFinite(snap.loss_pct) ? snap.loss_pct.toFixed(1) : '0.0') + '% loss';
        if (snap.resolution) hudRows[7].textContent = String(snap.resolution);
      }
    }

    /* ── 流参数与分辨率预设状态（Item #4 / #5）──────────────────── */
    var SVG_NS = 'http://www.w3.org/2000/svg';
    var DEFAULT_MOCK_PRESETS = [
      { width: 1512, height: 982, label: '1512 × 982', is_native: true },
      { width: 1386, height: 900, label: '1386 × 900', is_native: false },
      { width: 1110, height: 720, label: '1110 × 720', is_native: false },
      { width: 832, height: 540, label: '832 × 540', is_native: false }
    ];
    var streamState = {
      width: IS_TAURI ? 0 : 1512,
      height: IS_TAURI ? 0 : 982,
      fps: 144,
      nativeWidth: IS_TAURI ? 0 : 1512,
      nativeHeight: IS_TAURI ? 0 : 982,
      maxFps: 144,
      hasServerPresets: false,
      presets: DEFAULT_MOCK_PRESETS.slice()
    };

    function createSvgIcon(symbolId, size) {
      var svg = document.createElementNS(SVG_NS, 'svg');
      svg.setAttribute('class', 'sn-ico');
      svg.setAttribute('width', String(size));
      svg.setAttribute('height', String(size));
      svg.setAttribute('aria-hidden', 'true');
      var use = document.createElementNS(SVG_NS, 'use');
      use.setAttribute('href', '#' + symbolId);
      svg.appendChild(use);
      return svg;
    }

    function buildFallbackPresets(baseW, baseH) {
      var bw = Math.round(Number(baseW) || 0);
      var bh = Math.round(Number(baseH) || 0);
      if (!(bw > 0 && bh > 0)) return DEFAULT_MOCK_PRESETS.slice();
      var scales = [1, 0.85, 0.72, 0.55];
      var seen = Object.create(null);
      var list = [];
      scales.forEach(function (scale, idx) {
        var w = idx === 0 ? bw : Math.max(2, Math.round((bw * scale) / 2) * 2);
        var h = idx === 0 ? bh : Math.max(2, Math.round((bh * scale) / 2) * 2);
        if (idx > 0 && (w < 640 || h < 360)) return;
        var key = w + 'x' + h;
        if (seen[key]) return;
        seen[key] = true;
        list.push({
          width: w,
          height: h,
          label: w + ' × ' + h,
          is_native: idx === 0
        });
      });
      return list;
    }

    function formatPresetLabel(preset, w, h) {
      var raw = preset && preset.label ? String(preset.label).trim() : '';
      if (!raw) {
        raw = w + ' × ' + h;
      }
      raw = raw.replace(/^原生\s+/, '').replace(/\s*[（(]\s*原生\s*[)）]\s*$/, '');
      if (preset && preset.is_hidpi && raw.indexOf('(HiDPI)') === -1) {
        raw += ' (HiDPI)';
      }
      return raw || w + ' × ' + h;
    }

    function isPresetActive(preset) {
      if (!preset || !(streamState.width > 0 && streamState.height > 0)) return false;
      var w = Number(preset.width) || 0;
      var h = Number(preset.height) || 0;
      return Math.abs(streamState.width - w) <= 2 && Math.abs(streamState.height - h) <= 2;
    }

    function findMatchingPreset(presets, target) {
      if (!Array.isArray(presets) || !target) return null;
      var exact = null;
      var near = null;
      presets.forEach(function (p) {
        var pw = Number(p && p.width);
        var ph = Number(p && p.height);
        if (!(pw > 0 && ph > 0)) return;
        if (pw === target.width && ph === target.height && !exact) {
          exact = { width: pw, height: ph };
        } else if (
          !near &&
          Math.abs(pw - target.width) <= 2 &&
          Math.abs(ph - target.height) <= 2
        ) {
          near = { width: pw, height: ph };
        }
      });
      return exact || near;
    }

    function maybeAutoApplySavedResolution() {
      if (autoAppliedSavedRes || !preferredResolution || !hasDisplayConfig) return;
      autoAppliedSavedRes = true;
      var match = findMatchingPreset(streamState.presets, preferredResolution);
      if (match && (match.width !== streamState.width || match.height !== streamState.height)) {
        if (IS_TAURI && typeof Bridge.sessionCommand === 'function') {
          Bridge.sessionCommand({
            type: 'set_stream_params',
            width: match.width,
            height: match.height,
            fps: Number(streamState.fps || 144)
          }).catch(function () {});
        }
      }
    }

    function selectResolutionPreset(w, h) {
      var targetW = Number(w);
      var targetH = Number(h);
      streamState.width = targetW;
      streamState.height = targetH;
      renderAllResolutionLists();
      if (hudRows.length >= 8) {
        hudRows[7].textContent = targetW + 'x' + targetH;
      }
      preferredResolution = { width: targetW, height: targetH };
      closePopover();
      if (IS_TAURI) {
        if (typeof Bridge.saveStreamPrefs === 'function') {
          Bridge.saveStreamPrefs({
            resolutionWidth: targetW,
            resolutionHeight: targetH
          }).catch(function () {});
        }
        pendingUserResTarget = { width: targetW, height: targetH };
        Bridge.sessionCommand({
          type: 'set_stream_params',
          width: targetW,
          height: targetH,
          fps: Number(streamState.fps || 144)
        }).catch(function (err) {
          pendingUserResTarget = null;
          toast('切换分辨率失败：' + String(err || '未知错误'));
        });
        toast('正在切换分辨率至 ' + targetW + '×' + targetH + '…');
      } else {
        toast('已切换分辨率至 ' + targetW + '×' + targetH);
      }
    }

    function appendResolutionButton(container, preset, isNestedItem) {
      var w = Number(preset.width) || 0;
      var h = Number(preset.height) || 0;
      if (!(w > 0 && h > 0)) return;
      var isActive = isPresetActive(preset);

      var btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'sn-submenu__item';
      btn.setAttribute('role', 'menuitemradio');
      btn.setAttribute('aria-checked', isActive ? 'true' : 'false');
      btn.setAttribute('data-res-width', String(w));
      btn.setAttribute('data-res-height', String(h));

      var check = document.createElement('span');
      check.className = 'sn-check';
      check.hidden = !isActive;
      check.appendChild(createSvgIcon('i-check', 14));
      btn.appendChild(check);

      var label = document.createElement('span');
      label.className = 'sn-res-label';
      label.textContent = formatPresetLabel(preset, w, h);
      btn.appendChild(label);

      if (!isNestedItem) {
        btn.addEventListener('pointerenter', function () {
          if (typeof scheduleNestedClose === 'function') scheduleNestedClose();
        });
      }
      btn.addEventListener('click', function (e) {
        e.preventDefault();
        selectResolutionPreset(w, h);
      });

      container.appendChild(btn);
    }

    function renderMoreResolutionList(presets) {
      if (!screenMoreResListEl) return;
      while (screenMoreResListEl.firstChild) {
        screenMoreResListEl.removeChild(screenMoreResListEl.firstChild);
      }
      presets.forEach(function (preset) {
        appendResolutionButton(screenMoreResListEl, preset, true);
      });
    }

    function renderResolutionList(container) {
      if (!container) return;
      while (container.firstChild) {
        container.removeChild(container.firstChild);
      }
      var presets = Array.isArray(streamState.presets) ? streamState.presets : [];
      if (presets.length <= 6) {
        presets.forEach(function (preset) {
          appendResolutionButton(container, preset, false);
        });
        return;
      }

      var primary = presets.slice(0, 5);
      var rest = presets.slice(5);
      primary.forEach(function (preset) {
        appendResolutionButton(container, preset, false);
      });

      var moreActive = rest.some(isPresetActive);
      var moreBtn = document.createElement('button');
      moreBtn.type = 'button';
      moreBtn.className = 'sn-submenu__item';
      moreBtn.setAttribute('role', 'menuitem');
      moreBtn.setAttribute('aria-haspopup', 'menu');
      moreBtn.setAttribute('aria-expanded', 'false');

      var check = document.createElement('span');
      check.className = 'sn-check';
      check.hidden = !moreActive;
      check.appendChild(createSvgIcon('i-check', 14));
      moreBtn.appendChild(check);

      var label = document.createElement('span');
      label.className = 'sn-res-label';
      label.textContent = '其他分辨率';
      moreBtn.appendChild(label);

      var chevronWrap = document.createElement('span');
      chevronWrap.className = 'sn-menu__chevron';
      chevronWrap.appendChild(createSvgIcon('i-chevron-right', 14));
      moreBtn.appendChild(chevronWrap);

      var openMore = function () {
        if (typeof cancelNestedClose === 'function') cancelNestedClose();
        renderMoreResolutionList(rest);
        if (typeof openNestedMenu === 'function' && screenMoreEl) {
          openNestedMenu(moreBtn, screenMoreEl);
        }
      };
      moreBtn.addEventListener('pointerenter', openMore);
      moreBtn.addEventListener('focus', openMore);
      moreBtn.addEventListener('click', function (e) {
        e.preventDefault();
        openMore();
      });

      container.appendChild(moreBtn);
    }

    function renderAllResolutionLists() {
      renderResolutionList(screenMainResListEl);
      renderResolutionList(screenResListEl);
      if (
        screenMoreEl &&
        !screenMoreEl.hidden &&
        Array.isArray(streamState.presets) &&
        streamState.presets.length > 6
      ) {
        renderMoreResolutionList(streamState.presets.slice(5));
      }
    }

    function handleFitSessionWindow(e) {
      if (e) e.preventDefault();
      closePopover();
      if (IS_TAURI && typeof Bridge.fitSessionWindow === 'function') {
        Bridge.fitSessionWindow()
          .then(function () {
            toast('已适配窗口比例，消除黑边');
          })
          .catch(function (err) {
            toast('适配窗口比例失败：' + String(err || '未知错误'));
          });
        return;
      }
      toast('已适配窗口比例，消除黑边');
    }

    renderAllResolutionLists();

    /* ── S4 HUD 显隐（控制中心 → 更多 → 显示网络状态）─────────────── */
    if (showHudBtn) {
      showHudBtn.addEventListener('click', function (e) {
        e.preventDefault();
        var show = hudEl ? hudEl.hidden : true;
        applyHudVisibility(show, true);
        toast(show ? '已显示网络状态' : '已隐藏网络状态');
        closePopover();
      });
    }

    if (IS_TAURI && typeof Bridge.getAppPrefs === 'function') {
      Bridge.getAppPrefs()
        .then(function (prefs) {
          if (!prefs || typeof prefs !== 'object') return;
          applyHudVisibility(prefs.showHud !== false, false);
          applyLockOnExit(Boolean(prefs.lockOnExit), false, false);
          if (prefs.bitratePreset || prefs.bitrateBps) {
            syncBitrateMenuUi(prefs.bitratePreset || prefs.bitrateBps);
          }
          var savedFps = Number(prefs.fps);
          if (isFinite(savedFps) && savedFps > 0 && !seenNegotiated) {
            streamState.fps = savedFps;
            syncFpsMenuUi(savedFps);
          }
          var rw = Number(prefs.resolutionWidth);
          var rh = Number(prefs.resolutionHeight);
          if (isFinite(rw) && rw > 0 && isFinite(rh) && rh > 0) {
            preferredResolution = { width: rw, height: rh };
            maybeAutoApplySavedResolution();
          }
          if (!hasActiveSessionSecurity && typeof prefs.defaultEncryptConnection === 'boolean') {
            linkSecurityState.encryptConnection = prefs.defaultEncryptConnection;
            syncLinkSecurityUi();
          }
        })
        .catch(function () {});
    }
    var hasActiveSessionSecurity = false;
    if (IS_TAURI && typeof Bridge.getSecurityPrefs === 'function') {
      Bridge.getSecurityPrefs()
        .then(function (sec) {
          if (!sec || typeof sec !== 'object') return;
          if (!hasActiveSessionSecurity && typeof sec.defaultEncryptConnection === 'boolean') {
            linkSecurityState.encryptConnection = sec.defaultEncryptConnection;
            syncLinkSecurityUi();
          }
        })
        .catch(function () {});
    }
    if (IS_TAURI && typeof Bridge.getActiveSessionInfo === 'function') {
      Bridge.getActiveSessionInfo()
        .then(function (info) {
          if (!info || !info.active) return;
          hasActiveSessionSecurity = true;
          if (typeof info.mtlsActive === 'boolean' && info.connected) {
            linkSecurityState.encryptConnection = info.mtlsActive;
          } else if (typeof info.encryptConnection === 'boolean') {
            linkSecurityState.encryptConnection = info.encryptConnection;
          }
          if (info.peerCertFingerprint) {
            linkSecurityState.peerFingerprint = String(info.peerCertFingerprint);
          }
          if (info.transport) {
            linkSecurityState.transport = info.transport;
          }
          syncLinkSecurityUi();
        })
        .catch(function () {});
    }
    syncLinkSecurityUi();

    /* ════════════════════════════════════════════════════════════
     * 浮层管理（§7.4：点锚点开合、点外部关闭、同时只允许一个）
     * ════════════════════════════════════════════════════════════ */
    var open = null; /* { el, anchor, align } */

    function setArrow(el, anchor) {
      if (!el.classList.contains('sn-security')) return;
      var pr = el.getBoundingClientRect();
      var ar = anchor.getBoundingClientRect();
      var x = Math.round(ar.left + ar.width / 2 - pr.left);
      var lo = 16;
      var hi = Math.max(lo, pr.width - 16);
      el.style.setProperty('--sn-arrow-x', Math.max(lo, Math.min(hi, x)) + 'px');
    }

    /* §3.6：距锚点垂直 8px；S2 水平居中于锚点，S3 右缘对齐锚点右缘。
     * 边界 = 会话窗口（Tauri 内窗口即视口；浏览器预览时窗口居中，不能溢出窗口）。 */
    function place(el, anchor, align) {
      var r = anchor.getBoundingClientRect();
      var w = el.offsetWidth;
      var h = el.offsetHeight;
      var bounds = win.getBoundingClientRect();
      var gap = 8;
      var margin = 8;
      var loX = bounds.left + margin;
      var hiX = bounds.right - w - margin;
      var loY = bounds.top + margin;
      var hiY = bounds.bottom - h - margin;

      var left;
      if (align === 'center') {
        left = r.left + r.width / 2 - w / 2;
      } else if (align === 'left') {
        left = r.left;
      } else {
        left = r.right - w;
      }
      left = Math.max(loX, Math.min(left, Math.max(loX, hiX)));

      var top = r.bottom + gap;
      var above = false;
      if (top > hiY && r.top - gap - h >= loY) {
        top = r.top - gap - h; /* 文档未定义翻面；空间不足时的镜像兜底 */
        above = true;
      }
      top = Math.max(loY, Math.min(top, Math.max(loY, hiY)));

      el.style.left = Math.round(left) + 'px';
      el.style.top = Math.round(top) + 'px';
      el.classList.toggle('is-above', above);
      setArrow(el, anchor);
    }

    function closePopover() {
      if (!open) return;
      open.el.hidden = true;
      if (open.anchor && open.anchor.setAttribute) {
        open.anchor.setAttribute('aria-expanded', 'false');
      }
      closeSubmenu();
      open = null;
    }

    function openPopover(el, anchor, align) {
      if (open && open.el === el) {
        closePopover();
        return;
      }
      closePopover();
      el.hidden = false;
      open = { el: el, anchor: anchor, align: align };
      place(el, anchor, align);
      if (anchor && anchor.setAttribute) {
        anchor.setAttribute('aria-expanded', 'true');
      }
    }

    function onOutsidePointer(e) {
      if (!open) return;
      if (open.el.contains(e.target)) return;
      if (open.anchor && open.anchor.contains(e.target)) return;
      if (activeSubmenuEl && !activeSubmenuEl.hidden && activeSubmenuEl.contains(e.target)) return;
      if (activeNestedEl && !activeNestedEl.hidden && activeNestedEl.contains(e.target)) return;
      closePopover();
    }

    document.addEventListener('pointerdown', onOutsidePointer, true);

    /* ── S2 安全状态气泡 ───────────────────────────────────────── */
    var secPrivacy = document.getElementById('sec-privacy');
    var secMute = document.getElementById('sec-mute');
    var secLock = document.getElementById('sec-lock');
    var secMuteValue = document.getElementById('sec-mute-value');
    var secLockValue = document.getElementById('sec-lock-value');
    var warningPill = document.getElementById('security-warning');

    /* 「需关注」判定：防窥=关闭 或 自动锁屏=关 时显示，否则隐藏（§6 S2） */
    function syncSecurityState() {
      if (!secPrivacy || !secMute || !secLock) return;
      var privacyOff = secPrivacy.value === 'off';
      var lockOn = Boolean(secLock.checked);
      var mutedOn = Boolean(secMute.checked);

      if (warningPill) warningPill.hidden = !(privacyOff || !lockOn);

      /* 说明文字与 S3 控制中心子菜单双向同步 */
      if (secMuteValue) secMuteValue.textContent = mutedOn ? '已静音' : '已外放';
      if (secLockValue) secLockValue.textContent = lockOn ? '已开启' : '未开启';

      if (soundPlayBtn) {
        var playAudio = !mutedOn;
        soundPlayBtn.setAttribute('aria-checked', playAudio ? 'true' : 'false');
        var sc = soundPlayBtn.querySelector('.sn-check');
        if (sc) sc.hidden = !playAudio;
      }
      if (autoLockExitBtn) {
        autoLockExitBtn.setAttribute('aria-checked', lockOn ? 'true' : 'false');
        var lc = autoLockExitBtn.querySelector('.sn-check');
        if (lc) lc.hidden = !lockOn;
      }
    }

    function applyLockOnExit(enabled, persist, showToast) {
      var on = Boolean(enabled);
      if (secLock) secLock.checked = on;
      syncSecurityState();
      if (persist && IS_TAURI && typeof Bridge.saveStreamPrefs === 'function') {
        Bridge.saveStreamPrefs({ lockOnExit: on }).catch(function () {});
      }
      if (showToast) {
        toast(on ? '已开启退出时锁定被控端' : '已关闭退出时锁定被控端');
      }
    }

    function lockRemoteScreenNow() {
      if (watchMode) {
        toast('观看模式下无法锁定被控端');
        return false;
      }
      if (!IS_TAURI || !realSessionActive) {
        toast('锁屏指令将在远控连接后生效');
        return false;
      }
      Bridge.sessionCommand({ type: 'lock_remote_screen' }).catch(function () {});
      toast('已发送锁屏指令至被控端');
      return true;
    }

    function applyRemoteAudioMuted(muted, showToast) {
      var isMuted = Boolean(muted);
      if (secMute) secMute.checked = isMuted;
      syncSecurityState();
      if (IS_TAURI && typeof Bridge.sessionCommand === 'function') {
        Bridge.sessionCommand({
          type: 'set_audio_muted',
          muted: isMuted
        }).catch(function () {});
      }
      if (showToast) {
        toast(isMuted ? '已静音远端音频' : '已开启远端音频');
      }
    }

    function sendKeyCombo(modCodes, keyCode) {
      if (watchMode) {
        toast('观看模式下无法发送快捷键');
        return false;
      }
      if (!IS_TAURI || !realSessionActive) {
        toast('快捷键将在远控连接后生效');
        return false;
      }
      var mods = Array.isArray(modCodes) ? modCodes : [];
      var flags = { ctrl: false, shift: false, alt: false, meta: false };
      function setFlag(code, on) {
        if (code === 'ControlLeft' || code === 'ControlRight') flags.ctrl = on;
        if (code === 'ShiftLeft' || code === 'ShiftRight') flags.shift = on;
        if (code === 'AltLeft' || code === 'AltRight') flags.alt = on;
        if (code === 'MetaLeft' || code === 'MetaRight') flags.meta = on;
      }
      function sendKey(code, pressed) {
        Bridge.sendInputEvent({
          kind: 'key',
          code: code,
          pressed: pressed,
          ctrl: flags.ctrl,
          shift: flags.shift,
          alt: flags.alt,
          meta: flags.meta
        }).catch(function () {});
      }
      mods.forEach(function (mc) {
        setFlag(mc, true);
        sendKey(mc, true);
      });
      sendKey(keyCode, true);
      sendKey(keyCode, false);
      for (var i = mods.length - 1; i >= 0; i--) {
        setFlag(mods[i], false);
        sendKey(mods[i], false);
      }
      return true;
    }

    if (secPrivacy && secMute && secLock) {
      secPrivacy.addEventListener('change', syncSecurityState);
      secMute.addEventListener('change', function () {
        applyRemoteAudioMuted(Boolean(secMute.checked), IS_TAURI);
      });
      secLock.addEventListener('change', function () {
        applyLockOnExit(Boolean(secLock.checked), true, IS_TAURI);
      });
      syncSecurityState(); /* mock 默认：隐私屏保 / 静音关 / 自动锁屏关 → 显示需关注 */
    }

    if (shieldBtn) {
      shieldBtn.addEventListener('click', function (e) {
        e.preventDefault();
        openPopover(securityEl, shieldBtn, 'center');
      });
    }

    /* ── 显示屏选项卡右键分辨率菜单（Item #5）──────────────────── */
    if (screenTabEl && screenResMenuEl) {
      screenTabEl.addEventListener('contextmenu', function (e) {
        e.preventDefault();
        e.stopPropagation();
        openPopover(screenResMenuEl, screenTabEl, 'left');
      });
      screenTabEl.addEventListener('click', function () {
        if (open && open.el === screenResMenuEl) {
          closePopover();
        }
      });
    }

    if (fitWindowBtn) {
      fitWindowBtn.addEventListener('click', handleFitSessionWindow);
    }
    if (tabFitWindowBtn) {
      tabFitWindowBtn.addEventListener('click', handleFitSessionWindow);
    }
    if (windowFullscreenBtn) {
      windowFullscreenBtn.addEventListener('click', function (e) {
        e.preventDefault();
        closePopover();
        toggleFullscreen();
      });
    }

    /* ── S3 控制中心菜单 ───────────────────────────────────────── */
    var submenuAnchorItem = null;
    var activeSubmenuEl = null;
    var nestedAnchorItem = null;
    var activeNestedEl = null;
    var nestedCloseTimer = null;

    function cancelNestedClose() {
      if (nestedCloseTimer !== null) {
        window.clearTimeout(nestedCloseTimer);
        nestedCloseTimer = null;
      }
    }

    function closeNestedMenu() {
      cancelNestedClose();
      if (activeNestedEl) {
        activeNestedEl.hidden = true;
        activeNestedEl = null;
      }
      if (screenMoreEl) screenMoreEl.hidden = true;
      if (nestedAnchorItem) {
        nestedAnchorItem.classList.remove('is-open');
        nestedAnchorItem.setAttribute('aria-expanded', 'false');
      }
      nestedAnchorItem = null;
    }

    function scheduleNestedClose() {
      if (!activeNestedEl || activeNestedEl.hidden) return;
      cancelNestedClose();
      nestedCloseTimer = window.setTimeout(function () {
        nestedCloseTimer = null;
        closeNestedMenu();
      }, 180);
    }

    if (screenMoreEl) {
      screenMoreEl.addEventListener('pointerenter', cancelNestedClose);
    }

    function closeSubmenu() {
      closeNestedMenu();
      if (activeSubmenuEl) {
        activeSubmenuEl.hidden = true;
        activeSubmenuEl = null;
      }
      if (screenEl) screenEl.hidden = true;
      if (qualityEl) qualityEl.hidden = true;
      if (windowEl) windowEl.hidden = true;
      if (soundEl) soundEl.hidden = true;
      if (securitySubEl) securitySubEl.hidden = true;
      if (peripheralsEl) peripheralsEl.hidden = true;
      if (toolsEl) toolsEl.hidden = true;
      if (moreEl) moreEl.hidden = true;
      if (submenuAnchorItem) {
        submenuAnchorItem.classList.remove('is-open');
        submenuAnchorItem.setAttribute('aria-expanded', 'false');
      }
      submenuAnchorItem = null;
    }

    /* 子菜单贴父菜单侧边展开（§4 NativeMenu，支持二/三级菜单） */
    function placeFloatingMenu(item, subEl, parentEl) {
      var hostEl = parentEl || menuEl;
      if (!item || !subEl || !hostEl) return;
      var parentRect = hostEl.getBoundingClientRect();
      var itemRect = item.getBoundingClientRect();
      var bounds = win.getBoundingClientRect();
      var w = subEl.offsetWidth;
      var h = subEl.offsetHeight;
      var margin = 8;

      var left = parentRect.right - 2;
      if (left + w > bounds.right - margin) left = parentRect.left - w + 2;
      left = Math.max(bounds.left + margin, left);
      var top = Math.max(bounds.top + margin, Math.min(itemRect.top, bounds.bottom - h - margin));

      subEl.style.left = Math.round(left) + 'px';
      subEl.style.top = Math.round(top) + 'px';
    }

    function placeSubmenu(item, subEl) {
      placeFloatingMenu(item, subEl, menuEl);
    }

    function openNestedMenu(item, targetNested) {
      var nestEl = targetNested || screenMoreEl;
      if (!nestEl || !item) return;
      var parentEl = item.closest('.sn-popover') || activeSubmenuEl || (open && open.el);
      if (nestedAnchorItem === item && activeNestedEl === nestEl && !nestEl.hidden) {
        placeFloatingMenu(item, nestEl, parentEl);
        return;
      }
      closeNestedMenu();
      nestedAnchorItem = item;
      activeNestedEl = nestEl;
      nestEl.hidden = false;
      item.classList.add('is-open');
      item.setAttribute('aria-expanded', 'true');
      placeFloatingMenu(item, nestEl, parentEl);
    }

    function openSubmenu(item, targetSubmenu) {
      var subEl = targetSubmenu || qualityEl;
      if (!subEl) return;
      if (submenuAnchorItem === item && activeSubmenuEl === subEl && !subEl.hidden) {
        placeSubmenu(item, subEl);
        return;
      }
      closeSubmenu();
      submenuAnchorItem = item;
      activeSubmenuEl = subEl;
      subEl.hidden = false;
      item.classList.add('is-open');
      item.setAttribute('aria-expanded', 'true');
      placeSubmenu(item, subEl);
    }

    if (ctrlBtn) {
      ctrlBtn.addEventListener('click', function (e) {
        e.preventDefault();
        openPopover(menuEl, ctrlBtn, 'right');
      });
    }

    if (menuEl) {
      var items = Array.prototype.slice.call(menuEl.querySelectorAll('.sn-menu__item'));
      var submenuMap = {
        screen: screenEl,
        quality: qualityEl,
        window: windowEl,
        sound: soundEl,
        security: securitySubEl,
        peripherals: peripheralsEl,
        tools: toolsEl,
        more: moreEl
      };

      items.forEach(function (item) {
        var subKind = item.getAttribute('data-submenu');
        if (subKind && submenuMap[subKind]) {
          var subTarget = submenuMap[subKind];
          item.addEventListener('pointerenter', function () {
            openSubmenu(item, subTarget);
          });
          item.addEventListener('focus', function () {
            openSubmenu(item, subTarget);
          });
          item.addEventListener('click', function (e) {
            e.preventDefault();
            openSubmenu(item, subTarget);
          });
          return;
        }

        item.addEventListener('pointerenter', closeSubmenu);
        item.addEventListener('click', function (e) {
          e.preventDefault();
          var action = item.getAttribute('data-action');
          if (action === 'clipboard') {
            clipboardEnabled = !clipboardEnabled;
            syncClipboardMenuUi();
            if (IS_TAURI) {
              Bridge.sessionCommand({
                type: 'set_clipboard_enabled',
                enabled: clipboardEnabled
              }).catch(function () {});
            }
            toast(clipboardEnabled ? '已开启剪贴板同步' : '已关闭剪贴板同步');
            closePopover();
            return;
          }
          if (action === 'fullscreen') {
            closePopover();
            toggleFullscreen();
            return;
          }
          if (action === 'exit') {
            exitSession();
            return;
          }
          if (action === 'next-screen') {
            toast('被控端多显示器切换暂未支持');
            closePopover();
          }
        });
      });
    }

    if (soundPlayBtn) {
      soundPlayBtn.addEventListener('click', function (e) {
        e.preventDefault();
        var nextPlay = soundPlayBtn.getAttribute('aria-checked') !== 'true';
        applyRemoteAudioMuted(!nextPlay, true);
        closePopover();
      });
    }

    if (watchOnlyMenuBtn) {
      watchOnlyMenuBtn.addEventListener('click', function (e) {
        e.preventDefault();
        var nextWatch = !watchMode;
        if (IS_TAURI && typeof Bridge.setSessionWatchOnly === 'function') {
          Bridge.setSessionWatchOnly(nextWatch)
            .then(function () {
              setWatchMode(nextWatch);
              toast(nextWatch ? '已开启观看模式（禁止操作）' : '已恢复远程操作控制');
            })
            .catch(function (err) {
              toast('切换观看模式失败：' + String(err || '未知错误'));
            });
        } else {
          setWatchMode(nextWatch);
          toast(nextWatch ? '已开启观看模式（预览）' : '已恢复远程操作控制（预览）');
        }
        closePopover();
      });
    }

    if (lockNowBtn) {
      lockNowBtn.addEventListener('click', function (e) {
        e.preventDefault();
        closePopover();
        lockRemoteScreenNow();
      });
    }

    if (autoLockExitBtn && secLock) {
      autoLockExitBtn.addEventListener('click', function (e) {
        e.preventDefault();
        applyLockOnExit(!Boolean(secLock.checked), true, true);
        closePopover();
      });
    }

    if (mtlsSwitchEl) {
      mtlsSwitchEl.addEventListener('change', function () {
        var enabled = Boolean(mtlsSwitchEl.checked);
        var previewMsg = enabled
          ? '已开启 mTLS 双向加密握手'
          : '已关闭端到端 mTLS 加密（中转外层仍受 mTLS 保护）';
        if (!IS_TAURI || typeof Bridge.setSessionMtlsMode !== 'function') {
          linkSecurityState.encryptConnection = enabled;
          syncLinkSecurityUi();
          toast(previewMsg);
          return;
        }
        mtlsSwitchEl.disabled = true;
        Bridge.setSessionMtlsMode(enabled)
          .then(function () {
            linkSecurityState.encryptConnection = enabled;
            toast(enabled ? '正在重新协商 mTLS 加密连接…' : '正在切换为标准连接（中转外层仍受 mTLS 保护）…');
          })
          .catch(function (err) {
            mtlsSwitchEl.checked = !enabled;
            toast('切换 mTLS 模式失败：' + String(err || '未知错误'));
          })
          .then(function () {
            mtlsSwitchEl.disabled = false;
          });
      });
    }

    if (peripheralsEl) {
      var COMBO_MAP = {
        spotlight: { mods: ['MetaLeft'], key: 'Space', label: 'Spotlight (⌘Space)' },
        'switch-app': { mods: ['MetaLeft'], key: 'Tab', label: '切换应用 (⌘Tab)' },
        'force-quit': { mods: ['AltLeft', 'MetaLeft'], key: 'Escape', label: '强制退出应用 (⌥⌘Esc)' },
        screenshot: { mods: ['ShiftLeft', 'MetaLeft'], key: 'Digit3', label: '截屏 (⇧⌘3)' }
      };
      Array.prototype.forEach.call(
        peripheralsEl.querySelectorAll('[data-combo]'),
        function (btn) {
          btn.addEventListener('click', function (e) {
            e.preventDefault();
            var kind = btn.getAttribute('data-combo');
            closePopover();
            if (kind === 'lock-screen') {
              lockRemoteScreenNow();
              return;
            }
            var spec = COMBO_MAP[kind];
            if (!spec) return;
            if (sendKeyCombo(spec.mods, spec.key)) {
              toast('已发送快捷键：' + spec.label);
            }
          });
        }
      );
    }

    if (toolsEl) {
      var toolBtns = Array.prototype.slice.call(toolsEl.querySelectorAll('[data-tool]'));
      toolBtns.forEach(function (btn) {
        btn.addEventListener('click', function (e) {
          e.preventDefault();
          var kind = btn.getAttribute('data-tool');
          closePopover();
          if (IS_TAURI && typeof Bridge.openToolWindow === 'function') {
            Bridge.openToolWindow(kind).catch(function (err) {
              toast('打开工具窗口失败：' + String(err || '未知错误'));
            });
            return;
          }
          if (kind === 'transfer' || kind === 'terminal' || kind === 'portmap') {
            window.open(kind + '.html', '_blank', 'noopener');
          }
        });
      });
    }

    if (qualityEl) {
      var options = Array.prototype.slice.call(qualityEl.querySelectorAll('.sn-submenu__item'));
      options.forEach(function (opt) {
        opt.addEventListener('click', function () {
          var group = opt.getAttribute('data-group');
          var val = opt.getAttribute('data-value');
          options.forEach(function (other) {
            if (other.getAttribute('data-group') !== group) return;
            var checked = other === opt;
            other.setAttribute('aria-checked', checked ? 'true' : 'false');
            var check = other.querySelector('.sn-check');
            if (check) check.hidden = !checked;
          });
          if (group === 'fps') {
            var fpsNum = Number(val);
            if (isFinite(fpsNum) && fpsNum > 0) {
              streamState.fps = fpsNum;
            }
            if (IS_TAURI) {
              if (typeof Bridge.saveStreamPrefs === 'function' && isFinite(fpsNum) && fpsNum > 0) {
                Bridge.saveStreamPrefs({ fps: fpsNum }).catch(function () {});
              }
              Bridge.sessionCommand({
                type: 'set_stream_params',
                width: Number(streamState.width || 0),
                height: Number(streamState.height || 0),
                fps: Number(val)
              }).catch(function (err) {
                toast('切换帧率失败：' + String(err || '未知错误'));
              });
            }
            toast('已切换目标帧率至 ' + val + ' fps');
            return;
          }
          if (IS_TAURI && group === 'bitrate' && BITRATE_BPS[val]) {
            if (typeof Bridge.saveStreamPrefs === 'function') {
              Bridge.saveStreamPrefs({
                bitratePreset: val,
                bitrateBps: BITRATE_BPS[val]
              }).catch(function () {});
            }
            Bridge.sessionCommand({ type: 'set_bitrate', bps: BITRATE_BPS[val] }).catch(
              function () {}
            );
            toast('已切换码率');
            return;
          }
          /* 浏览器预览码率切换：仅改勾选态 + Toast（§6 S3 / T21 U3） */
          toast('画质将在下次连接生效');
        });
      });

      /* 安全中转 Switch 为独立开关（默认关），不属单选组 */
      if (relayEl) {
        relayEl.addEventListener('change', function () {
          if (!IS_TAURI) {
            toast(relayEl.checked ? '已开启安全中转连接（预览）' : '已关闭安全中转连接（预览）');
            relayEl.blur();
            return;
          }
          var desired = Boolean(relayEl.checked);
          if (typeof Bridge.setSessionRelayMode !== 'function') {
            relayEl.checked = !desired;
            toast('当前版本不支持切换安全中转');
            relayEl.blur();
            return;
          }
          relaySwitching = true;
          relayEl.disabled = true;
          toast(desired ? '正在切换至安全中转连接…' : '正在切换至直连模式…');
          function finishRelaySwitch() {
            relaySwitching = false;
            relayEl.disabled = false;
            relayEl.blur();
          }
          Bridge.setSessionRelayMode(desired)
            .then(function (label) {
              var modeLabel = label || (desired ? 'TCP Relay' : '直连');
              if (hudRows.length >= 2 && label) {
                hudRows[1].textContent = String(label);
              }
              toast('已切换传输模式：' + modeLabel);
              finishRelaySwitch();
            })
            .catch(function (err) {
              relayEl.checked = !desired;
              toast(String(err || '切换安全中转失败'));
              finishRelaySwitch();
            });
        });
      }
    }

    /* ── 全屏（§7.3 F / §6 S3「进入全屏幕」）───────────────────── */
    function fullscreenElement() {
      return document.fullscreenElement || document.webkitFullscreenElement || null;
    }

    function syncFullscreenLabels() {
      var isFs = Boolean(fullscreenElement() || win.classList.contains('is-fullscreen'));
      var text = isFs ? '退出全屏幕' : '进入全屏幕';
      var labels = document.querySelectorAll('.sn-fullscreen-label');
      Array.prototype.forEach.call(labels, function (el) {
        el.textContent = text;
      });
    }

    document.addEventListener('fullscreenchange', syncFullscreenLabels);
    document.addEventListener('webkitfullscreenchange', syncFullscreenLabels);

    function enterFallback() {
      /* 预览环境（file:// 无全屏权限）下用等价视觉态兜底；Tauri 内走 Fullscreen API */
      win.classList.add('is-fullscreen');
      syncFullscreenLabels();
    }

    function exitFallback() {
      win.classList.remove('is-fullscreen');
      syncFullscreenLabels();
    }

    function toggleFullscreen() {
      var doc = document;
      if (fullscreenElement()) {
        var exit = doc.exitFullscreen || doc.webkitExitFullscreen;
        if (exit) {
          var ep = exit.call(doc);
          if (ep && typeof ep.catch === 'function') ep.catch(function () {});
        }
        return;
      }
      if (win.classList.contains('is-fullscreen')) {
        exitFallback();
        return;
      }
      var request = win.requestFullscreen || win.webkitRequestFullscreen;
      if (request) {
        var p = request.call(win);
        if (p && typeof p.catch === 'function') {
          p.catch(enterFallback);
        }
        return;
      }
      enterFallback();
    }

    /* ── 退出远控（§6 S3：断开并销毁 S1 窗口回到主窗口 P1）────────── */
    function closeSessionWindow(lockRemote) {
      var inTauri = !!(window.__TAURI_INTERNALS__ || window.__TAURI__);
      if (IS_TAURI && typeof Bridge.closeSessionWindow === 'function') {
        Bridge.closeSessionWindow(lockRemote).catch(function () {
          try {
            window.close();
          } catch (err) {
            /* 忽略 */
          }
        });
        return;
      }
      try {
        window.close();
      } catch (err) {
        /* 忽略：落到下方的回主窗口兜底 */
      }
      if (inTauri) return;

      /* 浏览器预览兜底：窗口没被关闭时跳到主窗口页 */
      window.setTimeout(function () {
        if (!window.closed) window.location.assign('index.html');
      }, 80);
    }

    function exitSession() {
      closePopover();
      closeSessionWindow(Boolean(secLock && secLock.checked));
    }

    /* ── 快捷键（§7.3）────────────────────────────────────────── */
    document.addEventListener('keydown', function (e) {
      var target = e.target;
      var typing =
        target &&
        (target.tagName === 'INPUT' ||
          target.tagName === 'SELECT' ||
          target.tagName === 'TEXTAREA' ||
          target.isContentEditable);

      if (e.key === 'Escape') {
        /* §7.4：Esc 关闭「当前」浮层——三级子菜单优先，其次二级子菜单、主菜单/气泡 */
        if (activeNestedEl && !activeNestedEl.hidden) {
          e.preventDefault();
          closeNestedMenu();
          return;
        }
        if (activeSubmenuEl && !activeSubmenuEl.hidden) {
          e.preventDefault();
          closeSubmenu();
          return;
        }
        if (open) {
          e.preventDefault();
          closePopover();
        }
        return;
      }

      if (typing) return;

      /* ⌥⌘N：切换下一个屏幕（暂未支持多显示器，显式提示） */
      if (e.altKey && e.metaKey && (e.key === 'n' || e.key === 'N')) {
        e.preventDefault();
        toast('被控端多显示器切换暂未支持');
        return;
      }

      /* F：进入/退出全屏幕（无修饰键） */
      if (!e.metaKey && !e.ctrlKey && !e.altKey && (e.key === 'f' || e.key === 'F')) {
        e.preventDefault();
        if (open) closePopover();
        toggleFullscreen();
        return;
      }

      /* Tauri 真实控制会话：转发键盘按下（T21 U9 / 坑 8） */
      if (Bridge && Bridge.isTauri && realSessionActive && !watchMode && !open) {
        e.preventDefault();
        Bridge.sendInputEvent({
          kind: 'key',
          code: e.code || '',
          pressed: true,
          ctrl: e.ctrlKey,
          shift: e.shiftKey,
          alt: e.altKey,
          meta: e.metaKey
        }).catch(function () {});
      }
    });

    if (Bridge && Bridge.isTauri) {
      document.addEventListener('keyup', function (e) {
        if (!realSessionActive || watchMode || open) return;
        var target = e.target;
        var typing =
          target &&
          (target.tagName === 'INPUT' ||
            target.tagName === 'SELECT' ||
            target.tagName === 'TEXTAREA' ||
            target.isContentEditable);
        if (typing) return;
        e.preventDefault();
        Bridge.sendInputEvent({
          kind: 'key',
          code: e.code || '',
          pressed: false,
          ctrl: e.ctrlKey,
          shift: e.shiftKey,
          alt: e.altKey,
          meta: e.metaKey
        }).catch(function () {});
      });

      /* Stage ResizeObserver → Rust stage_rect（CSS 像素，由 Rust 乘以 scale_factor） */
      function reportStageRect() {
        var r = stage.getBoundingClientRect();
        if (!(r.width > 0 && r.height > 0)) return;
        Bridge.stageRect({
          x: r.left,
          y: r.top,
          width: r.width,
          height: r.height
        }).catch(function () {});
      }

      /* 订阅 Rust 侧 session://update 事件流（T21 U5 / §6 / U-M7 / U-M8） */
      var CONNECT_PHASE_TEXT = {
        endpoint_creating: '正在初始化连接…',
        connecting: '正在连接目标设备…',
        awaiting_welcome: '正在验证身份与协商参数…',
        first_frame_pending: '已连接，正在等待远端画面…'
      };
      var endedCloseTimer = 0;
      var sessionLifecycleState = 'idle'; /* idle | connecting | active | ended */
      var hasHardwareVideo = false;
      function setHasVideo(active) {
        var on = Boolean(active);
        document.body.classList.toggle('has-video', on);
        stage.classList.toggle('has-video', on);
      }
      function cancelEndedCloseTimer() {
        if (endedCloseTimer) {
          window.clearTimeout(endedCloseTimer);
          endedCloseTimer = 0;
        }
      }
      function isNormalEndReason(reason) {
        return reason === '用户主动断开' || reason === '会话已结束';
      }
      function resetSessionControlsForNewSession() {
        if (secMute) {
          secMute.checked = false;
          if (secPrivacy && secLock) {
            syncSecurityState();
          }
        }
        clipboardEnabled = true;
        syncClipboardMenuUi();
        seenNegotiated = false;
        hasActiveSessionSecurity = false;
        linkSecurityState.peerFingerprint = '';
        syncLinkSecurityUi();
        autoAppliedSavedRes = false;
        hasDisplayConfig = false;
        pendingUserResTarget = null;
        prevDisplayWidth = 0;
        prevDisplayHeight = 0;
        if (!relaySwitching && relayEl) {
          relayEl.checked = false;
          relayEl.disabled = false;
        }
        relaySwitching = false;
        streamState.width = 0;
        streamState.height = 0;
        streamState.nativeWidth = 0;
        streamState.nativeHeight = 0;
        streamState.hasServerPresets = false;
        streamState.presets = DEFAULT_MOCK_PRESETS.slice();
        renderAllResolutionLists();
      }

      function handleSessionUpdate(upd) {
        if (!upd || !upd.kind) return;
        var data = upd.data || {};
        if (upd.kind === 'connecting') {
          if (sessionLifecycleState !== 'connecting') {
            resetSessionControlsForNewSession();
          }
          sessionLifecycleState = 'connecting';
          seenNegotiated = false;
          hasHardwareVideo = false;
          setHasVideo(false);
          cancelEndedCloseTimer();
          stopLocalTicker();
          realSessionActive = true;
          if (typeof data.watch_only === 'boolean') {
            setWatchMode(data.watch_only);
          }
          if (stageHintEl) {
            stageHintEl.hidden = false;
            stageHintEl.textContent =
              (data.phase && CONNECT_PHASE_TEXT[data.phase]) || '正在连接远端画面…';
          }
          reportStageRect();
          return;
        }
        if (upd.kind === 'negotiated') {
          if (sessionLifecycleState === 'ended') {
            resetSessionControlsForNewSession();
          }
          sessionLifecycleState = 'active';
          seenNegotiated = true;
          cancelEndedCloseTimer();
          stopLocalTicker();
          realSessionActive = true;
          if (typeof data.watch_only === 'boolean') {
            setWatchMode(data.watch_only);
          }
          var backendStr = typeof data.backend === 'string' ? data.backend : '';
          hasHardwareVideo = backendStr.indexOf('metal') !== -1;
          setHasVideo(hasHardwareVideo);
          if (stageHintEl) {
            if (!hasHardwareVideo && backendStr.indexOf('降级') !== -1) {
              stageHintEl.hidden = false;
              stageHintEl.textContent = '硬件视频渲染不可用（当前为降级模式）';
            } else {
              stageHintEl.hidden = true;
            }
          }
          var negW = Number(data.width);
          var negH = Number(data.height);
          var negFps = Number(data.fps);
          if (isFinite(negW) && negW > 0) streamState.width = negW;
          if (isFinite(negH) && negH > 0) streamState.height = negH;
          if (isFinite(negFps) && negFps > 0) {
            streamState.fps = negFps;
            syncFpsMenuUi(negFps);
          }
          hasActiveSessionSecurity = true;
          if (typeof data.mtls_active === 'boolean') {
            linkSecurityState.encryptConnection = data.mtls_active;
          }
          linkSecurityState.peerFingerprint = data.peer_cert_fingerprint
            ? String(data.peer_cert_fingerprint)
            : '';
          syncRelaySwitchUi(data.transport);
          if (!streamState.hasServerPresets && streamState.width > 0 && streamState.height > 0) {
            streamState.nativeWidth = streamState.nativeWidth || streamState.width;
            streamState.nativeHeight = streamState.nativeHeight || streamState.height;
            streamState.presets = buildFallbackPresets(streamState.width, streamState.height);
          }
          renderAllResolutionLists();
          if (hudRows.length >= 8 && data.width && data.height) {
            hudRows[7].textContent = data.width + 'x' + data.height;
          }
          reportStageRect();
          return;
        }
        if (upd.kind === 'display_config') {
          sessionLifecycleState = 'active';
          cancelEndedCloseTimer();
          stopLocalTicker();
          realSessionActive = true;
          if (!seenNegotiated) {
            setHasVideo(true);
            reportStageRect();
          }
          if (stageHintEl && (hasHardwareVideo || !seenNegotiated)) {
            stageHintEl.hidden = true;
          }
          var cfgW = Number(data.width);
          var cfgH = Number(data.height);
          var cfgFps = Number(data.fps);
          var natW = Number(data.native_width);
          var natH = Number(data.native_height);
          var maxFps = Number(data.max_fps);
          var hasValidCfgDims = isFinite(cfgW) && cfgW > 0 && isFinite(cfgH) && cfgH > 0;
          var dimsChanged =
            hasValidCfgDims &&
            (cfgW !== prevDisplayWidth || cfgH !== prevDisplayHeight);
          if (isFinite(cfgW) && cfgW > 0) {
            streamState.width = cfgW;
            prevDisplayWidth = cfgW;
          }
          if (isFinite(cfgH) && cfgH > 0) {
            streamState.height = cfgH;
            prevDisplayHeight = cfgH;
          }
          if (isFinite(cfgFps) && cfgFps > 0) {
            streamState.fps = cfgFps;
            syncFpsMenuUi(cfgFps);
          }
          if (isFinite(natW) && natW > 0) streamState.nativeWidth = natW;
          if (isFinite(natH) && natH > 0) streamState.nativeHeight = natH;
          if (isFinite(maxFps) && maxFps > 0) streamState.maxFps = maxFps;
          if (Array.isArray(data.presets) && data.presets.length > 0) {
            streamState.presets = data.presets;
            streamState.hasServerPresets = true;
          } else if (!streamState.hasServerPresets) {
            var fbW = streamState.nativeWidth || streamState.width;
            var fbH = streamState.nativeHeight || streamState.height;
            if (fbW > 0 && fbH > 0) {
              streamState.presets = buildFallbackPresets(fbW, fbH);
            }
          }
          if (hudRows.length >= 8 && streamState.width > 0 && streamState.height > 0) {
            hudRows[7].textContent = streamState.width + 'x' + streamState.height;
          }
          renderAllResolutionLists();
          hasDisplayConfig = true;
          maybeAutoApplySavedResolution();
          if (pendingUserResTarget && hasValidCfgDims) {
            var matchedTarget =
              Math.abs(cfgW - pendingUserResTarget.width) <= 2 &&
              Math.abs(cfgH - pendingUserResTarget.height) <= 2;
            if (matchedTarget || dimsChanged) {
              pendingUserResTarget = null;
              toast('当前分辨率：' + cfgW + '×' + cfgH);
            }
          }
          return;
        }
        if (upd.kind === 'hud') {
          sessionLifecycleState = 'active';
          cancelEndedCloseTimer();
          stopLocalTicker();
          realSessionActive = true;
          if (!seenNegotiated) {
            /* 窗口迟到打开、仅经 sessionSnapshot 拉到 Hud 时，仍需透出底层 Metal 画面并补报矩形 */
            setHasVideo(true);
            reportStageRect();
          } else if (hasHardwareVideo) {
            setHasVideo(true);
          }
          if (stageHintEl && (hasHardwareVideo || !seenNegotiated)) {
            stageHintEl.hidden = true;
          }
          if (
            streamState.width === 0 &&
            data.resolution &&
            typeof data.resolution === 'string'
          ) {
            var resMatch = data.resolution.match(/^(\d+)\s*[x×]\s*(\d+)$/i);
            if (resMatch) {
              streamState.width = Number(resMatch[1]);
              streamState.height = Number(resMatch[2]);
              if (!streamState.hasServerPresets && streamState.width > 0 && streamState.height > 0) {
                streamState.nativeWidth = streamState.nativeWidth || streamState.width;
                streamState.nativeHeight = streamState.nativeHeight || streamState.height;
                streamState.presets = buildFallbackPresets(streamState.width, streamState.height);
              }
              renderAllResolutionLists();
            }
          }
          syncRelaySwitchUi(data.transport);
          applyHudSnapshot(data);
          return;
        }
        if (upd.kind === 'ended') {
          sessionLifecycleState = 'ended';
          seenNegotiated = false;
          hasHardwareVideo = false;
          setHasVideo(false);
          realSessionActive = false;
          if (relayEl) {
            relayEl.disabled = true;
          }
          var reason = data.reason || '会话已结束';
          toast(reason);
          if (isNormalEndReason(reason)) {
            closeSessionWindow();
          } else {
            /* U-M8：异常结束时在 stage 展示原因并延迟关闭，避免窗口秒闪导致用户看不到失败原因 */
            if (stageHintEl) {
              stageHintEl.hidden = false;
              stageHintEl.textContent = reason;
            }
            cancelEndedCloseTimer();
            endedCloseTimer = window.setTimeout(function () {
              endedCloseTimer = 0;
              closeSessionWindow();
            }, 2500);
          }
        }
      }

      Bridge.onSessionUpdate(handleSessionUpdate);

      /* Review M8：首帧主动拉取最近一次会话快照，消除窗口加载与事件推送之间的竞态窗口 */
      if (typeof Bridge.sessionSnapshot === 'function') {
        Bridge.sessionSnapshot()
          .then(function (snap) {
            if (snap) handleSessionUpdate(snap);
          })
          .catch(function () {});
      }

      reportStageRect();
      if (typeof window.ResizeObserver === 'function') {
        var ro = new window.ResizeObserver(reportStageRect);
        ro.observe(stage);
      }

      /* Stage 指针与滚轮输入转发（按实时 watchMode 动态门控，观看模式零注入，T21 U9 / U-M7） */
      var hudWrap = document.getElementById('hud-wrap');
      function isStageInputTarget(target) {
        if (!target) return false;
        if (hudWrap && hudWrap.contains(target)) return false;
        if (open) return false;
        return true;
      }

      function sendStagePointerMove(e) {
        var r = stage.getBoundingClientRect();
        if (!(r.width > 0 && r.height > 0)) return;
        Bridge.sendInputEvent({
          kind: 'mouse_move',
          x: e.clientX - r.left,
          y: e.clientY - r.top,
          stage_w: r.width,
          stage_h: r.height
        }).catch(function () {});
      }

      stage.addEventListener('pointermove', function (e) {
        if (!realSessionActive || watchMode || !isStageInputTarget(e.target)) return;
        sendStagePointerMove(e);
      });

      stage.addEventListener('pointerdown', function (e) {
        if (e.button === 2) {
          e.preventDefault();
        }
        if (!realSessionActive || watchMode || !isStageInputTarget(e.target)) return;
        sendStagePointerMove(e);
        Bridge.sendInputEvent({
          kind: 'mouse_button',
          button: e.button,
          pressed: true
        }).catch(function () {});
      });

      stage.addEventListener('pointerup', function (e) {
        if (!realSessionActive || watchMode || !isStageInputTarget(e.target)) return;
        sendStagePointerMove(e);
        Bridge.sendInputEvent({
          kind: 'mouse_button',
          button: e.button,
          pressed: false
        }).catch(function () {});
      });

      stage.addEventListener(
        'wheel',
        function (e) {
          if (!realSessionActive || watchMode || !isStageInputTarget(e.target)) return;
          e.preventDefault();
          var lh = parseFloat(window.getComputedStyle(stage).lineHeight);
          if (!isFinite(lh) || lh <= 0) lh = 16;
          Bridge.sendInputEvent({
            kind: 'mouse_wheel',
            delta_y_px: e.deltaY,
            line_height_px: lh,
            delta_x_px: e.deltaX || 0
          }).catch(function () {});
        },
        { passive: false }
      );
    }

    /* 窗口尺寸变化：重新定位当前浮层（§3.6 定位跟随锚点） */
    window.addEventListener('resize', function () {
      if (open) place(open.el, open.anchor, open.align);
      if (activeSubmenuEl && !activeSubmenuEl.hidden && submenuAnchorItem) {
        placeSubmenu(submenuAnchorItem, activeSubmenuEl);
      }
      if (activeNestedEl && !activeNestedEl.hidden && nestedAnchorItem) {
        var parentEl = nestedAnchorItem.closest('.sn-popover') || activeSubmenuEl || (open && open.el);
        placeFloatingMenu(nestedAnchorItem, activeNestedEl, parentEl);
      }
    });

    /* ── 独立预览（?preview=s2|s3|s3-quality|s4）─────────────────
     * 浮层默认收起；带参数时自动打开对应浮层，便于 file:// 逐屏验收。
     * 不属于产品交互。 */
    var preview = params.get('preview') || '';
    if (preview === 's2') {
      openPopover(securityEl, shieldBtn, 'center');
    } else if (preview === 's3' || preview === 's3-quality') {
      openPopover(menuEl, ctrlBtn, 'right');
      if (preview === 's3-quality' && qualityItem) openSubmenu(qualityItem, qualityEl);
    } else if (preview === 's4') {
      /* HUD 常驻，无需额外动作 */
    }
  }

  /* ── 启动 ─────────────────────────────────────────────────────── */
  function boot() {
    initTerminal();
    initSession();
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', boot);
  } else {
    boot();
  }

  /* 时钟注入点（§8.6.2）：传一个返回毫秒的函数；换钟后计时从新钟的当前时刻起算 */
  window.RCSession = {
    setClock: function (fn) {
      if (typeof fn === 'function') clock.nowMs = fn;
    },
    formatHms: formatHms
  };
})();
