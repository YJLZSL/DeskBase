/* ============================================================
   DeskBase 数据库页 · 空态引导（Onboarding）
   ============================================================
   目标：没有表的时候，左栏不再只有一句话，而是给一段引导步骤 +
   一个「创建示例台账」按钮，让用户立刻看到"哦，就是这样"。

   为什么是独立组件、不进 db.js：
     db.js 由另一位同事在并行改动，本文件刻意不碰它，避免互相覆盖。
     db.js 在 #db-table-list 为空时会渲染一个 `.db-empty` 占位块；本组件
     用 MutationObserver 盯着这个列表 —— 一旦它被渲染成"空"，就把引导
     块填进去；一旦有了表（或用户点了创建），引导块自然消失，互不打架。

   「创建示例台账」的行为（都来自已实现能力）：
     · 调已有的 IPC schema.createTable 建一张示例表
       （名称(文本) / 数量(整数) / 金额(金额) / 日期(日期) / 已结清(是-否)）
     · 建完自动打开它：刷新表列表后，找到这张表的条目并"点一下"，
       等于走 db.js 既有的 openTable 路径（不自己造一份打开逻辑）
     · 只在用户主动点的时候建；启动时绝不偷偷建

   样式走 db-onboard.css（自注入，带重复加载保护）。
   ============================================================ */
