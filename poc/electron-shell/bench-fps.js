// FPS 测量：拉起 electron --fps，页面渲染 1 万行表格并匀速滚动 30s，
// 渲染进程回报帧率统计后写入 out/fps.json，本脚本读取并落盘。
const { spawn } = require('child_process');
const fs = require('fs');
const path = require('path');

const APP = __dirname;
const ELECTRON = path.join(APP, 'node_modules', 'electron', 'dist', 'electron.exe');
const OUT = path.join(APP, 'out', 'fps.json');

function rmIfExists(p) { try { fs.unlinkSync(p); } catch (e) {} }
function waitFile(resolve) {
  if (fs.existsSync(OUT)) {
    try {
      const d = JSON.parse(fs.readFileSync(OUT, 'utf8'));
      fs.writeFileSync(path.join(APP, 'out', 'fps_result.json'), JSON.stringify(d, null, 2));
      resolve(d);
      return;
    } catch (e) {}
  }
  setTimeout(() => waitFile(resolve), 100);
}

(async () => {
  rmIfExists(OUT);
  const child = spawn(ELECTRON, ['.', '--fps'], { cwd: APP, env: process.env, stdio: 'ignore' });
  const guard = setTimeout(() => {
    try { child.kill('SIGKILL'); } catch (e) {}
    console.error('[fps] 超时未产出结果');
    process.exit(1);
  }, 60000);

  const d = await new Promise(waitFile);
  clearTimeout(guard);
  try { child.kill('SIGTERM'); } catch (e) {}
  console.log(JSON.stringify(d, null, 2));
  process.exit(0);
})();
