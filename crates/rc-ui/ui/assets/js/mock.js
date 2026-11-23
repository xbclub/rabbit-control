/* mock.js —— §6.0 Mock 数据总表（全 App 单一数据源）
 *
 * 唯一依据：docs/reference/design/08-ui-design.md §6.0「Mock 数据总表」
 * 纪律：所有名称 / 日期 / 类型 / 大小 / 路径逐字取自 §6.0，不得改写；
 *       新增条目必须先回填设计文档 §6.0。
 *
 * 暴露方式：window.RCMock（经典脚本；file:// 直接打开亦可用，不依赖 ES Module）
 * 使用范围：
 *   - P1 / P2 / P3：devices / defaultDeviceId / p3 / assist / account（app.js 消费）
 *   - 其余界面（W2 文件传输 / W4 端口映射 / S1 会话）的 mock 各自落在其
 *     页面脚本内；本文件同样按 §6.0 逐字收录了那几屏的数据
 *     （transfer / portmap / session），供后续收敛为单一数据源时直接取用。
 */
window.RCMock = (function () {
  'use strict';

  /* ── §6.0 设备（侧栏 3 台，全部在线=绿点） ──────────────────────
   * 名称 / 类型 / 图标逐字取自 §6.0；id 为路由用标识（非展示数据）。
   */
  var devices = [
    {
      id: 'desktop-q5q8e8r',
      name: 'DESKTOP-Q5Q8E8R',
      kind: 'windows',
      icon: 'app-window',
      online: true,
      isLocal: false
    },
    {
      id: 'yii-mbp',
      name: "yii's MacBook Pro",
      kind: 'mac',
      icon: 'laptop',
      online: true,
      isLocal: false
    },
    {
      id: 'yii-mac-mini',
      name: 'Y 的 Mac mini',
      kind: 'mac',
      icon: 'laptop',
      online: true,
      isLocal: false
    }
  ];

  /* §6.0 P3 全部设备：电脑 4（含本机「Y的 MacBook Pro」） + 手机/平板 4。 */
  var localMacBookPro = {
    id: 'y-local-mbp',
    name: 'Y的 MacBook Pro',
    kind: 'mac',
    icon: 'laptop',
    online: true,
    isLocal: true
  };

  var p3 = {
    computers: [localMacBookPro, devices[0], devices[1], devices[2]],
    mobiles: [
      {
        id: 'iphone-17-pro-max',
        name: 'iPhone 17 Pro Max',
        kind: 'phone',
        icon: 'smartphone',
        online: true,
        isLocal: false
      },
      {
        id: 'iqoo-pad2-pro',
        name: 'iQOO Pad2 Pro',
        kind: 'tablet',
        icon: 'tablet',
        online: true,
        isLocal: false
      },
      {
        id: 'oneplus-pgp110',
        name: 'OnePlusPGP110',
        kind: 'phone',
        icon: 'smartphone',
        online: true,
        isLocal: false
      },
      {
        id: 'oneplus-ace-pro',
        name: '一加 Ace Pro 全网通版',
        kind: 'phone',
        icon: 'smartphone',
        online: true,
        isLocal: false
      }
    ]
  };

  /* §6.0 P2 本设备：ID=183 435 140（三位一组空格分隔）；验证码=9ZGQ30ES。 */
  var assist = {
    deviceId: '183 435 140',
    code: '9ZGQ30ES',
    verifyModes: ['仅使用临时验证码', '自定义验证码']
  };

  /* §6.0 O3 账户：手机号=151******21。 */
  var account = {
    phone: '151******21'
  };

  /* ── §6.0 W2 左栏（本机）───────────────────────────────────────
   * 路径 /Users/yii/Downloads，8 行。
   */
  var transfer = {
    local: {
      label: 'Y的 MacBook Pro',
      pill: '本机',
      path: '/Users/yii/Downloads',
      files: [
        { name: 'openssl-3.5.tar.gz', modified: '2026年11月21日', type: 'GZip 归档', size: '9.44 MB', isDir: false },
        { name: 'Antigravity.dmg', modified: '2026年11月21日', type: '磁盘映像', size: '225.02 MB', isDir: false },
        { name: 'GUI-Server.exe', modified: '2026年11月19日', type: 'Windows 程序', size: '24.35 MB', isDir: false },
        { name: 'logs', modified: '2026年11月19日', type: '文件夹', size: '--', isDir: true },
        { name: 'logs.zip', modified: '2026年11月19日', type: 'Zip 归档', size: '7.44 MB', isDir: false },
        { name: 'expo-report.json', modified: '2026年11月18日', type: 'JSON', size: '1.24 KB', isDir: false },
        { name: 'notes.md', modified: '2026年11月18日', type: 'Markdown', size: '3.10 KB', isDir: false },
        { name: 'dump-1121.log', modified: '2026年11月17日', type: '日志', size: '56.72 MB', isDir: false }
      ]
    },
    remote: {
      label: "yii's MacBook Pro",
      pill: '远端',
      path: '/Users/yii/Documents/mycode/data',
      files: [
        { name: 'BiliBiliLiveRecorder.7z', modified: '2026年11月07日', type: '7z', size: '10.80 MB', isDir: false },
        { name: 'blivedm-go', modified: '2026年11月05日', type: '文件夹', size: '--', isDir: true },
        { name: 'browserclient', modified: '2026年11月05日', type: '文件夹', size: '--', isDir: true },
        { name: 'danmaku-coder.data', modified: '2026年11月06日', type: 'data', size: '21.29 MB', isDir: false },
        { name: 'danmaku-renderer.exe', modified: '2026年11月07日', type: 'exe', size: '15.85 MB', isDir: false },
        { name: 'docs', modified: '2026年11月07日', type: '文件夹', size: '--', isDir: true },
        { name: 'README.md', modified: '2026年11月07日', type: 'Markdown', size: '2.42 KB', isDir: false },
        { name: 'stream-config.toml', modified: '2026年11月06日', type: 'TOML', size: '1.05 KB', isDir: false }
      ]
    },
    /* 传输队列 4 条（状态：sent=已发送 / progress=进行中 / failed=失败）。 */
    queue: [
      {
        name: 'BiliBiliLiveRecorder-gui-master.7z',
        status: 'sent',
        size: '10.80 MB',
        from: '/Users/yii/Downloads/…ter.7z',
        to: '\\Users\\yii\\data\\…coder'
      },
      {
        name: 'Stream-ug.na…22:11:18.har',
        status: 'sent',
        size: '6.94 KB',
        from: '/Users/yii/Downloads/…18.har',
        to: '\\Users\\yii\\Downloads'
      },
      {
        name: 'danmaku-coder.data',
        status: 'progress',
        progress: 47,
        size: '21.29 MB',
        from: '/Users/yii/Downloads/…data',
        to: '\\Users\\yii\\data'
      },
      {
        name: 'GUI-Server.exe',
        status: 'failed',
        failReason: '连接中断',
        size: '24.35 MB',
        from: '/Users/yii/Downloads/…exe',
        to: '\\Users\\yii\\data'
      }
    ]
  };

  /* §6.0 W4 映射 3 行（状态：ok / fail / connecting）。 */
  var portmap = {
    rows: [
      { name: 'deepseek harness', localPort: '3080', target: '127.0.0.1', targetPort: '3080', status: 'ok' },
      { name: 'ssh-tunnel', localPort: '2222', target: '10.0.0.8', targetPort: '22', status: 'fail' },
      { name: 'jupyter-lab', localPort: '8888', target: '127.0.0.1', targetPort: '8888', status: 'connecting' }
    ]
  };

  /* §6.0 S1 会话：设备 / 屏幕 tab 文案 / HUD 静态值（计时从 00:00:00 起每秒 +1）。 */
  var session = {
    device: "yii's MacBook Pro",
    screen: '显示屏1 (Built-in Retina Display)',
    hud: {
      elapsed: '00:00:11',
      transport: 'UDP LAN',
      fps: '2 fps',
      bitrate: '227.2 Kbps',
      rtt: '110 ms',
      frame: '20 ms frm.',
      loss: '0.0% loss',
      resolution: '2K'
    }
  };

  return {
    /* App 初始状态：默认选中 yii's MacBook Pro → P1（08 §2）。 */
    defaultDeviceId: 'yii-mbp',
    devices: devices,
    p3: p3,
    assist: assist,
    account: account,
    transfer: transfer,
    portmap: portmap,
    session: session
  };
})();