(function () {
  "use strict";

  if (window.DeskBaseOnboard) return;

  const SAMPLE_NAME = "示例台账";

  // 示例表的列。键名严格对齐 db.js 里 schema.createTable 的入参：
  // { name, ty, not_null, default, primary_key, comment }
  const SAMPLE_COLUMNS = [
    { name: "名称", ty: "text", not_null: false, default: null, primary_key: false, comment: null },
    { name: "数量", ty: "integer", not_null: false, default: null, primary_key: false, comment: null },
    { name: "金额", ty: "money", not_null: false, default: null, primary_key: false, comment: null },
    { name: "日期", ty: "date", not_null: false, default: null, primary_key: false, comment: null },
    { name: "已结清", ty: "boolean", not_null: false, default: null, primary_key: false, comment: null },
  ];

  const SELF_SRC = (function () {
    const s = document.currentScript && document.currentScript.src;
    if (s) return s;
    const links = document.getElementsByTagName("script");
    for (let i = links.length - 1; i >= 0; i--) {
      if (/db-onboard\.js($|\?)/.test(links[i].src || "")) return links[i].src;
    }
    return null;
  })();

  const STYLE_ID = "dbonboard-styles";

  function injectStyles() {
    if (document.getElementById(STYLE_ID)) return;
    try {
      if (document.querySelector('link[rel="stylesheet"][href$="db-onboard.css"]')) return;
    } catch (e) {}
    let href = "db-onboard.css";
    try {
      href = new URL("db-onboard.css", SELF_SRC || document.baseURI).href;
    } catch (e) {}
    const link = document.createElement("link");
    link.id = STYLE_ID;
    link.rel = "stylesheet";
    link.href = href;
    link.addEventListener("error", () => {
      console.error(
        "[DeskBaseOnboard] db-onboard.css 加载失败：" + href +
          "\n  需在 app/src/assets.rs 的 lookup() 登记 /db-onboard.css。"
      );
    });
    document.head.appendChild(link);
  }

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

  function call(cmd, args) {
    const bridge = window.__deskbase;
    if (!bridge || typeof bridge.call !== "function") {
      return Promise.reject(new Error("IPC 桥不可用（app.js 未完成加载？）"));
    }
    return bridge.call(cmd, args);
  }

  function toast(text, kind) {
    const U = window.DeskBaseUI;
    if (U && typeof U.toast === "function") {
      U.toast(text, kind ? { kind: kind } : undefined);
    } else {
      console.log("[DeskBaseOnboard] " + text);
    }
  }

  // ---------- 引导块 ----------
  let guidance = null; // 单例 DOM，随列表的空/非空在 db.js 的占位里搬进搬出

  function buildGuidance() {
    const box = el("div", { class: "db-onboard" });
    box.appendChild(
      el("p", { class: "db-onboard-lead" },
        "还没有表格。跟着三步建第一张：")
    );
    const ol = el("ol", { class: "db-onboard-steps" });
    [
      "点上方「新建表格」，给表起名、加列。",
      "建好后切到「数据」页签，在表尾那行录数据。",
      "看不懂？读设置页底部的「办公套件使用教程」。",
    ].forEach((t) => ol.appendChild(el("li", null, t)));
    box.appendChild(ol);

    const btn = el("button", { class: "btn btn-primary db-onboard-btn", type: "button" },
      "创建示例台账");
    btn.addEventListener("click", () => createSample(btn));
    box.appendChild(btn);
    return box;
  }

  // ---------- 创建示例表并自动打开 ----------
  async function createSample(btn) {
    if (btn.disabled) return;
    btn.disabled = true;
    const old = btn.textContent;
    btn.textContent = "创建中…";
    try {
      const tables = await call("schema.listTables");
      const exists = Array.isArray(tables) && tables.some((t) => t.name === SAMPLE_NAME);
      if (!exists) {
        await call("schema.createTable", {
          spec: { name: SAMPLE_NAME, comment: "示例台账（可随时删除）", columns: SAMPLE_COLUMNS },
        });
      }
      // 刷新表列表（db.js 的 renderTableList 会重建列表，引导块随之消失）
      if (window.DeskBaseDb && typeof window.DeskBaseDb.refreshTables === "function") {
        await window.DeskBaseDb.refreshTables();
      }
      // 自动打开：点击列表里这张表的条目，复用 db.js 既有的打开逻辑
      const items = document.querySelectorAll("#db-table-list .db-table-item");
      for (const it of items) {
        const t = it.querySelector(".t");
        if (t && t.textContent === SAMPLE_NAME) {
          it.click();
          break;
        }
      }
      toast(exists ? "已打开「" + SAMPLE_NAME + "」" : "已创建并打开「" + SAMPLE_NAME + "」");
    } catch (e) {
      toast("创建示例台账失败：" + errText(e), "error");
    } finally {
      btn.disabled = false;
      btn.textContent = old;
    }
  }

  // ---------- 同步：没有表时，把引导块放到**主区域** ----------
  //
  // 为什么放主区域而不是侧栏：没有表的时候，**主区域整片是空的**，
  // 而侧栏只有 236px 宽。把三步引导塞在侧栏里会被挤成七八行、
  // 底部还被裁掉（走查截图上看得清清楚楚）——
  // 该放"下一步做什么"的地方本来是那块空地，不是那条窄缝。
  function sync(listEl) {
    if (!listEl) return;
    const hasItems = !!listEl.querySelector(".db-table-item");
    const pane = document.getElementById("db-pane-grid");
    if (!pane) return;

    if (hasItems) {
      // 有表了 → 收回引导。**保留单例**，用户把表全删了还能再用。
      if (guidance && guidance.parentElement) {
        guidance.parentElement.removeChild(guidance);
      }
      if (pane.dataset.onboard === "1") {
        pane.textContent = "";
        delete pane.dataset.onboard;
      }
      return;
    }

    // 没有表 → 引导住进主区域
    if (!guidance) guidance = buildGuidance();
    // 已经在里面了就别重复搬（MutationObserver 会被自己的改动再次触发）
    if (guidance.parentElement === pane) return;
    // 主区域此时理应没有内容。万一有（比如网格还没加载完留下的占位），
    // **不动它** —— 那是真的错误提示，比引导重要。
    if (pane.firstElementChild) return;
    pane.dataset.onboard = "1";
    pane.appendChild(guidance);
  }

  function init() {
    injectStyles();
    const listEl = document.getElementById("db-table-list");
    if (!listEl) return;

    // 列表内容由 db.js 在用户首次进入数据库页时才填充（懒加载），
    // 所以用 MutationObserver 盯着它：空 → 有表 → 空 都能正确响应。
    const mo = new MutationObserver(() => sync(listEl));
    mo.observe(listEl, { childList: true, subtree: true });
    sync(listEl);
  }

  window.DeskBaseOnboard = { init: init };

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();
