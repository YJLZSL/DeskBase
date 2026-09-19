#!/usr/bin/env node
/* ============================================================
   DeskBase 前端接线门禁（check-wiring.cjs）
   ============================================================
   存在的理由：本项目吃过三次"文件写了、功能没有"的亏 ——
     ① 新样式表没进 check-motion 的 TARGETS（绿勾是真的，覆盖是假的）
     ② 代理留下的 help.js / db-onboard.js 文件齐全，却漏了 assets.rs
        登记与 index.html 挂载（页面照开，功能悄悄没有）
     ③ Rust 的 has_more 被 JS 读成 hasMore（字段名契约，见 schema.rs 的测试）
   这三类都不是编译错误，全靠人肉发现太不划算，所以做成门禁。

   检查四项：
     1. index.html 引用的站内资源，是否都在 assets.rs 的 lookup() 里登记
     2. app/ui/*.js 是否都挂进了 index.html（写了没挂 = 死文件）
     3. app/ui/*.css 是否都进了 check-motion.cjs 的 TARGETS
     4. 前端调用的 IPC 命令，是否都在 main.rs 的 dispatch() 里有分支

   用法：node scripts/check-wiring.cjs
   ============================================================ */

const fs = require('fs');
const path = require('path');

const ROOT = path.resolve(__dirname, '..');
const UI = path.join(ROOT, 'app', 'ui');
const SRC = path.join(ROOT, 'app', 'src');

const problems = [];
const notes = [];

// ---------- 读文件 ----------
const assetsSrc = fs.readFileSync(path.join(SRC, 'assets.rs'), 'utf8');
const html = fs.readFileSync(path.join(UI, 'index.html'), 'utf8');
const mainSrc = fs.readFileSync(path.join(SRC, 'main.rs'), 'utf8');
const motionSrc = fs.readFileSync(path.join(ROOT, 'scripts', 'check-motion.cjs'), 'utf8');

/** assets.rs 里登记过的资源路径（形如 "/app.js"） */
const registered = new Set();
for (const m of assetsSrc.matchAll(/^\s*"(\/[^"]+)"\s*=>/gm)) {
  registered.add(m[1]);
}

/** index.html 里引用的站内资源（src= / href=，跳过外链与锚点） */
const referenced = [];
for (const attr of ['src="', 'href="']) {
  for (const part of html.split(attr).slice(1)) {
    const end = part.indexOf('"');
    if (end < 0) continue;
    const v = part.slice(0, end);
    if (!v || v.includes('://') || v.startsWith('data:') || v.startsWith('#')) continue;
    referenced.push(v);
  }
}

/** check-motion.cjs 的 TARGETS */
const targets = new Set();
for (const m of motionSrc.matchAll(/path\.join\(ROOT,\s*'app',\s*'ui',\s*'([^']+)'\)/g)) {
  targets.add(m[1]);
}

/**
 * 去掉 Rust 源码里的注释（字符串内容保留 —— 命令名本身就是字符串）。
 *
 * 为什么必须去注释：注释里提到一句 `// "schema.runQuery" => ...` 就会让
 * 第 4 项检查显示"有分支"。这正是本项目最忌讳的那种通过 ——
 * 绿勾是真的，覆盖是假的（D-040：门禁必须被负向验证过）。
 */
function stripRustComments(src) {
  let out = '';
  let i = 0;
  while (i < src.length) {
    const c = src[i];
    // 行注释
    if (c === '/' && src[i + 1] === '/') {
      while (i < src.length && src[i] !== '\n') i++;
      continue;
    }
    // 块注释（保留一个空格，免得把两侧的 token 粘在一起）
    if (c === '/' && src[i + 1] === '*') {
      i += 2;
      while (i < src.length - 1 && !(src[i] === '*' && src[i + 1] === '/')) i++;
      i += 2;
      out += ' ';
      continue;
    }
    // 字符串：整段原样保留
    if (c === '"') {
      out += '"';
      i++;
      while (i < src.length) {
        const d = src[i];
        if (d === '\\') {
          out += src.slice(i, i + 2);
          i += 2;
          continue;
        }
        out += d;
        i++;
        if (d === '"') break;
      }
      continue;
    }
    // 字符字面量（'a' / '\n'）。生命周期（&'static）不在这里处理 ——
    // 只认"紧跟一个字符再跟一个引号"的形态，避免把 'a 之后的整段代码吃掉。
    if (c === "'") {
      if (src[i + 1] === '\\' && src[i + 3] === "'") {
        out += src.slice(i, i + 4);
        i += 4;
        continue;
      }
      if (src[i + 2] === "'") {
        out += src.slice(i, i + 3);
        i += 3;
        continue;
      }
      out += c;
      i++;
      continue;
    }
    out += c;
    i++;
  }
  return out;
}

