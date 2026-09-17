/* ============================================================
   DeskBase 前端
   ============================================================
   与 Rust 的通信全部走 window.ipc.postMessage。
   前端不直接碰文件系统与数据库 —— 所有能力都要经 IPC 显式请求。
   ============================================================ */
(function () {
  "use strict";

  // ---------- IPC 桥 ----------
  let seq = 0;
  const pending = new Map();

  window.__deskbase = {
    resolve(payload) {
      let msg;
      try {
        msg = typeof payload === "string" ? JSON.parse(payload) : payload;
      } catch (e) {
        return;
      }
      const p = pending.get(msg.id);
      if (!p) return;
      pending.delete(msg.id);
      if (msg.ok) p.resolve(msg.data);
      else p.reject(new Error(msg.error || "未知错误"));
    },
  };

  function call(cmd, args) {
    return new Promise((resolve, reject) => {
      const id = ++seq;
      pending.set(id, { resolve, reject });
      setTimeout(() => {
        if (pending.has(id)) {
          pending.delete(id);
          reject(new Error("请求超时：" + cmd));
        }
      }, 15000);
      window.ipc.postMessage(JSON.stringify({ id, cmd, args: args || {} }));
    });
  }

  // ---------- 小工具 ----------
  const $ = (sel) => document.querySelector(sel);
  const root = document.documentElement;

  const toastEl = $("#toast");
  let toastTimer = null;
  function toast(text, kind) {
    toastEl.textContent = text;
    toastEl.dataset.kind = kind === "error" ? "error" : "info";
    toastEl.dataset.show = "true";
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => {
      toastEl.dataset.show = "false";
    }, 2400);
  }

  function fmtTime(ms) {
    if (!ms) return "";
    const d = new Date(ms);
    const p = (n) => String(n).padStart(2, "0");
    const today = new Date();
    const sameDay =
      d.getFullYear() === today.getFullYear() &&
      d.getMonth() === today.getMonth() &&
      d.getDate() === today.getDate();
    const hm = p(d.getHours()) + ":" + p(d.getMinutes());
    if (sameDay) return hm;
    return p(d.getMonth() + 1) + "-" + p(d.getDate()) + " " + hm;
  }

  function store(key, val) {
    try {
      if (val === undefined) return localStorage.getItem(key);
      localStorage.setItem(key, val);
    } catch (e) {}
    return null;
  }

  /** 有 View Transition API 就用它做整屏交叉淡入（切主题最怕"啪"地闪一下），
   *  没有就退回直接执行 —— 不影响正确性。 */
  function withTransition(fn) {
    if (document.startViewTransition && root.dataset.motion !== "off") {
      document.startViewTransition(fn);
    } else {
      fn();
    }
  }

  // ============================================================
  // 主题：9 个内置 + 跟随系统
  // ============================================================
  const LIGHT_FALLBACK = "xuan";   // 跟随系统时，系统为亮色用「宣纸」
  const DARK_FALLBACK = "yemo";    // 系统为暗色用「夜墨」
  const darkQuery = window.matchMedia("(prefers-color-scheme: dark)");

  function resolveTheme(name) {
    if (name !== "auto") return name;
    return darkQuery.matches ? DARK_FALLBACK : LIGHT_FALLBACK;
  }

  function applyTheme(name) {
    root.dataset.theme = resolveTheme(name);
    store("deskbase.theme", name);
  }

  // 跟随系统时要实时响应，事件驱动、不轮询（docs/18 要求）
  darkQuery.addEventListener("change", () => {
    if (store("deskbase.theme") === "auto") applyTheme("auto");
  });

  const themeSel = $("#theme-select");
  themeSel.addEventListener("change", () => {
    withTransition(() => applyTheme(themeSel.value));
  });

  // ============================================================
  // 质感强度 / 动效档位 / 标题字体
  // ============================================================
  function bindSelect(sel, key, apply, fallback) {
    const el = $(sel);
    if (!el) return null;
    el.addEventListener("change", () => {
      apply(el.value);
      store(key, el.value);
    });
    el._fallback = fallback;
    return el;
  }

  const textureSel = bindSelect("#texture-select", "deskbase.texture", (v) => {
    root.dataset.texture = v;
  }, "light");

  const motionSel = bindSelect("#motion-select", "deskbase.motion", (v) => {
    root.dataset.motion = v;
    if (v === "off") toast("动效已关闭");
  }, "standard");

  /** 标题字体：data-heading="serif" 才切回宋体系；默认（得意黑）时去掉属性，
   *  让 CSS 走 :root 的默认值 —— 纯 CSS 变量切换，不重新加载任何资源。 */
  function applyHeading(v) {
    if (v === "serif") root.dataset.heading = "serif";
    else root.removeAttribute("data-heading");
  }
  const headingSel = bindSelect("#heading-select", "deskbase.heading", applyHeading, "display");

  // 内置字体到底加载成功没有 —— 这是对「自定义协议 + 内嵌字体」整条链路的运行时自检。
  // docs/18 5.5 要求：加载失败必须能看出来，而不是悄悄回退。
  async function checkEmbeddedFont() {
    const hint = $("#heading-hint");
    if (!hint || !document.fonts) return;
    try {
      await document.fonts.load('400 16px "Smiley Sans Oblique"', "桌库");
      if (!document.fonts.check('400 16px "Smiley Sans Oblique"')) {
        hint.dataset.fontState = "failed";
        hint.textContent += "（内置字体未能加载，标题已回退到系统字体）";
      }
    } catch (e) {
      hint.dataset.fontState = "failed";
      hint.textContent += "（内置字体加载出错：" + e.message + "）";
    }
  }

  // ============================================================
  // 视图切换 + 导航滑动指示块
  // ============================================================
  const VIEW_ORDER = ["workbench", "notes", "database", "settings"];
  const TITLES = {
    workbench: "工作台",
    notes: "笔记",
    database: "数据库",
    settings: "设置",
  };
  let currentView = "notes";

  const navEl = $("#nav");
  const pillEl = $("#nav-pill");

  function movePill(btn) {
    if (!btn || !pillEl) return;
    // 用 transform 移动，不动 top/left —— 只触发合成，不触发重排
    const base = navEl.getBoundingClientRect();
    const r = btn.getBoundingClientRect();
    pillEl.style.height = r.height + "px";
    pillEl.style.transform = `translateY(${r.top - base.top - 0}px)`;
  }

  function showView(name, opts) {
    const from = VIEW_ORDER.indexOf(currentView);
    const to = VIEW_ORDER.indexOf(name);
    // 往下翻时内容从下方滑入，往上翻时从上方滑入 —— 方向感来自这里
    root.style.setProperty("--dir", to >= from ? "1" : "-1");

    currentView = name;
    VIEW_ORDER.forEach((v) => {
      const el = document.querySelector('[data-view="' + v + '"]');
      if (el) el.dataset.active = v === name ? "true" : "false";
    });
    let activeBtn = null;
    document.querySelectorAll(".nav-item").forEach((b) => {
      const on = b.dataset.target === name;
      b.setAttribute("aria-current", on ? "true" : "false");
      if (on) activeBtn = b;
    });
    $("#page-title").textContent = TITLES[name] || name;
    movePill(activeBtn);

    // 「删除 / 新建」是笔记页的操作，出现在别的页面上会让人以为按了会出事
    const noteOnly = name === "notes";
    $("#btn-delete-note").hidden = !noteOnly;
    $("#btn-new-note").hidden = !noteOnly;
    $("#search-wrap").hidden = !noteOnly;

    // 换页后让列表重新做一次逐项进场
    if (noteOnly && !(opts && opts.keepList)) renderNoteList();
  }

  document.querySelectorAll(".nav-item").forEach((btn) => {
    btn.addEventListener("click", () => showView(btn.dataset.target));
  });

  // ============================================================
  // 笔记
  // ============================================================
  let allNotes = [];   // 列表缓存：id / title / updated_at / excerpt
  let currentId = null;
  let dirty = false;
  let saveTimer = null;

  const listEl = $("#note-list");
  const titleEl = $("#note-title");
  const bodyEl = $("#note-body");
  const stateEl = $("#save-state");
  const searchEl = $("#search-input");

  // 筛选：全部 / 有内容 / 空笔记。纯客户端，不额外打库。
  let filter = "all";
  let keyword = "";

  const FILTERS = [
    { id: "all", label: "全部" },
    { id: "filled", label: "有内容" },
    { id: "blank", label: "空笔记" },
  ];

  const filtersEl = $("#filters");
  const filterPill = $("#filter-pill");

  function moveFilterPill() {
    if (!filtersEl || !filterPill) return;
    const active = filtersEl.querySelector('.filter[aria-selected="true"]');
    if (!active) return;
    const base = filtersEl.getBoundingClientRect();
    const r = active.getBoundingClientRect();
    filterPill.style.width = r.width + "px";
    filterPill.style.transform = `translateX(${r.left - base.left - 3}px)`;
  }

  /** 筛选条只建一次 —— 每次渲染重建会让滑块动画每次都从头开始，还会闪。 */
  function buildFilters() {
    if (!filtersEl) return;
    FILTERS.forEach((f) => {
      const b = document.createElement("button");
      b.className = "filter";
      b.dataset.filter = f.id;
      b.textContent = f.label;
      b.setAttribute("role", "tab");
      b.setAttribute("aria-selected", f.id === filter ? "true" : "false");
      b.addEventListener("click", () => {
        filter = f.id;
        filtersEl.querySelectorAll(".filter").forEach((x) => {
          x.setAttribute("aria-selected", x.dataset.filter === filter ? "true" : "false");
        });
        moveFilterPill();
        renderNoteList();
      });
      filtersEl.appendChild(b);
    });
    // 首帧摆好滑块（此时元素才有尺寸）
    requestAnimationFrame(moveFilterPill);
  }

  function matches(n) {
    if (filter === "filled" && !n.excerpt) return false;
    if (filter === "blank" && n.excerpt) return false;
    if (keyword) {
      const k = keyword.toLowerCase();
      const hay = (n.title + " " + n.excerpt).toLowerCase();
      if (hay.indexOf(k) < 0) return false;
    }
    return true;
  }

  function setSaveState(state, text) {
    stateEl.dataset.state = state || "";
    stateEl.textContent = text || "";
  }

  async function refreshList() {
    try {
      allNotes = await call("note.list");
      renderNoteList();
    } catch (e) {
      toast("读取笔记列表失败：" + e.message, "error");
    }
  }

  function renderNoteList() {
    listEl.textContent = "";

    const shown = allNotes.filter(matches);
    if (!shown.length) {
      const d = document.createElement("div");
      d.className = "empty";
      const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
      svg.setAttribute("class", "ico");
      const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
      use.setAttribute("href", "#i-note");
      svg.appendChild(use);
      d.appendChild(svg);
      const h = document.createElement("h2");
      h.textContent = allNotes.length ? "没有符合条件的笔记" : "还没有笔记";
      const p = document.createElement("p");
      p.textContent = allNotes.length
        ? "换个筛选条件或清空搜索试试。"
        : "点右上角「新建」开始。内容会自动保存，退出也不会丢。";
      d.appendChild(h);
      d.appendChild(p);
      listEl.appendChild(d);
      return;
    }

    shown.forEach((n, i) => {
      const b = document.createElement("button");
      b.className = "note-item";
      b.dataset.id = n.id;
      // 逐项延迟：超过 12 条就不再累加，否则列表长了要等很久才显示完
      b.style.setProperty("--i", String(Math.min(i, 12)));
      if (n.id === currentId) b.setAttribute("aria-selected", "true");

      const t = document.createElement("span");
      t.className = "t";
      t.textContent = n.title || "（无标题）";
      const d = document.createElement("span");
      d.className = "d";
      d.textContent = fmtTime(n.updated_at);
      b.appendChild(t);
      b.appendChild(d);
      if (n.excerpt) {
        const ex = document.createElement("span");
        ex.className = "excerpt";
        ex.textContent = n.excerpt.slice(0, 48);
        b.appendChild(ex);
      }

      b.addEventListener("click", () => openNote(n.id));
      listEl.appendChild(b);
    });
  }

  async function openNote(id) {
    if (dirty) await flushSave();
    try {
      const n = await call("note.get", { id });
      currentId = n.id;
      titleEl.value = n.title;
      bodyEl.value = n.content;
      dirty = false;
      setSaveState("", "");
      markSelected(id);
    } catch (e) {
      toast("打开笔记失败：" + e.message, "error");
    }
  }

  function markSelected(id) {
    listEl.querySelectorAll(".note-item").forEach((el) => {
      if (el.dataset.id === id) el.setAttribute("aria-selected", "true");
      else el.removeAttribute("aria-selected");
    });
  }

  async function flushSave() {
    if (!currentId || !dirty) return;
    try {
      setSaveState("saving", "保存中…");
      const r = await call("note.save", {
        id: currentId,
        title: titleEl.value,
        content: bodyEl.value,
      });
      dirty = false;
      setSaveState("saved", "已保存");
      const rec = allNotes.find((x) => x.id === currentId);
      if (rec) {
        rec.title = titleEl.value;
        rec.updated_at = r.updatedAt;
        // 摘要就地更新，免得整列表重绘打断输入
        rec.excerpt = (bodyEl.value || "").replace(/\s+/g, " ").slice(0, 240).trim();
      }
      const item = listEl.querySelector('.note-item[data-id="' + currentId + '"]');
      if (item) {
        item.querySelector(".t").textContent = titleEl.value || "（无标题）";
        item.querySelector(".d").textContent = fmtTime(r.updatedAt);
        let ex = item.querySelector(".excerpt");
        if (rec && rec.excerpt) {
          if (!ex) {
            ex = document.createElement("span");
            ex.className = "excerpt";
            item.appendChild(ex);
          }
          ex.textContent = rec.excerpt.slice(0, 48);
        } else if (ex) {
          ex.remove();
        }
      }
    } catch (e) {
      setSaveState("error", "保存失败");
      toast("保存失败：" + e.message, "error");
    }
  }

  function scheduleSave() {
    dirty = true;
    setSaveState("", "未保存");
    clearTimeout(saveTimer);
    saveTimer = setTimeout(flushSave, 1200);
  }

  titleEl.addEventListener("input", scheduleSave);
  bodyEl.addEventListener("input", scheduleSave);
  titleEl.addEventListener("blur", flushSave);
  bodyEl.addEventListener("blur", flushSave);

  // 搜索：输入防抖 + 结果重新做逐项进场
  let searchTimer = null;
  searchEl.addEventListener("input", () => {
    clearTimeout(searchTimer);
    searchTimer = setTimeout(() => {
      keyword = searchEl.value.trim();
      renderNoteList();
    }, 140);
  });
  searchEl.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      searchEl.value = "";
      keyword = "";
      renderNoteList();
      searchEl.blur();
    }
  });
  // Ctrl/Cmd+K 聚焦搜索（命令面板的前身）
  window.addEventListener("keydown", (e) => {
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k") {
      e.preventDefault();
      showView("notes");
      searchEl.focus();
      searchEl.select();
    }
  });

  $("#btn-new-note").addEventListener("click", async () => {
    try {
      if (dirty) await flushSave();
      const n = await call("note.create", { title: "未命名笔记" });
      await refreshList();
      await openNote(n.id);
      titleEl.focus();
      titleEl.select();
    } catch (e) {
      toast("新建失败：" + e.message, "error");
    }
  });

  $("#btn-delete-note").addEventListener("click", async () => {
    if (!currentId) {
      toast("先选中一条笔记");
      return;
    }
    try {
      await call("note.delete", { id: currentId });
      currentId = null;
      titleEl.value = "";
      bodyEl.value = "";
      dirty = false;
      setSaveState("", "");
      await refreshList();
      toast("已移入回收站");
    } catch (e) {
      toast("删除失败：" + e.message, "error");
    }
  });

  // ============================================================
  // 启动
  // ============================================================
  async function boot() {
    // 主题（默认宣纸；D-016 决定不让"跟随系统"当默认，保证用户第一眼看到宣纸）
    const saved = store("deskbase.theme") || "xuan";
    themeSel.value = saved;
    applyTheme(saved);

    // 质感
    const tex = store("deskbase.texture") || "light";
    textureSel.value = tex;
    root.dataset.texture = tex;

    // 动效：默认「标准」；若系统要求减少动效，则降级为「精简」并如实显示
    let mo = store("deskbase.motion") || "standard";
    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches && !store("deskbase.motion")) {
      mo = "minimal";
      const hint = $("#motion-hint");
      if (hint) hint.textContent += "（检测到系统「减少动效」，已自动降为「精简」）";
    }
    motionSel.value = mo;
    root.dataset.motion = mo;

    // 标题字体
    const hd = store("deskbase.heading") || "display";
    headingSel.value = hd;
    applyHeading(hd);
    checkEmbeddedFont();

    // 筛选条只建一次
    buildFilters();

    try {
      const info = await call("app.info");
      $("#about-version").textContent = info.version;
      $("#about-version-2").textContent = info.version;
      $("#about-datadir").textContent = info.dataDir;
      $("#about-datadir-2").textContent = info.dataDir;
      $("#about-count").textContent = String(info.noteCount);
    } catch (e) {
      toast("初始化失败：" + e.message, "error");
    }
    await refreshList();
    showView("notes", { keepList: true });

    // 窗口尺寸变化时指示块要跟着走（它靠像素位置定位）
    window.addEventListener("resize", () => {
      movePill(document.querySelector('.nav-item[aria-current="true"]'));
      moveFilterPill();
    });
  }

  // 关闭前尽力保存（窗口关闭不保证能走完，主要靠输入时的自动保存）
  window.addEventListener("beforeunload", () => {
    if (dirty) flushSave();
  });

  boot();
})();
