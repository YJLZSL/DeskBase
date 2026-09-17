// 体积测量：用 PowerShell 统计 node_modules 与 electron 运行时目录大小（MB）
const { execFile } = require('child_process');
const fs = require('fs');
const path = require('path');

const APP = __dirname;

function sizeMB(dir) {
  return new Promise((resolve) => {
    const cmd = `((Get-ChildItem -Path '${dir}' -Recurse -File -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum)/1MB`;
    execFile('powershell.exe', ['-NoProfile', '-Command', cmd], (err, stdout) => {
      if (err) { resolve(null); return; }
      const v = parseFloat(stdout);
      resolve(isNaN(v) ? null : v);
    });
  });
}

(async () => {
  const nm = path.join(APP, 'node_modules');
  const edist = path.join(APP, 'node_modules', 'electron', 'dist');
  const nmMB = await sizeMB(nm);
  const edistMB = await sizeMB(edist);
  const result = {
    node_modules_MB: nmMB === null ? null : +nmMB.toFixed(1),
    electron_dist_MB: edistMB === null ? null : +edistMB.toFixed(1),
    note: 'node_modules 含 electron 运行时本体；便携解压体积约等于 node_modules 大小（本 PoC 无其它依赖）',
  };
  fs.mkdirSync(path.join(APP, 'out'), { recursive: true });
  fs.writeFileSync(path.join(APP, 'out', 'volume.json'), JSON.stringify(result, null, 2));
  console.log(JSON.stringify(result, null, 2));
})();
