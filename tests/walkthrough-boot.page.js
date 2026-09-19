/* ============================================================
   自动点击走查 · 启动页脚本（最小）
   ============================================================
   它做的事只有两件：
     1) 让 Rust 侧进入"烟测模式"（DESKBASE_UI_SMOKE 已设置），从而打开
        CDP 端口 9222 —— 真正的点击、截图、布局体检全部由外部驱动
        （tests/ui-walkthrough.cjs）通过 DevTools 协议完成；
     2) 在日志里留一行"注入通道活着"的凭据，方便走查卡住时二分。

   刻意**不**在这里做任何点击：走查的节奏由外部驱动掌握，
   页面脚本一旦掺进来，时序就会变成两套机制互相等。
   ============================================================ */
(async () => {
  "use strict";
  window.__walkthroughBooted = true;

  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  for (let i = 0; i < 60; i++) {
    if (window.__deskbase && typeof window.__deskbase.call === "function") break;
    await sleep(200);
  }
  try {
    await window.__deskbase.call("app.smokeProgress", { msg: "走查：注入通道就绪，外部驱动即将接管" });
  } catch (_) {}
})();
