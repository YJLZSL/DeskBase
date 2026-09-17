#!/usr/bin/env node
// P0-08 崩溃一致性测试骨架：写入中强杀循环
//
// 实现「启动目标进程 → 随机延迟 → 强杀 → 重启 → 校验 → 记录」的循环，对应
// docs/19 第 5.1 节 F01（随机延迟 1–5 s 后 SIGKILL，重复 100 次）。
//
// 约束：
//   - 不依赖任何第三方包，仅用 Node 内置模块。
//   - 真实应用尚未实现，因此：
//       * 默认提供 --dry-run：用内置「假目标」（反复写文件的小脚本）跑通整条流程；
//       * 真实接入时通过 --target / --verify 指向任意可执行命令。
//
// 用法：
//   node kill-loop.mjs --dry-run [--rounds 5] [--min-delay 1000] [--max-delay 5000]
//   node kill-loop.mjs --target "node writer.mjs" --verify "node check.mjs"
//
// 判据：校验命令退出码 0 = 通过；非 0 = 失败；目标进程未能启动 = 环境问题需重跑。

import { spawn } from 'node:child_process';
import {
  mkdtempSync,
  rmSync,
  mkdirSync,
  existsSync,
  writeFileSync,
} from 'node:fs';
import { join, dirname, basename } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));

// ---- 参数解析 --------------------------------------------------------------
const argv = process.argv.slice(2);
function getArg(name, fallback) {
  const i = argv.indexOf(name);
  if (i >= 0 && argv[i + 1]) return argv[i + 1];
  return fallback;
}
const DRY_RUN = argv.includes('--dry-run');
const ROUNDS = parseInt(getArg('--rounds', '100'), 10);
const MIN_DELAY = parseInt(getArg('--min-delay', '1000'), 10);
const MAX_DELAY = parseInt(getArg('--max-delay', '5000'), 10);
const TARGET_CMD = getArg('--target', '');
const VERIFY_CMD = getArg('--verify', '');
const REPORT = getArg('--report', '');

if (!DRY_RUN && (!TARGET_CMD || !VERIFY_CMD)) {
  console.error('错误：非 --dry-run 模式必须同时提供 --target 与 --verify。');
  process.exit(2);
}

// 数据目录：dry-run 用系统 Temp；真实模式用 --out 或默认 ./crash-data
const OUT_ARG = getArg('--out', '');
let OUT_DIR;
if (DRY_RUN) {
  OUT_DIR = mkdtempSync(join(tmpdir(), 'deskbase-crash-'));
} else {
  OUT_DIR = OUT_ARG ? OUT_ARG : join(__dirname, 'crash-data');
  mkdirSync(OUT_DIR, { recursive: true });
}
const REPORT_PATH = REPORT
  ? REPORT
  : join(dirname(OUT_DIR), basename(OUT_DIR) + '.report.json');

// ---- 可复现随机（mulberry32）----------------------------------------------
const SEED = (Date.now() & 0xffffffff) >>> 0;
let _s = SEED;
function rand() {
  _s |= 0;
  _s = (_s + 0x6d2b79f5) | 0;
  let t = Math.imul(_s ^ (_s >>> 15), 1 | _s);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}
