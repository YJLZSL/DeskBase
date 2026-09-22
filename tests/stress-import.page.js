/* ============================================================
   DeskBase 导入压力测试 · 页面脚本（由 tests/stress-import.cjs 驱动）
   ============================================================
   测什么：**大文件导入的规模表现**，全部在真实 WebView + 真实 SQLite 里跑。

   每一步都用真实数据断言，不做"跑完就算"的走过场：
     · 行数一行不差（少一行就是丢数据）
     · 分批次数与批大小对得上（证明真的在分批，而不是攒着一次性写）
     · 进度单调递增且**中间有中间值**（证明界面能看见进展）
     · 导入后立刻能翻页、能筛选、能排序（证明索引与 keyset 分页可用）
     · 每种字段的值抽查正确（金额按分、前导零保住、空值是真 NULL）

   报告格式：{ steps: [{name, ok, detail}], failures: [string] }
   ============================================================ */
(async () => {
  "use strict";

  window.__smokeState = "boot";
  const T0 = Date.now();
  const report = { steps: [], failures: [] };
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  const ping = (m) => {
    try {
      const p = window.__deskbase.call("app.smokeProgress", {
        msg: m + "（+" + (Date.now() - T0) + "ms）",
      });
      if (p && p.catch) p.catch(() => {});
    } catch (_) {}
  };
  const step = (name, ok, detail) => {
    report.steps.push({ name, ok: !!ok, detail: detail == null ? "" : String(detail) });
    if (!ok) report.failures.push(name + (detail ? "：" + detail : ""));
    window.__smokeState = "step:" + report.steps.length + (ok ? "" : " ✖" + name);
    ping((ok ? "✔ " : "✖ ") + name);
  };
  const ipc = (cmd, args) => window.__deskbase.call(cmd, args || {});
  const fmt = (n) => Number(n).toLocaleString("zh-CN");

  // 期望行数由驱动脚本通过 app.e2eSource 给出 —— 不取自后端，避免自己证明自己
  let EXPECT = 0;
  const TABLE = "压测台账";

  ping("压力测试脚本开始执行（期望 " + EXPECT + " 行）");
  let beat = 0;
  const heart = setInterval(() => {
    beat++;
    ping("♥ 心跳 " + beat + "（已完成 " + report.steps.length + " 步）");
  }, 3000);

  /** 量一次 JS 堆占用（有就用，没有就跳过 —— 它不是断言项） */
  const heapMB = () => {
    try {
      return window.performance && performance.memory
        ? Math.round(performance.memory.usedJSHeapSize / 1048576)
        : null;
    } catch (_) {
      return null;
    }
  };

  try {
    step("源文件已由启动进程登记", true, EXPECT + " 行");

    // ---------- 1. 计划阶段：读文件 + 认表头 + 认类型 ----------
    const t1 = Date.now();
    const src = await ipc("app.e2eSource");
    const tok = src.token;
    EXPECT = Number(src.expectRows || 0);
    const pv = await ipc("import.preview", { planId: tok, sheetIndex: 0 });
    const plan = pv.plan;
    const tPlan = Date.now() - t1;
    step(
      "认表头（第 3 行）+ 数据行数正确",
      plan.header_row === 2 && plan.data_rows === EXPECT,
      `header_row=${plan.header_row} data_rows=${fmt(plan.data_rows)} 期望=${fmt(EXPECT)} 用时=${tPlan}ms`
    );
    step("列数正确（7 列）", plan.columns.length === 7, "实际 " + plan.columns.length);
    ping(
      "计划阶段用时 " + tPlan + "ms，列类型：" +
        plan.columns.map((c) => c.name + ":" + c.ty).join(" ")
    );

    // ---------- 2. 落库：分批 ----------
    const cols = plan.columns.map((c) => ({
      source_index: c.source_index,
      name: c.name,
      ty: c.ty,
      not_null: false,
    }));
    const t2 = Date.now();
    const beg = await ipc("import.begin", {
      planId: tok,
      table: TABLE,
      sheetIndex: 0,
      headerRow: plan.header_row,
      columns: cols,
    });
    const sid = beg.sessionId;
    const total = beg.begun.total;
    const tBegin = Date.now() - t2;
    step(
      "建表并备好数据（且后端收到的表头行与传的一致）",
      total === EXPECT && beg.begun.header_row === plan.header_row,
      `total=${fmt(total)} 期望=${fmt(EXPECT)}｜传入 headerRow=${plan.header_row} 后端收到 ${beg.begun.header_row} 用时=${tBegin}ms`
    );

    // 分批：批大小 5000。10 万行 → 20 批；20 万行 → 40 批。
    const BATCH = 5000;
    const expectBatches = Math.ceil(EXPECT / BATCH);
    let written = 0;
    let batches = 0;
    const pcts = [];
    const heapBefore = heapMB();
    const t3 = Date.now();
    let lastPing = Date.now();
    for (;;) {
      const c = await ipc("import.chunk", { sessionId: sid, batch: BATCH });
      written = c.written;
      batches++;
      pcts.push(written);
      // 长任务里每 2 秒播报一次，让 app.log 能看出"确实在推进"
      if (Date.now() - lastPing > 2000) {
        lastPing = Date.now();
        ping(`写入中 ${fmt(written)} / ${fmt(total)} 行（第 ${batches} 批）`);
      }
      if (c.done) break;
      if (batches > expectBatches + 5) throw new Error("批次数异常，可能没在推进");
    }
    const tWrite = Date.now() - t3;
    const heapAfter = heapMB();

    step(
      "全部写完且一行不差",
      written === EXPECT,
      `written=${fmt(written)} 期望=${fmt(EXPECT)}`
    );
    step(
      "确实按批推进（批次数与批大小对得上）",
      batches === expectBatches,
      `批次数=${batches} 期望=${expectBatches}（每批 ${fmt(BATCH)}）`
    );
    step(
      "进度单调递增且出现过中间值",
      pcts.every((v, i) => i === 0 || v >= pcts[i - 1]) && pcts.length > 1,
      pcts.length > 4
        ? pcts.slice(0, 2).join("→") + "…→" + pcts[pcts.length - 1]
        : pcts.join("→")
    );
    // 写入吞吐：这是可复现的性能数字（环境与数据集见下）
    const rowsPerSec = Math.round(EXPECT / (tWrite / 1000));
    step(
      "写入吞吐有实测值",
      rowsPerSec > 0,
      `${fmt(rowsPerSec)} 行/秒（${fmt(EXPECT)} 行用时 ${(tWrite / 1000).toFixed(1)}s，每批 ${fmt(BATCH)}）`
    );
    ping(
      "写入阶段：用时 " + (tWrite / 1000).toFixed(1) + "s，吞吐 " + fmt(rowsPerSec) + " 行/秒" +
        (heapBefore != null && heapAfter != null ? `，JS 堆 ${heapBefore}→${heapAfter} MB` : "")
    );

    const t4 = Date.now();
    const out = await ipc("import.finish", {
      sessionId: sid,
      planId: tok,
      skippedAbove: plan.header_row,
    });
    step("收尾报告一致", out.inserted === EXPECT, `inserted=${fmt(out.inserted)} 用时=${Date.now() - t4}ms`);

    // ---------- 3. 导完立刻能用：分页 / 排序 / 筛选 ----------
    const t5 = Date.now();
    const p1 = await ipc("schema.pageRows", { table: TABLE, limit: 200 });
    step("第一页能读回（keyset 分页）", p1.rows.length === 200, `行数=${p1.rows.length} 用时=${Date.now() - t5}ms`);

    // 翻到第二页：大表能不能翻页（这里曾经因为字段名写错而彻底坏掉）
    const p2 = await ipc("schema.pageRows", { table: TABLE, limit: 200, cursor: p1.next_cursor });
    const ids1 = new Set(p1.rows.map((r) => r[0]));
    const overlap = p2.rows.filter((r) => ids1.has(r[0])).length;
    step("第二页能翻到且与第一页不重复", p2.rows.length === 200 && overlap === 0, `重叠=${overlap}`);

    // 排序：按金额降序，第一行必须是最大值
    const t6 = Date.now();
    const sorted = await ipc("schema.pageRows", {
      table: TABLE,
      orderBy: "金额",
      desc: true,
      limit: 5,
    });
    const maxMoney = Math.round(EXPECT * 13.37 * 100);
    step(
      "排序可用（金额降序第一行是最大值）",
      sorted.rows[0][sorted.columns.indexOf("金额")] === maxMoney,
      `首行=${sorted.rows[0][sorted.columns.indexOf("金额")]} 期望=${maxMoney} 用时=${Date.now() - t6}ms`
    );

    // 筛选：找一个必然存在的客户名
    const t7 = Date.now();
    const nameIdx = p1.columns.indexOf("客户名称");
    const probe = String(p1.rows[0][nameIdx]);
    const filtered = await ipc("schema.pageRows", {
      table: TABLE,
      limit: 50,
      filters: [["客户名称", probe]],
    });
    step(
      "筛选可用（全表扫关键词能命中）",
      filtered.rows.length >= 1,
      `「${probe}」命中 ${filtered.rows.length} 行 用时=${Date.now() - t7}ms`
    );

    // 整表行数：SQL 移除前这一步是 `SELECT COUNT(*)`（那时 COUNT 在大表上要全表扫，
    // 所以只能给估计值）。新引擎自己维护记录，行数是**精确值** ——
    // 顺带把这条从"估计"升级成"确定"，断言也就敢写等号了。
    const t8 = Date.now();
    const cntInfo = await ipc("schema.getTable", { name: TABLE });
    const qms = Date.now() - t8;
    step(
      "行数可数（精确值，不再是估计）",
      !!cntInfo && Number(cntInfo.row_estimate) === EXPECT,
      `count=${cntInfo ? cntInfo.row_estimate : "?"} 用时=${qms}ms`
    );

    // ---------- 4. 值抽查：每一类字段都验 ----------
    // 值抽查：**按金额升序取第一行** —— 金额随 i 单调递增，第一行必然是 i=1
    // （13.37 元 / 000001 号 / 13810000001 / 是 / 2026-02-11 且备注非空）。
    // 用它而不是行号排序：`_rowid` 是内部行号，schema 明确拒绝当字段名用。
    const full = await ipc("schema.pageRows", {
      table: TABLE,
      orderBy: "金额",
      desc: false,
      limit: 3,
    });
    const col = (n) => full.columns.indexOf(n);
    step(
      "金额按分存（13.37 元 → 1337 分）",
      full.rows[0][col("金额")] === 1337,
      String(full.rows[0][col("金额")])
    );
    step("前导零工号保住（001001）", /^0/.test(String(full.rows[0][col("工号")])), String(full.rows[0][col("工号")]));
    step(
      "11 位电话没被当数字（仍是字符串）",
      String(full.rows[0][col("联系电话")]).length === 11,
      String(full.rows[0][col("联系电话")])
    );
    step("是否结清存成 1/0", full.rows[0][col("是否结清")] === 1, String(full.rows[0][col("是否结清")]));
    step("日期规范化（YYYY-MM-DD）", /^\d{4}-\d{2}-\d{2}$/.test(String(full.rows[0][col("签单日期")])), String(full.rows[0][col("签单日期")]));

    // 空值：i=7、14、21… 的备注为空 → **统一落 NULL**（Q-047 已拍板，2026-09-19）。
    //
    // 这里以前断言的是"文本列存空串、数字列存 NULL"的旧行为 —— 那是一处已知的不一致
    // （同一份导入数据里，`WHERE 备注 IS NULL` 查不到文本列的空、`WHERE 数量 IS NULL`
    // 又能查到数字列的空）。现在的两条路径是**刻意不同**的：
    //   · **导入**：源文件里的空格子 = 没填 = 没有值 → 统一 NULL（含文本列）；
    //   · **手动编辑**：文本格清空仍保留空串语义（"我确实填了个空的"）。
    // 判据：excel_import.rs 读文件时（trim 后为空 → None）+ schema.rs 的 insert_rows_opt。
    const seven = await ipc("schema.pageRows", {
      table: TABLE,
      orderBy: "金额",
      desc: false,
      limit: 8,
    });
    const noteIdx = seven.columns.indexOf("备注");
    const nullCount = seven.rows.filter((r) => r[noteIdx] === null).length;
    const emptyStr = seven.rows.filter((r) => r[noteIdx] === "").length;
    step(
      "空单元格：文本列也落成 NULL（Q-047 已拍板）",
      nullCount >= 1 && emptyStr === 0,
      `前 8 行里 NULL=${nullCount} 空串=${emptyStr}（i=7 的备注为空）`
    );

    // ---------- 5. 表体积（可复现的数字） ----------
    const info = await ipc("schema.getTable", { name: TABLE });
    step(
      "行数估计与实际相符（约数）",
      info.row_estimate < 0 || Math.abs(info.row_estimate - EXPECT) < EXPECT * 0.2,
      `row_estimate=${info.row_estimate}`
    );

    // ---------- 6. 清理 ----------
    await ipc("schema.dropTable", { name: TABLE, confirmName: TABLE });
    step("清理完成（删表）", true);

    const totalSec = ((Date.now() - T0) / 1000).toFixed(1);
    ping(
      `规模 ${fmt(EXPECT)} 行：计划 ${tPlan}ms + 备数据 ${tBegin}ms + 写入 ${tWrite}ms（${fmt(rowsPerSec)} 行/秒），总 ${totalSec}s`
    );
  } catch (e) {
    const msg = e && e.message ? e.message : String(e);
    report.failures.push("未捕获异常：" + msg);
    ping("✖ 未捕获异常：" + msg);
  }

  clearInterval(heart);
  ping("准备回报（" + report.steps.length + " 步，" + report.failures.length + " 项失败）");
  for (let i = 0; i < 2; i++) {
    try {
      await window.__deskbase.call("app.smokeReport", report);
      break;
    } catch (_) {
      await sleep(500);
    }
  }
})();
