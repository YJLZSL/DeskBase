// 冷启动测量：拉起 electron 5 次，记录每次「进程拉起 -> 首帧绘制」耗时
// 第 1 次标 cold（本轮首次，fs 缓存未预热），后 4 次标 warm
// 内部计时来自 app 自身埋点(coldMs)，外部计时来自父进程拉起墙钟(externalMs)
const { spawn } = require('child_process');
const fs = require('fs');
const path = require('path');

const APP = __dirname;
const ELECTRON = path.join(APP, 'node_modules', 'electron', 'dist', 'electron.exe');
const OUT = path.join(APP, 'out', 'coldstart.json');

function rmIfExists(p) { try { fs.unlinkSync(p); } catch (e) {} }
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function runOnce(label) {
  return new Promise((resolve) => {
    rmIfExists(OUT);
    const spawnEpoch = Date.now();
    const child = spawn(ELECTRON, ['.'], {
      cwd: APP,
      env: Object.assign({}, process.env, { SPAWN_EPOCH: String(spawnEpoch) }),
      stdio: 'ignore',
    });
    let done = false;
    const guard = setTimeout(() => {
      if (!done) { done = true; try { child.kill('SIGKILL'); } catch (e) {} resolve({ error: 'timeout', label }); }
    }, 90000);

    function waitFile() {
      if (done) return;
      if (fs.existsSync(OUT)) {
        try {
          const data = JSON.parse(fs.readFileSync(OUT, 'utf8'));
          done = true;
          clearTimeout(guard);
          try { child.kill('SIGTERM'); } catch (e) {}
          data.label = label;
          resolve(data);
          return;
        } catch (e) {}
      }
      setTimeout(waitFile, 50);
    }
    child.on('exit', () => clearTimeout(guard));
    waitFile();
  });
}

function median(arr) {
  if (!arr.length) return null;
  const a = arr.slice().sort((x, y) => x - y);
  return a[Math.floor(a.length / 2)];
}

(async () => {
  const labels = ['cold', 'warm1', 'warm2', 'warm3', 'warm4'];
  const results = [];
  for (let i = 0; i < 5; i++) {
    const r = await runOnce(labels[i]);
    results.push(r);
    console.error(`[coldstart] ${labels[i]}: coldMs=${r.coldMs} externalMs=${r.externalMs}`);
    await sleep(1500); // 给进程退出 + fs 缓存预热留时间
  }
  fs.mkdirSync(path.join(APP, 'out'), { recursive: true });
  fs.writeFileSync(path.join(APP, 'out', 'coldstart_runs.json'), JSON.stringify(results, null, 2));

  const coldMs = results.map((r) => r.coldMs).filter((v) => typeof v === 'number');
  const extMs = results.map((r) => r.externalMs).filter((v) => typeof v === 'number');
  const summary = {
    raw: results,
    internalColdMs_median: median(coldMs),
    externalColdMs_median: median(extMs),
  };
  fs.writeFileSync(path.join(APP, 'out', 'coldstart_summary.json'), JSON.stringify(summary, null, 2));
  console.log(JSON.stringify(summary, null, 2));
})();
