#!/usr/bin/env node
/* ============================================================
   DeskBase 发布说明生成器（gen-changelog.cjs）
   ============================================================
   存在的理由：我们每次发版都在**手写** CHANGELOG —— 而提交信息本来就强制
   Conventional Commits（见 CONTRIBUTING.md），素材早就齐了，只是没人去用。
   手写的代价不是费时间，是**会漏**：写的人只记得自己做过的那几件事。

   做法照 MAA：由 CI 从提交历史生成草稿，人只负责润色。
   参考 local-docs/reference/24-auto-update-research.md 第 3.1 节。

   用法：
     node scripts/gen-changelog.cjs                      生成 上一个标签..HEAD
     node scripts/gen-changelog.cjs --to v0.2.1          生成 上一个标签..v0.2.1
     node scripts/gen-changelog.cjs --from <ref> --to <ref>
     node scripts/gen-changelog.cjs --write              写到 dist/release-notes/<to>-generated.md
     node scripts/gen-changelog.cjs --extract 0.2.1      从 CHANGELOG.md 抽出该版本的正文

   `--extract` 是给发布流水线用的：`dist/` 被 .gitignore 排除，**CI 里读不到
   dist/release-notes/*.md**，所以要有一个"从仓库内文件取发布正文"的路径。
   而 CHANGELOG.md 本来就是单一事实来源 —— 再复制一份到别处只会两处漂移。

   两条刻意的设计：
   1. **不认识的提交不丢，单独列成「未归类」** —— 静默丢掉一条提交，
      等于发布说明对着用户撒谎。宁可丑，不可漏。
   2. **只生成草稿**（文件名带 -generated），人润色后的那一份才是发布用的。
      自动生成的文字不该直接发出去。
   ============================================================ */

const fs = require('fs');
const path = require('path');
const { execFileSync } = require('child_process');

const ROOT = path.resolve(__dirname, '..');

const argv = process.argv.slice(2);
const opt = (name, def) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 && argv[i + 1] ? argv[i + 1] : def;
};
const write = argv.includes('--write');

// ---------- 模式一：从 CHANGELOG.md 抽取某版本的正文 ----------
const extractVersion = opt('extract', null);
if (extractVersion) {
  const ver = extractVersion.replace(/^v/, '');
  const file = path.join(ROOT, 'CHANGELOG.md');
  if (!fs.existsSync(file)) {
    console.error(`✖ 找不到 ${path.relative(ROOT, file)}`);
    process.exit(1);
  }
  const lines = fs.readFileSync(file, 'utf8').split('\n');
  const start = lines.findIndex((l) => new RegExp(`^##\\s*\\[${ver.replace(/\./g, '\\.')}\\]`).test(l));
  if (start < 0) {
    console.error(`✖ CHANGELOG.md 里没有 [${ver}] 这一节`);
    process.exit(1);
  }
  let end = lines.length;
  for (let i = start + 1; i < lines.length; i++) {
    if (/^##\s*\[/.test(lines[i])) { end = i; break; }
  }
  const body = lines
    .slice(start, end)
    .join('\n')
    .replace(/\n+---\s*$/, '')   // 去掉小节末尾的分隔线
    .trim();
  if (!body) {
    console.error(`✖ [${ver}] 这一节是空的`);
    process.exit(1);
  }
  console.log(body);
  process.exit(0);
}

function git(args) {
  return execFileSync('git', args, { cwd: ROOT, encoding: 'utf8' });
}

// ---------- 决定范围 ----------
const to = opt('to', 'HEAD');

let from = opt('from', null);
if (!from) {
  // 默认取"能到达 to 的最近一个标签"＝上一个正式发布点
  try {
    from = git(['describe', '--tags', '--abbrev=0', `${to}^`]).trim();
  } catch {
    from = null; // 一个标签都没有（首次发版）
  }
}

const range = from ? `${from}..${to}` : to;

// ---------- 读提交 ----------
// 用 \x1f 分字段、\x1e 分记录：提交正文里可能有换行，按行解析一定会错位
const RAW = git(['log', `--format=%h%x1f%s%x1f%b%x1e`, range]);

const commits = RAW.split('\x1e')
  .map((r) => r.replace(/^\n+/, ''))
  .filter((r) => r.trim())
  .map((r) => {
    const [hash, subject, body = ''] = r.split('\x1f');
    return { hash: (hash || '').trim(), subject: (subject || '').trim(), body: body.trim() };
  });

// ---------- 归类 ----------
// 分类与 CHANGELOG 的既有分类保持一致（新增 / 变更 / 修复 / 性能 / 文档 / 安全 / 工程 / 测试 / 回退）
const GROUPS = [
  { key: 'feat', title: '### 新增' },
  { key: 'fix', title: '### 修复' },
  { key: 'perf', title: '### 性能' },
  { key: 'refactor', title: '### 变更' },
  { key: 'docs', title: '### 文档' },
  { key: 'security', title: '### 安全' },
  { key: 'test', title: '### 测试' },
  { key: 'build', title: '### 工程' },
  { key: 'ci', title: '### 工程' },
  { key: 'chore', title: '### 工程' },
  { key: 'revert', title: '### 回退' },
];

// `type(scope)!: subject` / `type: subject`
const RE = /^([a-z]+)(?:\(([^)]*)\))?(!)?:\s*(.+)$/;