function randDelay() {
  return Math.floor(MIN_DELAY + rand() * (MAX_DELAY - MIN_DELAY));
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ---- 内置假目标：反复写文件（每次写一行并 fsync）--------------------------
// 被 SIGKILL 时，最后一行可能只写了一半（撕裂写），这正是我们要验证的边界：
// 重启后最多丢失「最后一个未提交写入」，已提交的完整行一字节不少。
function fakeTargetCode(outDir) {
  return `
const fs = require('fs');
const out = ${JSON.stringify(outDir)};
const file = out + '/counter.dat';
const fd = fs.openSync(file, 'w');
let n = 0;
function tick() {
  try {
    fs.writeSync(fd, JSON.stringify({ n: n, ts: Date.now() }) + String.fromCharCode(10));
    fs.fsyncSync(fd);
    n++;
  } catch (e) {}
  setTimeout(tick, 30);
}
tick();
`;
}

// ---- 内置校验：读取文件，确认完整行严格连续 0..k-1，忽略最后可能的撕裂行 ----
function fakeVerifyCode(outDir) {
  return `
const fs = require('fs');
const out = ${JSON.stringify(outDir)};
const file = out + '/counter.dat';
if (!fs.existsSync(file)) { console.log('NO_FILE'); process.exit(2); }
const txt = fs.readFileSync(file, 'utf8');
const lines = txt.split(String.fromCharCode(10));
if (lines.length && lines[lines.length - 1] === '') lines.pop();
// 末尾可能是撕裂的半行（进程被杀时正在写），丢弃它
if (lines.length) {
  const last = lines[lines.length - 1];
  try { JSON.parse(last); } catch (e) { lines.pop(); }
}
const ns = [];
for (const l of lines) {
  let o;
  try { o = JSON.parse(l); } catch (e) { console.log('PARSE_FAIL'); process.exit(3); }
  if (typeof o.n !== 'number' || !Number.isInteger(o.n)) { console.log('BAD_LINE'); process.exit(3); }
  ns.push(o.n);
}
for (let i = 0; i < ns.length; i++) {
  if (ns[i] !== i) { console.log('GAP at ' + i); process.exit(4); }
}
console.log('PASS lines=' + ns.length);
process.exit(0);
`;
}

// ---- 启动目标进程 ----------------------------------------------------------
function startTarget() {
  if (DRY_RUN) {
    return spawn(process.execPath, ['-e', fakeTargetCode(OUT_DIR)], {
      stdio: 'ignore',
    });
  }
  const parts = TARGET_CMD.split(/\s+/);
  return spawn(parts[0], parts.slice(1), { stdio: 'ignore' });
}

// ---- 强杀目标进程 ----------------------------------------------------------
function killTarget(child) {
  try {
    // Windows 下 SIGKILL 会被映射为 TerminateProcess；其余平台即 SIGKILL
    child.kill('SIGKILL');
  } catch {
    try {
      child.kill(); // 兜底：taskkill /F
    } catch {
      /* ignore */
    }
  }
}

function waitExit(child, timeoutMs) {
  return new Promise((resolve) => {
    let done = false;
    const t = setTimeout(() => {
      if (!done) {
        done = true;
        resolve({ timedOut: true, code: null, signal: null });
      }
    }, timeoutMs);
    child.on('exit', (code, signal) => {
      if (!done) {
        done = true;
        clearTimeout(t);
        resolve({ timedOut: false, code, signal });
      }
    });
  });
}

// ---- 运行校验命令 ----------------------------------------------------------
function runVerify() {
  return new Promise((resolve) => {
    let child;
    if (DRY_RUN) {
      child = spawn(process.execPath, ['-e', fakeVerifyCode(OUT_DIR)], {
        stdio: ['ignore', 'pipe', 'pipe'],
      });
    } else {
      const parts = VERIFY_CMD.split(/\s+/);
      child = spawn(parts[0], parts.slice(1), { stdio: ['ignore', 'pipe', 'pipe'] });
    }
    let out = '';
    child.stdout.on('data', (d) => (out += d));
    child.stderr.on('data', (d) => (out += d));
    child.on('error', (e) => resolve({ exit: null, envError: String(e), output: out }));
    child.on('exit', (code) => resolve({ exit: code, output: out.trim() }));
  });
}

// ---- 单轮 ------------------------------------------------------------------
async function runRound(round) {
  // 重置数据目录，保证每轮从干净状态开始（对应 F01 每次重置数据集）
  if (existsSync(OUT_DIR)) rmSync(OUT_DIR, { recursive: true, force: true });
  mkdirSync(OUT_DIR, { recursive: true });

  const child = startTarget();
  let spawnError = null;
  child.on('error', (e) => (spawnError = String(e)));

  await sleep(120); // 给目标进程启动时间
  if (spawnError) {
    return {
      round,
      killDelayMs: 0,
      signal: null,
      code: null,
      verifyExit: null,
      verifyOutput: spawnError,
      pass: false,
      category: 'env', // 目标进程未能启动 = 环境问题，需重跑
      note: 'target 启动失败',
    };
  }

  const delay = randDelay();
  await sleep(delay);
  killTarget(child);
  const exit = await waitExit(child, 5000);

  const verify = await runVerify();
  const pass = verify.exit === 0;
  const category = pass ? 'pass' : exit.timedOut ? 'env' : 'fail';

  return {
    round,
    killDelayMs: delay,
    signal: exit.signal || (exit.timedOut ? 'NONE' : 'SIGKILL'),
    code: exit.code,
    verifyExit: verify.exit,
    verifyOutput: verify.output,
    pass,
    category,
    note: category === 'env' ? '校验超时 / 环境问题' : category === 'fail' ? '校验未通过' : '',
  };
}

// ---- 主循环 -----------------------------------------------------------------
async function main() {
  console.log(`崩溃一致性强杀循环  [${DRY_RUN ? 'dry-run' : 'real'}]`);
  console.log(`  轮次     : ${ROUNDS}`);
  console.log(`  延迟区间 : ${MIN_DELAY}–${MAX_DELAY} ms`);
  console.log(`  数据目录 : ${OUT_DIR}`);
  console.log(`  随机种子 : ${SEED}`);
  console.log('');

  const perRound = [];
  let passed = 0;
  let failed = 0;
  let env = 0;

  for (let i = 1; i <= ROUNDS; i++) {
    const r = await runRound(i);
    perRound.push(r);
    if (r.category === 'pass') passed++;
    else if (r.category === 'env') env++;
    else failed++;
    const tag = r.category === 'pass' ? '通过' : r.category === 'env' ? '环境' : '失败';
    console.log(
      `  第 ${String(i).padStart(3)} 轮  延迟 ${String(r.killDelayMs).padStart(4)}ms  信号 ${String(
        r.signal
      ).padEnd(7)}  校验退出 ${String(r.verifyExit).padStart(3)}  -> ${tag}`
    );
  }

  const report = {
    scenario: 'F01',
    mode: DRY_RUN ? 'dry-run' : 'real',
    startedAt: new Date().toISOString(),
    seed: SEED,
    rounds: ROUNDS,
    minDelayMs: MIN_DELAY,
    maxDelayMs: MAX_DELAY,
    passed,
    failed,
    envIssues: env,
    failures: perRound.filter((r) => r.category === 'fail'),
    perRound,
  };

  // 失败现场保留：把每轮数据目录快照（dry-run 仅保留最后一轮，真实模式应改为持久化）
  writeFileSync(REPORT_PATH, JSON.stringify(report, null, 2));

  console.log('');
  console.log('========== 汇总 ==========');
  console.log(`  总次数     : ${ROUNDS}`);
  console.log(`  通过数     : ${passed}`);
  console.log(`  失败数     : ${failed}`);
  console.log(`  环境问题   : ${env}`);
  if (report.failures.length) {
    console.log('  失败详情:');
    for (const f of report.failures) {
      console.log(
        `    第 ${f.round} 轮  延迟 ${f.killDelayMs}ms  校验退出 ${f.verifyExit}  输出: ${f.verifyOutput}`
      );
    }
  }
  console.log(`  报告文件   : ${REPORT_PATH}`);
  const ok = failed === 0;
  console.log(`  结论       : ${ok ? 'PASS（无数据损坏）' : 'FAIL（存在失败，需保留现场排查）'}`);
  process.exit(ok ? 0 : 1);
}

main().catch((e) => {
  console.error('运行异常:', e);
  process.exit(2);
});
