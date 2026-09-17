/**
 * 主题对比度校验
 * ============================================================
 * 对应 docs/18 第 4.2 节与 P2-07：每个主题的每一组「文字 / 底色」都必须过线。
 * 这是纯计算，比肉眼看可靠得多 —— 9 个主题 × 若干组合，眼睛是看不过来的。
 *
 * 用法：
 *   node scripts/check-contrast.cjs            校验，不过则退出码 1
 *   node scripts/check-contrast.cjs --all      顺带打印全部实测比值
 *
 * 阈值依据 WCAG 2.1：
 *   正文      ≥ 4.5:1（--ink / --ink-2 落在纸面与卡片上）
 *   大字与次要 ≥ 3.0:1（--ink-3 是提示文字，属于「次要」）
 *   按钮文字   ≥ 4.5:1
 * 另外把主文字（--ink）卡在 ≥ 7:1：办公工具里长时间阅读的是正文，
 * 只到 4.5 会累。
 */

const fs = require('fs');
const path = require('path');

const CSS = path.resolve(__dirname, '..', 'app', 'ui', 'theme.css');
const css = fs.readFileSync(CSS, 'utf8');

// ---------- 解析主题块 ----------
// 形如 [data-theme="xuan"] { --paper: #f6f2e9; ... }，以及 :root, [data-theme="xuan"] { ... }
const THEME_RE = /^\s*(?::root\s*,\s*)?\[data-theme="([a-z-]+)"\]\s*\{([\s\S]*?)\}/gm;

function parseVars(body) {
  const out = {};
  const re = /--([a-z0-9-]+)\s*:\s*([^;]+);/gi;
  let m;
  while ((m = re.exec(body))) out[m[1]] = m[2].trim();
  return out;
}

function hex(v) {
  const m = /^#([0-9a-f]{6})$/i.exec(v.trim());
  if (!m) return null;
  const n = parseInt(m[1], 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}

/** WCAG 相对亮度 */
function lum([r, g, b]) {
  const f = (c) => {
    const s = c / 255;
    return s <= 0.03928 ? s / 12.92 : Math.pow((s + 0.055) / 1.055, 2.4);
  };
  return 0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b);
}

function ratio(a, b) {
  const la = lum(a);
  const lb = lum(b);
  return (Math.max(la, lb) + 0.05) / (Math.min(la, lb) + 0.05);
}

// ---------- 收集 :root 默认值（宣纸），供主题缺项时继承 ----------
const rootBlock = /^:root\s*\{([\s\S]*?)\}/m.exec(css);
const defaults = rootBlock ? parseVars(rootBlock[1]) : {};

const THEMES = [];
let m;
const byId = new Map();
while ((m = THEME_RE.exec(css))) {
  const id = m[1];
  // 同一个主题可能出现多次：默认值块（`:root, [data-theme="xuan"]`）以及
  // 后续的补充规则（例如 `[data-theme="hc-light"], [data-theme="hc-dark"] { --texture-opacity: 0 }`）。
  // 后者只覆盖部分 token，所以必须**合并**而不是当成一个独立的主题定义 ——
  // 否则会解析出一个"缺一堆 token"的假主题。
  const vars = { ...defaults, ...(byId.get(id) || {}), ...parseVars(m[2]) };
  byId.set(id, vars);
}
for (const [id, vars] of byId) THEMES.push({ id, vars });

if (!THEMES.length) {
  console.error('✗ 没有解析到任何主题，theme.css 的结构变了？');
  process.exit(1);
}

/** 需要校验的组合：[文字 token, 底色 token, 最低比值, 说明] */
const CHECKS = [
  ['ink', 'paper', 7.0, '正文 / 纸面'],
  ['ink', 'card', 7.0, '正文 / 卡片'],
  ['ink-2', 'paper', 4.5, '次要文字 / 纸面'],
  ['ink-2', 'card', 4.5, '次要文字 / 卡片'],
  ['ink-3', 'paper', 3.0, '提示文字 / 纸面'],
  ['ink-3', 'card', 3.0, '提示文字 / 卡片'],
  ['ink-2', 'paper-2', 4.5, '次要文字 / 侧栏'],
  ['ink-3', 'paper-2', 3.0, '提示文字 / 侧栏'],
  ['accent', 'paper', 3.0, '强调色 / 纸面'],
  ['accent', 'accent-soft', 4.5, '强调色 / 强调底（导航选中态）'],
  ['accent-ink', 'accent', 4.5, '主按钮文字 / 强调底'],
  ['seal', 'paper', 3.0, '朱砂 / 纸面'],
  ['danger', 'card', 4.5, '错误色 / 卡片'],
  ['success', 'card', 4.5, '成功色 / 卡片'],
  ['warning', 'card', 4.5, '警告色 / 卡片'],
];

const showAll = process.argv.includes('--all');
let failures = 0;

console.log('主题对比度校验 · ' + THEMES.length + ' 个主题 × ' + CHECKS.length + ' 组\n');

for (const t of THEMES) {
  const rows = [];
  let bad = 0;
  for (const [fgKey, bgKey, min, label] of CHECKS) {
    const fg = hex(t.vars[fgKey] || '');
    const bg = hex(t.vars[bgKey] || '');
    if (!fg || !bg) {
      rows.push({ label, r: null, min, ok: false, note: '缺 token' });
      bad++;
      continue;
    }
    const r = ratio(fg, bg);
    const ok = r >= min;
    if (!ok) bad++;
    rows.push({ label, r, min, ok });
  }
  failures += bad;

  const mark = bad ? '✗' : '✔';
  console.log(`${mark} ${t.id.padEnd(9)} ${bad ? bad + ' 项不达标' : '全部通过'}`);
  for (const row of rows) {
    if (!showAll && row.ok) continue;
    const rTxt = row.r === null ? '  —  ' : row.r.toFixed(2);
    console.log(
      `    ${row.ok ? ' ' : '!'} ${row.label.padEnd(26)} ${String(rTxt).padStart(6)} : 1` +
        `   （要求 ≥ ${row.min.toFixed(1)}）${row.note ? '  ' + row.note : ''}`
    );
  }
}

// ---------- 结构完整性：每个主题都必须给全 token ----------
const NEEDED = [
  'paper', 'paper-2', 'card', 'ink', 'ink-2', 'ink-3', 'line', 'line-strong',
  'accent', 'accent-soft', 'accent-ink', 'seal', 'success', 'warning', 'danger', 'selection',
];
let missing = 0;
for (const t of THEMES) {
  const lack = NEEDED.filter((k) => !t.vars[k]);
  if (lack.length) {
    console.log(`✗ ${t.id} 缺少 token：${lack.join(', ')}`);
    missing += lack.length;
  }
}
failures += missing;

console.log('');
if (failures) {
  console.error(`✗ 共 ${failures} 项未通过`);
  process.exit(1);
}
console.log('✔ 全部通过（9 个主题的 token 齐全，对比度全部达标）');
