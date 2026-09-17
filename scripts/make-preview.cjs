/**
 * 生成 UI 预览文件
 * ============================================================
 * 为什么需要它：
 *   app/ui/index.html 用 `<link href="theme.css">` 与 `<script src="app.js">`
 *   引用资源，真实运行时这些相对路径由 deskbase:// 协议解析（见 app/src/assets.rs）。
 *   直接双击 index.html 会走 file://，既没有 IPC，字体也会被浏览器的 CORS 挡掉。
 *
 *   这个脚本把 HTML / CSS / JS / 字体合成一个**自包含**文件，并注入一个假的 IPC 层，
 *   让它在普通浏览器里双击就能完整渲染出界面外观与交互，用于设计评审。
 *
 * 用法：
 *   node scripts/make-preview.cjs
 * 产出：
 *   app/ui/preview.html  （可直接双击打开，也可在 WorkBuddy 里预览）
 */

const fs = require('fs');
const path = require('path');

const ROOT = path.resolve(__dirname, '..');
const UI = path.join(ROOT, 'app', 'ui');
const FONT_REL = 'fonts/smiley-sans-oblique.woff2';

const html = fs.readFileSync(path.join(UI, 'index.html'), 'utf8');
const css = fs.readFileSync(path.join(UI, 'theme.css'), 'utf8');
const js = fs.readFileSync(path.join(UI, 'app.js'), 'utf8');
const font = fs.readFileSync(path.join(UI, FONT_REL));

const LINK_TAG = '<link rel="stylesheet" href="theme.css" />';
const SCRIPT_TAG = '<script src="app.js"></script>';

// 结构变了就报错，而不是安静地生成一个没样式的预览
if (!html.includes(LINK_TAG)) {
  console.error('✗ index.html 里找不到样式引用标记：' + LINK_TAG);
  console.error('  预览脚本与 index.html 的结构已经对不上了，请同步 scripts/make-preview.cjs');
  process.exit(1);
}
if (!html.includes(SCRIPT_TAG)) {
  console.error('✗ index.html 里找不到脚本引用标记：' + SCRIPT_TAG);
  process.exit(1);
}

// 字体必须内联成 data: URI —— 从 file:// 加载字体受 CORS 限制，相对路径会被拦掉
const fontB64 = font.toString('base64');
const marker = `url("${FONT_REL}")`;
if (!css.includes(marker)) {
  console.error('✗ theme.css 里找不到字体引用：' + marker);
  process.exit(1);
}
const cssInlined = css.replace(marker, `url("data:font/woff2;base64,${fontB64}")`);

// 假的 IPC：用内存里的示例数据模拟 Rust 侧
const mock = `
/* ===== 预览模式：模拟 Rust 侧的 IPC =====
   这段只在预览文件里存在，真实应用里由 Rust 提供。 */
(function () {
  var notes = [
    { id: '01J8XK2M9P0000000000000001', title: '诊所用药记录', content: '张爷爷  氨氯地平  每日一次  早饭后\\n李奶奶  二甲双胍  每日两次  随餐\\n\\n注意：王大爷这个月该复诊了。', created_at: Date.now() - 86400000 * 6, updated_at: Date.now() - 600000 },
    { id: '01J8XK2M9P0000000000000002', title: '客户报价 · 林维', content: '10 月 3 日  婚礼跟拍  8000 元\\n10 月 18 日 产品拍摄  4200 元\\n\\n备注：第 18 日那场要提前一天去踩点。', created_at: Date.now() - 86400000 * 3, updated_at: Date.now() - 3600000 },
    { id: '01J8XK2M9P0000000000000003', title: '换机待办', content: '1. 把数据目录整个拷到移动硬盘\\n2. 确认备份健康检查是绿的\\n3. 新机器上解压便携版，指向同一个数据目录', created_at: Date.now() - 86400000, updated_at: Date.now() - 7200000 }
  ];
  var seq = 0, pending = {};

  window.ipc = {
    postMessage: function (raw) {
      var req = JSON.parse(raw);
      var self = this;
      setTimeout(function () {
        var data = null, error = null;
        if (req.cmd === 'app.info') {
          data = { name: 'DeskBase 桌库', version: '0.1.0-alpha.1（预览）',
                   dataDir: 'D:\\\\DeskBaseData', noteCount: notes.length, uptimeMs: 1234 };
        } else if (req.cmd === 'note.list') {
          data = notes.map(function (n) { return { id: n.id, title: n.title, updated_at: n.updated_at }; });
        } else if (req.cmd === 'note.get') {
          var f = notes.filter(function (n) { return n.id === req.args.id; })[0];
          if (f) data = f; else error = '笔记不存在';
        } else if (req.cmd === 'note.create') {
          var n = { id: 'preview-' + (++seq), title: req.args.title || '未命名笔记',
                    content: '', created_at: Date.now(), updated_at: Date.now() };
          notes.unshift(n); data = n;
        } else if (req.cmd === 'note.save') {
          notes.forEach(function (n) {
            if (n.id === req.args.id) { n.title = req.args.title; n.content = req.args.content; n.updated_at = Date.now(); }
          });
          data = { updatedAt: Date.now() };
        } else if (req.cmd === 'note.delete') {
          notes = notes.filter(function (n) { return n.id !== req.args.id; });
          data = {};
        } else { error = '预览模式不支持的命令: ' + req.cmd; }

        window.__deskbase.resolve(JSON.stringify({ id: req.id, ok: !error, data: data, error: error }));
      }, 120);
    }
  };

  // 预览提示条
  window.addEventListener('DOMContentLoaded', function () {
    var b = document.createElement('div');
    b.style.cssText = 'position:fixed;top:0;left:0;right:0;z-index:99;background:#A63A2E;color:#fff;' +
      'font:12px/1.6 system-ui;text-align:center;padding:3px';
    b.textContent = '预览模式 · 数据是假的，字体是真的（内联），仅用于看界面外观';
    document.body.appendChild(b);
    document.querySelector('.app').style.paddingTop = '0';
  });
})();
`;

const out = html
  .replace(LINK_TAG, `<style>\n${cssInlined}\n</style>`)
  .replace(SCRIPT_TAG, `<script>\n${mock}\n${js}\n</script>`);

const dest = path.join(UI, 'preview.html');
fs.writeFileSync(dest, out, 'utf8');
console.log('✔ 已生成预览文件：' + dest);
console.log('  大小：' + (fs.statSync(dest).size / 1024).toFixed(1) + ' KB');
console.log('  内联字体：' + (font.length / 1024).toFixed(1) + ' KB（base64 后约 ' +
  (fontB64.length / 1024).toFixed(1) + ' KB）');
console.log('  可直接用浏览器打开。注意：真实应用走 deskbase:// 协议，不需要这个文件。');
