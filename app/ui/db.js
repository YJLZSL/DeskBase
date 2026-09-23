/* ============================================================
   DeskBase 数据库页装配层
   ============================================================
   数据网格（grid.js）只认识"数据从哪来、
   结果往哪去"，不认识这个应用。本文件负责把它们装配成「数据库」页：

     左栏 表格列表（listTables / createTable / dropTable）
     右栏 数据页签  → DeskBaseGrid.mount(...)，行数据走 keyset 分页
          （SQL 页签已于 2026-09-20 按 ADR-0020 撤下：数据网格成为唯一面板。
            DeskBaseSql 的挂载代码与策略层闸门刻意保留在下方，是零成本的可逆点。）

   与 Rust 的通信走 window.__deskbase.call（app.js 暴露的 IPC 桥）。
   本文件不直接碰数据库，也不拼任何 SQL —— 参数校验都在 Rust 侧。

   三条贯穿全文件的约定（来自 schema.rs 的接口约定，不得破坏）：
   1. Page.columns[0] 恒为 "_rowid"、rows[i][0] 是行号 —— 行号是**行标识**，
      不显示给用户。行对象在交给网格前被映射成 { __rowid, 列名: 值 }，
      网格靠 opts.rowId 取 __rowid 调 updateCell / deleteRows。
   2. money 列按「分」存整数 —— 出库时换算成"元"的字符串给用户看，
      提交时原样交回字符串，由 Rust 的 money_parse 再换算回分。
      两头都不在本文件里做算术，避免第二套实现。
   3. （已撤下）危险 SQL 的判定与确认在**两个**层面：sql.js 的粗判（组件层）+
      schema.runQuery 的策略层闸门（needsConfirm 应答）。本文件负责把
      策略层的确认请求用模态框问出来，用户拒绝就抛错停手。
   ============================================================ */
