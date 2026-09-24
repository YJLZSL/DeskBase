const fs = require('fs');
const F = 'tests/ui-input.cjs';
let s = fs.readFileSync(F, 'utf8');
function need(from, to, label) {
  if (!s.includes(from)) { console.error('!! 未命中 ' + label); process.exit(1); }
  s = s.split(from).join(to);
  console.log('ok ' + label);
}

// realClick：加移动轨迹
need(
  `/** 真实鼠标点击：走屏幕坐标 + 命中测试，和真人点一样。 */
async function realClick(cdp, x, y) {
  await cdp.send("Input.dispatchMouseEvent", {
    type: "mousePressed",
    x,
    y,
    button: "left",
    clickCount: 1,
    buttons: 1,
  });
  await cdp.send("Input.dispatchMouseEvent", {
    type: "mouseReleased",
    x,
    y,
    button: "left",
    clickCount: 1,
    buttons: 0,
  });
}`,
  `/** 真实鼠标点击：走屏幕坐标 + 命中测试，和真人点一样。 */
async function realClick(cdp, x, y) {
  // 先移动再按下 —— 真人点击有这个轨迹，而有些控件（hover 态才渲染、
  // 或依赖 mouseover 绑定）只收得到移动之后的事件。
  await cdp.send("Input.dispatchMouseEvent", { type: "mouseMoved", x, y, buttons: 0 });
  await sleep(60);
  await cdp.send("Input.dispatchMouseEvent", {
    type: "mousePressed",
    x,
    y,
    button: "left",
    clickCount: 1,
    buttons: 1,
  });
  await sleep(60);
  await cdp.send("Input.dispatchMouseEvent", {
    type: "mouseReleased",
    x,
    y,
    button: "left",
    clickCount: 1,
    buttons: 0,
  });
}`,
  'realClick 加移动轨迹'
);

// main：连接后把窗口置顶 + 诊断 DPI
need(
  `    await cdp.send("Page.enable");
    await cdp.send("Runtime.enable");
    await sleep(600);`,
  `    await cdp.send("Page.enable");
    await cdp.send("Runtime.enable");
    // 真实鼠标事件**需要窗口在前台** —— 后台窗口收不到 Input 事件。
    // 第一版没做这一步，点击全部石沉大海（坐标是对的、按钮也在，就是不响应）。
    await cdp.send("Page.bringToFront").catch(() => {});
    await sleep(600);
    // 诊断：DPI 缩放与窗口尺寸。若 devicePixelRatio 不是 1 而点击仍然不准，
    // 下一步就要考虑坐标是否被缩放了。
    const dpr = await cdp.eval("window.devicePixelRatio || 1").catch(() => 1);
    const win = await cdp.eval("(() => ({ w: window.innerWidth, h: window.innerHeight }))()").catch(() => null);
    console.log(`　（devicePixelRatio=${dpr}　视口=${win ? win.w + "x" + win.h : "?"}）`);`,
  '置顶窗口 + DPI 诊断'
);

fs.writeFileSync(F, s, 'utf8');
console.log('done');
