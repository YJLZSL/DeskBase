// DeskBase Electron 外壳 PoC —— 主进程
// 目的：测外壳自身开销，不引入任何 UI 框架 / TS / 打包器
const { app, BrowserWindow, ipcMain } = require('electron');
const fs = require('fs');
const path = require('path');

// 近似「进程启动时刻」：process.uptime() 是 node 进程已运行秒数，
// 用它回推启动_epoch，可覆盖 node 启动 + 模块加载耗时（比 main.js 顶部打点更早）。
const processStartEpoch = Date.now() - Math.round(process.uptime() * 1000);

const OUT_DIR = path.join(__dirname, 'out');

function writeJson(name, obj) {
  fs.mkdirSync(OUT_DIR, { recursive: true });
  fs.writeFileSync(path.join(OUT_DIR, name), JSON.stringify(obj, null, 2));
}

// (a) 冷启动：渲染进程首帧绘制后回报渲染侧墙钟，主进程算差值
// 同时记录父进程经环境变量传入的拉起时刻，用于外部秒表交叉验证
const spawnEpoch = process.env.SPAWN_EPOCH ? Number(process.env.SPAWN_EPOCH) : null;
ipcMain.on('first-frame', (event, rendererEpoch) => {
  const coldMs = rendererEpoch - processStartEpoch;
  const externalMs = spawnEpoch ? rendererEpoch - spawnEpoch : null;
  writeJson('coldstart.json', {
    processStartEpoch,
    rendererEpoch,
    spawnEpoch,
    coldMs,
    externalMs,
    note: 'coldMs=渲染首帧墙钟-主进程近似启动墙钟；externalMs=渲染首帧墙钟-父进程拉起墙钟(外部秒表)',
  });
});

// (c) FPS：渲染进程回报滚动帧率统计
ipcMain.on('fps-result', (event, data) => {
  writeJson('fps.json', data);
});

function createWindow() {
  const win = new BrowserWindow({
    width: 1024,
    height: 768,
    show: true,
    webPreferences: {
      preload: path.join(__dirname, 'preload.js'),
      contextIsolation: true,
      nodeIntegration: false,
    },
  });

  const fpsMode = process.argv.includes('--fps');
  if (fpsMode) {
    win.loadFile('index.html', { search: '?fps=1' });
  } else {
    win.loadFile('index.html');
  }
}

app.whenReady().then(createWindow);

// PoC 模式：作为子进程被测量脚本拉起时，保持前台运行，由外部脚本 kill
app.on('window-all-closed', () => {
  // 让脚本可随时杀掉；正常退出交给 SIGTERM
});
