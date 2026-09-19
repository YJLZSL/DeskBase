#!/usr/bin/env node
/* ============================================================
   DeskBase 体积守卫（check-size.cjs）
   ============================================================
   存在的理由：5.9 MB 这个数字不是天上掉的，是「WebView2 + 零前端框架」
   换来的。它也是这个产品在同类里少有的硬优势 —— 而优势会**悄悄**消失：
   加一个依赖、开一个默认 feature，多几百 KB 是看不出来的，
   等发现时已经涨了一倍。

   历史上真发生过一次：`image` crate 的默认特性会拉进 AV1 编码器（+666 KB）。
   当时靠人眼看出来纯属运气。所以把它变成会响的门禁。

   阈值 10 MB（依据 `local-docs/reference/22-engineering-deep-dive.md`）：
   当前 5.95 MB，留一倍余量 —— 不是卡死，是**涨到需要解释的时候必须解释**。

   用法：
     node scripts/check-size.cjs                检查 release 产物
     node scripts/check-size.cjs --zip          同时检查便携包
   ============================================================ */

const fs = require('fs');
const path = require('path');

const ROOT = path.resolve(__dirname, '..');
const EXE = path.join(ROOT, 'app', 'target', 'release', 'deskbase.exe');
const THRESHOLD = 10 * 1024 * 1024; // 10 MB
const MB = 1024 * 1024;

const problems = [];

function mb(bytes) {
  return (bytes / MB).toFixed(3) + ' MB';
}

// ---------- exe ----------
if (!fs.existsSync(EXE)) {
  console.error(`✖ 找不到 release 产物：${EXE}`);
  console.error('  先跑 `node scripts/build.cjs`（体积守卫检查的是真产物，不是估算）。');
  process.exit(1);
}

const exeSize = fs.statSync(EXE).size;
const ratio = exeSize / THRESHOLD;

console.log(`体积守卫 · 阈值 ${mb(THRESHOLD)}`);
console.log('');
console.log(`  ${mb(exeSize).padStart(10)}  deskbase.exe  （占阈值 ${(ratio * 100).toFixed(1)}%）`);
console.log(`  ${mb(THRESHOLD - exeSize).padStart(10)}  余量`);

if (exeSize > THRESHOLD) {
  problems.push(
    `release 产物 ${mb(exeSize)} 超过阈值 ${mb(THRESHOLD)}（超 ${mb(exeSize - THRESHOLD)}）`,
  );
}

// 90% 是个提醒线，不是失败线：到这里就该有人去看一眼是谁涨的
if (ratio >= 0.9 && exeSize <= THRESHOLD) {
  console.log('');
  console.log('  ⚠ 已用掉阈值的 90% 以上 —— 该查一下最近哪次依赖变更带来的增长。');
}

// ---------- 便携包（可选） ----------
if (process.argv.includes('--zip')) {
  const cargoToml = fs.readFileSync(path.join(ROOT, 'app', 'Cargo.toml'), 'utf8');
  const m = cargoToml.match(/^version\s*=\s*"([^"]+)"/m);
  const version = m ? m[1] : '0.0.0';
  const zip = path.join(ROOT, 'dist', `deskbase-${version}-windows-x64-portable.zip`);
  if (fs.existsSync(zip)) {
    const zipSize = fs.statSync(zip).size;
    console.log(`  ${mb(zipSize).padStart(10)}  ${path.basename(zip)}  （压缩后，未计入阈值）`);
  } else {
    console.log(`  （未找到 ${path.basename(zip)}，跳过 —— 先跑 node scripts/package.cjs）`);
  }
}

console.log('');

if (problems.length) {
  for (const p of problems) console.error(`✖ ${p}`);
  console.error('');
  console.error('  体积涨了不一定是错，但**必须有理由**：');
  console.error('  去 app/Cargo.toml 看新依赖成不成立，能关的默认 feature 就关掉。');
  process.exit(1);
}

console.log('✔ 通过（release 产物在阈值内）');
