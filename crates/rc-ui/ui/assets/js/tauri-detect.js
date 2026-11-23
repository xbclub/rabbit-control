/* tauri-detect.js —— Tauri 真窗口检测（工具窗口的唯一实现）
 *
 * 工具窗口在 window.rs 里统一设为 titleBarStyle: Overlay + hidden_title，
 * macOS 原生交通灯已由窗口提供；页面里自绘的那组（.traffic / .tm-traffic /
 * .rc-traffic）必须隐藏，否则两套灯会重叠。浏览器 file:// 预览时保留自绘灯，
 * 便于逐屏比对设计稿。
 *
 * 用法：在 <body> 起始处同步引入（非 defer），首帧前打好 .is-tauri，
 * 避免原生灯与自绘灯同时出现一帧。
 */
(function () {
  "use strict";
  if (window.__TAURI_INTERNALS__ || window.__TAURI__) {
    document.body.classList.add("is-tauri");
  }
})();
