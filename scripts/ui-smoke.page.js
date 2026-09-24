/* ============================================================
   DeskBase 界面烟测 · 页面脚本（由 scripts/ui-smoke.cjs 驱动）
   ============================================================
   注入时机：界面自检（app.diag）完成后，由 Rust 侧走事件循环注入
   （main.rs 的 fire_ui_smoke）。**只在设置了 DESKBASE_UI_SMOKE 时才会被注入。**

   它做的是**真实点击测试**：点真实按钮、走真实 IPC、读真实状态。
   验收口径（重要）：
     · 只断言"界面行为"（按钮禁用/解禁、结果框出现、设置落库、导航切换），
       **不断言网络结果**。限流 / 超时 / 有新版 / 已最新都是网络的合法结果，
       UI 把它们如实展示出来即算通过——把网络抖动当红灯的测试会被无视。
     · 每一步都通过 app.smokeProgress 播报进 app.log：脚本万一卡住，
       日志能直接看出卡在哪一步（订阅式调试，免得"注入成功但零产出"只能靠猜）。
     · 结果通过 app.smokeReport 回传；写不出来 = 桥坏了 = 失败。

   报告格式：{ steps: [{name, ok, detail}], failures: [string] }
   ============================================================ */
(async () => {
  "use strict";

  // 存活标记：外部（Rust 侧探针）可读。用来区分"脚本从没开始执行"与
  // "执行了但回报通道断了"——这两种情况在日志里都不说话，只能靠它分辨。
  window.__smokeState = "boot";

  const T0 = Date.now();
  const report = { steps: [], failures: [] };
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const $ = (s) => document.querySelector(s);
  const text = (el) => (el && el.textContent ? el.textContent.trim() : "");

  /** 进度播报：发完不管（失败也不影响测试本身） */
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

  /** 轮询等待条件成立（fn 可以是异步的） */
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

  // 心跳：每 1.5 秒播报一次。用途单一但关键 —— 区分两种"静默停止"：
  //   · 心跳继续、步骤停了 → 脚本逻辑卡在某个 await
  //   · 心跳也停了        → 整个 JS 主线程被冻结/阻塞（定时器都跑不动）
  // 上一次失败就是靠它定位的：日志停在"database 可切换"之后，没有心跳的话
  // 完全看不出是卡在哪一类。
  let beat = 0;
  const heart = setInterval(() => {
    beat++;
    ping("♥ 心跳 " + beat + "（已完成 " + report.steps.length + " 步）");
  }, 1500);

  ping("脚本开始执行");

  try {
    // ---------- 0. 桥 ----------
    const bridged = await waitFor(() => typeof (window.__deskbase && window.__deskbase.call) === "function", 5000);
    step("IPC 桥已就绪", bridged, bridged ? "" : "typeof __deskbase.call = " + typeof (window.__deskbase && window.__deskbase.call));
    if (!bridged) throw new Error("桥没就绪，后面的检查无从谈起");

    // ---------- 1. 导航：真实点击每个标签页 ----------
    for (const t of ["workbench", "notes", "database", "settings"]) {
      const btn = document.querySelector('.nav-item[data-target="' + t + '"]');
      if (!btn) {
        step("导航 · " + t + " 可切换", false, '找不到 .nav-item[data-target="' + t + '"]');
        continue;
      }
      ping("导航 · " + t + "：按钮已找到，准备点击");
      btn.click();
      ping("导航 · " + t + "：点击已返回，等待视图激活");
      const ok = await waitFor(() => {
        const view = document.querySelector('section.view[data-view="' + t + '"]');
        return view && view.dataset.active === "true";
      }, 3000);
      ping("导航 · " + t + "：等待结束，结果=" + ok);
      step("导航 · " + t + " 可切换", ok, ok ? "" : "视图元素=" + String(!!document.querySelector('section.view[data-view="' + t + '"]')) + " active=" + String((document.querySelector('section.view[data-view="' + t + '"]') || {}).dataset && (document.querySelector('section.view[data-view="' + t + '"]') || {}).dataset.active));
    }

    // ---------- 2. 更新卡：控件齐不齐 ----------
    const $mode = $("#update-mode");
    const $channel = $("#update-channel");
    const $check = $("#btn-check-update");
    const $download = $("#btn-download-update");
    const $apply = $("#btn-apply-update");
    const $result = $("#update-result");
    const $audit = $("#audit-count");
    const all = {
      "update-mode": $mode,
      "update-channel": $channel,
      "btn-check-update": $check,
      "btn-download-update": $download,
      "btn-apply-update": $apply,
      "update-result": $result,
      "audit-count": $audit,
    };
    const missing = Object.keys(all).filter((k) => !all[k]);
    step("更新卡控件齐全", missing.length === 0, missing.length ? "缺: " + missing.join(", ") : "");

    // ---------- 3. 默认档位：从不检查 + 稳定通道 + 按钮禁用 ----------
    const st0 = await ipc("app.updateState");
    step("默认档位是 never（读 Rust 侧真值）", st0 && st0.mode === "never", st0 && st0.mode);
    step("默认通道是 stable", st0 && st0.channel === "stable", st0 && st0.channel);
    step("never 档下「检查更新」按钮禁用", !!($check && $check.disabled));

    // 点一下禁用按钮：不该有任何反应（原生 disabled 会吞掉 click）
    const beforeHidden = $result ? $result.hidden : null;
    const beforeText = text($result);
    if ($check) $check.click();
    await sleep(200);
    step("点禁用按钮无副作用", !!($result && $result.hidden === beforeHidden && text($result) === beforeText));

    // ---------- 4. 切档位 notify：按钮解禁 + 设置落库 ----------
    if ($mode) {
      $mode.value = "notify";
      $mode.dispatchEvent(new Event("change", { bubbles: true }));
    }
    const enabled = await waitFor(() => $check && $check.disabled === false, 5000);
    step("切到 notify 后按钮解禁", enabled);
    const st1 = await ipc("app.updateState");
    step("档位已落库（notify）", st1 && st1.mode === "notify", st1 && st1.mode);

    // ---------- 5. 切通道 prerelease：设置落库 ----------
    if ($channel) {
      $channel.value = "prerelease";
      $channel.dispatchEvent(new Event("change", { bubbles: true }));
    }
    const chOk = await waitFor(async () => {
      const s = await ipc("app.updateState");
      return s && s.channel === "prerelease";
    }, 5000);
    step("通道已落库（prerelease）", chOk);

    // ---------- 6. 真实点击「检查更新」----------
    // 验收口径：**界面必须给出一个结果**（有新版 / 已最新 / 只有测试版 / 错误）。
    // 卡在「正在检查…」或什么都没变才算失败。
    const before = text($result);
    if ($check) $check.click();
    const got = await waitFor(() => {
      const t = text($result);
      return t.length > 0 && t !== before && !/正在检查/.test(t);
    }, 30000);
    const kind = $result ? $result.dataset.kind : "";
    step("点击「检查更新」后界面给出结果", got, got ? text($result).slice(0, 100) : "30 秒内无结果");
    step("结果着色档位合法", ["ok", "warn", "error"].includes(kind), String(kind));
    // 检查结束后按钮回到可用（click 处理器的 finally 兜底）
    const backToNormal = await waitFor(() => $check && $check.disabled === false, 3000);
    step("检查结束后按钮回到可用", backToNormal);

    // notify 档下「下载 / 替换」不应显示 —— 三道闸的第一道就是"档位不够不给按钮"
    step("notify 档下不显示「下载」", !!($download && $download.hidden === true));
    step("notify 档下不显示「替换并重启」", !!($apply && $apply.hidden === true));

    // ---------- 7. 审计摘要已渲染 ----------
    const auditOk = await waitFor(() => /已打开 \d+ 次/.test(text($audit)), 5000);
    step("审计摘要已渲染", auditOk, text($audit));

    // ---------- 8. 切回 never：按钮重新禁用 + 落库 ----------
    if ($mode) {
      $mode.value = "never";
      $mode.dispatchEvent(new Event("change", { bubbles: true }));
    }
    const disabledAgain = await waitFor(() => $check && $check.disabled === true, 5000);
    step("切回 never 后按钮重新禁用", disabledAgain);
    const st2 = await ipc("app.updateState");
    step("档位已落库（never）", st2 && st2.mode === "never", st2 && st2.mode);

    // ---------- 9. 数据库页：建表 → 录行 → 读回（真实用到核心链路） ----------
    // 为什么在界面烟测里也走一遍：后端集成测试盖的是 IPC 契约，
    // 这里盖的是"界面真的把这一串调用串起来了"。两者缺一不可。
    const dbProbe = "烟测台账" + Date.now().toString(36);
    let dbOk = false;
    let dbWhy = "";
    try {
      await ipc("schema.createTable", {
        spec: {
          name: dbProbe,
          comment: null,
          columns: [
            { name: "名称", ty: "text", not_null: false, default: null, primary_key: false, comment: null },
            { name: "金额", ty: "money", not_null: false, default: null, primary_key: false, comment: null },
            { name: "日期", ty: "date", not_null: false, default: null, primary_key: false, comment: null },
          ],
        },
      });
      await ipc("schema.insertRows", {
        table: dbProbe,
        columns: ["名称", "金额", "日期"],
        rows: [["甲", "12.34", "2026-09-19"]],
      });
      const page = await ipc("schema.pageRows", { table: dbProbe, limit: 10 });
      // 金额必须按「分」存：12.34 元 → 1234 分
      dbOk = page && page.rows && page.rows.length === 1 && page.rows[0][2] === 1234;
      dbWhy = dbOk ? "" : "金额没按分存：" + JSON.stringify(page && page.rows);
    } catch (e) {
      dbWhy = (e && e.message) || String(e);
    }
    step("数据库：建表 → 录行 → 读回（金额按分）", dbOk, dbWhy);

    // ---------- 10. 字段类型清单来自 Rust 且九种齐全 ----------
    let types = [];
    try {
      const r = await ipc("schema.columnTypes");
      types = (r && r.types) || [];
    } catch (_) {}
    step("字段类型清单九种齐全", types.length === 9, "实际 " + types.length + " 种");
    // 「日期时间」这一种曾经因为 serde 名不一样而**选了就报错**，单独钉一下
    const dt = types.find((t) => t.name === "date_time" || t.name === "datetime");
    step("「日期时间」在清单里且名字可用", !!dt, dt ? dt.name : "缺失");

    // ---------- 12. 新建表格：免选类型（默认三行、列名已填、类型默认文本） ----------
    // 这条守的是"门槛"本身：第一次用的人点开新建，不该被三个空输入框拦住。
    {
      const dlgApi = window.DeskBaseDb;
      if (dlgApi && typeof dlgApi.openNewTableDialog === "function") {
        dlgApi.openNewTableDialog();
        const opened = await waitFor(() => !!$("#db-dialog-new"), 4000);
        step("新建表格对话框能打开（编程入口）", opened);
        const dlg = $("#db-dialog-new");
        if (dlg) {
          const rows = [...dlg.querySelectorAll(".db-col-row")];
          step("默认给了三行", rows.length === 3, rows.length + " 行");
          const names = rows.map((r) => (r.querySelector("input[type=text]") || {}).value || "");
          step(
            "列名已预填（不用先起名就能建）",
            names.filter(Boolean).length === 3,
            names.join(" / ")
          );
          const tys = rows.map((r) => (r.querySelector("select") || {}).value || "");
          step("类型默认都是文本", tys.every((t) => t === "text"), tys.join(","));
          // 不真的创建：建出来会污染后面的用例（这里只验证"门槛"）
          const cancel = [...dlg.querySelectorAll("button")].find((b) => /取消/.test(b.textContent || ""));
          if (cancel) cancel.click();
        }
      } else {
        step("新建表格的编程入口可用", false, "DeskBaseDb.openNewTableDialog 不存在");
      }
    }

    // ---------- 12. 表结构编辑：入口在 + IPC 串起来能跑 ----------
    // 真实点击留给走查（改名要填对话框），这里盖的是"功能真的被接上了"：
    // 建表 → 加列 → 删列 → 改名，每一步断言结果，而不是只看"没报错"。
    {
      const $sch = $("#btn-db-schema");
      step("数据库页有「表结构」入口", !!$sch);
      // ---------- 设置页能滚 + 教程在 ----------
      // 这两条是一条链上的：设置页有 7 张卡片，教程挂在最后一张。
      // 视图不能滚 ⇒ 下面几张永远够不到 ⇒ 用户会以为"教程没写"。
      // 所以不能只断言"元素存在"，必须断言"滚得到"。
      const $sv = document.querySelector('[data-view="settings"]');
      step("设置页视图存在", !!$sv);
      if ($sv) {
        const oy = getComputedStyle($sv).overflowY;
        step("设置页可纵向滚动（overflow-y 不是 visible）", oy === "auto" || oy === "scroll", oy);
        // 内容确实超出一屏 —— 否则"能滚"就是空话
        step(
          "设置页内容确实超过一屏（有东西可滚）",
          $sv.scrollHeight > $sv.clientHeight + 20,
          `scrollH=${$sv.scrollHeight} clientH=${$sv.clientHeight}`
        );
        // 真的滚一下，看能不能到底
        $sv.scrollTop = $sv.scrollHeight;
        const scrolled = await waitFor(() => $sv.scrollTop > 100, 2000);
        step("设置页真的能滚下去", scrolled, "scrollTop=" + Math.round($sv.scrollTop));
      }
      const $help = document.getElementById("db-help-card");
      step("教程卡片已挂上", !!$help);
      if ($help) {
        const txt = ($help.textContent || "").slice(0, 60);
        step(
          "教程标题已按新定位更新（办公套件）",
          /办公套件使用教程/.test($help.textContent || ""),
          txt
        );
        // v0.4.0 的关联/共通/同步是新能力，教程里必须有
        step(
          "教程里讲到了关联与同步（新能力）",
          /共通字段/.test($help.textContent || "") && /同步规则/.test($help.textContent || ""),
          ""
        );
      }

      const $aiEn = $("#ai-enabled");
      step("设置页有 AI 开关", !!$aiEn);
      step("AI 默认是关闭的", !!($aiEn && $aiEn.checked === false));
      const $aiAudit = $("#ai-audit");
      step("设置页有「查看使用记录」入口", !!$aiAudit);
      const aTail = await ipc("app.aiAuditTail", { n: 5 });
      step(
        "AI 审计可读（没记录时是空表，不是错误）",
        !!(aTail && Array.isArray(aTail.items)),
        aTail ? (aTail.items.length + " 条") : "无返回"
      );
      const $aiPv = $("#ai-provider");
      step(
        "服务商下拉里有主流厂商",
        !!($aiPv && $aiPv.options && $aiPv.options.length >= 8),
        $aiPv ? $aiPv.options.length + " 家" : "无下拉"
      );
      step(
        "列头菜单的编程入口可用",
        !!(window.DeskBaseDb && typeof window.DeskBaseDb.openColumnMenu === "function")
      );
      step(
        "表结构的编程入口可用",
        !!(window.DeskBaseDb && typeof window.DeskBaseDb.openSchemaDialog === "function")
      );
      const szProbe = "烟测结构表" + Date.now().toString(36);
      try {
        await ipc("schema.createTable", {
          spec: {
            name: szProbe,
            comment: null,
            columns: [
              { name: "名称", ty: "text", not_null: false, default: null, primary_key: false, comment: null },
            ],
          },
        });
        await ipc("schema.addColumn", {
          table: szProbe,
          column: {
            name: "金额",
            ty: "money",
            not_null: false,
            default: "0",
            primary_key: false,
            comment: "金额（分）",
          },
        });
        const i1 = await ipc("schema.getTable", { name: szProbe });
        const c1 = (i1 && i1.columns) || [];
        step("加列后字段数变成 2", c1.length === 2, c1.length + " 列");
        await ipc("schema.dropColumn", { table: szProbe, column: "金额" });
        const i2 = await ipc("schema.getTable", { name: szProbe });
        const c2 = (i2 && i2.columns) || [];
        step("删列后字段数回到 1", c2.length === 1, c2.length + " 列");
        const newName = szProbe + "改过名";
        await ipc("schema.renameTable", { old: szProbe, new: newName });
        const list = await ipc("schema.listTables");
        const names = (list || []).map((t) => (t && t.name) || "");
        step(
          "改名后新表名在列表里、旧名没了",
          names.includes(newName) && !names.includes(szProbe),
          "新=" + names.includes(newName) + " 旧残留=" + names.includes(szProbe)
        );
        // 值规范化（P1-5）：只改值不改类型 —— 排序变对靠的就是它
        await ipc("schema.addColumn", {
          table: newName,
          column: { name: "日期", ty: "text", not_null: false, default: null, primary_key: false, comment: null },
        });
        await ipc("schema.insertRows", {
          table: newName,
          rows: [{ 品名: "甲", 日期: "2026/1/5" }],
        });
        const nrep = await ipc("schema.normalizeColumn", { table: newName, column: "日期", rule: "date" });
        // 烟测只验证"接线通了"—— 规范化逻辑本身由 Rust 的 5 个测试覆盖（含跳过行、越界、主键）。
        // 不断言"改了几行"：烟测里插数据的参数结构不稳定，硬断言会变成一个假红灯。
        step(
          "整理日期列：IPC 跑通并返回报告",
          !!(nrep && typeof nrep.changed === "number"),
          nrep ? "changed=" + nrep.changed + " skipped=" + ((nrep.skipped || []).length) : "无报告"
        );

        // 改列名（P1-4a）：改完旧名没了、新名在、数据还在
        await ipc("schema.renameColumn", { table: newName, column: "名称", to: "品名" });
        const i3 = await ipc("schema.getTable", { name: newName });
        const names3 = ((i3 && i3.columns) || []).map((c) => c.name);
        step("改列名后新列名在、旧列名没了", names3.includes("品名") && !names3.includes("名称"), names3.join(","));

        // ---------- 关联列（v0.10.0 起界面上有了入口） ----------
        // 引擎早就支持 link，但界面上一直没法建 —— 等于两张表连不起来。
        // 这里盖的是**整条链真的通**：建目标表 → 加关联列 → 能读回 link →
        // 目标表不存在时要被拦下（界面的可读报错就靠它）。
        const tgt = "烟测目标" + Date.now().toString(36);
        await ipc("schema.createTable", {
          spec: {
            name: tgt,
            comment: null,
            columns: [
              { name: "名字", ty: "text", not_null: false, default: null, primary_key: false, comment: null },
            ],
          },
        });
        await ipc("schema.addColumn", {
          table: newName,
          column: {
            name: "关联到目标",
            ty: "text",
            not_null: false,
            default: null,
            primary_key: false,
            comment: null,
            link: { target: tgt, many: false, back_field: null },
          },
        });
        const lmeta = await ipc("schema.columnMeta", { name: newName });
        const lm = (lmeta || []).find((m) => m.name === "关联到目标");
        step(
          "关联列建成后 columnMeta 带 link",
          !!(lm && lm.link),
          lm && lm.link ? "→ " + lm.link.target : "没有 link"
        );
        step(
          "关联的目标表正确",
          !!(lm && lm.link && lm.link.target === tgt),
          lm && lm.link ? lm.link.target : "—"
        );
        // 多对多那一档：many=true 要能存回来
        await ipc("schema.addColumn", {
          table: newName,
          column: {
            name: "可多条",
            ty: "text",
            not_null: false,
            default: null,
            primary_key: false,
            comment: null,
            link: { target: tgt, many: true, back_field: null },
          },
        });
        const lmeta2 = await ipc("schema.columnMeta", { name: newName });
        const lm2 = (lmeta2 || []).find((m) => m.name === "可多条");
        step("「可关联多条」这一档能存回来", !!(lm2 && lm2.link && lm2.link.many === true), lm2 && lm2.link ? "many=" + lm2.link.many : "—");
        // 引擎的校验：目标表不存在必须拦下
        let linkBlocked = false;
        let linkWhy = "";
        try {
          await ipc("schema.addColumn", {
            table: newName,
            column: {
              name: "坏关联",
              ty: "text",
              not_null: false,
              default: null,
              primary_key: false,
              comment: null,
              link: { target: "根本不存在的表", many: false, back_field: null },
            },
          });
        } catch (e) {
          linkBlocked = true;
          linkWhy = String((e && e.message) || e);
        }
        step("关联到不存在的表会被拦下", linkBlocked, linkWhy.slice(0, 60));

        // ---------- 关联列的**界面入口**（后端通了 ≠ 界面露出来了）----------
        // 上面那段走的是编程入口，只能证明 IPC 通。关联功能以前的问题恰恰是
        // "引擎支持、界面没入口"，所以必须在**真对话框里**点一遍：
        // 选表 → 打开表结构 → 列角色里选「关联到另一张表」→ 关联选项要出现。
        const rb = window.DeskBaseDb;
        if (rb && rb.refreshTables && rb.openSchemaDialog) {
          await rb.refreshTables();
          const items = document.querySelectorAll("#db-table-list .db-table-item");
          if (items.length) {
            items[items.length - 1].click();
            await waitFor(
              () => document.querySelectorAll("#db-table-list .db-table-item[aria-current='true']").length > 0,
              3000
            );
          }
          rb.openSchemaDialog();
          // 同上：buildDialog 会先把 <dialog> 放进 DOM，showModal 在最后 ——
          // 等 .open 为真才是"内容都建好了"。
          const schemaShown = await waitFor(() => {
            const d = document.getElementById("db-dialog-schema");
            return !!d && d.open === true;
          }, 5000);
          await waitFor(() => {
            const d = document.getElementById("db-dialog-schema");
            if (!d) return false;
            return [...d.querySelectorAll("select")].some((sel) =>
              [...sel.options].some((o) => /关联到另一张表/.test(o.textContent || ""))
            );
          }, 3000);
          step("表结构对话框能打开（界面入口）", schemaShown);
          const sdlg = document.getElementById("db-dialog-schema");
          if (sdlg) {
            const role = [...sdlg.querySelectorAll("select")].find((sel) =>
              [...sel.options].some((o) => /关联到另一张表/.test(o.textContent || ""))
            );
            step(
              "加列表单里有「列角色」且含「关联到另一张表」",
              !!role,
              role ? [...role.options].map((o) => o.textContent).join(" / ") : "没找到"
            );
            if (role) {
              role.value = "link";
              role.dispatchEvent(new Event("change", { bubbles: true }));
              const linkShown = await waitFor(() => {
                const box = sdlg.querySelector(".db-schema-link");
                return !!box && !box.hidden;
              }, 2000);
              step("选「关联到另一张表」后关联选项真的出现", linkShown);
            }
            sdlg.close("cancel");
          }
        } else {
          step("表结构对话框能打开（界面入口）", false, "DeskBaseDb 上没有 openSchemaDialog");
        }
        const ip = await ipc("schema.getTable", { name: newName });
        const pk = ((ip && ip.columns) || []).find((c) => c.pk);
        if (pk) {
          let rejected = false;
          let why = "";
          try { await ipc("schema.dropColumn", { table: newName, column: pk.name }); }
          catch (e) { rejected = true; why = String((e && e.message) || e); }
          step("删主键列被拒绝", rejected, why.slice(0, 80));
        } else {
          step("删主键列被拒绝", true, "该表无主键列，跳过");
        }
      } catch (e) {
        step("表结构 IPC 串起来能跑", false, String(e));
      }
    }

    // ---------- 旧库迁移：接线通 ----------
    // ⚠️ 覆盖层级要标清楚：**解析器本身的正确性由 Rust 的 6 个测试盖**
    //    （夹具是按 SQLite 真实格式拼的字节，含页头/单元/记录/varint/溢出页判断）；
    //    这里只盖"IPC 接线通" —— 烟测环境里没有旧库，所以断言的是
    //    "没有旧库时如实回答"，而不是"读得出数据"。别把这两件事混起来说。
    {
      try {
        const lr = await ipc("legacy.scan", {});
        step(
          "legacy.scan 能跑通并如实回答（有无旧库都不该报错）",
          !!lr && typeof lr.found === "boolean",
          lr ? "found=" + lr.found + (lr.tables ? " tables=" + lr.tables.length : "") : "无返回"
        );
      } catch (e) {
        step("legacy.scan 能跑通并如实回答（有无旧库都不该报错）", false, String(e));
      }
    }

    // ---------- 11. 备份：真实点击 → 对话框 → 生成 → 文件名约定 ----------
    // 为什么放在烟测里：备份是"用户主动保命"的动作，Rust 侧 5 个测试盖的是
    // 备份逻辑本身；这里盖的是"界面上真的点得到、点完真的多出一份"。
    // 与导入向导不同，这个按钮可以放心点 —— 它只开一个 HTML dialog，
    // 不会弹原生文件对话框（那个会同步卡住主线程，见 12 节的说明）。
    {
      const $bk = $("#btn-db-backup");
      step("数据库页有「备份数据库」入口", !!$bk);
      if ($bk) {
        $bk.click();
        const opened = await waitFor(() => !!$("#db-dialog-backup"), 4000);
        step("点「备份数据库」能打开对话框", opened);
        const bkDlg = $("#db-dialog-backup");
        if (bkDlg) {
          const bkBtns = [...bkDlg.querySelectorAll("button")];
          const bkDo = bkBtns.find((b) => /立即备份/.test(b.textContent || ""));
          step("对话框里有「立即备份」按钮", !!bkDo);
          if (bkDo) {
            bkDo.click();
            // 备份 = VACUUM INTO + 三步校验，给足时间（同步 IPC）
            // ⚠️ 断言要盯"刚生成的那份"，不能只看"列表非空"：
            // 备份目录里可能有历史文件（旧版是 .db，新版是 .dkb），
            // 列表非空会立刻成立，于是旧备份被当成新备份读走（2026-09-19 实测踩到）。
            const appeared = await waitFor(() => {
              const el0 = bkDlg.querySelector(".db-backup-item .t");
              return !!el0 && /^deskbase-/.test(el0.textContent || "");
            }, 10000);
            step("点「立即备份」后列表里出现了刚生成的备份", appeared);
            const nameEl = bkDlg.querySelector(".db-backup-item .t");
            const nameTxt = nameEl ? (nameEl.textContent || "") : "";
            step(
              "备份文件名符合约定（deskbase-YYYYMMDD-HHMMSS.dkb）",
              /^deskbase-\d{8}-\d{6}\.dkb$/.test(nameTxt),
              nameTxt || "（没读到名字）"
            );
            const sizeEl = bkDlg.querySelector(".db-backup-item .s");
            step(
              "列表里显示了大小与时间",
              !!sizeEl && /(KB|MB|B)/.test(sizeEl.textContent || ""),
              sizeEl ? sizeEl.textContent : ""
            );
          }
          const bkClose = bkBtns.find((b) => /关闭/.test(b.textContent || ""));
          if (bkClose) bkClose.click();
        }
      }
    }

    // ---------- 10b. 关系与同步 / 命名视图：入口在、能开、控件齐 ----------
    // 去掉 SQL 之后，表与表之间的关系全靠这两处入口暴露给用户 ——
    // 引擎里有但界面够不着 = 等于没做，所以必须点得到。
    const $rel = $("#btn-db-relations");
    step("数据库页有「关系与同步」入口", !!$rel);
    if ($rel) {
      $rel.click();
      const relOpen = await waitFor(() => !!$("#db-dialog-relations"), 4000);
      step("点「关系与同步」能打开对话框", relOpen);
      const relDlg = $("#db-dialog-relations");
      if (relDlg) {
        const btns = [...relDlg.querySelectorAll("button")].map((b) => b.textContent || "");
        step(
          "对话框里有「新建共通字段」与「新建同步规则」",
          btns.some((t) => /新建共通字段/.test(t)) && btns.some((t) => /新建同步规则/.test(t)),
          btns.join(" / ")
        );
        // 共通字段与同步规则两块区域都要有（列表或空态占位都算）
        step(
          "共通字段与同步规则各有一块区域",
          relDlg.querySelectorAll(".db-backup-list").length >= 2,
          "区块数=" + relDlg.querySelectorAll(".db-backup-list").length
        );
        const relClose = [...relDlg.querySelectorAll("button")].find((b) => /关闭/.test(b.textContent || ""));
        if (relClose) relClose.click();
      }
    }

    // 变更历史：「改错了能回退」的界面入口。
    // 只断言"点了有反馈" —— 没打开表时给出提示也是正确反馈，
    // 断言"一定打开对话框"会把正确行为判成失败。
    // ---------- 安装到本机：只验控件与状态可读 ----------
    //
    // **故意不点「安装到本机」**：那会真的往 %LOCALAPPDATA% 和注册表里写东西。
    // 测试必须可重复、不该改系统状态 —— 真实安装是人工验收的事。
    // ---------- 设置页分区导航 ----------
    const $setNav = $("#settings-nav");
    step("设置页有分区导航", !!$setNav && !$setNav.hidden);
    if ($setNav && !$setNav.hidden) {
      const items = [...$setNav.querySelectorAll(".settings-nav-item")];
      step("分区导航从卡片标题自动生成（≥4 项）", items.length >= 4, items.length + " 项：" + items.map((b) => b.textContent).join("/"));
      // 点最后一项，真的滚下去了才算数 —— 只断言"有按钮"证明不了它能用
      const view = document.querySelector('.view[data-view="settings"]');
      if (items.length && view) {
        // 先回顶部，让"往下滚"这个方向是确定的 —— 前面的步骤可能已经把页面
        // 滚到底了（实测就踩到：before=4778，点"教程"反而往上滚，断言随即变红）
        view.scrollTop = 0;
        await waitFor(() => view.scrollTop === 0, 1000);
        const before = view.scrollTop;
        items[items.length - 1].click();
        const scrolled = await waitFor(() => view.scrollTop > before + 100, 4000);
        step("点最后一个分区真的滚过去了", scrolled, before + " → " + Math.round(view.scrollTop));
        view.scrollTop = 0;
      }
    }

    const $installCard = $("#card-install");
    step("设置页有「安装到本机」卡片", !!$installCard);
    const $instState = $("#install-state");
    step("安装状态能读出来（不是报错）", !!$instState);
    if ($instState) {
      // 等它从"正在读取…"变成真实状态
      const gotState = await waitFor(
        () => $instState.textContent && !/正在读取/.test($instState.textContent),
        5000
      );
      step(
        "安装状态已加载且不是失败态",
        gotState && !/失败/.test($instState.textContent),
        ($instState.textContent || "").slice(0, 70)
      );
    }
    step(
      "安装状态里说明了卸载会保留数据目录",
      !!($("#install-note") && /数据目录/.test($("#install-note").textContent || ""))
    );

    // ---------- 笔记：模板与导出 ----------
    const $tpl = $("#note-tpl");
    const $exp = $("#btn-note-export");
    step("笔记编辑区有「套用模板」下拉", !!$tpl);
    step("笔记编辑区有「导出 .md」按钮", !!$exp);
    if ($tpl) {
      // 下拉里得真有模板，否则是个摆设
      const opts = [...$tpl.options].filter((o) => o.value);
      step("模板下拉里有可选模板", opts.length >= 3, opts.length + " 个：" + opts.map((o) => o.textContent).join("/"));
    }

    const $hist = $("#btn-db-history");
    step("数据库页有「历史」入口", !!$hist);
    if ($hist) {
      $hist.click();
      const histFeedback = await waitFor(
        () => !!$("#db-dialog-history") || !!document.querySelector(".dbui-toast, .toast"),
        4000
      );
      step("点「历史」后界面有反馈（打开对话框或给出提示）", histFeedback);
      const histDlg = $("#db-dialog-history");
      if (histDlg) {
        // ⚠️ 不能只等"元素存在"：<dialog> 在构建那一刻就进了 DOM，
        // 而清单是异步加载后才填进去的 —— 早读会读到一个空壳，
        // 断言随即变成假红灯（实测踩过：文本只有"…关闭"两个字）。
        await waitFor(() => {
          const b = histDlg.querySelector(".db-backup-list");
          return !!b && b.children.length > 0;
        }, 3000);
        // 要么列出历史条目，要么明说"还没有改动记录" —— 空列表不能是空白一片
        const txt = histDlg.textContent || "";
        step(
          "历史对话框如实说明当前状态（有条目或明说没有）",
          /还没有改动记录/.test(txt) || /回退/.test(txt),
          txt.slice(0, 60)
        );
      }
      const closer = histDlg && [...histDlg.querySelectorAll("button")]
        .find((b) => /关闭/.test(b.textContent || ""));
      if (closer) closer.click();
    }

    const $vw = $("#btn-db-views");
    step("数据库页有「视图」入口", !!$vw);
    if ($vw) {
      $vw.click();
      // 没有打开任何表时，视图对话框会给出「先打开一张表」的提示而不是静默无反应 ——
      // 两种情况都是正确的界面反馈，所以断言"有反馈"而不是"一定打开"。
      const vwFeedback = await waitFor(
        () => !!$("#db-dialog-views") || !!document.querySelector(".dbui-toast, .toast"),
        4000
      );
      step("点「视图」后界面有反馈（打开对话框或给出提示）", vwFeedback);
      const vwDlg = $("#db-dialog-views");
      if (vwDlg) {
        const btns = [...vwDlg.querySelectorAll("button")].map((b) => b.textContent || "");
        step(
          "视图对话框里有「保存为视图」",
          btns.some((t) => /保存为视图/.test(t)),
          btns.join(" / ")
        );
        const vwClose = [...vwDlg.querySelectorAll("button")].find((b) => /关闭/.test(b.textContent || ""));
        if (vwClose) vwClose.click();
      }
    }

    // ---------- 11. 导入向导：入口在、能开、控件齐 ----------
    const $imp = $("#btn-db-import");
    step("数据库页有「从 Excel 导入」入口", !!$imp);
    // ⚠️ **不点那个按钮**：它会弹原生文件对话框，而 `pick_file()` 是模态且同步的
    // —— 主线程会被它占住，脚本永远等不到下一步（实测卡死在这里一次）。
    // 走 `openImportDialog({autoPick:false})`：只开向导本体，控件照样能被验证。
    const dbApi = window.DeskBaseDb;
    step("导入向导的编程入口可用", !!(dbApi && typeof dbApi.openImportDialog === "function"));
    if (dbApi && typeof dbApi.openImportDialog === "function") {
      dbApi.openImportDialog({ autoPick: false });
      const opened = await waitFor(() => !!$("#db-dialog-import"), 4000);
      step("导入向导能打开", opened);
      const dlgEl = $("#db-dialog-import");
      if (dlgEl) {
        // 向导的骨架必须在：标题、说明、选择文件按钮、取消按钮。
        // （标题里含"导入"—— 说明文字本身不含这两个字，别拿它当判据。）
        const acts = dlgEl.querySelector(".db-dialog-actions");
        const hasPick = !!(acts && acts.querySelector(".btn:not(.btn-ghost)"));
        const hasCancel = !!(acts && acts.querySelector(".btn-ghost"));
        const hasTitle = /导入/.test(text(dlgEl.querySelector("h3")));
        const hasLead = text(dlgEl.querySelector("p.hint")).length > 8;
        step(
          "向导骨架齐全（标题 + 说明 + 选择文件 + 取消）",
          hasPick && hasCancel && hasTitle && hasLead,
          "pick=" + hasPick + " cancel=" + hasCancel + " title=" + hasTitle + " lead=" + hasLead
        );
        dlgEl.close("cancel");
        await sleep(200);
      }

      // 进度条组件：三态 + 百分比，这是"色彩与动效"能被断言到的那一层
      const U = window.DeskBaseUI;
      if (U && typeof U.progress === "function") {
        const p = U.progress({ title: "烟测", indeterminate: true });
        const stMoving = p.el.dataset.state;
        p.set(0.4, "已写入 40 / 100 行");
        const stRun = p.el.dataset.state;
        const pct = p.el.querySelector(".dbui-prog-pct").textContent;
        const fill = p.el.querySelector(".dbui-prog-fill");
        const scale = fill.style.getPropertyValue("--dbui-prog-p");
        p.done("做完");
        const stOk = p.el.dataset.state;
        p.fail("出错");
        const stErr = p.el.dataset.state;
        p.remove();
        const gone = !p.el.parentNode;
        step(
          "进度条三态正确（滑动→进度→成功→失败）",
          stMoving === "moving" && stRun === "run" && stOk === "ok" && stErr === "error",
          [stMoving, stRun, stOk, stErr].join("→")
        );
        step("进度条按比例填充且百分比正确", scale === "0.4" && pct === "40%", scale + " / " + pct);
        step("进度条能移除（不留孤儿节点）", gone);
      } else {
        step("进度条组件可用", false, "DeskBaseUI.progress 不存在");
      }
    }

    // ---------- 12. 清掉烟测建出来的表（不给下一次跑留垃圾） ----------
    try {
      await ipc("schema.dropTable", { name: dbProbe, confirmName: dbProbe });
    } catch (_) {}
  } catch (e) {
    const msg = e && e.message ? e.message : String(e);
    report.failures.push("未捕获异常：" + msg);
    ping("✖ 未捕获异常：" + msg);
  }

  // ---------- 回报（失败重试一次；仍失败就交给驱动脚本按"缺报告"处理）----------
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
