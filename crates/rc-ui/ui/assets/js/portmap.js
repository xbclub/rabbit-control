/* portmap.js —— W4 端口映射 / §6 W4 + §6.0 mock + §7.1 校验 + §7.2 空态 + §7.4 浮层 + Tauri 真实端口映射
 *
 * 浏览器预览（!IS_TAURI）：
 *   mock 数据逐字取自 docs/reference/design/08-ui-design.md §6.0，行为 100% 保持。
 * Tauri 真窗口（IS_TAURI）：
 *   先订阅 portmap://update 再调用 portmap_list；支持添加、启停切换、删除与状态/流量提示。
 * 安全纪律：所有规则名/目标地址/错误信息均经 escapeHtml() 严格转义。
 */

(function () {
  'use strict';

  var Validate = window.RCValidate;
  var Bridge = window.RCBridge;
  var IS_TAURI = Boolean(Bridge && Bridge.isTauri);

  /* ── 严格 HTML 转义 ─────────────────────────────────────── */

  function escapeHtml(value) {
    return String(value == null ? '' : value)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
      .replace(/'/g, '&#39;');
  }

  function formatBytes(bytes) {
    var n = Number(bytes);
    if (!isFinite(n) || n < 0) return '0 B';
    if (n < 1024) return Math.round(n) + ' B';
    if (n < 1024 * 1024) return (n / 1024).toFixed(1) + ' KB';
    if (n < 1024 * 1024 * 1024) return (n / (1024 * 1024)).toFixed(2) + ' MB';
    return (n / (1024 * 1024 * 1024)).toFixed(2) + ' GB';
  }

  function errMessage(err) {
    if (!err) return '操作失败';
    if (typeof err === 'string') return err;
    if (err.message) return String(err.message);
    return String(err);
  }

  /* ── §6.0 mock 映射 3 行（仅 !IS_TAURI 使用） ────────────── */

  var MOCK_MAPPINGS = [
    { name: 'deepseek harness', localPort: '3080', targetAddr: '127.0.0.1', targetPort: '3080', status: 'ok', enabled: true },
    { name: 'ssh-tunnel', localPort: '2222', targetAddr: '10.0.0.8', targetPort: '22', status: 'fail', enabled: true },
    { name: 'jupyter-lab', localPort: '8888', targetAddr: '127.0.0.1', targetPort: '8888', status: 'pending', enabled: true }
  ];

  var mappings = IS_TAURI ? [] : MOCK_MAPPINGS.map(function (r) {
    return {
      name: r.name,
      localPort: r.localPort,
      targetAddr: r.targetAddr,
      targetPort: r.targetPort,
      status: r.status,
      enabled: r.enabled
    };
  });

  var sortAsc = null; /* null = 原始顺序；true/false = 名称升/降序 */
  var pendingDeleteRule = null;
  var sessionOnline = !IS_TAURI;
  var sessionWatchOnly = false;
  var openAddBtn = document.getElementById('open-add');

  var ICON = {
    ellipsis:
      '<svg class="ico" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="12" cy="12" r="1"/><circle cx="19" cy="12" r="1"/><circle cx="5" cy="12" r="1"/></svg>',
    check:
      '<svg class="ico" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M20 6 9 17l-5-5"/></svg>',
    x:
      '<svg class="ico" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M18 6 6 18"/><path d="m6 6 12 12"/></svg>',
    clock:
      '<svg class="ico" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="12" cy="12" r="10"/><polyline points="12 6 12 12 16 14"/></svg>',
    pause:
      '<svg class="ico" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="6" y="4" width="4" height="16" rx="1"/><rect x="14" y="4" width="4" height="16" rx="1"/></svg>'
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

  /* ── 数据归一 ───────────────────────────────────────────── */

  function normalizeRule(rule) {
    return {
      id: rule.id != null ? Number(rule.id) : null,
      name: String(rule.name || ''),
      localPort: String(rule.localPort != null ? rule.localPort : ''),
      targetAddr: String(rule.targetAddr || ''),
      targetPort: String(rule.targetPort != null ? rule.targetPort : ''),
      enabled: rule.enabled !== false,
      status: String(rule.status || 'pending'),
      activeConns: Number(rule.activeConns || 0),
      bytesTx: Number(rule.bytesTx || 0),
      bytesRx: Number(rule.bytesRx || 0),
      error: rule.error ? String(rule.error) : ''
    };
  }

  function findRuleById(id) {
    for (var i = 0; i < mappings.length; i += 1) {
      if (mappings[i].id === id) {
        return mappings[i];
      }
    }
    return null;
  }

  function hasDuplicateLocalPort(portVal) {
    var target = String(portVal == null ? '' : portVal).trim();
    if (!target) return false;
    return mappings.some(function (row) {
      return String(row.localPort == null ? '' : row.localPort).trim() === target;
    });
  }

  function applyRulesList(rules) {
    if (!Array.isArray(rules)) return;
    mappings = rules.map(normalizeRule);
    if (
      pendingDeleteRule &&
      pendingDeleteRule.id != null &&
      !findRuleById(pendingDeleteRule.id)
    ) {
      closeDeleteModal();
    }
    render();
  }

  function upsertRule(rule) {
    var norm = normalizeRule(rule);
    var idx = -1;
    for (var i = 0; i < mappings.length; i += 1) {
      if (mappings[i].id === norm.id) {
        idx = i;
        break;
      }
    }
    if (idx >= 0) {
      mappings[idx] = norm;
    } else {
      mappings.push(norm);
    }
    render();
  }

  /* ── 列表渲染 ───────────────────────────────────────────── */

  function statusTooltip(row) {
    if (row.error) {
      return row.error;
    }
    if (IS_TAURI) {
      return (
        '活跃连接 ' +
        row.activeConns +
        ' · 发送 ' +
        formatBytes(row.bytesTx) +
        ' · 接收 ' +
        formatBytes(row.bytesRx)
      );
    }
    return '';
  }

  function statusMarkup(row) {
    var tip = statusTooltip(row);
    var titleAttr = tip ? ' title="' + escapeHtml(tip) + '"' : '';
    var badgeHtml;
    if (!row.enabled) {
      badgeHtml =
        '<span class="pm-row__status pm-row__status--paused"' +
        titleAttr +
        '>' +
        ICON.pause +
        '<span>已暂停</span></span>';
    } else if (row.status === 'ok') {
      badgeHtml =
        '<span class="pm-row__status pm-row__status--ok"' +
        titleAttr +
        '>' +
        ICON.check +
        '<span>' +
        (IS_TAURI ? '正常' : '成功') +
        '</span></span>';
    } else if (row.status === 'fail') {
      badgeHtml =
        '<span class="pm-row__status pm-row__status--fail"' +
        titleAttr +
        '>' +
        ICON.x +
        '<span>失败</span></span>';
    } else {
      badgeHtml =
        '<span class="pm-row__status pm-row__status--pending"' +
        titleAttr +
        '>' +
        ICON.clock +
        '<span>连接中</span></span>';
    }

    if (!IS_TAURI) {
      return badgeHtml.replace(
        '<span class="pm-row__status ',
        '<span role="cell" class="pm-row__status '
      );
    }

    var metricsText =
      row.activeConns +
      ' 连接 · ↑ ' +
      formatBytes(row.bytesTx) +
      ' / ↓ ' +
      formatBytes(row.bytesRx);
    var errHtml = row.error
      ? '<span class="pm-row__err" title="' +
        escapeHtml(row.error) +
        '">' +
        escapeHtml(row.error) +
        '</span>'
      : '';
    return (
      '<div class="pm-row__status-wrap" role="cell">' +
      badgeHtml +
      '<span class="pm-row__metrics" title="' +
      escapeHtml(metricsText) +
      '">' +
      escapeHtml(metricsText) +
      '</span>' +
      errHtml +
      '</div>'
    );
  }

  function orderedIndices() {
    var indices = mappings.map(function (_, index) {
      return index;
    });
    if (sortAsc === null) {
      return indices;
    }
    indices.sort(function (a, b) {
      var result = mappings[a].name.localeCompare(mappings[b].name, 'zh-Hans-CN');
      return sortAsc ? result : -result;
    });
    return indices;
  }

  function render() {
    var body = document.getElementById('map-body');
    var sortEl = document.getElementById('sort-name');
    sortEl.classList.toggle('is-asc', sortAsc === true);
    sortEl.classList.toggle('is-desc', sortAsc === false);

    if (!mappings.length) {
      body.innerHTML = '<div class="pm-empty">暂无映射，点击右上角添加</div>';
      return;
    }

    body.innerHTML = orderedIndices()
      .map(function (index) {
        var row = mappings[index];
        var escName = escapeHtml(row.name);
        var escLocalPort = escapeHtml(row.localPort);
        var escTargetAddr = escapeHtml(row.targetAddr);
        var escTargetPort = escapeHtml(row.targetPort);
        var moreTitle = IS_TAURI ? '删除映射' : '更多';
        var moreAria = IS_TAURI ? '删除映射 ' + escName : '更多操作';
        var ruleIdAttr = IS_TAURI && row.id != null ? ' data-rule-id="' + escapeHtml(row.id) + '"' : '';
        return (
          '<div class="pm-row' + (row.enabled ? '' : ' is-off') + '" role="row" data-index="' + index + '"' + ruleIdAttr + '>' +
          '<span class="pm-row__name" role="cell" title="' + escName + '">' + escName + '</span>' +
          '<span class="pm-row__port" role="cell">' + escLocalPort + '</span>' +
          '<span class="pm-row__addr" role="cell" title="' + escTargetAddr + '">' + escTargetAddr + '</span>' +
          '<span class="pm-row__port" role="cell">' + escTargetPort + '</span>' +
          statusMarkup(row) +
          '<span class="pm-row__op" role="cell">' +
          '<input type="checkbox" class="switch" data-toggle="' + index + '"' + ruleIdAttr + ' aria-label="启用 ' + escName + '"' +
          (row.enabled ? ' checked' : '') + '>' +
          '<button type="button" class="btn btn--ghost" data-more="' + index + '"' + ruleIdAttr + ' aria-label="' + moreAria + '" title="' + moreTitle + '">' +
          ICON.ellipsis + '</button>' +
          '</span>' +
          '</div>'
        );
      })
      .join('');
  }

  /* ── 添加 Modal ─────────────────────────────────────────── */

  var modal = document.getElementById('add-modal');
  var form = document.getElementById('add-form');
  var saveBtn = document.getElementById('save-add');

  var deleteModal = document.getElementById('delete-modal');
  var deleteDescEl = document.getElementById('delete-modal-desc');
  var cancelDeleteBtn = document.getElementById('cancel-delete');
  var confirmDeleteBtn = document.getElementById('confirm-delete');

  var FIELDS = [
    { key: 'name', input: 'f-name', error: 'e-name' },
    { key: 'localPort', input: 'f-local-port', error: 'e-local-port' },
    { key: 'targetAddr', input: 'f-target-addr', error: 'e-target-addr' },
    { key: 'targetPort', input: 'f-target-port', error: 'e-target-port' }
  ];

  var attempted = false;
  var touched = { name: false, localPort: false, targetAddr: false, targetPort: false };
  var submitting = false;

  function readForm() {
    return {
      name: document.getElementById('f-name').value,
      localPort: document.getElementById('f-local-port').value,
      targetAddr: document.getElementById('f-target-addr').value,
      targetPort: document.getElementById('f-target-port').value
    };
  }

  function paint(errors) {
    var valid = true;
    var anyTouched = FIELDS.some(function (field) {
      return touched[field.key];
    });
    FIELDS.forEach(function (field) {
      var input = document.getElementById(field.input);
      var error = document.getElementById(field.error);
      var message = errors[field.key];
      if (message) {
        valid = false;
      }
      var show = Boolean(message) && (touched[field.key] || anyTouched || attempted);
      input.classList.toggle('is-invalid', show);
      input.setAttribute('aria-invalid', show ? 'true' : 'false');
      error.textContent = show ? message : '';
      error.classList.toggle('is-shown', show);
    });
    saveBtn.disabled = !valid || submitting;
    return valid;
  }

  function withDuplicatePortCheck(values, errors, valid) {
    var nextErrors = {
      name: errors && errors.name ? errors.name : '',
      localPort: errors && errors.localPort ? errors.localPort : '',
      targetAddr: errors && errors.targetAddr ? errors.targetAddr : '',
      targetPort: errors && errors.targetPort ? errors.targetPort : ''
    };
    var nextValid = Boolean(valid);
    if (!nextErrors.localPort && hasDuplicateLocalPort(values.localPort)) {
      nextErrors.localPort = '该本地端口已存在映射';
      nextValid = false;
    }
    return { errors: nextErrors, valid: nextValid };
  }

  function validateLive() {
    var values = readForm();
    var base = Validate.validateMapForm(values);
    var checked = withDuplicatePortCheck(values, base.errors, base.valid);
    paint(checked.errors);
  }

  function openModal() {
    if (IS_TAURI && (!sessionOnline || sessionWatchOnly)) {
      toast(sessionWatchOnly ? '观看模式不可管理端口映射' : '远控会话未就绪，请先连接设备');
      return;
    }
    attempted = false;
    submitting = false;
    touched = { name: false, localPort: false, targetAddr: false, targetPort: false };
    form.reset();
    validateLive();
    modal.classList.add('is-open');
    modal.setAttribute('aria-hidden', 'false');
    document.getElementById('f-name').focus();
  }

  function closeModal() {
    submitting = false;
    modal.classList.remove('is-open');
    modal.setAttribute('aria-hidden', 'true');
  }

  function openDeleteModal(rule) {
    if (!deleteModal || !rule) return;
    pendingDeleteRule = rule;
    if (confirmDeleteBtn) confirmDeleteBtn.disabled = false;
    if (deleteDescEl) {
      deleteDescEl.textContent =
        '确定删除映射「' +
        rule.name +
        '」（本地 ' +
        rule.localPort +
        ' → ' +
        rule.targetAddr +
        ':' +
        rule.targetPort +
        '）吗？';
    }
    deleteModal.classList.add('is-open');
    deleteModal.setAttribute('aria-hidden', 'false');
    if (confirmDeleteBtn) confirmDeleteBtn.focus();
  }

  function closeDeleteModal() {
    pendingDeleteRule = null;
    if (confirmDeleteBtn) confirmDeleteBtn.disabled = false;
    if (!deleteModal) return;
    deleteModal.classList.remove('is-open');
    deleteModal.setAttribute('aria-hidden', 'true');
  }

  /* ── 事件接线 ───────────────────────────────────────────── */

  if (openAddBtn) {
    openAddBtn.addEventListener('click', openModal);
  }
  document.getElementById('cancel-add').addEventListener('click', closeModal);

  if (cancelDeleteBtn) {
    cancelDeleteBtn.addEventListener('click', closeDeleteModal);
  }

  if (confirmDeleteBtn) {
    confirmDeleteBtn.addEventListener('click', function () {
      var rule = pendingDeleteRule;
      if (!rule || rule.id == null) {
        closeDeleteModal();
        return;
      }
      confirmDeleteBtn.disabled = true;
      Bridge.portmapDelete(rule.id).then(
        function () {
          confirmDeleteBtn.disabled = false;
          mappings = mappings.filter(function (item) {
            return item.id !== rule.id;
          });
          render();
          closeDeleteModal();
          toast('已删除映射');
        },
        function (err) {
          confirmDeleteBtn.disabled = false;
          closeDeleteModal();
          toast('删除失败：' + errMessage(err));
        }
      );
    });
  }

  FIELDS.forEach(function (field) {
    var el = document.getElementById(field.input);
    el.addEventListener('input', function () {
      touched[field.key] = true;
      validateLive();
    });
    el.addEventListener('blur', function () {
      touched[field.key] = true;
      validateLive();
    });
  });

  form.addEventListener('submit', function (event) {
    event.preventDefault();
    if (submitting) return;
    attempted = true;
    var values = readForm();

    function commitIfValid(errors, valid) {
      var checked = withDuplicatePortCheck(values, errors, valid);
      paint(checked.errors);
      if (!checked.valid) {
        return;
      }
      var cleanName = values.name.trim();
      var cleanLocalPort = Number(values.localPort);
      var cleanTargetAddr = values.targetAddr.trim();
      var cleanTargetPort = Number(values.targetPort);

      if (IS_TAURI) {
        submitting = true;
        saveBtn.disabled = true;
        Bridge.portmapAdd(cleanName, cleanLocalPort, cleanTargetAddr, cleanTargetPort).then(
          function (created) {
            submitting = false;
            if (created) {
              upsertRule(created);
            }
            closeModal();
            if (created && created.status === 'fail') {
              toast(created.error || '本地端口绑定失败');
            } else {
              toast('已添加映射');
            }
          },
          function (err) {
            submitting = false;
            saveBtn.disabled = false;
            toast('添加失败：' + errMessage(err));
          }
        );
        return;
      }

      mappings.push({
        name: cleanName,
        localPort: String(cleanLocalPort),
        targetAddr: cleanTargetAddr,
        targetPort: String(cleanTargetPort),
        status: 'pending',
        enabled: true
      });
      render();
      closeModal();
    }

    if (IS_TAURI) {
      Bridge.validateMapForm(values).then(
        function (res) {
          commitIfValid(
            {
              name: res.name || '',
              localPort: res.localPort || '',
              targetAddr: res.targetAddr || '',
              targetPort: res.targetPort || ''
            },
            Boolean(res.valid)
          );
        },
        function () {
          var fallback = Validate.validateMapForm(values);
          commitIfValid(fallback.errors, fallback.valid);
        }
      );
      return;
    }

    var result = Validate.validateMapForm(values);
    commitIfValid(result.errors, result.valid);
  });

  document.getElementById('map-body').addEventListener('click', function (event) {
    var more = event.target.closest('[data-more]');
    if (!more) return;
    if (!IS_TAURI) {
      toast('功能开发中');
      return;
    }
    if (!sessionOnline || sessionWatchOnly) {
      toast(sessionWatchOnly ? '观看模式不可管理端口映射' : '远控会话未就绪');
      return;
    }
    var ruleIdAttr = more.getAttribute('data-rule-id');
    var rule =
      ruleIdAttr != null && isFinite(Number(ruleIdAttr))
        ? findRuleById(Number(ruleIdAttr))
        : mappings[Number(more.getAttribute('data-more'))];
    if (rule) {
      openDeleteModal(rule);
    }
  });

  document.getElementById('map-body').addEventListener('change', function (event) {
    var toggle = event.target.closest('[data-toggle]');
    if (!toggle) return;

    var nextEnabled = Boolean(toggle.checked);

    if (IS_TAURI) {
      if (!sessionOnline || sessionWatchOnly) {
        toggle.checked = !nextEnabled;
        toast(sessionWatchOnly ? '观看模式不可管理端口映射' : '远控会话未就绪');
        return;
      }
      var ruleIdAttr = toggle.getAttribute('data-rule-id');
      var ruleId = ruleIdAttr != null ? Number(ruleIdAttr) : NaN;
      var currentRule = isFinite(ruleId) ? findRuleById(ruleId) : null;
      if (!currentRule) return;

      currentRule.enabled = nextEnabled;
      render();
      Bridge.portmapToggle(ruleId, nextEnabled).then(
        function (updated) {
          if (updated) {
            upsertRule(updated);
          }
        },
        function (err) {
          var latest = findRuleById(ruleId);
          if (latest) {
            latest.enabled = !nextEnabled;
          }
          render();
          toast('切换失败：' + errMessage(err));
        }
      );
      return;
    }

    var index = Number(toggle.getAttribute('data-toggle'));
    var rule = mappings[index];
    if (!rule) return;
    var rowEl = toggle.closest('.pm-row');
    rule.enabled = nextEnabled;
    if (rowEl) {
      rowEl.classList.toggle('is-off', !nextEnabled);
    }
  });

  /* 名称列可排序（§6 W4「名称，可排序」） */
  var sortEl = document.getElementById('sort-name');
  function toggleSort() {
    sortAsc = sortAsc === null ? true : sortAsc ? false : null;
    render();
  }
  sortEl.addEventListener('click', toggleSort);
  sortEl.addEventListener('keydown', function (event) {
    if (event.key === 'Enter' || event.key === ' ') {
      event.preventDefault();
      toggleSort();
    }
  });

  /* Esc 关闭当前浮层（§7.3） */
  document.addEventListener('keydown', function (event) {
    if (event.key !== 'Escape') return;
    if (deleteModal && deleteModal.classList.contains('is-open')) {
      event.preventDefault();
      closeDeleteModal();
      return;
    }
    if (modal.classList.contains('is-open')) {
      event.preventDefault();
      closeModal();
    }
  });

  /* 初始渲染与 Tauri 真实数据同步 */
  render();
  validateLive();

  if (IS_TAURI) {
    var headerNameEl = document.getElementById('pm-device-name');
    var headerDotEl = document.getElementById('pm-status-dot');
    var headerStateEl = document.getElementById('pm-status-text');
    var deviceParam = new URLSearchParams(window.location.search).get('device') || '';
    var currentDeviceName = '';

    function syncToolbarAvailability() {
      var blocked = !sessionOnline || sessionWatchOnly;
      if (openAddBtn) {
        openAddBtn.disabled = blocked;
        openAddBtn.title = sessionWatchOnly
          ? '观看模式（只读）不可添加映射'
          : !sessionOnline
          ? '请先连接远控设备'
          : '添加端口映射';
      }
      if (blocked) {
        if (modal && modal.classList.contains('is-open')) {
          closeModal();
        }
        if (deleteModal && deleteModal.classList.contains('is-open')) {
          closeDeleteModal();
        }
      }
    }

    function updateHeaderStatus(state, nameOverride, watchOnlyFlag) {
      var isOnline = state === 'online';
      var isConnecting = state === 'connecting';
      var isWatchOnly = Boolean(isOnline && watchOnlyFlag);
      var wasReady = sessionOnline && !sessionWatchOnly;

      sessionOnline = isOnline;
      sessionWatchOnly = isWatchOnly;
      if (state === 'offline') {
        currentDeviceName = '';
      } else if (nameOverride) {
        currentDeviceName = String(nameOverride);
      }
      syncToolbarAvailability();

      if (headerNameEl) {
        headerNameEl.textContent =
          currentDeviceName ||
          deviceParam ||
          (isOnline || isConnecting ? '远程设备' : '未连接设备');
      }
      if (headerDotEl) {
        headerDotEl.classList.toggle('status-dot--online', isOnline);
        headerDotEl.classList.toggle('dot--offline', !isOnline);
      }
      if (headerStateEl) {
        headerStateEl.textContent = isWatchOnly
          ? '观看模式（只读）'
          : isOnline
          ? '已连接'
          : isConnecting
          ? '连接中…'
          : '未连接';
      }

      var nowReady = sessionOnline && !sessionWatchOnly;
      if (!wasReady && nowReady) {
        Bridge.portmapList()
          .then(applyRulesList)
          .catch(function (err) {
            toast(errMessage(err));
          });
      } else if (!isOnline && !isConnecting && mappings.length) {
        mappings = [];
        render();
      }
    }

    function applyActiveSessionInfo(info) {
      if (!info || !info.active) {
        updateHeaderStatus('offline', '', false);
        return;
      }
      var name = info.deviceName || info.target || '';
      if (info.connected) {
        updateHeaderStatus('online', name, Boolean(info.watchOnly));
      } else {
        updateHeaderStatus('connecting', name, false);
      }
    }

    function handleSessionUpdate(upd) {
      if (!upd || !upd.kind) {
        return;
      }
      var data = upd.data || {};
      var devName = data.device_name || data.device || '';
      if (upd.kind === 'negotiated') {
        var nextWatch = typeof data.watch_only === 'boolean' ? data.watch_only : sessionWatchOnly;
        updateHeaderStatus('online', devName, nextWatch);
      } else if (upd.kind === 'hud') {
        updateHeaderStatus('online', devName, sessionWatchOnly);
      } else if (upd.kind === 'connecting') {
        updateHeaderStatus('connecting', devName, false);
      } else if (upd.kind === 'ended') {
        updateHeaderStatus('offline', '', false);
      }
    }

    /* Tauri 真窗口默认先置为未连接，避免静态写死的 mock 设备名/在线态误导 */
    updateHeaderStatus('offline', '', false);

    /* 等待异步监听注册完成后再拉取初始活动会话与端口映射快照 */
    Promise.all([
      Promise.resolve(Bridge.onPortmapUpdate(applyRulesList)).catch(function () {}),
      Promise.resolve(
        typeof Bridge.onActiveSessionState === 'function'
          ? Bridge.onActiveSessionState(applyActiveSessionInfo)
          : null
      ).catch(function () {}),
      Promise.resolve(
        typeof Bridge.onSessionUpdate === 'function'
          ? Bridge.onSessionUpdate(handleSessionUpdate)
          : null
      ).catch(function () {})
    ]).then(function () {
      if (typeof Bridge.getActiveSessionInfo === 'function') {
        Bridge.getActiveSessionInfo()
          .then(function (info) {
            applyActiveSessionInfo(info);
          })
          .catch(function () {
            if (typeof Bridge.sessionSnapshot === 'function') {
              Bridge.sessionSnapshot().then(handleSessionUpdate).catch(function () {});
            }
          });
      } else {
        Bridge.portmapList()
          .then(applyRulesList)
          .catch(function (err) {
            toast(errMessage(err));
          });
        if (typeof Bridge.sessionSnapshot === 'function') {
          Bridge.sessionSnapshot()
            .then(handleSessionUpdate)
            .catch(function () {});
        }
      }
    });
  }
})();
