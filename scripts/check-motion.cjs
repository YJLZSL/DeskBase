/**
 * 动效合规静态检查
 * ============================================================
 * 对应 docs/18 原则 7.1#2：「只动 transform / opacity」。
 *
 * 为什么必须有这个脚本：
 *   动画一个布局属性（width / height / top / left / margin / grid-template-*）
 *   会让浏览器每帧重算几何 —— 在 120Hz 屏上等于每秒 120 次重排。这类代码
 *   写下去当时看不出问题，等列表长到一万行才开始掉帧，而那时已经很难定位。
 *   所以把它做成一道机器检查，而不是靠人记住。
 *
 * 同时检查另外两条容易漏的：
 *   · `.btn:active` 那类硬编码时长 —— 它们不响应动效档位，用户调到「关」仍会动
 *   · `transition: all` —— 会捕获所有属性变化，是最常见的隐形性能坑
 *
 * 用法：
 *   node scripts/check-motion.cjs          检查，不过则退出码 1
 *   node scripts/check-motion.cjs --list   同时列出全部被检查的声明
 *
 * 豁免：确有必要时在声明上一行写注释 `/* motion-allow: 理由 *\/`。
 *   豁免必须带理由，且会在输出里逐条列出来接受复核。
 */

const fs = require('fs');
const path = require('path');

const ROOT = path.resolve(__dirname, '..');
const TARGETS = [
  path.join(ROOT, 'app', 'ui', 'theme.css'),
];

/** 布局属性：动画它们会触发布局重算 */
const LAYOUT_PROPS = [
  'width', 'height', 'min-width', 'max-width', 'min-height', 'max-height',
  'top', 'right', 'bottom', 'left',
  'margin', 'margin-top', 'margin-right', 'margin-bottom', 'margin-left',
  'padding', 'padding-top', 'padding-right', 'padding-bottom', 'padding-left',
  'border-width',
  'font-size', 'line-height', 'letter-spacing',
  'grid-template-columns', 'grid-template-rows', 'grid-template-areas',
  'flex', 'flex-basis', 'flex-grow', 'flex-shrink',
  'inset', 'gap', 'row-gap', 'column-gap',
];

/** 绘制属性：不重排但每帧重绘。允许但提示 —— box-shadow 是最典型的 */
const PAINT_PROPS = ['box-shadow', 'background-position', 'filter', 'clip-path'];

/** 只合成：完全安全 */
const OK_PROPS = ['transform', 'opacity', 'color', 'background-color', 'border-color', 'outline-color', 'fill', 'stroke', 'visibility'];

const listAll = process.argv.includes('--list');
const problems = [];
const allowed = [];

/** 从一段声明块里取出某个属性（含 `transition: a, b` 简写）的值 */
function transitionProps(value) {
  return value
    .split(',')
    .map((part) => part.trim().split(/\s+/)[0])
    .filter(Boolean);
}

/**
 * 把源码切成「声明」而不是「行」。
 *
 * 为什么必须这样：CSS 里 `transition:` 经常写成多行 ——
 *   transition: transform var(--dur-normal) ...,
 *     height var(--dur-normal) ...;
 * 只按行扫会漏掉续行上的属性。这个脚本第一版就漏了两处（nav-pill 的 height、
 * filter-pill 的 width），而那两处正是要找的东西。**漏报比不检查更糟。**
 */
function declarations(lines) {
  const out = [];
  let buf = null;
  lines.forEach((line, i) => {
    const text = line.trim();
    if (!buf) {
      const m = /^([a-z-]+)\s*:\s*(.*)$/i.exec(text);
      if (!m) return;
      buf = { prop: m[1], value: m[2], lineNo: i + 1, prev: (lines[i - 1] || '').trim() };
      // 单行声明
      if (/;\s*$/.test(text) || text.endsWith('}')) {
        out.push({ ...buf, value: buf.value.replace(/[;}]\s*$/, '') });
        buf = null;
      }
      return;
    }
    // 续行
    buf.value += ' ' + text;
    if (/;\s*$/.test(text) || text.endsWith('}')) {
      out.push({ ...buf, value: buf.value.replace(/[;}]\s*$/, '') });
      buf = null;
    }
  });
  return out;
}

