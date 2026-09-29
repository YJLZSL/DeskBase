// ============================================================
// 单据编辑器（"能打印的表格出口"）
// ============================================================
// 它不是 Photoshop，也不需要是。它要解决的事只有一件：
// **把"送货单/订货单/收据"这种单据，让人能在五分钟内自己摆好并打出来。**
//
// 为什么做它（证据）：`local-docs/reference/31-user-pain-points.md` §0.3 ——
// "DeskBase 现在最该补的三处：行内公式列 ＞ **打印/报表出口** ＞ 单表导出"，
// 而三条用户场景都点名同一个缺口（S1 打不出库单、S3"只能用 Excel 打印"、
// S4 对账单格式）。`30` §0.3 说得更准：Access 留下的空位是
// "能给同事用的小应用"——只有表、没有出口就接不住。
//
// 借鉴同类产品（送货单打印软件）的四条共性做法：
//   ① 内置模板库  ② 可视化拖拽  ③ 纸张规格可选  ④ 打印预览
// 没做的：条码/二维码（要引库，体积代价与 2.9 MB 的定位不符）。
//
// **坐标一律毫米。** 打印这件事上像素是错的单位（随 DPI 变），毫米不是。
// 屏幕上按 96dpi 换算，Rust 侧渲染也用同一套坐标 —— 两边各算一套迟早对不上。
// ============================================================