(function () {
  "use strict";

  if (window.DeskBaseDb) return;

  // ---------- 小工具 ----------
  function el(tag, attrs, text) {
    const node = document.createElement(tag);
    if (attrs) {
      for (const k in attrs) {
        if (attrs[k] == null) continue;
        node.setAttribute(k, attrs[k]);
      }
    }
    if (text != null) node.textContent = text;
    return node;
  }

  function errText(e) {
    if (!e) return "未知错误";
    if (typeof e === "string") return e;
    return e.message || String(e);
  }

  function ui() {
    return window.DeskBaseUI || null;
  }

  function toast(text, kind) {
    const U = ui();
    if (U && U.toast) U.toast(text, kind ? { kind } : undefined);
    else console.log("[DeskBaseDb] " + text);
  }

  function call(cmd, args) {
    const bridge = window.__deskbase;
    if (!bridge || typeof bridge.call !== "function") {
      return Promise.reject(new Error("IPC 桥不可用（app.js 未完成加载？）"));
    }
    return bridge.call(cmd, args);
  }

  /** 金额：分 → "元"字符串（与 Rust 侧 money_display 同一条规则，只此一份 JS 实现） */
  function centsToYuan(cents) {
    const neg = cents < 0;
    const v = Math.abs(cents);
    const s = Math.floor(v / 100) + "." + String(v % 100).padStart(2, "0");
    return neg ? "-" + s : s;
  }

  function clip(s, n) {
    const t = String(s);
    return t.length > n ? t.slice(0, n) + "…" : t;
  }

  /** 模态确认：组件库在就用它，不在就退回系统 confirm —— 闸门宁丑不缺 */
  function confirmBox(title, body) {
    const U = ui();
    if (U && typeof U.confirm === "function") {
      return U.confirm({ title: title, body: body, confirmText: "确认执行", cancelText: "取消", danger: true });
    }
    return Promise.resolve(window.confirm(title + "\n\n" + body));
  }

  // ============================================================
  // 状态
  // ============================================================
  const paneGrid = document.getElementById("db-pane-grid");
  const paneSql = document.getElementById("db-pane-sql");
  const listEl = document.getElementById("db-table-list");
  const currentName = document.getElementById("db-current-name");
  const tabsEl = document.getElementById("db-tabs");
  const sideEl = document.querySelector(".db-side");

  const state = {
    tables: [],
    current: null, // 当前打开的表名
    cols: [], // 当前表的列（{name, type, notNull, comment, pk, default}）
    moneyCols: new Set(), // 语义类型为金额的列名（出库要换算）
    grid: null, // DeskBaseGrid 实例
    sql: null, // DeskBaseSql 实例
    sqlDirty: false, // 建过/删过表后 SQL 侧栏过期，下次切到 SQL 页签重挂
    listed: false, // 表列表是否已经拉过（懒加载）
    listing: null, // 表列表在途的 Promise（并发守卫，见 refreshTables）
    tab: "data",
    viewId: null, // 当前应用的命名视图（null = 直接看整张表）
  };

  // ============================================================
  // 表格列表
  // ============================================================
  async function refreshTables() {
    // 并发守卫：快速来回切视图会连着触发几次，"后来居上"的旧响应可能覆盖新的
    // （列表顺序错位）。在途时直接复用同一个 Promise，不再多发一次 IPC。
    if (state.listing) return state.listing;
    state.listing = (async () => {
      try {
        state.tables = await call("schema.listTables");
        state.listed = true;
        renderTableList();
        // 表集合变了，SQL 侧栏里的表名单也过期了
        if (state.sql) state.sqlDirty = true;
      } catch (e) {
        toast("读取表列表失败：" + errText(e), "error");
      } finally {
        state.listing = null;
      }
    })();
    return state.listing;
  }

  function renderTableList() {
    listEl.textContent = "";
    if (!state.tables.length) {
      listEl.appendChild(
        el("div", { class: "db-empty" },
          "还没有表格。点上方「新建表格」建一张，或者把 Excel / CSV 拖进来。")
      );
      return;
    }
    for (const t of state.tables) {
      const item = el("button", { class: "db-table-item", type: "button" });
      if (t.name === state.current) item.setAttribute("aria-current", "true");
      const title = el("span", { class: "t" }, t.name);
      const ren = el("button", { class: "del", type: "button", title: "给表 " + t.name + " 改名" }, "改名");
      ren.addEventListener("click", (ev) => {
        ev.stopPropagation();
        promptRenameTable(t.name);
      });
      const del = el("button", { class: "del", type: "button", title: "删除表 " + t.name }, "删除");
      del.addEventListener("click", (ev) => {
        ev.stopPropagation();
        askDropTable(t.name);
      });
      const sub = el("span", { class: "d" });
      // row_estimate 是估计值：必须带"约"；-1 表示无法估计，显示"未知"
      const n = t.row_estimate < 0 ? "未知" : "约 " + t.row_estimate + " 行";
      sub.append(el("span", { class: "n" }, n));
      if (t.comment) sub.appendChild(document.createTextNode(" · " + t.comment));
      item.append(ren, del, title, sub);
      item.addEventListener("click", () => openTable(t.name));
      listEl.appendChild(item);
    }
  }

  // ============================================================
  // 打开一张表（数据页签）
  // ============================================================
  async function openTable(name) {
    try {
      const [info, metas] = await Promise.all([
        call("schema.getTable", { name: name }),
        call("schema.columnMeta", { name: name }).catch(() => []),
      ]);
      state.current = name;
      state.moneyCols = new Set();
      state.cols = (info.columns || []).map((c) => {
        const m = (metas || []).find(
          (x) => x.name && x.name.toLowerCase() === c.name.toLowerCase()
        );
        const sem = m && m.semantic;
        if (sem === "money") state.moneyCols.add(c.name);
        return {
          name: c.name,
          // 金额列给网格看的是语义类型（它按文本编辑、提交时由 Rust 换算回分）
          type: sem || c.decl_type,
          not_null: !!c.not_null,
          comment: (m && m.comment) || "",
          pk: !!c.pk,
          default: c.default,
        };
      });

      currentName.textContent = "当前表：" + name;
      renderTableList();
      showTab("data");

      if (state.grid) {
        state.grid.destroy();
        state.grid = null;
      }
      paneGrid.textContent = "";
      if (!window.DeskBaseGrid) {
        paneGrid.appendChild(
          el("div", { class: "db-empty" },
            "数据网格没有加载成功 —— 请检查 app/src/assets.rs 是否登记了 /grid.js。")
        );
        return;
      }
      state.grid = window.DeskBaseGrid.mount(paneGrid, {
        table: name,
        columns: state.cols,
        editable: true,
        // 列头那个「⋯」：网格只报告点了哪一列，动作由这里决定（P1-4b）
        onColumnMenu: openColumnMenu,
        pageSize: 200,
        // 行标识：Page 的第 0 列（_rowid），映射时挂在 __rowid 上，不给用户看
        rowId: (row) => (row && row.__rowid != null ? row.__rowid : null),
        loadPage: loadPage,
        commitCell: commitCell,
        deleteRows: deleteRows,
        insertRow: insertRow,
      });
    } catch (e) {
      toast("打开表失败：" + errText(e), "error");
    }
  }

  /** Page（rows 为数组、首列是行号）→ 网格行对象（__rowid + 列名取值） */
  function toRowObject(page) {
    const names = page.columns.slice(1); // 第 0 个是 _rowid
    return page.rows.map((arr) => {
      const o = { __rowid: arr[0] };
      for (let i = 0; i < names.length; i++) {
        let v = arr[i + 1];
        if (typeof v === "number" && state.moneyCols.has(names[i])) {
          v = centsToYuan(v); // 金额按分存 —— 给用户看的是元
        }
        o[names[i]] = v;
      }
      return o;
    });
  }

  async function loadPage(q) {
    if (!state.current) return { rows: [], hasMore: false, nextCursor: null };
    // 正在看某个命名视图：取数走视图，筛选/排序都在视图配置里，不用再传
    if (state.viewId) {
      const vp = await call("view.page", {
        id: state.viewId,
        cursor: q.after == null ? null : String(q.after),
        limit: q.limit,
      });
      return {
        rows: toRowObject(vp),
        hasMore: !!vp.has_more,
        nextCursor: vp.next_cursor == null ? null : vp.next_cursor,
      };
    }
    const filters = [];
    if (q.filters) {
      for (const k in q.filters) {
        if (q.filters[k]) filters.push([k, String(q.filters[k])]);
      }
    }
    const page = await call("schema.pageRows", {
      table: state.current,
      orderBy: q.sort ? q.sort.column : null,
      desc: !!(q.sort && q.sort.dir === "desc"),
      cursor: q.after == null ? null : String(q.after),
      limit: q.limit,
      filters: filters,
    });
    // ⚠️ 列名是 Rust 侧的 snake_case（`Page { has_more, next_cursor }`），
    // 这里**不能**写成 hasMore —— 写错了不会报错，只会让"加载更多"永远不触发：
    // 超过一页的表就再也翻不动，而界面看起来一切正常（P1，见 BUG_HUNT-2026-09-18.md）。
    return {
      rows: toRowObject(page),
      hasMore: !!page.has_more,
      nextCursor: page.next_cursor == null ? null : page.next_cursor,
    };
  }

  async function commitCell(a) {
    await call("schema.updateCell", {
      table: state.current,
      rowid: a.rowId,
      column: a.column,
      // 金额列交回的是"元"字符串，money_parse 会换算回分；NULL 传 null
      value: a.value === undefined ? null : a.value,
    });
  }

  async function deleteRows(ids) {
    const r = await call("schema.deleteRows", {
      table: state.current,
      rowids: (ids || []).map((x) => Number(x)),
    });
    return r && r.deleted != null ? r.deleted : undefined;
  }

  async function insertRow(values) {
    const cols = Object.keys(values);
    const row = cols.map((k) => {
      const v = values[k];
      if (typeof v === "boolean") return v ? "1" : "0";
      return String(v == null ? "" : v);
    });
    await call("schema.insertRows", {
      table: state.current,
      columns: cols,
      rows: [row],
    });
    // 网格支持把"新建的那一行"接回表尾，但 insert_rows 只回报条数；
    // 与其编一个假行，不如整体刷新第一页 —— 表通常不大，代价可接受。
    if (state.grid) state.grid.reload();
    return null;
  }

  // ============================================================
  // SQL 执行入口（2026-09-20 移除，见 ADR-0021）
  // ------------------------------------------------------------
  // 原来这里的 gatedRunQuery / interruptQuery 调的是 schema.runQuery。
  // 去掉 SQL 之后这条 IPC 连同整个查询编辑器一起删了 —— 查询这件事
  // 改由「命名视图」承担（筛选 / 排序 / 分组 / 隐藏列），没有语句、没有输入框。
  // ============================================================
  // ============================================================
  // 页签
  // ============================================================
  function showTab(tab) {
    state.tab = tab;
    tabsEl.querySelectorAll(".db-tab").forEach((b) => {
      b.setAttribute("aria-selected", b.dataset.tab === tab ? "true" : "false");
    });
    paneGrid.hidden = tab !== "data";
    // SQL 面板已按 ADR-0020 撤下：这里不再有第二个页签要切。
    // ensureSql / gatedRunQuery 的函数体刻意保留（JS 无死代码告警，
    // 留着是零成本的可逆点 —— 恢复 SQL 只需把 ui/legacy/sql.js 移回并重新挂载）。
  }

  // ============================================================
  // 新建表格 / 删除表 对话框
  // ============================================================
  // ============================================================
  // 列类型清单
  // ============================================================
  //
  // ⚠️ **唯一来源是 Rust**（`schema.columnTypes` ← `schema::ColType`）。
  //
  // 以前这份清单是抄在本文件里的数组。抄一份的代价已经付过：2026-09-19 发现
  // serde 把 `DateTime` 序列化成 `date_time`，而这里传的是 `datetime` ——
  // 「日期时间」这个类型**选了就报错**，而且因为没人拿它建过表，一直没暴露。
  //
  // 现在改成启动时拉一次、缓存在这里。拉不到就不给选（而不是退回一份本地副本）：
  // 退回副本等于又把第二个来源请回来，下次还会分叉。
  let TYPES = null; // [["text","文本"], ...]

  async function ensureTypes() {
    if (TYPES) return TYPES;
    const r = await call("schema.columnTypes");
    TYPES = (r && r.types ? r.types : []).map((t) => [t.name, t.label]);
    if (!TYPES.length) {
      throw new Error("读不到列类型清单（schema.columnTypes 返回空）");
    }
    return TYPES;
  }

  /** `DEFAULT` 里可以原样写的关键字（与 schema.rs 的 normalize_default 白名单一致） */
  const KEYWORD_DEFAULTS = [
    "NULL",
    "TRUE",
    "FALSE",
    "CURRENT_DATE",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
  ];

  /**
   * 用户填的「默认值」→ `DEFAULT` 子句允许的 SQL 片段。
   *
   * 为什么必须翻译这一层：白名单（schema.rs 的 `normalize_default`）只收数字
   * 字面量、成对引号的字符串、以及上面那几个关键字。让用户自己去写 `'未结清'`
   * 那种引号是不可接受的 —— 他只会写「未结清」。这层翻译就是把"人话"变成
   * "合法的 DDL 片段"，而不是把规则抛给用户记。
   *
   * 金额列**不在这里做算术**：原样包成字符串交回去，由 Rust 的 `money_parse`
   * 按「元」换算成分（D-034：金额换算全局只有那一份实现）。
   */
  function defaultLiteral(raw, ty) {
    const v = String(raw == null ? "" : raw).trim();
    if (!v) return { ok: true, value: null };
    const kw = v.toUpperCase();
    if (KEYWORD_DEFAULTS.indexOf(kw) >= 0) return { ok: true, value: kw };
    if (ty === "integer" || ty === "real" || ty === "boolean") {
      if (!/^-?\d+(\.\d+)?$/.test(v)) {
        return { ok: false, why: "「" + v + "」不是数字，数字列的默认值只能是数字" };
      }
      return { ok: true, value: v };
    }
    // 文本 / 金额 / 日期 / JSON：包成字符串字面量，内部的单引号翻倍
    return { ok: true, value: "'" + v.replace(/'/g, "''") + "'" };
  }

  // ---------- 默认模板 ----------
  // 为什么要有模板：调研里最硬的一条是"非程序员可用性是生死线"，而空表对新手等于
  // 无从下手。所以提供几张**能直接开始记东西**的表（发起人要求：数据库要有默认模板）。
  // 约定：列名一律中文；金额用 money（按分存）；日期用 date。
  // 改这里 = 改用户第一次看到的东西，改完必须同步 `help.js` 的教程，别让教程说谎。
  const TEMPLATES = {
    客户台账: [
      { name: "客户名称", ty: "text", nn: true },
      { name: "联系人", ty: "text" },
      { name: "电话", ty: "text" },
      { name: "应收金额", ty: "money" },
      { name: "最近跟进", ty: "date" },
      { name: "已结清", ty: "boolean" },
    ],
    进出货: [
      { name: "品名", ty: "text", nn: true },
      { name: "规格", ty: "text" },
      { name: "数量", ty: "integer" },
      { name: "单价", ty: "money" },
      { name: "进出", ty: "text" },
      { name: "日期", ty: "date" },
      { name: "经办人", ty: "text" },
    ],
    费用报销: [
      { name: "事由", ty: "text", nn: true },
      { name: "类别", ty: "text" },
      { name: "金额", ty: "money" },
      { name: "发生日期", ty: "date" },
      { name: "发票号", ty: "text" },
      { name: "已报销", ty: "boolean" },
    ],
    库存盘点: [
      { name: "物料", ty: "text", nn: true },
      { name: "规格", ty: "text" },
      { name: "单位", ty: "text" },
      { name: "账面数", ty: "integer" },
      { name: "实盘数", ty: "integer" },
      { name: "盘点日期", ty: "date" },
      { name: "备注", ty: "text" },
    ],
  };

  function buildDialog(id, title, hint) {
    let dlg = document.getElementById(id);
    if (dlg) return dlg;
    dlg = el("dialog", { class: "db-dialog", id: id });
    const h = el("h3", null, title);
    const p = el("p", { class: "hint" }, hint);
    dlg.append(h, p);
    document.body.appendChild(dlg);
    // 点 backdrop 关闭：dialog 不提供这个事件，靠点击坐标判断
    dlg.addEventListener("click", (ev) => {
      if (ev.target === dlg) dlg.close("cancel");
    });
    return dlg;
  }

  async function openNewTableDialog() {
    // 类型清单来自 Rust（唯一来源）。拉不到就明说，不给一个"半能用"的向导。
    try {
      await ensureTypes();
    } catch (e) {
      toast("打不开新建表格向导：" + errText(e), "error");
      return;
    }
    const dlg = buildDialog(
      "db-dialog-new",
      "新建表格",
      "列名可以用中文、字母、数字、下划线。金额按「分」存储：写入 12.34 存 1234，" +
        "显示时自动换算回来。主键列必须是整数或文本。"
    );
    dlg.textContent = "";
    dlg.append(el("h3", null, "新建表格"));
    dlg.append(
      el("p", { class: "hint" },
        "列名可以用中文、字母、数字、下划线，不能叫 rowid。金额按「分」存储：" +
          "写入 12.34 存 1234，显示时自动换算回来。二进制列不能在表格里直接编辑。" +
          "「默认值」是新增行时自动填的内容，留空表示不填。")
    );

    const form = el("form", { novalidate: "novalidate" });

    // 模板放在最上面：新手第一眼看到的是"我可以先套一个"，而不是一排空白输入框
    const tpl = el("select", { class: "db-tpl-select" });
    tpl.appendChild(el("option", { value: "" }, "从模板开始（可选）"));
    Object.keys(TEMPLATES).forEach((k) => tpl.appendChild(el("option", { value: k }, k)));
    const tplHint = el("p", { class: "hint db-tpl-hint" }, "不知道从哪下手？先套一个模板，列和表名都能改。");
    form.appendChild(tpl);
    form.appendChild(tplHint);

    const nameInput = el("input", { type: "text", placeholder: "表名，例如：费用明细" });
    const nameField = el("div");
    nameField.appendChild(nameInput);
    form.appendChild(nameField);

    const colsBox = el("div", { class: "db-cols" });
    form.appendChild(colsBox);

    function colRow(col) {
      const row = el("div", { class: "db-col-row" });
      const name = el("input", { type: "text", placeholder: "列名" });
      if (col) name.value = col.name;
      const type = el("select");
      const wantTy = col ? col.ty : "text";
      TYPES.forEach(([v, label]) => {
        const opt = el("option", { value: v }, label);
        if (v === wantTy) opt.setAttribute("selected", "selected");
        type.appendChild(opt);
      });
      type.title = "一般不用改：默认按文本存，什么内容都能装";
      const pk = el("label");
      const pkBox = el("input", { type: "checkbox" });
      pk.title = "一般不用动。只有需要精确区分每一行时才勾（不勾也能正常用）";
      pk.append(pkBox, document.createTextNode("主键"));
      const nn = el("label");
      const nnBox = el("input", { type: "checkbox" });
      nn.append(nnBox, document.createTextNode("必填"));
      if (col) {
        pkBox.checked = !!col.pk;
        nnBox.checked = !!col.nn;
      }
      const rm = el("button", { class: "rm", type: "button", title: "移除这一列" }, "×");
      rm.addEventListener("click", () => {
        if (colsBox.children.length > 1) row.remove();
      });
      row.append(name, type, pk, nn, rm);
      return row;
    }

    function applyTemplate(tname) {
      const cols = TEMPLATES[tname];
      if (!cols) return;
      // 表名没填才自动填 —— 用户自己起的名字比模板名重要
      if (!nameInput.value.trim()) nameInput.value = tname;
      colsBox.textContent = "";
      cols.forEach((c) => colsBox.appendChild(colRow(c)));
      tplHint.textContent =
        "已套用「" + tname + "」，" + cols.length + " 个列 —— 表名和列都能改，用不上的删掉就行。";
    }
    tpl.addEventListener("change", () => applyTemplate(tpl.value));

    // Excel 式：预填三个列名，用户想改就在这儿改，不想改直接点「创建」。
    // 为什么要预填：空输入框会让第一次用的人停在"我得起三个名字"这一步 ——
    // 而 Excel 的心智是"先有表，名字慢慢改"。
    colsBox.appendChild(colRow({ name: "列1" }));
    colsBox.appendChild(colRow({ name: "列2" }));
    colsBox.appendChild(colRow({ name: "列3" }));

    const addCol = el("button", { class: "btn btn-ghost", type: "button" }, "添加列");
    addCol.addEventListener("click", () => colsBox.appendChild(colRow()));
    form.appendChild(addCol);

    const actions = el("div", { class: "db-dialog-actions" });
    const cancel = el("button", { class: "btn btn-ghost", type: "button" }, "取消");
    const submit = el("button", { class: "btn btn-primary", type: "submit" }, "创建");
    actions.append(cancel, submit);
    form.appendChild(actions);
    dlg.appendChild(form);

    cancel.addEventListener("click", () => dlg.close("cancel"));
    form.addEventListener("submit", async (ev) => {
      ev.preventDefault();
      const tname = nameInput.value.trim();
      if (!tname) {
        toast("先给表起个名字", "error");
        nameInput.focus();
        return;
      }
      const columns = [];
      for (const row of colsBox.children) {
        const inputs = row.querySelectorAll("input[type=text], select, input[type=checkbox]");
        const cname = inputs[0].value.trim();
        const type = inputs[1].value;
        const pk = inputs[2].checked;
        const nn = inputs[3].checked;
        if (!cname) continue; // 空行视为没填，跳过
        // inputs[4] = 默认值输入框（顺序由 colRow 里 row.append 的顺序决定）
        const def = defaultLiteral(inputs[4] ? inputs[4].value : "", type);
        if (!def.ok) {
          toast(def.why, "error");
          if (inputs[4]) inputs[4].focus();
          return;
        }
        columns.push({
          name: cname,
          ty: type,
          not_null: nn || pk,
          default: def.value,
          primary_key: pk,
          comment: null,
        });
      }
      if (!columns.length) {
        toast("至少要有一个列", "error");
        return;
      }
      submit.disabled = true;
      submit.textContent = "创建中…";
      try {
        await call("schema.createTable", { spec: { name: tname, comment: null, columns: columns } });
        toast("已新建表格「" + tname + "」");
        dlg.close();
        await refreshTables();
        await openTable(tname);
      } catch (e) {
        toast("新建表格失败：" + errText(e), "error");
        submit.disabled = false;
        submit.textContent = "创建";
      }
    });

    dlg.showModal();
    nameInput.focus();
  }

  function askDropTable(name) {
    const dlg = buildDialog(
      "db-dialog-drop",
      "删除表",
      "DROP 会连表带数据一起删掉，删完无法恢复。要确认的话，请在下面逐字输入表名。"
    );
    dlg.textContent = "";
    dlg.append(el("h3", null, "删除表「" + name + "」"));
    dlg.append(
      el("p", { class: "hint" },
        "DROP 会连表带数据一起删掉，删完无法恢复。要确认的话，请在下面逐字输入这张表的名字。")
    );
    const form = el("form", { novalidate: "novalidate" });
    const input = el("input", { type: "text", placeholder: "输入表名确认" });
    form.appendChild(input);
    const actions = el("div", { class: "db-dialog-actions" });
    const cancel = el("button", { class: "btn btn-ghost", type: "button" }, "取消");
    const submit = el("button", { class: "btn btn-primary", type: "submit", disabled: "disabled" }, "删除");
    submit.style.background = "var(--danger)";
    submit.style.borderColor = "var(--danger)";
    actions.append(cancel, submit);
    form.appendChild(actions);
    dlg.appendChild(form);

    input.addEventListener("input", () => {
      if (input.value === name) submit.removeAttribute("disabled");
      else submit.setAttribute("disabled", "disabled");
    });
    cancel.addEventListener("click", () => dlg.close("cancel"));
    form.addEventListener("submit", async (ev) => {
      ev.preventDefault();
      if (input.value !== name) return;
      try {
        await call("schema.dropTable", { name: name, confirmName: input.value });
        toast("已删除表「" + name + "」");
        dlg.close();
        if (state.current === name) {
          if (state.grid) {
            state.grid.destroy();
            state.grid = null;
          }
          paneGrid.textContent = "";
          state.current = null;
          currentName.textContent = "";
        }
        await refreshTables();
      } catch (e) {
        toast("删除失败：" + errText(e), "error");
      }
    });

    dlg.showModal();
    input.focus();
  }

  // ============================================================
  // 从 Excel / CSV 导入新建表格（v0.3.0 的第一优先级）
  // ============================================================
  //
  // 为什么值得这么多代码：发起人把这条明确成"不是替代 Excel，是要能导入 Excel"。
  // 用户手上那堆台账、名单、流水都在 Excel 里，能不能把它们搬进来决定了这个
  // 软件对他有没有用。
  //
  // ## 这个向导的三个设计要点
  //
  // 1. **表头行让用户指**。中文台账的上面经常压着标题行、导出日期行。程序猜，
  //    猜错了他不一定看得出来（猜错的行会变成列名）。所以把原文铺出来、
  //    让他点一行 —— 点错了他自己能看见并改回来。
  // 2. **每一列都说清"为什么判成这个类型"**。类型判错是**静默毁数据**：
  //    前导零的工号存成整数就永久丢了零，而事后从库里已经看不出来。
  //    所以置信度低的那几列要显眼，理由要写出来让人能评判。
  // 3. **进度是真的**。写库走 `import.begin / chunk / finish` 三段，
  //    前端驱动循环 —— 每批之间界面能重绘，进度条真地动，也能中途取消。
  //    一口气写完会让界面连进度条一起冻住，那比没有进度条更糟。

  /** 表头行之前的预览行数上限：够看清结构就行，不必把整张表铺出来 */
  const IMP_PREVIEW_MAX = 30;

  /**
   * 打开导入向导。
   *
   * ## ⚠️ 一条重要的实现约束：原生文件对话框**会阻塞主线程**
   *
   * `import.pickAndPlan` 在 Rust 侧用 `rfd` 弹原生对话框，而 `pick_file()` 是
   * **模态且同步**的 —— 它没被处理掉之前，事件循环不再处理任何东西。对用户没有
   * 影响（他要的就是"选个文件"），但对**自动化测试**是致命的：界面烟测点开向导
   * 之后会看到原生对话框挂在那里，脚本永远等不到下一步（实测卡死在这里）。
   *
   * 所以留一个显式的接缝 `autoPick`：默认 `true`（打开就弹对话框，省用户一次点击），
   * 传 `false` 则只打开向导本体、等用户点「选择文件…」。烟测走 `false` 这条，
   * 于是向导的界面与控件能被真实验证，而不必去驱动一个驱动不了的原生窗口。
   *
   * @param {{autoPick?: boolean}} [opts]
   */
  function openImportDialog(opts) {
    const autoPick = !(opts && opts.autoPick === false);
    const dlg = buildDialog(
      "db-dialog-import",
      "从 Excel / CSV 导入",
      "选一个表格文件，程序会读它的结构、给每列猜一个类型，你确认后再落库。"
    );
    dlg.textContent = "";
    const h = el("h3", null, "从 Excel / CSV 导入");
    const lead = el(
      "p",
      { class: "hint" },
      "选一个表格文件 → 确认表头在哪一行与每列的类型 → 落库成一张新表。" +
        "原文件不会被改动，已有同名表也不会被覆盖。"
    );
    dlg.append(h, lead);

    // 进度条容器（一开始就占好位置，避免它出现时把下面的按钮顶下去）
    const progBox = el("div", { class: "db-imp-prog" });
    dlg.appendChild(progBox);

    const body = el("div", { class: "db-imp" });
    dlg.appendChild(body);

    const actions = el("div", { class: "db-dialog-actions" });
    const btnCancel = el("button", { class: "btn btn-ghost", type: "button" }, "取消");
    const btnPick = el("button", { class: "btn", type: "button" }, "选择文件…");
    const btnGo = el("button", { class: "btn btn-primary", type: "button", hidden: "" }, "导入");
    actions.append(btnCancel, btnPick, btnGo);
    dlg.appendChild(actions);

    /** 当前这一轮的状态。整段流程共用，别塞进闭包外的变量。 */
    const st = {
      planId: null,
      plan: null,
      sessionId: null,
      busy: false,
    };

    const prog = window.DeskBaseUI.progress({
      title: "准备中…",
      hint: "正在读取文件结构",
      indeterminate: true,
    });
    progBox.appendChild(prog.el);
    progBox.hidden = true;

    /** 把 "客户台账.xlsx" → "客户台账"（表名默认值） */
    function suggestTableName(fileName) {
      let s = String(fileName || "").replace(/\.[^.]+$/, "").trim();
      s = s.replace(/[^\u4e00-\u9fa5A-Za-z0-9_]/g, "_");
      if (!s) s = "导入表";
      if (/^\d/.test(s)) s = "表_" + s;
      return s.slice(0, 40);
    }

    function setBusy(on, note) {
      st.busy = on;
      btnPick.disabled = on;
      btnGo.disabled = on;
      btnCancel.textContent = on ? "中断导入" : "取消";
      progBox.hidden = !on;
      if (on && note) prog.set(null, note);
    }

    // ---------- 渲染整张向导（拿到计划之后调） ----------
    function render(plan) {
      st.plan = plan;
      body.textContent = "";

      // ---- 文件信息 + 工作表 ----
      const info = el("div", { class: "db-imp-file" });
      info.append(el("span", { class: "db-imp-name" }, plan.file_name));
      info.append(
        el(
          "span",
          { class: "db-imp-meta" },
          "共 " + plan.data_rows.toLocaleString("zh-CN") + " 行数据 · " + plan.total_cols + " 列"
        )
      );
      body.appendChild(info);

      if (plan.sheets && plan.sheets.length > 1) {
        const sw = el("div", { class: "db-imp-field" });
        sw.append(el("label", null, "工作表"));
        const sel = el("select", { class: "select" });
        plan.sheets.forEach((n, i) => {
          const o = el("option", { value: String(i) }, n);
          if (i === plan.sheet_index) o.setAttribute("selected", "selected");
          sel.appendChild(o);
        });
        sel.addEventListener("change", async () => {
          await replan({ sheetIndex: Number(sel.value) });
        });
        sw.append(sel);
        body.appendChild(sw);
      }

      // ---- 警告：这些是最该先看到的东西 ----
      if (plan.warnings && plan.warnings.length) {
        const wbox = el("div", { class: "db-imp-warn" });
        wbox.append(el("div", { class: "db-imp-warn-h" }, "导入前请注意"));
        plan.warnings.forEach((w) => {
          const it = el("div", { class: "db-imp-warn-i", "data-kind": w.kind });
          it.append(el("div", { class: "t" }, "· " + w.advice));
          if (w.samples && w.samples.length) {
            it.append(el("div", { class: "s mono" }, "例如：" + w.samples.join("、")));
          }
          wbox.appendChild(it);
        });
        body.appendChild(wbox);
      }

      // ---- 表名 ----
      const nf = el("div", { class: "db-imp-field" });
      nf.append(el("label", null, "导入成哪张表"));
      const nameInput = el("input", {
        type: "text",
        placeholder: "表名，例如：客户台账",
      });
      nameInput.value = suggestTableName(plan.file_name);
      nf.append(nameInput);
      const nameHint = el("div", { class: "hint" }, "");
      nf.append(nameHint);
      body.appendChild(nf);
      st.nameInput = nameInput;
      st.nameHint = nameHint;

      // ---- 预览 + 表头行选择 ----
      const ph = el("div", { class: "db-imp-sec-h" });
      ph.append(el("span", null, "表头在第几行？"));
      const phHint = el("span", { class: "hint" }, "点一行把它设为表头；它上面的行不会被导入");
      ph.appendChild(phHint);
      body.appendChild(ph);

      const wrap = el("div", { class: "db-imp-preview" });
      const table = el("table");
      const rows = (plan.preview || []).slice(0, IMP_PREVIEW_MAX);
      rows.forEach((r, idx) => {
        const tr = el("tr");
        if (idx === plan.header_row) tr.dataset.head = "true";
        if (idx < plan.header_row) tr.dataset.above = "true";
        const no = el("td", { class: "no mono" }, String(idx + 1));
        tr.appendChild(no);
        const narrow = r.slice(0, 8);
        narrow.forEach((c) => {
          const td = el("td", null, c === "" ? "" : c);
          if (c === "") td.dataset.empty = "true";
          tr.appendChild(td);
        });
        if (r.length > narrow.length) {
          tr.appendChild(el("td", { class: "more" }, "…+" + (r.length - narrow.length)));
        }
        tr.addEventListener("click", async () => {
          if (st.busy || st.plan.header_row === idx) return;
          await replan({ headerRow: idx });
        });
        table.appendChild(tr);
      });
      wrap.appendChild(table);
      body.appendChild(wrap);
      if (plan.preview_truncated) {
        body.append(
          el(
            "div",
            { class: "hint" },
            "预览只显示前 " + rows.length + " 行；导入会把整张表读进来。"
          )
        );
      }
      if (plan.skipped_above > 0) {
        body.append(
          el(
            "div",
            { class: "db-imp-skip" },
            "表头上面那 " + plan.skipped_above + " 行（已标灰）不会被导入。"
          )
        );
      }
      body.append(el("div", { class: "hint" }, plan.header_reason));

      // ---- 列：名字 / 类型 / 保留 ----
      const ch = el("div", { class: "db-imp-sec-h" });
      ch.append(el("span", null, "列与类型"));
      ch.append(el("span", { class: "hint" }, "改错了会毁数据 —— 拿不准就看样例"));
      body.appendChild(ch);

      const colsBox = el("div", { class: "db-imp-cols" });
      plan.columns.forEach((c) => colsBox.appendChild(impColRow(c)));
      body.appendChild(colsBox);

      btnGo.hidden = false;
      btnGo.textContent = "导入 " + plan.data_rows.toLocaleString("zh-CN") + " 行";
      nameInput.focus();
      nameInput.select();
    }

    /** 一列一行：保留勾选 + 列名 + 类型 + 判断依据 + 样例 */
    function impColRow(c) {
      const row = el("div", { class: "db-imp-col" });
      row.dataset.src = String(c.source_index);

      const keep = el("input", { type: "checkbox", class: "keep" });
      keep.checked = true;
      keep.title = "取消勾选则这一列不导入";
      row.appendChild(keep);

      const name = el("input", { type: "text", class: "nm" });
      name.value = c.name;
      row.appendChild(name);

      const type = el("select", { class: "ty" });
      (TYPES || []).forEach(([v, label]) => {
        const o = el("option", { value: v }, label);
        if (v === c.ty) o.setAttribute("selected", "selected");
        type.appendChild(o);
      });
      row.appendChild(type);

      // 置信度徽标：低置信度要显眼 —— 那正是最可能毁数据的地方
      const badge = el(
        "span",
        { class: "db-imp-badge", "data-c": c.confidence },
        "把握" + c.confidence
      );
      badge.title = c.reason || "";
      row.appendChild(badge);

      const meta = el("div", { class: "db-imp-col-meta" });
      meta.append(el("span", { class: "why" }, c.reason || ""));
      if (c.samples && c.samples.length) {
        meta.append(el("span", { class: "samples mono" }, "样例：" + c.samples.join(" · ")));
      }
      row.appendChild(meta);

      const nn = el("label", { class: "nn" });
      const nnBox = el("input", { type: "checkbox" });
      nnBox.checked = !!c.not_null;
      nn.append(nnBox, document.createTextNode("必填"));
      row.appendChild(nn);

      // 取消勾选时把整行淡化 —— 让"哪些会被导入"一眼可见
      keep.addEventListener("change", () => {
        row.dataset.off = keep.checked ? "false" : "true";
      });
      return row;
    }

    /** 收集用户确认后的列 */
    function collectColumns() {
      const out = [];
      for (const row of body.querySelectorAll(".db-imp-col")) {
        const keep = row.querySelector(".keep");
        if (!keep.checked) continue;
        out.push({
          source_index: Number(row.dataset.src),
          name: row.querySelector(".nm").value.trim(),
          ty: row.querySelector(".ty").value,
          not_null: row.querySelector(".nn input").checked,
        });
      }
      return out;
    }

    // ---------- 交互 ----------

    /** 用户改了工作表或表头行 → 重新要计划（只读前若干行，不读整表） */
    async function replan(patch) {
      if (!st.planId || st.busy) return;
      const sheetIndex = patch.sheetIndex != null ? patch.sheetIndex : st.plan.sheet_index;
      setBusy(true, "正在重新识别…");
      prog.set(null, "正在重新识别表头与类型");
      try {
        const r = await call("import.preview", { planId: st.planId, sheetIndex: sheetIndex });
        let plan = r.plan;
        if (patch.headerRow != null) {
          // 表头行是纯前端的重新推导：不必再读一次文件
          plan = Object.assign({}, plan, {
            headerRow: patch.headerRow,
            skippedAbove: patch.headerRow,
            columns: deriveColumnsLocally(plan, patch.headerRow),
          });
        }
        render(plan);
      } catch (e) {
        prog.fail(errText(e));
        toast("重新识别失败：" + errText(e), "error");
      } finally {
        setBusy(false);
        progBox.hidden = true;
      }
    }

    /**
     * 表头行改了之后的列建议。
     *
     * 为什么在前端算而不是再跑一次 IPC：改表头行只影响"哪一行当表头"，
     * 而预览行已经在手上了 —— 为了一个纯函数再往返一次 IPC，用户会看到
     * 一次不必要的等待。推导逻辑本身在 Rust 侧（`import_plan::guess_type`），
     * 这里只是**复用同一批预览行**做同样的判断吗？不是 —— 这里只做
     * "把表头文字当列名"这一步，类型仍按原来的列位置沿用。
     *
     * 之所以敢这么做：真正的类型判断在**落库前那次** `import.begin` 里由
     * Rust 重新做（它读的是整表），界面上的建议只是给人看的初值。
     */
    function deriveColumnsLocally(plan, headerRow) {
      const head = (plan.preview || [])[headerRow] || [];
      return plan.columns.map((c, i) => {
        const raw = head[i];
        return Object.assign({}, c, {
          original: raw == null ? "" : String(raw),
          name: raw ? String(raw).trim() || c.name : c.name,
        });
      });
    }

    /** 选文件：走原生对话框（路径只在 Rust 侧流转，这里拿不到也不需要） */
    async function pick() {
      if (st.busy) return;
      setBusy(true, "正在读取文件…");
      prog.set(null, "正在读取文件结构与样例数据");
      try {
        const r = await call("import.pickAndPlan");
        if (r.cancelled) {
          progBox.hidden = true;
          setBusy(false);
          return;
        }
        st.planId = r.planId;
        progBox.hidden = true;
        setBusy(false);
        render(r.plan);
      } catch (e) {
        setBusy(false);
        prog.fail(errText(e));
        toast("读不了这个文件：" + errText(e), "error");
      }
    }

    /** 真正导入：begin → 循环 chunk → finish。每批之间界面能重绘，进度是真的。 */
    async function run() {
      if (st.busy || !st.planId) return;
      const table = st.nameInput.value.trim();
      if (!table) {
        toast("先给这张表起个名字", "error");
        st.nameInput.focus();
        return;
      }
      const columns = collectColumns();
      if (!columns.length) {
        toast("至少要保留一列", "error");
        return;
      }
      const badName = columns.find((c) => !c.name);
      if (badName) {
        toast("有列名是空的，先填上", "error");
        return;
      }

      setBusy(true, "正在准备…");
      prog.set(null, "正在新建表格并读取数据（这一步可能要几秒）");
      const t0 = Date.now();
      try {
        const beg = await call("import.begin", {
          planId: st.planId,
          table: table,
          sheetIndex: st.plan.sheet_index,
          headerRow: st.plan.header_row,
          columns: columns,
        });
        st.sessionId = beg.sessionId;
        const total = beg.begun.total || 0;

        if (total === 0) {
          // 一行数据都没有：这不是"成功"，要说清楚
          await call("import.finish", {
            sessionId: st.sessionId,
            planId: st.planId,
            skippedAbove: beg.begun.skipped_above,
          });
          st.sessionId = null;
          setBusy(false);
          prog.fail("这张表里没有可导入的数据行（表头之外是空的）。表已经建好，但没有任何记录。");
          toast("没有可导入的数据行", "error");
          await refreshTables();
          return;
        }

        // 循环写批：批大小取 2000 —— 太小则 IPC 往返成为瓶颈，
        // 太大则一次占用主线程过久、进度条会一跳一跳的。
        let written = 0;
        for (;;) {
          const c = await call("import.chunk", { sessionId: st.sessionId, batch: 2000 });
          written = c.written;
          const frac = total > 0 ? written / total : 1;
          prog.set(
            frac,
            "已写入 " + written.toLocaleString("zh-CN") + " / " +
              total.toLocaleString("zh-CN") + " 行"
          );
          if (c.done) break;
        }

        const out = await call("import.finish", {
          sessionId: st.sessionId,
          planId: st.planId,
          skippedAbove: st.plan.header_row,
        });
        st.sessionId = null;

        const secs = ((Date.now() - t0) / 1000).toFixed(1);
        prog.done(
          "已导入 " + out.inserted.toLocaleString("zh-CN") + " 行 → 表「" + out.table +
            "」（用时 " + secs + " 秒）"
        );

        // 成功之后把向导换成"结果视图"：不再让用户对着一个还能再点一次的按钮。
        // 二次点击会撞上"表已存在"，那是个没必要的失败。
        setBusy(false);
        btnPick.hidden = true;
        btnCancel.textContent = "关闭";
        btnGo.hidden = false;
        btnGo.textContent = "打开这张表";
        btnGo.disabled = false;
        btnGo.onclick = async () => {
          dlg.close();
          await refreshTables();
          await openTable(out.table);
        };
        const extra = [];
        if (out.skipped_above > 0) {
          extra.push("表头上面 " + out.skipped_above + " 行未导入");
        }
        if (out.skipped_empty > 0) {
          extra.push("跳过 " + out.skipped_empty + " 行空行");
        }
        if (extra.length) {
          progBox.append(el("div", { class: "hint" }, extra.join(" · ")));
        }
        // 顺手刷新左栏，让新表立刻可见（用户点"打开"之前就能看到它）
        await refreshTables();
      } catch (e) {
        const msg = errText(e);
        prog.fail(msg);
        toast("导入失败：" + msg, "error");
        st.sessionId = null;
        setBusy(false);
        // 失败之后留在向导里，用户改完能直接再试 —— 而不是从头选一遍文件
      }
    }

    // ---------- 绑定 ----------
    btnPick.addEventListener("click", pick);
    btnGo.addEventListener("click", () => {
      if (btnGo.textContent === "打开这张表") return; // 已被结果态接管
      run();
    });
    btnCancel.addEventListener("click", async () => {
      if (st.busy) {
        // 中断：先把表删掉，再退出。**不留半张表** ——
        // 留一张写了一半的表，用户下次导入会撞上"表已存在"，而他又看不出那张表哪来的。
        const go = await confirmBox(
          "中断这次导入？",
          "已经写入的 " + (st.sessionId ? "部分" : "0") + " 行会被丢弃，" +
            "中途建出来的那张表也会被删掉。原文件不受影响。"
        );
        if (!go) return;
        try {
          if (st.sessionId) {
            await call("import.abort", { sessionId: st.sessionId });
            st.sessionId = null;
          }
        } catch (_) {
          // 中断失败不该把用户扣在向导里 —— 后面 refreshTables 会反映真实状态
        }
        prog.fail("已中断，库里没有留下数据。");
        setBusy(false);
        await refreshTables();
        return;
      }
      dlg.close();
    });
    dlg.addEventListener("close", () => {
      // 关窗前如果还有没收拾干净的会话，尽力清一下（不阻塞关闭）
      if (st.sessionId) {
        call("import.abort", { sessionId: st.sessionId }).catch(() => {});
      }
    });
    dlg.addEventListener("click", (ev) => {
      if (ev.target === dlg && !st.busy) dlg.close("cancel");
    });

    dlg.showModal();
    // 打开就直奔选文件：这一步没有别的可做，不必让用户再点一次。
    // `autoPick === false` 时留给用户/测试自己点（见上面的约束说明）。
    if (autoPick) pick();
  }

  // ============================================================
  // 事件绑定与生命周期
  // ============================================================
  document.getElementById("btn-db-new-table").addEventListener("click", openNewTableDialog);
  const btnImport = document.getElementById("btn-db-import");
  if (btnImport) btnImport.addEventListener("click", openImportDialog);
  document.getElementById("btn-db-refresh").addEventListener("click", () => refreshTables());
  // 备份按钮：进「数据库」页就能点，不需要先打开某张表
  document.getElementById("btn-db-backup").addEventListener("click", () => { openBackupDialog(); });
  // 表结构按钮：需要一个当前表，没有就由对话框自己提示
  document.getElementById("btn-db-schema").addEventListener("click", () => { openSchemaDialog(); });
  document.getElementById("btn-db-relations").addEventListener("click", () => { openRelationsDialog(); });
  document.getElementById("btn-db-views").addEventListener("click", () => { openViewsDialog(); });
  document.getElementById("btn-db-history").addEventListener("click", () => { openHistoryDialog(); });
  tabsEl.addEventListener("click", (ev) => {
    const b = ev.target.closest(".db-tab");
    if (b) showTab(b.dataset.tab);
  });

  // 窄屏抽屉开关（与笔记页同一档位，见 database.css）
  const sideToggle = document.getElementById("btn-db-side");
  if (sideToggle) {
    sideToggle.addEventListener("click", () => {
      const open = sideEl.dataset.open === "true";
      sideEl.dataset.open = open ? "false" : "true";
    });
    // 点到主区就收抽屉
    paneGrid.addEventListener("click", () => {
      if (sideEl.dataset.open === "true") sideEl.dataset.open = "false";
    });
  }

  /** app.js 的 showView 会调这里：第一次进数据库页时才拉表列表（懒加载） */
  function onShow(view) {
    if (view !== "database") return;
    if (!state.listed) refreshTables();
  }

  // `openImportDialog` 也暴露出去：烟测需要它（带 autoPick:false 才不会被
  // 原生文件对话框卡住，见那个函数的说明）。对外它是"程序化打开导入向导"的入口，
  // 命令面板将来加「导入 Excel」也可以用同一条路。
  // ============================================================
  // 备份（v0.3.0 · 不丢数据）
  // ============================================================
  // 为什么值得一个对话框：备份是"用户主动保命"的动作，必须让他看见三件事 ——
  // 存到哪、多大、有没有校验过。只弹一个"成功"等于没说。
  function fmtBytes(n) {
    if (n >= 1024 * 1024 * 1024) return (n / 1024 / 1024 / 1024).toFixed(2) + " GB";
    if (n >= 1024 * 1024) return (n / 1024 / 1024).toFixed(2) + " MB";
    if (n >= 1024) return (n / 1024).toFixed(1) + " KB";
    return n + " B";
  }

  function fmtStamp(ms) {
    if (!ms) return "—";
    const d = new Date(ms);
    const p = (n) => String(n).padStart(2, "0");
    return d.getFullYear() + "-" + p(d.getMonth() + 1) + "-" + p(d.getDate()) +
      " " + p(d.getHours()) + ":" + p(d.getMinutes());
  }

  /**
   * 建节点，支持多个子节点。
   *
   * 为什么单独有一个：文件里的 `el(tag, attrs, text)` 第三个参数是**文本**
   * （`textContent`），塞 DOM 元素进去会变成 "[object HTMLInputElement]" ——
   * 界面上就是"控件不见了"。对话框里一行要放好几个控件，所以用这个。
   */
  function node(tag, attrs) {
    const n = document.createElement(tag);
    if (attrs) {
      for (const k in attrs) {
        if (attrs[k] == null) continue;
        n.setAttribute(k, attrs[k]);
      }
    }
    for (let i = 2; i < arguments.length; i++) {
      const c = arguments[i];
      if (c == null) continue;
      if (typeof c === "string" || typeof c === "number") n.append(String(c));
      else n.append(c);
    }
    return n;
  }

  // ============================================================
  // 关系与同步（ADR-0022）
  //
  // 去掉 SQL 之后，表与表之间的"连着"不再藏在外键和触发器里，
  // 而是这三类用户看得见、改得动的东西：共通字段、同步规则、关联字段。
  // ============================================================

  async function openRelationsDialog() {
    const dlg = buildDialog(
      "db-dialog-relations",
      "关系与同步",
      "共通字段：一次定义、多表引用，改一处所有引用它的字段一起变。同步规则：改一张表，关联表按规则一起更新 —— 规则可以随时关掉，关掉之后目标字段就恢复可编辑。"
    );
    dlg.textContent = "";
    dlg.append(node("h3", null, "关系与同步"));
    dlg.append(
      node("p", { class: "hint" },
        "共通字段解决「同一个东西在好几张表里重复维护」；同步规则解决「改了一处，另一处也要跟着变」。两者都能随时停用，出了问题能一键止血。"
      )
    );

    // ---------- 共通字段 ----------
    const sharedBox = node("div", { class: "db-backup-list" });
    dlg.append(node("h4", null, "共通字段"));
    dlg.append(sharedBox);

    async function reloadShared() {
      sharedBox.textContent = "";
      let list = [];
      try {
        list = await call("shared.list", {});
      } catch (e) {
        sharedBox.append(node("div", { class: "db-empty" }, "读不到共通字段：" + errText(e)));
        return;
      }
      if (!list || !list.length) {
        sharedBox.append(node("div", { class: "db-empty" }, "还没有共通字段"));
      }
      (list || []).forEach((f) => {
        const n = (f.used_by || []).length;
        sharedBox.append(
          node("div", { class: "db-backup-item" },
            node("span", { class: "t" }, "⇄ " + f.name),
            node("span", { class: "s" }, n + " 张表在用")
          )
        );
      });
    }

    // 新建一个共通字段：名字 + 类型 +（选择类型时的）选项
    const shName = node("input", { type: "text", placeholder: "字段名，如「状态」" });
    const shType = node("select", {});
    ["text", "integer", "money", "date", "boolean"].forEach((t) => {
      shType.append(node("option", { value: t }, t));
    });
    const shOpts = node("input", { type: "text", placeholder: "选项，逗号分隔（可留空）" });
    dlg.append(
      node("div", { class: "db-form-row" }, shName, shType),
      node("div", { class: "db-form-row" }, shOpts)
    );

    // ---------- 同步规则 ----------
    dlg.append(node("h4", null, "同步规则"));
    const ruleBox = node("div", { class: "db-backup-list" });
    dlg.append(ruleBox);

    async function reloadRules() {
      ruleBox.textContent = "";
      let list = [];
      try {
        list = await call("sync.list", {});
      } catch (e) {
        ruleBox.append(node("div", { class: "db-empty" }, "读不到同步规则：" + errText(e)));
        return;
      }
      if (!list || !list.length) {
        ruleBox.append(node("div", { class: "db-empty" }, "还没有同步规则"));
      }
      (list || []).forEach((r) => {
        const on = node("input", { type: "checkbox" });
        on.checked = !!r.enabled;
        on.addEventListener("change", async () => {
          try {
            await call("sync.toggle", { id: r.id, enabled: on.checked });
            toast(on.checked ? "已启用同步：" + r.name : "已停用同步：" + r.name, "info");
          } catch (e2) {
            on.checked = !on.checked;
            toast("改不了：" + errText(e2), "error");
          }
        });
        const del = node("button", { class: "btn btn-ghost db-mini", type: "button" }, "删除");
        del.addEventListener("click", async () => {
          try {
            await call("sync.delete", { id: r.id });
            await reloadRules();
          } catch (e2) {
            toast("删不掉：" + errText(e2), "error");
          }
        });
        ruleBox.append(
          node("div", { class: "db-backup-item" },
            node("span", { class: "t" },
              (r.enabled ? "● " : "○ ") + r.name + "：" +
              r.source_table + "." + r.source_field + " → " + r.target_table + "." + r.target_field
            ),
            node("span", { class: "s" }, on, del)
          )
        );
      });
    }

    // 新建一条同步规则（源表.字段 → 目标表.字段，经由某个关联字段）
    const rName = node("input", { type: "text", placeholder: "规则名，如「客户改名同步到订单」" });
    const rSrc = node("input", { type: "text", placeholder: "源表.字段，如 客户.客户名" });
    const rDst = node("input", { type: "text", placeholder: "目标表.字段，如 订单.客户名" });
    const rVia = node("input", { type: "text", placeholder: "经由的关联字段，如 客户" });
    const rMode = node("select", {});
    [["mirror", "单向镜像（目标只读）"], ["two_way", "双向"], ["suggest", "只给建议不自动写"]].forEach((m) => {
      rMode.append(node("option", { value: m[0] }, m[1]));
    });
    const rConflict = node("select", {});
    [
      ["source_wins", "冲突时以源为准"],
      ["target_wins", "冲突时以目标为准"],
      ["last_write_wins", "冲突时以最后改动为准"],
    ].forEach((m) => {
      rConflict.append(node("option", { value: m[0] }, m[1]));
    });
    const rScope = node("select", {});
    [["always", "总是覆盖目标"], ["fill_empty_only", "只填目标为空的"]].forEach((m) => {
      rScope.append(node("option", { value: m[0] }, m[1]));
    });
    dlg.append(
      node("div", { class: "db-form-row" }, rName),
      node("div", { class: "db-form-row" }, rSrc, rDst),
      node("div", { class: "db-form-row" }, rVia, rMode),
      node("div", { class: "db-form-row" }, rConflict, rScope)
    );

    const actions = node("div", { class: "db-dialog-actions" });
    const btnAddShared = node("button", { class: "btn", type: "button" }, "新建共通字段");
    btnAddShared.addEventListener("click", async () => {
      const name = shName.value.trim();
      if (!name) { toast("给共通字段起个名字", "error"); return; }
      const opts = shOpts.value
        .split(/[,，]/)
        .map((x) => x.trim())
        .filter(Boolean);
      try {
        await call("shared.save", {
          field: {
            id: "", name: name, ty: shType.value, options: opts,
            default: null, comment: null, used_by: [],
          },
          apply: true,
        });
        shName.value = ""; shOpts.value = "";
        await reloadShared();
        toast("共通字段已建好", "info");
      } catch (e) {
        toast("建不了：" + errText(e), "error");
      }
    });

    const btnAddRule = node("button", { class: "btn", type: "button" }, "新建同步规则");
    btnAddRule.addEventListener("click", async () => {
      const src = rSrc.value.trim();
      const dst = rDst.value.trim();
      if (!rName.value.trim() || !src || !dst || !rVia.value.trim()) {
        toast("规则名、源、目标、经由字段都要填", "error");
        return;
      }
      const sp = src.split(".");
      const dp = dst.split(".");
      if (sp.length !== 2 || dp.length !== 2) {
        toast("源和目标都写成「表.字段」", "error");
        return;
      }
      try {
        await call("sync.save", {
          rule: {
            id: "", name: rName.value.trim(), enabled: true,
            source_table: sp[0], source_field: sp[1],
            target_table: dp[0], target_field: dp[1],
            via: rVia.value.trim(),
            mode: rMode.value, conflict: rConflict.value, scope: rScope.value,
            created_at: 0,
          },
        });
        rName.value = ""; rSrc.value = ""; rDst.value = ""; rVia.value = "";
        await reloadRules();
        toast("同步规则已建好，改动源表时目标表会跟着变", "info");
      } catch (e) {
        toast("建不了：" + errText(e), "error");
      }
    });
    actions.append(btnAddShared, btnAddRule);
    dlg.append(actions);

    const closer = node("div", { class: "db-dialog-actions" });
    const btnClose = node("button", { class: "btn btn-ghost", type: "button" }, "关闭");
    btnClose.addEventListener("click", () => dlg.close());
    closer.append(btnClose);
    dlg.append(closer);
    // 先弹出来，再去拉列表 —— 骨架已经在 DOM 里，用户不会看到空壳
    dlg.showModal();
    await reloadShared();
    await reloadRules();
  }

  // ============================================================
  // 命名视图（替代 SELECT）
  //
  // 没有查询语句、没有输入框：把「筛选哪些行、按什么排序、隐藏哪些列」
  // 存成一个有名字的视角，下次点一下就回到这个视角。
  // ============================================================
  /**
   * 变更历史：改错了、删错了，在这里退回去。
   *
   * 为什么要有它：项目承诺「不丢数据」。崩溃不丢那半边由日志 + fsync 兑现了，
   * 但"改错了能回退"一直没有 —— 误删一行只能眼睁睁看着。这个对话框就是那半边。
   * 只记「改」与「删」：新增不会丢东西，记进去只会让列表变吵。
   */
  async function openHistoryDialog() {
    if (!state.current) {
      toast("先打开一张表，再看它的历史", "error");
      return;
    }
    const dlg = buildDialog(
      "db-dialog-history",
      "变更历史 · " + state.current,
      "这里列出最近改了什么、删了什么，每一条都能退回去。最多留 500 条，超了从最旧的开始丢 —— 它是「改错了能回退」，不是全量审计。"
    );
    dlg.textContent = "";
    dlg.append(node("h3", null, "变更历史 · " + state.current));
    const box = node("div", { class: "db-backup-list" });
    dlg.append(box);

    async function reload() {
      box.textContent = "";
      let list = [];
      try {
        const r = await call("schema.history", { table: state.current, limit: 100 });
        list = (r && r.items) || [];
      } catch (e) {
        box.append(node("div", { class: "db-empty" }, "读不到历史：" + errText(e)));
        return;
      }
      if (!list.length) {
        box.append(node("div", { class: "db-empty" }, "这张表还没有改动记录"));
        return;
      }
      list.forEach((h) => {
        const when = h.at_ms
          ? new Date(Number(h.at_ms)).toLocaleString("zh-CN", { hour12: false })
          : "";
        const undo = node("button", { class: "btn btn-ghost db-mini", type: "button" }, "回退");
        undo.addEventListener("click", async () => {
          undo.disabled = true;
          try {
            const r = await call("schema.undo", { table: state.current, key: h.key });
            toast((r && r.message) || "已回退", "ok");
            await reload();
            await loadPage();
          } catch (e2) {
            toast("回退失败：" + errText(e2), "error");
            undo.disabled = false;
          }
        });
        box.append(
          node(
            "div",
            { class: "db-backup-item" },
            node("span", { class: "t" }, (h.op === "delete" ? "✕ " : "✎ ") + (h.preview || "")),
            node("span", { class: "s" }, when),
            undo
          )
        );
      });
    }

    const actions = node("div", { class: "db-dialog-actions" });
    const btnClose = node("button", { class: "btn", type: "button" }, "关闭");
    btnClose.addEventListener("click", () => closeDialog(dlg));
    actions.append(btnClose);
    dlg.append(actions);
    dlg.showModal();
    await reload();
  }

  async function openViewsDialog() {
    if (!state.current) {
      toast("先打开一张表，再建视图", "error");
      return;
    }
    const dlg = buildDialog(
      "db-dialog-views",
      "视图",
      "视图 = 筛选条件 + 排序 + 隐藏列的组合，存下来以后一键回到这个视角。不需要写任何查询语句。"
    );
    dlg.textContent = "";
    dlg.append(node("h3", null, "视图 · " + state.current));
    const box = node("div", { class: "db-backup-list" });
    dlg.append(box);

    async function reload() {
      box.textContent = "";
      let list = [];
      try {
        list = await call("view.list", { table: state.current });
      } catch (e) {
        box.append(node("div", { class: "db-empty" }, "读不到视图：" + errText(e)));
        return;
      }
      if (!list || !list.length) {
        box.append(node("div", { class: "db-empty" }, "这张表还没有视图"));
      }
      (list || []).forEach((v) => {
        const use = node("button", { class: "btn btn-ghost db-mini", type: "button" }, "应用");
        use.addEventListener("click", async () => {
          state.viewId = v.id;
          dlg.close();
          if (state.grid) await state.grid.reload();
          toast("已切到视图「" + v.name + "」", "info");
        });
        const del = node("button", { class: "btn btn-ghost db-mini", type: "button" }, "删除");
        del.addEventListener("click", async () => {
          try {
            await call("view.delete", { id: v.id });
            if (state.viewId === v.id) {
              state.viewId = null;
              if (state.grid) await state.grid.reload();
            }
            await reload();
          } catch (e2) {
            toast("删不掉：" + errText(e2), "error");
          }
        });
        box.append(
          node("div", { class: "db-backup-item" },
            node("span", { class: "t" }, v.name),
            node("span", { class: "s" }, use, del)
          )
        );
      });
    }
    const vName = node("input", { type: "text", placeholder: "视图名，如「未收款的订单」" });
    const vField = node("input", { type: "text", placeholder: "筛选字段（留空=不筛选）" });
    const vOp = node("select", {});
    [
      ["contains", "包含"], ["eq", "等于"], ["ne", "不等于"],
      ["gt", "大于"], ["lt", "小于"], ["is_empty", "为空"], ["is_not_empty", "不为空"],
    ].forEach((m) => vOp.append(node("option", { value: m[0] }, m[1])));
    const vVal = node("input", { type: "text", placeholder: "值" });
    const vSort = node("input", { type: "text", placeholder: "排序字段（留空=不排序）" });
    const vDesc = node("select", {});
    [["asc", "升序"], ["desc", "降序"]].forEach((m) =>
      vDesc.append(node("option", { value: m[0] }, m[1])));
    dlg.append(
      node("div", { class: "db-form-row" }, vName),
      node("div", { class: "db-form-row" }, vField, vOp, vVal),
      node("div", { class: "db-form-row" }, vSort, vDesc)
    );

    const actions = node("div", { class: "db-dialog-actions" });
    const btnSave = node("button", { class: "btn", type: "button" }, "保存为视图");
    btnSave.addEventListener("click", async () => {
      const name = vName.value.trim();
      if (!name) { toast("给视图起个名字", "error"); return; }
      const filter = vField.value.trim()
        ? {
            all: true,
            items: [{
              field: vField.value.trim(),
              op: vOp.value,
              value: vVal.value,
            }],
          }
        : null;
      const sorts = vSort.value.trim()
        ? [{ field: vSort.value.trim(), desc: vDesc.value === "desc" }]
        : [];
      try {
        await call("view.save", {
          view: {
            id: "", table: state.current, name: name,
            filter: filter, sorts: sorts, group_by: null, hidden: [],
          },
        });
        vName.value = ""; vField.value = ""; vVal.value = ""; vSort.value = "";
        await reload();
        toast("视图已保存", "info");
      } catch (e) {
        toast("存不了：" + errText(e), "error");
      }
    });
    const btnAll = node("button", { class: "btn btn-ghost", type: "button" }, "看整张表");
    btnAll.addEventListener("click", async () => {
      state.viewId = null;
      dlg.close();
      if (state.grid) await state.grid.reload();
    });
    const btnClose = node("button", { class: "btn btn-ghost", type: "button" }, "关闭");
    btnClose.addEventListener("click", () => dlg.close());
    actions.append(btnSave, btnAll, btnClose);
    dlg.append(actions);
    dlg.showModal();
    await reload();
  }

  async function openBackupDialog() {
    const dlg = buildDialog(
      "db-dialog-backup",
      "备份数据",
      "备份是当前数据的一份完整快照（先把状态压实成快照，再复制它 —— 不是直接复制数据文件），" +
        "放在数据目录的 backups/ 里。生成后会立刻三步校验：非空、能被解析回一份完整状态、表数与当前库一致。"
    );
    dlg.textContent = "";
    dlg.append(el("h3", null, "备份数据"));
    dlg.append(
      el("p", { class: "hint" },
        "备份是当前数据的完整快照（先压实成快照再复制，不是直接复制数据文件 —— 直接复制正在写入的日志可能拿到半截状态）。" +
          "每份备份生成后立刻三步校验：非空、能被解析回一份完整状态、表数与当前库一致。")
    );

    const listBox = el("div", { class: "db-backup-list" });
    const statusLine = el("p", { class: "hint" }, "正在读取备份列表…");
    const actions = el("div", { class: "db-dialog-actions" });
    const btnDo = el("button", { class: "btn btn-primary", type: "button" }, "立即备份");
    const btnClose = el("button", { class: "btn", type: "button" }, "关闭");
    actions.append(btnDo, btnClose);

    async function refresh() {
      try {
        const r = await call("app.backupList");
        const items = (r && r.items) || [];
        listBox.textContent = "";
        if (!items.length) {
          listBox.appendChild(el("div", { class: "db-empty" }, "还没有备份。点「立即备份」生成第一份。"));
          statusLine.textContent = "";
        } else {
          for (const it of items.slice(0, 10)) {
            const row = el("div", { class: "db-backup-item" });
            row.append(el("span", { class: "t" }, it.name));
            row.append(el("span", { class: "s" }, fmtBytes(it.size) + " · " + fmtStamp(it.created_ms)));
            listBox.append(row);
          }
          if (items.length > 10) {
            listBox.append(el("p", { class: "hint" }, "只列最近 10 份；全部都在数据目录的 backups/ 里。"));
          }
          statusLine.textContent = "共 " + items.length + " 份备份（新的在上）";
        }
      } catch (e) {
        statusLine.textContent = "读取备份列表失败：" + errText(e);
      }
    }

    btnDo.addEventListener("click", async () => {
      btnDo.disabled = true;
      const old = btnDo.textContent;
      btnDo.textContent = "备份中…";
      try {
        const info = await call("app.backupCreate");
        toast("已备份 " + info.name + "（" + fmtBytes(info.size) + "，已校验）");
        await refresh();
      } catch (e) {
        toast("备份失败：" + errText(e), "error");
      } finally {
        btnDo.disabled = false;
        btnDo.textContent = old;
      }
    });
    btnClose.addEventListener("click", () => dlg.close("cancel"));

    dlg.append(listBox, statusLine, actions);
    dlg.showModal();
    refresh();
  }

  // ============================================================
  // 表结构编辑（v0.3.0 · F 包）
  // ============================================================
  // 三个入口：表项上的「改名」、侧栏的「表结构」、对话框里的加列/删列/表注释。
  // 所有暗礁（主键、NOT NULL 默认值、被索引引用）都在 Rust 层拦 ——
  // 界面只负责把操作送过去、把错误原样说给人听，不自己预判。

  function fmtColType(decl) {
    // 声明类型是 SQLite 的宽松原文（可能为空），翻译成人能看懂的
    const t = (decl || "").toUpperCase();
    if (t.includes("MONEY") || t.includes("BIGINT")) return "金额";
    if (t.includes("DATETIME")) return "日期时间";
    if (t.includes("DATE")) return "日期";
    if (t.includes("BOOL")) return "是/否";
    if (t.includes("INT")) return "整数";
    if (t.includes("REAL") || t.includes("FLOA") || t.includes("DOUB")) return "小数";
    if (t.includes("BLOB")) return "二进制";
    return "文本";
  }

  function promptRenameTable(oldName) {
    const dlg = buildDialog(
      "db-dialog-rename",
      "给表改名",
      "数据、索引和注释都会跟着新名字走，别的什么都不用动。"
    );
    dlg.textContent = "";
    dlg.append(el("h3", null, "给表改名"));
    dlg.append(el("p", { class: "hint" }, "把「" + oldName + "」改成："));
    const input = el("input", { class: "input", type: "text", value: oldName });
    const errLine = el("p", { class: "hint", style: "color: #b00" }, "");
    const actions = el("div", { class: "db-dialog-actions" });
    const btnOk = el("button", { class: "btn btn-primary", type: "button" }, "确认改名");
    const btnCancel = el("button", { class: "btn", type: "button" }, "取消");
    actions.append(btnOk, btnCancel);
    btnOk.addEventListener("click", async () => {
      const newName = input.value.trim();
      if (!newName || newName === oldName) { dlg.close("cancel"); return; }
      btnOk.disabled = true;
      try {
        await call("schema.renameTable", { old: oldName, new: newName });
        dlg.close("ok");
        toast("已改名：" + oldName + " → " + newName);
        if (state.current === oldName) state.current = newName;
        await refreshTables();
      } catch (e) {
        errLine.textContent = errText(e);
        btnOk.disabled = false;
      }
    });
    btnCancel.addEventListener("click", () => dlg.close("cancel"));
    dlg.append(input, errLine, actions);
    dlg.showModal();
  }

  async function openSchemaDialog() {
    if (!state.current) {
      toast("先在左侧选一张表，再看它的结构", "error");
      return;
    }
    const tname = state.current;
    const dlg = buildDialog(
      "db-dialog-schema",
      "表结构 · " + tname,
      "加列、删列、改表注释都在这里。删列不可恢复 —— " +
        "重要的表先去侧栏点「备份数据库」。"
    );
    dlg.textContent = "";
    dlg.append(el("h3", null, "表结构 · " + tname));

    // 表注释
    let info;
    try {
      info = await call("schema.getTable", { name: tname });
    } catch (e) {
      toast("读表结构失败：" + errText(e), "error");
      return;
    }
    const commentBox = el("div", { class: "db-schema-comment" });
    const commentInput = el("input", { class: "input", type: "text", value: info.comment || "", placeholder: "这张表是干什么用的（表注释）" });
    const btnComment = el("button", { class: "btn btn-ghost", type: "button" }, "保存注释");
    commentBox.append(commentInput, btnComment);

    // 列清单
    const list = el("div", { class: "db-schema-list" });
    for (const c of info.columns || []) {
      const row = el("div", { class: "db-schema-col" });
      const tags = [
        c.pk ? "主键" : "",
        c.not_null ? "必填" : "",
        c.default != null && c.default !== "" ? "默认 " + c.default : "",
      ].filter(Boolean).join(" · ");
      row.append(
        el("span", { class: "n" }, c.name),
        el("span", { class: "t" }, fmtColType(c.decl_type) + (tags ? "（" + tags + "）" : ""))
      );
      const btnRen = el("button", { class: "btn btn-ghost db-mini", type: "button" }, "改名");
      btnRen.addEventListener("click", () => {
        dlg.close("ok");
        promptRenameColumn(tname, c.name);
      });
      row.append(btnRen);
      if (c.pk) {
        row.append(el("span", { class: "s" }, "主键不可删"));
      } else {
        const btnDel = el("button", { class: "btn btn-ghost db-mini", type: "button" }, "删列");
        btnDel.addEventListener("click", async () => {
          if (!confirm("删掉列「" + c.name + "」？这一列的数据会一起消失，且不可恢复。")) return;
          try {
            await call("schema.dropColumn", { table: tname, column: c.name });
            toast("已删列「" + c.name + "」");
            dlg.close("ok");
            openSchemaDialog(); // 重新打开 = 刷新内容
            openTable(tname);   // 网格也要跟着变
          } catch (e) {
            toast(errText(e), "error");
          }
        });
        row.append(btnDel);
      }
      list.append(row);
    }

    // 加列表单
    await ensureTypes();
    const addBox = el("div", { class: "db-schema-add" });
    const nameInput = el("input", { class: "input", type: "text", placeholder: "列名" });
    const typeSel = el("select", { class: "select" });
    for (const [v, label] of TYPES || []) typeSel.appendChild(el("option", { value: v }, label));
    const nnChk = el("input", { type: "checkbox" });
    const defInput = el("input", { class: "input", type: "text", placeholder: "默认值（可选，如 0）" });
    const cmtInput = el("input", { class: "input", type: "text", placeholder: "列说明（可选）" });
    const btnAdd = el("button", { class: "btn btn-primary", type: "button" }, "加列");
    const addErr = el("p", { class: "hint", style: "color: #b00" }, "");
    addBox.append(
      el("p", { class: "hint" }, "加一个新列（已有的行会用默认值填充）："),
      nameInput, typeSel,
      el("label", { class: "hint" }, " 必填 ", nnChk),
      defInput, cmtInput, btnAdd, addErr
    );
    btnAdd.addEventListener("click", async () => {
      const cname = nameInput.value.trim();
      if (!cname) { addErr.textContent = "先给列起个名"; return; }
      btnAdd.disabled = true;
      try {
        await call("schema.addColumn", {
          table: tname,
          column: {
            name: cname,
            ty: typeSel.value,
            not_null: nnChk.checked,
            default: defInput.value.trim() === "" ? null : defInput.value.trim(),
            primary_key: false,
            comment: cmtInput.value.trim() === "" ? null : cmtInput.value.trim(),
          },
        });
        toast("已加列「" + cname + "」");
        dlg.close("ok");
        openSchemaDialog();
        openTable(tname);
      } catch (e) {
        addErr.textContent = errText(e);
        btnAdd.disabled = false;
      }
    });

    btnComment.addEventListener("click", async () => {
      try {
        await call("schema.setTableComment", { table: tname, comment: commentInput.value });
        toast("表注释已保存");
      } catch (e) {
        toast(errText(e), "error");
      }
    });

    dlg.append(commentBox, el("p", { class: "hint" }, "列（" + (info.columns || []).length + " 个）："), list, addBox);
    dlg.showModal();
  }

  /** 改列名的小对话框（表结构对话框与列头都用它）。 */
  function promptRenameColumn(tableName, colName) {
    const dlg = buildDialog("db-dialog-rename-col", "改列名",
      "数据、索引与注释都会跟着新列名走。");
    dlg.textContent = "";
    dlg.append(el("h3", null, "改列名"));
    dlg.append(el("p", { class: "hint" }, "把「" + colName + "」改成："));
    const input = el("input", { class: "input", type: "text", value: colName });
    const errLine = el("p", { class: "hint", style: "color: #b00" }, "");
    const actions = el("div", { class: "db-dialog-actions" });
    const btnOk = el("button", { class: "btn btn-primary", type: "button" }, "确认");
    const btnCancel = el("button", { class: "btn", type: "button" }, "取消");
    actions.append(btnOk, btnCancel);
    btnOk.addEventListener("click", async () => {
      const to = input.value.trim();
      if (!to || to === colName) { dlg.close("cancel"); return; }
      btnOk.disabled = true;
      try {
        await call("schema.renameColumn", { table: tableName, column: colName, to: to });
        dlg.close("ok");
        toast("已改列名：" + colName + " → " + to);
        openSchemaDialog(); // 刷新对话框内容
        openTable(tableName); // 网格跟着变
      } catch (e) {
        errLine.textContent = errText(e);
        btnOk.disabled = false;
      }
    });
    btnCancel.addEventListener("click", () => dlg.close("cancel"));
    dlg.append(input, errLine, actions);
    dlg.showModal();
  }
  // ============================================================
  // 列头菜单（v0.3.0 · P1-4b）
  // ============================================================
  // 网格把"用户点了哪一列的 ⋯"抛上来，这里决定能做什么。
  // 三个动作都复用已有的 IPC（改列名 / 加列 / 删列）—— 界面层不新增业务规则，
  // 三道 SQLite 暗礁（主键、NOT NULL 默认值、被索引引用）依旧由 Rust 拦。
  /** 整理这一列（P1-5）：只改值不改类型。报告里带"哪几行看不懂"。 */
  async function runNormalize(tname, colName, rule, label) {
    if (!confirm("把「" + colName + "」这一列按" + label + "整理一遍？\n\n" +
      "只会改动格式不一致的值（例如 2026/1/5 → 2026-01-05），看不懂的行会原样留着并告诉你。")) return;
    try {
      const rep = await call("schema.normalizeColumn", { table: tname, column: colName, rule: rule });
      const skipped = (rep && rep.skipped) || [];
      let msg = "已整理「" + colName + "」：" + (rep ? rep.changed : 0) + " 行被规范化";
      if (skipped.length) {
        msg += "，" + skipped.length + " 行看不懂（保持原样）";
      }
      toast(msg, skipped.length ? "warn" : "ok");
      if (skipped.length) {
        logLine("看不懂的行（前 5 条）：" + skipped.slice(0, 5).map((s) => "第" + s.rowid + "行「" + s.value + "」").join("，"));
      }
      openTable(tname); // 网格刷新
    } catch (e) {
      toast("整理失败：" + errText(e), "error");
    }
  }

  function openColumnMenu(info) {
    if (!state.current) return;
    const tname = state.current;
    const dlg = buildDialog(
      "db-dialog-colmenu",
      "列操作 · " + info.name,
      "对「" + info.name + "」这一列做什么？"
    );
    dlg.textContent = "";
    dlg.append(el("h3", null, "列「" + info.name + "」"));
    const actions = el("div", { class: "db-dialog-actions" });
    const btnRen = el("button", { class: "btn", type: "button" }, "改列名");
    const btnAdd = el("button", { class: "btn", type: "button" }, "加一列");
    const btnDel = el("button", { class: "btn", type: "button" }, "删掉这一列");
    const btnDate = el("button", { class: "btn", type: "button" }, "统一日期格式");
    const btnNum = el("button", { class: "btn", type: "button" }, "清洗数字");
    const btnCancel = el("button", { class: "btn", type: "button" }, "取消");
    actions.append(btnRen, btnAdd, btnDel, btnDate, btnNum, btnCancel);
    btnDate.addEventListener("click", () => {
      dlg.close("ok");
      runNormalize(tname, info.name, "date", "日期格式");
    });
    btnNum.addEventListener("click", () => {
      dlg.close("ok");
      runNormalize(tname, info.name, "number", "数字清洁");
    });
    btnRen.addEventListener("click", () => {
      dlg.close("ok");
      promptRenameColumn(tname, info.name);
    });
    btnAdd.addEventListener("click", () => {
      dlg.close("ok");
      openSchemaDialog(); // 加列表单在「表结构」底部（一处实现，两处入口）
      toast("在「表结构」底部的加列表单里填新列");
    });
    btnDel.addEventListener("click", async () => {
      if (!confirm("删掉列「" + info.name + "」？这一列的数据会一起消失，且不可恢复。")) return;
      try {
        await call("schema.dropColumn", { table: tname, column: info.name });
        dlg.close("ok");
        toast("已删列「" + info.name + "」");
        await refreshTables();
        openTable(tname);
      } catch (e) {
        toast(errText(e), "error"); // 主键列等由 Rust 拦下并说明原因
      }
    });
    btnCancel.addEventListener("click", () => dlg.close("cancel"));
    dlg.append(actions);
    dlg.showModal();
  }
  window.DeskBaseDb = {
    onShow: onShow,
    openBackupDialog: openBackupDialog,
    openSchemaDialog: openSchemaDialog,
    openNewTableDialog: openNewTableDialog,
    openColumnMenu: openColumnMenu,
    refreshTables: refreshTables,
    openImportDialog: openImportDialog,
  };
})();
