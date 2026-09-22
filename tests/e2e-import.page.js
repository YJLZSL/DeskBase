/* ============================================================
   DeskBase 端到端验收 · 页面脚本（由 tests/e2e-import.cjs 驱动）
   ============================================================
   用途：在**真实的 WebView 里跑一遍完整的导入流程** —— 不是断言 IPC 契约，
   而是把界面真会用到的路径走完：

     读文件 → 认表头 → 认类型 → 用户改一列 → 落库（分批，进度是真的）
       → 表出现在左栏 → 打开它 → 数据网格里读回第一页 → 删表清理

   为什么需要它（而不是只靠 Rust 集成测试）：Rust 那侧测的是"IPC 能通"，
   而用户在界面上是**一长串调用串起来**的。链条断裂的地方往往在两段之间
   （例如进度条走了但表名传丢了、分批写完了但左栏没刷新）。

   验收口径（与 ui-smoke 一致）：
     · 只断言界面行为与库里的真实数据，不断言网络
     · 每一步都通过 app.smokeProgress 播报进 app.log（卡住时能看出卡在哪）
     · 结果通过 app.smokeReport 回传

   报告格式：{ steps: [{name, ok, detail}], failures: [string] }
   ============================================================ */
(async () => {
  "use strict";

  window.__smokeState = "boot";

  const T0 = Date.now();
  const report = { steps: [], failures: [] };
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const $ = (s) => document.querySelector(s);
  const text = (el) => (el && el.textContent ? el.textContent.trim() : "");

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

  const waitFor = async (fn, ms) => {
    const t0 = Date.now();
    for (;;) {
      let v = false;
      try {
        v = await fn();
      } catch (_) {}
      if (v) return true;
      if (Date.now() - t0 > ms) return false;
      await sleep(60);
    }
  };

  const ipc = (cmd, args) => window.__deskbase.call(cmd, args || {});

  const TABLE = "E2E客户台账";

  ping("端到端脚本开始执行");
  let beat = 0;
  const heart = setInterval(() => {
    beat++;
    ping("♥ 心跳 " + beat + "（已完成 " + report.steps.length + " 步）");
  }, 1500);

  try {

    // ---------- 1. 直接走 IPC：计划 ----------
    // 说明：真实的用户路径是点按钮弹原生文件对话框，而那个对话框**会阻塞主线程、
    // 无法自动化**（见 db.js 的 openImportDialog 注释）。所以这里把"选完文件之后"
    // 的那一段拿真数据跑满 —— 覆盖的是从计划到落库到界面回读的全部环节。
    const planId = (await ipc("app.e2eSource")).token;
    step("把源文件交给程序（取得令牌）", !!planId, String(planId).slice(0, 12));

    const pv = await ipc("import.preview", { planId: planId, sheetIndex: 0 });
    const plan = pv.plan;
    step("认出表头在第 3 行（上面两行是标题/日期）", plan.header_row === 2, "header_row=" + plan.header_row);
    step("如实报告表头上面有 2 行不会导入", plan.skipped_above === 2, "skipped_above=" + plan.skipped_above);
    step("数据行数正确（1200 行）", plan.data_rows === 1200, "data_rows=" + plan.data_rows);
    step("六列都识别出来了", plan.columns.length === 6, plan.columns.map((c) => c.name).join(","));

    // 逐列核对类型判断 —— 这是"静默毁数据"最可能发生的地方
    const byName = {};
    plan.columns.forEach((c) => (byName[c.name] = c));
    step(
      "长编号列判成文本（不能当数字）",
      byName["联系电话"] && byName["联系电话"].ty === "text",
      byName["联系电话"] && byName["联系电话"].ty
    );
    step(
      "前导零工号判成文本（当数字就丢零）",
      byName["工号"] && byName["工号"].ty === "text",
      byName["工号"] && byName["工号"].ty
    );
    step(
      "金额列判成金额（按分存）",
      byName["金额"] && byName["金额"].ty === "money",
      byName["金额"] && byName["金额"].ty
    );
    step(
      "是否结清判成是/否",
      byName["是否结清"] && byName["是否结清"].ty === "boolean",
      byName["是否结清"] && byName["是否结清"].ty
    );
    step(
      "日期列判成日期",
      byName["签单日期"] && byName["签单日期"].ty === "date",
      byName["签单日期"] && byName["签单日期"].ty
    );
    // 每一列都必须给出"为什么这么判"—— 用户凭它评判，不能是黑箱
    const noReason = plan.columns.filter((c) => !c.reason).map((c) => c.name);
    step("每列都写了判断依据", noReason.length === 0, noReason.join(","));

    // ---------- 2. 用户改一列：签单日期 → 文本（模拟"我不信你的判断"）----------
    // 这一步是刻意设计的：证明**用户的改动真的会被采纳**，而不是被后端的建议覆盖。
    const cols = plan.columns.map((c) => ({
      source_index: c.source_index,
      name: c.name,
      ty: c.name === "签单日期" ? "text" : c.ty,
      not_null: false,
    }));

    // ---------- 3. 落库：begin → 分批 chunk（进度是真的）----------
    const beg = await ipc("import.begin", {
      planId: planId,
      table: TABLE,
      sheetIndex: 0,
      headerRow: plan.header_row,
      columns: cols,
    });
    const sid = beg.sessionId;
    const total = beg.begun.total;
    step("开始导入：拿到待写行数", total === 1200, "total=" + total);

    // 界面用的进度条，这里也真用一遍 —— 顺带验证它在长任务里能正常更新
    const prog = window.DeskBaseUI.progress({ title: "导入 " + TABLE });
    document.body.appendChild(prog.el);

    let written = 0;
    let batches = 0;
    let pctSeen = [];
    for (;;) {
      const c = await ipc("import.chunk", { sessionId: sid, batch: 400 });
      written = c.written;
      batches++;
      const frac = total ? written / total : 1;
      prog.set(frac, "已写入 " + written + " / " + total + " 行");
      pctSeen.push(Math.round(frac * 100));
      if (c.done) break;
      if (batches > 50) throw new Error("分批次数异常，可能没在推进");
    }
    step("分批写完 1200 行", written === 1200, "written=" + written + " 批次数=" + batches);
    step("确实分了多批（不是一口气写完）", batches >= 2, "批次数=" + batches);
    step(
      "进度是单调递增的（进度条不会往回跳）",
      pctSeen.every((v, i) => i === 0 || v >= pctSeen[i - 1]),
      pctSeen.join(",")
    );

    const out = await ipc("import.finish", {
      sessionId: sid,
      planId: planId,
      skippedAbove: plan.header_row,
    });
    prog.done("导入完成：" + out.inserted + " 行");
    step("收尾报告导入了 1200 行", out.inserted === 1200, "inserted=" + out.inserted);
    step("收尾报告表头上面跳过了 2 行", out.skipped_above === 2, "skipped_above=" + out.skipped_above);

    // ---------- 4. 左栏刷新能看到这张表 ----------
    const dbApi = window.DeskBaseDb;
    await dbApi.refreshTables();
    const appeared = await waitFor(
      () => !!Array.from(document.querySelectorAll(".db-table-item .t")).find((e) => text(e) === TABLE),
      5000
    );
    step("新表出现在左栏列表里", appeared);

    // ---------- 5. 打开它，数据网格读回第一页 ----------
    const page = await ipc("schema.pageRows", { table: TABLE, limit: 5 });
    step("能读回数据（分页第一页）", page && page.rows && page.rows.length === 5, "行数=" + (page && page.rows ? page.rows.length : 0));
    step("列名是原表头（中文）", page.columns[1] === "客户名称", page.columns.join(","));

    // 金额按「分」存：13.37 元 → 1337 分（第一行 i=1 → 1*13.37 = 13.37）
    const moneyIdx = page.columns.indexOf("金额");
    step("金额按分存（13.37 元 → 1337 分）", page.rows[0][moneyIdx] === 1337, String(page.rows[0][moneyIdx]));
    // 工号的前导零必须还在（这是最容易丢的数据）
    const codeIdx = page.columns.indexOf("工号");
    const code = page.rows[0][codeIdx];
    step("工号的前导零保住了（001001）", /^0/.test(String(code)), String(code));
    // 联系电话 13 位不能变成科学计数
    const telIdx = page.columns.indexOf("联系电话");
    step(
      "联系电话没被当数字处理（仍是 11 位字符串）",
      String(page.rows[0][telIdx]).length === 11,
      String(page.rows[0][telIdx])
    );
    // 用户改过的那一列：签单日期按文本存，原样保留
    const dIdx = page.columns.indexOf("签单日期");
    step(
      "用户把「签单日期」改成文本后，值原样保留",
      /^2026-/.test(String(page.rows[0][dIdx])),
      String(page.rows[0][dIdx])
    );
    // 表头上面那两行绝不能进库
    // ⚠️ 不能靠"一次取 2000 行"来数行：分页有硬上限（MAX_PAGE_LIMIT=500，
    // 那是防一次取太多拖垮界面的保护，不是缺陷）。改用 schema.getTable 的
    // 精确行数 —— 新引擎自己维护记录数，不再需要"估计"。
    const info = await ipc("schema.getTable", { name: TABLE });
    step(
      "表里正好 1200 行（一行不多一行不少）",
      Number(info.row_estimate) === 1200,
      "实际=" + info.row_estimate
    );

    // ---------- 6. 重名保护 ----------
    let dupMsg = "";
    try {
      const planId2 = (await ipc("app.e2eSource")).token;
      await ipc("import.begin", {
        planId: planId2,
        table: TABLE,
        sheetIndex: 0,
        headerRow: plan.header_row,
        columns: cols,
      });
    } catch (e) {
      dupMsg = (e && e.message) || String(e);
    }
    step("同名表被明确拒绝（不覆盖已有数据）", /已经有一张叫/.test(dupMsg), dupMsg.slice(0, 60));

    // ---------- 7. 清理 ----------
    await ipc("schema.dropTable", { name: TABLE, confirmName: TABLE });
    await dbApi.refreshTables();
    const gone = await waitFor(
      () => !Array.from(document.querySelectorAll(".db-table-item .t")).find((e) => text(e) === TABLE),
      5000
    );
    step("删表之后左栏不再显示它", gone);

    prog.remove();
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