for (const file of TARGETS) {
  if (!fs.existsSync(file)) continue;
  const rel = path.relative(ROOT, file).replace(/\\/g, '/');
  const lines = fs.readFileSync(file, 'utf8').split(/\r?\n/);

  for (const d of declarations(lines)) {
    const { prop: declProp, value, lineNo } = d;

    // 豁免注释写在声明开始那一行的上一行
    const exempt = /motion-allow:\s*(.+?)\s*\*?\//.exec(d.prev);
    const reason = exempt ? exempt[1].replace(/\*\/$/, '').trim() : null;

    // transition / transition-property / animation
    let props = null;
    if (declProp === 'transition-property') props = value.split(',').map((s) => s.trim());
    else if (declProp === 'transition' || declProp === 'animation') props = transitionProps(value);

    if (props) {
      for (const p of props) {
        if (!p || p === 'none') continue;
        if (p === 'all') {
          if (reason) allowed.push({ rel, lineNo, prop: p, reason });
          else
            problems.push({
              rel, lineNo, prop: p, kind: 'forbidden',
              why: 'transition: all 会捕获所有属性变化（包括你没打算动的），是最常见的隐形性能坑',
            });
          continue;
        }
        if (LAYOUT_PROPS.includes(p)) {
          if (reason) allowed.push({ rel, lineNo, prop: p, reason });
          else
            problems.push({
              rel, lineNo, prop: p, kind: 'layout',
              why: `动画布局属性 ${p}：每帧触发几何重算。改用 FLIP（先测量、再只动 transform）或换成不影响布局的实现`,
            });
        } else if (PAINT_PROPS.includes(p)) {
          problems.push({
            rel, lineNo, prop: p, kind: 'paint',
            why: `动画绘制属性 ${p}：不重排但每帧重绘像素。若是阴影，改成用伪元素承载阴影、只过渡它的 opacity`,
          });
        } else if (listAll) {
          allowed.push({ rel, lineNo, prop: p, reason: '（合成层属性）' });
        }
      }
    }

    // 硬编码时长：动效档位管不到它
    const mDur = /(\d+)ms/.exec(value);
    if (
      mDur &&
      ['transition', 'transition-duration', 'animation', 'animation-duration'].includes(declProp)
    ) {
      // 允许 1ms —— 那是「关」档把时长压到接近 0 的写法
      if (Number(mDur[1]) > 1 && !/var\(--dur/.test(value)) {
        if (reason) allowed.push({ rel, lineNo, prop: `${mDur[1]}ms`, reason });
        else
          problems.push({
            rel, lineNo, prop: `${mDur[1]}ms`, kind: 'hardcoded',
            why: '硬编码时长不响应动效档位 —— 用户把动效调到「关」时它仍然会动。改用 --dur-* token',
          });
      }
    }
  }
}

// ---------- 输出 ----------
const byKind = {
  layout: problems.filter((p) => p.kind === 'layout'),
  forbidden: problems.filter((p) => p.kind === 'forbidden'),
  hardcoded: problems.filter((p) => p.kind === 'hardcoded'),
  paint: problems.filter((p) => p.kind === 'paint'),
};

console.log(`动效合规检查 · ${TARGETS.length} 个文件\n`);

if (problems.length) {
  for (const [kind, label] of [
    ['layout', '布局属性'],
    ['forbidden', '禁止的写法'],
    ['hardcoded', '硬编码时长'],
    ['paint', '绘制属性（提示）'],
  ]) {
    const list = byKind[kind];
    if (!list.length) continue;
    console.log(`${label} · ${list.length} 处`);
    for (const p of list) {
      console.log(`  ${p.rel}:${p.lineNo}  ${p.prop}`);
      console.log(`      ${p.why}`);
    }
    console.log('');
  }
}

if (allowed.length && listAll) {
  console.log(`已豁免 / 合规 · ${allowed.length} 处`);
  for (const a of allowed) {
    console.log(`  ${a.rel}:${a.lineNo}  ${a.prop}  ${a.reason ? '— ' + a.reason : ''}`);
  }
  console.log('');
}

// 布局属性与硬编码时长是真问题；绘制属性只提示
const blocking = byKind.layout.length + byKind.forbidden.length + byKind.hardcoded.length;
if (blocking) {
  console.error(`✗ ${blocking} 处需要修（另有 ${byKind.paint.length} 处绘制属性提示）`);
  process.exit(1);
}
console.log(
  `✔ 通过（无布局属性动画、无硬编码时长）` +
    (byKind.paint.length ? `；${byKind.paint.length} 处绘制属性提示待复核` : '')
);
