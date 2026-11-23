/* validate.js —— 08-ui-design.md §7.1 输入校验（唯一纯逻辑校验，W4 添加映射表单）
 *
 * 纯函数，无 DOM 依赖：
 *   - 浏览器：`<script src=".../validate.js">` 后挂到 `window.RCValidate`
 *   - Node：`require('./validate.js')` 直接拿到同一 API（便于单测）
 *
 * 错误文案逐字取自 §7.1，UI 直接渲染，不另行拼装。
 */

(function (root, factory) {
  'use strict';
  var api = factory();
  if (typeof module === 'object' && module.exports) {
    module.exports = api;
  } else {
    root.RCValidate = api;
  }
})(typeof globalThis !== 'undefined' ? globalThis : this, function () {
  'use strict';

  /* §7.1 三条错误文案（逐字） */
  var MESSAGES = {
    port: '端口需为 1–65535 的整数',
    targetAddr: '目标地址不能为空',
    name: '名称需为 1–64 个字符'
  };

  var PORT_PATTERN = /^[0-9]+$/;
  var NAME_MAX_LENGTH = 64;

  function toTrimmedString(value) {
    if (value === null || value === undefined) {
      return '';
    }
    return String(value).trim();
  }

  /* 本地端口 / 目标端口：1–65535 整数（0、70000、非数字、空 → 错误） */
  function validatePort(value) {
    var raw = toTrimmedString(value);
    if (!PORT_PATTERN.test(raw)) {
      return MESSAGES.port;
    }
    var port = Number(raw);
    if (!Number.isInteger(port) || port < 1 || port > 65535) {
      return MESSAGES.port;
    }
    return '';
  }

  /* 目标地址：非空且不能包含空白或控制字符（IPv4/IPv6/域名严格校验留 TODO，§7.1） */
  function validateTargetAddr(value) {
    var raw = toTrimmedString(value);
    if (raw === '') {
      return MESSAGES.targetAddr;
    }
    if (/[\s\u0000-\u001f\u007f]/.test(raw)) {
      return '目标地址不能包含空白或控制字符';
    }
    return '';
  }

  /* 名称：非空且 ≤64 字符（按 UTF-16 码元计数，与 JS 字符串长度一致） */
  function validateName(value) {
    var raw = toTrimmedString(value);
    if (raw.length === 0 || raw.length > NAME_MAX_LENGTH) {
      return MESSAGES.name;
    }
    return '';
  }

  /* 全表单：四个字段各自返回 ''（通过）或错误文案；valid = 全通过 */
  function validateMapForm(form) {
    var input = form || {};
    var errors = {
      name: validateName(input.name),
      localPort: validatePort(input.localPort),
      targetAddr: validateTargetAddr(input.targetAddr),
      targetPort: validatePort(input.targetPort)
    };
    return {
      errors: errors,
      valid:
        !errors.name && !errors.localPort && !errors.targetAddr && !errors.targetPort
    };
  }

  return {
    MESSAGES: MESSAGES,
    NAME_MAX_LENGTH: NAME_MAX_LENGTH,
    validatePort: validatePort,
    validateTargetAddr: validateTargetAddr,
    validateName: validateName,
    validateMapForm: validateMapForm
  };
});
