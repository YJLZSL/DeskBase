const { spawn } = require('child_process');
const fs = require('fs');
const path = require('path');
const APP = '<仓库根目录>/poc/electron-shell';
const E = path.join(APP, 'node_modules', 'electron', 'dist', 'electron.exe');
const OUT = path.join(APP, 'out', 'coldstart.json');
const LOG = require('path').join(require('os').tmpdir(), 'poc_electron_stderr.log');
try { fs.unlinkSync(OUT); } catch (e) {}
const se = Date.now();
const c = spawn(E, ['.'], { cwd: APP, env: Object.assign({}, process.env, { SPAWN_EPOCH: String(se) }), stdio: ['ignore', 'inherit', 'inherit'] });
const log = fs.createWriteStream(LOG);
c.stdout && c.stdout.pipe(log);
c.stderr && c.stderr.pipe(log);
const t = setTimeout(() => {
  log.write('SMOKE_TIMEOUT\n');
  try { c.kill('SIGKILL'); } catch (e) {}
  process.exit(2);
}, 30000);
function w() {
  if (fs.existsSync(OUT)) {
    try {
      log.write('SMOKE_OK ' + fs.readFileSync(OUT, 'utf8') + '\n');
      clearTimeout(t);
      try { c.kill('SIGTERM'); } catch (e) {}
      process.exit(0);
    } catch (e) {}
  }
  setTimeout(w, 100);
}
c.on('exit', (code) => { log.write('CHILD_EXIT ' + code + '\n'); clearTimeout(t); });
w();
