/* ============================================================
   崩溃恢复端到端 · 阶段 1 页面脚本：「受害者」
   ============================================================
   由 tests/crash-recovery.cjs 注入（DESKBASE_UI_SMOKE）。
   职责单一：建一张表，然后**以稳定节奏持续写入**，每批提交后播报进度。

   它不需要"跑完" —— 测试会在写到一半时把进程强杀。
   为什么每批 50 行、间隔 80ms：让"强杀落在事务附近"成为常态，
   同时保证几秒内就有几百行**已提交**数据可供验证。
   ============================================================ */
(async () => {
  "use strict";
  window.__crashRole = "writer";

  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const ipc = (cmd, args) => window.__deskbase.call(cmd, args || {});
  const ping = (m) => {
    try {
      const p = ipc("app.smokeProgress", { msg: m });
      if (p && p.catch) p.catch(() => {});
    } catch (_) {}
  };

  for (let i = 0; i < 60; i++) {
    if (window.__deskbase && typeof window.__deskbase.call === "function") break;
    await sleep(200);
  }
  ping("crashtest-writer: 桥就绪");

  const TABLE = "崩溃测试" + Date.now().toString(36);
  try {
    await ipc("schema.createTable", {
      spec: {
        name: TABLE,
        comment: null,
        columns: [
          { name: "序号", ty: "integer", not_null: false, default: null, primary_key: false, comment: null },
          { name: "备注", ty: "text", not_null: false, default: null, primary_key: false, comment: null },
        ],
      },
    });
    ping("crashtest-writer: 表已建 " + TABLE);
  } catch (e) {
    ping("crashtest-writer: 建表失败 " + ((e && e.message) || e));
    return;
  }

  const BATCH = 50;
  let n = 0;
  for (let round = 0; round < 2000; round++) {
    const rows = [];
    for (let i = 0; i < BATCH; i++) {
      n += 1;
      rows.push([String(n), "行" + n]);
    }
    try {
      await ipc("schema.insertRows", { table: TABLE, columns: ["序号", "备注"], rows });
      // 播报在**提交之后**（insertRows 返回 = 事务已提交）——
      // 这个数就是"已提交下界"，驱动脚本拿它做精确断言。
      ping("crashtest-writer: 已写入 " + n);
    } catch (e) {
      ping("crashtest-writer: 写入失败于 " + n + "：" + ((e && e.message) || e));
      await sleep(400);
      continue;
    }
    await sleep(80);
  }
})();
