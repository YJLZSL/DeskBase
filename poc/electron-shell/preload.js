// 极简 preload：仅暴露两个埋点回调，不引入任何框架
const { ipcRenderer, contextBridge } = require('electron');

contextBridge.exposeInMainWorld('bench', {
  // 渲染进程首帧绘制完成后回报（传入渲染侧的墙钟时刻）
  firstFrame: () => ipcRenderer.send('first-frame', Date.now()),
  // FPS 测试结果回报
  fpsResult: (data) => ipcRenderer.send('fps-result', data),
});
