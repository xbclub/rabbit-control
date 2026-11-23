/* transfer.js —— W2 文件传输 / §6 W2 + §6.0 mock + §7 交互 + Tauri 真实双向文件传输
 *
 * 浏览器预览（!IS_TAURI）：
 *   mock 数据逐字取自 docs/reference/design/08-ui-design.md §6.0，行为 100% 保持。
 * Tauri 真窗口（IS_TAURI）：
 *   接入 RCBridge 本地/远端目录浏览、新建文件夹、上传/下载、实时进度与取消/清理队列。
 * 安全纪律：所有动态文件名/路径/错误信息均经 escapeHtml() 严格转义。
 */

(function () {
  'use strict';

  var Bridge = window.RCBridge;
  var IS_TAURI = Boolean(Bridge && Bridge.isTauri);

  /* ── 严格 HTML 转义（防文件系统名/路径注入） ──────────────── */

  function escapeHtml(value) {
    return String(value == null ? '' : value)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
      .replace(/'/g, '&#39;');
  }

  /* ── §6.0 mock 数据（仅 !IS_TAURI 浏览器静态预览使用） ──────── */

  var MOCK_LOCAL_DIR = '/Users/yii/Downloads';
  var MOCK_REMOTE_DIR = '/Users/yii/Documents/mycode/data';

  var MOCK_LOCAL_FILES = [
    { name: 'openssl-3.5.tar.gz', date: '2026年11月21日', type: 'GZip 归档', size: '9.44 MB', folder: false },
    { name: 'Antigravity.dmg', date: '2026年11月21日', type: '磁盘映像', size: '225.02 MB', folder: false },
    { name: 'GUI-Server.exe', date: '2026年11月19日', type: 'Windows 程序', size: '24.35 MB', folder: false },
    { name: 'logs', date: '2026年11月19日', type: '文件夹', size: '--', folder: true },
    { name: 'logs.zip', date: '2026年11月19日', type: 'Zip 归档', size: '7.44 MB', folder: false },
    { name: 'expo-report.json', date: '2026年11月18日', type: 'JSON', size: '1.24 KB', folder: false },
    { name: 'notes.md', date: '2026年11月18日', type: 'Markdown', size: '3.10 KB', folder: false },
    { name: 'dump-1121.log', date: '2026年11月17日', type: '日志', size: '56.72 MB', folder: false }
  ];

  var MOCK_REMOTE_FILES = [
    { name: 'BiliBiliLiveRecorder.7z', date: '2026年11月07日', type: '7z', size: '10.80 MB', folder: false },
    { name: 'blivedm-go', date: '2026年11月05日', type: '文件夹', size: '--', folder: true },
    { name: 'browserclient', date: '2026年11月05日', type: '文件夹', size: '--', folder: true },
    { name: 'danmaku-coder.data', date: '2026年11月06日', type: 'data', size: '21.29 MB', folder: false },
    { name: 'danmaku-renderer.exe', date: '2026年11月07日', type: 'exe', size: '15.85 MB', folder: false },
    { name: 'docs', date: '2026年11月07日', type: '文件夹', size: '--', folder: true },
    { name: 'README.md', date: '2026年11月07日', type: 'Markdown', size: '2.42 KB', folder: false },
    { name: 'stream-config.toml', date: '2026年11月06日', type: 'TOML', size: '1.05 KB', folder: false }
  ];

  var MOCK_QUEUE = [
    {
      name: 'BiliBiliLiveRecorder-gui-master.7z',
      direction: 'upload',
      status: 'done',
      size: '10.80 MB',
      from: '/Users/yii/Downloads/…ter.7z',
      to: '\\Users\\yii\\data\\…coder'
    },
    {
      name: 'Stream-ug.na…22:11:18.har',
      direction: 'upload',
      status: 'done',
      size: '6.94 KB',
      from: '/Users/yii/Downloads/…18.har',
      to: '\\Users\\yii\\Downloads'
    },
    {
      name: 'danmaku-coder.data',
      direction: 'upload',
      status: 'active',
      size: '21.29 MB',
      progress: 47,
      from: '/Users/yii/Downloads/…data',
      to: '\\Users\\yii\\data'
    },
    {
      name: 'GUI-Server.exe',
      direction: 'upload',
      status: 'failed',
      size: '24.35 MB',
      from: '/Users/yii/Downloads/…exe',
      to: '\\Users\\yii\\data',
      hint: '连接中断'
    }
  ];

  /* 运行态状态 */
  var REMOTE_REQ_TIMEOUT_MS = 15000;
  var remoteSession = {
    active: !IS_TAURI,
    connected: !IS_TAURI,
    watchOnly: false,
    deviceName: ''
  };
  var queue = IS_TAURI ? [] : MOCK_QUEUE.slice();
  var panes = {
    local: {
      side: 'local',
      el: null,
      dir: IS_TAURI ? '' : MOCK_LOCAL_DIR,
      files: IS_TAURI ? [] : MOCK_LOCAL_FILES.slice(),
      history: [],
      reqSeq: 0,
      loading: false,
      error: '',
      mkdirOpen: false,
      editingPath: false,
      sortBy: null,
      sortAsc: true
    },
    remote: {
      side: 'remote',
      el: null,
      dir: IS_TAURI ? '' : MOCK_REMOTE_DIR,
      files: IS_TAURI ? [] : MOCK_REMOTE_FILES.slice(),
      history: [],
      reqSeq: 0,
      pendingRemoteResponses: 0,
      pendingHistoryDir: null,
      timeoutId: 0,
      mkdirTimeoutId: 0,
      loading: false,
      error: '',
      mkdirOpen: false,
      editingPath: false,
      sortBy: null,
      sortAsc: true
    }
  };

  /* ── 内联 Lucide 图标（离线可跑，不引 CDN） ──────────────── */

  var ICON = {
    file: '<svg class="ico" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z"/><path d="M14 2v4a2 2 0 0 0 2 2h4"/></svg>',
    folder: '<svg class="ico" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/></svg>',
    arrowLeft: '<svg class="ico" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="m12 19-7-7 7-7"/><path d="M19 12H5"/></svg>',
    arrowUp: '<svg class="ico" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="m5 12 7-7 7 7"/><path d="M12 19V5"/></svg>',
    rotateCw: '<svg class="ico" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M21 12a9 9 0 1 1-9-9c2.52 0 4.93 1 6.74 2.74L21 8"/><path d="M21 3v5h-5"/></svg>',
    folderPlus: '<svg class="ico" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M12 10v6"/><path d="M9 13h6"/><path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/></svg>',
    chevronDown: '<svg class="ico" width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="m6 9 6 6 6-6"/></svg>',
    check: '<svg class="ico" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M20 6 9 17l-5-5"/></svg>',
    x: '<svg class="ico" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M18 6 6 18"/><path d="m6 6 12 12"/></svg>',
    trash: '<svg class="ico" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M3 6h18"/><path d="M19 6v14c0 1-1 2-2 2H7c-1 0-2-1-2-2V6"/><path d="M8 6V4c0-1 1-2 2-2h4c1 0 2 1 2 2v2"/><line x1="10" x2="10" y1="11" y2="17"/><line x1="14" x2="14" y1="11" y2="17"/></svg>'
  };

  /* ── Toast（§4 / §7.4） ─────────────────────────────────── */

  var toastEl = document.getElementById('toast');
  var toastTimer = 0;

  function toast(message) {
    if (!toastEl) return;
    toastEl.textContent = String(message == null ? '' : message);
    toastEl.classList.add('is-visible');
    window.clearTimeout(toastTimer);
    toastTimer = window.setTimeout(function () {
      toastEl.classList.remove('is-visible');
    }, 1800);
  }

  /* ── 数据格式化辅助 ─────────────────────────────────────── */

  function pad2(n) {
    return n < 10 ? '0' + n : String(n);
  }

  function formatBytes(bytes) {
    var n = Number(bytes);
    if (!isFinite(n) || n < 0) return '--';
    if (n < 1024) return Math.round(n) + ' B';
    if (n < 1024 * 1024) return (n / 1024).toFixed(2) + ' KB';
    if (n < 1024 * 1024 * 1024) return (n / (1024 * 1024)).toFixed(2) + ' MB';
    return (n / (1024 * 1024 * 1024)).toFixed(2) + ' GB';
  }

  function formatDate(modifiedSec) {
    var sec = Number(modifiedSec);
    if (!isFinite(sec) || sec <= 0) return '--';
    var d = new Date(sec * 1000);
    if (isNaN(d.getTime())) return '--';
    return d.getFullYear() + '年' + pad2(d.getMonth() + 1) + '月' + pad2(d.getDate()) + '日';
  }

  function formatKind(entry) {
    if (entry.isDir) return '文件夹';
    var kind = String(entry.kind || '').toLowerCase();
    var map = {
      dir: '文件夹',
      image: '图片',
      video: '视频',
      audio: '音频',
      archive: '归档',
      code: '代码',
      text: '文本',
      doc: '文档',
      pdf: 'PDF',
      app: '程序',
      file: '文件'
    };
    return map[kind] || (entry.kind ? String(entry.kind) : '文件');
  }

  function mapEntries(entries) {
    if (!Array.isArray(entries)) return [];
    return entries.map(function (e) {
      return {
        name: String(e.name || ''),
        path: String(e.path || ''),
        folder: Boolean(e.isDir),
        isDir: Boolean(e.isDir),
        modifiedSec: Number(e.modifiedSec || 0),
        date: formatDate(e.modifiedSec),
        type: formatKind(e),
        size: e.isDir ? '--' : formatBytes(e.size),
        rawSize: Number(e.size || 0)
      };
    });
  }

  function applyPaneSort(pane) {
    if (!pane || !pane.sortBy || !Array.isArray(pane.files)) return;
    var key = pane.sortBy;
    var dirFactor = pane.sortAsc ? 1 : -1;
    pane.files.sort(function (a, b) {
      if (Boolean(a.folder) !== Boolean(b.folder)) {
        return a.folder ? -1 : 1;
      }
      var cmp = 0;
      if (key === 'size') {
        cmp = (Number(a.rawSize || 0) - Number(b.rawSize || 0));
      } else if (key === 'date') {
        if (a.modifiedSec != null || b.modifiedSec != null) {
          cmp = Number(a.modifiedSec || 0) - Number(b.modifiedSec || 0);
        } else {
          cmp = String(a.date || '').localeCompare(String(b.date || ''), 'zh-Hans-CN');
        }
      } else if (key === 'type') {
        cmp = String(a.type || '').localeCompare(String(b.type || ''), 'zh-Hans-CN');
      } else {
        cmp = String(a.name || '').localeCompare(String(b.name || ''), 'zh-Hans-CN');
      }
      if (cmp === 0) {
        cmp = String(a.name || '').localeCompare(String(b.name || ''), 'zh-Hans-CN');
      }
      return cmp * dirFactor;
    });
  }

  function isRemoteReady() {
    if (!IS_TAURI) return true;
    return Boolean(remoteSession.active && remoteSession.connected && !remoteSession.watchOnly);
  }

  function parentDir(dirPath) {
    if (!dirPath) return '';
    var s = String(dirPath);
    /* Windows 盘符根路径如 C:\ / C:/ / C: */
    if (/^[A-Za-z]:[\\/]?$/.test(s)) {
      return s;
    }
    /* Windows UNC 共享根路径如 \\server\share 或 \\server\share\ */
    if (/^(?:\\\\|\/\/)[^\\/]+[\\/][^\\/]+[\\/]?$/.test(s)) {
      return s;
    }
    var trimmed = s.replace(/[\\/]+$/, '');
    if (!trimmed) return '/';
    var slashIdx = Math.max(trimmed.lastIndexOf('/'), trimmed.lastIndexOf('\\'));
    if (slashIdx < 0) return trimmed;
    if (slashIdx === 0) return '/';
    var parent = trimmed.slice(0, slashIdx);
    if (/^[A-Za-z]:$/.test(parent)) {
      return parent + (trimmed.charAt(slashIdx) === '/' ? '/' : '\\');
    }
    return parent;
  }

  function errMessage(err) {
    if (!err) return '操作失败';
    if (typeof err === 'string') return err;
    if (err.message) return String(err.message);
    return String(err);
  }

  function mapTransferItem(item) {
    var total = Number(item.totalBytes || 0);
    var done = Number(item.doneBytes || 0);
    var pct = total > 0 ? Math.min(100, Math.round((done * 100) / total)) : (item.status === 'done' ? 100 : 0);
    return {
      id: Number(item.id),
      name: String(item.name || ''),
      direction: item.direction === 'download' ? 'download' : 'upload',
      status: String(item.status || 'active'),
      totalBytes: total,
      doneBytes: done,
      size: formatBytes(total),
      progress: pct,
      from: String(item.fromPath || ''),
      to: String(item.toPath || ''),
      hint: item.error ? String(item.error) : ''
    };
  }

  /* ── 双栏文件浏览器渲染 ─────────────────────────────────── */

  function renderPane(pane) {
    var paneEl = pane.el;
    if (!paneEl) return;

    applyPaneSort(pane);
    var files = pane.files || [];
    var dirDisplay = pane.dir || (pane.loading ? '加载中…' : '--');
    var escDir = escapeHtml(dirDisplay);
    var paneEnabled = pane.side === 'local' || isRemoteReady();

    var backAttr = IS_TAURI
      ? 'data-action="back"' + (paneEnabled && pane.history.length ? '' : ' disabled')
      : 'data-toast';
    var upAttr = IS_TAURI
      ? 'data-action="up"' + (paneEnabled && pane.dir && parentDir(pane.dir) !== pane.dir ? '' : ' disabled')
      : 'data-toast';
    var refreshAttr = IS_TAURI
      ? 'data-action="refresh"' + (paneEnabled ? '' : ' disabled')
      : 'data-toast';
    var mkdirAttr = IS_TAURI
      ? 'data-action="mkdir"' + (paneEnabled && pane.dir ? '' : ' disabled')
      : 'data-toast';

    var pathWidget;
    if (IS_TAURI && pane.editingPath) {
      pathWidget =
        '<form class="tr-path" data-path-form="' + escapeHtml(pane.side) + '" style="flex:1;padding:0;border:0;background:transparent;display:flex;gap:6px;">' +
        '<input type="text" class="tr-path" data-path-input autocomplete="off" spellcheck="false" value="' + escDir + '" aria-label="输入目录路径" style="cursor:text;flex:1;" />' +
        '<button type="submit" class="btn btn--primary" style="height:28px;padding:0 10px;font-size:var(--size-secondary);">前往</button>' +
        '<button type="button" class="btn btn--secondary" data-action="path-cancel" style="height:28px;padding:0 8px;font-size:var(--size-secondary);">取消</button>' +
        '</form>';
    } else {
      var pathAttr = IS_TAURI
        ? (paneEnabled ? 'data-action="edit-path"' : 'disabled')
        : 'data-toast';
      pathWidget =
        '<button type="button" class="tr-path" ' + pathAttr + ' aria-label="当前路径（点击可编辑跳转）" title="' + escDir + '">' +
        '<span class="tr-path__icon">' + ICON.folder + '</span>' +
        '<span class="tr-path__value">' + escDir + '</span>' +
        '<span class="tr-path__chevron">' + ICON.chevronDown + '</span>' +
        '</button>';
    }

    var tools =
      '<div class="tr-pane__tools">' +
      '<button type="button" class="btn btn--ghost" ' + backAttr + ' aria-label="返回" title="返回">' + ICON.arrowLeft + '</button>' +
      '<button type="button" class="btn btn--ghost" ' + upAttr + ' aria-label="上级" title="上级">' + ICON.arrowUp + '</button>' +
      '<button type="button" class="btn btn--ghost" ' + refreshAttr + ' aria-label="刷新" title="刷新">' + ICON.rotateCw + '</button>' +
      '<button type="button" class="btn btn--ghost" ' + mkdirAttr + ' aria-label="新建文件夹" title="新建文件夹">' + ICON.folderPlus + '</button>' +
      pathWidget +
      '</div>';

    var mkdirBar = '';
    if (IS_TAURI && pane.mkdirOpen) {
      mkdirBar =
        '<form class="tr-pane__tools" data-mkdir-form="' + escapeHtml(pane.side) + '">' +
        '<input type="text" class="tr-path" data-mkdir-input autocomplete="off" spellcheck="false" placeholder="输入新文件夹名称" aria-label="新文件夹名称" style="cursor:text;" />' +
        '<button type="submit" class="btn btn--primary" style="height:30px;padding:0 12px;font-size:var(--size-secondary);">创建</button>' +
        '<button type="button" class="btn btn--secondary" data-action="mkdir-cancel" style="height:30px;padding:0 10px;font-size:var(--size-secondary);">取消</button>' +
        '</form>';
    }

    function thHtml(colKey, label, extraCls) {
      var active = pane.sortBy === colKey;
      var arrowStyle = active && !pane.sortAsc ? ' style="transform:rotate(180deg);display:inline-flex;"' : '';
      return (
        '<span role="columnheader" tabindex="0" class="tr-th' + (extraCls ? ' ' + extraCls : '') + (active ? ' is-sorted' : '') + '" data-sort="' + colKey + '" style="cursor:pointer;">' +
        label +
        '<span class="tr-th__sort"' + arrowStyle + '>' + ICON.chevronDown + '</span></span>'
      );
    }

    var thead =
      '<div class="tr-thead" role="row">' +
      '<span role="columnheader"></span>' +
      thHtml('name', '名称', '') +
      thHtml('date', '修改日期', '') +
      thHtml('type', '类型', '') +
      thHtml('size', '大小', 'tr-th--num') +
      '</div>';

    var contentHtml;
    if (pane.error) {
      contentHtml = '<div class="tr-empty">' + escapeHtml(pane.error) + '</div>';
    } else if (pane.loading && !files.length) {
      contentHtml = '<div class="tr-empty">正在读取目录…</div>';
    } else if (!files.length) {
      contentHtml = '<div class="tr-empty">此文件夹为空</div>';
    } else {
      var body = files
        .map(function (file, index) {
          var icon = file.folder ? ICON.folder : ICON.file;
          var escName = escapeHtml(file.name);
          var escDate = escapeHtml(file.date);
          var escType = escapeHtml(file.type);
          var escSize = escapeHtml(file.size);
          return (
            '<div class="tr-file-row" role="row" data-index="' + index + '">' +
            '<span class="tr-file-row__check" role="cell">' +
            '<input type="checkbox" class="checkbox" aria-label="' + escName + '"></span>' +
            '<span class="tr-file-row__name" role="cell">' +
            '<span class="tr-file-row__icon' + (file.folder ? ' tr-file-row__icon--folder' : '') + '">' + icon + '</span>' +
            '<span class="tr-file-row__label" data-full="' + escName + '" title="' + escName + '">' + escName + '</span></span>' +
            '<span class="tr-file-row__meta" role="cell">' + escDate + '</span>' +
            '<span class="tr-file-row__meta" role="cell">' + escType + '</span>' +
            '<span class="tr-file-row__size" role="cell">' + escSize + '</span>' +
            '</div>'
          );
        })
        .join('');
      contentHtml = '<div class="tr-tbody" role="rowgroup">' + body + '</div>';
    }

    paneEl.innerHTML =
      tools +
      mkdirBar +
      '<div class="tr-table">' +
      thead +
      contentHtml +
      '</div>';

    if (IS_TAURI && pane.editingPath) {
      var pathInput = paneEl.querySelector('[data-path-input]');
      if (pathInput) {
        pathInput.focus();
        pathInput.select();
      }
    } else if (IS_TAURI && pane.mkdirOpen) {
      var input = paneEl.querySelector('[data-mkdir-input]');
      if (input) input.focus();
    }

    truncatePane(paneEl);
    syncSendState();
  }

  /* ── 名称超长中截断（§6 W2「名称（超长中截断）」） ───────── */

  function middleTruncate(el) {
    var full = el.getAttribute('data-full') || '';
    el.textContent = full;
    if (el.scrollWidth <= el.clientWidth) {
      return;
    }
    var ext = '';
    var dot = full.lastIndexOf('.');
    if (dot > 0 && full.length - dot <= 12) {
      ext = full.slice(dot);
    }
    var stem = ext ? full.slice(0, dot) : full;
    var head = Math.max(1, Math.ceil(stem.length * 0.5));
    var tail = Math.max(1, stem.length - head);
    var text = '…';
    while (head >= 1 && tail >= 1) {
      text = stem.slice(0, head) + '…' + stem.slice(stem.length - tail) + ext;
      el.textContent = text;
      if (el.scrollWidth <= el.clientWidth) {
        return;
      }
      head -= 1;
      tail -= 1;
    }
    el.textContent = text;
  }

  function truncatePane(paneEl) {
    Array.prototype.forEach.call(paneEl.querySelectorAll('.tr-file-row__label'), middleTruncate);
  }

  function truncateAll() {
    Array.prototype.forEach.call(document.querySelectorAll('.tr-file-row__label'), middleTruncate);
  }

  var resizeTimer = 0;

  window.addEventListener('resize', function () {
    window.clearTimeout(resizeTimer);
    resizeTimer = window.setTimeout(truncateAll, 100);
  });

  /* ── 队列渲染 ───────────────────────────────────────────── */

  function statusCell(row) {
    if (row.status === 'done') {
      var doneLabel = row.direction === 'download' ? '已接收' : '已发送';
      return (
        '<span class="tr-qrow__status tr-qrow__status--done" role="cell">' +
        ICON.check + '<span class="tr-qrow__status-text">' + doneLabel + '</span></span>'
      );
    }
    if (row.status === 'failed' || row.status === 'cancelled') {
      var failLabel = row.status === 'cancelled' ? '已取消' : '失败';
      var hint = escapeHtml(row.hint || (row.status === 'cancelled' ? '已取消传输' : '传输失败'));
      return (
        '<span class="tr-qrow__status tr-qrow__status--failed" role="cell" title="' +
        hint + '">' + ICON.x + '<span class="tr-qrow__status-text">' + failLabel + '</span></span>'
      );
    }
    var pct = Math.max(0, Math.min(100, Number(row.progress || 0)));
    return (
      '<span class="tr-qrow__status tr-qrow__status--active" role="cell">' +
      '<span class="tr-qrow__status-text">进行中</span>' +
      '<span class="progress">' +
      '<span class="progress__track"><span class="progress__fill" style="width:' + pct + '%"></span></span>' +
      '<span class="progress__pct">' + pct + '%</span>' +
      '</span></span>'
    );
  }

  function renderQueue() {
    var body = document.getElementById('queue-body');
    var count = document.getElementById('queue-count');
    var done = queue.filter(function (row) {
      return row.status === 'done';
    }).length;

    count.textContent = '已传输 ' + done + ' 个文件';

    if (!queue.length) {
      body.innerHTML = '<div class="tr-qempty">暂无传输任务</div>';
      return;
    }

    body.innerHTML = queue
      .map(function (row, index) {
        var escName = escapeHtml(row.name);
        var escSize = escapeHtml(row.size);
        var escFrom = escapeHtml(row.from);
        var escTo = escapeHtml(row.to);
        var isDownload = row.direction === 'download';
        var fromPill = isDownload
          ? '<span class="pill pill--remote">远端</span>'
          : '<span class="pill pill--local">本机</span>';
        var toPill = isDownload
          ? '<span class="pill pill--local">本机</span>'
          : '<span class="pill pill--remote">远端</span>';

        var opBtn;
        if (IS_TAURI && row.status === 'active') {
          opBtn =
            '<button type="button" class="btn btn--ghost" data-cancel="' + escapeHtml(row.id) + '" aria-label="取消任务" title="取消">' +
            ICON.x + '</button>';
        } else {
          var removeVal = IS_TAURI ? escapeHtml(row.id) : String(index);
          opBtn =
            '<button type="button" class="btn btn--ghost" data-remove="' + removeVal + '" aria-label="移除任务" title="移除">' +
            ICON.trash + '</button>';
        }

        return (
          '<div class="tr-qrow" role="row" data-index="' + index + '">' +
          '<span class="tr-qrow__name" role="cell">' +
          '<span class="tr-qrow__icon">' + ICON.file + '</span>' +
          '<span class="tr-qrow__label" data-full="' + escName + '" title="' + escName + '">' + escName + '</span></span>' +
          statusCell(row) +
          '<span class="tr-qrow__size" role="cell">' + escSize + '</span>' +
          '<span class="tr-qrow__path" role="cell">' +
          fromPill +
          '<span class="tr-qrow__mono" title="' + escFrom + '">' + escFrom + '</span></span>' +
          '<span class="tr-qrow__path" role="cell">' +
          toPill +
          '<span class="tr-qrow__mono" title="' + escTo + '">' + escTo + '</span></span>' +
          '<span class="tr-qrow__op" role="cell">' +
          opBtn + '</span>' +
          '</div>'
        );
      })
      .join('');

    truncateAll();
  }

  /* ── 勾选状态 → 发送按钮激活（§6 W2） ───────────────────── */

  function checkedFiles(paneEl) {
    if (!paneEl) return [];
    return Array.prototype.filter
      .call(paneEl.querySelectorAll('.tr-file-row'), function (row) {
        var box = row.querySelector('.checkbox');
        var checked = Boolean(box && box.checked);
        row.classList.toggle('is-selected', checked);
        return checked;
      })
      .map(function (row) {
        return Number(row.getAttribute('data-index'));
      });
  }

  function clearPaneChecks(paneEl) {
    if (!paneEl) return;
    Array.prototype.forEach.call(paneEl.querySelectorAll('.tr-file-row'), function (row) {
      var box = row.querySelector('.checkbox');
      if (box) box.checked = false;
      row.classList.remove('is-selected');
    });
    syncSendState();
  }

  function syncSendState() {
    var ready = isRemoteReady();
    var localChecked = checkedFiles(panes.local.el).length > 0;
    var remoteChecked = checkedFiles(panes.remote.el).length > 0;
    document.getElementById('send-to-remote').disabled = !(ready && localChecked);
    document.getElementById('send-to-local').disabled = !(ready && remoteChecked);
  }

  /* ── Tauri 真实目录加载与事件处理 ───────────────────────── */

  function settleAll(promises) {
    if (typeof Promise.allSettled === 'function') {
      return Promise.allSettled(promises);
    }
    return Promise.all(
      promises.map(function (p) {
        return Promise.resolve(p).then(
          function (value) {
            return { status: 'fulfilled', value: value };
          },
          function (reason) {
            return { status: 'rejected', reason: reason };
          }
        );
      })
    );
  }

  function sortQueueDesc() {
    queue.sort(function (a, b) {
      return Number(b.id || 0) - Number(a.id || 0);
    });
  }

  function loadLocalDir(path, pushHistory) {
    var pane = panes.local;
    var prevDir = pushHistory && pane.dir && pane.dir !== path ? pane.dir : null;
    var seq = ++pane.reqSeq;
    pane.loading = true;
    pane.error = '';
    pane.mkdirOpen = false;
    renderPane(pane);

    Bridge.fileListLocal(path == null ? null : path).then(
      function (res) {
        if (seq !== pane.reqSeq) return;
        var nextDir = (res && res.path) || '';
        if (prevDir && prevDir !== nextDir) {
          pane.history.push(prevDir);
        }
        pane.loading = false;
        pane.error = '';
        pane.dir = nextDir;
        pane.files = mapEntries((res && res.entries) || []);
        renderPane(pane);
      },
      function (err) {
        if (seq !== pane.reqSeq) return;
        pane.loading = false;
        pane.error = errMessage(err);
        pane.files = [];
        renderPane(pane);
        toast(pane.error);
      }
    );
  }

  function clearRemoteTimers() {
    window.clearTimeout(panes.remote.timeoutId);
    panes.remote.timeoutId = 0;
    window.clearTimeout(panes.remote.mkdirTimeoutId);
    panes.remote.mkdirTimeoutId = 0;
  }

  function loadRemoteDir(path, pushHistory) {
    var pane = panes.remote;
    if (IS_TAURI && !isRemoteReady()) {
      pane.loading = false;
      pane.error = remoteSession.watchOnly
        ? '当前为只读观看模式，无法使用文件传输'
        : remoteSession.active
          ? '正在建立远控连接…'
          : '请先建立远控会话';
      renderPane(pane);
      return;
    }
    var seq = ++pane.reqSeq;
    pane.pendingHistoryDir = pushHistory && pane.dir && pane.dir !== path ? pane.dir : null;
    pane.pendingRemoteResponses += 1;
    pane.loading = true;
    pane.error = '';
    pane.mkdirOpen = false;
    pane.editingPath = false;
    renderPane(pane);

    window.clearTimeout(pane.timeoutId);
    pane.timeoutId = window.setTimeout(function () {
      if (seq !== pane.reqSeq || !pane.loading) return;
      pane.timeoutId = 0;
      pane.pendingHistoryDir = null;
      pane.loading = false;
      pane.error = '远端响应超时，请点击刷新重试';
      renderPane(pane);
      toast('远端目录读取超时');
    }, REMOTE_REQ_TIMEOUT_MS);

    Bridge.fileListRemote(path == null ? null : path).catch(function (err) {
      pane.pendingRemoteResponses = Math.max(0, pane.pendingRemoteResponses - 1);
      if (seq !== pane.reqSeq) return;
      window.clearTimeout(pane.timeoutId);
      pane.timeoutId = 0;
      pane.pendingHistoryDir = null;
      pane.loading = false;
      pane.error = errMessage(err);
      pane.files = [];
      renderPane(pane);
      toast(pane.error);
    });
  }

  function upsertTransfer(rawItem) {
    var mapped = mapTransferItem(rawItem);
    var prevStatus = null;
    var foundIdx = -1;
    for (var i = 0; i < queue.length; i += 1) {
      if (queue[i].id === mapped.id) {
        prevStatus = queue[i].status;
        foundIdx = i;
        break;
      }
    }
    if (foundIdx >= 0) {
      queue[foundIdx] = mapped;
    } else {
      queue.push(mapped);
    }
    sortQueueDesc();
    renderQueue();

    if (prevStatus !== 'done' && mapped.status === 'done') {
      if (mapped.direction === 'upload' && panes.remote.dir) {
        loadRemoteDir(panes.remote.dir, false);
      } else if (mapped.direction === 'download' && panes.local.dir) {
        loadLocalDir(panes.local.dir, false);
      }
    }
  }

  function mergeTransfers(items, suppressRefresh) {
    if (!Array.isArray(items)) return;
    var seenIds = Object.create(null);
    var refreshRemote = false;
    var refreshLocal = false;

    items.forEach(function (rawItem) {
      var mapped = mapTransferItem(rawItem);
      seenIds[mapped.id] = true;
      var prevStatus = null;
      var foundIdx = -1;
      for (var i = 0; i < queue.length; i += 1) {
        if (queue[i].id === mapped.id) {
          prevStatus = queue[i].status;
          foundIdx = i;
          break;
        }
      }
      if (foundIdx >= 0) {
        queue[foundIdx] = mapped;
      } else {
        queue.push(mapped);
      }
      if (!suppressRefresh && prevStatus !== 'done' && mapped.status === 'done') {
        if (mapped.direction === 'upload' && panes.remote.dir) {
          refreshRemote = true;
        } else if (mapped.direction === 'download' && panes.local.dir) {
          refreshLocal = true;
        }
      }
    });

    queue = queue.filter(function (row) {
      return Boolean(seenIds[row.id]);
    });
    sortQueueDesc();
    renderQueue();

    if (refreshRemote && panes.remote.dir) {
      loadRemoteDir(panes.remote.dir, false);
    }
    if (refreshLocal && panes.local.dir) {
      loadLocalDir(panes.local.dir, false);
    }
  }

  function setHeaderDeviceName(elId, name) {
    var el = document.getElementById(elId);
    if (!el) return;
    el.textContent = name;
    el.title = name;
  }

  function resetRemotePaneDisconnected(message, failQueue) {
    clearRemoteTimers();
    var pane = panes.remote;
    pane.reqSeq += 1;
    pane.pendingRemoteResponses = 0;
    pane.pendingHistoryDir = null;
    pane.loading = false;
    pane.mkdirOpen = false;
    pane.editingPath = false;
    pane.dir = '';
    pane.files = [];
    pane.history = [];
    pane.error = message;

    var queueChanged = false;
    if (failQueue) {
      queue.forEach(function (row) {
        if (row.status === 'active') {
          row.status = 'failed';
          row.hint = '会话已断开';
          queueChanged = true;
        }
      });
    }
    renderPane(pane);
    if (queueChanged) {
      renderQueue();
    }
    syncSendState();
  }

  function applyActiveSessionInfo(raw) {
    var s = raw && typeof raw === 'object' ? raw : {};
    var nextActive = Boolean(s.active);
    var nextConnected = Boolean(s.connected);
    var nextWatch = Boolean(s.watchOnly !== undefined ? s.watchOnly : s.watch_only);
    var nextName = String(
      s.deviceName || s.device_name || s.target || ''
    );
    remoteSession.active = nextActive;
    remoteSession.connected = nextConnected;
    remoteSession.watchOnly = nextWatch;
    remoteSession.deviceName = nextName;
    setHeaderDeviceName('tr-remote-name', nextName || '未连接远端');

    if (!nextActive) {
      resetRemotePaneDisconnected('请先建立远控会话', true);
      return;
    }
    if (nextWatch) {
      resetRemotePaneDisconnected('当前为只读观看模式，无法使用文件传输', false);
      return;
    }
    if (!nextConnected) {
      resetRemotePaneDisconnected('正在建立远控连接…', false);
      return;
    }
    syncSendState();
    if (!panes.remote.dir && !panes.remote.loading) {
      loadRemoteDir(null, false);
    }
    Bridge.fileListTransfers()
      .then(function (items) {
        mergeTransfers(items, true);
      })
      .catch(function () {});
  }

  function handleFileEvent(ev) {
    if (!ev || !ev.type) return;
    if (ev.type === 'remoteDir') {
      var pane = panes.remote;
      if (pane.pendingRemoteResponses <= 0) {
        return;
      }
      pane.pendingRemoteResponses -= 1;
      if (pane.pendingRemoteResponses > 0 || !pane.loading) {
        /* 丢弃已过期的早期目录响应或已超时请求的迟到响应，避免覆盖新请求状态 */
        return;
      }
      window.clearTimeout(pane.timeoutId);
      pane.timeoutId = 0;
      var pendingPrevDir = pane.pendingHistoryDir;
      pane.pendingHistoryDir = null;
      pane.loading = false;
      if (ev.error) {
        pane.error = String(ev.error);
        pane.files = [];
        renderPane(pane);
        toast(pane.error);
      } else {
        var nextDir = ev.path || '';
        if (pendingPrevDir && pendingPrevDir !== nextDir) {
          pane.history.push(pendingPrevDir);
        }
        pane.error = '';
        pane.dir = nextDir;
        pane.files = mapEntries(ev.entries || []);
        renderPane(pane);
      }
      return;
    }
    if (ev.type === 'mkdirResult') {
      window.clearTimeout(panes.remote.mkdirTimeoutId);
      panes.remote.mkdirTimeoutId = 0;
      if (ev.ok) {
        toast('已新建远端文件夹');
        loadRemoteDir(panes.remote.dir || null, false);
      } else {
        toast('新建失败：' + String(ev.error || '未知错误'));
      }
      return;
    }
    if (ev.type === 'transferUpdated') {
      upsertTransfer(ev.item || ev);
    }
  }

  function handleSessionUpdate(upd) {
    if (!upd || !upd.kind) return;
    if (upd.kind === 'ended') {
      remoteSession.active = false;
      remoteSession.connected = false;
      remoteSession.watchOnly = false;
      setHeaderDeviceName('tr-remote-name', '未连接远端');
      resetRemotePaneDisconnected('请先建立远控会话', true);
      return;
    }
    if (upd.kind === 'negotiated') {
      var data = upd.data || {};
      var dName = data.device_name || data.device || remoteSession.deviceName;
      remoteSession.active = true;
      remoteSession.connected = true;
      if (typeof data.watch_only === 'boolean') {
        remoteSession.watchOnly = data.watch_only;
      }
      if (dName) {
        remoteSession.deviceName = String(dName);
        setHeaderDeviceName('tr-remote-name', String(dName));
      }
      if (!remoteSession.watchOnly) {
        syncSendState();
        loadRemoteDir(null, false);
        Bridge.fileListTransfers()
          .then(function (items) {
            mergeTransfers(items, true);
          })
          .catch(function () {});
      }
    }
  }

  /* ── 发送入队（Mock 与 Tauri 双路径） ───────────────────── */

  function enqueueMock(files, indices, isDownload) {
    if (!indices.length) return;
    indices.forEach(function (index) {
      var file = files[index];
      if (!file) return;
      queue.push({
        name: file.name,
        direction: isDownload ? 'download' : 'upload',
        status: 'done',
        size: file.size,
        from: (isDownload ? MOCK_REMOTE_DIR : MOCK_LOCAL_DIR) + '/' + file.name,
        to: (isDownload ? MOCK_LOCAL_DIR : MOCK_REMOTE_DIR) + '/' + file.name
      });
    });
    renderQueue();
    toast('已加入传输队列');
  }

  function startRealTransfer(side) {
    var srcPane = side === 'local' ? panes.local : panes.remote;
    var dstPane = side === 'local' ? panes.remote : panes.local;
    var indices = checkedFiles(srcPane.el);
    if (!indices.length) return;

    if (!dstPane.dir) {
      toast(side === 'local' ? '请先建立远控会话' : '本机目标目录未就绪');
      return;
    }

    var selectedFiles = [];
    var skippedDirs = 0;
    indices.forEach(function (idx) {
      var entry = srcPane.files[idx];
      if (!entry) return;
      if (entry.folder) {
        skippedDirs += 1;
      } else {
        selectedFiles.push(entry);
      }
    });

    if (!selectedFiles.length) {
      toast('暂不支持直接传输文件夹，请进入文件夹后选择文件');
      return;
    }

    clearPaneChecks(srcPane.el);

    var promises = selectedFiles.map(function (file) {
      return side === 'local'
        ? Bridge.fileUpload(file.path, dstPane.dir)
        : Bridge.fileDownload(file.path, dstPane.dir);
    });

    settleAll(promises).then(function (results) {
      var okCount = 0;
      var errors = [];
      results.forEach(function (res, i) {
        if (res.status === 'fulfilled') {
          okCount += 1;
        } else {
          var name = selectedFiles[i] ? selectedFiles[i].name : '';
          errors.push((name ? name + '：' : '') + errMessage(res.reason));
        }
      });

      var verb = side === 'local' ? '上传' : '下载';
      if (errors.length === 0) {
        toast(
          skippedDirs > 0
            ? '已开始' + verb + ' ' + okCount + ' 个文件（已跳过文件夹）'
            : '已开始' + verb + ' ' + okCount + ' 个文件'
        );
      } else if (okCount === 0) {
        toast(verb + '失败：' + errors[0]);
      } else {
        toast(okCount + ' 个成功，' + errors.length + ' 个失败：' + errors[0]);
      }

      return Bridge.fileListTransfers()
        .then(function (items) {
          mergeTransfers(items, false);
        })
        .catch(function () {});
    });
  }

  /* ── 面板事件接线 ───────────────────────────────────────── */

  function bindPane(pane) {
    var paneEl = pane.el;

    paneEl.addEventListener('click', function (event) {
      var sortHeader = event.target.closest('[data-sort]');
      if (sortHeader) {
        var sortKey = sortHeader.getAttribute('data-sort');
        if (sortKey) {
          if (pane.sortBy === sortKey) {
            pane.sortAsc = !pane.sortAsc;
          } else {
            pane.sortBy = sortKey;
            pane.sortAsc = sortKey === 'date' || sortKey === 'size' ? false : true;
          }
          renderPane(pane);
        }
        return;
      }

      var toastBtn = event.target.closest('[data-toast]');
      if (toastBtn) {
        toast('功能开发中');
        return;
      }

      var actionBtn = event.target.closest('[data-action]');
      if (actionBtn && IS_TAURI) {
        var action = actionBtn.getAttribute('data-action');
        if (action === 'back') {
          if (pane.history.length) {
            var prev = pane.history.pop();
            if (pane.side === 'local') loadLocalDir(prev, false);
            else loadRemoteDir(prev, false);
          }
          return;
        }
        if (action === 'up') {
          var up = parentDir(pane.dir);
          if (up && up !== pane.dir) {
            if (pane.side === 'local') loadLocalDir(up, true);
            else loadRemoteDir(up, true);
          }
          return;
        }
        if (action === 'refresh') {
          if (pane.side === 'local') loadLocalDir(pane.dir || null, false);
          else loadRemoteDir(pane.dir || null, false);
          return;
        }
        if (action === 'mkdir') {
          if (!pane.dir) return;
          pane.editingPath = false;
          pane.mkdirOpen = !pane.mkdirOpen;
          renderPane(pane);
          return;
        }
        if (action === 'mkdir-cancel') {
          pane.mkdirOpen = false;
          renderPane(pane);
          return;
        }
        if (action === 'edit-path') {
          pane.mkdirOpen = false;
          pane.editingPath = true;
          renderPane(pane);
          return;
        }
        if (action === 'path-cancel') {
          pane.editingPath = false;
          renderPane(pane);
          return;
        }
        if (action === 'copy-path') {
          if (pane.dir && typeof Bridge.copyText === 'function') {
            Bridge.copyText(pane.dir).then(
              function () { toast('已复制路径'); },
              function () {}
            );
          }
          return;
        }
      }

      if (event.target.closest('[data-mkdir-form]') || event.target.closest('[data-path-form]')) {
        return;
      }

      var row = event.target.closest('.tr-file-row');
      if (!row) return;
      if (event.target.closest('.checkbox')) {
        return; /* 原生勾选自行处理 */
      }
      var box = row.querySelector('.checkbox');
      if (box) {
        box.checked = !box.checked;
        syncSendState();
      }
    });

    paneEl.addEventListener('dblclick', function (event) {
      if (!IS_TAURI) return;
      if (event.target.closest('.checkbox')) return;
      var row = event.target.closest('.tr-file-row');
      if (!row) return;
      var idx = Number(row.getAttribute('data-index'));
      var entry = pane.files[idx];
      if (!entry || !entry.folder || !entry.path) return;
      if (pane.side === 'local') {
        loadLocalDir(entry.path, true);
      } else {
        loadRemoteDir(entry.path, true);
      }
    });

    paneEl.addEventListener('submit', function (event) {
      if (!IS_TAURI) return;
      var pathForm = event.target.closest('[data-path-form]');
      if (pathForm) {
        event.preventDefault();
        var pathInput = pathForm.querySelector('[data-path-input]');
        var nextPath = pathInput ? pathInput.value.trim() : '';
        pane.editingPath = false;
        renderPane(pane);
        if (nextPath) {
          if (pane.side === 'local') {
            loadLocalDir(nextPath, true);
          } else {
            loadRemoteDir(nextPath, true);
          }
        }
        return;
      }
      var form = event.target.closest('[data-mkdir-form]');
      if (!form) return;
      event.preventDefault();
      var input = form.querySelector('[data-mkdir-input]');
      var name = input ? input.value.trim() : '';
      if (!name) {
        toast('请输入文件夹名称');
        if (input) input.focus();
        return;
      }
      pane.mkdirOpen = false;
      renderPane(pane);
      if (pane.side === 'local') {
        Bridge.fileMkdirLocal(pane.dir, name).then(
          function () {
            toast('已新建文件夹');
            loadLocalDir(pane.dir, false);
          },
          function (err) {
            toast('新建失败：' + errMessage(err));
          }
        );
      } else {
        window.clearTimeout(panes.remote.mkdirTimeoutId);
        panes.remote.mkdirTimeoutId = window.setTimeout(function () {
          panes.remote.mkdirTimeoutId = 0;
          toast('远端新建文件夹超时，请刷新确认');
        }, REMOTE_REQ_TIMEOUT_MS);
        Bridge.fileMkdirRemote(pane.dir, name).catch(function (err) {
          window.clearTimeout(panes.remote.mkdirTimeoutId);
          panes.remote.mkdirTimeoutId = 0;
          toast('新建失败：' + errMessage(err));
        });
      }
    });

    paneEl.addEventListener('keydown', function (event) {
      if ((event.key === 'Enter' || event.key === ' ') && event.target.closest('[data-sort]')) {
        event.preventDefault();
        event.target.closest('[data-sort]').click();
        return;
      }
      if (event.key === 'Escape' && (pane.mkdirOpen || pane.editingPath)) {
        event.preventDefault();
        pane.mkdirOpen = false;
        pane.editingPath = false;
        renderPane(pane);
      }
    });

    paneEl.addEventListener('change', function (event) {
      if (event.target.classList.contains('checkbox')) {
        syncSendState();
      }
    });
  }

  /* ── 启动 ───────────────────────────────────────────────── */

  function init() {
    panes.local.el = document.getElementById('pane-local');
    panes.remote.el = document.getElementById('pane-remote');

    bindPane(panes.local);
    bindPane(panes.remote);

    document.getElementById('send-to-remote').addEventListener('click', function () {
      if (IS_TAURI) {
        startRealTransfer('local');
      } else {
        enqueueMock(panes.local.files, checkedFiles(panes.local.el), false);
      }
    });

    document.getElementById('send-to-local').addEventListener('click', function () {
      if (IS_TAURI) {
        startRealTransfer('remote');
      } else {
        enqueueMock(panes.remote.files, checkedFiles(panes.remote.el), true);
      }
    });

    document.getElementById('clear-finished').addEventListener('click', function () {
      if (IS_TAURI) {
        Bridge.fileClearFinished().then(
          function () {
            queue = queue.filter(function (row) {
              return row.status === 'active';
            });
            renderQueue();
          },
          function (err) {
            toast(errMessage(err));
          }
        );
        return;
      }
      queue = queue.filter(function (row) {
        return row.status === 'active';
      });
      renderQueue();
    });

    var cancelAllBtn = document.getElementById('cancel-all');
    if (cancelAllBtn) {
      cancelAllBtn.addEventListener('click', function (event) {
        if (!IS_TAURI) {
          return; /* 由下方 [data-toast] 监听器处理 */
        }
        event.stopImmediatePropagation();
        var active = queue.filter(function (row) {
          return row.status === 'active';
        });
        if (!active.length) {
          toast('当前没有进行中的任务');
          return;
        }
        var cancelPromises = active.map(function (row) {
          return Bridge.fileCancel(row.id);
        });
        settleAll(cancelPromises).then(function (results) {
          var failed = results.filter(function (res) {
            return res.status === 'rejected';
          });
          if (failed.length === 0) {
            toast('已取消全部进行中任务');
          } else if (failed.length === results.length) {
            toast('取消失败：' + errMessage(failed[0].reason));
          } else {
            toast(
              '已取消 ' +
                (results.length - failed.length) +
                ' 个任务，' +
                failed.length +
                ' 个失败：' +
                errMessage(failed[0].reason)
            );
          }
        });
      });
    }

    document.getElementById('queue-body').addEventListener('click', function (event) {
      var cancelBtn = event.target.closest('[data-cancel]');
      if (cancelBtn && IS_TAURI) {
        var cancelId = Number(cancelBtn.getAttribute('data-cancel'));
        Bridge.fileCancel(cancelId).then(
          function () { toast('已取消传输'); },
          function (err) { toast(errMessage(err)); }
        );
        return;
      }

      var removeBtn = event.target.closest('[data-remove]');
      if (!removeBtn) return;

      if (IS_TAURI) {
        var removeId = Number(removeBtn.getAttribute('data-remove'));
        Bridge.fileRemoveTransfer(removeId).then(
          function () {
            queue = queue.filter(function (row) {
              return row.id !== removeId;
            });
            renderQueue();
          },
          function (err) {
            toast(errMessage(err));
          }
        );
        return;
      }

      queue.splice(Number(removeBtn.getAttribute('data-remove')), 1);
      renderQueue();
    });

    Array.prototype.forEach.call(document.querySelectorAll('.tr-queue__actions [data-toast]'), function (btn) {
      btn.addEventListener('click', function () {
        toast('功能开发中');
      });
    });

    if (IS_TAURI) {
      setHeaderDeviceName('tr-local-name', '本机');
      setHeaderDeviceName('tr-remote-name', '未连接远端');
      renderPane(panes.local);
      renderPane(panes.remote);
      renderQueue();

      /* 本地目录不依赖事件流可直接加载；远端目录与队列等待事件监听及会话状态就绪后再拉取，消除首包竞态 */
      loadLocalDir(null, false);
      if (typeof Bridge.getHostStatus === 'function') {
        Bridge.getHostStatus()
          .then(function (host) {
            if (host && (host.displayAddr || host.listenAddr)) {
              setHeaderDeviceName('tr-local-name', '本机 (' + (host.displayAddr || host.listenAddr) + ')');
            }
          })
          .catch(function () {});
      }
      Promise.all([
        Promise.resolve(Bridge.onFileEvent(handleFileEvent)).catch(function () {}),
        Promise.resolve(
          typeof Bridge.onSessionUpdate === 'function'
            ? Bridge.onSessionUpdate(handleSessionUpdate)
            : null
        ).catch(function () {}),
        Promise.resolve(
          typeof Bridge.onActiveSessionState === 'function'
            ? Bridge.onActiveSessionState(applyActiveSessionInfo)
            : null
        ).catch(function () {})
      ]).then(function () {
        if (typeof Bridge.getActiveSessionInfo === 'function') {
          Bridge.getActiveSessionInfo()
            .then(applyActiveSessionInfo)
            .catch(function () {
              resetRemotePaneDisconnected('请先建立远控会话', false);
            });
        } else {
          loadRemoteDir(null, false);
          Bridge.fileListTransfers()
            .then(function (items) {
              mergeTransfers(items, true);
            })
            .catch(function () {});
        }
      });
    } else {
      renderPane(panes.local);
      renderPane(panes.remote);
      renderQueue();
      truncateAll();
    }

    syncSendState();
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
