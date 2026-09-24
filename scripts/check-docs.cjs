#!/usr/bin/env node
/**
 * 文档一致性核对（机器核对，不是肉眼看）
 * ============================================================
 * v1.0 门槛第 ④ 条要求：**用户文档 + 开发者文档 + ADR 全部与实现一致（机器核对）**。
 * 这个脚本就是那个"机器核对"。
 *
 * 为什么需要它：README 曾经同时写着"安装版还没做"和"自动更新是手动下载"，
 * 而这两件事都已经做完了 —— 用户照着 README 会得到完全错误的印象。
 * 人手核对一遍没用，因为**改完代码那一刻它就开始过期**。
 *
 * 检查项（只查**能确定判定对错**的东西，不做模糊猜测）：
 *   1. 版本号自洽：README / CHANGELOG / AGENTS 里出现的最新版本 == app/Cargo.toml
 *   2. 路径引用存在：文档里 `docs/…` `scripts/…` `app/…` 形式的引用，文件要真的在
 *   3. 门禁清单一致：文档里列的 check-* 脚本要真的存在
 *   4. 反向提示：README 的"还没做"清单里若出现已完成能力的关键词，提示人工确认
 *
 * 用法：node scripts/check-docs.cjs
 */

const fs = require('fs');
const path = require('path');

const ROOT = path.join(__dirname, '..');
const problems = [];
const notes = [];

function read(rel) {
  try {
    return fs.readFileSync(path.join(ROOT, rel), 'utf8');
  } catch (_) {
    return '';
  }
}

// ---------- 1) 版本号自洽 ----------
const cargoToml = read('app/Cargo.toml');
const verMatch = cargoToml.match(/^version\s*=\s*"([^"]+)"/m);
if (!verMatch) {
  problems.push('app/Cargo.toml 里读不到 version');
} else {
  const ver = verMatch[1];
  const readme = read('README.md');
  // 取 README 里出现的**最高**版本号与 Cargo.toml 比对。
  //
  // ⚠️ 原来写的是匹配"最新为 \`vX.Y.Z\`"这句特定措辞 —— 而那句话后来在改 README 时
  // 被删掉了，检查随即**变成空转**：不报错，但也没在查。空转的检查比没有检查更危险，
  // 它给人一种"已经被把关了"的错觉。所以改成扫全部版本号引用、取最大值。
  const vs = [...readme.matchAll(/\bv(\d+\.\d+\.\d+)\b/g)].map((m) => m[1]);
  if (vs.length) {
    const max = vs.sort((a, b) => {
      const pa = a.split('.').map(Number);
      const pb = b.split('.').map(Number);
      for (let i = 0; i < 3; i++) if (pa[i] !== pb[i]) return pb[i] - pa[i];
      return 0;
    })[0];
    if (max !== ver) {
      problems.push(`README 里最高的版本号是 v${max}，而 Cargo.toml 是 v${ver}`);
    }
  } else {
    // 一个版本号都不提也不好 —— 用户想知道"最新是什么"
    notes.push('README 里没有出现任何版本号，建议写明当前版本');
  }
  // CHANGELOG 必须有一节对应当前版本（发版时先改版本号再写日志，漏了这里能抓到）
  const log = read('CHANGELOG.md');
  if (log && !new RegExp(`^## \\[${ver.replace(/\./g, '\\.')}\\]`, 'm').test(log)) {
    problems.push(`CHANGELOG 里没有 [${ver}] 这一节（版本号改了但没写日志？）`);
  }
}

// ---------- 2) 文档里的本地路径引用要存在 ----------
const DOC_FILES = ['README.md', 'AGENTS.md', 'CONTRIBUTING.md', 'ROADMAP.md'];
for (const f of DOC_FILES) {
  const text = read(f);
  if (!text) continue;
  // 只认"看起来就是一个具体文件"的引用：带扩展名、不含通配与省略号
  for (const m of text.matchAll(/`((?:docs|scripts|tests|app|tools|testdata)\/[\w./\u4e00-\u9fa5-]+\.\w+)`/g)) {
    const rel = m[1];
    if (rel.includes('*') || rel.includes('…') || rel.includes('...')) continue;
    if (!fs.existsSync(path.join(ROOT, rel))) {
      problems.push(`${f} 引用了不存在的路径：${rel}`);
    }
  }
}
// docs 下的文档之间也会互相引用
const docsDir = path.join(ROOT, 'docs');
if (fs.existsSync(docsDir)) {
  const walk = (dir) => {
    for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
      const p = path.join(dir, e.name);
      if (e.isDirectory()) {
        walk(p);
        continue;
      }
      if (!e.name.endsWith('.md')) continue;
      const rel = path.relative(ROOT, p).replace(/\\/g, '/');
      const text = fs.readFileSync(p, 'utf8');
      for (const m of text.matchAll(/\]\(([\w./-]+\.\w+)\)/g)) {
        // 只查仓库内的相对链接
        const target = m[1];
        if (/^https?:/.test(target)) continue;
        const abs = path.resolve(path.dirname(p), target);
        if (!fs.existsSync(abs)) {
          problems.push(`${rel} 里的链接指向不存在的文件：${target}`);
        }
      }
    }
  };
  walk(docsDir);
}

// ---------- 3) 文档里提到的门禁脚本要真的存在 ----------
{
  const text = read('AGENTS.md') + read('README.md');
  for (const m of text.matchAll(/\b(check-[a-z]+|[a-z-]+-(?:smoke|import|recovery|walkthrough))\.cjs/g)) {
    const name = m[1] + '.cjs';
    const inScripts = fs.existsSync(path.join(ROOT, 'scripts', name));
    const inTests = fs.existsSync(path.join(ROOT, 'tests', name));
    if (!inScripts && !inTests) {
      problems.push(`文档提到了不存在的脚本：${name}`);
    }
  }
}

// ---------- 4) 反向提示：README 的"还没做"里出现已完成能力的关键词 ----------
{
  const readme = read('README.md');
  const i = readme.indexOf('| 还没做 |');
  if (i >= 0) {
    const seg = readme.slice(i, readme.indexOf('\n\n', i));
    // 这些能力已经做完了 —— 若又出现在"还没做"里，多半是忘了更新
    const done = {
      安装版: 'app/src/installer.rs',
      自动更新: 'app/src/updater.rs',
      表结构编辑: 'app/src/model/mod.rs',
      变更历史: 'app/src/model/mod.rs',
      旧库: 'app/src/legacy.rs',
    };
    for (const [kw, impl] of Object.entries(done)) {
      if (seg.includes(kw) && fs.existsSync(path.join(ROOT, impl))) {
        notes.push(
          `README 的「还没做」里提到了「${kw}」，但 ${impl} 已经有实现 —— 确认一下是不是忘删了`
        );
      }
    }
  }
}

// ---------- 输出 ----------
if (notes.length) {
  console.log('提示（需人工确认，不算失败）：');
  notes.forEach((n) => console.log('  · ' + n));
  console.log('');
}
if (problems.length) {
  console.error('✘ 文档与实现不一致（' + problems.length + ' 处）：');
  problems.forEach((p) => console.error('  · ' + p));
  process.exit(1);
}
console.log('✔ 通过（版本号自洽、路径引用存在、门禁脚本存在）');