(function () {
  "use strict";

  /** 1mm 在 96dpi 下的 CSS 像素数 */
  const MM = 96 / 25.4;

  const state = {
    papers: [],
    builtin: [],
    templates: [],
    cur: null, // 当前编辑的模板（深拷贝，改它不直接动列表）
    sel: null, // 选中的元素下标
    dirty: false,
  };

  function $(id) {
    return document.getElementById(id);
  }
  function el(tag, attrs, text) {
    const n = document.createElement(tag);
    if (attrs) {
      for (const k in attrs) {
        if (k === "class") n.className = attrs[k];
        else if (k === "style") n.style.cssText = attrs[k];
        else n.setAttribute(k, attrs[k]);
      }
    }
    if (text != null) n.textContent = text;
    return n;
  }
  function call(cmd, args) {
    return window.__deskbase.call(cmd, args || {});
  }
  function errText(e) {
    if (window.DeskBaseUI && typeof window.DeskBaseUI.errText === "function") {
      return window.DeskBaseUI.errText(e);
    }
    return (e && e.message) || String(e);
  }
  function toast(text, kind) {
    const U = window.DeskBaseUI;
    if (U && typeof U.toast === "function") U.toast(text, kind ? { kind: kind } : undefined);
    else console.log("[DeskBaseForms] " + text);
  }
  function clone(o) {
    return JSON.parse(JSON.stringify(o));
  }

  // ---------- 模板列表 ----------
  function renderList() {
    const box = $("forms-list");
    if (!box) return;
    box.textContent = "";

    const add = (t, isBuiltin) => {
      const item = el("button", { class: "forms-item", type: "button" });
      if (state.cur && state.cur.id === t.id) item.setAttribute("aria-current", "true");
      item.append(el("span", { class: "n" }, t.name));
      if (isBuiltin) item.append(el("span", { class: "tag" }, "内置"));
      item.addEventListener("click", () => openTemplate(t, isBuiltin));
      box.append(item);
    };

    if (state.templates.length) {
      box.append(el("div", { class: "forms-group" }, "我保存的"));
      state.templates.forEach((t) => add(t, false));
    }
    box.append(el("div", { class: "forms-group" }, "内置模板"));
    state.builtin.forEach((t) => add(t, true));

    if (!state.templates.length && !state.builtin.length) {
      box.append(el("p", { class: "hint" }, "模板没读出来。"));
    }
  }

  /** 打开一个模板：内置的**先复制一份**再编辑 —— 改坏内置模板是说不清的麻烦 */
  function openTemplate(t, isBuiltin) {
    if (state.dirty && !window.confirm("当前模板有未保存的改动，确定丢掉吗？")) return;
    const copy = clone(t);
    if (isBuiltin) {
      copy.id = "user-" + Date.now().toString(36);
      copy.name = t.name + "（副本）";
    }
    state.cur = copy;
    state.sel = null;
    state.dirty = false;
    renderAll();
  }

  function newBlank() {
    const blank = state.builtin.find((t) => /空白/.test(t.name)) || state.builtin[0];
    if (!blank) {
      toast("内置模板还没加载好", "error");
      return;
    }
    openTemplate(blank, true);
  }

  // ---------- 画布 ----------
  /** 把画布上的一个元素渲染成 DOM。拖拽、缩放的把手也在这里挂。 */
  function renderEl(e, idx) {
    let node;
    if (e.kind === "line") {
      node = el("div", { class: "fe fe-line" });
      node.style.borderTopWidth = Math.max(1, (e.size / 3) * MM) + "px";
    } else if (e.kind === "rect") {
      node = el("div", { class: "fe fe-rect" });
      node.style.borderWidth = Math.max(1, (e.size / 3) * MM) + "px";
    } else if (e.kind === "field") {
      node = el("div", { class: "fe fe-field" }, e.text || "");
      node.style.fontSize = e.size * 1.333 + "px";
    } else if (e.kind === "table") {
      node = el("div", { class: "fe fe-table" });
      const tb = el("table");
      const thead = el("thead");
      const tr = el("tr");
      (e.columns || []).forEach((c) => tr.append(el("th", null, c)));
      thead.append(tr);
      tb.append(thead);
      const tb2 = el("tbody");
      for (let r = 0; r < (e.rows || 1); r++) {
        const tr2 = el("tr");
        for (let c = 0; c < (e.columns || []).length; c++) tr2.append(el("td"));
        tb2.append(tr2);
      }
      tb.append(tb2);
      node.append(tb);
      node.style.fontSize = e.size * 1.333 + "px";
    } else {
      node = el("div", { class: "fe fe-text" }, e.text || "");
      node.style.fontSize = e.size * 1.333 + "px";
      if (e.bold) node.style.fontWeight = "600";
      node.style.justifyContent =
        e.align === "center" ? "center" : e.align === "right" ? "flex-end" : "flex-start";
      node.style.alignItems = "center";
    }
    node.style.left = e.x * MM + "px";
    node.style.top = e.y * MM + "px";
    node.style.width = Math.max(0, e.w) * MM + "px";
    if (e.kind !== "line") node.style.height = Math.max(0, e.h) * MM + "px";
    if (idx === state.sel) node.classList.add("sel");

    node.dataset.idx = String(idx);
    attachDrag(node, idx);
    return node;
  }

  function renderCanvas() {
    const sheet = $("forms-sheet");
    if (!sheet) return;
    sheet.textContent = "";
    if (!state.cur) {
      sheet.append(
        el("p", { class: "forms-empty-hint" }, "从左边的模板里挑一张开始，或者点「新建空白」。")
      );
      return;
    }
    const p = state.cur.paper;
    sheet.style.width = p.w * MM + "px";
    sheet.style.height = p.h * MM + "px";
    state.cur.elements.forEach((e, i) => sheet.append(renderEl(e, i)));

    // 点空白处取消选中
    sheet.addEventListener("pointerdown", (ev) => {
      if (ev.target === sheet) {
        state.sel = null;
        renderAll();
      }
    });
  }

  // ---------- 拖拽 / 缩放 ----------
  function attachDrag(node, idx) {
    let mode = null;
    let start = null;

    node.addEventListener("pointerdown", (ev) => {
      ev.stopPropagation();
      state.sel = idx;
      const e = state.cur.elements[idx];
      // 右下角 10px 内算"缩放"，其余算"移动" —— 不再另画一个把手，
      // 免得小元素上两个把手互相盖住（同类工具常见的别扭处）
      const r = node.getBoundingClientRect();
      mode = ev.clientX > r.right - 10 && ev.clientY > r.bottom - 10 ? "resize" : "move";
      start = { x: ev.clientX, y: ev.clientY, ex: e.x, ey: e.y, ew: e.w, eh: e.h };
      node.setPointerCapture(ev.pointerId);
      renderProps();
      renderList();
    });

    node.addEventListener("pointermove", (ev) => {
      if (!start) return;
      const e = state.cur.elements[idx];
      const dx = (ev.clientX - start.x) / MM;
      const dy = (ev.clientY - start.y) / MM;
      if (mode === "move") {
        // 0.5mm 对齐：单据排版基本靠肉眼对齐，半毫米足够细
        e.x = Math.round((start.ex + dx) * 2) / 2;
        e.y = Math.round((start.ey + dy) * 2) / 2;
      } else {
        e.w = Math.max(4, Math.round((start.ew + dx) * 2) / 2);
        e.h = Math.max(2, Math.round((start.eh + dy) * 2) / 2);
      }
      node.style.left = e.x * MM + "px";
      node.style.top = e.y * MM + "px";
      node.style.width = e.w * MM + "px";
      if (e.kind !== "line") node.style.height = e.h * MM + "px";
      state.dirty = true;
    });

    const up = (ev) => {
      if (!start) return;
      start = null;
      try {
        node.releasePointerCapture(ev.pointerId);
      } catch (_) {}
      renderProps();
      renderCanvas();
    };
    node.addEventListener("pointerup", up);
    node.addEventListener("pointercancel", up);
  }

  // ---------- 属性面板 ----------
  function renderProps() {
    const box = $("forms-props");
    if (!box) return;
    box.textContent = "";
    if (!state.cur) return;

    const e = state.sel != null ? state.cur.elements[state.sel] : null;

    // --- 模板级 ---
    const tplTitle = el("div", { class: "forms-props-title" }, "模板");
    box.append(tplTitle);

    const nameRow = el("div", { class: "row" });
    nameRow.append(el("label", null, "名称"));
    const nameIn = el("input", { type: "text", value: state.cur.name });
    nameIn.addEventListener("input", () => {
      state.cur.name = nameIn.value;
      state.dirty = true;
    });
    nameRow.append(nameIn);
    box.append(nameRow);

    const paperRow = el("div", { class: "row" });
    paperRow.append(el("label", null, "纸张"));
    const psel = el("select");
    state.papers.forEach((p, i) => {
      const o = el("option", { value: String(i) }, p.label || p.w + "×" + p.h + " mm");
      if (Math.abs(p.w - state.cur.paper.w) < 0.01 && Math.abs(p.h - state.cur.paper.h) < 0.01) {
        o.selected = true;
      }
      psel.append(o);
    });
    psel.addEventListener("change", () => {
      const p = state.papers[Number(psel.value)];
      state.cur.paper = { w: p.w, h: p.h, label: p.label };
      state.dirty = true;
      renderAll();
    });
    paperRow.append(psel);
    box.append(paperRow);

    // --- 元素级 ---
    const addRow = el("div", { class: "forms-add" });
    [
      ["text", "文字"],
      ["field", "待填栏"],
      ["table", "表格"],
      ["line", "横线"],
      ["rect", "方框"],
    ].forEach(([kind, label]) => {
      const b = el("button", { class: "btn btn-ghost db-mini", type: "button" }, label);
      b.addEventListener("click", () => addElement(kind));
      addRow.append(b);
    });
    box.append(el("div", { class: "forms-props-title" }, "加元素"));
    box.append(addRow);

    if (!e) {
      box.append(el("p", { class: "hint" }, "点画布上的元素可以改它，或者先加一个。"));
      return;
    }

    box.append(el("div", { class: "forms-props-title" }, "选中的元素"));

    const mk = (label, input) => {
      const r = el("div", { class: "row" });
      r.append(el("label", null, label));
      r.append(input);
      box.append(r);
    };

    // 文字内容
    if (e.kind === "text" || e.kind === "field") {
      const ta = el("textarea", { rows: "2" });
      ta.value = e.text || "";
      ta.addEventListener("input", () => {
        e.text = ta.value;
        state.dirty = true;
        renderCanvas();
      });
      mk(e.kind === "field" ? "填写提示" : "文字", ta);
    }
    if (e.kind === "field") {
      const lab = el("input", { type: "text", value: e.label || "" });
      lab.addEventListener("input", () => {
        e.label = lab.value;
        state.dirty = true;
      });
      mk("这一栏是", lab);
    }
    if (e.kind === "table") {
      const cols = el("input", { type: "text", value: (e.columns || []).join(",") });
      cols.addEventListener("input", () => {
        e.columns = cols.value.split(",").map((s) => s.trim()).filter(Boolean);
        state.dirty = true;
        renderCanvas();
      });
      mk("列（逗号分开）", cols);
      const rows = el("input", { type: "number", min: "1", max: "40", value: String(e.rows || 5) });
      rows.addEventListener("input", () => {
        e.rows = Math.max(1, Math.min(40, Number(rows.value) || 1));
        state.dirty = true;
        renderCanvas();
      });
      mk("空行数", rows);
    }

    if (e.kind !== "rect" && e.kind !== "line") {
      const size = el("input", {
        type: "number",
        min: "6",
        max: "72",
        step: "0.5",
        value: String(e.size),
      });
      size.addEventListener("input", () => {
        e.size = Number(size.value) || 10;
        state.dirty = true;
        renderCanvas();
      });
      mk("字号 pt", size);
    }

    const alignSel = el("select");
    [["left", "左"], ["center", "中"], ["right", "右"]].forEach(([v, t]) => {
      const o = el("option", { value: v }, t);
      if (e.align === v) o.selected = true;
      alignSel.append(o);
    });
    alignSel.addEventListener("change", () => {
      e.align = alignSel.value;
      state.dirty = true;
      renderCanvas();
    });
    if (e.kind === "text") mk("对齐", alignSel);

    const boldBox = el("label", { class: "chk" });
    const boldIn = el("input", { type: "checkbox" });
    boldIn.checked = !!e.bold;
    boldIn.addEventListener("change", () => {
      e.bold = boldIn.checked;
      state.dirty = true;
      renderCanvas();
    });
    boldBox.append(boldIn, el("span", null, "加粗"));
    if (e.kind === "text") box.append(boldBox);

    // 位置微调：拖拽对大元素够用，但"左边缘差半毫米"这种只能靠数字
    [
      ["X mm", "x"],
      ["Y mm", "y"],
      ["宽 mm", "w"],
      ["高 mm", "h"],
    ].forEach(([label, key]) => {
      if (key === "h" && e.kind === "line") return;
      const inp = el("input", { type: "number", step: "0.5", value: String(e[key]) });
      inp.addEventListener("input", () => {
        e[key] = Number(inp.value) || 0;
        state.dirty = true;
        renderCanvas();
      });
      mk(label, inp);
    });

    const del = el("button", { class: "btn btn-ghost db-mini", type: "button" }, "删掉这个元素");
    del.addEventListener("click", () => {
      state.cur.elements.splice(state.sel, 1);
      state.sel = null;
      state.dirty = true;
      renderAll();
    });
    box.append(del);
  }

  function addElement(kind) {
    if (!state.cur) {
      toast("先开一张模板（或点「新建空白」）", "error");
      return;
    }
    const presets = {
      text: { text: "文字", size: 12, w: 60, h: 10, align: "left" },
      field: { kind: "field", text: "________", label: "填写说明", size: 10, w: 50, h: 8 },
      table: { columns: ["品名", "数量", "单价", "金额"], rows: 5, size: 9.5, w: 100, h: 40 },
      line: { size: 1, w: 80, h: 0 },
      rect: { size: 0.4, w: 80, h: 30 },
    };
    const base = presets[kind] || {};
    const e = Object.assign(
      {
        kind: kind,
        x: 20,
        y: 20,
        w: 60,
        h: 10,
        text: "",
        size: 10,
        bold: false,
        align: "left",
        label: "",
        columns: [],
        rows: 5,
      },
      base,
      { kind: kind }
    );
    state.cur.elements.push(e);
    state.sel = state.cur.elements.length - 1;
    state.dirty = true;
    renderAll();
  }

  function renderAll() {
    renderCanvas();
    renderProps();
    renderList();
  }

  // ---------- 保存 / 打印 ----------
  async function save() {
    if (!state.cur) return;
    if (!state.cur.name.trim()) {
      toast("给模板起个名字再存", "error");
      return;
    }
    try {
      await call("forms.save", { template: state.cur });
      state.dirty = false;
      await reload();
      toast("已保存「" + state.cur.name + "」");
    } catch (e) {
      toast("保存失败：" + errText(e), "error");
    }
  }

  async function printIt() {
    if (!state.cur) return;
    try {
      const r = await call("forms.print", { template: state.cur });
      toast("打印视图已生成，正在打开…");
      try {
        await call("app.openExport", { path: r.path });
      } catch (e2) {
        toast("文件已生成，但没能自动打开：" + r.path, "error");
      }
    } catch (e) {
      toast("生成打印视图失败：" + errText(e), "error");
    }
  }

  async function del() {
    if (!state.cur) return;
    if (String(state.cur.id).startsWith("builtin-")) {
      toast("内置模板删不掉 —— 它是程序自带的，你可以另存一份再改", "error");
      return;
    }
    if (!window.confirm("删掉模板「" + state.cur.name + "」？")) return;
    try {
      await call("forms.delete", { id: state.cur.id });
      state.cur = null;
      await reload();
      toast("已删掉");
    } catch (e) {
      toast("删除失败：" + errText(e), "error");
    }
  }

  async function reload() {
    try {
      const r = await call("forms.list", {});
      state.templates = (r && r.templates) || [];
    } catch (e) {
      state.templates = [];
    }
    renderAll();
  }

  // ---------- 初始化 ----------
  async function init() {
    const newBtn = $("forms-new");
    if (newBtn) newBtn.addEventListener("click", newBlank);
    const saveBtn = $("forms-save");
    if (saveBtn) saveBtn.addEventListener("click", save);
    const printBtn = $("forms-print");
    if (printBtn) printBtn.addEventListener("click", printIt);
    const delBtn = $("forms-del");
    if (delBtn) delBtn.addEventListener("click", del);

    try {
      const r = await call("forms.papers", {});
      state.papers = (r && r.papers) || [];
      state.builtin = (r && r.builtin) || [];
    } catch (e) {
      toast("读纸张与内置模板失败：" + errText(e), "error");
    }
    await reload();
  }

  // 侧栏/工具栏快捷键：Ctrl+S 存、Ctrl+P 打印
  document.addEventListener("keydown", (ev) => {
    const view = document.querySelector('.view[data-view="forms"][data-active="true"]');
    if (!view) return;
    if ((ev.ctrlKey || ev.metaKey) && ev.key.toLowerCase() === "s") {
      ev.preventDefault();
      save();
    }
    if ((ev.ctrlKey || ev.metaKey) && ev.key.toLowerCase() === "p") {
      ev.preventDefault();
      printIt();
    }
  });

  window.DeskBaseForms = { init: init };

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();
