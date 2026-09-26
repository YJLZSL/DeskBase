// 项目体检：代码质量 + 文件冗余
const fs = require('fs');
const path = require('path');

const ROOT = 'E:/AIGC/DeskBase';
const skip = new Set(['.git', 'node_modules']);

function walk(dir, out = []) {
  let ents;
  try { ents = fs.readdirSync(dir, { withFileTypes: true }); } catch { return out; }
  for (const e of ents) {
    if (skip.has(e.name)) continue;
    const p = path.join(dir, e.name);
    if (e.isDirectory()) walk(p, out);
    else out.push(p);
  }
  return out;
}

function dirSize(dir) {
  let n = 0;
  for (const f of walk(dir)) {
    try { n += fs.statSync(f).size; } catch {}
  }
  return n;
}
const MB = (b) => (b / 1024 / 1024).toFixed(1) + ' MB';

console.log('=== 一、代码规模 ===');
const rs = walk(path.join(ROOT, 'app/src')).filter((f) => f.endsWith('.rs'));
const ui = walk(path.join(ROOT, 'app/ui')).filter((f) => /\.(js|css|html)$/.test(f));
let rsLines = 0, uiLines = 0;
for (const f of rs) rsLines += fs.readFileSync(f, 'utf8').split('\n').length;
for (const f of ui) uiLines += fs.readFileSync(f, 'utf8').split('\n').length;
console.log('  Rust：' + rs.length + ' 个文件，' + rsLines + ' 行');
console.log('  前端：' + ui.length + ' 个文件，' + uiLines + ' 行');

console.log('\n=== 二、前端函数重名（跨文件同名容易出隐蔽问题）===');
const fns = new Map();
for (const f of ui.filter((x) => x.endsWith('.js'))) {
  const t = fs.readFileSync(f, 'utf8');
  for (const m of t.matchAll(/^\s*function\s+([A-Za-z_$][\w$]*)/gm)) {
    const k = m[1];
    if (!fns.has(k)) fns.set(k, []);
    fns.get(k).push(path.basename(f));
  }
}
let dupN = 0;
for (const [k, arr] of fns) {
  const u = [...new Set(arr)];
  if (u.length > 1) { console.log('  ⚠ ' + k + ' 出现在：' + u.join(', ')); dupN++; }
}
console.log('  重名函数：' + dupN + ' 组');

console.log('\n=== 三、Rust 里被显式允许的 dead_code ===');
let allowN = 0;
for (const f of rs) {
  const t = fs.readFileSync(f, 'utf8');
  const n = (t.match(/#\[allow\(dead_code\)\]/g) || []).length;
  if (n) { console.log('  ' + path.relative(ROOT, f).replace(/\\/g, '/') + '：' + n + ' 处'); allowN += n; }
}
console.log('  合计：' + allowN + ' 处');

console.log('\n=== 四、磁盘占用（前 12 大目录）===');
const top = [];
for (const e of fs.readdirSync(ROOT, { withFileTypes: true })) {
  if (!e.isDirectory() || e.name === '.git') continue;
  top.push([e.name, dirSize(path.join(ROOT, e.name))]);
}
top.sort((a, b) => b[1] - a[1]);
for (const [n, s] of top.slice(0, 12)) console.log('  ' + n.padEnd(16) + MB(s));

console.log('\n=== 五、看着像"攒下来的"东西 ===');
const suspects = [
  ['local-docs/runs', '历史界面走查产物（截图 + 报告）'],
  ['dist', '发布产物档案（旧版本 zip）'],
  ['poc', '调研期产物'],
  ['local-docs/tools', '本地脚本'],
  ['testdata', '测试夹具'],
];
for (const [rel, desc] of suspects) {
  const p = path.join(ROOT, rel);
  if (!fs.existsSync(p)) { console.log('  （无 ' + rel + '）'); continue; }
  const entries = fs.readdirSync(p);
  console.log('  ' + rel.padEnd(20) + entries.length + ' 项 · ' + MB(dirSize(p)) + '  — ' + desc);
}

console.log('\n=== 六、dist 里的旧版本（保留最近 2 个就够）===');
const dist = path.join(ROOT, 'dist');
if (fs.existsSync(dist)) {
  const zips = fs.readdirSync(dist).filter((f) => f.endsWith('.zip')).sort();
  console.log('  zip 共 ' + zips.length + ' 个：');
  zips.forEach((z) => console.log('    ' + z));
}

console.log('\n=== 七、local-docs/runs 明细 ===');
const runs = path.join(ROOT, 'local-docs/runs');
if (fs.existsSync(runs)) {
  const ds = fs.readdirSync(runs).filter((d) => fs.statSync(path.join(runs, d)).isDirectory()).sort();
  console.log('  共 ' + ds.length + ' 个走查目录，最早 ' + ds[0] + '，最新 ' + ds[ds.length - 1]);
  console.log('  合计 ' + MB(dirSize(runs)));
}
