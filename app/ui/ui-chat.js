/* ============================================================
   DeskBase · AI 对话面板（ui-chat.js）
   ============================================================
   形态：自包含 IIFE —— 自己建 DOM、自己注册命令面板命令、自己注入样式表，
   不需要 app.js 调用任何初始化函数（与 help.js / db-onboard.js 同一种形态）。

   接口契约（冻结）：local-docs/handoff/V1.10-INTERFACE-CONTRACT.md。
   本文件按契约调用 5 条 IPC：app.aiChatPrepare / app.aiChatSend /
   app.aiChatPoll / app.aiChatHistory / app.aiChatClear / app.aiPrivacyPanel，
   字段一律 snake_case（历史上 has_more / elapsed_ms 就因为写成驼峰，
   界面毫无异常地不显示数据）。

   ## 隐私相关的取舍（这些注释是给下一个接手的人读的，不是给编译器读的）

   1. **本机模型为什么不用弹预览**：ADR-0017 的边界是"数据有没有离开本机"。
      本机推理时数据进的是本机进程，没离开这台机器 —— 对它要求逐次授权，
      只会训练用户"闭眼点确认"，反而稀释了真正跨出本机那一次的分量。
      所以本机模式不弹预览、不勾选、不需要确认；隐私条上明写"数据不出本机"。

   2. **外部服务为什么必须展示原文**：契约 §2.2 规定 app.aiChatSend 把 payload
      **原样照发**，后端不再重新拼一次。这一条只有在"用户看到的就是将要发出去的"
      前提下才成立 —— 所以预览展示的是 prepare 返回的 payload 字符串本身
      （不是我们另写一段描述），并带上 payload_bytes / data_rows 供核对。

   3. **为什么默认只发结构**：表名、列名、类型是"结构"，不是"数据行"。
      勾选框默认不勾 = 默认只发结构；要发数据行，用户得自己勾上、看过原文、
      再点一次「确认发送」。这条默认值就是 ADR-0017 在界面上的落点。

   4. **拿不准服务商是本地还是云端时，按"外部"处理**：宁可多要求一次授权，
      也不要少要求一次。所以「读不到状态」一律降级成"未启用"，而不是乐观放行。

   5. **模型回复一律 textContent**：助手回复来自外部服务，是**外部内容**。
      外部内容进 innerHTML 就是注入面（模型完全可能回一段 <img onerror=…>）。
      本文件全文不出现 innerHTML，任何动态文本都走 textContent。

   6. **AI 没启用时把输入区整个禁用**，而不是"点了再报错"：ADR-0017 第 6 条 ——
      "放一个点了没反应的按钮，比不放更伤信任"。

   ## 已知边界（写下来免得被当成 bug）

   · 不做真流式输出：一次拿完整回复 + 轮询（契约 §4）。
   · 不做多会话管理：一份本机历史 + 清空。
   · 关闭面板**不会**取消在跑的请求：它跑完仍会写进本机历史（下次打开能看到）。
     轮询本身有超时，超时会停下并给「重试」。
   ============================================================ */
