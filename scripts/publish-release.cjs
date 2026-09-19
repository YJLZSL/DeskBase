#!/usr/bin/env node
/* ============================================================
   DeskBase 一键发布（publish-release.cjs）
   ============================================================
   存在的理由：发一次版要手工做四件事 —— 推提交、推标签、建 Release、
   传三个产物。前三件都能脚本化，第四件（拖文件）最容易漏一个：
   漏传 .sha256 就等于把"可校验"这个承诺丢掉了。

   用法：
     set GITHUB_TOKEN=xxx
     node scripts/publish-release.cjs v0.2.1
     node scripts/publish-release.cjs v0.2.1 --dry-run     只看将做什么
     node scripts/publish-release.cjs v0.2.1 --prerelease  标预发布
     node scripts/publish-release.cjs v0.2.1 --skip-push   只建 Release

   前置：
     · 标签已经打在本地（`git tag -a vX.Y.Z`）
     · 产物已经打好（`node scripts/package.cjs`）
     · 发布正文放在 dist/release-notes/<tag>.md
     · 凭据用环境变量传，**不写进任何文件**（项目红线：不把令牌写进日志）

   网络上的一处本机特殊：本机 `github.com:443` 曾经连不通，而
   `api.github.com` / `uploads.github.com` 通。所以这个脚本只走 API ——
   即使 git push 推不动，建 Release 与传产物这两步照样能做。
   ============================================================ */

const fs = require('fs');
const path = require('path');
const https = require('https');
const { execFileSync } = require('child_process');

const ROOT = path.resolve(__dirname, '..');
const API_HOST = 'api.github.com';
const UPLOAD_HOST = 'uploads.github.com';

// ---------- 参数 ----------
const argv = process.argv.slice(2);
const tag = argv.find((a) => /^v\d/.test(a));
const dryRun = argv.includes('--dry-run');
const prerelease = argv.includes('--prerelease');
const skipPush = argv.includes('--skip-push');

function die(msg, hint) {
  console.error(`✖ ${msg}`);
  if (hint) console.error(`  ${hint}`);
  process.exit(1);
}

if (!tag) die('用法：node scripts/publish-release.cjs vX.Y.Z [--dry-run] [--prerelease] [--skip-push]');

const token = process.env.GITHUB_TOKEN || process.env.GH_TOKEN;

// ---------- 本机前置检查 ----------
function git(args) {
  return execFileSync('git', args, { cwd: ROOT, encoding: 'utf8' }).trim();
}

let remoteUrl;
try {
  remoteUrl = git(['remote', 'get-url', 'origin']);
} catch {
  die('读不到 origin 远端地址');
}

// https://github.com/Owner/Repo.git  →  Owner / Repo
const m = remoteUrl.match(/github\.com[/:]([^/]+)\/([^/]+?)(?:\.git)?$/);
if (!m) die(`解析不了远端地址：${remoteUrl}`);
const owner = m[1];
const repo = m[2];

const notesPath = path.join(ROOT, 'dist', 'release-notes', `${tag}.md`);
const issues = [];

try {
  git(['rev-parse', '--verify', `refs/tags/${tag}`]);
} catch {
  issues.push(`本地没有标签 ${tag} —— 先 git tag -a ${tag} -m "…"`);
}

if (!fs.existsSync(notesPath)) {
  issues.push(`缺发布正文：dist/release-notes/${tag}.md`);
}

// 产物：三个文件一个都不能少（漏传 .sha256 等于丢掉"可校验"）
const version = tag.replace(/^v/, '');
const artifacts = [
  `deskbase-${version}-windows-x64-portable.zip`,
  `deskbase-${version}-windows-x64-portable.zip.sha256`,
  `deskbase-${version}.cdx.json`,
].map((f) => path.join(ROOT, 'dist', f));

for (const a of artifacts) {
  if (!fs.existsSync(a)) issues.push(`缺产物：${path.relative(ROOT, a)}（先跑 node scripts/package.cjs）`);
}

const dirty = git(['status', '--porcelain']);
if (dirty) issues.push(`工作区不干净，先提交：\n${dirty}`);

const ahead = (() => {
  try {
    return git(['log', '--oneline', 'origin/main..main']).split('\n').filter(Boolean);
  } catch {
    return [];
  }
})();

console.log(`发布 ${tag}  →  ${owner}/${repo}`);
console.log(`  正文   ${path.relative(ROOT, notesPath)}`);
console.log(`  产物   ${artifacts.map((a) => path.basename(a)).join('\n         ')}`);
console.log(`  预发布 ${prerelease ? '是' : '否'}`);
console.log(`  待推送 ${ahead.length} 个提交${skipPush ? '（--skip-push，忽略）' : ''}`);

if (issues.length) {
  console.error('');
  for (const i of issues) console.error(`✖ ${i}`);
  process.exit(1);
}

