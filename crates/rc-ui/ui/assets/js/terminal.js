/* terminal.js —— W3 远程 PTY 交互终端（纯静态前端，多标签，零 innerHTML，零第三方依赖）
 *
 * 纪律：
 *   - !IS_TAURI（浏览器静态预览）：保留 §6 W3 静态提示行并驱动本页右上独立计时器；
 *   - IS_TAURI（Tauri 真窗口）：先订阅 terminal://event 与会话状态，再开启远端 PTY（最多 4 个标签），
 *     支持 VT100/ANSI（含清屏/光标定位/卷动区/备选屏幕 ?1049h/l / SGR 256+24bit 真彩色 / 括号粘贴 ?2004h/l）、
 *     CJK 双倍字符宽定位、Alt/F1-F12/Shift+Tab/Ctrl+Shift+C/V、中文 IME 与 100ms 防抖 Resize；
 *   - 严禁使用 innerHTML：所有终端输出与标签页一律经 createElement + textContent 渲染。
 */

(function () {
  'use strict';

  var Bridge = window.RCBridge;
  var IS_TAURI = Boolean(Bridge && Bridge.isTauri);

  function pad2(n) {
    return n < 10 ? '0' + n : String(n);
  }

  function formatHms(totalSec) {
    var s = Math.max(0, Math.floor(totalSec));
    var h = Math.floor(s / 3600);
    var m = Math.floor((s % 3600) / 60);
    var rem = s % 60;
    return pad2(h) + ':' + pad2(m) + ':' + pad2(rem);
  }

  /* 浏览器预览模式：仅驱动轻量独立计时器，保留 §6 W3 静态稿 */
  if (!IS_TAURI) {
    var previewTimer = document.getElementById('terminal-timer');
    if (previewTimer) {
      var mockSec = 0;
      previewTimer.textContent = formatHms(mockSec);
      window.setInterval(function () {
        mockSec += 1;
        previewTimer.textContent = formatHms(mockSec);
      }, 1000);
    }
    return;
  }

  var MAX_LINES = 2000;
  var MAX_TABS = 4;
  var RESIZE_DEBOUNCE_MS = 100;

  var FG_CLASS_BY_CODE = {
    30: 'tm-ansi-fg-muted',
    31: 'tm-ansi-fg-red',
    32: 'tm-ansi-fg-green',
    33: 'tm-ansi-fg-yellow',
    34: 'tm-ansi-fg-blue',
    35: 'tm-ansi-fg-magenta',
    36: 'tm-ansi-fg-cyan',
    37: 'tm-ansi-fg-white',
    90: 'tm-ansi-fg-muted',
    91: 'tm-ansi-fg-red',
    92: 'tm-ansi-fg-green',
    93: 'tm-ansi-fg-yellow',
    94: 'tm-ansi-fg-blue',
    95: 'tm-ansi-fg-magenta',
    96: 'tm-ansi-fg-cyan',
    97: 'tm-ansi-fg-white'
  };

  var BG_CLASS_BY_CODE = {
    41: 'tm-ansi-bg-red',
    42: 'tm-ansi-bg-green',
    43: 'tm-ansi-bg-yellow',
    44: 'tm-ansi-bg-blue',
    101: 'tm-ansi-bg-red',
    102: 'tm-ansi-bg-green',
    103: 'tm-ansi-bg-yellow',
    104: 'tm-ansi-bg-blue'
  };

  var FKEY_SEQ = {
    F1: '\x1bOP',
    F2: '\x1bOQ',
    F3: '\x1bOR',
    F4: '\x1bOS',
    F5: '\x1b[15~',
    F6: '\x1b[17~',
    F7: '\x1b[18~',
    F8: '\x1b[19~',
    F9: '\x1b[20~',
    F10: '\x1b[21~',
    F11: '\x1b[23~',
    F12: '\x1b[24~'
  };

  var ANSI16_COLORS = [
    '#18181b', '#ef4444', '#22c55e', '#eab308', '#3b82f6', '#a855f7', '#06b6d4', '#e4e4e7',
    '#71717a', '#f87171', '#4ade80', '#facc15', '#60a5fa', '#c084fc', '#22d3ee', '#fafafa'
  ];

  function clampByte(v) {
    var n = parseInt(v, 10);
    if (isNaN(n) || n < 0) return 0;
    if (n > 255) return 255;
    return n;
  }

  function ansi256ToCss(idx) {
    var n = clampByte(idx);
    if (n < 16) return ANSI16_COLORS[n];
    if (n >= 232) {
      var gray = 8 + (n - 232) * 10;
      return 'rgb(' + gray + ',' + gray + ',' + gray + ')';
    }
    var c = n - 16;
    var r = Math.floor(c / 36);
    var g = Math.floor((c % 36) / 6);
    var b = c % 6;
    var steps = [0, 95, 135, 175, 215, 255];
    return 'rgb(' + steps[r] + ',' + steps[g] + ',' + steps[b] + ')';
  }

  function emptyStyle() {
    return {
      fg: '',
      bg: '',
      fgCss: '',
      bgCss: '',
      bold: false,
      dim: false,
      italic: false,
      underline: false,
      inverse: false
    };
  }

  function styleClassName(st) {
    var cls = [];
    if (st.bold) cls.push('tm-ansi-bold');
    if (st.dim) cls.push('tm-ansi-dim');
    if (st.italic) cls.push('tm-ansi-italic');
    if (st.underline) cls.push('tm-ansi-underline');
    if (st.inverse) cls.push('tm-ansi-inverse');
    if (!st.fgCss && st.fg) cls.push(st.fg);
    if (!st.bgCss && st.bg) cls.push(st.bg);
    return cls.join(' ');
  }

  function makeStyleToken(st) {
    var cls = styleClassName(st);
    if (!cls && !st.fgCss && !st.bgCss) return '';
    return cls + '|' + (st.fgCss || '') + '|' + (st.bgCss || '');
  }

  function parseStyleToken(token) {
    if (!token) return null;
    var parts = token.split('|');
    return {
      cls: parts[0] || '',
      fgCss: parts[1] || '',
      bgCss: parts[2] || ''
    };
  }

  function charDisplayWidth(ch) {
    if (!ch) return 0;
    var cp = ch.codePointAt ? ch.codePointAt(0) : ch.charCodeAt(0);
    if (cp == null || cp < 0x20 || (cp >= 0x7f && cp < 0xa0)) return 0;
    /* 组合音标 */
    if (
      (cp >= 0x0300 && cp <= 0x036f) ||
      (cp >= 0x1ab0 && cp <= 0x1aff) ||
      (cp >= 0x20d0 && cp <= 0x20ff) ||
      (cp >= 0xfe20 && cp <= 0xfe2f)
    ) {
      return 0;
    }
    /* CJK / 全角 / 韩文 / 日文 / 常用宽表情 */
    if (
      cp >= 0x1100 &&
      (cp <= 0x115f ||
        cp === 0x2329 ||
        cp === 0x232a ||
        (cp >= 0x2e80 && cp <= 0x303e) ||
        (cp >= 0x3040 && cp <= 0x3247) ||
        (cp >= 0x3250 && cp <= 0x4dbf) ||
        (cp >= 0x4e00 && cp <= 0xa4cf) ||
        (cp >= 0xa960 && cp <= 0xa97f) ||
        (cp >= 0xac00 && cp <= 0xd7a3) ||
        (cp >= 0xf900 && cp <= 0xfaff) ||
        (cp >= 0xfe10 && cp <= 0xfe19) ||
        (cp >= 0xfe30 && cp <= 0xfe6f) ||
        (cp >= 0xff00 && cp <= 0xff60) ||
        (cp >= 0xffe0 && cp <= 0xffe6) ||
        (cp >= 0x1f300 && cp <= 0x1faff) ||
        (cp >= 0x20000 && cp <= 0x3fffd))
    ) {
      return 2;
    }
    return 1;
  }

  function makeLine(system) {
    return {
      chars: [],
      styles: [],
      dirty: true,
      system: Boolean(system),
      node: null
    };
  }

  function errMessage(err) {
    if (!err) return '连接远程终端失败';
    if (typeof err === 'string') return err;
    if (err.message) return String(err.message);
    return String(err);
  }

  function init() {
    var bodyEl = document.getElementById('terminal-body');
    var mockRowEl = document.getElementById('terminal-mock-row');
    var tabsWrapEl = document.getElementById('terminal-tabs');
    var tabListEl = document.getElementById('terminal-tab-list');
    var newTabBtn = document.getElementById('terminal-new-tab');
    var titleEl = document.getElementById('terminal-title');
    var timerEl = document.getElementById('terminal-timer');
    var statusBarEl = document.getElementById('terminal-status-bar');
    var statusRightEl = document.getElementById('terminal-status-right');
    var statusDotEl = document.getElementById('terminal-status-dot');
    var statusTextEl = document.getElementById('terminal-status-text');
    var cwdEl = document.getElementById('terminal-cwd');
    var reconnectBtn = document.getElementById('terminal-reconnect');
    var screenEl = document.getElementById('terminal-screen');
    var imeEl = document.getElementById('terminal-ime');
    var noticeEl = document.getElementById('terminal-notice');
    var noticeTextEl = document.getElementById('terminal-notice-text');
    var noticeReconnectBtn = document.getElementById('terminal-notice-reconnect');

    if (!bodyEl || !screenEl) return;

    /* 切换到实时交互模式布局，#terminal-timer 完全由本模块独立管理 */
    bodyEl.classList.add('is-live');
    if (mockRowEl) mockRowEl.hidden = true;
    if (tabsWrapEl) tabsWrapEl.hidden = false;
    if (statusBarEl) statusBarEl.hidden = false;
    if (timerEl && statusRightEl) {
      statusRightEl.insertBefore(timerEl, reconnectBtn || null);
      timerEl.textContent = '00:00:00';
    }
    screenEl.hidden = false;

    /* ── 独立计时器 ─────────────────────────────────────────── */
    var elapsedSec = 0;
    var timerHandle = 0;

    function syncTimerRunning() {
      var anyConnected = tabs.some(function (t) {
        return t.connected;
      });
      if (anyConnected && !timerHandle) {
        timerHandle = window.setInterval(function () {
          elapsedSec += 1;
          if (timerEl) timerEl.textContent = formatHms(elapsedSec);
        }, 1000);
      } else if (!anyConnected && timerHandle) {
        window.clearInterval(timerHandle);
        timerHandle = 0;
      }
    }

    function resetSessionTimer() {
      if (timerHandle) {
        window.clearInterval(timerHandle);
        timerHandle = 0;
      }
      elapsedSec = 0;
      if (timerEl) timerEl.textContent = '00:00:00';
    }

    /* ── 多标签终端状态（最多 4 个） ─────────────────────────── */
    var sessionState = {
      active: false,
      connected: false,
      watchOnly: false,
      deviceName: ''
    };
    var lastEndedReason = '';
    var tabs = [];
    var activeTabId = null;
    var nextLocalTabId = 1;
    var staleTermIds = Object.create(null);
    var lastCols = 80;
    var lastRows = 24;
    var resizeTimer = 0;
    var rafId = 0;
    var renderPending = false;
    var fullRebuild = true;
    var removedNodes = [];
    var prevCursorLineIdx = -1;
    var prevCursorCol = -1;
    var prevCursorVisible = false;
    var isComposing = false;

    function createTab(label) {
      return {
        tabId: nextLocalTabId++,
        label: label || '终端 ' + nextLocalTabId,
        termId: null,
        connectSeq: 0,
        opening: false,
        connected: false,
        title: '',
        cwd: '',
        lines: [makeLine(false)],
        altLines: null,
        savedMainState: null,
        inAltScreen: false,
        scrollTop: 0,
        scrollBottom: -1,
        curLineIdx: 0,
        curCol: 0,
        savedCursor: null,
        cursorVisible: true,
        bracketedPaste: false,
        lastBsCol: -1,
        lastBsSpaceCol: -1,
        curStyle: emptyStyle(),
        curStyleToken: '',
        parseState: 'normal',
        csiBuf: '',
        oscBuf: '',
        pendingSurrogate: '',
        utf8Decoder: typeof TextDecoder === 'function' ? new TextDecoder('utf-8', { fatal: false }) : null
      };
    }

    function getActiveTab() {
      for (var i = 0; i < tabs.length; i += 1) {
        if (tabs[i].tabId === activeTabId) return tabs[i];
      }
      return null;
    }

    function findTabByTermId(termId) {
      for (var i = 0; i < tabs.length; i += 1) {
        if (tabs[i].termId === termId) return tabs[i];
      }
      return null;
    }

    function isSessionReady() {
      return Boolean(sessionState.active && sessionState.connected && !sessionState.watchOnly);
    }

    function resetTabBuffer(tab) {
      tab.lines = [makeLine(false)];
      tab.altLines = null;
      tab.savedMainState = null;
      tab.inAltScreen = false;
      tab.scrollTop = 0;
      tab.scrollBottom = -1;
      tab.curLineIdx = 0;
      tab.curCol = 0;
      tab.savedCursor = null;
      tab.cursorVisible = true;
      tab.bracketedPaste = false;
      tab.lastBsCol = -1;
      tab.lastBsSpaceCol = -1;
      tab.curStyle = emptyStyle();
      tab.curStyleToken = '';
      tab.parseState = 'normal';
      tab.csiBuf = '';
      tab.oscBuf = '';
      tab.pendingSurrogate = '';
      if (typeof TextDecoder === 'function') {
        tab.utf8Decoder = new TextDecoder('utf-8', { fatal: false });
      }
      if (tab.tabId === activeTabId) {
        removedNodes = [];
        fullRebuild = true;
        prevCursorLineIdx = -1;
        prevCursorCol = -1;
        prevCursorVisible = false;
        scheduleRender();
      }
    }

    /* ── UI 状态栏与标签栏渲染 ──────────────────────────────── */

    function setStatus(kind, text) {
      if (statusTextEl) statusTextEl.textContent = text;
      if (statusDotEl) {
        statusDotEl.classList.toggle('is-online', kind === 'online');
        statusDotEl.classList.toggle('is-error', kind === 'error');
      }
    }

    function showNotice(message, showReconnect) {
      if (noticeTextEl) noticeTextEl.textContent = message;
      if (noticeEl) noticeEl.hidden = false;
      if (noticeReconnectBtn) noticeReconnectBtn.hidden = showReconnect === false;
      if (reconnectBtn) reconnectBtn.hidden = showReconnect === false;
    }

    function hideNotice() {
      if (noticeEl) noticeEl.hidden = true;
    }

    function renderTabBar() {
      if (tabsWrapEl) {
        tabsWrapEl.hidden = false;
      }
      if (tabListEl) {
        tabListEl.textContent = '';
        tabs.forEach(function (tab, index) {
          var isActive = tab.tabId === activeTabId;
          var btn = document.createElement('button');
          btn.type = 'button';
          btn.className = 'tm-tab' + (isActive ? ' is-active' : '');
          btn.setAttribute('role', 'tab');
          btn.setAttribute('aria-selected', isActive ? 'true' : 'false');

          var labelSpan = document.createElement('span');
          labelSpan.className = 'tm-tab__title';
          labelSpan.textContent = tab.title || tab.label || ('终端 ' + (index + 1));
          btn.appendChild(labelSpan);

          var closeBtn = document.createElement('span');
          closeBtn.className = 'tm-tab__close';
          closeBtn.setAttribute('role', 'button');
          closeBtn.setAttribute('aria-label', '关闭标签');
          closeBtn.textContent = '×';
          closeBtn.addEventListener('click', function (ev) {
            ev.stopPropagation();
            closeTab(tab.tabId);
          });
          btn.appendChild(closeBtn);

          btn.addEventListener('click', function () {
            switchTab(tab.tabId);
          });
          tabListEl.appendChild(btn);
        });
      }
      if (newTabBtn) {
        newTabBtn.disabled = !isSessionReady() || tabs.length >= MAX_TABS;
      }
    }

    function syncActiveTabChrome() {
      var tab = getActiveTab();
      var devSuffix = sessionState.deviceName ? ' · ' + sessionState.deviceName : '';
      if (!tab) {
        if (titleEl) {
          titleEl.textContent = sessionState.deviceName || '远程终端';
          document.title = (sessionState.deviceName || '远程终端') + ' — RabbitControl';
        }
        if (cwdEl) cwdEl.hidden = true;
        if (!isSessionReady()) {
          setStatus('error', sessionState.watchOnly ? '观看模式只读' : sessionState.active ? '连接中…' : '未连接');
        } else {
          setStatus('error', '未打开终端');
        }
        renderTabBar();
        syncTimerRunning();
        return;
      }

      var baseTitle = tab.title || tab.label || '远程终端';
      if (titleEl) {
        titleEl.textContent = baseTitle + devSuffix;
        document.title = baseTitle + devSuffix + ' — RabbitControl';
      }
      if (cwdEl) {
        if (tab.cwd) {
          cwdEl.textContent = tab.cwd;
          cwdEl.hidden = false;
        } else {
          cwdEl.hidden = true;
        }
      }
      if (tab.opening) {
        setStatus('connecting', '连接中…' + devSuffix);
        if (reconnectBtn) reconnectBtn.hidden = true;
      } else if (tab.connected) {
        setStatus('online', '已连接' + devSuffix);
        if (reconnectBtn) reconnectBtn.hidden = true;
      } else {
        setStatus('error', '未连接' + devSuffix);
        if (reconnectBtn) reconnectBtn.hidden = !isSessionReady();
      }
      renderTabBar();
      syncTimerRunning();
    }

    function focusTerminal() {
      if (imeEl) {
        imeEl.focus({ preventScroll: true });
      } else {
        screenEl.focus({ preventScroll: true });
      }
    }

    /* ── VT100 / ANSI 缓冲区操作（作用于指定 Tab） ──────────── */

    function resetBsTracking(tab) {
      tab.lastBsCol = -1;
      tab.lastBsSpaceCol = -1;
    }

    function viewportTopOf(tab) {
      return Math.max(0, tab.lines.length - lastRows);
    }

    function scrollBoundsOf(tab) {
      var top = viewportTopOf(tab);
      var rTop = Math.max(0, Math.min(lastRows - 1, tab.scrollTop));
      var rawBot = tab.scrollBottom >= 0 ? tab.scrollBottom : lastRows - 1;
      var rBot = Math.max(rTop, Math.min(lastRows - 1, rawBot));
      return {
        startIdx: top + rTop,
        endIdx: top + rBot
      };
    }

    function trimLinesAboveMax(tab) {
      var dropLimit = tab.inAltScreen ? lastRows : MAX_LINES;
      while (tab.lines.length > dropLimit) {
        var dropped = tab.lines.shift();
        if (tab.tabId === activeTabId && dropped && dropped.node) {
          removedNodes.push(dropped.node);
        }
        if (tab.curLineIdx > 0) tab.curLineIdx -= 1;
        if (tab.tabId === activeTabId && prevCursorLineIdx >= 0) prevCursorLineIdx -= 1;
      }
    }

    function truncateLinesAfter(tab, keepCount) {
      while (tab.lines.length > keepCount) {
        var popped = tab.lines.pop();
        if (tab.tabId === activeTabId && popped && popped.node) {
          removedNodes.push(popped.node);
        }
      }
      if (tab.tabId === activeTabId && prevCursorLineIdx >= tab.lines.length) {
        prevCursorLineIdx = -1;
      }
    }

    function ensureLineAt(tab, idx) {
      while (tab.lines.length <= idx) {
        tab.lines.push(makeLine(false));
      }
      trimLinesAboveMax(tab);
      if (idx >= tab.lines.length) idx = tab.lines.length - 1;
      return tab.lines[idx];
    }

    function ensureCurrentLine(tab) {
      if (!tab.lines.length) {
        tab.lines.push(makeLine(false));
        tab.curLineIdx = 0;
        tab.curCol = 0;
      }
      if (tab.curLineIdx >= tab.lines.length) {
        tab.curLineIdx = tab.lines.length - 1;
      }
      if (tab.curLineIdx < 0) {
        tab.curLineIdx = 0;
      }
      return tab.lines[tab.curLineIdx];
    }

    function clearWideAt(line, col) {
      if (col < 0 || col >= line.chars.length) return;
      if (line.chars[col] === '' && col > 0) {
        line.chars[col - 1] = ' ';
        line.chars[col] = ' ';
        line.dirty = true;
      } else if (col + 1 < line.chars.length && line.chars[col + 1] === '') {
        line.chars[col] = ' ';
        line.chars[col + 1] = ' ';
        line.dirty = true;
      }
    }

    function putChar(tab, ch) {
      var width = charDisplayWidth(ch);
      if (width === 0) return;

      var line = ensureCurrentLine(tab);
      while (line.chars.length < tab.curCol) {
        line.chars.push(' ');
        line.styles.push('');
      }
      if (
        ch === ' ' &&
        !tab.curStyleToken &&
        tab.curCol === tab.lastBsCol &&
        tab.curCol === line.chars.length - 1
      ) {
        tab.lastBsSpaceCol = tab.curCol;
        tab.lastBsCol = -1;
      } else {
        resetBsTracking(tab);
      }

      clearWideAt(line, tab.curCol);
      if (width === 2) {
        clearWideAt(line, tab.curCol + 1);
      }

      line.chars[tab.curCol] = ch;
      line.styles[tab.curCol] = tab.curStyleToken;
      if (width === 2) {
        line.chars[tab.curCol + 1] = '';
        line.styles[tab.curCol + 1] = tab.curStyleToken;
      }
      line.dirty = true;
      tab.curCol += width;
    }

    function scrollUpInRegion(tab, count) {
      var bounds = scrollBoundsOf(tab);
      var endLine = ensureLineAt(tab, bounds.endIdx);
      void endLine;
      var start = bounds.startIdx;
      var end = Math.min(tab.lines.length - 1, bounds.endIdx);
      for (var k = 0; k < count; k += 1) {
        if (tab.scrollTop === 0 && (tab.scrollBottom < 0 || tab.scrollBottom >= lastRows - 1) && !tab.inAltScreen) {
          tab.lines.push(makeLine(false));
          trimLinesAboveMax(tab);
        } else {
          var removed = tab.lines.splice(start, 1)[0];
          if (tab.tabId === activeTabId && removed && removed.node) {
            removedNodes.push(removed.node);
          }
          tab.lines.splice(end, 0, makeLine(false));
          if (tab.tabId === activeTabId) fullRebuild = true;
        }
      }
    }

    function scrollDownInRegion(tab, count) {
      var bounds = scrollBoundsOf(tab);
      ensureLineAt(tab, bounds.endIdx);
      var start = bounds.startIdx;
      var end = Math.min(tab.lines.length - 1, bounds.endIdx);
      for (var k = 0; k < count; k += 1) {
        var removed = tab.lines.splice(end, 1)[0];
        if (tab.tabId === activeTabId && removed && removed.node) {
          removedNodes.push(removed.node);
        }
        tab.lines.splice(start, 0, makeLine(false));
      }
      if (tab.tabId === activeTabId) fullRebuild = true;
    }

    function newLine(tab) {
      resetBsTracking(tab);
      var bounds = scrollBoundsOf(tab);
      if (tab.curLineIdx === bounds.endIdx) {
        scrollUpInRegion(tab, 1);
        tab.curLineIdx = Math.min(tab.lines.length - 1, bounds.endIdx);
      } else if (tab.curLineIdx < tab.lines.length - 1) {
        tab.curLineIdx += 1;
      } else {
        tab.lines.push(makeLine(false));
        trimLinesAboveMax(tab);
        tab.curLineIdx = tab.lines.length - 1;
      }
      tab.curCol = 0;
    }

    function lineFeedOnly(tab) {
      resetBsTracking(tab);
      var bounds = scrollBoundsOf(tab);
      if (tab.curLineIdx === bounds.endIdx) {
        scrollUpInRegion(tab, 1);
        tab.curLineIdx = Math.min(tab.lines.length - 1, bounds.endIdx);
      } else {
        ensureLineAt(tab, tab.curLineIdx + 1);
        tab.curLineIdx = Math.min(tab.lines.length - 1, tab.curLineIdx + 1);
      }
    }

    function appendSystemLine(tab, text) {
      resetBsTracking(tab);
      var sysLine = makeLine(true);
      sysLine.chars = String(text).split('');
      tab.lines.push(sysLine);
      trimLinesAboveMax(tab);
      tab.curLineIdx = tab.lines.length - 1;
      tab.curCol = tab.lines[tab.curLineIdx].chars.length;
      if (tab.tabId === activeTabId) scheduleRender();
    }

    function applySgr(tab, paramsStr) {
      var raw = paramsStr === '' ? ['0'] : paramsStr.split(';');
      for (var i = 0; i < raw.length; i += 1) {
        var code = parseInt(raw[i], 10);
        if (isNaN(code)) code = 0;
        if (code === 0) {
          tab.curStyle = emptyStyle();
        } else if (code === 1) {
          tab.curStyle.bold = true;
        } else if (code === 2) {
          tab.curStyle.dim = true;
        } else if (code === 3) {
          tab.curStyle.italic = true;
        } else if (code === 4) {
          tab.curStyle.underline = true;
        } else if (code === 7) {
          tab.curStyle.inverse = true;
        } else if (code === 22) {
          tab.curStyle.bold = false;
          tab.curStyle.dim = false;
        } else if (code === 23) {
          tab.curStyle.italic = false;
        } else if (code === 24) {
          tab.curStyle.underline = false;
        } else if (code === 27) {
          tab.curStyle.inverse = false;
        } else if (code === 39) {
          tab.curStyle.fg = '';
          tab.curStyle.fgCss = '';
        } else if (code === 49) {
          tab.curStyle.bg = '';
          tab.curStyle.bgCss = '';
        } else if (FG_CLASS_BY_CODE[code]) {
          tab.curStyle.fg = FG_CLASS_BY_CODE[code];
          tab.curStyle.fgCss = '';
        } else if (BG_CLASS_BY_CODE[code]) {
          tab.curStyle.bg = BG_CLASS_BY_CODE[code];
          tab.curStyle.bgCss = '';
        } else if (code === 38 || code === 48) {
          var mode = parseInt(raw[i + 1], 10);
          var cssVal = '';
          if (mode === 5 && i + 2 < raw.length) {
            cssVal = ansi256ToCss(raw[i + 2]);
            i += 2;
          } else if (mode === 2 && i + 4 < raw.length) {
            cssVal =
              'rgb(' +
              clampByte(raw[i + 2]) +
              ',' +
              clampByte(raw[i + 3]) +
              ',' +
              clampByte(raw[i + 4]) +
              ')';
            i += 4;
          }
          if (cssVal) {
            if (code === 38) {
              tab.curStyle.fgCss = cssVal;
              tab.curStyle.fg = '';
            } else {
              tab.curStyle.bgCss = cssVal;
              tab.curStyle.bg = '';
            }
          }
        }
      }
      tab.curStyleToken = makeStyleToken(tab.curStyle);
    }

    function enterAltScreen(tab, saveCursorFirst) {
      if (saveCursorFirst) {
        tab.savedCursor = { lineIdx: tab.curLineIdx, col: tab.curCol };
      }
      if (!tab.inAltScreen) {
        tab.savedMainState = {
          lines: tab.lines,
          curLineIdx: tab.curLineIdx,
          curCol: tab.curCol,
          scrollTop: tab.scrollTop,
          scrollBottom: tab.scrollBottom
        };
        tab.inAltScreen = true;
      }
      tab.lines = [];
      for (var i = 0; i < lastRows; i += 1) {
        tab.lines.push(makeLine(false));
      }
      tab.curLineIdx = 0;
      tab.curCol = 0;
      tab.scrollTop = 0;
      tab.scrollBottom = -1;
      if (tab.tabId === activeTabId) fullRebuild = true;
    }

    function leaveAltScreen(tab, restoreCursorAfter) {
      if (tab.inAltScreen && tab.savedMainState) {
        tab.lines = tab.savedMainState.lines;
        tab.curLineIdx = tab.savedMainState.curLineIdx;
        tab.curCol = tab.savedMainState.curCol;
        tab.scrollTop = tab.savedMainState.scrollTop;
        tab.scrollBottom = tab.savedMainState.scrollBottom;
        tab.savedMainState = null;
        tab.inAltScreen = false;
        tab.lines.forEach(function (ln) {
          ln.node = null;
          ln.dirty = true;
        });
        if (tab.tabId === activeTabId) fullRebuild = true;
      }
      if (restoreCursorAfter && tab.savedCursor) {
        tab.curLineIdx = Math.max(0, Math.min(tab.lines.length - 1, tab.savedCursor.lineIdx));
        tab.curCol = Math.max(0, tab.savedCursor.col);
      }
    }

    function handleDecPrivateMode(tab, paramsStr, enable) {
      var items = paramsStr.split(';');
      items.forEach(function (item) {
        var mode = parseInt(item, 10);
        if (mode === 1049) {
          if (enable) enterAltScreen(tab, true);
          else leaveAltScreen(tab, true);
        } else if (mode === 47 || mode === 1047) {
          if (enable) enterAltScreen(tab, false);
          else leaveAltScreen(tab, false);
        } else if (mode === 25) {
          tab.cursorVisible = enable;
        } else if (mode === 2004) {
          tab.bracketedPaste = enable;
        }
      });
    }

    function handleCsi(tab, paramsRaw, finalChar) {
      if (paramsRaw.charAt(0) === '?') {
        if (finalChar === 'h' || finalChar === 'l') {
          handleDecPrivateMode(tab, paramsRaw.slice(1), finalChar === 'h');
        }
        return;
      }
      if (paramsRaw.charAt(0) === '>') {
        return;
      }
      if (finalChar === 'm') {
        applySgr(tab, paramsRaw);
        return;
      }
      resetBsTracking(tab);
      var n = Math.max(1, parseInt(paramsRaw || '1', 10) || 1);

      if (finalChar === 'A') {
        var boundsUp = scrollBoundsOf(tab);
        var minLine = Math.max(viewportTopOf(tab), boundsUp.startIdx);
        tab.curLineIdx = Math.max(minLine, tab.curLineIdx - n);
        return;
      }
      if (finalChar === 'B') {
        var maxLineB = viewportTopOf(tab) + Math.max(0, lastRows - 1);
        var nextLineB = Math.min(maxLineB, tab.curLineIdx + n);
        ensureLineAt(tab, nextLineB);
        tab.curLineIdx = Math.min(nextLineB, tab.lines.length - 1);
        return;
      }
      if (finalChar === 'C') {
        tab.curCol = Math.min(lastCols - 1, tab.curCol + n);
        return;
      }
      if (finalChar === 'D') {
        tab.curCol = Math.max(0, tab.curCol - n);
        return;
      }
      if (finalChar === 'E') {
        var nextE = Math.min(viewportTopOf(tab) + Math.max(0, lastRows - 1), tab.curLineIdx + n);
        ensureLineAt(tab, nextE);
        tab.curLineIdx = Math.min(nextE, tab.lines.length - 1);
        tab.curCol = 0;
        return;
      }
      if (finalChar === 'F') {
        tab.curLineIdx = Math.max(viewportTopOf(tab), tab.curLineIdx - n);
        tab.curCol = 0;
        return;
      }
      if (finalChar === 'G') {
        var colAbs = parseInt(paramsRaw || '1', 10);
        tab.curCol = Math.max(0, Math.min(lastCols - 1, (isNaN(colAbs) ? 1 : colAbs) - 1));
        return;
      }
      if (finalChar === 'H' || finalChar === 'f') {
        var parts = paramsRaw.split(';');
        var row = parseInt(parts[0] || '1', 10);
        var col = parseInt(parts[1] || '1', 10);
        if (isNaN(row) || row < 1) row = 1;
        if (isNaN(col) || col < 1) col = 1;
        var vpTop = viewportTopOf(tab);
        var targetLine = Math.min(vpTop + Math.max(0, lastRows - 1), vpTop + (row - 1));
        ensureLineAt(tab, targetLine);
        tab.curLineIdx = Math.min(targetLine, tab.lines.length - 1);
        tab.curCol = Math.max(0, Math.min(lastCols - 1, col - 1));
        return;
      }
      if (finalChar === 'J') {
        var jMode = parseInt(paramsRaw || '0', 10);
        if (jMode === 2 || jMode === 3) {
          tab.lines = [makeLine(false)];
          if (tab.tabId === activeTabId) {
            removedNodes = [];
            fullRebuild = true;
            prevCursorLineIdx = -1;
          }
          tab.curLineIdx = 0;
          tab.curCol = 0;
        } else if (jMode === 1) {
          var vpStart = viewportTopOf(tab);
          for (var i1 = vpStart; i1 < tab.curLineIdx && i1 < tab.lines.length; i1 += 1) {
            tab.lines[i1].chars = [];
            tab.lines[i1].styles = [];
            tab.lines[i1].dirty = true;
          }
          var ln1 = ensureCurrentLine(tab);
          for (var c1 = 0; c1 <= tab.curCol && c1 < ln1.chars.length; c1 += 1) {
            ln1.chars[c1] = ' ';
            ln1.styles[c1] = '';
          }
          ln1.dirty = true;
        } else {
          var ln0 = ensureCurrentLine(tab);
          ln0.chars.length = Math.min(ln0.chars.length, tab.curCol);
          ln0.styles.length = ln0.chars.length;
          ln0.dirty = true;
          truncateLinesAfter(tab, tab.curLineIdx + 1);
        }
        return;
      }
      if (finalChar === 'K') {
        var kMode = parseInt(paramsRaw || '0', 10);
        var lineK = ensureCurrentLine(tab);
        if (kMode === 2) {
          lineK.chars = [];
          lineK.styles = [];
          lineK.dirty = true;
        } else if (kMode === 1) {
          for (var ck = 0; ck < tab.curCol && ck < lineK.chars.length; ck += 1) {
            lineK.chars[ck] = ' ';
            lineK.styles[ck] = '';
          }
          lineK.dirty = true;
        } else {
          if (tab.curCol > 0 && lineK.chars[tab.curCol] === '') {
            clearWideAt(lineK, tab.curCol);
          }
          lineK.chars.length = Math.min(lineK.chars.length, tab.curCol);
          lineK.styles.length = lineK.chars.length;
          lineK.dirty = true;
        }
        return;
      }
      if (finalChar === 'P') {
        var lineP = ensureCurrentLine(tab);
        clearWideAt(lineP, tab.curCol);
        if (tab.curCol < lineP.chars.length) {
          lineP.chars.splice(tab.curCol, n);
          lineP.styles.splice(tab.curCol, n);
          lineP.dirty = true;
        }
        return;
      }
      if (finalChar === '@') {
        var lineAt = ensureCurrentLine(tab);
        clearWideAt(lineAt, tab.curCol);
        while (lineAt.chars.length < tab.curCol) {
          lineAt.chars.push(' ');
          lineAt.styles.push('');
        }
        for (var a = 0; a < n; a += 1) {
          lineAt.chars.splice(tab.curCol, 0, ' ');
          lineAt.styles.splice(tab.curCol, 0, '');
        }
        if (lineAt.chars.length > lastCols) {
          lineAt.chars.length = lastCols;
          lineAt.styles.length = lastCols;
        }
        lineAt.dirty = true;
        return;
      }
      if (finalChar === 'X') {
        var lineX = ensureCurrentLine(tab);
        while (lineX.chars.length < tab.curCol + n) {
          lineX.chars.push(' ');
          lineX.styles.push('');
        }
        for (var cx = 0; cx < n; cx += 1) {
          clearWideAt(lineX, tab.curCol + cx);
          lineX.chars[tab.curCol + cx] = ' ';
          lineX.styles[tab.curCol + cx] = '';
        }
        lineX.dirty = true;
        return;
      }
      if (finalChar === 'L') {
        var boundsL = scrollBoundsOf(tab);
        if (tab.curLineIdx >= boundsL.startIdx && tab.curLineIdx <= boundsL.endIdx) {
          ensureLineAt(tab, boundsL.endIdx);
          for (var il = 0; il < n; il += 1) {
            var rmL = tab.lines.splice(Math.min(tab.lines.length - 1, boundsL.endIdx), 1)[0];
            if (tab.tabId === activeTabId && rmL && rmL.node) removedNodes.push(rmL.node);
            tab.lines.splice(tab.curLineIdx, 0, makeLine(false));
          }
          if (tab.tabId === activeTabId) fullRebuild = true;
        }
        return;
      }
      if (finalChar === 'M') {
        var boundsM = scrollBoundsOf(tab);
        if (tab.curLineIdx >= boundsM.startIdx && tab.curLineIdx <= boundsM.endIdx) {
          ensureLineAt(tab, boundsM.endIdx);
          for (var dl = 0; dl < n; dl += 1) {
            var rmM = tab.lines.splice(tab.curLineIdx, 1)[0];
            if (tab.tabId === activeTabId && rmM && rmM.node) removedNodes.push(rmM.node);
            tab.lines.splice(Math.min(tab.lines.length, boundsM.endIdx), 0, makeLine(false));
          }
          if (tab.tabId === activeTabId) fullRebuild = true;
        }
        return;
      }
      if (finalChar === 'r') {
        var rParts = paramsRaw.split(';');
        var topR = parseInt(rParts[0] || '1', 10);
        var botR = parseInt(rParts[1] || String(lastRows), 10);
        if (isNaN(topR) || topR < 1) topR = 1;
        if (isNaN(botR) || botR > lastRows) botR = lastRows;
        if (topR < botR) {
          tab.scrollTop = topR - 1;
          tab.scrollBottom = botR - 1;
        } else {
          tab.scrollTop = 0;
          tab.scrollBottom = -1;
        }
        tab.curLineIdx = viewportTopOf(tab);
        tab.curCol = 0;
        return;
      }
      if (finalChar === 's') {
        tab.savedCursor = { lineIdx: tab.curLineIdx, col: tab.curCol };
        return;
      }
      if (finalChar === 'u' && tab.savedCursor) {
        tab.curLineIdx = Math.max(0, Math.min(tab.lines.length - 1, tab.savedCursor.lineIdx));
        tab.curCol = Math.max(0, tab.savedCursor.col);
      }
    }

    function handleOsc(tab, content) {
      var sep = content.indexOf(';');
      if (sep < 0) return;
      var code = content.slice(0, sep);
      var payload = content.slice(sep + 1);
      if ((code === '0' || code === '2') && payload) {
        tab.title = payload;
        if (tab.tabId === activeTabId) {
          syncActiveTabChrome();
        } else {
          renderTabBar();
        }
      }
    }

    function feedText(tab, text) {
      if (!text) return;
      var stream = tab.pendingSurrogate ? tab.pendingSurrogate + text : text;
      tab.pendingSurrogate = '';
      if (stream.length > 0) {
        var tailCode = stream.charCodeAt(stream.length - 1);
        if (tailCode >= 0xd800 && tailCode <= 0xdbff) {
          tab.pendingSurrogate = stream.charAt(stream.length - 1);
          stream = stream.slice(0, -1);
        }
      }

      for (var i = 0; i < stream.length; i += 1) {
        var code = stream.charCodeAt(i);
        var ch = stream.charAt(i);
        if (code >= 0xd800 && code <= 0xdbff && i + 1 < stream.length) {
          var low = stream.charCodeAt(i + 1);
          if (low >= 0xdc00 && low <= 0xdfff) {
            ch = stream.slice(i, i + 2);
            i += 1;
          }
        }

        if (tab.parseState === 'normal') {
          if (code === 0x1b) {
            tab.parseState = 'esc';
          } else if (ch === '\r') {
            resetBsTracking(tab);
            tab.curCol = 0;
          } else if (ch === '\n') {
            newLine(tab);
          } else if (code === 0x0b || code === 0x0c) {
            lineFeedOnly(tab);
          } else if (code === 0x08) {
            var prevCol = tab.curCol;
            var bsLine = ensureCurrentLine(tab);
            if (prevCol >= 2 && bsLine.chars[prevCol - 1] === '') {
              tab.curCol = prevCol - 2;
            } else {
              tab.curCol = Math.max(0, prevCol - 1);
            }
            if (
              tab.curCol === tab.lastBsSpaceCol &&
              tab.curCol === bsLine.chars.length - 1 &&
              bsLine.chars[tab.curCol] === ' ' &&
              !bsLine.styles[tab.curCol]
            ) {
              bsLine.chars.pop();
              bsLine.styles.pop();
              bsLine.dirty = true;
              resetBsTracking(tab);
            } else {
              tab.lastBsCol = tab.curCol;
              tab.lastBsSpaceCol = -1;
            }
          } else if (ch === '\t') {
            resetBsTracking(tab);
            var nextStop = tab.curCol + (8 - (tab.curCol % 8));
            while (tab.curCol < nextStop) {
              putChar(tab, ' ');
            }
          } else if (code === 0x07) {
            /* BEL 忽略 */
          } else if (code >= 0x20) {
            putChar(tab, ch);
          }
        } else if (tab.parseState === 'esc') {
          if (ch === '[') {
            tab.parseState = 'csi';
            tab.csiBuf = '';
          } else if (ch === ']') {
            tab.parseState = 'osc';
            tab.oscBuf = '';
          } else if (ch === '(' || ch === ')' || ch === '*' || ch === '+') {
            tab.parseState = 'charset';
          } else if (ch === '7') {
            tab.savedCursor = { lineIdx: tab.curLineIdx, col: tab.curCol };
            tab.parseState = 'normal';
          } else if (ch === '8') {
            if (tab.savedCursor) {
              tab.curLineIdx = Math.max(0, Math.min(tab.lines.length - 1, tab.savedCursor.lineIdx));
              tab.curCol = Math.max(0, tab.savedCursor.col);
            }
            tab.parseState = 'normal';
          } else if (ch === 'M') {
            var boundsRv = scrollBoundsOf(tab);
            if (tab.curLineIdx === boundsRv.startIdx) {
              scrollDownInRegion(tab, 1);
            } else if (tab.curLineIdx > 0) {
              tab.curLineIdx -= 1;
            }
            tab.parseState = 'normal';
          } else if (ch === 'c') {
            resetTabBuffer(tab);
            tab.parseState = 'normal';
          } else {
            tab.parseState = 'normal';
          }
        } else if (tab.parseState === 'charset') {
          tab.parseState = 'normal';
        } else if (tab.parseState === 'csi') {
          if (code >= 0x40 && code <= 0x7e) {
            handleCsi(tab, tab.csiBuf, ch);
            tab.parseState = 'normal';
            tab.csiBuf = '';
          } else if (tab.csiBuf.length < 64) {
            tab.csiBuf += ch;
          }
        } else if (tab.parseState === 'osc') {
          if (code === 0x07) {
            handleOsc(tab, tab.oscBuf);
            tab.parseState = 'normal';
            tab.oscBuf = '';
          } else if (code === 0x1b) {
            tab.parseState = 'oscEsc';
          } else if (tab.oscBuf.length < 256) {
            tab.oscBuf += ch;
          }
        } else if (tab.parseState === 'oscEsc') {
          if (ch === '\\') {
            handleOsc(tab, tab.oscBuf);
          }
          tab.parseState = 'normal';
          tab.oscBuf = '';
        }
      }
      if (tab.tabId === activeTabId) {
        scheduleRender();
      }
    }

    /* ── 安全 DOM 渲染（零 innerHTML） ──────────────────────── */

    function appendStyledRange(parent, chars, styles, start, end) {
      if (start >= end) return;
      var segStart = start;
      var segToken = styles[start] || '';
      for (var i = start + 1; i <= end; i += 1) {
        var nextToken = i < end ? styles[i] || '' : null;
        if (nextToken !== segToken) {
          var chunk = chars.slice(segStart, i).join('');
          if (chunk) {
            if (segToken) {
              var parsed = parseStyleToken(segToken);
              var span = document.createElement('span');
              if (parsed.cls) span.className = parsed.cls;
              if (parsed.fgCss) span.style.color = parsed.fgCss;
              if (parsed.bgCss) span.style.backgroundColor = parsed.bgCss;
              span.textContent = chunk;
              parent.appendChild(span);
            } else {
              parent.appendChild(document.createTextNode(chunk));
            }
          }
          segStart = i;
          segToken = nextToken;
        }
      }
    }

    function populateLineNode(div, line, hasCursor, cursorCol) {
      div.textContent = '';
      div.className = line.system ? 'tm-line tm-line--system' : 'tm-line';

      if (line.system) {
        div.textContent = line.chars.join('');
        return;
      }

      var chars = line.chars;
      var styles = line.styles;

      if (!hasCursor) {
        appendStyledRange(div, chars, styles, 0, chars.length);
        return;
      }

      var col = Math.max(0, cursorCol);
      if (col > 0 && col < chars.length && chars[col] === '') {
        col -= 1;
      }
      var cursor = document.createElement('span');
      cursor.className = 'tm-live-cursor';
      cursor.setAttribute('aria-hidden', 'true');

      if (col < chars.length) {
        appendStyledRange(div, chars, styles, 0, col);
        div.appendChild(cursor);
        appendStyledRange(div, chars, styles, col, chars.length);
      } else {
        appendStyledRange(div, chars, styles, 0, chars.length);
        div.appendChild(cursor);
      }
    }

    function renderLineNode(line, hasCursor, cursorCol) {
      var div = document.createElement('div');
      populateLineNode(div, line, hasCursor, cursorCol);
      return div;
    }

    function flushRender() {
      rafId = 0;
      if (!renderPending) return;
      renderPending = false;

      var tab = getActiveTab();
      if (!tab) {
        screenEl.textContent = '';
        return;
      }

      var wasNearBottom =
        screenEl.scrollHeight - screenEl.scrollTop - screenEl.clientHeight <= 48;

      var cursorVisible = Boolean(tab.connected && tab.cursorVisible);
      var cursorLineIdx = cursorVisible ? tab.curLineIdx : -1;
      var lines = tab.lines;

      if (fullRebuild) {
        fullRebuild = false;
        removedNodes.length = 0;
        var frag = document.createDocumentFragment();
        for (var i = 0; i < lines.length; i += 1) {
          var ln = lines[i];
          ln.node = renderLineNode(ln, i === cursorLineIdx, tab.curCol);
          ln.dirty = false;
          frag.appendChild(ln.node);
        }
        screenEl.textContent = '';
        screenEl.appendChild(frag);
      } else {
        for (var r = 0; r < removedNodes.length; r += 1) {
          var rn = removedNodes[r];
          if (rn && rn.parentNode === screenEl) {
            screenEl.removeChild(rn);
          }
        }
        removedNodes.length = 0;

        if (
          prevCursorVisible !== cursorVisible ||
          prevCursorLineIdx !== cursorLineIdx ||
          prevCursorCol !== tab.curCol
        ) {
          if (prevCursorLineIdx >= 0 && prevCursorLineIdx < lines.length) {
            lines[prevCursorLineIdx].dirty = true;
          }
          if (cursorLineIdx >= 0 && cursorLineIdx < lines.length) {
            lines[cursorLineIdx].dirty = true;
          }
        }

        for (var j = 0; j < lines.length; j += 1) {
          var line = lines[j];
          var isCur = j === cursorLineIdx;
          if (!line.node) {
            line.node = renderLineNode(line, isCur, tab.curCol);
            line.dirty = false;
            if (j === 0) {
              screenEl.insertBefore(line.node, screenEl.firstChild);
            } else {
              var prevNode = lines[j - 1].node;
              screenEl.insertBefore(line.node, prevNode ? prevNode.nextSibling : null);
            }
          } else if (line.dirty) {
            populateLineNode(line.node, line, isCur, tab.curCol);
            line.dirty = false;
          }
        }
      }

      prevCursorVisible = cursorVisible;
      prevCursorLineIdx = cursorLineIdx;
      prevCursorCol = tab.curCol;

      if (wasNearBottom) {
        screenEl.scrollTop = screenEl.scrollHeight;
      }
    }

    function scheduleRender() {
      renderPending = true;
      if (!rafId) {
        rafId = window.requestAnimationFrame(flushRender);
      }
    }

    /* ── 网格列/行尺寸测量 ──────────────────────────────────── */

    function measureCell() {
      var probe = document.createElement('span');
      probe.style.position = 'absolute';
      probe.style.visibility = 'hidden';
      probe.style.whiteSpace = 'pre';
      probe.textContent = 'MMMMMMMMMM';
      screenEl.appendChild(probe);
      var rect = probe.getBoundingClientRect();
      screenEl.removeChild(probe);
      var charW = rect.width > 0 ? rect.width / 10 : 7.8;
      var lineH = rect.height > 0 ? rect.height : 19;
      return { charW: charW, lineH: lineH };
    }

    function computeGrid() {
      var cell = measureCell();
      var rect = screenEl.getBoundingClientRect();
      var style = window.getComputedStyle(screenEl);
      var padX = (parseFloat(style.paddingLeft) || 0) + (parseFloat(style.paddingRight) || 0);
      var padY = (parseFloat(style.paddingTop) || 0) + (parseFloat(style.paddingBottom) || 0);
      var w = Math.max(0, rect.width - padX);
      var h = Math.max(0, rect.height - padY);
      return {
        cols: Math.min(512, Math.max(40, Math.floor(w / cell.charW))),
        rows: Math.min(512, Math.max(12, Math.floor(h / cell.lineH)))
      };
    }

    /* ── 键盘与剪贴板输入 ───────────────────────────────────── */

    function sendInputToTab(tab, data) {
      if (!tab || !tab.connected || tab.termId == null || !data) return;
      Bridge.terminalInput(tab.termId, data).catch(function () {});
    }

    function sendPasteText(tab, text) {
      if (!text) return;
      var payload = tab && tab.bracketedPaste ? '\x1b[200~' + text + '\x1b[201~' : text;
      sendInputToTab(tab, payload);
    }

    function copyTerminalSelection() {
      var sel = window.getSelection ? window.getSelection().toString() : '';
      if (!sel) return false;
      if (typeof Bridge.copyText === 'function') {
        Bridge.copyText(sel).catch(function () {});
        return true;
      }
      if (navigator.clipboard && typeof navigator.clipboard.writeText === 'function') {
        navigator.clipboard.writeText(sel).catch(function () {});
        return true;
      }
      return false;
    }

    function mapKeyEvent(e) {
      if (e.metaKey) return null;

      if (FKEY_SEQ[e.key]) {
        return FKEY_SEQ[e.key];
      }

      if (e.shiftKey && e.key === 'Tab') {
        return '\x1b[Z';
      }

      if (e.ctrlKey && !e.altKey && e.key && e.key.length === 1) {
        var upper = e.key.toUpperCase();
        var cc = upper.charCodeAt(0);
        if (cc >= 65 && cc <= 90) {
          return String.fromCharCode(cc - 64);
        }
        if (e.key === '[') return '\x1b';
        if (e.key === '\\') return '\x1c';
        if (e.key === ']') return '\x1d';
        return null;
      }

      if (e.altKey && !e.ctrlKey && e.key && e.key.length === 1) {
        return '\x1b' + e.key;
      }

      if (e.key === 'Enter') return '\r';
      if (e.key === 'Backspace') return '\x7f';
      if (e.key === 'Tab') return '\t';
      if (e.key === 'Escape') return '\x1b';
      if (e.key === 'ArrowUp') return '\x1b[A';
      if (e.key === 'ArrowDown') return '\x1b[B';
      if (e.key === 'ArrowRight') return '\x1b[C';
      if (e.key === 'ArrowLeft') return '\x1b[D';
      if (e.key === 'Home') return '\x1b[H';
      if (e.key === 'End') return '\x1b[F';
      if (e.key === 'Delete') return '\x1b[3~';
      if (e.key === 'PageUp') return '\x1b[5~';
      if (e.key === 'PageDown') return '\x1b[6~';

      if (e.key && e.key.length === 1 && !e.ctrlKey && !e.altKey) {
        return e.key;
      }
      return null;
    }

    function onKeyDown(e) {
      var tab = getActiveTab();
      if (e.metaKey && (e.key || '').toLowerCase() === 'a') {
        e.preventDefault();
        if (window.getSelection && document.createRange) {
          var range = document.createRange();
          range.selectNodeContents(screenEl);
          var sel = window.getSelection();
          if (sel) {
            sel.removeAllRanges();
            sel.addRange(range);
          }
        }
        return;
      }

      if (e.ctrlKey && e.shiftKey && !e.metaKey && !e.altKey) {
        var upperCtrl = (e.key || '').toUpperCase();
        if (upperCtrl === 'C') {
          if (copyTerminalSelection()) {
            e.preventDefault();
            return;
          }
        } else if (upperCtrl === 'V') {
          e.preventDefault();
          if (navigator.clipboard && typeof navigator.clipboard.readText === 'function' && tab && tab.connected) {
            navigator.clipboard.readText().then(function (txt) {
              sendPasteText(tab, txt);
            }).catch(function () {});
          }
          return;
        }
      }

      if (!tab || !tab.connected || tab.termId == null) return;
      if (e.isComposing || isComposing || e.keyCode === 229) return;

      var seq = mapKeyEvent(e);
      if (seq != null) {
        e.preventDefault();
        sendInputToTab(tab, seq);
        if (imeEl) imeEl.value = '';
      }
    }

    function onPaste(e) {
      var tab = getActiveTab();
      if (!tab || !tab.connected || tab.termId == null) return;
      var clip = e.clipboardData || window.clipboardData;
      var text = clip ? clip.getData('text') : '';
      if (text) {
        e.preventDefault();
        sendPasteText(tab, text);
        if (imeEl) imeEl.value = '';
      }
    }

    screenEl.addEventListener('keydown', onKeyDown);
    screenEl.addEventListener('paste', onPaste);
    screenEl.addEventListener('mouseup', function () {
      var tab = getActiveTab();
      var sel = window.getSelection ? window.getSelection().toString() : '';
      if (!sel && tab && tab.connected) {
        focusTerminal();
      }
    });

    if (imeEl) {
      imeEl.addEventListener('keydown', onKeyDown);
      imeEl.addEventListener('paste', onPaste);
      imeEl.addEventListener('compositionstart', function () {
        isComposing = true;
      });
      imeEl.addEventListener('compositionend', function (e) {
        isComposing = false;
        var data = e.data || imeEl.value || '';
        imeEl.value = '';
        var tab = getActiveTab();
        if (data && tab) {
          sendInputToTab(tab, data);
        }
      });
      imeEl.addEventListener('input', function () {
        if (isComposing) return;
        var val = imeEl.value;
        if (val) {
          imeEl.value = '';
          var tab = getActiveTab();
          if (tab) sendInputToTab(tab, val);
        }
      });
    }

    /* ── 多标签控制与打开/重连 ──────────────────────────────── */

    function connectTab(tab) {
      if (!tab || tab.opening) return;
      if (!isSessionReady()) {
        showNotice(
          sessionState.watchOnly
            ? '当前为只读观看模式，无法使用远程终端'
            : sessionState.active
              ? '正在建立远控连接…'
              : '请先建立远控会话',
          isSessionReady()
        );
        syncActiveTabChrome();
        return;
      }

      if (tab.termId != null) {
        var prevId = tab.termId;
        staleTermIds[prevId] = true;
        tab.termId = null;
        tab.connected = false;
        Bridge.terminalClose(prevId).catch(function () {});
      }

      var mySeq = ++tab.connectSeq;
      tab.opening = true;
      tab.connected = false;
      hideNotice();
      resetTabBuffer(tab);
      syncActiveTabChrome();

      var grid = computeGrid();
      lastCols = grid.cols;
      lastRows = grid.rows;

      Bridge.terminalOpen(grid.cols, grid.rows).then(
        function (termId) {
          if (mySeq !== tab.connectSeq || tabs.indexOf(tab) < 0) {
            if (termId != null) {
              var orphanId = Number(termId);
              staleTermIds[orphanId] = true;
              Bridge.terminalClose(orphanId).catch(function () {});
            }
            return;
          }
          if (tab.opening && tab.termId == null && termId != null) {
            tab.termId = Number(termId);
          }
        },
        function (err) {
          if (mySeq !== tab.connectSeq || tabs.indexOf(tab) < 0) return;
          tab.opening = false;
          tab.connected = false;
          tab.termId = null;
          syncActiveTabChrome();
          if (tab.tabId === activeTabId) {
            showNotice(errMessage(err), isSessionReady());
            scheduleRender();
          }
        }
      );
    }

    function openNewTab() {
      if (!isSessionReady() || tabs.length >= MAX_TABS) return;
      var tab = createTab('终端 ' + (tabs.length + 1));
      tabs.push(tab);
      activeTabId = tab.tabId;
      fullRebuild = true;
      connectTab(tab);
    }

    function switchTab(tabId) {
      if (tabId === activeTabId) {
        focusTerminal();
        return;
      }
      var target = null;
      for (var i = 0; i < tabs.length; i += 1) {
        if (tabs[i].tabId === tabId) {
          target = tabs[i];
          break;
        }
      }
      if (!target) return;
      activeTabId = target.tabId;
      target.lines.forEach(function (ln) {
        ln.node = null;
        ln.dirty = true;
      });
      removedNodes = [];
      fullRebuild = true;
      prevCursorLineIdx = -1;
      prevCursorCol = -1;
      prevCursorVisible = false;
      hideNotice();
      syncActiveTabChrome();
      scheduleRender();
      if (target.connected && target.termId != null) {
        var grid = computeGrid();
        if (grid.cols !== lastCols || grid.rows !== lastRows) {
          lastCols = grid.cols;
          lastRows = grid.rows;
          Bridge.terminalResize(target.termId, lastCols, lastRows).catch(function () {});
        }
        focusTerminal();
      }
    }

    function closeTab(tabId) {
      var idx = -1;
      for (var i = 0; i < tabs.length; i += 1) {
        if (tabs[i].tabId === tabId) {
          idx = i;
          break;
        }
      }
      if (idx < 0) return;
      var tab = tabs[idx];
      tab.connectSeq += 1;
      if (tab.termId != null) {
        var closingId = tab.termId;
        staleTermIds[closingId] = true;
        tab.termId = null;
        Bridge.terminalClose(closingId).catch(function () {});
      }
      tabs.splice(idx, 1);
      if (activeTabId === tabId) {
        var fallback = tabs[Math.min(idx, tabs.length - 1)] || null;
        activeTabId = fallback ? fallback.tabId : null;
        if (fallback) {
          fallback.lines.forEach(function (ln) {
            ln.node = null;
            ln.dirty = true;
          });
        }
        fullRebuild = true;
        prevCursorLineIdx = -1;
        prevCursorCol = -1;
        prevCursorVisible = false;
      }
      if (!tabs.length) {
        screenEl.textContent = '';
        if (isSessionReady()) {
          showNotice('终端标签已全部关闭，点击下方按钮新建', true);
        } else if (sessionState.watchOnly) {
          showNotice('当前为只读观看模式，无法使用远程终端', false);
        } else if (sessionState.active && !sessionState.connected) {
          showNotice('正在建立远控连接…', false);
        } else {
          showNotice(lastEndedReason || '请先建立远控会话', false);
        }
      } else {
        hideNotice();
      }
      syncActiveTabChrome();
      scheduleRender();
    }

    if (newTabBtn) {
      newTabBtn.addEventListener('click', openNewTab);
    }

    function reconnectCurrentOrOpen() {
      var tab = getActiveTab();
      if (tab) {
        connectTab(tab);
      } else {
        openNewTab();
      }
    }

    if (reconnectBtn) {
      reconnectBtn.addEventListener('click', reconnectCurrentOrOpen);
    }
    if (noticeReconnectBtn) {
      noticeReconnectBtn.addEventListener('click', reconnectCurrentOrOpen);
    }

    /* ── 远端事件与会话状态处理 ─────────────────────────────── */

    function handleTerminalEvent(ev) {
      if (!ev || !ev.type) return;

      if (ev.type === 'opened') {
        if (staleTermIds[ev.termId]) {
          delete staleTermIds[ev.termId];
          return;
        }
        var tab = null;
        for (var i = 0; i < tabs.length; i += 1) {
          var t = tabs[i];
          if (t.opening && (t.termId == null || t.termId === ev.termId)) {
            tab = t;
            break;
          }
        }
        if (!tab) return;
        tab.opening = false;
        if (!ev.ok) {
          tab.connected = false;
          tab.termId = null;
          syncActiveTabChrome();
          if (tab.tabId === activeTabId) {
            showNotice(ev.error || '无法打开远程终端', isSessionReady());
            scheduleRender();
          }
          return;
        }
        tab.termId = ev.termId;
        tab.connected = true;
        if (ev.title) tab.title = String(ev.title);
        if (ev.cwd) tab.cwd = String(ev.cwd);
        if (tab.tabId === activeTabId) {
          hideNotice();
          scheduleRender();
          focusTerminal();
        }
        syncActiveTabChrome();

        var latest = computeGrid();
        lastCols = latest.cols;
        lastRows = latest.rows;
        Bridge.terminalResize(tab.termId, lastCols, lastRows).catch(function () {});
        return;
      }

      if (ev.type === 'output') {
        var outTab = findTabByTermId(ev.termId);
        if (!outTab) return;
        var text;
        if (outTab.utf8Decoder && Array.isArray(ev.bytes)) {
          text = outTab.utf8Decoder.decode(new Uint8Array(ev.bytes), { stream: true });
        } else if (outTab.utf8Decoder && ev.bytes instanceof Uint8Array) {
          text = outTab.utf8Decoder.decode(ev.bytes, { stream: true });
        } else {
          text = ev.textUtf8Lossy || '';
        }
        feedText(outTab, text);
        return;
      }

      if (ev.type === 'exited') {
        if (staleTermIds[ev.termId]) {
          delete staleTermIds[ev.termId];
          return;
        }
        var exTab = findTabByTermId(ev.termId);
        if (!exTab) return;
        if (exTab.utf8Decoder) {
          var tail = exTab.utf8Decoder.decode();
          if (tail) feedText(exTab, tail);
        }
        exTab.opening = false;
        exTab.connected = false;
        exTab.termId = null;
        appendSystemLine(exTab, '[进程已退出，退出码 ' + String(ev.code) + ']');
        syncActiveTabChrome();
      }
    }

    function applySessionInfo(raw) {
      var s = raw && typeof raw === 'object' ? raw : {};
      var wasReady = isSessionReady();
      sessionState.active = Boolean(s.active);
      sessionState.connected = Boolean(s.connected);
      sessionState.watchOnly = Boolean(s.watchOnly !== undefined ? s.watchOnly : s.watch_only);
      sessionState.deviceName = String(s.deviceName || s.device_name || s.target || '');

      if (!sessionState.active) {
        var anyHadActive = false;
        tabs.forEach(function (t) {
          if (t.connected || t.opening || t.termId != null) anyHadActive = true;
          t.connectSeq += 1;
          t.opening = false;
          t.connected = false;
          t.termId = null;
        });
        if (anyHadActive) {
          var curActiveTab = getActiveTab();
          if (curActiveTab) {
            appendSystemLine(curActiveTab, '[远控会话已断开]');
          }
        } else if (!tabs.length) {
          showNotice(lastEndedReason || '请先建立远控会话', false);
        }
        syncActiveTabChrome();
        return;
      }

      lastEndedReason = '';

      if (sessionState.watchOnly) {
        tabs.forEach(function (t) {
          t.connectSeq += 1;
          t.opening = false;
          t.connected = false;
          t.termId = null;
        });
        showNotice('当前为只读观看模式，无法使用远程终端', false);
        syncActiveTabChrome();
        return;
      }

      if (!sessionState.connected) {
        if (!tabs.length) {
          showNotice('正在建立远控连接…', false);
        }
        syncActiveTabChrome();
        return;
      }

      if (!wasReady) {
        resetSessionTimer();
      }
      var hasOpenOrOpening = tabs.some(function (t) {
        return t.connected || t.opening;
      });
      if (!tabs.length) {
        openNewTab();
      } else if (!hasOpenOrOpening) {
        var cur = getActiveTab() || tabs[0];
        activeTabId = cur.tabId;
        connectTab(cur);
      } else {
        hideNotice();
        syncActiveTabChrome();
      }
    }

    function handleSessionUpdate(upd) {
      if (!upd || !upd.kind) return;
      if (upd.kind === 'ended') {
        var reason = (upd.data && upd.data.reason) || '远控会话已断开';
        lastEndedReason = reason;
        sessionState.active = false;
        sessionState.connected = false;
        sessionState.watchOnly = false;
        tabs.forEach(function (t) {
          t.connectSeq += 1;
          t.opening = false;
          t.connected = false;
          t.termId = null;
        });
        if (tabs.length > 0) {
          var activeTab = getActiveTab() || tabs[0];
          activeTabId = activeTab.tabId;
          appendSystemLine(activeTab, '[' + reason + ']');
        } else {
          showNotice(reason, false);
          scheduleRender();
        }
        syncActiveTabChrome();
        return;
      }
      if (upd.kind === 'negotiated') {
        var data = upd.data || {};
        var wasReady = isSessionReady();
        lastEndedReason = '';
        sessionState.active = true;
        sessionState.connected = true;
        if (typeof data.watch_only === 'boolean') {
          sessionState.watchOnly = data.watch_only;
        }
        if (data.device_name || data.device) {
          sessionState.deviceName = String(data.device_name || data.device);
        }
        if (sessionState.watchOnly) {
          showNotice('当前为只读观看模式，无法使用远程终端', false);
          syncActiveTabChrome();
          return;
        }
        if (!wasReady) resetSessionTimer();
        var hasLive = tabs.some(function (t) {
          return t.connected || t.opening;
        });
        if (!tabs.length) {
          openNewTab();
        } else if (!hasLive) {
          var targetTab = getActiveTab() || tabs[0];
          activeTabId = targetTab.tabId;
          connectTab(targetTab);
        } else {
          syncActiveTabChrome();
        }
      }
    }

    /* ── ResizeObserver（100ms 防抖） ───────────────────────── */

    function handleResize() {
      var grid = computeGrid();
      if (grid.cols === lastCols && grid.rows === lastRows) return;
      lastCols = grid.cols;
      lastRows = grid.rows;
      tabs.forEach(function (tab) {
        if (tab.inAltScreen) {
          while (tab.lines.length > lastRows) {
            var popped = tab.lines.pop();
            if (tab.tabId === activeTabId && popped && popped.node) {
              removedNodes.push(popped.node);
            }
          }
          while (tab.lines.length < lastRows) {
            tab.lines.push(makeLine(false));
          }
          tab.curLineIdx = Math.max(0, Math.min(lastRows - 1, tab.curLineIdx));
          tab.curCol = Math.max(0, Math.min(lastCols - 1, tab.curCol));
          if (tab.scrollTop >= lastRows) {
            tab.scrollTop = 0;
            tab.scrollBottom = -1;
          }
          if (tab.tabId === activeTabId) {
            fullRebuild = true;
            scheduleRender();
          }
        } else {
          tab.curCol = Math.max(0, Math.min(lastCols - 1, tab.curCol));
          if (tab.scrollTop >= lastRows) {
            tab.scrollTop = 0;
            tab.scrollBottom = -1;
          }
        }
        if (tab.connected && tab.termId != null) {
          Bridge.terminalResize(tab.termId, grid.cols, grid.rows).catch(function () {});
        }
      });
    }

    if (typeof window.ResizeObserver === 'function') {
      var ro = new window.ResizeObserver(function () {
        window.clearTimeout(resizeTimer);
        resizeTimer = window.setTimeout(handleResize, RESIZE_DEBOUNCE_MS);
      });
      ro.observe(screenEl);
    } else {
      window.addEventListener('resize', function () {
        window.clearTimeout(resizeTimer);
        resizeTimer = window.setTimeout(handleResize, RESIZE_DEBOUNCE_MS);
      });
    }

    /* ── 窗口关闭清理所有活动 PTY ───────────────────────────── */

    window.addEventListener('beforeunload', function () {
      tabs.forEach(function (tab) {
        if (tab.termId != null) {
          Bridge.terminalClose(tab.termId).catch(function () {});
        }
      });
    });

    /* 先注册事件监听再拉取当前会话信息，彻底消除首包竞态 */
    Promise.all([
      Promise.resolve(Bridge.onTerminalEvent(handleTerminalEvent)).catch(function () {}),
      Promise.resolve(
        typeof Bridge.onSessionUpdate === 'function'
          ? Bridge.onSessionUpdate(handleSessionUpdate)
          : null
      ).catch(function () {}),
      Promise.resolve(
        typeof Bridge.onActiveSessionState === 'function'
          ? Bridge.onActiveSessionState(applySessionInfo)
          : null
      ).catch(function () {})
    ]).then(function () {
      if (typeof Bridge.getActiveSessionInfo === 'function') {
        Bridge.getActiveSessionInfo()
          .then(applySessionInfo)
          .catch(function () {
            openNewTab();
          });
      } else {
        sessionState.active = true;
        sessionState.connected = true;
        openNewTab();
      }
    });
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