(function () {
  "use strict";

  // 幂等：脚本被挂两次（或页面热重载）时不重复建 DOM、不重复注册命令
  if (window.AiChat) return;

  // ============================================================
  // 零、常量与小工具
  // ============================================================
  const STYLE_ID = "aichat-styles";
  const POLL_INTERVAL_MS = 500; // 契约 §3.2：轮询间隔约 500ms
  const POLL_TIMEOUT_MS = 120000; // 2 分钟：够慢模型跑完，又不至于让人以为卡死
  const HISTORY_N = 40; // 契约 §2.4 的默认值
  const AUDIT_N = 50; // 契约 §2.6 的默认值
  const SETTINGS_HINT =
    "在「设置」页的「AI 表格设置」里勾选「启用 AI」并保存；" +
    "服务商换成 Ollama / LM Studio 这类本机模型也可以，那样数据不出本机。";

  const SELF_SRC = (function () {
    const s = document.currentScript && document.currentScript.src;
    if (s) return s;
    const list = document.getElementsByTagName("script");
    for (let i = list.length - 1; i >= 0; i--) {
      if (/ui-chat\.js($|\?)/.test(list[i].src || "")) return list[i].src;
    }
    return null;
  })();

  function injectStyles() {
    if (document.getElementById(STYLE_ID)) return;
    try {
      if (document.querySelector('link[rel="stylesheet"][href$="ai-chat.css"]')) return;
    } catch (e) {}
    let href = "ai-chat.css";
    try {
      href = new URL("ai-chat.css", SELF_SRC || document.baseURI).href;
    } catch (e) {}
    const link = document.createElement("link");
    link.id = STYLE_ID;
    link.rel = "stylesheet";
    link.href = href;
    link.addEventListener("error", () => {
      console.error(
        "[AiChat] ai-chat.css 加载失败：" + href +
          "\n  需在 app/src/assets.rs 的 lookup() 登记 /ai-chat.css。"
      );
    });
    document.head.appendChild(link);
  }

  function errText(e) {
    if (!e) return "未知错误";
    if (typeof e === "string") return e;
    return e.message || String(e);
  }

  function toast(text, opts) {
    const U = window.DeskBaseUI;
    if (U && typeof U.toast === "function") U.toast(text, opts);
    else console.log("[AiChat] " + text);
  }

  function call(cmd, args) {
    const bridge = window.__deskbase;
    if (!bridge || typeof bridge.call !== "function") {
      return Promise.reject(new Error("IPC 桥不可用（app.js 未完成加载？）"));
    }
    return bridge.call(cmd, args);
  }

  /** 建元素。动态文本一律走 textContent —— 本文件没有 innerHTML。 */
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

  /** index.html 里的 sprite 图标：#i-ai-local（显示器）/ #i-ai-cloud（云） */
  function svgIcon(name) {
    const NS = "http://www.w3.org/2000/svg";
    const svg = document.createElementNS(NS, "svg");
    svg.setAttribute("class", "ico");
    svg.setAttribute("aria-hidden", "true");
    const use = document.createElementNS(NS, "use");
    use.setAttribute("href", "#i-" + name);
    svg.appendChild(use);
    return svg;
  }

  function fmtTime(ms) {
    const n = Number(ms);
    const d = new Date(isFinite(n) && n > 0 ? n : Date.now());
    const p = (v) => (v < 10 ? "0" + v : String(v));
    return p(d.getHours()) + ":" + p(d.getMinutes());
  }

  function isoSafe(ms) {
    const n = Number(ms);
    if (!isFinite(n) || n <= 0) return "";
    try {
      return new Date(n).toISOString();
    } catch (e) {
      return "";
    }
  }

  // ============================================================
  // 一、状态
  // ============================================================
  const state = {
    open: false,
    ready: false, // 是否成功读过一次面板状态（决定 open 要不要等）
    enabled: false,
    mode: "off", // off | local | cloud
    provider: "",
    providerLabel: "",
    model: "",
    sentRows: 0, // 本次会话（审计口径）已发出的数据行数
    calls: 0,
    statusError: "",
    items: [], // 审计条目
    convo: [], // [{role, content}] —— 只留契约 §2.1 要的两个字段
    phase: "", // "" | "prepare" | "preview" | "wait"
    taskId: "",
    pollTimer: 0,
    deadline: 0,
    lastText: "", // 供失败/超时后「重试」
    ctx: null, // 调用方经 open({table, columns}) 给的上下文，优先于 DOM 探测
    lastFocus: null,
  };

  let dom = null;

  // ============================================================
  // 二、DOM（全部运行时创建，index.html 只挂脚本与样式表）
  // ============================================================
  function buildDom() {
    if (dom) return dom;

    const root = el("section", {
      class: "aichat-root",
      role: "dialog",
      "aria-modal": "true",
      "aria-label": "AI 对话",
      tabindex: "-1",
      "data-open": "false",
      "data-kind": "off",
    });
    root.id = "ai-chat"; // 契约 §3.3 指定的根节点 id（门禁按它认领）

    // ---------- 标题栏 ----------
    const title = el("h2", { class: "aichat-title" }, "AI 对话");
    const badge = el("span", { class: "aichat-badge", "data-kind": "off" });
    const badgeIco = el("span", { class: "aichat-badge-ico" });
    const badgeText = el("span", { class: "aichat-badge-text" }, "未启用");
    badge.append(badgeIco, badgeText);
    const closeBtn = el(
      "button",
      { class: "btn btn-ghost aichat-close", type: "button", "aria-label": "关闭 AI 对话" },
      "关闭"
    );
    const head = el("header", { class: "aichat-head" });
    head.append(title, badge, closeBtn);

    // ---------- 隐私条：常驻、不折叠（ADR-0017 第 5 条） ----------
    const privacyMain = el("p", { class: "aichat-privacy-main" });
    const privacyRows = el("p", { class: "aichat-privacy-rows" });
    const privacySub = el("p", { class: "aichat-privacy-sub" });
    const privacy = el("div", { class: "aichat-privacy" });
    privacy.append(privacyMain, privacyRows, privacySub);

    // ---------- 授权区：只有外部服务时才显示 ----------
    const cb = el("input", { type: "checkbox", class: "aichat-cb" });
    const cbText = el(
      "span",
      { class: "aichat-check-text" },
      "连同当前表格的数据行一起发送"
    );
    const check = el("label", { class: "aichat-check" });
    check.append(cb, cbText);
    const ctxLine = el("p", { class: "aichat-ctx" });
    const auth = el("div", { class: "aichat-auth" });
    auth.append(check, ctxLine);
    auth.hidden = true; // 默认隐藏：本机模式与未启用时都不出现

    // ---------- 消息区 ----------
    const msgs = el("div", {
      class: "aichat-msgs",
      role: "log",
      "aria-live": "polite",
      "aria-label": "对话内容",
    });

    // ---------- 输入区 ----------
    const ta = el("textarea", {
      class: "aichat-ta",
      rows: "3",
      placeholder: "问点什么…（Ctrl+Enter 发送）",
      "aria-label": "输入给 AI 的内容",
    });
    const hint = el("span", { class: "aichat-hint" }, "Ctrl+Enter 发送");
    const sendBtn = el("button", { class: "btn btn-primary aichat-send", type: "button" }, "发送");
    const inputFoot = el("div", { class: "aichat-input-foot" });
    inputFoot.append(hint, sendBtn);

    // 未启用提示块（含一个"怎么开启"的按钮 —— 点它会说清去哪开，不是哑按钮）
    const disabledText = el(
      "p",
      { class: "aichat-disabled-text" },
      "AI 还没启用 —— 去设置页开启，或把服务商换成本机模型。"
    );
    const helpBtn = el("button", { class: "btn btn-ghost aichat-help", type: "button" }, "怎么开启？");
    const disabledNote = el("div", { class: "aichat-disabled" });
    disabledNote.append(disabledText, helpBtn);

    const input = el("div", { class: "aichat-input" });
    input.append(disabledNote, ta, inputFoot);

    // ---------- 底部动作 ----------
    const recordsBtn = el(
      "button",
      { class: "btn btn-ghost aichat-records-btn", type: "button" },
      "查看发送记录"
    );
    const clearBtn = el(
      "button",
      { class: "btn btn-ghost aichat-clear-btn", type: "button" },
      "清空对话"
    );
    const foot = el("div", { class: "aichat-foot" });
    foot.append(recordsBtn, clearBtn);

    root.append(head, privacy, auth, msgs, input, foot);
    document.body.appendChild(root);

    // ---------- 事件 ----------
    closeBtn.addEventListener("click", () => close());
    sendBtn.addEventListener("click", () => onSend());
    recordsBtn.addEventListener("click", () => openRecords());
    clearBtn.addEventListener("click", () => clearHistory());
    helpBtn.addEventListener("click", () => toast(SETTINGS_HINT, { kind: "info", duration: 9000 }));
    cb.addEventListener("change", () => syncCtxLine());
    ta.addEventListener("keydown", (e) => {
      // Ctrl/Cmd + Enter 发送（用键盘就把话说出去，不用离开输入法）
      if ((e.ctrlKey || e.metaKey) && e.key === "Enter") {
        e.preventDefault();
        onSend();
      }
    });

    dom = {
      root: root,
      badge: badge,
      badgeIco: badgeIco,
      badgeText: badgeText,
      closeBtn: closeBtn,
      privacyMain: privacyMain,
      privacyRows: privacyRows,
      privacySub: privacySub,
      cb: cb,
      ctxLine: ctxLine,
      auth: auth,
      msgs: msgs,
      ta: ta,
      send: sendBtn,
      disabledNote: disabledNote,
      helpBtn: helpBtn,
    };
    return dom;
  }

  // ============================================================
  // 三、隐私面板（状态 → 界面）
  // ============================================================

  /** 服务商模式。**拿不准就按外部算**：多要求一次授权，比少要求一次安全。 */
  function modeFrom(p) {
    if (!p || !p.enabled) return "off";
    if (p.endpoint_kind === "local" || p.local === true) return "local";
    if (p.endpoint_kind === "cloud") return "cloud";
    return "cloud";
  }

  async function refreshPrivacy() {
    let p = null;
    try {
      p = await call("app.aiPrivacyPanel", { n: AUDIT_N });
    } catch (e) {
      // 读不到状态 → 当成"未启用"。宁可让人以为 AI 没开（去设置页一眼就知道），
      // 也不要因为一次读取失败就放行一次可能离开本机的发送。
      state.statusError = errText(e);
      state.enabled = false;
      state.mode = "off";
    }
    if (p) {
      state.statusError = "";
      state.enabled = !!p.enabled;
      state.provider = String(p.provider || "");
      state.providerLabel = String(p.provider_label || "");
      state.model = String(p.model || "");
      state.sentRows = Number(p.session_sent_rows || 0) || 0;
      state.calls = Number(p.session_calls || 0) || 0;
      state.items = Array.isArray(p.items) ? p.items : [];
      state.mode = modeFrom(p);
    }
    // 失败也算"试过了"：否则每次打开都要等 IPC 超时，界面像卡死
    state.ready = true;
    paintPrivacy();
    paintControls();
    return p;
  }

  function paintPrivacy() {
    if (!dom) return;
    const mode = state.mode;
    dom.root.dataset.kind = mode;
    dom.badge.dataset.kind = mode;

    // 徽标：图标 + 两个字（图标按模式着色，文字留在 ink-2）
    dom.badgeText.textContent = mode === "local" ? "本机" : mode === "cloud" ? "外部服务" : "未启用";
    dom.badgeIco.replaceChildren();
    if (mode !== "off") {
      dom.badgeIco.appendChild(svgIcon(mode === "local" ? "ai-local" : "ai-cloud"));
    }

    // 常驻大字：现在到底在用哪一种，以及数据会不会离开本机
    const label = mode === "local" ? "本机模型" : mode === "cloud" ? "外部服务" : "没启用";
    const tail =
      mode === "local"
        ? " —— 数据不出本机"
        : mode === "cloud"
          ? " —— 数据会离开本机"
          : " —— 不会有任何内容发出去";
    dom.privacyMain.replaceChildren(
      document.createTextNode("现在用的是"),
      el("strong", { class: mode === "local" ? "is-local" : mode === "cloud" ? "is-cloud" : "is-off" }, label),
      document.createTextNode(tail)
    );

    // 常驻小字：本次会话已发出多少行数据（>0 用警示色）
    const n = state.sentRows;
    dom.privacyRows.textContent = "本次会话已发出 " + n + " 行数据";
    dom.privacyRows.dataset.alert = n > 0 ? "1" : "0";
    dom.privacyRows.title = state.calls ? "审计里共 " + state.calls + " 次调用" : "";

    if (state.statusError) {
      dom.privacySub.textContent = "读不到 AI 状态：" + state.statusError;
    } else if (mode === "off") {
      dom.privacySub.textContent = SETTINGS_HINT;
    } else {
      const bits = [state.providerLabel || state.provider, state.model].filter(Boolean);
      dom.privacySub.textContent = bits.length ? bits.join(" · ") : "服务商信息不完整，去设置页检查";
    }
  }

  function paintControls() {
    if (!dom) return;
    const on = state.mode !== "off";
    const busy = !!state.phase;
    dom.ta.disabled = !on || busy;
    dom.send.disabled = !on || busy;
    dom.send.textContent = state.phase === "wait" ? "正在思考…" : busy ? "准备中…" : "发送";
    dom.disabledNote.hidden = on;
    dom.auth.hidden = state.mode !== "cloud"; // 授权区只在外部服务时存在
    dom.cb.disabled = busy;
  }

  function setPhase(p) {
    state.phase = p || "";
    paintControls();
  }

  // ============================================================
  // 四、上下文（表名 / 列）—— 决定"结构"到底带什么
  // ============================================================

  /** db.js 把当前表名写在 #db-current-name（形如「当前表：台账」）。
   *  前缀对不上就返回空 —— 宁可这次少带一点结构，也不要把「当前表：」这半句
   *  当成表名发给外部服务。 */
  function currentTableName() {
    const box = document.getElementById("db-current-name");
    const text = box && box.textContent ? box.textContent.trim() : "";
    const m = /^当前表：(.*)$/.exec(text);
    return m ? m[1].trim() : "";
  }

  async function resolveContext() {
    // 调用方经 open({table, columns}) 显式给的优先 —— 比从 DOM 文案里猜可靠
    if (state.ctx && state.ctx.table) return state.ctx;
    const name = currentTableName();
    if (!name) return { table: "", columns: [] };
    // 列结构问后端要（getTable 是裸结构：name / type / …）。
    // 不去 DOM 抠表头文字：那里是展示用的短类型（"文本"），不是真实类型。
    try {
      const t = await call("schema.getTable", { name: name });
      const cols = (t && Array.isArray(t.columns) ? t.columns : [])
        .map((c) => ({
          name: String(c && c.name != null ? c.name : ""),
          type: String(c && c.type != null ? c.type : ""),
        }))
        .filter((c) => c.name);
      return { table: name, columns: cols };
    } catch (e) {
      // 列拿不到不影响发送：少给一点上下文 ≠ 隐私问题
      return { table: name, columns: [] };
    }
  }

  async function syncCtxLine() {
    if (!dom || state.mode !== "cloud") return;
    const ctx = await resolveContext();
    if (dom.cb.checked) {
      dom.ctxLine.textContent =
        "已勾选：发送前会先给你看将要发出的完整原文（含数据行），你点「确认发送」才会真的发出去。";
      return;
    }
    const n = ctx.columns ? ctx.columns.length : 0;
    dom.ctxLine.textContent = ctx.table
      ? "当前表：「" + ctx.table + "」，" + n + " 列。列名与类型属于结构，默认就会发；数据行不会。"
      : "当前没有打开的表 —— 这次只能发结构，上下文里不会有表名与列名。";
  }

  /** 只有外部服务才谈得上"发数据行"；本机模式这个勾选框根本不出现。 */
  function wantDataRows() {
    return state.mode === "cloud" && !!(dom && dom.cb.checked);
  }

  // ============================================================
  // 五、消息渲染（全部 textContent）
  // ============================================================

  function scrollToEnd() {
    if (dom && dom.msgs) dom.msgs.scrollTop = dom.msgs.scrollHeight;
  }

  function appendMsg(role, text, atMs) {
    const wrap = el("div", { class: "aichat-msg", "data-role": role });
    const bubble = el("div", { class: "aichat-bubble" });
    bubble.textContent = String(text == null ? "" : text); // ★ 外部内容只走 textContent
    wrap.appendChild(bubble);
    if (atMs) {
      wrap.appendChild(
        el("time", { class: "aichat-time", datetime: isoSafe(atMs) }, fmtTime(atMs))
      );
    }
    dom.msgs.appendChild(wrap);
    scrollToEnd();
    return wrap;
  }

  function appendNotice(text, opts) {
    const wrap = el("div", { class: "aichat-msg", "data-role": "notice" });
    const bubble = el("div", { class: "aichat-bubble" });
    bubble.appendChild(el("span", null, String(text == null ? "" : text)));
    const retryText = opts && opts.retry ? String(opts.retry) : "";
    if (retryText) {
      const btn = el("button", { class: "btn btn-ghost aichat-retry", type: "button" }, "重试");
      btn.addEventListener("click", () => {
        if (state.phase) return;
        wrap.remove();
        doSend(retryText); // 重试=重走一遍 prepare → （外部）预览确认 → 发送
      });
      bubble.appendChild(btn);
    }
    wrap.appendChild(bubble);
    dom.msgs.appendChild(wrap);
    scrollToEnd();
    return wrap;
  }

  function appendEmptyHint() {
    appendMsg("empty", "还没有对话。可以从一句大白话开始，例如「这张表里的列名都什么意思？」");
  }

  /** 契约 §2.1：messages 只带 role / content（不含本次输入）。
   *  不多塞字段：后端是结构化解析，多带的字段要么被忽略、要么变成兼容负担。 */
  function historyMessages() {
    return state.convo.map((m) => ({ role: m.role, content: m.content }));
  }

  async function loadHistory() {
    if (!dom) return;
    // 一次性铺 40 条时先静音：逐条播报会把读屏器淹掉
    dom.msgs.setAttribute("aria-live", "off");
    dom.msgs.replaceChildren();
    let r = null;
    let failed = "";
    try {
      r = await call("app.aiChatHistory", { n: HISTORY_N });
    } catch (e) {
      failed = errText(e);
    }
    const items = r && Array.isArray(r.items) ? r.items : [];
    state.convo = [];
    if (failed) {
      // 读不到就如实说 —— 装作"还没有对话"会让人以为记录丢了
      appendNotice("读取本机对话记录失败：" + failed);
    } else if (!items.length) {
      appendEmptyHint();
    } else {
      for (const it of items) {
        const role = it && it.role === "user" ? "user" : it && it.role === "assistant" ? "assistant" : "notice";
        const content = it && it.content != null ? String(it.content) : "";
        // 认不出的角色照常显示，但不进入下次发送的上下文（免得把系统消息当助手说的）
        if (role !== "notice") state.convo.push({ role: role, content: content });
        appendMsg(role, content, it && it.at_ms);
      }
    }
    dom.msgs.setAttribute("aria-live", "polite");
  }

  // ============================================================
  // 六、发送链路
  // ============================================================

  async function onSend() {
    if (state.phase) return;
    if (state.mode === "off") {
      toast("AI 还没启用。" + SETTINGS_HINT, { kind: "info", duration: 9000 });
      return;
    }
    const text = (dom.ta.value || "").trim();
    if (!text) {
      toast("先写一句想问的", "error");
      dom.ta.focus();
      return;
    }
    await doSend(text);
  }

  async function doSend(text) {
    if (state.phase) return;
    const input = String(text == null ? "" : text);
    setPhase("prepare");

    let ctx = { table: "", columns: [] };
    try {
      ctx = await resolveContext();
    } catch (e) {
      ctx = { table: "", columns: [] };
    }
    const includeData = wantDataRows();

    let prep = null;
    try {
      // prepare 不发任何网络请求（契约 §2.1）：它只把"这次到底要发什么"算出来
      prep = await call("app.aiChatPrepare", {
        messages: historyMessages(),
        input: input,
        table: ctx.table || "",
        columns: ctx.columns || [],
        include_data: includeData,
      });
    } catch (e) {
      setPhase("");
      appendNotice("准备失败：" + errText(e), { retry: input });
      return;
    }

    const local = prep && (prep.local === true || prep.is_local === true);
    if (!local) {
      // 外部服务：先把原文摆给用户看，确认了才发（契约 §3.2）
      setPhase("preview");
      const ok = await confirmPreview(prep);
      if (!ok) {
        setPhase(""); // 取消：什么都不发，输入框里的字保留
        return;
      }
    }
    await commitSend(input, prep, ctx, includeData);
  }

  async function commitSend(text, prep, ctx, includeData) {
    const payload = prep && prep.payload != null ? String(prep.payload) : "";
    if (!payload) {
      setPhase("");
      appendNotice("没有拿到要发送的内容（payload 为空），已取消这次发送。", { retry: text });
      return;
    }
    // 到这里才把用户那一轮放进界面：在这之前用户还可能取消，
    // 而"界面上已经出现了他说的话"就等于告诉他"已经发出去了"。
    state.convo.push({ role: "user", content: text });
    appendMsg("user", text, Date.now());
    dom.ta.value = "";
    setPhase("wait");

    let r = null;
    try {
      r = await call("app.aiChatSend", {
        payload: payload, // ★ 原样照发：用户看到的和发出去的必须是同一份
        provider: String((prep && prep.provider) || state.provider || ""),
        model: String((prep && prep.model) || state.model || ""),
        table: ctx.table || "",
        data_rows: Number((prep && prep.data_rows) || 0) || 0,
        include_data: !!includeData,
      });
    } catch (e) {
      setPhase("");
      appendNotice("发送失败：" + errText(e), { retry: text });
      return;
    }

    const taskId = r && r.task_id ? String(r.task_id) : "";
    if (!taskId) {
      setPhase("");
      appendNotice("发送失败：后端没有返回 task_id。", { retry: text });
      return;
    }
    state.lastText = text;
    state.taskId = taskId;
    state.deadline = Date.now() + POLL_TIMEOUT_MS;
    pollOnce();
  }

  async function pollOnce() {
    if (!state.taskId) return;
    if (Date.now() > state.deadline) {
      const retry = state.lastText;
      state.taskId = "";
      setPhase("");
      appendNotice(
        "等了 " + Math.round(POLL_TIMEOUT_MS / 1000) +
          " 秒还没等到回复，已经停止等待（请求可能还在跑，但结果不再显示）。",
        { retry: retry }
      );
      refreshPrivacy();
      return;
    }
    let r = null;
    try {
      r = await call("app.aiChatPoll", { task_id: state.taskId });
    } catch (e) {
      const retry = state.lastText;
      state.taskId = "";
      setPhase("");
      appendNotice("取回复失败：" + errText(e), { retry: retry });
      refreshPrivacy();
      return;
    }
    if (!r || !r.done) {
      state.pollTimer = setTimeout(pollOnce, POLL_INTERVAL_MS);
      return;
    }
    state.taskId = "";
    setPhase("");
    if (r.ok) {
      const text = r.text == null ? "" : String(r.text);
      if (text.trim()) {
        state.convo.push({ role: "assistant", content: text });
        appendMsg("assistant", text, Date.now());
      } else {
        appendNotice("收到了空回复 —— 服务商没有返回内容。", { retry: state.lastText });
      }
    } else {
      appendNotice("回复失败：" + String(r.error || "未知错误"), { retry: state.lastText });
    }
    // 数据行计数与审计可能刚变了：隐私条要立刻跟上，不能等到下次打开
    refreshPrivacy();
  }

  // ============================================================
  // 七、预览（外部服务发出去之前的最后一道关）
  // ============================================================

  function collapsible(summary, text) {
    const d = el("details", { class: "aichat-details" });
    d.appendChild(el("summary", null, summary));
    d.appendChild(el("pre", { class: "aichat-pre" }, String(text == null ? "" : text)));
    return d;
  }

  function buildPreviewBody(prep, rows) {
    const box = el("div", { class: "aichat-preview" });
    const lead = el("p", { class: "aichat-preview-lead" });
    if (rows > 0) {
      lead.dataset.kind = "danger";
      lead.textContent =
        "这次会把 " + rows + " 行数据发给「" +
        String((prep && (prep.provider_label || prep.provider)) || "外部服务") +
        "」—— 数据会离开本机。";
    } else {
      lead.dataset.kind = "ok";
      lead.textContent = "这次只发结构（表名、列名、类型），不含任何数据行。";
    }
    box.appendChild(lead);

    const byteLen = Number((prep && prep.payload_bytes) || 0) || 0;
    const list = el("dl", { class: "aichat-kv" });
    const kv = (k, v) => {
      list.append(el("dt", null, k), el("dd", null, String(v == null ? "" : v)));
    };
    kv("服务商", (prep && (prep.provider_label || prep.provider)) || "—");
    kv("模型", (prep && prep.model) || "—");
    kv("地址", (prep && prep.endpoint) || "—");
    kv("消息条数", Number((prep && prep.message_count) || 0) || 0);
    kv("数据行数", rows);
    kv("字节数", byteLen);
    box.appendChild(list);

    const warnings = prep && Array.isArray(prep.warnings) ? prep.warnings : [];
    warnings.forEach((w) => box.appendChild(el("p", { class: "aichat-preview-warn" }, String(w))));

    // 原文：给的不是"我们写的描述"，而是 prepare 返回的 payload 本身。
    // 契约 §2.2 规定发送端把它原样照发 —— 所以这就是将要发出去的那串字节。
    box.appendChild(collapsible("展开完整原文（" + byteLen + " 字节）", (prep && prep.payload) || ""));
    const sp = String((prep && prep.system_prompt) || "");
    if (sp) box.appendChild(collapsible("系统提示词（也在这份请求里）", sp));
    return box;
  }

  function confirmPreview(prep) {
    const U = window.DeskBaseUI;
    if (!U || typeof U.confirm !== "function") {
      // 拿不出预览就**不发**：契约 §3.2 要求外部服务先把原文给用户看，
      // "先发了再说"是这条契约明文要防的事。禁掉这条路，别改成乐观放行。
      toast("预览组件没加载，为安全起见这次没有发出。请重启程序再试。", "error");
      return Promise.resolve(false);
    }
    const rows = Number((prep && prep.data_rows) || 0) || 0;
    return U.confirm({
      title: "确认这次要发出去的内容",
      body: buildPreviewBody(prep, rows),
      confirmText: rows > 0 ? "确认发送（含 " + rows + " 行数据）" : "确认发送",
      cancelText: "取消",
      danger: rows > 0, // 含数据行时按钮用危险色：它真的会离开本机
    });
  }

  // ============================================================
  // 八、发送记录（审计）与清空对话
  // ============================================================

  async function openRecords() {
    await refreshPrivacy(); // 先读一次，别展示几分钟前的旧账
    if (!dom) return;
    const box = el("div", { class: "aichat-records" });
    if (state.statusError) {
      box.appendChild(el("p", { class: "aichat-notice" }, "读不到发送记录：" + state.statusError));
    } else {
      box.appendChild(
        el(
          "p",
          { class: "aichat-records-lead" },
          "本机保存的发送审计，共 " + state.items.length +
            " 条；其中含数据行的调用累计 " + state.sentRows + " 行。"
        )
      );
      if (!state.items.length) {
        box.appendChild(el("p", { class: "aichat-notice" }, "还没有发送记录。"));
      } else {
        const table = el("table", { class: "aichat-table" });
        const thead = el("thead");
        const htr = el("tr");
        ["时间", "动作", "服务商", "对象", "行数", "结果"].forEach((h) =>
          htr.appendChild(el("th", null, h))
        );
        thead.appendChild(htr);
        const tbody = el("tbody");
        for (const it of state.items) {
          const tr = el("tr");
          tr.appendChild(el("td", null, fmtTime(it && it.at_ms)));
          tr.appendChild(el("td", null, String((it && it.action) == null ? "" : it.action)));
          tr.appendChild(el("td", null, String((it && it.provider) == null ? "" : it.provider)));
          const obj = [it && it.table, it && it.column].filter(Boolean).map(String).join(" · ");
          tr.appendChild(el("td", null, obj || "—"));
          tr.appendChild(el("td", null, (it && it.rows) == null ? "" : String(it.rows)));
          const res = el("td");
          res.appendChild(el("span", null, String((it && it.result) == null ? "" : it.result)));
          if (it && it.note) res.appendChild(el("span", { class: "aichat-note" }, " " + String(it.note)));
          tr.appendChild(res);
          tbody.appendChild(tr);
        }
        table.append(thead, tbody);
        box.appendChild(table);
      }
    }
    const U = window.DeskBaseUI;
    if (U && typeof U.modal === "function") {
      U.modal({
        title: "发送记录（审计）",
        ariaLabel: "发送记录",
        body: box,
        actions: [{ label: "关闭", kind: "ghost" }],
      });
    } else {
      toast("记录组件没加载，无法展示。", "error");
    }
  }

  async function clearHistory() {
    const U = window.DeskBaseUI;
    let ok = false;
    if (U && typeof U.confirm === "function") {
      ok = await U.confirm({
        title: "清空本机对话记录？",
        body: "本机保存的对话会被删除，界面上的气泡也会清掉。发送审计是另一份证据，不受这里影响。",
        confirmText: "清空",
        danger: true,
      });
    } else {
      ok = window.confirm("清空本机对话记录？");
    }
    if (!ok) return;
    let r = null;
    try {
      r = await call("app.aiChatClear", {});
    } catch (e) {
      toast("清空失败：" + errText(e), "error");
      return;
    }
    state.convo = [];
    if (dom) {
      dom.msgs.replaceChildren();
      appendEmptyHint();
    }
    const n = r && r.cleared != null ? Number(r.cleared) || 0 : 0;
    toast("已清空 " + n + " 条对话记录");
  }

  // ============================================================
  // 九、开 / 关（含焦点与 Esc）
  // ============================================================

  function focusables() {
    if (!dom) return [];
    // 只认 <a href>，不写 [href]：SVG 的 <use href="#i-ai-…"> 也带 href，
    // 而它**不可聚焦**（`.focus()` 静默失败）。焦点陷阱的 first/last 一旦选中它，
    // Tab 就再也绕不回来了 —— 这条是真实浏览器走查抓到的，不是理论问题。
    const sel =
      'button:not([disabled]), textarea:not([disabled]), input:not([disabled]), a[href], [tabindex]';
    return Array.from(dom.root.querySelectorAll(sel)).filter(
      (n) =>
        n.tabIndex >= 0 && // 真的在 Tab 顺序里（<use> / tabindex="-1" 都会是 -1）
        n.getClientRects().length > 0 // 真的可见（display:none 的子树没有 rect）
    );
  }

  function onDocKey(e) {
    if (!state.open) return;
    // 别的浮层开着时让路：命令面板，以及 DeskBaseUI 的对话框（预览/确认就在里面）。
    // 必须让路的原因很具体：本监听器注册得比那些对话框更早，同一阶段同一节点上
    // 先注册的先跑 —— 不让路就会"Esc 把底下的 AI 面板一起关了"。
    if (
      document.querySelector(".dbui-scrim") ||
      document.querySelector('.dp-root[data-open="true"]')
    ) {
      return;
    }
    if (e.key === "Escape") {
      e.preventDefault();
      close();
      return;
    }
    if (e.key !== "Tab") return;
    // 焦点陷阱：面板声明了 aria-modal="true"，Tab 就不该跑到背后的界面上
    const list = focusables();
    if (!list.length) return;
    const first = list[0];
    const last = list[list.length - 1];
    const active = document.activeElement;
    const inside = dom.root.contains(active);
    if (e.shiftKey) {
      if (!inside || active === first) {
        e.preventDefault();
        last.focus();
      }
    } else if (!inside || active === last) {
      e.preventDefault();
      first.focus();
    }
  }

  async function open(opts) {
    buildDom();
    const cfg = opts || {};
    if (cfg.table) {
      state.ctx = {
        table: String(cfg.table),
        columns: Array.isArray(cfg.columns) ? cfg.columns : [],
      };
    }
    if (!state.open) {
      state.lastFocus = document.activeElement; // 关闭时还回去（与 DeskBaseUI 的对话框同一条纪律）
      state.open = true;
      dom.root.dataset.open = "true";
      document.addEventListener("keydown", onDocKey, true);
    }
    // 每次打开都重读一次状态：用户可能刚在设置页开了/关了 AI 或换了服务商。
    // 先把上次已知状态画出来（面板立刻可用、不闪），再等这一次的真状态。
    //
    // 为什么必须**等**它再定焦点：禁用态下焦点落在「怎么开启？」上，而真状态若
    // 是"已启用"，那个按钮所在的整块提示会被隐藏 —— 焦点随即掉到 body，
    // 用户接下来打字就打进了空气里（这个 bug 是真实浏览器走查抓到的）。
    paintPrivacy();
    paintControls();
    await refreshPrivacy();
    if (!state.phase) loadHistory();
    syncCtxLine();

    // 焦点进入面板：能打字就把光标放进输入框，不能打就给"怎么开启"
    const target = !dom.ta.disabled ? dom.ta : dom.helpBtn;
    if (target && typeof target.focus === "function") target.focus();
    return window.AiChat;
  }

  function close() {
    if (!state.open || !dom) return;
    state.open = false;
    dom.root.dataset.open = "false";
    document.removeEventListener("keydown", onDocKey, true);
    // 注意：**不取消**在跑的请求 —— 它跑完仍会写进本机历史（下次打开看得到）
    const back = state.lastFocus;
    if (back && back.isConnected && typeof back.focus === "function") back.focus();
  }

  function toggle() {
    if (state.open) {
      close();
      return undefined;
    }
    return open();
  }

  // ============================================================
  // 十、对外 API + 命令面板注册
  // ============================================================
  window.AiChat = {
    open: open,
    close: close,
    toggle: toggle,
  };

  let paletteRegistered = false;
  function registerCommand() {
    if (paletteRegistered) return true;
    const P = window.DeskBasePalette;
    if (!P || typeof P.register !== "function") return false;
    P.register({
      id: "ai.chat",
      title: "AI 对话",
      group: "AI",
      py: "ai duihua lianliao chatbot",
      shortcut: "",
      run: () => window.AiChat.open(),
    });
    paletteRegistered = true;
    return true;
  }

  function registerCommandEventually() {
    if (registerCommand()) return;
    // 脚本挂载顺序不由本文件决定（index.html 归 lead），所以按"可能晚到"处理：
    // 先试 DOMContentLoaded / load，再补几次短重试；都不成才告警 ——
    // 静默失败正是本项目最忌讳的一类 bug（check-wiring.cjs 第 2 项就是为它存在的）。
    let tries = 0;
    const retry = () => {
      if (registerCommand()) return;
      tries++;
      if (tries < 25) setTimeout(retry, 200);
      else console.warn("[AiChat] 命令面板未就绪：ai.chat 没有注册（palette.js 挂了吗？）");
    };
    if (document.readyState === "loading") {
      document.addEventListener("DOMContentLoaded", retry, { once: true });
    } else {
      setTimeout(retry, 0);
    }
    window.addEventListener("load", retry, { once: true });
  }

  function init() {
    injectStyles();
    registerCommandEventually();
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();
