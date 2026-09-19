/* ============================================================
   DeskBase 基础组件库（宿主组件）
   ============================================================
   这里的六个组件是 docs/18 动效清单的**宿主**：在此之前那些动效条目
   （toast 让位、弹窗缩放、骨架交接、开关滑块、工具提示、滚动边缘阴影）
   都没有对应的组件可挂，所以一直做不出来。

   设计约束（都是刻意的）：
     · 零依赖、零构建。整个 UI 是原生 HTML/CSS/JS 经 deskbase:// 加载，
       没有打包器也没有 npm。这个文件必须能被 <script src> 直接跑起来。
     · DOM 全部运行时创建并 append 到 body。调用方不需要改 index.html，
       也就不需要在 HTML 里维护"组件挂载点"这类和业务无关的标记。
     · 样式单独放在 components.css，由 injectStyles() 幂等地注入
       （见下面 STYLE_ID 的注释：为什么要用 <link> 而不是把 CSS 塞进 JS）。
     · 只动 transform / opacity，时长全部走 token —— 动效档位调到「关」
       时 token 自己变成 1ms，这里不需要写任何档位特例。

   用法：
     DeskBaseUI.injectStyles();          // 只注入一次，重复调用无害
     DeskBaseUI.toast("已保存", { kind: "success" });
     const ok = await DeskBaseUI.confirm({ title: "删除这条笔记？", body: "删除后无法恢复。", danger: true });
   ============================================================ */
