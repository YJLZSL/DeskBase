/* ============================================================
   真实输入测试 · 启动页脚本（最小）
   ============================================================
   它只做两件事：
     1) 让 Rust 侧进入"烟测模式"（DESKBASE_UI_SMOKE 已设置），从而打开
        CDP 端口 9222 —— 真正的鼠标点击与键盘由外部驱动（tests/ui-input.cjs）
        通过 DevTools 的 Input 域发出；
     2) 在日志里留一行凭据，方便卡住时二分。

   和走查那份一样，**刻意不在页面里做任何点击**：点击一旦由页面脚本掺进来，
   它就变成了 `element.click()`（绕开真实事件链），这套测试的意义也就没了。
   ============================================================ */
(async () => {
  "use strict";
  window.__inputBooted = true;

  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  for (let i = 0; i < 60; i++) {
    if (window.__deskbase && typeof window.__deskbase.call === "function") break;
    await sleep(200);
  }
  try {
    await window.__deskbase.call("app.smokeProgress", { msg: "真实输入测试：注入通道就绪，外部驱动即将接管" });
  } catch (_) {}
})();