// 元提交：描述"发版/合并"这类动作本身，不是给用户看的功能变更。
// **不是静默丢掉** —— 跳过几条会在文件头的注释里写清楚。
const META = new Set(['release', 'merge']);

const buckets = new Map();
const unclassified = [];
const breaking = [];
let metaCount = 0;

for (const c of commits) {
  const m = c.subject.match(RE);
  if (!m) {
    // 例如 "Merge pull request #3915" 这类 —— 进未归类，不丢
    unclassified.push(c);
    continue;
  }
  const [, type, scope, bang, text] = m;
  if (META.has(type)) {
    metaCount++;
    continue;
  }
  const group = GROUPS.find((g) => g.key === type);
  if (!group) {
    unclassified.push(c);
    continue;
  }
  if (!buckets.has(group.title)) buckets.set(group.title, []);
  buckets.get(group.title).push({ ...c, scope, text });

  if (bang || /^BREAKING[ -]CHANGE:/m.test(c.body)) {
    breaking.push(c);
  }
}

// ---------- 输出 ----------
const lines = [];
const rangeText = from ? `${from} → ${to}` : `（首个版本）→ ${to}`;
lines.push(`<!-- 由 scripts/gen-changelog.cjs 从提交历史生成 · 范围 ${rangeText} · ${commits.length} 个提交 -->`);
if (metaCount) {
  lines.push(`<!-- 已跳过 ${metaCount} 条元提交（release / merge），它们描述的是发版动作本身，不是给用户看的变更 -->`);
}
lines.push('<!-- 这是**草稿**：请润色成人话再发布，不要直接发。 -->');
lines.push('');

if (breaking.length) {
  lines.push('### ⚠️ 不兼容变更');
  lines.push('');
  for (const c of breaking) lines.push(`- ${c.subject.replace(RE, '$4')}  \`${c.hash}\``);
  lines.push('');
}

for (const g of GROUPS) {
  const items = buckets.get(g.title);
  if (!items || !items.length) continue;
  lines.push(g.title);
  lines.push('');
  for (const it of items) {
    const scope = it.scope ? `**${it.scope}**：` : '';
    lines.push(`- ${scope}${it.text}  \`${it.hash}\``);
  }
  lines.push('');
  buckets.set(g.title, []); // 同一标题只输出一次（build/ci/chore 都归「工程」）
}

if (unclassified.length) {
  lines.push('### 未归类（提交信息不符合 Conventional Commits，请人工确认）');
  lines.push('');
  for (const c of unclassified) lines.push(`- ${c.subject}  \`${c.hash}\``);
  lines.push('');
}

if (!commits.length) {
  lines.push('_这个范围内没有提交。范围写错了？_');
  lines.push('');
}

const out = lines.join('\n');

if (write) {
  const dir = path.join(ROOT, 'dist', 'release-notes');
  fs.mkdirSync(dir, { recursive: true });
  const file = path.join(dir, `${to.replace(/[^\w.-]/g, '_')}-generated.md`);
  fs.writeFileSync(file, out, 'utf8');
  console.log(`✔ 已写出 ${path.relative(ROOT, file)}`);
  console.log(`  范围 ${rangeText}，共 ${commits.length} 个提交`);
  if (metaCount) console.log(`  跳过 ${metaCount} 条元提交（release / merge）`);
  if (unclassified.length) {
    console.log(`  ⚠ 有 ${unclassified.length} 条提交没能归类，已在「未归类」一节列出（没有丢）`);
  }
} else {
  console.log(out);
}