if (dryRun) {
  console.log('\n--dry-run：前置全部就绪，未调用任何 API。');
  process.exit(0);
}

if (!token) {
  die(
    '缺少凭据：请设环境变量 GITHUB_TOKEN（细粒度 token，仅本仓库 Contents 读写即可）',
    '注意：不要把 token 写进任何文件或提交；用完随时在 GitHub 设置里撤销。',
  );
}

// ---------- 极简 HTTP ----------
// 为什么不用 fetch：这里只需要 GET/POST 两个动词，`https.request` 足够，
// 且不依赖任何三方库（项目红线：不引入会联网的依赖）。
//
// 为什么不做代理隧道：本机实测 `api.github.com` 与 `uploads.github.com`
// **直连可达**（不通的只有 `github.com`，而它只在 git push 那一步用到，
// 由 git 自己的 http.proxy 配置去处理）。试过自己写 CONNECT 隧道，
// 在这台机器上 TLS 握手会被断，反而是条没验过的代码路径 —— 删掉。
function request({ method, host, path: p, headers = {}, body }) {
  return new Promise((resolve, reject) => {
    const req = https.request({ method, host, path: p, headers, agent: false }, (res) => {
      const chunks = [];
      res.on('data', (c) => chunks.push(c));
      res.on('end', () => {
        const text = Buffer.concat(chunks).toString('utf8');
        let json = null;
        try {
          json = JSON.parse(text);
        } catch {
          /* 有些响应不是 JSON，保留原文即可 */
        }
        resolve([res.statusCode, json, text]);
      });
    });
    req.on('error', reject);
    if (body) req.write(body);
    req.end();
  });
}

function api(method, p, { body, host = API_HOST } = {}) {
  const payload = body ? JSON.stringify(body) : undefined;
  return request({
    method,
    host,
    path: p,
    headers: {
      'User-Agent': 'DeskBase-publish-release',
      Authorization: `Bearer ${token}`,
      Accept: 'application/vnd.github+json',
      'X-GitHub-Api-Version': '2022-11-28',
      ...(payload
        ? { 'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(payload) }
        : {}),
    },
    body: payload,
  });
}

(async () => {
  // ---------- ① 推提交与标签 ----------
  if (!skipPush) {
    console.log('\n① 推送 main 与标签 …');
    try {
      execFileSync('git', ['push', 'origin', 'main', '--follow-tags'], { cwd: ROOT, stdio: 'inherit' });
    } catch {
      die(
        'git push 失败（多半是凭据或 github.com 不通）',
        '本脚本只走 API 的步骤仍可用：加 --skip-push 重跑，并在网页上手动推。',
      );
    }
  } else {
    console.log('\n① 跳过推送（--skip-push）');
  }

  // ---------- ② 建 Release ----------
  console.log('② 创建 Release …');
  const body = fs.readFileSync(notesPath, 'utf8');
  let [status, json, text] = await api('POST', `/repos/${owner}/${repo}/releases`, {
    body: { tag_name: tag, name: tag, body, prerelease, draft: false },
  });

  if (status === 422 && /already_exists/.test(text || '')) {
    // 已经建过：读回它，继续传产物（重复跑这个脚本不该报错）
    [status, json, text] = await api('GET', `/repos/${owner}/${repo}/releases/tags/${tag}`);
    if (status !== 200) die(`Release 已存在但读不回来（HTTP ${status}）`, text);
    console.log(`   Release 已存在，复用它：#${json.id}`);
  } else if (status !== 201) {
    die(`建 Release 失败（HTTP ${status}）`, text);
  } else {
    console.log(`   已创建：${json.html_url}`);
  }

  const releaseId = json.id;

  // ---------- ③ 传产物 ----------
  console.log('③ 上传产物 …');
  for (const a of artifacts) {
    const name = path.basename(a);
    const data = fs.readFileSync(a);
    const ctype = name.endsWith('.json')
      ? 'application/json'
      : name.endsWith('.zip')
        ? 'application/zip'
        : 'text/plain';
    const [s, j, t] = await request({
      method: 'POST',
      host: UPLOAD_HOST,
      path: `/repos/${owner}/${repo}/releases/${releaseId}/assets?name=${encodeURIComponent(name)}`,
      headers: {
        'User-Agent': 'DeskBase-publish-release',
        Authorization: `Bearer ${token}`,
        Accept: 'application/vnd.github+json',
        'Content-Type': ctype,
        'Content-Length': data.length,
      },
      body: data,
    });
    if (s === 201) {
      console.log(`   ✔ ${name}  ${(data.length / 1024 / 1024).toFixed(2)} MB`);
    } else if (s === 422) {
      console.log(`   · ${name} 已存在，跳过`);
    } else {
      die(`上传 ${name} 失败（HTTP ${s}）`, t);
    }
  }

  console.log(`\n✔ 发布完成：${json.html_url}`);
})().catch((e) => die(e.message));
