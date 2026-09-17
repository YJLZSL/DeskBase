/* ============================================================
   DeskBase 数据库页装配层
   ============================================================
   数据网格（grid.js）与 SQL 编辑器（sql.js）只认识"数据从哪来、
   结果往哪去"，不认识这个应用。本文件负责把它们装配成「数据库」页：

     左栏 库表列表（listTables / createTable / dropTable）
     右栏 数据页签  → DeskBaseGrid.mount(...)，行数据走 keyset 分页
          SQL 页签  → DeskBaseSql.mount(...)，执行走策略层闸门

   与 Rust 的通信走 window.__deskbase.call（app.js 暴露的 IPC 桥）。
   本文件不直接碰数据库，也不拼任何 SQL —— 参数校验都在 Rust 侧。

   三条贯穿全文件的约定（来自 schema.rs 的接口约定，不得破坏）：
   1. Page.columns[0] 恒为 "_rowid"、rows[i][0] 是行号 —— 行号是**行标识**，
      不显示给用户。行对象在交给网格前被映射成 { __rowid, 列名: 值 }，
      网格靠 opts.rowId 取 __rowid 调 updateCell / deleteRows。
   2. money 列按「分」存整数 —— 出库时换算成"元"的字符串给用户看，
      提交时原样交回字符串，由 Rust 的 money_parse 再换算回分。
      两头都不在本文件里做算术，避免第二套实现。
   3. 危险 SQL 的判定与确认在**两个**层面：sql.js 的粗判（组件层）+
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
  };

  // ============================================================
  // 库表列表
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
          "还没有库表。点上方「新建表」建一张，或者用 SQL 页签执行 CREATE TABLE。")
      );
      return;
    }
    for (const t of state.tables) {
      const item = el("button", { class: "db-table-item", type: "button" });
      if (t.name === state.current) item.setAttribute("aria-current", "true");
      const title = el("span", { class: "t" }, t.name);
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
      item.append(del, title, sub);
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
          notNull: !!c.not_null,
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
    // ⚠️ 字段名是 Rust 侧的 snake_case（`Page { has_more, next_cursor }`），
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
  // SQL 页签（策略层闸门在这里落地）
  // ============================================================
  function ensureSql() {
    if (state.sql && !state.sqlDirty) return;
    if (!window.DeskBaseSql) {
      paneSql.textContent = "";
      paneSql.appendChild(
        el("div", { class: "db-empty" },
          "SQL 编辑器没有加载成功 —— 请检查 app/src/assets.rs 是否登记了 /sql.js。")
      );
      return;
    }
    if (state.sql) {
      state.sql.destroy();
      state.sql = null;
    }
    paneSql.textContent = "";
    state.sqlDirty = false;
    state.sql = window.DeskBaseSql.mount(paneSql, {
      maxRows: 5000,
      listTables: () => call("schema.listTables"),
      runQuery: gatedRunQuery,
    });
  }

  /**
   * 带策略层闸门的执行入口。
   *
   * Rust 侧对危险语句（DROP / TRUNCATE / 无顶层 WHERE 的 DELETE / UPDATE …）
   * 会回 { needsConfirm: 理由 } 而不是直接执行 —— 本函数负责把确认框问出来，
   * 用户拒绝就抛错（sql.js 会把它当这条语句的错误展示，后面的语句不再执行）。
   * 同一条 SQL 被问两次（组件层 + 策略层）只发生在两边都认成危险的情形 ——
   * 宁可多问一次，不能少问一次。
   */
  async function gatedRunQuery(sql, opts) {
    const maxRows = opts && opts.maxRows ? opts.maxRows : 5000;
    let r = await call("schema.runQuery", { sql: sql, maxRows: maxRows, confirmed: false });
    if (r && r.needsConfirm) {
      const go = await confirmBox(
        "确认要执行这条写操作吗？",
        r.needsConfirm + "\n\n" + clip(sql, 400)
      );
      if (!go) throw new Error("已取消，未执行");
      r = await call("schema.runQuery", { sql: sql, maxRows: maxRows, confirmed: true });
    }
    return r;
  }

  // ============================================================
  // 页签
  // ============================================================
  function showTab(tab) {
    state.tab = tab;
    tabsEl.querySelectorAll(".db-tab").forEach((b) => {
      b.setAttribute("aria-selected", b.dataset.tab === tab ? "true" : "false");
    });
    paneGrid.hidden = tab !== "data";
    paneSql.hidden = tab !== "sql";
    if (tab === "sql") ensureSql();
  }

  // ============================================================
  // 新建表 / 删除表 对话框
  // ============================================================
  const TYPES = [
    ["text", "文本"],
    ["integer", "数字（整数）"],
    ["real", "数字（小数）"],
    ["money", "金额"],
    ["boolean", "是 / 否"],
    ["date", "日期"],
    ["datetime", "日期时间"],
    ["json", "JSON（进阶）"],
    ["blob", "二进制（进阶）"],
  ];

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

  function openNewTableDialog() {
    const dlg = buildDialog(
      "db-dialog-new",
      "新建表",
      "字段名可以用中文、字母、数字、下划线。金额按「分」存储：写入 12.34 存 1234，" +
        "显示时自动换算回来。主键列必须是整数或文本。"
    );
    dlg.textContent = "";
    dlg.append(el("h3", null, "新建表"));
    dlg.append(
      el("p", { class: "hint" },
        "字段名可以用中文、字母、数字、下划线，不能叫 rowid。金额按「分」存储：" +
          "写入 12.34 存 1234，显示时自动换算回来。二进制列不能在表格里直接编辑。")
    );

    const form = el("form", { novalidate: "novalidate" });
    const nameInput = el("input", { type: "text", placeholder: "表名，例如：费用明细" });
    const nameField = el("div");
    nameField.appendChild(nameInput);
    form.appendChild(nameField);

    const colsBox = el("div", { class: "db-cols" });
    form.appendChild(colsBox);

    function colRow() {
      const row = el("div", { class: "db-col-row" });
      const name = el("input", { type: "text", placeholder: "字段名" });
      const type = el("select");
      TYPES.forEach(([v, label]) => {
        const opt = el("option", { value: v }, label);
        if (v === "text") opt.setAttribute("selected", "selected");
        type.appendChild(opt);
      });
      const pk = el("label");
      const pkBox = el("input", { type: "checkbox" });
      pk.append(pkBox, document.createTextNode("主键"));
      const nn = el("label");
      const nnBox = el("input", { type: "checkbox" });
      nn.append(nnBox, document.createTextNode("必填"));
      const rm = el("button", { class: "rm", type: "button", title: "移除这一列" }, "×");
      rm.addEventListener("click", () => {
        if (colsBox.children.length > 1) row.remove();
      });
      row.append(name, type, pk, nn, rm);
      return row;
    }
    colsBox.appendChild(colRow());
    colsBox.appendChild(colRow());
    colsBox.appendChild(colRow());

    const addCol = el("button", { class: "btn btn-ghost", type: "button" }, "添加字段");
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
        columns.push({
          name: cname,
          ty: type,
          not_null: nn || pk,
          default: null,
          primary_key: pk,
          comment: null,
        });
      }
      if (!columns.length) {
        toast("至少要有一个字段", "error");
        return;
      }
      submit.disabled = true;
      submit.textContent = "创建中…";
      try {
        await call("schema.createTable", { spec: { name: tname, comment: null, columns: columns } });
        toast("已创建表「" + tname + "」");
        dlg.close();
        await refreshTables();
        await openTable(tname);
      } catch (e) {
        toast("建表失败：" + errText(e), "error");
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
  // 事件绑定与生命周期
  // ============================================================
  document.getElementById("btn-db-new-table").addEventListener("click", openNewTableDialog);
  document.getElementById("btn-db-refresh").addEventListener("click", () => refreshTables());
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

  window.DeskBaseDb = { onShow: onShow, refreshTables: refreshTables };
})();
