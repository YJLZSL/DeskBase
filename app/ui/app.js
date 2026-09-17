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
      // 15 秒兜底，避免请求石沉大海
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
    return (
      d.getFullYear() +
      "-" +
      p(d.getMonth() + 1) +
      "-" +
      p(d.getDate()) +
      " " +
      p(d.getHours()) +
      ":" +
      p(d.getMinutes())
    );
  }

  // ---------- 视图切换 ----------
  const views = ["workbench", "notes", "database", "settings"];
  const titles = {
    workbench: "工作台",
    notes: "笔记",
    database: "数据库",
    settings: "设置",
  };

  function showView(name) {
    views.forEach((v) => {
      const el = document.querySelector('[data-view="' + v + '"]');
      if (el) el.dataset.active = v === name ? "true" : "false";
    });
    document.querySelectorAll(".nav-item").forEach((b) => {
      b.setAttribute("aria-current", b.dataset.target === name ? "true" : "false");
    });
    $("#page-title").textContent = titles[name] || name;
  }

  document.querySelectorAll(".nav-item").forEach((btn) => {
    btn.addEventListener("click", () => showView(btn.dataset.target));
  });

  // ---------- 笔记 ----------
  let currentId = null;
  let dirty = false;
  let saveTimer = null;

  const listEl = $("#note-list");
  const titleEl = $("#note-title");
  const bodyEl = $("#note-body");
  const stateEl = $("#save-state");

  function setSaveState(state, text) {
    stateEl.dataset.state = state || "";
    stateEl.textContent = text || "";
  }

  async function refreshList() {
    try {
      const notes = await call("note.list");
      listEl.innerHTML = "";
      if (!notes.length) {
        const d = document.createElement("div");
        d.className = "empty";
        d.innerHTML =
          "<h2>还没有笔记</h2><p>点右上角「新建」开始。</p>";
        listEl.appendChild(d);
        return;
      }
      for (const n of notes) {
        const b = document.createElement("button");
        b.className = "note-item";
        b.dataset.id = n.id;
        if (n.id === currentId) b.setAttribute("aria-selected", "true");

        const t = document.createElement("span");
        t.className = "t";
        t.textContent = n.title || "（无标题）";
        const d = document.createElement("span");
        d.className = "d";
        d.textContent = fmtTime(n.updated_at);
        b.appendChild(t);
        b.appendChild(d);

        b.addEventListener("click", () => openNote(n.id));
        listEl.appendChild(b);
      }
    } catch (e) {
      toast("读取笔记列表失败：" + e.message, "error");
    }
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
      // 只更新当前项的标题与时间，不整列表重绘（避免打断输入）
      const item = listEl.querySelector('.note-item[data-id="' + currentId + '"]');
      if (item) {
        item.querySelector(".t").textContent = titleEl.value || "（无标题）";
        item.querySelector(".d").textContent = fmtTime(r.updatedAt);
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
  // 失焦立即落盘，不等防抖
  titleEl.addEventListener("blur", flushSave);
  bodyEl.addEventListener("blur", flushSave);

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

  // ---------- 主题 ----------
  const themeSel = $("#theme-select");
  function applyTheme(name) {
    document.documentElement.dataset.theme = name;
    try {
      localStorage.setItem("deskbase.theme", name);
    } catch (e) {}
  }
  themeSel.addEventListener("change", () => applyTheme(themeSel.value));

  // ---------- 标题字体 ----------
  // display = 内置的得意黑（默认）；serif = 退回系统宋体系。
  // 走 CSS 变量切换，不重新加载任何资源，所以是即时的（见 theme.css 的 [data-heading] 规则）
  const headingSel = $("#heading-select");
  function applyHeadingFont(name) {
    if (name === "serif") document.documentElement.dataset.heading = "serif";
    else delete document.documentElement.dataset.heading;
    try {
      localStorage.setItem("deskbase.heading", name);
    } catch (e) {}
  }
  headingSel.addEventListener("change", () => applyHeadingFont(headingSel.value));

  // 内置字体到底加载成功没有 —— 这是对「自定义协议 + 内嵌字体」整条链路的运行时自检。
  // docs/18 5.5 要求：加载失败必须能看出来，而不是悄悄回退。
  async function checkEmbeddedFont() {
    const hint = document.querySelector("#heading-select")?.closest(".field")?.querySelector(".hint");
    if (!hint || !document.fonts) return;
    try {
      await document.fonts.load('400 16px "Smiley Sans Oblique"', "桌库");
      if (document.fonts.check('400 16px "Smiley Sans Oblique"')) {
        hint.dataset.fontState = "ok";
      } else {
        hint.dataset.fontState = "failed";
        hint.textContent += "（内置字体未能加载，标题已回退到系统字体）";
      }
    } catch (e) {
      hint.dataset.fontState = "failed";
      hint.textContent += "（内置字体加载出错：" + e.message + "）";
    }
  }

  // ---------- 启动 ----------
  async function boot() {
    // 恢复主题
    let saved = "xuan";
    try {
      saved = localStorage.getItem("deskbase.theme") || "xuan";
    } catch (e) {}
    themeSel.value = saved;
    applyTheme(saved);

    // 恢复标题字体
    let savedHeading = "display";
    try {
      savedHeading = localStorage.getItem("deskbase.heading") || "display";
    } catch (e) {}
    headingSel.value = savedHeading;
    applyHeadingFont(savedHeading);
    checkEmbeddedFont();

    try {
      const info = await call("app.info");
      $("#about-version").textContent = info.version;
      $("#about-datadir").textContent = info.dataDir;
      $("#about-count").textContent = String(info.noteCount);
    } catch (e) {
      toast("初始化失败：" + e.message, "error");
    }
    await refreshList();
    showView("notes");
  }

  // 关闭前尽力保存（窗口关闭不保证能走完，主要靠输入时的自动保存）
  window.addEventListener("beforeunload", () => {
    if (dirty) flushSave();
  });

  boot();
})();