/** main.rs 的 dispatch() 里出现的命令字符串（注释已剔除） */
const mainCode = stripRustComments(mainSrc);
const ipcCommands = new Set();
for (const m of mainCode.matchAll(/"([a-z]+\.[a-zA-Z]+)"\s*=>/g)) {
  ipcCommands.add(m[1]);
}
/* 也认 `req.cmd == "x.y"` 这种形式。异步命令（如 schema.runQuery 的
   run_query_async）不一定写成 match 分支 —— 只认 `"x" =>` 会误报
   "命令没有分支"，逼着人改代码去迎合门禁。门禁该适配代码，不是反过来。
   （main.rs 里目前是 match 形式；这条是给后来者的余地。） */
for (const m of mainCode.matchAll(/cmd\s*==\s*"([a-z]+\.[a-zA-Z]+)"/g)) {
  ipcCommands.add(m[1]);
}

// ---------- 1. 引用 → 登记 ----------
if (referenced.length < 5) {
  problems.push(`index.html 只扫到 ${referenced.length} 个站内引用，扫描逻辑可能失效了`);
}
for (const ref of referenced) {
  if (!registered.has('/' + ref)) {
    problems.push(
      `index.html 引用了 "${ref}"，但 assets.rs 的 lookup() 没有登记它 —— 浏览器拿 404，页面不报错、功能静默失效`
    );
  }
}

// ---------- 2. UI 脚本 → 挂载 ----------
const jsFiles = fs.readdirSync(UI).filter((f) => f.endsWith('.js'));
for (const f of jsFiles) {
  if (!referenced.includes(f)) {
    problems.push(`app/ui/${f} 存在但没有挂进 index.html —— 写了等于没写`);
  }
}

// ---------- 3. 样式表 → 动效门禁 ----------
const cssFiles = fs.readdirSync(UI).filter((f) => f.endsWith('.css'));
for (const f of cssFiles) {
  if (!targets.has(f)) {
    problems.push(
      `app/ui/${f} 不在 scripts/check-motion.cjs 的 TARGETS 里 —— 门禁会给"绿勾是真的、覆盖是假的"的通过`
    );
  }
}

// ---------- 4. 前端调用的 IPC 命令 → dispatch 分支 ----------
for (const f of jsFiles) {
  const code = fs.readFileSync(path.join(UI, f), 'utf8');
  for (const m of code.matchAll(/call\(\s*"([a-z]+\.[a-zA-Z]+)"/g)) {
    if (!ipcCommands.has(m[1])) {
      problems.push(`app/ui/${f} 调用了 IPC "${m[1]}"，但 main.rs 的 dispatch() 里没有这个分支`);
    }
  }
}

// ---------- 输出 ----------
console.log(`接线检查 · 资源登记 ${registered.size} 个 · 站内引用 ${referenced.length} 个 · UI 脚本 ${jsFiles.length} 个 · 样式表 ${cssFiles.length} 份 · IPC ${ipcCommands.size} 条\n`);
if (notes.length) notes.forEach((n) => console.log(`· ${n}`));

if (problems.length) {
  console.error('✘ 接线有问题：\n');
  problems.forEach((p) => console.error(`  - ${p}`));
  console.error('\n修完再提交。这四项都不是编译错误，漏了只能靠这道门禁。');
  process.exit(1);
}
console.log('✔ 通过（引用都已登记、脚本都已挂载、样式表都进门禁、IPC 都有分支）');
