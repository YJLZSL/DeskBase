// 空闲内存测量：拉起 electron（主窗口打开、不操作），
// 先等 60s 稳定，再连续采样 60s（每 1s 一次），取全部样本平均。
// 内存 = 所有 electron 进程 WorkingSet64 之和（main/renderer/gpu/utility 等）。
const { spawn, execFile } = require('child_process');
const fs = require('fs');
const path = require('path');

const APP = __dirname;
const ELECTRON = path.join(APP, 'node_modules', 'electron', 'dist', 'electron.exe');
const SETTLE_MS = 60000;
const SAMPLE_MS = 60000;
const INTERVAL = 1000;

function sampleMB() {
  return new Promise((resolve) => {
    // 用 PowerShell 汇总所有 electron 进程的 WorkingSet64（KB -> MB）
    const cmd = '(Get-Process -Name electron* -ErrorAction SilentlyContinue | Measure-Object WorkingSet64 -Sum).Sum / 1MB';
    execFile('powershell.exe', ['-NoProfile', '-Command', cmd], (err, stdout) => {
      if (err) { resolve(null); return; }
      const v = parseFloat(stdout);
      resolve(isNaN(v) ? null : v);
    });
  });
}

(async () => {
  const child = spawn(ELECTRON, ['.', '--idle'], {
    cwd: APP,
    env: process.env,
    stdio: 'ignore',
  });

  console.error('[memory] 等待 60s 稳定...');
  await new Promise((r) => setTimeout(r, SETTLE_MS));

  console.error('[memory] 采样 60s...');
  const samples = [];
  const t0 = Date.now();
  while (Date.now() - t0 < SAMPLE_MS) {
    const mb = await sampleMB();
    if (mb !== null) samples.push(mb);
    console.error(`[memory] sample ${samples.length}: ${mb === null ? 'NA' : mb.toFixed(1)} MB`);
    await new Promise((r) => setTimeout(r, INTERVAL));
  }

  try { child.kill('SIGTERM'); } catch (e) {}

  const avg = samples.reduce((a, b) => a + b, 0) / (samples.length || 1);
  const min = samples.length ? Math.min(...samples) : null;
  const max = samples.length ? Math.max(...samples) : null;
  const result = {
    settleSec: SETTLE_MS / 1000,
    sampleSec: SAMPLE_MS / 1000,
    sampleCount: samples.length,
    avgMB: +avg.toFixed(1),
    minMB: min === null ? null : +min.toFixed(1),
    maxMB: max === null ? null : +max.toFixed(1),
    rawMB: samples.map((x) => +x.toFixed(1)),
  };
  fs.mkdirSync(path.join(APP, 'out'), { recursive: true });
  fs.writeFileSync(path.join(APP, 'out', 'memory.json'), JSON.stringify(result, null, 2));
  console.log(JSON.stringify(result, null, 2));
})();
