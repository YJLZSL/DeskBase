/* ============================================================
   崩溃恢复端到端 · 阶段 2 页面脚本：「验尸官」
   ============================================================
   在**同一个数据目录**上重启之后运行，断言五件事：
     1) Rust 侧报出"上次未正常退出"且自检通过（quick_check / WAL）；
     2) 恢复向导**真的出现在界面上**（用户看得见，不是只写进日志）；
     3) 崩溃前已提交的数据活着（表还在、首行不丢、行数下界交给驱动算）；
     4) 「生成数据快照」点了真的能出文件；
     5) 「知道了」点了向导能关。

   收尾：把报告交给 app.smokeReport —— 应用走正常退出路径关掉，
   **退出路径会删掉 boot 标记**，这正是阶段 3 要验证的行为。
   ============================================================ */
(async () => {
  "use strict";
  window.__crashRole = "verify";

  const T0 = Date.now();
  const report = { steps: [], failures: [] };
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const ipc = (cmd, args) => window.__deskbase.call(cmd, args || {});
  const ping = (m) => {
    try {
      const p = ipc("app.smokeProgress", { msg: m });
      if (p && p.catch) p.catch(() => {});
    } catch (_) {}
  };
  const waitFor = async (fn, ms) => {
    const t0 = Date.now();
    for (;;) {
      let v = false;
      try { v = await fn(); } catch (_) {}
      if (v) return true;
      if (Date.now() - t0 > ms) return false;
      await sleep(80);
    }
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
  ping("crashtest-verify: 桥就绪（+" + (Date.now() - T0) + "ms）");

  try {
    // ---------- 1. Rust 侧自检结论 ----------
    let st = null;
    try { st = await ipc("recovery.status"); } catch (_) {}
    step("recovery.status 报出「上次未正常退出」", st && st.unclean === true, st ? "unclean=" + st.unclean : "无应答");
    // 新引擎（ADR-0021）不再有 PRAGMA quick_check，一致性判据是"日志能否被完整解析"。
    // 结论文字是 "ok（N 条事务完整）" 或 "可恢复：…尾部半写已丢弃" —— 两者都算通过，
    // 所以判 st.quick_check_ok 且结论以 ok / 可恢复 开头，而不是死盯着 "ok" 这个字面量。
    step(
      "完整性自检通过（日志能被解析）",
      !!(st && st.quick_check_ok && /^(ok|可恢复)/.test(st.quick_check || "")),
      st && st.quick_check
    );
    step("WAL 检查 ok", !!(st && st.wal_checkpoint === "ok"), st && st.wal_checkpoint);
    step(
      "报告里带着上次启动的信息",
      !!(st && st.last_boot && st.last_boot.started_at_ms > 0),
      st && st.last_boot ? JSON.stringify(st.last_boot).slice(0, 90) : "无 last_boot"
    );

    // ---------- 2. 恢复向导真的出现在界面上 ----------
    const shown = await waitFor(() => !!document.getElementById("recovery-overlay"), 15000);
    step("恢复向导已出现在界面上", shown);
    const dlg = document.getElementById("recovery-overlay");
    if (dlg) {
      const h3 = dlg.querySelector("h3");
      step("向导标题是「没有正常退出」", !!h3 && /没有正常退出/.test(h3.textContent || ""), h3 && h3.textContent);
      const verdict = document.getElementById("recovery-verdict");
      step(
        "向导给出自检结论（绿色通过）",
        !!verdict && verdict.className.indexOf("is-ok") >= 0,
        verdict ? (verdict.textContent || "").slice(0, 80) : "无结论横幅"
      );
      const btns = [".recovery-btn-snap", ".recovery-btn-reveal", ".recovery-btn-ok"].every(
        (s) => !!dlg.querySelector(s)
      );
      step("三个动作齐全（快照 / 打开目录 / 知道了）", btns);

      // ---------- 3. 快照按钮真的能出文件 ----------
      let snapPath = "";
      const snapBtn = dlg.querySelector(".recovery-btn-snap");
      if (snapBtn) {
        snapBtn.click();
        const okSnap = await waitFor(
          () => /快照已生成/.test((document.getElementById("recovery-result") || {}).textContent || ""),
          20000
        );
        const txt = (document.getElementById("recovery-result") || {}).textContent || "";
        snapPath = txt.replace(/^快照已生成：/, "");
        step("点「生成数据快照」后界面报出路径", okSnap && snapPath.length > 8, txt.slice(0, 140));
      } else {
        step("点「生成数据快照」后界面报出路径", false, "找不到快照按钮");
      }

      // ---------- 4. 数据活着 ----------
      const raw = await ipc("schema.listTables");
      const list = Array.isArray(raw) ? raw : (raw && raw.tables) || [];
      const t = list.find((x) => /^崩溃测试/.test(x.name));
      step("崩溃前建的表还在", !!t, list.map((x) => x.name).join(", ").slice(0, 80));
      if (t) {
        const hi = await ipc("schema.pageRows", { table: t.name, orderBy: "序号", desc: true, limit: 1 });
        const lo = await ipc("schema.pageRows", { table: t.name, orderBy: "序号", desc: false, limit: 1 });
        const max = hi && hi.rows && hi.rows[0] ? Number(hi.rows[0][1]) : -1;
        const min = lo && lo.rows && lo.rows[0] ? Number(lo.rows[0][1]) : -1;
        step("已提交的数据活着：首行不丢（min=1）", min === 1, "min=" + min);
        step("已提交的数据活着：行数可数（max≥50）", max >= 50, "max=" + max);
        // 给驱动脚本：它拿日志里的"已提交下界"做精确断言（页面看不到日志）
        report.data = { table: t.name, min: min, max: max, snapshot: snapPath };
      }

      // ---------- 5. 「知道了」可用 ----------
      const okBtn = dlg.querySelector(".recovery-btn-ok");
      if (okBtn) {
        okBtn.click();
        const closed = await waitFor(() => !document.getElementById("recovery-overlay"), 4000);
        step("点「知道了」后向导关闭", closed);
      } else {
        step("点「知道了」后向导关闭", false, "找不到按钮");
      }
    }
  } catch (e) {
    const msg = (e && e.message) || String(e);
    report.failures.push("未捕获异常：" + msg);
    ping("✖ 未捕获异常：" + msg);
  }

  ping("crashtest-verify: 准备回报（" + report.steps.length + " 步，" + report.failures.length + " 项失败）");
  for (let i = 0; i < 2; i++) {
    try {
      await ipc("app.smokeReport", report);
      break;
    } catch (_) {
      await sleep(500);
    }
  }
})();