(function () {
  "use strict";

  // 同一份脚本被加载两次时（WebView 里出现过重复注入），
  // 第二次直接退出：重复定义会覆盖掉已经挂在元素上的监听器引用。
  if (window.DeskBaseUI) return;

  // ============================================================
  // 样式注入
  // ============================================================
  const STYLE_ID = "dbui-styles";

  /**
   * 为什么用 <link> 而不是把 CSS 文本内联进这个文件：
   *   内联要维护一份 CSS 字符串常量，等于把样式复制到两处 ——
   *   改样式的人只改 components.css 是没用的，而且动效门禁
   *   （scripts/check-motion.cjs）扫的是样式表，扫不到字符串里的 CSS。
   *   <link> 让 components.css 保持唯一的真相来源。
   *
   * 以脚本自身的位置解析 CSS 路径（而不是 location.href）：
   *   两个文件是兄弟关系，用 currentScript.src 当基准，即使将来
   *   它们被放进子目录也依然找得到对方。
   */
  const cssHref = (function () {
    const self = document.currentScript && document.currentScript.src;
    try {
      return new URL("components.css", self || document.baseURI).href;
    } catch (e) {
      return "components.css";
    }
  })();

  function injectStyles() {
    if (document.getElementById(STYLE_ID)) return; // 幂等
    const link = document.createElement("link");
    link.id = STYLE_ID;
    link.rel = "stylesheet";
    link.href = cssHref;
    // 样式没加载成功时组件会以"裸样式"出现（位置错乱、没有遮罩）。
    // 这类失败必须响亮：docs/18 第 20 条微交互「静默失败禁止」。
    link.addEventListener("error", () => {
      console.error(
        "[DeskBaseUI] components.css 加载失败：" + cssHref +
          "\n  deskbase:// 只服务编译期登记过的资源 —— 需要在 app/src/assets.rs 的 " +
          "lookup() 资源表里登记 /components.css（与 /app.js 一样是两行）。"
      );
    });
    document.head.appendChild(link);
  }

  // ============================================================
  // 小工具
  // ============================================================
  let uid = 0;
  const nextId = (prefix) => prefix + "-" + (++uid);

  const clamp = (v, lo, hi) => (v < lo ? lo : v > hi ? hi : v);

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

  /**
   * 读一个元素上所有过渡时长里最长的那个（毫秒）。
   * 为什么不用常数：动效档位把 --dur-* 压到 1ms 时，清理定时器也该跟着
   * 变短。直接问浏览器"你现在花了多久"，档位语义自动跟着走。
   */
  function maxTransitionMs(node) {
    const raw = getComputedStyle(node).transitionDuration || "0s";
    let max = 0;
    for (const part of raw.split(",")) {
      const v = parseFloat(part) || 0;
      max = Math.max(max, /ms\s*$/.test(part.trim()) ? v : v * 1000);
    }
    return max;
  }

  /**
   * 摘掉进场动画，让退出过渡有起点。
   *
   * 为什么必须有这一步：进场用 animation + fill: both，动画结束后填充值
   * 仍然"压"着 transform/opacity。此时再改类名，浏览器认为计算值没变
   * （都被动画盖着），于是不启动过渡 —— 表现为退出是"啪"地消失。
   * 摘掉动画并强制一次样式重算，起点才是确定的。
   */
  function releaseEnterAnimation(node) {
    node.style.animation = "none";
    void node.offsetWidth; // 强制重算：不要合并这两次改动
  }

  const FOCUSABLE_SELECTOR = [
    "a[href]",
    "area[href]",
    "button:not([disabled])",
    'input:not([disabled]):not([type="hidden"])',
    "select:not([disabled])",
    "textarea:not([disabled])",
    "iframe",
    "audio[controls]",
    "video[controls]",
    '[contenteditable]:not([contenteditable="false"])',
    '[tabindex]:not([tabindex="-1"])',
  ].join(",");

  function focusables(root) {
    const out = [];
    for (const n of root.querySelectorAll(FOCUSABLE_SELECTOR)) {
      if (n.hasAttribute("hidden") || n.getAttribute("aria-hidden") === "true") continue;
      // getClientRects 而不是 offsetParent：fixed 定位元素的 offsetParent 可能是 null
      if (n.getClientRects().length) out.push(n);
    }
    return out;
  }

  // ============================================================
  // Toast 栈
  // ============================================================
  const TOAST_DWELL = 3200; // 普通提示的停留时间
  const TOAST_DWELL_ACTION = 5000; // 带「撤销」这类动作的停留时间（docs/18 7.6 #16 要求 5 秒）

  let toastStack = null;
  let toasts = []; // 旧 → 新；末尾那条贴在最下面
  let stackResizeBound = false;

  function ensureStack() {
    if (toastStack && toastStack.isConnected) return toastStack;
    toastStack = el("div", { class: "dbui-toasts" });
    document.body.appendChild(toastStack);
    // 窗口变窄时 toast 里的文字可能换行、高度变化，让位距离要重算。
    // 只在栈存在时挂监听，且只挂一次（栈被外力移除重建时不要再挂一条）。
    if (!stackResizeBound) {
      stackResizeBound = true;
      window.addEventListener("resize", relayout);
    }
    return toastStack;
  }

  /**
   * 重新分配每条 toast 的 --dbui-stack-y。
   * 先读后写：先一次性读完所有高度，再写 transform 变量。交织读写会让
   * 每写一次都触发一次重排（布局抖动）。
   */
  function relayout() {
    if (!toasts.length) return;
    const gap = (function () {
      const v = parseFloat(getComputedStyle(toastStack).getPropertyValue("--dbui-toast-gap"));
      return isNaN(v) ? 8 : v;
    })();
    const heights = toasts.map((t) => t.el.offsetHeight);
    let y = 0;
    for (let i = toasts.length - 1; i >= 0; i--) {
      toasts[i].el.style.setProperty("--dbui-stack-y", y + "px");
      y += heights[i] + gap;
    }
  }

  function armToast(rec) {
    clearTimeout(rec.timer);
    if (!(rec.dwell > 0)) return;
    rec.timer = setTimeout(() => dismissToast(rec), rec.dwell);
  }

  function dismissToast(rec) {
    if (rec.gone) return;
    rec.gone = true;
    clearTimeout(rec.timer);
    toasts = toasts.filter((t) => t !== rec);
    releaseEnterAnimation(rec.el);
    rec.el.dataset.state = "leaving";
    relayout(); // 让位立刻开始，不必等这条从 DOM 里消失
    const wait = maxTransitionMs(rec.el) + 40;
    setTimeout(() => rec.el.remove(), wait);
  }

  /**
   * @param {string} text
   * @param {{kind?: "info"|"error"|"success", duration?: number,
   *          action?: {label: string, onClick: Function}}} [opts]
   * @returns {{close: Function}}
   */
  function toast(text, opts) {
    injectStyles();
    const cfg = opts || {};
    const kind = cfg.kind === "error" || cfg.kind === "success" ? cfg.kind : "info";
    const stack = ensureStack();

    const node = el("div", { class: "dbui-toast", "data-kind": kind, "data-state": "in", role: "status", "aria-live": "polite" });
    const body = el("span", { class: "dbui-toast-text" });
    node.appendChild(body);
    // 先入 DOM 再写字：live region 必须先存在，读屏器才会播报后插入的文本
    stack.appendChild(node);
    body.textContent = text;

    const rec = { el: node, timer: null, gone: false, dwell: 0 };
    rec.dwell = cfg.duration != null ? cfg.duration : cfg.action ? TOAST_DWELL_ACTION : kind === "error" ? TOAST_DWELL_ACTION : TOAST_DWELL;
    toasts.push(rec);

    if (cfg.action && cfg.action.label) {
      const btn = el("button", { class: "dbui-toast-act", type: "button" }, cfg.action.label);
      btn.addEventListener("click", () => {
        try {
          cfg.action.onClick && cfg.action.onClick();
        } finally {
          dismissToast(rec);
        }
      });
      node.appendChild(btn);
    }

    relayout();
    armToast(rec);

    // 悬停/聚焦时暂停倒计时（WCAG 2.2.1：计时可延长）。
    // 鼠标停在上面说明用户正在读或正要按下「撤销」—— 这时候让它消失是最糟的。
    node.addEventListener("mouseenter", () => clearTimeout(rec.timer));
    node.addEventListener("mouseleave", () => armToast(rec));
    node.addEventListener("focusin", () => clearTimeout(rec.timer));
    node.addEventListener("focusout", () => armToast(rec));

    return {
      close() {
        dismissToast(rec);
      },
    };
  }

  // ============================================================
  // 确认框 / 模态框
  // ============================================================

  /**
   * 按触发位置算 transform-origin。
   *
   * 为什么不用"默认居中"了事：从按钮位置长出来的面板，用户一眼就知道
   * "它和刚才点的东西有关"；从屏幕中心长出来则丢失了这个因果。
   * 触发元素取主动焦点元素 —— 点按钮和键盘回车都会把焦点留在按钮上，
   * 这是唯一不需要调用方额外传参就能拿到的"触发位置"。
   * 拿不到（脚本触发、body 有焦点）时返回 null，交给 CSS 的居中默认值。
   */
  function originFromTrigger(trigger, panelRect) {
    if (!trigger || trigger === document.body || trigger === document.documentElement) return null;
    if (typeof trigger.getBoundingClientRect !== "function") return null;
    const r = trigger.getBoundingClientRect();
    if (!r.width && !r.height) return null;
    const ox = clamp((r.left + r.width / 2 - panelRect.left) / panelRect.width, 0, 1);
    const oy = clamp((r.top + r.height / 2 - panelRect.top) / panelRect.height, 0, 1);
    return (ox * 100).toFixed(1) + "% " + (oy * 100).toFixed(1) + "%";
  }

  function appendContent(host, content) {
    if (content == null) return;
    if (typeof content === "string") {
      // 只走 textContent，不接受 HTML 字符串：渲染层里 innerHTML 是注入面，
      // 需要富内容时调用方传 DOM 节点即可（见下面的 Node 分支）。
      host.textContent = content;
      return;
    }
    if (content instanceof Node) {
      host.appendChild(content);
      return;
    }
    if (Array.isArray(content)) {
      content.forEach((c) => appendContent(host, c));
      return;
    }
    console.error("[DeskBaseUI] body 只接受字符串或 DOM 节点：", content);
  }

  function openDialog(cfg) {
    injectStyles();
    const trigger = document.activeElement;
    const titleId = nextId("dbui-title");

    const scrim = el("div", { class: "dbui-scrim" });
    const panel = el("div", {
      class: "dbui-panel",
      role: "dialog",
      "aria-modal": "true",
      tabindex: "-1",
    });

    if (cfg.title) {
      panel.appendChild(el("h2", { class: "dbui-panel-title", id: titleId }, cfg.title));
      panel.setAttribute("aria-labelledby", titleId);
    } else {
      panel.setAttribute("aria-label", cfg.ariaLabel || "对话框");
    }

    if (cfg.body != null) {
      const body = el("div", { class: "dbui-panel-body" });
      appendContent(body, cfg.body);
      panel.appendChild(body);
    }

    const actions = Array.isArray(cfg.actions) ? cfg.actions : [];
    if (actions.length) {
      const bar = el("div", { class: "dbui-panel-actions" });
      actions.forEach((a) => {
        const btn = el(
          "button",
          { class: "dbui-btn" + (a.kind ? " dbui-btn-" + a.kind : ""), type: "button" },
          a.label || "确定"
        );
        btn.addEventListener("click", () => {
          if (typeof a.onClick === "function") a.onClick();
          // close: false 允许"点了不关"（例如按钮里做校验）
          if (a.close !== false) close({ dismiss: false });
        });
        bar.appendChild(btn);
      });
      panel.appendChild(bar);
    }

    scrim.appendChild(panel);
    document.body.appendChild(scrim);

    // 量面板的**未变换**盒子：此刻 data-state 还没写，进场动画尚未开始，
    // 量到的就是它最终的位置。等动画跑起来再量会带上 scale(0.96) 的误差。
    // （面板现在是 visibility: hidden，布局照常计算，所以量得到。）
    const rect = panel.getBoundingClientRect();
    const origin = originFromTrigger(trigger, rect);
    if (origin) panel.style.transformOrigin = origin;

    scrim.dataset.state = "open";

    let closed = false;
    function close(opts) {
      if (closed) return;
      closed = true;
      document.removeEventListener("keydown", onKeyDown, true);
      releaseEnterAnimation(panel);
      scrim.dataset.state = "leaving";
      const wait = Math.max(maxTransitionMs(panel), maxTransitionMs(scrim)) + 40;
      setTimeout(() => scrim.remove(), wait);
      // 焦点还给触发元素：键盘用户关掉弹窗后必须回到原来那一格，
      // 否则焦点掉回 body，下一次 Tab 从页面开头开始。
      if (trigger && trigger.isConnected && typeof trigger.focus === "function") trigger.focus();
      if (opts && opts.dismiss && typeof cfg.onDismiss === "function") cfg.onDismiss();
    }

    function onKeyDown(e) {
      if (e.key === "Escape") {
        // 用 capture + stopPropagation 抢在应用的全局快捷键之前：
        // Esc 在弹窗打开时的语义应该是"关掉弹窗"，而不是同时触发底下的动作。
        e.preventDefault();
        e.stopPropagation();
        close({ dismiss: true });
        return;
      }
      if (e.key !== "Tab") return;
      // 焦点陷阱：Tab 在对话框内部循环，不许跑到背后的界面上去
      const list = focusables(panel);
      if (!list.length) {
        e.preventDefault();
        panel.focus();
        return;
      }
      const first = list[0];
      const last = list[list.length - 1];
      const active = document.activeElement;
      const inside = panel.contains(active);
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
    document.addEventListener("keydown", onKeyDown, true);

    // 点遮罩 = 关闭（对确认框来说是「取消」）。
    // 破坏性操作里，误关比误确认安全得多，所以遮罩一律按取消处理。
    scrim.addEventListener("click", (e) => {
      if (e.target === scrim) close({ dismiss: true });
    });

    // 打开时把焦点移到第一个可聚焦元素（动作条里「取消」在前，
    // 于是它天然成为初始焦点 —— 回车不会误触发危险操作）。
    const first = focusables(panel)[0];
    (first || panel).focus();

    return { close: (opts) => close(opts || {}), el: scrim };
  }

  /**
   * @param {{title?: string, body?: string|Node, actions?: Array<{label: string,
   *          kind?: "primary"|"danger"|"ghost", close?: boolean, onClick?: Function}>}} opts
   * @returns {{close: Function}}
   */
  function modal(opts) {
    const cfg = opts || {};
    const handle = openDialog({
      title: cfg.title,
      body: cfg.body,
      actions: cfg.actions,
      ariaLabel: cfg.ariaLabel,
    });
    return { close: () => handle.close() };
  }

  /**
   * @param {{title?: string, body?: string|Node, confirmText?: string, danger?: boolean}} opts
   * @returns {Promise<boolean>}
   */
  function confirm(opts) {
    const cfg = opts || {};
    return new Promise((resolve) => {
      openDialog({
        title: cfg.title || "请确认",
        body: cfg.body,
        actions: [
          { label: cfg.cancelText || "取消", kind: "ghost", onClick: () => resolve(false) },
          { label: cfg.confirmText || "确定", kind: cfg.danger ? "danger" : "primary", onClick: () => resolve(true) },
        ],
        // Esc / 点遮罩 = 取消。按钮路径不会走到这里，所以 resolve 只被调用一次。
        onDismiss: () => resolve(false),
      });
    });
  }

  // ============================================================
  // 骨架屏
  // ============================================================

  /**
   * 在容器里铺一层骨架，返回交接方法。
   *
   * 交换时的两条硬要求（docs/18 7.5 C）：
   *   1. 骨架**就地**淡出，不做位移 —— 骨架一滑动，用户看到的"内容到达"
   *      就发生在错误的位置上，位置感比不显示骨架还糟。
   *   2. 两边各 200ms、同时开始，中间不留空档。
   *
   * 所以交接时把骨架冻结在原位并退出文档流：留在流里的话，真实内容会被
   * 挤到骨架下面，等骨架被移除时再"跳"上来 —— 那个跳动恰好发生在内容
   * 到达的瞬间，正是要避免的东西。
   *
   * @param {Element} container 骨架铺进哪个容器
   * @param {{rows?: number}} [opts]
   * @returns {Function} replace(contentEl) —— 也可用 .replace/.remove 属性访问
   */
  function skeleton(container, opts) {
    injectStyles();
    const cfg = opts || {};
    if (!container || typeof container.appendChild !== "function") {
      console.error("[DeskBaseUI] skeleton() 的第一个参数必须是容器元素");
      return Object.assign(function () {}, { replace() {}, remove() {} });
    }
    const rows = clamp(Math.round(cfg.rows || 3), 1, 24);

    const sk = el("div", { class: "dbui-skel", role: "status", "aria-label": "正在加载" });
    for (let i = 0; i < rows; i++) sk.appendChild(el("div", { class: "dbui-skel-row" }));
    container.appendChild(sk);

    let done = false;

    function finish() {
      sk.remove();
    }

    function replace(contentEl) {
      if (done) return;
      done = true;

      // 分层：让骨架待在新内容的**后面**（z-index: -1 只压过容器背景，
      // 压不到在流内容上）。不这么做的话，绝对定位的骨架会浮在文字上面，
      // 200ms 里两边是"糊"在一起的。
      if (getComputedStyle(container).position === "static") container.style.position = "relative";
      if (getComputedStyle(container).isolation !== "isolate") container.style.isolation = "isolate";

      // 先读后写：改样式之后再看 offset* 就是第二次重排了
      const box = { x: sk.offsetLeft, y: sk.offsetTop, w: sk.offsetWidth, h: sk.offsetHeight };

      sk.style.position = "absolute";
      sk.style.left = box.x + "px";
      sk.style.top = box.y + "px";
      sk.style.width = box.w + "px";
      sk.style.height = box.h + "px";
      sk.style.zIndex = "-1";

      if (contentEl instanceof Node) {
        contentEl.classList.add("dbui-arrive");
        contentEl.addEventListener("animationend", () => contentEl.classList.remove("dbui-arrive"), { once: true });
        container.appendChild(contentEl);
      } else if (contentEl != null) {
        console.error("[DeskBaseUI] skeleton.replace() 需要一个 DOM 节点：", contentEl);
      }

      // 同一帧里开始：骨架淡出 200ms，内容淡入 200ms
      sk.dataset.state = "leaving";
      setTimeout(finish, maxTransitionMs(sk) + 40);
    }

    // 规格里写的是"返回一个 replace(contentEl) 方法"。
    // 直接返回函数、同时把 replace/remove 挂在它身上，两种写法都能用：
    //   const sk = skeleton(c, {}); sk.replace(el)  /  sk(el)
    replace.replace = replace;
    replace.remove = function () {
      if (done) return;
      done = true;
      sk.remove();
    };
    return replace;
  }

  // ============================================================
  // 开关
  // ============================================================

  /**
   * @param {boolean} checked
   * @param {(v: boolean) => void} onChange
   * @param {{label?: string}} [opts] label 会写成 aria-label —— 开关没有
   *        可见文字，不给名字的话读屏器只会念"开关"，等于没说。
   * @returns {HTMLButtonElement} 元素上额外提供 checked 读写（getter/setter）
   */
  function switchControl(checked, onChange, opts) {
    injectStyles();
    const cfg = opts || {};
    const btn = el("button", { class: "dbui-switch", type: "button", role: "switch" });
    if (cfg.label) btn.setAttribute("aria-label", cfg.label);
    if (typeof cfg.title === "string") btn.setAttribute("title", cfg.title);

    let on = !!checked;
    const paint = () => btn.setAttribute("aria-checked", on ? "true" : "false");

    function set(v, silent) {
      const next = !!v;
      if (next === on) return;
      on = next;
      paint();
      if (!silent && typeof onChange === "function") onChange(on);
    }

    paint();
    // 用 button 而不是 div：Enter / Space 的激活、焦点顺序、
    // 禁用态语义全都由浏览器给，不需要自己补键盘处理。
    btn.addEventListener("click", () => set(!on));

    Object.defineProperty(btn, "checked", {
      get: () => on,
      // 外部同步状态时不回调 onChange，否则会形成"A 通知 B，B 又通知 A"的环
      set: (v) => set(v, true),
    });
    return btn;
  }

  // ============================================================
  // 工具提示
  // ============================================================

  /**
   * 给元素挂工具提示（鼠标悬停与键盘聚焦都会出）。
   * @param {Element} target
   * @param {string} text
   * @returns {{destroy: Function, el: HTMLElement}}
   */
  function tooltip(target, text) {
    injectStyles();
    if (!target || typeof target.addEventListener !== "function") {
      console.error("[DeskBaseUI] tooltip() 的第一个参数必须是元素");
      return { destroy() {}, el: null };
    }
    const id = nextId("dbui-tip");
    const tip = el("div", { class: "dbui-tip", role: "tooltip", id: id });
    tip.textContent = text;
    document.body.appendChild(tip);
    // 描述关系在挂载时就建立：元素始终在文档里，读屏器任何时候都读得到
    target.setAttribute("aria-describedby", id);

    let hideTimer = null;
    let shown = false;

    // 指针移开后延迟 80ms 再消失：指针从元素划向气泡、或在密集的工具栏里
    // 横向划过时，立刻隐藏会闪成一串；80ms 足够吃掉这种抖动，又不会让人觉得
    // 提示"赖着不走"。
    const HIDE_DELAY = 80;

    function onScroll() {
      hide(true); // 位置一旦变了，固定定位的气泡就对不上了，直接收掉
    }

    function place() {
      const r = target.getBoundingClientRect();
      // 先量气泡自身尺寸：它已在文档里（opacity: 0），尺寸是准的
      const tw = tip.offsetWidth;
      const th = tip.offsetHeight;
      const gap = 8;

      let top = r.top - th - gap;
      let dir = 1; // 1 = 气泡在目标上方，进场从下往上收
      if (top < 4 && r.bottom + gap + th <= window.innerHeight - 4) {
        top = r.bottom + gap;
        dir = -1; // 下方：进场从上往下收
      }
      const left = clamp(r.left + r.width / 2 - tw / 2, 4, Math.max(4, window.innerWidth - tw - 4));
      top = clamp(top, 4, Math.max(4, window.innerHeight - th - 4));
      tip.style.left = left + "px";
      tip.style.top = top + "px";
      // transform-origin 朝向被提示元素：横向取目标中心在气泡上的投影，
      // 纵向取靠近目标的那条边。于是气泡是"从元素那一点长出来"的。
      const ox = clamp((r.left + r.width / 2 - left) / tw, 0, 1);
      tip.style.transformOrigin = (ox * 100).toFixed(1) + "% " + (dir === 1 ? 100 : 0) + "%";
      tip.style.setProperty("--dbui-tip-dir", String(dir));
    }

    function show() {
      clearTimeout(hideTimer);
      hideTimer = null;
      place();
      tip.dataset.state = "on";
      if (!shown) {
        shown = true;
        // 只在显示期间监听滚动：一收起来就摘掉，页面上不留常驻监听器
        window.addEventListener("scroll", onScroll, { passive: true, capture: true });
        window.addEventListener("resize", onScroll, { passive: true });
      }
    }

    function hide(now) {
      clearTimeout(hideTimer);
      hideTimer = setTimeout(
        () => {
          tip.dataset.state = "off";
          shown = false;
          window.removeEventListener("scroll", onScroll, { capture: true });
          window.removeEventListener("resize", onScroll);
        },
        now ? 0 : HIDE_DELAY
      );
    }

    const onEnter = () => show();
    const onLeave = () => hide(false);
    const onDown = () => hide(true);

    target.addEventListener("mouseenter", onEnter);
    target.addEventListener("mouseleave", onLeave);
    target.addEventListener("focus", onEnter);
    target.addEventListener("blur", onLeave);
    target.addEventListener("pointerdown", onDown);

    return {
      el: tip,
      destroy() {
        clearTimeout(hideTimer);
        window.removeEventListener("scroll", onScroll, { capture: true });
        window.removeEventListener("resize", onScroll);
        target.removeEventListener("mouseenter", onEnter);
        target.removeEventListener("mouseleave", onLeave);
        target.removeEventListener("focus", onEnter);
        target.removeEventListener("blur", onLeave);
        target.removeEventListener("pointerdown", onDown);
        target.removeAttribute("aria-describedby");
        tip.remove();
      },
    };
  }

  // ============================================================
  // 滚动边缘阴影
  // ============================================================
  let tlSupport = null;
  function supportsScrollTimeline() {
    if (tlSupport === null) {
      tlSupport =
        typeof CSS !== "undefined" &&
        typeof CSS.supports === "function" &&
        CSS.supports("animation-timeline", "scroll()");
    }
    return tlSupport;
  }

  let reducedMotion = null;
  function prefersReducedMotion() {
    if (!reducedMotion && typeof window.matchMedia === "function") {
      reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)");
    }
    return !!(reducedMotion && reducedMotion.matches);
  }

  /**
   * 走哪条路径。
   *
   * 优先 scroll 时间线（纯 CSS，滚动时一行 JS 都不跑）。两条降级条件：
   *   · 不支持 animation-timeline（旧内核）
   *   · 档位是「关」或系统开了"减少动效" —— 时间线动画不认 --dur-*，
   *     档位压不到它，所以这种情况必须换到不带动画的 IO 路径，
   *     否则会出现"用户关掉了动效，边缘阴影还在自己动"。
   */
  function scrollShadowPath() {
    if (document.documentElement.dataset.motion === "off") return "io";
    if (prefersReducedMotion()) return "io";
    return supportsScrollTimeline() ? "tl" : "io";
  }

  const scrollRegistry = new Map();

  function mountScrollShadow(rec) {
    const host = rec.el;

    const display = getComputedStyle(host).display;
    if (display === "grid" || display === "inline-grid") {
      // grid 里 sticky 伪元素会被当成网格项：它会占掉一个格子，把原来
      // auto-place 的内容挤到隐式的下一列去 —— 布局直接就坏了。
      // 宁可不加阴影，也不能把调用方的布局改坏，所以两条路径都直接跳过
      // 并说清原因（静默失败是禁止的，静默改坏布局更糟）。
      console.warn(
        "[DeskBaseUI] scrollShadow()：容器是 grid，sticky 伪元素会被当成网格项并改变布局，已跳过。" +
          "把可滚动内容包一层 block 元素再调用即可。",
        host
      );
      return;
    }

    if (scrollShadowPath() === "tl") {
      host.classList.add("dbui-scroll", "dbui-scroll-tl");
      rec.path = "tl";
      return;
    }

    host.classList.add("dbui-scroll", "dbui-scroll-io");
    rec.path = "io";

    const setSide = (side, on) => {
      const key = side === "top" ? "dbuiTop" : "dbuiBottom";
      host.dataset[key] = on ? "true" : "false";
    };

    const io = new IntersectionObserver(
      (entries) => {
        const kids = host.children;
        if (!kids.length) return;
        const firstEl = kids[0];
        const lastEl = kids[kids.length - 1];
        for (const entry of entries) {
          if (entry.target === firstEl) setSide("top", !entry.isIntersecting);
          if (entry.target === lastEl) setSide("bottom", !entry.isIntersecting);
        }
      },
      { root: host, threshold: 0 }
    );

    const bind = () => {
      io.disconnect();
      const kids = host.children;
      if (!kids.length) {
        // 内容被清空：观察目标没了，得把上一次的影子也撤掉，
        // 否则容器空了还会挂着一条"下面还有内容"的假提示。
        setSide("top", false);
        setSide("bottom", false);
        return;
      }
      io.observe(kids[0]);
      if (kids.length > 1) io.observe(kids[kids.length - 1]);
      // 观察的是"内容还在不在视口里"，等价于"上面/下面还有没有东西"。
      // 边界是近似的（以首/末子元素的可见性为准），这是这条降级路径的代价，
      // 换来的是滚动期间完全不跑 JS。
    };
    bind();
    rec.io = io;

    // 列表会重渲染（笔记列表就是），子元素被换掉后观察目标会失效 —— 重绑
    rec.mo = new MutationObserver(() => {
      if (rec.moQueued) return;
      rec.moQueued = true;
      queueMicrotask(() => {
        rec.moQueued = false;
        if (rec.path === "io") bind();
      });
    });
    rec.mo.observe(host, { childList: true });
    rec.bind = bind;
  }

  function unmountScrollShadow(rec) {
    rec.el.classList.remove("dbui-scroll", "dbui-scroll-tl", "dbui-scroll-io");
    delete rec.el.dataset.dbuiTop;
    delete rec.el.dataset.dbuiBottom;
    if (rec.io) rec.io.disconnect();
    if (rec.mo) rec.mo.disconnect();
    rec.io = null;
    rec.mo = null;
    rec.path = null;
  }

  let motionWatchBound = false;
  function bindMotionWatch() {
    if (motionWatchBound) return;
    motionWatchBound = true;
    const remount = () => {
      // 档位/系统偏好变了：先全部拆掉再按新路径挂回去。
      // 不做这一步，用户把动效调到「关」之后，已经挂着的边缘阴影会继续动。
      for (const rec of scrollRegistry.values()) {
        unmountScrollShadow(rec);
        mountScrollShadow(rec);
      }
    };
    new MutationObserver(remount).observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["data-motion"],
    });
    if (typeof window.matchMedia === "function") {
      const mq = window.matchMedia("(prefers-reduced-motion: reduce)");
      if (mq.addEventListener) mq.addEventListener("change", remount);
    }
  }

  /**
   * 给滚动容器加边缘阴影遮罩（顶部：内容滚上去了；底部：下面还有内容）。
   * 注意：容器得是 block 流（flex 也行，但组件的 gap 会让伪元素多占一点
   * 空间；grid 会直接跳过并给出警告）。
   * @param {Element} host
   * @returns {{destroy: Function}}
   */
  function scrollShadow(host) {
    injectStyles();
    // 注意别写成 typeof host.classList !== "function" —— classList 是个对象
    // （DOMTokenList），typeof 是 "object"，那样写会把所有元素都挡在门外。
    if (!host || !host.classList || typeof host.classList.add !== "function") {
      console.error("[DeskBaseUI] scrollShadow() 的第一个参数必须是元素");
      return { destroy() {} };
    }
    const existing = scrollRegistry.get(host);
    if (existing) return existing.handle;

    const rec = { el: host, path: null, io: null, mo: null, handle: null, moQueued: false };
    rec.handle = {
      destroy() {
        unmountScrollShadow(rec);
        scrollRegistry.delete(host);
      },
    };
    scrollRegistry.set(host, rec);
    bindMotionWatch();
    mountScrollShadow(rec);
    return rec.handle;
  }

  // ============================================================
  // 进度条（长任务的状态与进度）
  // ============================================================

  /**
   * 一条会动的进度条 + 状态文字。给"要跑一会儿"的操作用（导入数据、批量转换）。
   *
   * ## 为什么要它，而不是继续用一句"正在处理…"
   *
   * 一句静止的文字在长任务里等于没有信息：用户看不出"在动"还是"死了"。
   * 进度条的价值不在于好看，而在于**区分这两件事**。
   *
   * ## 三个状态各有各的颜色，而且都必须能被看见
   *
   * | 状态 | 颜色 | 什么时候 |
   * |------|------|---------|
   * | 进行中 | `--accent` | 有确定进度时按比例填充 |
   * | 不确定 | `--accent` 循环滑动 | 还不知道总量（例如"正在读文件"） |
   * | 成功 | `--success` | 完成后停在满格，不再动 |
   * | 失败 | `--danger` | 填充条变红并停住，并给出下一步 |
   *
   * 颜色只是**加强**：状态文字始终写着"已完成 3,000 / 12,000 行"这种具体量，
   * 色盲用户与高对比主题下也能读懂（`--accent` 这类语义 token 在 11 个主题里
   * 都过了对比度门禁）。
   *
   * ## 动效只动 transform
   *
   * 填充用 `transform: scaleX()` 而不是 `width` —— 后者每帧触发布局，
   * 前者走合成层。这一条是机器门禁（`check-motion.cjs`）在守的，不是自律。
   * 不确定态用一段循环的 `translateX` 动画；动效档位调到「关」时，
   * `motion.css` 会把时长压到 1ms，进度条就变成"静态但位置正确"——
   * 信息（进度数值与状态色）一点不丢。
   *
   * @param {{title?: string, hint?: string, indeterminate?: boolean}} [opts]
   * @returns {{set: Function, done: Function, fail: Function, remove: Function, el: Element}}
   */
  function progress(opts) {
    injectStyles();
    const cfg = opts || {};

    const root = el("div", { class: "dbui-prog", role: "status", "aria-live": "polite" });
    const head = el("div", { class: "dbui-prog-head" });
    const titleEl = el("span", { class: "dbui-prog-title" }, cfg.title || "正在处理…");
    const pctEl = el("span", { class: "dbui-prog-pct" }, "");
    head.appendChild(titleEl);
    head.appendChild(pctEl);

    const track = el("div", { class: "dbui-prog-track" });
    const fill = el("div", { class: "dbui-prog-fill" });
    track.appendChild(fill);

    const hintEl = el("div", { class: "dbui-prog-hint" }, cfg.hint || "");
    root.appendChild(head);
    root.appendChild(track);
    root.appendChild(hintEl);

    const setState = (s) => {
      root.dataset.state = s;
    };
    setState(cfg.indeterminate ? "moving" : "idle");

    /** 0..1 的数字 → 填充比例。拒绝 NaN/负数/超过 1，免得把条子拉坏。 */
    const clamp01 = (v) => {
      const n = Number(v);
      if (!isFinite(n)) return 0;
      return Math.max(0, Math.min(1, n));
    };

    return {
      el: root,

      /**
       * 更新进度。`fraction` 省略 = 不确定态（滑动）。
       * @param {number} [fraction] 0..1
       * @param {string} [label] 覆盖状态文字；不传则用百分比
       */
      set(fraction, label) {
        if (fraction == null) {
          setState("moving");
          pctEl.textContent = "";
        } else {
          const f = clamp01(fraction);
          setState("run");
          fill.style.setProperty("--dbui-prog-p", String(f));
          pctEl.textContent = Math.round(f * 100) + "%";
        }
        if (label != null) hintEl.textContent = label;
      },

      /** 成功：停在满格。文字要说清"做完了什么"，不能只说"成功"。 */
      done(label) {
        setState("ok");
        fill.style.setProperty("--dbui-prog-p", "1");
        pctEl.textContent = "100%";
        hintEl.textContent = label || "完成";
      },

      /** 失败：条子变红停住，并给下一步（只说"失败"等于没说）。 */
      fail(label) {
        setState("error");
        pctEl.textContent = "";
        hintEl.textContent = label || "出错了，请重试";
      },

      remove() {
        if (root.parentNode) root.parentNode.removeChild(root);
      },
    };
  }

  // ============================================================
  // 导出
  // ============================================================
  window.DeskBaseUI = {
    injectStyles,
    toast,
    confirm,
    modal,
    skeleton,
    switchControl,
    tooltip,
    scrollShadow,
    progress,
  };

  // 默认就把样式注进去：调用方只写一个 <script src="components.js"> 也能用。
  // injectStyles 是幂等的，所以 importCustoms 里再显式调一次没有任何副作用，
  // 只是把"样式先于组件"这件事写成明规则。
  injectStyles();
})();
