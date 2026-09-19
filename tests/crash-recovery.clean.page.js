/* ============================================================
   崩溃恢复端到端 · 阶段 3 页面脚本：「复检」
   ============================================================
   上一阶段走的是"向导 → 知道了 → 正常退出"。正常退出会删掉 boot 标记，
   所以这一阶段必须看到：
     · unclean = false（不再报未清理）；
     · **恢复向导不再出现** —— 它只在出事时出现。如果它常驻，
       用户会学会无视它，那比没有还糟；
     · 数据仍在。

   收尾同样走 app.smokeReport（正常退出，顺手把标记清干净）。
   ============================================================ */
(async () => {
  "use strict";
  window.__crashRole = "clean";

  const report = { steps: [], failures: [] };
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const ipc = (cmd, args) => window.__deskbase.call(cmd, args || {});
  const ping = (m) => {
    try {
      const p = ipc("app.smokeProgress", { msg: m });
      if (p && p.catch) p.catch(() => {});
    } catch (_) {}
  };
  const step = (name, ok, detail) => {
    report.steps.push({ name, ok: !!ok, detail: detail == null ? "" : String(detail) });
    if (!ok) report.failures.push(name + (detail ? "：" + detail : ""));
    ping((ok ? "✔ " : "✖ ") + name);
  };

  for (let i = 0; i < 60; i++) {
    if (window.__deskbase && typeof window.__deskbase.call === "function") break;
    await sleep(200);
  }

  try {
    let st = null;
    try { st = await ipc("recovery.status"); } catch (_) {}
    step("正常退出后不再报未清理（unclean=false）", st && st.unclean === false, st ? "unclean=" + st.unclean : "无应答");

    // 给 recovery.js 足够时间"该弹就弹"（它只在不清理时才弹；这里必须没弹）
    await sleep(1500);
    step("恢复向导不再出现", !document.getElementById("recovery-overlay"));

    const raw = await ipc("schema.listTables");
    const list = Array.isArray(raw) ? raw : (raw && raw.tables) || [];
    const t = list.find((x) => /^崩溃测试/.test(x.name));
    step("数据仍在（崩溃测试表还在）", !!t, list.map((x) => x.name).join(", ").slice(0, 80));
  } catch (e) {
    const msg = (e && e.message) || String(e);
    report.failures.push("未捕获异常：" + msg);
    ping("✖ 未捕获异常：" + msg);
  }

  for (let i = 0; i < 2; i++) {
    try {
      await ipc("app.smokeReport", report);
      break;
    } catch (_) {
      await sleep(500);
    }
  }
})();
