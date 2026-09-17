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

  /** 从 localStorage 恢复一个下拉的值，并**校验它仍然是合法选项**。
   *
   *  为什么必须校验：给 <select> 赋一个不在 options 里的值时，浏览器不会报错，
   *  它只是悄悄退回第一个选项 —— 于是界面显示的和实际生效的就不是一回事。
   *  这个坑真踩过：旧版本存过 qingci / songyan 这些主题 id，主题表换掉之后
   *  设置页显示「跟随系统」，而 CSS 拿到的是一个已经不存在的主题名。 */
  function restoreSelect(sel, key, fallback) {
    const valid = Array.from(sel.options).map((o) => o.value);
    let v = store(key) || fallback;
    if (!valid.includes(v)) {
      v = fallback;
      store(key, v);   // 顺手把无效值清掉，下次不再出现
    }
    sel.value = v;
    return v;
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

  /** 主题切换时只允许颜色类属性过渡。
   *  不加这一步，一切换就有几百个元素同时开始做位移/缩放动画 —— 又卡又乱。 */
  function applyThemeSmoothly(fn) {
    root.classList.add("theme-switching");
    withTransition(fn);
    setTimeout(() => root.classList.remove("theme-switching"), 260);
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
    applyThemeSmoothly(() => applyTheme(themeSel.value));
  });

  // ============================================================
  // 自适应：侧栏档位 + 笔记单栏/双栏
  // ============================================================
  // 调研结论：必须区分「窗口变窄导致的自动折叠」与「用户手动折叠」。
  // 窗口变宽时只恢复前者，不要把用户自己关掉的面板强行打开。
  const NARROW = 720;       // 低于这个宽度侧栏变覆盖抽屉
  const COMPACT = 1024;     // 低于这个宽度侧栏默认收成图标条

  let sidebarPref = store("deskbase.sidebar") || "auto";   // auto | full | rail
  const sidebarBtn = $("#btn-sidebar");

  function isNarrow() {
    return window.innerWidth < NARROW;
  }

  function applySidebar() {
    if (isNarrow()) {
      // 窄屏：侧栏是覆盖抽屉，默认收起；按钮负责开合
      if (root.dataset.sidebar !== "open") root.dataset.sidebar = "hidden";
      sidebarBtn.title = root.dataset.sidebar === "open" ? "收起侧栏" : "展开侧栏";
      return;
    }
    delete root.dataset.sidebar;
    if (root.dataset.sidebarOpen === "1") return;
    if (sidebarPref === "rail") root.dataset.sidebar = "rail";
    else if (sidebarPref === "full") root.dataset.sidebar = "full";
    else root.dataset.sidebar = window.innerWidth < COMPACT ? "rail" : "full";
    sidebarBtn.title = root.dataset.sidebar === "rail" ? "展开侧栏" : "收起侧栏";
  }

  sidebarBtn.addEventListener("click", () => {
    const app = document.querySelector(".app");
    if (isNarrow()) {
      root.dataset.sidebar = root.dataset.sidebar === "open" ? "hidden" : "open";
      sidebarBtn.title = root.dataset.sidebar === "open" ? "收起侧栏" : "展开侧栏";
      return;
    }
    // 宽屏：在 完整 / 图标条 之间切，并记住用户的选择
    const now = root.dataset.sidebar === "rail" ? "full" : "rail";
    sidebarPref = now;
    store("deskbase.sidebar", now);
    // 侧栏宽度变化会让内容区重排；限定范围，别让整页跟着重算
    app.classList.add("is-animating-layout");
    flip(".main", () => {
      root.dataset.sidebar = now;
    });
    sidebarBtn.title = now === "rail" ? "展开侧栏" : "收起侧栏";
    setTimeout(() => {
      app.classList.remove("is-animating-layout");
      movePill(document.querySelector('.nav-item[aria-current="true"]'));
    }, 320);
  });

  // 点击抽屉外的区域收起抽屉（窄屏）
  document.querySelector(".content").addEventListener("click", () => {
    if (isNarrow() && root.dataset.sidebar === "open") {
      root.dataset.sidebar = "hidden";
    }
  });

  // 笔记面板：窄屏时是单栏，列表 ⇄ 编辑之间切
  const notesEl = $("#notes");
  function setPane(which) {
    if (!notesEl) return;
    notesEl.dataset.pane = which;
  }
  $("#btn-pane-back").addEventListener("click", () => setPane("list"));

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

  /** 动效档位：系统「减少动效」是**封顶**而不是覆盖。
   *
   *  之前只在启动时判断一次，而且仅当存的是 "standard" 才降级 —— 存了「丰富」
   *  的用户开了系统减少动效也照样满屏动。而且系统设置运行中改了不会生效。
   *  现在：effective = min(用户选择, 精简)（当系统要求减少动效时），
   *  并且监听系统设置变化。用户的选择不被覆盖，系统设置改回去就恢复。 */
  const TIERS = ["off", "minimal", "standard", "rich"];
  const reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)");

  function effectiveMotion(stored) {
    if (!reduceMotion.matches) return stored;
    return TIERS.indexOf(stored) > TIERS.indexOf("minimal") ? "minimal" : stored;
  }

  function applyMotion(stored) {
    const eff = effectiveMotion(stored);
    root.dataset.motion = eff;
    motionSel.value = eff;
    const hint = $("#motion-hint");
    if (hint) {
      hint.textContent = reduceMotion.matches
        ? "动效服务于理解，不做循环播放的装饰动画。检测到系统开启了「减少动效」，已封顶为「精简」（你的选择没被改掉，系统设置改回去就恢复）。"
        : "动效服务于理解，不做循环播放的装饰动画。调到「关」会立刻停掉全部过渡与位移。";
    }
  }

  const motionSel = bindSelect("#motion-select", "deskbase.motion", (v) => {
    applyMotion(v);
    if (v === "off") toast("动效已关闭");
  }, "standard");

  // 系统设置随时可能变，必须监听（事件驱动，不轮询）
  reduceMotion.addEventListener("change", () => {
    applyMotion(store("deskbase.motion") || "standard");
  });

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
    // 指示块是固定高度（与导航项同高），所以只需要动 translateY —— 不改 height
    const base = navEl.getBoundingClientRect();
    const r = btn.getBoundingClientRect();
    pillEl.style.transform = `translateY(${r.top - base.top}px)`;
  }

  /**
   * FLIP：先量位置 → 执行改动 → 再量 → 用 transform 把差值补回去 → 过渡到 0。
   *
   * 为什么不直接 transition grid-template-columns：那是布局属性，浏览器每帧
   * 都要重算几何。FLIP 把"动画"从布局挪到合成层 —— 宽度只瞬间变一次（重排一次），
   * 之后动的只有 transform。
   */
  function flip(target, mutate) {
    const el = typeof target === "string" ? document.querySelector(target) : target;
    if (!el) {
      mutate();
      return;
    }
    const before = el.getBoundingClientRect();
    mutate();
    const dx = before.left - el.getBoundingClientRect().left;
    if (!dx) return;
    el.style.transition = "none";
    el.style.transform = `translateX(${dx}px)`;
    requestAnimationFrame(() => {
      el.style.transition = `transform var(--dur-normal) var(--ease-glide)`;
      el.style.transform = "";
      setTimeout(() => {
        el.style.transition = "";
      }, 320);
    });
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
    // 三个筛选项宽度不同，所以宽度得变 —— 但用 scaleX 而不是 width。
    // 基准宽度与 theme.css 里 .filter-pill 的 --pill-base 必须一致。
    const baseW = parseFloat(getComputedStyle(filterPill).getPropertyValue("--pill-base")) || 100;
    filterPill.style.transform =
      `translateX(${r.left - base.left - 3}px) scaleX(${(r.width / baseW).toFixed(4)})`;
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
      // 窄屏单栏：打开一条笔记就切到编辑区（返回按钮在编辑区左上）
      if (window.innerWidth < 720) setPane("editor");
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

  async function refreshAudit() {
    try {
      const a = await call("audit.tail");
      $("#audit-count").textContent = `已打开 ${a.opened} 次 · 已拒绝 ${a.denied} 次`;
    } catch (e) {
      /* 审计摘要拿不到不影响任何功能，静默即可 */
    }
  }

  // ============================================================
  // 启动
  // ============================================================
  async function boot() {
    // 主题（默认宣纸；D-016 决定不让"跟随系统"当默认，保证用户第一眼看到宣纸）
    applyTheme(restoreSelect(themeSel, "deskbase.theme", "xuan"));

    // 质感
    root.dataset.texture = restoreSelect(textureSel, "deskbase.texture", "light");

    // 动效：默认「标准」；系统要求减少动效时封顶为「精简」并如实说明
    applyMotion(restoreSelect(motionSel, "deskbase.motion", "standard"));

    // 标题字体
    applyHeading(restoreSelect(headingSel, "deskbase.heading", "display"));
    checkEmbeddedFont();

    // 筛选条只建一次
    buildFilters();

    try {
      const info = await call("app.info");
      $("#about-version").textContent = info.version;
      $("#about-version-2").textContent = info.version;
      $("#update-current").textContent = info.version;
      $("#about-datadir").textContent = info.dataDir;
      $("#about-datadir-2").textContent = info.dataDir;
      $("#about-count").textContent = String(info.noteCount);

      // 仓库地址与文档链接。地址由 Rust 侧的常量给出，前端只负责显示与打开。
      const repo = info.repo || "";
      const repoLink = $("#repo-link");
      const docsLink = $("#docs-link");
      repoLink.textContent = repo || "尚未设置";
      // 占位地址在界面上明说，避免用户点进一个不存在的仓库
      const isPlaceholder = /deskbase-app\/deskbase$/.test(repo);
      $("#repo-tbd").hidden = !isPlaceholder;
      if (isPlaceholder) {
        $("#repo-hint").textContent =
          "这是一个占位地址，仓库公开前需要改成真实地址（见 app/src/main.rs 的 PROJECT_REPO）。";
      }
      const openLink = async (url) => {
        try {
          await call("app.openExternal", { url });
          refreshAudit();
        } catch (e) {
          toast(e.message, "error");
        }
      };
      repoLink.addEventListener("click", (e) => {
        e.preventDefault();
        openLink(repo);
      });
      docsLink.addEventListener("click", (e) => {
        e.preventDefault();
        openLink(repo + "/tree/main/docs");
      });
      $("#btn-check-update").addEventListener("click", () => openLink(repo + "/releases"));
      refreshAudit();
    } catch (e) {
      toast("初始化失败：" + e.message, "error");
    }
    await refreshList();
    showView("notes", { keepList: true });
    applySidebar();

    // 窗口尺寸变化时：指示块与侧栏档位都要跟着走。
    // 用 rAF 合并，避免拖动窗口边缘时每一像素都算一次布局
    let resizeRaf = 0;
    window.addEventListener("resize", () => {
      if (resizeRaf) return;
      resizeRaf = requestAnimationFrame(() => {
        resizeRaf = 0;
        applySidebar();
        movePill(document.querySelector('.nav-item[aria-current="true"]'));
        moveFilterPill();
        // 从窄屏回到宽屏时，把笔记还原成双栏
        if (window.innerWidth >= 720) setPane("list");
      });
    });
  }

  // 关闭前尽力保存（窗口关闭不保证能走完，主要靠输入时的自动保存）
  window.addEventListener("beforeunload", () => {
    if (dirty) flushSave();
  });

  boot();
})();
