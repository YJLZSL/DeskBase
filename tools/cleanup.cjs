// 清理冗余：先列清单（dry-run），确认无误再删
const fs = require('fs');
const path = require('path');
const ROOT = 'E:/AIGC/DeskBase';
const MB = (b) => (b / 1024 / 1024).toFixed(1) + ' MB';

function dirSize(d) {
  let n = 0;
  const st = [];
  const walk = (p) => {
    let ents;
    try { ents = fs.readdirSync(p, { withFileTypes: true }); } catch { return; }
    for (const e of ents) {
      const q = path.join(p, e.name);
      if (e.isDirectory()) walk(q);
      else { try { n += fs.statSync(q).size; } catch {} }
    }
  };
  walk(d);
  return n;
}

const plan = [];

// ---- 1) poc 的构建缓存（源码留着 —— AGENTS 明确说"缓存可删，源码不要删"）----
for (const sub of ['electron-shell', 'rust-webview-shell']) {
  for (const cache of ['node_modules', 'target-msvc', 'dist', 'build']) {
    const p = path.join(ROOT, 'poc', sub, cache);
    if (fs.existsSync(p)) plan.push({ p, why: '调研期构建缓存（源码保留）' });
  }
}

// ---- 2) dist 里的旧版本 zip（保留了 GitHub Release，本地副本是冗余）----
const distDir = path.join(ROOT, 'dist');
if (fs.existsSync(distDir)) {
  const zips = fs.readdirSync(distDir)
    .filter((f) => /^deskbase-.*\.zip$/.test(f))
    .map((f) => ({ f, at: fs.statSync(path.join(distDir, f)).mtimeMs }))
    .sort((a, b) => b.at - a.at);
  const KEEP = 3;
  for (const z of zips.slice(KEEP)) {
    plan.push({ p: path.join(distDir, z.f), why: '旧版本 zip（GitHub Release 里都有）' });
  }
  console.log('dist：zip 共 ' + zips.length + ' 个，保留最近 ' + KEEP + ' 个：');
  zips.slice(0, KEEP).forEach((z) => console.log('    ✔ 保留 ' + z.f));
}

// ---- 3) local-docs/runs 的旧走查目录 ----
const runsDir = path.join(ROOT, 'local-docs/runs');
if (fs.existsSync(runsDir)) {
  const ds = fs.readdirSync(runsDir)
    .filter((d) => { try { return fs.statSync(path.join(runsDir, d)).isDirectory(); } catch { return false; } })
    .map((d) => ({ d, at: fs.statSync(path.join(runsDir, d)).mtimeMs }))
    .sort((a, b) => b.at - a.at);
  const KEEP = 5;
  for (const x of ds.slice(KEEP)) {
    plan.push({ p: path.join(runsDir, x.d), why: '旧界面走查产物（截图+报告）' });
  }
  console.log('runs：共 ' + ds.length + ' 个走查目录，保留最近 ' + KEEP + ' 个：');
  ds.slice(0, KEEP).forEach((x) => console.log('    ✔ 保留 ' + x.d));
}

// ---- 汇总 ----
console.log('\n=== 将删除（' + plan.length + ' 项）===');
let total = 0;
for (const x of plan) {
  const s = dirSize(x.p);
  total += s;
  console.log('  ' + MB(s).padStart(10) + '  ' + path.relative(ROOT, x.p).replace(/\\/g, '/'));
}
console.log('\n合计可释放：' + MB(total));
fs.writeFileSync('C:/Users/23501/AppData/Local/Temp/cleanup-plan.json', JSON.stringify(plan, null, 2), 'utf8');
console.log('（清单已存盘；加 --go 才真的删）');

if (process.argv.includes('--go')) {
  console.log('\n=== 执行删除 ===');
  let done = 0, freed = 0;
  for (const x of plan) {
    const s = dirSize(x.p);
    try {
      fs.rmSync(x.p, { recursive: true, force: true });
      freed += s; done++;
      console.log('  ✔ ' + path.relative(ROOT, x.p).replace(/\\/g, '/'));
    } catch (e) {
      console.log('  ✘ ' + path.relative(ROOT, x.p).replace(/\\/g, '/') + ' — ' + e.message);
    }
  }
  console.log('\n删了 ' + done + ' 项，释放 ' + MB(freed));
}
