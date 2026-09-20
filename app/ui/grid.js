/* ============================================================
   DeskBase 数据网格（Data Grid）
   ============================================================
   用户查看与编辑表数据的主界面。能力清单见 docs/06 §5（数据编辑与校验），
   性能口径见 docs/06 §7（虚拟滚动、游标分页、结果集上限），
   视觉与动效见 docs/18。

   这个文件只做一件事：**把"一页一页取回来的行"画成一张能用的表**。
   它不认识 SQL、不认识表结构之外的东西、不自己排序也不自己筛选 ——
   排序与筛选一律原样交给调用方（opts.loadPage），因为本地只加载了
   一部分行，本地排序排的是"这一页"，不是这张表，那是错的排序。

   ## 为什么必须虚拟滚动（而且必须是"复用 DOM"的那种）
   会计会翻到 20 万行。20 万个 <tr> 光是建出来就要几秒、内存几百 MB，
   滚动时每一帧都要把它们全部重排 —— 这不是"慢一点"，是直接卡死。
   所以：
     · 只渲染视口内的行（外加 OVERSCAN 行缓冲），行高固定（--row-h），
       位置由 translateY 表达，不靠 margin/padding 撑开；
     · 行元素在滚动中被**复用**（池子大小恒定），滚动过程中不创建/删除
       节点 —— 建节点比改文本贵一个数量级，而滚动是最频繁的操作；
     · 滚动事件用 requestAnimationFrame 合并，一帧最多算一次；
     · 渲染函数里"先读后写"：只在开头读 scrollTop/clientHeight，之后
       全是写，不动任何会触发布局的量（避免读写交织造成的布局抖动）。

   ## NULL 与空串为什么必须分开显示（这是真实痛点）
   会计看一张费用表，看到空格子，第一件事是问"这格是没填，还是填了个空？"
   这两个状态在业务上完全不同：
     · NULL  = 没填过。可以参与"补全"统计，`IS NULL` 查得到；
     · 空串  = 填过，内容是"空"。`IS NULL` 查不到，"不该为空"的校验也抓不住。
   如果两者都显示成空白，这个区别就只存在于数据库里，用户在界面上**永远
   看不见** —— 于是导出的报表莫名其妙少了几行、对不上账，也没人能定位。
   所以：
     · NULL 显示为灰色的 NULL 占位符（不是空白，一眼能认出来）；
     · 空串显示为空白（它确实是空的），但编辑时的占位文字会写明
       "空字符串"，并且提供"设为 NULL"按钮主动把值清成 NULL。
   判断依据写在 valueView() 里，两个方向都能走得通（空 → NULL / NULL → 空）。

   ## 对外接口
     const grid = DeskBaseGrid.mount(el, { loadPage, commitCell, ... });
     grid.state() / reload() / loadMore() / goto(i) / destroy()
   细节见文件末尾的导出与注释。
   ============================================================ */
(function () {
  "use strict";

  // 同一份脚本被加载两次时（WebView 里出现过重复注入），第二次直接退出。
  if (window.DeskBaseGrid) return;

  /**
   * 本脚本自身的 URL，**在加载时立刻记下来**。
   *
   * 为什么不能等到用的时候再读 document.currentScript：它只在脚本
   * 同步执行期间有值。而 mount() 是应用在切到"数据库"视图时才调的 ——
   * 那时 currentScript 已经是 null，相对路径就会按**页面**去解析。
   * （实测踩过：grid.css 被解析到页面所在目录，样式整份没加载。）
   */
  const SELF_SRC = (function () {
    const s = document.currentScript && document.currentScript.src;
    if (s) return s;
    const links = document.getElementsByTagName("script");
    for (let i = links.length - 1; i >= 0; i--) {
      if (/grid\.js($|\?)/.test(links[i].src || "")) return links[i].src;
    }
    return null;
  })();

  // ============================================================
  // 常量
  // ============================================================
  const OVERSCAN = 2; // 视口上下各多渲染几行，滚动时不会先露白再补
  const MIN_COL_W = 56;
  const MAX_COL_W = 760;
  const LONG_TEXT_CHARS = 60; // 超过这个长度的值改用文本域 + 模态框编辑
  const ROW_H_FALLBACK = 34; // 读不到 --row-h 时的兜底（与宣纸主题一致）
  const CHECK_W = 40; // 行首勾选列宽
  const FILTER_DEBOUNCE = 260; // 筛选输入防抖：每敲一个字都去查库是不礼貌的
  const ENTER_ROWS = 14; // 入场动画只给首屏可见的这十几行（docs/18 的硬要求）
  // 单个元素的高度上限。Chromium 大约 3355 万 px，这里留足余量。
  // 超过它之后靠滚动就到不了更下面的行了（见 syncBodyHeight 的注释）。
  const MAX_BODY_H = 24e6;
  const COLW_KEY = "deskbase.grid.colw.";
  const ID_KEYS = ["id", "ID", "Id", "rowid", "rowId", "_id"];

  // ============================================================
  // 小工具
  // ============================================================
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
   * 问浏览器"你现在要花多久过渡"。动效档位把 --dur-* 压到 1ms 时，
   * 清理定时器也该跟着变短 —— 直接读计算值比写死常数更省事也更准。
   */
  function msFromTransition(node) {
    const raw = getComputedStyle(node).transitionDuration || "0s";
    let max = 0;
    for (const part of raw.split(",")) {
      const v = parseFloat(part) || 0;
      max = Math.max(max, /ms\s*$/.test(part.trim()) ? v : v * 1000);
    }
    return max;
  }

  function errText(e) {
    if (!e) return "未知错误";
    if (typeof e === "string") return e;
    return e.message || String(e);
  }

  // 组件库（toast/confirm/skeleton）的统一入口。
  // 拿不到时降级而不是崩掉：网格本身仍然要能看数据，
  // 但降级路径必须是"响的"（console + window.confirm），不能静默什么都不做。
  function ui() {
    return window.DeskBaseUI || null;
  }

  function toast(text, kind) {
    const U = ui();
    if (U && U.toast) U.toast(text, kind ? { kind } : undefined);
    else console.warn("[DeskBaseGrid] " + text);
  }

  function confirmBox(opts) {
    const U = ui();
    if (U && U.confirm) return U.confirm(opts);
    // 没有组件库时的兜底：用系统对话框，文案保持不变。
    return Promise.resolve(window.confirm(opts.title + "\n\n" + (opts.body || "")));
  }

  // ============================================================
  // 列类型 → 编辑器/对齐/默认列宽
  // ============================================================
  // 类型名来自调用方（Rust 侧的表结构），可能是 SQLite 的宽松类型名
  // （INTEGER / TEXT / REAL / NUMERIC / BOOLEAN / DATETIME / DATE…），
  // 也可能是别的方言。这里只做**子串匹配**，不做枚举白名单 ——
  // 认不出来的就当文本，绝不会因为不认识某个类型名就把格子变成只读。
  function kindOf(type) {
    const t = String(type == null ? "" : type).toLowerCase();
    if (!t) return "text";
    if (t.indexOf("bool") >= 0) return "bool";
    if (t.indexOf("timestamp") >= 0 || t.indexOf("datetime") >= 0 || t === "time") return "datetime";
    if (t.indexOf("date") >= 0) return "date";
    if (t.indexOf("int") >= 0) return "int";
    if (/real|floa|doub|dec|numeric|number/.test(t)) return "num";
    if (/json|blob|clob/.test(t)) return "long";
    return "text";
  }

  const W_BY_KIND = { bool: 88, int: 104, num: 120, date: 132, datetime: 172, long: 240, text: 176 };

  function isNumericKind(k) {
    return k === "int" || k === "num";
  }

  /**
   * 这一列的值是不是"数据库自己会给"。
   *
   * 为什么必须区分：自增主键、带默认值的列一定是 notNull，但用户**不该**
   * 也不能手填它们。如果一律按"非空必填"拦下来，表尾新增行在自增主键的
   * 表上就直接没法用了 —— 而那恰恰是最常见的表。
   * 调用方从 PRAGMA table_info 之类的地方拿到 pk / dflt_value 时顺手映射成
   * auto / default 即可；认不出来时按"要填"处理（宁可多问一句）。
   */
  function isAutoColumn(c) {
    if (!c) return false;
    if (c.auto || c.autoIncrement || c.auto_increment || c.generated) return true;
    if (c.pk && kindOf(c.type) === "int") return true;
    return c.default != null && c.default !== "";
  }

  function pad2(n) {
    return n < 10 ? "0" + n : String(n);
  }

  /**
   * 把数据库里的日期值归一成 <input type="date"> 认识的字符串。
   * 为什么必须归一：SQLite 里日期可能是文本也可能是 Unix 秒/毫秒，
   * 而日期控件只认 yyyy-mm-dd —— 直接塞进去会得到一片空白，
   * 用户会以为"这格本来就是空的"，然后一提交就把真值覆盖掉了。
   */
  function toDateInput(v, withTime) {
    if (v == null || v === "") return "";
    if (typeof v === "number") {
      // 大于 1e11 的当毫秒（到 5138 年才溢出，够用），否则当秒
      const d = new Date(v > 1e11 ? v : v * 1000);
      if (isNaN(d.getTime())) return "";
      const base = d.getFullYear() + "-" + pad2(d.getMonth() + 1) + "-" + pad2(d.getDate());
      return withTime ? base + "T" + pad2(d.getHours()) + ":" + pad2(d.getMinutes()) : base;
    }
    const s = String(v).trim();
    if (!s) return "";
    const iso = /^(\d{4})-(\d{2})-(\d{2})(?:[T ](\d{2}):(\d{2}))?/.exec(s);
    if (iso) {
      const base = iso[1] + "-" + iso[2] + "-" + iso[3];
      if (!withTime) return base;
      return base + "T" + (iso[4] || "00") + ":" + (iso[5] || "00");
    }
    const d = new Date(s);
    if (isNaN(d.getTime())) return "";
    const base = d.getFullYear() + "-" + pad2(d.getMonth() + 1) + "-" + pad2(d.getDate());
    return withTime ? base + "T" + pad2(d.getHours()) + ":" + pad2(d.getMinutes()) : base;
  }

  /**
   * 日期控件 → 写回数据库的值。
   * 用空格分隔的 "YYYY-MM-DD HH:MM:SS" 而不是 ISO 的 "T" 形式：
   * SQLite 的日期函数两种都吃，但空格形式是它自己的规范写法，
   * 也是别的工具（DB Browser 等）显示得最正常的那种。
   */
  function fromDateInput(s, withTime) {
    if (!s) return null;
    return withTime ? s.replace("T", " ") + ":00" : s;
  }

  // ============================================================
  // 样式注入
  // ============================================================
  const STYLE_ID = "dbgrid-styles";

  /**
   * 与 components.js 同样的策略：样式放在独立的 grid.css 里，由 <script> 自己
   * 注入。理由有两条：
   *   1. 把 CSS 内联成 JS 字符串等于把样式复制到两处，改样式的人改 grid.css
   *      是没用的；而且动效门禁扫的是样式表文件，扫不到字符串里的 CSS。
   *   2. 调用方只需要加一行 <script src="grid.js">，index.html 里不用维护
   *      任何与网格有关的标记（DOM 全部运行时创建）。
   * 已经手动加了 <link href="grid.css"> 的情况要认出来，别加载第二遍。
   */
  function injectStyles() {
    if (document.getElementById(STYLE_ID)) return;
    try {
      if (document.querySelector('link[rel="stylesheet"][href$="grid.css"]')) return;
    } catch (e) {
      /* 选择器不支持就算了，下面照常注入 */
    }
    let href = "grid.css";
    try {
      href = new URL("grid.css", SELF_SRC || document.baseURI).href;
    } catch (e) {
      /* 保底用相对路径 */
    }
    const link = el("link", { id: STYLE_ID, rel: "stylesheet", href: href });
    link.addEventListener("error", () => {
      // 样式没加载成功时网格会以裸样式出现（列宽全崩、无法阅读）。
      // 这类失败必须响亮：docs/18 微交互第 20 条「静默失败禁止」。
      console.error(
        "[DeskBaseGrid] grid.css 加载失败：" + href +
          "\n  deskbase:// 只服务编译期登记过的资源 —— 需要在 app/src/assets.rs 的 lookup() 资源表里登记 /grid.css。"
      );
    });
    document.head.appendChild(link);
  }

  // ============================================================
  // 主函数
  // ============================================================

  /**
   * @param {Element} container 挂载点（网格会把自己的根节点塞进去）
   * @param {{
   *   loadPage: (q: {after: any, limit: number, sort: ?{column: string, dir: "asc"|"desc"},
   *                  filters: Object}) => Promise<{rows: Array, columns?: Array,
   *                  hasMore?: boolean, nextCursor?: any}>,
   *   commitCell?: (a: {rowId: any, column: string, value: any}) => Promise<void>,
   *   deleteRows?: (rowIds: Array<any>) => Promise<void>,
   *   insertRow?: (values: Object) => Promise<any>,
   *   columns?: Array<{name: string, type?: string, notNull?: boolean, comment?: string}>,
   *   table?: string,          // 只用于 state() 与 localStorage 的列宽键；网格不碰数据库
   *   editable?: boolean,
   *   pageSize?: number,
   *   rowIdKey?: string,       // 行主键叫什么（默认依次找 id / rowid / _id）
   *   rowId?: (row: any, index: number) => any,  // 取不到主键时的兜底
   *   overscan?: number
   * }} opts
   */
  function mount(container, opts) {
    if (!container || typeof container.appendChild !== "function") {
      console.error("[DeskBaseGrid] mount() 的第一个参数必须是容器元素");
      return null;
    }
    const cfg = opts || {};
    if (typeof cfg.loadPage !== "function") {
      // 分页由调用方实现（它去调 Rust 的 keyset 分页）。网格不拼 SQL，
      // 也不认识表名以外的任何数据库细节 —— 没有 loadPage 就没有数据来源。
      console.error("[DeskBaseGrid] opts.loadPage 必须是函数：分页由调用方实现。");
      return null;
    }
    injectStyles();

    // ---------- 配置与状态 ----------
    const pageSize = clamp(Math.round(cfg.pageSize || 200), 1, 5000);
    const overscan = cfg.overscan == null ? OVERSCAN : clamp(cfg.overscan, 0, 30);
    const idKeys = (cfg.rowIdKey ? [cfg.rowIdKey] : []).concat(ID_KEYS);

    const st = {
      table: cfg.table == null ? "" : String(cfg.table),
      columns: Array.isArray(cfg.columns) ? cfg.columns.slice() : [],
      rows: [], // 已加载的行（引用调用方给的对象，不复制：20 万行复制不起）
      rowIds: [], // 与 rows 一一对应的主键值（取不到时为 null）
      cursor: null, // 下一页的 keyset 游标（由 Rust 侧给回）
      hasMore: true,
      loading: false,
      loadError: null,
      sort: null, // {column, dir} —— 三态里的"无"就是 null
      filters: {}, // {列名: 关键词}
      selection: new Set(), // 选中的行主键
      colW: [], // 列宽（按列序，虚拟滚动与表头共用一张表）
      rowH: ROW_H_FALLBACK,
      headH: 44,
      cursorPos: null, // {r, c} 键盘光标（与"选中的行"是两件事）
      pending: 0, // 正在提交中的写入数
      newTouched: false, // 表尾新增行里有没有填过内容
    };
    const editable = !!cfg.editable;
    const canCommit = typeof cfg.commitCell === "function";
    const canDelete = typeof cfg.deleteRows === "function";
    const canInsert = typeof cfg.insertRow === "function";
    if (editable && !canCommit) {
      // 说了能编辑却没给提交函数：与其在用户双击时才发现，不如现在就说清楚。
      console.error("[DeskBaseGrid] editable=true 但 opts.commitCell 不是函数，单元格将保持只读。");
    }

    let destroyed = false;
    let loadSeq = 0; // 请求序号：迟到的响应不许覆盖新的
    let inflight = null; // 在途的那次加载：loadMore 撞上它时要返回它而不是空转
    let rafId = 0;
    let filterTimer = null;
    let veilTimer = null;
    let skelHandle = null; // DeskBaseUI.skeleton 的句柄
    let edit = null; // 当前编辑会话
    let pool = []; // 行元素池（只会变长，不会变短 —— 复用是性能的全部）
    let newRow = null; // 表尾"新增"行
    let newFields = []; // 新增行里的输入控件
    let entering = false; // 本次渲染要不要做入场动画

    // ---------- DOM 骨架 ----------
    // 结构：bar（工具条）/ scroll（唯一滚动容器）
    //        └ inner（内容宽度 = 所有列宽之和）
    //           ├ head（sticky：横向跟滚、纵向钉住）
    //           └ body（高度 = 行数 × 行高，是滚动条的"虚拟空间"）
    //              ├ rows（translateY 到视口起点，里面是复用的行）
    //              └ newrow（表尾新增行，钉在最后一行下面）
    //       veil（骨架屏 / 空态 / 错误态，盖在上面）
    const root = el("div", { class: "dbgrid", role: "grid", tabindex: "0", "aria-label": st.table ? st.table + " 数据网格" : "数据网格" });
    if (st.table) root.dataset.table = st.table;

    const bar = el("div", { class: "dbgrid-bar" });
    const info = el("span", { class: "dbgrid-info" });
    const selInfo = el("span", { class: "dbgrid-info dbgrid-selinfo", hidden: "hidden" });
    const barSpacer = el("span", { class: "dbgrid-bar-spacer" });
    const btnAdd = el("button", { class: "btn", type: "button" }, "新增一行");
    const btnReload = el("button", { class: "btn btn-ghost", type: "button" }, "刷新");
    const btnDel = el("button", { class: "btn", type: "button" }, "删除所选");
    const roBadge = el("span", { class: "badge dbgrid-badge", hidden: "hidden" }, "只读");
    // 底部状态行与"加载更多"：放在工具条里，不额外占一行高度
    const footEl = el("span", { class: "dbgrid-foot" });
    const btnMore = el("button", { class: "btn btn-ghost dbgrid-more", type: "button" }, "加载更多");
    btnMore.addEventListener("click", () => loadMore());
    bar.append(info, selInfo, barSpacer, footEl, btnMore, roBadge, btnAdd, btnReload, btnDel);
    if (!canInsert || !editable) btnAdd.hidden = true;
    if (!canDelete || !editable) btnDel.hidden = true;
    if (!editable) roBadge.hidden = false;

    const scrollEl = el("div", { class: "dbgrid-scroll" });
    const inner = el("div", { class: "dbgrid-inner" });
    const headEl = el("div", { class: "dbgrid-head", role: "rowgroup" });
    const hrow = el("div", { class: "dbgrid-hrow", role: "row" });
    const frow = el("div", { class: "dbgrid-frow", role: "row" });
    headEl.append(hrow, frow);
    const bodyEl = el("div", { class: "dbgrid-body" });
    const rowsWrap = el("div", { class: "dbgrid-rows" });
    bodyEl.appendChild(rowsWrap);
    inner.append(headEl, bodyEl);
    scrollEl.appendChild(inner);

    // 覆盖层只盖住"数据区"，不盖工具条：空态里写着"点「新增一行」"，
    // 而按钮就在工具条上 —— 盖住它等于让人去点一个看不见的东西。
    const mainEl = el("div", { class: "dbgrid-main" });
    const veil = el("div", { class: "dbgrid-veil", hidden: "hidden", "aria-live": "polite" });
    mainEl.append(scrollEl, veil);
    root.append(bar, mainEl);
    container.appendChild(root);

    // ============================================================
    // 取值 / 写值
    // ============================================================
    // rows 可能是「按列名取值的对象」，也可能是「按下标取值的数组」。
    // 两种都支持，因为 Rust 侧序列化出来是哪一种不由网格决定。
    function cellOf(row, col, i) {
      if (row == null) return null;
      if (Array.isArray(row)) return row[i];
      return row[col.name];
    }

    function setCell(row, col, i, v) {
      if (row == null) return;
      if (Array.isArray(row)) row[i] = v;
      else row[col.name] = v;
    }

    /**
     * 行主键。批量删除、单元格提交都要它。
     * 取不到就返回 null —— 让它"看起来能选、点了才发现删不掉"是最糟的设计，
     * 所以取不到主键的行：勾选框禁用、双击不进入编辑，并说明原因。
     */
    function rowIdOf(row, index) {
      if (typeof cfg.rowId === "function") {
        const v = cfg.rowId(row, index);
        if (v != null) return v;
      }
      if (row == null) return null;
      if (Array.isArray(row)) {
        for (let i = 0; i < st.columns.length; i++) {
          if (idKeys.indexOf(String(st.columns[i].name)) >= 0 && row[i] != null) return row[i];
        }
        return null;
      }
      for (let i = 0; i < idKeys.length; i++) {
        const v = row[idKeys[i]];
        if (v != null) return v;
      }
      return null;
    }

    const selKey = (id) => (typeof id === "object" ? JSON.stringify(id) : String(id));

    // ============================================================
    // 度量：行高、表头高、视口高
    // ============================================================
    /**
     * 行高固定，并且是和 CSS 共用的同一个 token（--row-h）：JS 用它算行位置，
     * CSS 用它定行高。两处都读同一个变量，就不会出现"算的和画的对不上"。
     */
    function readRowH() {
      const raw = getComputedStyle(root).getPropertyValue("--row-h");
      const v = parseFloat(raw);
      return isFinite(v) && v > 8 ? v : ROW_H_FALLBACK;
    }

    function measure() {
      st.rowH = readRowH();
      // headH 只在结构/尺寸变化时读一次并缓存：每帧都读表头高度会让渲染
      // 函数开头多一次强制重排，那是滚动掉帧最常见的来源。
      st.headH = headEl.offsetHeight || st.rowH * 2;
      syncBodyHeight();
    }

    function visibleRange() {
      // 表头是 sticky 的，它压在内容上面 —— 视口真正的可见行区间要减掉表头高度，
      // 否则滚到表头下面那几行会被算成"可见"却看不见。
      const top = scrollEl.scrollTop + st.headH;
      const rowH = st.rowH;
      const first = Math.max(0, Math.floor(top / rowH) - overscan);
      const view = scrollEl.clientHeight - st.headH;
      const count = Math.ceil(Math.max(0, view) / rowH) + overscan * 2 + 1;
      return { first: first, count: count };
    }

    function syncBodyHeight() {
      const total = st.rows.length * st.rowH;
      const capped = Math.min(total, MAX_BODY_H);
      // 新增行永远在最后一行下面（表尾），所以 body 高度要多留一行。
      // 超过 MAX_BODY_H（约 70 万行）之后滚动到不了更下面 —— 这是浏览器
      // 单元素高度的硬上限，不是省事：真有那么大的表，用户应该用筛选或
      // goto() 定位，而不是一屏一屏地滚。
      const extra = newRow && canInsert && editable ? st.rowH : 0;
      bodyEl.style.height = capped + extra + "px";
      if (newRow) newRow.style.transform = "translateY(" + capped + "px)";
    }

    // ============================================================
    // 构建：表头 / 列宽 / 行
    // ============================================================
    function colSignature() {
      return st.columns.map((c) => c.name + ":" + (c.type || "")).join("|");
    }

    function loadColWidths() {
      // 列宽记在 localStorage 里，键按表名分：同一个用户有多张表，
      // 列宽是"这张表的这个列"的属性，不该互相串。
      // 没有表名时不记也不读 —— 宁可这次不记住，也别把别的表的列宽覆盖掉。
      st.colW = st.columns.map((c) => W_BY_KIND[kindOf(c.type)] || 176);
      if (!st.table) return;
      try {
        const raw = localStorage.getItem(COLW_KEY + st.table);
        if (!raw) return;
        const saved = JSON.parse(raw);
        if (!saved || typeof saved !== "object") return;
        st.columns.forEach((c, i) => {
          const w = Number(saved[c.name]);
          if (isFinite(w) && w >= MIN_COL_W) st.colW[i] = clamp(Math.round(w), MIN_COL_W, MAX_COL_W);
        });
      } catch (e) {
        // 读不出来就当没有：列宽是锦上添花，不能因为它把网格拦在门外
        console.warn("[DeskBaseGrid] 列宽读取失败，已用默认值：", errText(e));
      }
    }

    function saveColWidths() {
      if (!st.table) return;
      try {
        const out = {};
        st.columns.forEach((c, i) => (out[c.name] = Math.round(st.colW[i])));
        localStorage.setItem(COLW_KEY + st.table, JSON.stringify(out));
      } catch (e) {
        console.warn("[DeskBaseGrid] 列宽保存失败：", errText(e));
      }
    }

    function applyColVars() {
      st.columns.forEach((c, i) => root.style.setProperty("--dgc-" + i, Math.round(st.colW[i]) + "px"));
    }

    function setColW(i, w) {
      st.colW[i] = clamp(Math.round(w), MIN_COL_W, MAX_COL_W);
      root.style.setProperty("--dgc-" + i, st.colW[i] + "px");
      syncInnerWidth();
    }

    function totalColW() {
      let sum = CHECK_W;
      for (let i = 0; i < st.colW.length; i++) sum += st.colW[i];
      return sum;
    }

    function syncInnerWidth() {
      // inner 的宽度就是横向滚动范围。min-width:100% 保证列少时表头铺满整行。
      inner.style.width = totalColW() + "px";
    }

    function buildHeader() {
      hrow.replaceChildren();
      frow.replaceChildren();
      // 行首勾选列
      const ck = el("div", { class: "dbgrid-th dbgrid-ck", role: "columnheader" });
      const allBox = el("input", { type: "checkbox", class: "dbgrid-check", "aria-label": "全选已加载的行" });
      allBox.dataset.act = "select-all";
      ck.appendChild(allBox);
      hrow.appendChild(ck);
      st.columns.forEach((c, i) => {
        const kind = kindOf(c.type);
        const th = el("div", { class: "dbgrid-th", role: "columnheader" });
        th.dataset.c = i;
        th.style.width = "var(--dgc-" + i + ")";
        // 列含义（comment）挂在表头上：会计要看的往往就是"这一列到底指什么"，
        // 而不是它的类型。类型另给一个小字角标。
        const tips = [];
        if (c.comment) tips.push(String(c.comment));
        if (c.type) tips.push("类型 " + c.type);
        if (c.notNull) tips.push("不允许为空");
        if (tips.length) th.title = tips.join("　·　");
        const label = el("span", { class: "dbgrid-th-label" }, c.name);
        const typeTag = el("span", { class: "dbgrid-th-type" }, shortType(c.type));
        const caret = el("span", { class: "dbgrid-sort", "aria-hidden": "true" });
        const rz = el("span", { class: "dbgrid-rz", role: "separator", "aria-label": c.name + " 列宽", title: "拖动调整列宽，双击自适应" });
        rz.dataset.c = i;
        // 列操作入口（Excel 那个"列头按钮"）：网格不认识业务，
        // 只把"用户点了哪一列的菜单"抛给上层（cfg.onColumnMenu）。
        // 与 commitCell / deleteRows 同一套分层：设置里没给回调就不挂按钮。
        if (typeof cfg.onColumnMenu === "function") {
          const menuBtn = el("button", {
            class: "dbgrid-colmenu",
            type: "button",
            title: c.name + "：改列名 / 加列 / 删列",
            "aria-label": c.name + " 列操作",
          }, "⋯");
          menuBtn.addEventListener("click", (ev) => {
            ev.stopPropagation(); // 别触发列头的排序
            cfg.onColumnMenu({ index: i, name: c.name });
          });
          th.append(label, typeTag, caret, menuBtn, rz);
        } else {
          th.append(label, typeTag, caret, rz);
        }
        hrow.appendChild(th);

        const fc = el("div", { class: "dbgrid-fcell", role: "gridcell" });
        fc.style.width = "var(--dgc-" + i + ")";
        const input = el("input", { class: "dbgrid-filter", type: "search", placeholder: "筛选", "aria-label": "筛选 " + c.name });
        input.dataset.c = i;
        if (st.filters[c.name]) input.value = st.filters[c.name];
        fc.appendChild(input);
        frow.appendChild(fc);
      });
      // 末尾的"补齐格"：列宽之和小于视口时，用它把表头和行铺满，
      // 不至于在右边留一条没有分界线的空白。
      hrow.append(el("div", { class: "dbgrid-th dbgrid-fill", "aria-hidden": "true" }));
      frow.append(el("div", { class: "dbgrid-fcell dbgrid-fill", "aria-hidden": "true" }));
      root.setAttribute("aria-colcount", String(st.columns.length + (editable ? 1 : 0)));
      applyColVars();
      syncInnerWidth();
      paintHeadState();
    }

    function shortType(type) {
      const t = String(type == null ? "" : type);
      if (!t) return "";
      const m = /^([a-zA-Z ]+)/.exec(t);
      const word = (m ? m[1] : t).trim();
      return word.length > 8 ? word.slice(0, 8) : word;
    }

    function buildRowEl(kindTag) {
      const row = el("div", { class: "dbgrid-row", role: "row" });
      row.dataset.kind = kindTag;
      const ck = el("div", { class: "dbgrid-cell dbgrid-ck", role: "gridcell" });
      ck.style.width = CHECK_W + "px";
      const box = el("input", { type: "checkbox", class: "dbgrid-check", "aria-label": "选择这一行" });
      ck.appendChild(box);
      row.appendChild(ck);
      row._ck = box;
      row._cells = [];
      st.columns.forEach((c, i) => {
        const cell = el("div", { class: "dbgrid-cell", role: "gridcell", "aria-colindex": i + 2 });
        cell.dataset.c = i;
        cell.style.width = "var(--dgc-" + i + ")";
        if (isNumericKind(kindOf(c.type))) cell.classList.add("is-num");
        const nullMark = el("span", { class: "dbgrid-null" });
        nullMark.textContent = "NULL";
        nullMark.hidden = true;
        const txt = el("span", { class: "dbgrid-txt" });
        cell.append(nullMark, txt);
        cell._null = nullMark;
        cell._txt = txt;
        row._cells.push(cell);
        row.appendChild(cell);
      });
      row.appendChild(el("div", { class: "dbgrid-cell dbgrid-fill", "aria-hidden": "true" }));
      return row;
    }

    function rebuildRows() {
      // 列变了（表结构变了、或者第一页才把 columns 带回来）时，池子里的
      // 行元素还挂着旧列数的单元格，只能整池重建。这是罕见事件，不做增量。
      pool.forEach((r) => r.remove());
      pool = [];
      if (newRow) newRow.remove();
      newRow = null;
      newFields = [];
      buildNewRow();
      syncBodyHeight();
    }

    // ============================================================
    // 画一行
    // ============================================================
    // valueView 决定一个格子"显示成什么"。
    // 这里是 NULL 与空串分家的地方（文件头有为什么）：
    //   null / undefined → 灰色的 NULL 占位符
    //   ""               → 真正的空白（但加了 title，鼠标停上去能问出区别）
    function valueView(v, kind) {
      if (v === null || v === undefined) return { mode: "null" };
      if (v === "") return { mode: "blank" };
      if (kind === "bool") return { mode: "text", text: v === true || v === 1 || v === "1" ? "是" : "否" };
      if (typeof v === "object") {
        try {
          return { mode: "text", text: JSON.stringify(v) };
        } catch (e) {
          return { mode: "text", text: String(v) };
        }
      }
      return { mode: "text", text: String(v) };
    }

    function paintCell(cell, row, col, i) {
      const view = valueView(cellOf(row, col, i), kindOf(col && col.type));
      if (view.mode === "null") {
        cell._null.hidden = false;
        cell._txt.textContent = "";
        cell.title = "未填写（NULL）";
      } else if (view.mode === "blank") {
        cell._null.hidden = true;
        cell._txt.textContent = "";
        // 这一条是给会计的：空白有两种，鼠标停一下就能确认是哪一种
        cell.title = "空字符串（填过，内容是空 —— 不是 NULL）";
      } else {
        cell._null.hidden = true;
        // 文本内容变了才写：20 万行滚动时同一格会被反复赋值，
        // 无脑写入会让浏览器做无谓的文本重排。
        if (cell._txt.textContent !== view.text) cell._txt.textContent = view.text;
        cell.removeAttribute("title");
      }
    }

    function paintRowSelection(rowEl, idx) {
      const id = st.rowIds[idx];
      const on = id != null && st.selection.has(selKey(id));
      rowEl.dataset.sel = on ? "true" : "false";
      if (rowEl._ck) {
        rowEl._ck.checked = on;
        rowEl._ck.disabled = !editable || !canDelete || id == null;
        if (id == null) rowEl._ck.title = "这一行没有主键，无法选中或删除";
      }
    }

    function bindRow(rowEl, idx, force) {
      if (!force && rowEl._idx === idx) return;
      // 复用前先把这一行上残留的入场动画摘掉：入场动画只属于"数据刚到时
      // 首屏那十几行"，滚动过程中绝不能重新播一遍（docs/18 明令禁止大列表入场）。
      if (rowEl._enter) {
        rowEl.classList.remove("dbgrid-enter");
        rowEl._enter = false;
      }
      rowEl._idx = idx;
      rowEl.dataset.i = idx;
      // 斑马纹：行的 DOM 顺序和视觉行号在虚拟滚动里并不一致，
      // 所以按"数据行号"给奇偶属性，而不是用 nth-child。
      rowEl.dataset.parity = idx % 2 ? "odd" : "even";
      rowEl.setAttribute("aria-rowindex", String(idx + 2)); // +1 表头，+1 从 1 数起
      const row = st.rows[idx];
      const id = st.rowIds[idx];
      const locked = id == null || !editable || !canCommit;
      rowEl.dataset.locked = locked ? "true" : "false";
      for (let i = 0; i < st.columns.length; i++) {
        const cell = rowEl._cells[i];
        if (!cell) continue;
        paintCell(cell, row, st.columns[i], i);
        cell.setAttribute("aria-colindex", String(i + 2));
      }
      paintRowSelection(rowEl, idx);
      if (entering && idx < ENTER_ROWS) {
        rowEl.style.setProperty("--dbgrid-i", String(idx));
        rowEl.classList.add("dbgrid-enter");
        rowEl._enter = true;
      }
    }

    // ============================================================
    // 渲染（虚拟滚动的心脏）
    // ============================================================
    function render(force) {
      if (destroyed) return;
      const maxRows = st.rows.length;
      const range = visibleRange();
      // 行被删掉之后 scrollTop 可能还停在老位置（超出新高度），
      // 这时算出来的 first 会落在末尾之外，必须夹回来。
      const first = maxRows ? Math.min(range.first, maxRows - 1) : 0;
      let count = maxRows ? Math.min(range.count, maxRows - first) : 0;

      // 池子只长不缩：视口变大时补几个，变小了就 hidden 掉多出来的，
      // 下次变大还能直接用（创建节点是这里最贵的操作）。
      while (pool.length < Math.max(count, 1) && pool.length < maxRows) {
        const rowEl = buildRowEl("data");
        pool.push(rowEl);
        rowsWrap.appendChild(rowEl);
      }

      // 正在编辑的那一行如果被复用到了别的位置，先把编辑提交掉。
      // 不这么做的话，用户刚敲进去的字会跟着元素"跑"到别的行上去 ——
      // 这是虚拟滚动 + 就地编辑最经典的坑。
      if (edit && edit.rowEl) {
        const pi = pool.indexOf(edit.rowEl);
        if (pi < 0 || first + pi !== edit.idx) endEdit(true);
      }

      // 一次写入：整个视口的内容整体位移，不逐行改 top/left。
      rowsWrap.style.transform = "translateY(" + first * st.rowH + "px)";

      for (let i = 0; i < pool.length; i++) {
        const rowEl = pool[i];
        const idx = first + i;
        if (i >= count) {
          if (!rowEl.hidden) rowEl.hidden = true;
          rowEl._idx = -1; // 让它下次一定要重新绑定
          continue;
        }
        if (rowEl.hidden) rowEl.hidden = false;
        bindRow(rowEl, idx, force);
      }
      // 这里刻意不刷新工具条与表头状态：滚动每帧都跑，而它们只在
      // "选中/加载/排序"变化时才变 —— 那是各自的事件里去做的事。
      paintCursor();
      entering = false;
    }

    function schedule() {
      if (rafId || destroyed) return;
      rafId = requestAnimationFrame(() => {
        rafId = 0;
        render(false);
      });
    }

    // ============================================================
    // 表头状态（排序角标 / 全选）
    // ============================================================
    function paintHeadState() {
      const ths = hrow.querySelectorAll(".dbgrid-th[data-c]");
      for (const th of ths) {
        const i = Number(th.dataset.c);
        const col = st.columns[i];
        if (!col) continue;
        const dir = st.sort && st.sort.column === col.name ? st.sort.dir : "";
        if (dir) th.dataset.sort = dir;
        else delete th.dataset.sort;
        th.setAttribute("aria-sort", dir === "asc" ? "ascending" : dir === "desc" ? "descending" : "none");
      }
      const all = hrow.querySelector('[data-act="select-all"]');
      if (all) {
        const total = st.rows.length;
        // 没有选中任何行时不要遍历全部行 —— 20 万行 × 每次翻页都扫一遍
        // 会让"加载更多"变成秒级操作，而这只是为了一颗勾选框的状态。
        let picked = 0;
        if (st.selection.size) {
          for (let i = 0; i < total; i++) {
            const id = st.rowIds[i];
            if (id != null && st.selection.has(selKey(id))) picked++;
          }
        }
        all.checked = total > 0 && picked === total;
        all.indeterminate = picked > 0 && picked < total;
        all.disabled = !editable || !canDelete || total === 0;
        all.title = "全选已加载的 " + total + " 行";
      }
      // 工具条
      const loaded = st.rows.length;
      info.textContent = loaded ? "已加载 " + loaded.toLocaleString("zh-CN") + " 行" : "";
      const picked = st.selection.size;
      selInfo.hidden = picked === 0;
      selInfo.textContent = picked ? "已选 " + picked + " 行" : "";
      btnDel.textContent = picked ? "删除所选（" + picked + "）" : "删除所选";
      btnDel.disabled = picked === 0 || !canDelete;
      root.setAttribute("aria-rowcount", String(loaded + (newRow ? 2 : 1)));
      const foot = footText(loaded);
      footEl.textContent = foot.text;
      btnMore.hidden = !foot.more;
      btnMore.disabled = foot.busy;
    }

    function footText(loaded) {
      if (st.loading && loaded) return { text: "正在加载…", more: false, busy: true };
      if (st.loadError) return { text: "加载失败：" + st.loadError, more: true, busy: false };
      if (!loaded) return { text: "", more: !!st.hasMore, busy: false };
      if (st.hasMore) return { text: "下面还有更多", more: true, busy: false };
      return { text: "已经到底了", more: false, busy: false };
    }

    // ============================================================
    // 光标（键盘导航）
    // ============================================================
    function cellElOf(r, c) {
      const rowEl = rowElOfIdx(r);
      if (!rowEl || !rowEl._cells) return null;
      return rowEl._cells[c] || null;
    }

    function rowElOfIdx(r) {
      for (let i = 0; i < pool.length; i++) {
        if (pool[i]._idx === r && !pool[i].hidden) return pool[i];
      }
      return null;
    }

    function paintCursor() {
      const pos = st.cursorPos;
      const cell = pos ? cellElOf(pos.r, pos.c) : null;
      // 先把旧的标记清掉（元素可能已经被复用到别的行上）
      for (let i = 0; i < pool.length; i++) {
        const rowEl = pool[i];
        if (!rowEl._cursorCell) continue;
        rowEl._cursorCell.removeAttribute("data-cursor");
        rowEl._cursorCell.removeAttribute("id");
        rowEl._cursorCell = null;
      }
      if (!cell) {
        root.removeAttribute("aria-activedescendant");
        return;
      }
      cell.setAttribute("data-cursor", "true");
      cell.id = "dbgrid-activedesc";
      const rowEl = rowElOfIdx(pos.r);
      if (rowEl) rowEl._cursorCell = cell;
      root.setAttribute("aria-activedescendant", "dbgrid-activedesc");
    }

    function setCursor(r, c, ensureVisibleFlag) {
      const maxR = Math.max(0, st.rows.length - 1);
      const maxC = Math.max(0, st.columns.length - 1);
      const nr = clamp(r, 0, maxR);
      const nc = clamp(c, 0, maxC);
      st.cursorPos = st.rows.length ? { r: nr, c: nc } : null;
      if (ensureVisibleFlag !== false && st.cursorPos) ensureVisible(nr, nc);
      paintCursor();
    }

    function ensureVisible(r, c) {
      const rowH = st.rowH;
      const top = r * rowH;
      const bottom = top + rowH;
      const viewTop = scrollEl.scrollTop + st.headH;
      const viewBottom = scrollEl.scrollTop + scrollEl.clientHeight;
      if (top < viewTop) scrollEl.scrollTop = Math.max(0, top - st.headH);
      else if (bottom > viewBottom) scrollEl.scrollTop = bottom - scrollEl.clientHeight;
      if (c != null && st.columns.length) {
        const left = CHECK_W + colPrefix(c);
        const right = left + st.colW[c];
        const vLeft = scrollEl.scrollLeft;
        const vRight = vLeft + scrollEl.clientWidth;
        if (left < vLeft) scrollEl.scrollLeft = Math.max(0, left - CHECK_W);
        else if (right > vRight) scrollEl.scrollLeft = right - scrollEl.clientWidth + CHECK_W;
      }
    }

    function colPrefix(c) {
      let sum = 0;
      for (let i = 0; i < c; i++) sum += st.colW[i];
      return sum;
    }

    // ============================================================
    // 编辑
    // ============================================================
    function detachEditor(e) {
      if (!e) return;
      if (e.holder && e.holder.parentNode) e.holder.parentNode.removeChild(e.holder);
      else if (e.editor && e.editor.parentNode) e.editor.parentNode.removeChild(e.editor);
      if (e.modal) {
        // 模态编辑器由组件库托管，关闭即可
        if (e.modal.close) e.modal.close();
        e.modal = null;
      }
      if (e.cellEl) e.cellEl.removeAttribute("data-editing");
      e.editor = null;
      e.holder = null;
    }

    /**
     * 开始编辑一个格子。
     * @param {number} r 行号（st.rows 的下标）
     * @param {number} c 列号
     * @param {?string} seed 直接敲进来的第一个字符（表格习惯：打字即改写）
     */
    function beginEdit(r, c, seed) {
      if (!editable || !canCommit) return;
      const row = st.rows[r];
      const col = st.columns[c];
      if (row == null || !col) return;
      if (st.rowIds[r] == null) {
        toast("这一行没有主键，不能直接改。给表加一个主键列，或者用 opts.rowId 告诉网格怎么取行号。", "error");
        return;
      }
      if (edit) endEdit(true);
      setCursor(r, c);
      const rowEl = rowElOfIdx(r);
      const cellEl = rowEl && rowEl._cells ? rowEl._cells[c] : null;
      if (!cellEl) return;
      const kind = kindOf(col.type);
      const value = cellOf(row, col, c);
      const text = value == null ? "" : typeof value === "object" ? JSON.stringify(value) : String(value);
      // 超长文本用文本域 + 模态框：让一个 800 字的备注在 34px 高的格子里
      // 单行横着滚，是在折磨用户（docs/06 §5 的"行详情"也是同一个道理）。
      if (kind === "long" || text.length > LONG_TEXT_CHARS || text.indexOf("\n") >= 0) {
        openTextModal(r, c, text, value);
        return;
      }

      const session = {
        idx: r,
        col: c,
        rowEl: rowEl,
        cellEl: cellEl,
        pending: null,
        kind: kind,
        editor: null,
      };

      let editor;
      if (kind === "bool") {
        const U = ui();
        if (!U || !U.switchControl) {
          toast("开关组件不可用（components.js 未加载），布尔列先用文本框编辑", "error");
        }
        const truthy = value === true || value === 1 || value === "1";
        if (U && U.switchControl) {
          editor = U.switchControl(truthy, (next) => {
            // 开关是"一拨即生效"的控件，没有"回车提交"这一步：
            // 拨动本身就是提交动作。
            session.pending = next;
            endEdit(true);
          }, { label: col.name + " 布尔值", title: truthy ? "是 → 否" : "否 → 是" });
          editor.classList.add("dbgrid-switchcell");
        } else {
          editor = el("select", { class: "dbgrid-input" });
          editor.append(el("option", { value: "true" }, "是"), el("option", { value: "false" }, "否"));
          editor.value = truthy ? "true" : "false";
        }
      } else if (isNumericKind(kind)) {
        editor = el("input", { class: "dbgrid-input", type: "number", inputmode: "decimal", step: kind === "int" ? "1" : "any" });
        editor.value = value == null ? "" : String(value);
        // 数字列的空值只能是 NULL："" 在数字列里没有意义，
        // 而且它会悄悄绕过"非空"校验之外的一切数值检查。
        editor.placeholder = "NULL（未填写）";
      } else if (kind === "date" || kind === "datetime") {
        const withTime = kind === "datetime";
        editor = el("input", { class: "dbgrid-input", type: withTime ? "datetime-local" : "date" });
        editor.value = toDateInput(value, withTime);
        editor.placeholder = "NULL（未填写）";
      } else {
        editor = el("input", { class: "dbgrid-input", type: "text" });
        editor.value = text;
        // 占位文字是"这一格到底是 NULL 还是空串"的编辑期答案：
        // 屏幕上两者都是一片空白，只有这里能说清楚。
        editor.placeholder = value == null ? "NULL（未填写）" : "空字符串";
      }

      session.editor = editor;
      if (kind !== "bool") {
        const holder = el("div", { class: "dbgrid-editwrap" });
        holder.appendChild(editor);
        // NULL / 空串是两种不同的值，就必须有两条不同的路能回到它们。
        // 只靠"清空输入框"只能得到一种（而且用户猜不出是哪一种）。
        if (value != null) {
          const nullBtn = el("button", { class: "dbgrid-nullbtn", type: "button", title: "把这一格的值改成 NULL（未填写）" }, "NULL");
          nullBtn.addEventListener("mousedown", (ev) => {
            // mousedown 而不是 click：blur 会先一步把编辑提交掉
            ev.preventDefault();
            session.pending = null;
            session.setNull = true;
            endEdit(true);
          });
          holder.appendChild(nullBtn);
        }
        session.holder = holder;
      }

      cellEl.setAttribute("data-editing", "true");
      cellEl.appendChild(kind === "bool" ? editor : session.holder);
      if (typeof editor.focus === "function") {
        editor.focus({ preventScroll: true });
        if (seed != null && kind !== "bool") {
          editor.value = seed;
          const n = editor.value.length;
          // number / date 这类输入框在部分内核上禁止 setSelectionRange
          // （抛 InvalidStateError）。光标位置只是锦上添花，不能因为它
          // 把整个编辑流程打断。
          try {
            if (editor.setSelectionRange) editor.setSelectionRange(n, n);
          } catch (e) {
            /* 忽略：光标落到末尾即可 */
          }
        } else if (editor.select) {
          try {
            editor.select();
          } catch (e) {
            /* number 输入框在某些内核上不让全选，忽略 */
          }
        }
      }
      edit = session;
      editor._session = session;
      editor.addEventListener("keydown", onEditorKey);
      editor.addEventListener("blur", onEditorBlur, { once: true });
    }

    function onEditorKey(ev) {
      const e = ev.currentTarget._session;
      if (!e || e !== edit) return;
      const key = ev.key;
      if (key === "Enter") {
        ev.preventDefault();
        ev.stopPropagation();
        endEdit(true);
        setCursor(e.idx + 1, e.col);
        root.focus({ preventScroll: true });
        return;
      }
      if (key === "Escape") {
        ev.preventDefault();
        ev.stopPropagation();
        endEdit(false);
        root.focus({ preventScroll: true });
        return;
      }
      if (key === "Tab") {
        ev.preventDefault();
        ev.stopPropagation();
        endEdit(true);
        let r = e.idx;
        let c = e.col + (ev.shiftKey ? -1 : 1);
        if (c >= st.columns.length) {
          c = 0;
          r += 1;
        } else if (c < 0) {
          c = st.columns.length - 1;
          r -= 1;
        }
        setCursor(r, c);
        root.focus({ preventScroll: true });
        return;
      }
      if (key === "Delete" && (ev.ctrlKey || ev.metaKey)) {
        // Ctrl+Delete 在表格里是"清空这一格"的通用手势，这里清成 NULL
        ev.preventDefault();
        e.pending = null;
        e.setNull = true;
        endEdit(true);
        root.focus({ preventScroll: true });
      }
    }

    function onEditorBlur(ev) {
      const e = ev.currentTarget._session;
      if (!e || e !== edit) return;
      // 点击别处 = 提交（与表格软件一致）。取消只能靠 Esc。
      endEdit(true);
    }

    /**
     * 结束编辑。commit=true 时把值写回去（失败会回滚），false 时丢弃。
     */
    function endEdit(commit) {
      const e = edit;
      if (!e) return;
      edit = null; // 先清状态：下面的 DOM 操作会再触发 blur，不能递归进来
      const editor = e.editor;
      let raw = null;
      let hasNew = false;
      if (e.kind === "bool") {
        if (e.pending != null) {
          raw = e.pending;
          hasNew = true;
        }
      } else if (e.setNull) {
        raw = null;
        hasNew = true;
      } else if (editor) {
        raw = editor.value;
        hasNew = true;
      }
      detachEditor(e);
      if (!commit || !hasNew) {
        // 取消：把这一格重新画回原样（编辑期间可能被滚动复用动过）
        repaintRowAt(e.idx);
        return;
      }
      const col = st.columns[e.col];
      const kind = e.kind;
      let next;
      if (e.kind === "bool") {
        next = raw === true;
      } else if (kind === "date" || kind === "datetime") {
        next = fromDateInput(String(raw || ""), kind === "datetime");
      } else if (isNumericKind(kind)) {
        const s = String(raw == null ? "" : raw).trim();
        if (s === "") next = null;
        else {
          const n = Number(s);
          if (!isFinite(n)) {
            // 数字列里塞了非数字：直接拒绝并说明，不要把它当字符串写进库
            // —— 那会让"这一列是数字"这件事在某个格子突然不成立。
            toast("「" + s + "」不是数字，" + col.name + " 是数字列", "error");
            repaintRowAt(e.idx);
            return;
          }
          if (kind === "int" && Math.floor(n) !== n) {
            toast("「" + s + "」不是整数，" + col.name + " 是整数列", "error");
            repaintRowAt(e.idx);
            return;
          }
          next = n;
        }
      } else {
        next = String(raw == null ? "" : raw);
        // 文本列把空输入框理解为"空字符串"（用户看着它敲过、又删空了），
        // 想清成 NULL 请按那个 NULL 按钮 —— 两种意图不该靠猜。
        if (e.setNull) next = null;
      }
      commitValue(e.idx, e.col, next);
    }

    function repaintRowAt(r) {
      const rowEl = rowElOfIdx(r);
      if (!rowEl) return;
      for (let i = 0; i < st.columns.length; i++) paintCell(rowEl._cells[i], st.rows[r], st.columns[i], i);
    }

    function flashCell(r, c) {
      const cell = cellElOf(r, c);
      if (!cell) return;
      cell.removeAttribute("data-error");
      // 强制一次样式重算，让同名动画能重新播放（不然第二次失败看不到抖动）
      void cell.offsetWidth;
      cell.setAttribute("data-error", "true");
      setTimeout(() => cell.removeAttribute("data-error"), 400);
    }

    /**
     * 提交一格。乐观更新 + 失败回滚（docs/06 §5 的"提交即写入"）。
     * 为什么乐观：等一个来回（Rust → SQLite）再显示，用户会以为没点上，
     * 于是再双击一次 —— 那才是最糟的。
     */
    async function commitValue(r, c, next) {
      const row = st.rows[r];
      const col = st.columns[c];
      if (!row || !col) return;
      const id = st.rowIds[r];
      const prev = cellOf(row, col, c);
      if (prev === next) {
        repaintRowAt(r);
        return;
      }
      setCell(row, col, c, next);
      repaintRowAt(r);
      st.pending++;
      syncDirty();
      try {
        await cfg.commitCell({ rowId: id, column: col.name, value: next });
      } catch (err) {
        // 回滚到原值：界面上绝不能留下一个"看起来改成功了"的假值
        setCell(row, col, c, prev);
        repaintRowAt(r);
        flashCell(r, c);
        toast("保存失败：" + errText(err) + "，这一格已恢复成原来的值。", "error");
      } finally {
        st.pending--;
        syncDirty();
      }
    }

    /**
     * 超长文本 / JSON：用组件库的模态框 + 文本域。
     * 单元格保持原样不动，等用户在模态框里点保存才写。
     */
    function openTextModal(r, c, text, value) {
      const U = ui();
      const col = st.columns[c];
      const wrap = el("div", { class: "dbgrid-modal" });
      const hint = el("p", { class: "dbgrid-modal-hint" });
      hint.textContent =
        value == null
          ? "这一格现在是 NULL（未填写）。清空文本后点保存 = 空字符串；点「设为 NULL」= NULL。"
          : "现在是 " + text.length.toLocaleString("zh-CN") + " 个字符。换行会原样保存。";
      const ta = el("textarea", { class: "dbgrid-textarea", spellcheck: "false", "aria-label": col.name });
      ta.value = text;
      wrap.append(ta, hint);
      const apply = (next) => commitValue(r, c, next);

      if (U && U.modal) {
        U.modal({
          title: col.name + (col.comment ? "（" + col.comment + "）" : ""),
          body: wrap,
          actions: [
            { label: "取消", kind: "ghost" },
            { label: "设为 NULL", kind: "ghost", onClick: () => apply(null) },
            { label: "保存", kind: "primary", onClick: () => apply(ta.value) },
          ],
        });
        // 打开就把光标放进文本域：这是"改这一格"的唯一目的地
        ta.focus();
        const n = ta.value.length;
        if (ta.setSelectionRange) ta.setSelectionRange(n, n);
        return;
      }

      // 组件库没加载时的降级：就地展开一个文本域。不好看，但能改。
      const holder = el("div", { class: "dbgrid-editwrap dbgrid-modal-inline" });
      const bar2 = el("div", { class: "dbgrid-modal-acts" });
      const cancel = el("button", { class: "btn btn-ghost", type: "button" }, "取消");
      const toNull = el("button", { class: "btn btn-ghost", type: "button" }, "设为 NULL");
      const ok = el("button", { class: "btn btn-primary", type: "button" }, "保存");
      const close = () => holder.remove();
      cancel.addEventListener("click", close);
      toNull.addEventListener("click", () => {
        close();
        apply(null);
      });
      ok.addEventListener("click", () => {
        close();
        apply(ta.value);
      });
      ta.addEventListener("keydown", (ev) => {
        if (ev.key === "Escape") close();
      });
      bar2.append(cancel, toNull, ok);
      holder.append(ta, bar2);
      const rowEl = rowElOfIdx(r);
      if (rowEl && rowEl._cells[c]) rowEl._cells[c].appendChild(holder);
      ta.focus();
    }

    // ============================================================
    // 选择 / 批量删除
    // ============================================================
    function toggleRow(r, on) {
      const id = st.rowIds[r];
      if (id == null || !canDelete) return;
      const k = selKey(id);
      if (on == null) on = !st.selection.has(k);
      if (on) st.selection.add(k);
      else st.selection.delete(k);
      const rowEl = rowElOfIdx(r);
      if (rowEl) paintRowSelection(rowEl, r);
      paintHeadState();
    }

    function selectAll(on) {
      if (!canDelete) return;
      st.selection.clear();
      if (on) {
        for (let i = 0; i < st.rows.length; i++) {
          const id = st.rowIds[i];
          if (id != null) st.selection.add(selKey(id));
        }
      }
      render(true);
    }

    async function deleteSelected() {
      const ids = [];
      for (let i = 0; i < st.rows.length; i++) {
        const id = st.rowIds[i];
        if (id != null && st.selection.has(selKey(id))) ids.push({ id: id, index: i });
      }
      if (!ids.length) {
        toast("先勾选要删除的行");
        return;
      }
      const n = ids.length;
      // 破坏性操作必须说清楚三件事：删多少行、在哪张表、还能不能回来。
      const ok = await confirmBox({
        title: "删除 " + n + " 行？",
        body:
          "将从" + (st.table ? "「" + st.table + "」表" : "当前表") + "里删除 " + n + " 行。" +
          "删除后无法恢复。",
        confirmText: "删除 " + n + " 行",
        danger: true,
      });
      if (!ok) return;
      btnDel.disabled = true;
      try {
        await cfg.deleteRows(ids.map((x) => x.id));
      } catch (err) {
        toast("删除失败：" + errText(err) + "，数据没有变化。", "error");
        paintHeadState();
        return;
      }
      // 本地先摘掉：等重新拉一页再刷新会让用户盯着"已经删掉的行"好几秒。
      // 注意游标：keyset 的 after 指向最后一行，如果那行正好被删了，
      // 下一次 loadMore 拿到什么由调用方决定（它可以选择改用过滤条件重查）。
      const dropped = new Set(ids.map((x) => x.id));
      const rows = [];
      const rowIds = [];
      for (let i = 0; i < st.rows.length; i++) {
        const id = st.rowIds[i];
        if (id != null && dropped.has(id)) continue;
        rows.push(st.rows[i]);
        rowIds.push(id);
      }
      st.rows = rows;
      st.rowIds = rowIds;
      st.selection.clear();
      syncBodyHeight();
      render(true);
      paintHeadState();
      toast("已删除 " + n + " 行", "success");
      if (!st.rows.length && st.hasMore) loadMore();
    }

    // ============================================================
    // 表尾新增行
    // ============================================================
    function buildNewRow() {
      if (!editable || !canInsert || !st.columns.length) {
        newFields = [];
        return;
      }
      const row = el("div", { class: "dbgrid-row dbgrid-newrow", role: "row" });
      row.dataset.kind = "new"; // 事件里靠它把"新增行"和普通行区分开
      const ck = el("div", { class: "dbgrid-cell dbgrid-ck", "aria-hidden": "true" });
      ck.style.width = CHECK_W + "px";
      ck.appendChild(el("span", { class: "dbgrid-newmark" }, "+"));
      row.appendChild(ck);
      newFields = [];
      st.columns.forEach((c, i) => {
        const cell = el("div", { class: "dbgrid-cell dbgrid-editable" });
        cell.dataset.c = i;
        cell.style.width = "var(--dgc-" + i + ")";
        const kind = kindOf(c.type);
        const required = !!c.notNull && !isAutoColumn(c);
        const ph = required ? "必填" : isAutoColumn(c) ? "自动" : "可空";
        let field;
        if (kind === "bool") {
          const U = ui();
          if (U && U.switchControl) {
            field = U.switchControl(false, () => {
              field.dataset.touched = "1";
              st.newTouched = true;
              syncDirty();
            }, { label: c.name });
            field.classList.add("dbgrid-switchcell");
          } else {
            field = el("select", { class: "dbgrid-input" });
            field.append(el("option", { value: "" }, "—"), el("option", { value: "true" }, "是"), el("option", { value: "false" }, "否"));
          }
        } else if (isNumericKind(kind)) {
          field = el("input", { class: "dbgrid-input", type: "number", step: kind === "int" ? "1" : "any", placeholder: ph });
        } else if (kind === "date" || kind === "datetime") {
          field = el("input", { class: "dbgrid-input", type: kind === "datetime" ? "datetime-local" : "date", placeholder: ph });
        } else {
          field = el("input", { class: "dbgrid-input", type: "text", placeholder: ph });
        }
        field._col = c;
        field._kind = kind;
        field._required = required;
        field.dataset.c = i;
        field.addEventListener("input", () => {
          field.dataset.touched = "1";
          st.newTouched = true;
          syncDirty();
        });
        field.addEventListener("keydown", onNewKey);
        newFields.push(field);
        cell.appendChild(field);
        row.appendChild(cell);
      });
      let tips = "";
      if (st.columns.some((c) => c.notNull && !isAutoColumn(c))) tips = "带「必填」的列不能留空";
      const hintCell = el("div", { class: "dbgrid-cell dbgrid-fill dbgrid-newhint", "aria-hidden": "true" }, "填好后按 Enter 提交" + (tips ? "　·　" + tips : ""));
      row.appendChild(hintCell);
      newRow = row;
      bodyEl.appendChild(row);
      syncBodyHeight();
    }

    function onNewKey(ev) {
      if (ev.key === "Enter") {
        ev.preventDefault();
        ev.stopPropagation();
        commitNewRow();
        return;
      }
      if (ev.key === "Escape") {
        ev.preventDefault();
        clearNewRow();
        root.focus({ preventScroll: true });
        return;
      }
      if (ev.key === "Tab") {
        // Tab 在新增行里正常跳转，但提交时机不变（别把半填的一行提交上去）
        st.newTouched = true;
        syncDirty();
      }
    }

    function clearNewRow() {
      newFields.forEach((f) => {
        if (f.checked != null && f.tagName === "BUTTON") f.checked = false;
        else if (f.value != null) f.value = "";
        delete f.dataset.touched;
      });
      st.newTouched = false;
      syncDirty();
    }

    async function commitNewRow() {
      if (!canInsert) return;
      const values = {};
      let any = false;
      let bad = null;
      newFields.forEach((f) => {
        const c = f._col;
        const kind = f._kind;
        if (kind === "bool") {
          // 开关永远有值（false），但"没碰过"和"明确设成否"不是一回事：
          // 没碰过就不提交这一列，让数据库用它自己的默认值。
          if (f.dataset.touched) {
            values[c.name] = f.checked === true;
            any = true;
          }
          return;
        }
        const raw = String(f.value == null ? "" : f.value).trim();
        if (raw === "") {
          // 自增主键/带默认值的列不拦（它们本来就该空着等数据库给）
          if (f._required) bad = bad || c.name;
          return;
        }
        any = true;
        if (isNumericKind(kind)) values[c.name] = Number(raw);
        else if (kind === "date" || kind === "datetime") values[c.name] = fromDateInput(raw, kind === "datetime");
        else values[c.name] = raw;
      });
      if (bad) {
        toast("「" + bad + "」不允许为空，先填上再提交", "error");
        return;
      }
      if (!any) {
        toast("还没有填任何内容");
        return;
      }
      btnAdd.disabled = true;
      st.pending++;
      syncDirty();
      try {
        const created = await cfg.insertRow(values);
        clearNewRow();
        toast("已新增 1 行", "success");
        // 调用方如果把新建的那一行返回回来，就顺手接到表尾 ——
        // 用户不用刷新就能看到自己刚加的东西（也就不用重新定位）。
        if (created && typeof created === "object") {
          st.rows.push(created);
          st.rowIds.push(rowIdOf(created, st.rows.length - 1));
          syncBodyHeight();
          schedule();
        }
      } catch (err) {
        toast("新增失败：" + errText(err) + "，这一行还留在表尾，可以改完再提交。", "error");
      } finally {
        st.pending--;
        syncDirty();
        btnAdd.disabled = false;
      }
    }

    // ============================================================
    // 加载
    // ============================================================
    function query() {
      const filters = {};
      let hasFilter = false;
      for (const k in st.filters) {
        if (st.filters[k]) {
          filters[k] = st.filters[k];
          hasFilter = true;
        }
      }
      return {
        after: st.cursor,
        limit: pageSize,
        sort: st.sort ? { column: st.sort.column, dir: st.sort.dir } : null,
        filters: hasFilter ? filters : null,
      };
    }

    function hasFilter() {
      for (const k in st.filters) if (st.filters[k]) return true;
      return false;
    }

    function adoptColumns(cols) {
      if (!Array.isArray(cols) || !cols.length) return false;
      const norm = cols.map((c) => ({
        name: c && c.name != null ? String(c.name) : "",
        type: c && c.type != null ? String(c.type) : "",
        notNull: !!(c && c.notNull),
        comment: c && c.comment != null ? String(c.comment) : "",
      }));
      const same = colSignature() === norm.map((c) => c.name + ":" + c.type).join("|");
      if (same) return false;
      st.columns = norm;
      loadColWidths();
      buildHeader();
      rebuildRows();
      return true;
    }

    async function fetchPage(seq, append) {
      const q = query();
      const res = await cfg.loadPage(q);
      if (destroyed || seq !== loadSeq) return;
      const rows = res && Array.isArray(res.rows) ? res.rows : [];
      if (!append) {
        // 第一页：列信息可能随结果一起回来（调用方不一定在 opts.columns 里给全）
        if (Array.isArray(res && res.columns) && res.columns.length) adoptColumns(res.columns);
      }
      const base = append ? st.rows.length : 0;
      for (let i = 0; i < rows.length; i++) {
        st.rows.push(rows[i]);
        st.rowIds.push(rowIdOf(rows[i], base + i));
      }
      if (!append) {
        st.selection.clear();
        entering = true; // 入场动画只在这一帧给首屏那十几行
      }
      st.cursor = res && res.nextCursor != null ? res.nextCursor : st.cursor;
      let more = !!(res && res.hasMore);
      if (more && (st.cursor == null)) {
        // 说还有更多、却不给游标：再拉就会从第一页重新开始（重复行 + 死循环）。
        // 宁可停在"不再加载"，也不能让界面进入无限拉取。
        more = false;
        console.warn("[DeskBaseGrid] loadPage 返回 hasMore=true 但没有 nextCursor，已停止继续加载。");
      }
      st.hasMore = more;
      st.loadError = null;
      syncBodyHeight();
      if (!append) measure();
      // 追加时不必强制重画：可见的那些行号没变，bindRow 自己会跳过。
      // 1000 页就是 1000 次无谓的重画 —— 攒起来是秒级的白费力气。
      render(!append);
      paintHeadState();
      return true;
    }

    async function load(arg) {
      const append = !!(arg && arg.append);
      // 已经有一次加载在跑：把它返回出去，而不是立刻返回一个空的 Promise ——
      // 调用方（比如"滚到底自动加载"和"加载更多按钮"同时触发）会以为
      // 操作完成了，然后立刻再拉一次，结果拉了两遍同一页。
      if (st.loading) return inflight || Promise.resolve();
      const seq = ++loadSeq;
      st.loading = true;
      if (append) paintHeadState();
      else showSkeleton();
      const job = (async () => {
        try {
          await fetchPage(seq, append);
          if (destroyed || seq !== loadSeq) return; // 迟到的响应：什么都不做
          if (append) paintHeadState();
        } catch (err) {
          if (destroyed || seq !== loadSeq) return;
          st.loadError = errText(err);
          if (append) toast("加载更多失败：" + st.loadError, "error");
          else showError(st.loadError);
        } finally {
          if (seq === loadSeq) {
            st.loading = false;
            inflight = null;
            if (!destroyed) paintHeadState();
          }
        }
      })();
      inflight = job;
      return job;
    }

    /** 拉第一页，并按结果决定给用户看骨架、空态还是数据 */
    function loadFirst() {
      return load({ append: false }).then(() => {
        if (destroyed) return;
        if (st.loadError) return; // 错误态已经在 showError 里立起来了
        if (!st.rows.length) showEmpty();
        else hideVeil();
      });
    }

    function reload() {
      if (edit) endEdit(true);
      st.rows = [];
      st.rowIds = [];
      st.cursor = null;
      st.hasMore = true;
      st.loadError = null;
      st.selection.clear();
      loadSeq++; // 让在途的响应失效
      st.loading = false;
      inflight = null;
      if (scrollEl.scrollTop) scrollEl.scrollTop = 0;
      if (scrollEl.scrollLeft) scrollEl.scrollLeft = 0;
      syncBodyHeight();
      render(true);
      paintHeadState();
      return loadFirst();
    }

    function loadMore() {
      // 加载中不重复发起：load() 会把在途的那次返回给调用方
      if (!st.hasMore || (st.loadError && !st.rows.length)) return Promise.resolve();
      return load({ append: true });
    }

    function appendRow(row) {
      st.rows.push(row);
      st.rowIds.push(rowIdOf(row, st.rows.length - 1));
      syncBodyHeight();
      schedule();
    }

    // ============================================================
    // 覆盖层：骨架屏 / 空态 / 错误态
    // ============================================================
    function showSkeleton() {
      clearVeilContent();
      veil.hidden = false;
      veil.dataset.state = "loading";
      const U = ui();
      if (U && U.skeleton) {
        // 骨架屏只在"还没有任何数据"时出现；已经有数据时刷新走的是
        // "盖一层薄纱"的路子，不然用户会以为数据没了。
        skelHandle = U.skeleton(veil, { rows: Math.min(10, Math.max(4, Math.round(scrollEl.clientHeight / 40))) });
      } else {
        veil.appendChild(el("div", { class: "dbgrid-state" }, "正在加载…"));
      }
    }

    function clearVeilContent() {
      if (skelHandle) {
        if (skelHandle.remove) skelHandle.remove();
        skelHandle = null;
      }
      Array.prototype.slice.call(veil.children).forEach((n) => n.remove());
    }

    function hideVeil() {
      if (!veil || veil.hidden) return;
      clearVeilContent();
      veil.dataset.state = "leaving";
      // 淡出的时长问浏览器要（动效档位调到「关」时它会是 1ms），
      // 到点才真的从文档里拿掉，省得它继续挡住鼠标。
      const wait = msFromTransition(veil) + 40;
      clearTimeout(veilTimer);
      veilTimer = setTimeout(() => {
        // 只有在空态/错误态没有顶上来的时候才收起来
        if (veil.dataset.state === "leaving") veil.hidden = true;
      }, wait);
    }

    function showState(node) {
      clearVeilContent();
      clearTimeout(veilTimer);
      veil.hidden = false;
      veil.dataset.state = "show";
      veil.appendChild(node);
    }

    function showEmpty() {
      const box = el("div", { class: "dbgrid-state" });
      box.appendChild(el("div", { class: "dbgrid-state-mark" }, "空"));
      const h = el("h2", null, hasFilter() ? "没有符合筛选条件的行" : "这张表还没有数据");
      const p = el(
        "p",
        null,
        hasFilter()
          ? "把上面的筛选条件清掉，或者换一个关键词再试。"
          : editable && canInsert
            ? "点工具条上的「新增一行」，或者滚到表尾在「+」那一行里直接填。"
            : "这张表是空的，而且当前没有开启编辑。"
      );
      box.append(h, p);
      showState(box);
    }

    function showError(msg) {
      const box = el("div", { class: "dbgrid-state dbgrid-state-error" });
      box.appendChild(el("div", { class: "dbgrid-state-mark" }, "！"));
      box.appendChild(el("h2", null, "数据没加载出来"));
      box.appendChild(el("p", null, msg));
      box.appendChild(el("p", { class: "dbgrid-state-hint" }, "数据没有被修改。检查一下文件或权限，然后点「重试」。"));
      const retry = el("button", { class: "btn btn-primary", type: "button" }, "重试");
      retry.addEventListener("click", () => reload());
      box.appendChild(retry);
      showState(box);
    }

    // ============================================================
    // 表头交互：排序 / 列宽
    // ============================================================
    function cycleSort(i) {
      const col = st.columns[i];
      if (!col) return;
      // 三态：升 → 降 → 无。为什么要"无"这一态：没有它，用户想回到
      // "数据库默认顺序"就只能刷新页面。
      let next = { column: col.name, dir: "asc" };
      if (st.sort && st.sort.column === col.name) {
        if (st.sort.dir === "asc") next = { column: col.name, dir: "desc" };
        else next = null;
      }
      // 排序交给调用方（opts.loadPage 的 sort 参数）。本地只加载了一部分行，
      // 在本地排出来的顺序是"这一页的顺序"，不是这张表的顺序 —— 那是错的。
      st.sort = next;
      paintHeadState();
      reload();
    }

    let resizeRec = null;
    let swallowClick = false; // 拖完列宽后紧跟的那次 click 不能当成"点击表头排序"

    function beginResize(ev, i) {
      if (!isFinite(i) || st.colW[i] == null) return;
      ev.preventDefault();
      ev.stopPropagation();
      resizeRec = { i: i, startX: ev.clientX, startW: st.colW[i], moved: false };
      root.classList.add("is-resizing");
      // 监听挂在 window 上而不是元素上：指针一旦滑出那 7px 的把手，
      // 元素上的 pointermove 就断了，列宽会"跟丢"。
      window.addEventListener("pointermove", onResizeMove, true);
      window.addEventListener("pointerup", onResizeEnd, true);
      window.addEventListener("pointercancel", onResizeEnd, true);
    }

    function onResizeMove(ev) {
      if (!resizeRec) return;
      const dx = ev.clientX - resizeRec.startX;
      if (Math.abs(dx) > 2) resizeRec.moved = true;
      setColW(resizeRec.i, resizeRec.startW + dx);
    }

    function onResizeEnd() {
      if (!resizeRec) return;
      window.removeEventListener("pointermove", onResizeMove, true);
      window.removeEventListener("pointerup", onResizeEnd, true);
      window.removeEventListener("pointercancel", onResizeEnd, true);
      root.classList.remove("is-resizing");
      if (resizeRec.moved) {
        swallowClick = true;
        saveColWidths();
        // 列宽变了，表头高度可能因为换行而变，重新量一次
        measure();
      }
      resizeRec = null;
    }

    let measureCtx = null;

    /**
     * 双击分隔线：按内容自适应列宽。
     * 用 canvas 量文字而不是 DOM：DOM 量法要先把单元格改成 width:auto，
     * 那是"每量一列就重排一整屏"，在 20 万行的表上够呛。canvas 的度量
     * 与真实排版有 1–2px 的出入（字距微调、等宽数字的处理），所以留了余量。
     */
    function autoFit(i) {
      const col = st.columns[i];
      if (!col || !st.colW.length) return;
      const headCell = hrow.querySelector('.dbgrid-th[data-c="' + i + '"]');
      if (!measureCtx) measureCtx = document.createElement("canvas").getContext("2d");
      let widest = 0;
      const measure = (text) => {
        if (!text) return;
        const cs = getComputedStyle(headCell || root);
        measureCtx.font = cs.fontStyle + " " + cs.fontWeight + " " + cs.fontSize + " " + cs.fontFamily;
        widest = Math.max(widest, measureCtx.measureText(text).width);
      };
      measure(col.name);
      // 抽样而不是全量：已加载 20 万行时逐格量一遍要几秒，而"自适应列宽"
      // 是个"大概齐"的操作 —— 抽 400 行足够反映这列有多宽，用户也能立刻
      // 拿到结果。抽样是等距的，短值和长值都会轮到。
      const total = st.rows.length;
      const sample = 400;
      const stride = Math.max(1, Math.floor(total / sample));
      for (let r = 0; r < total; r += stride) {
        const v = cellOf(st.rows[r], col, i);
        if (v == null) continue;
        measure(typeof v === "object" ? JSON.stringify(v) : String(v));
      }
      // 加上内边距、右边框、排序角标的位置，再留一点富余
      const w = Math.ceil(widest) + 34 + (col.type ? 26 : 0);
      setColW(i, w || st.colW[i]);
      saveColWidths();
      measure();
    }

    // ============================================================
    // 事件
    // ============================================================
    function onScroll() {
      // 滚动事件按帧合并：每像素都算一次的话，20 万行下必掉帧。
      schedule();
    }

    function onPointerDown(ev) {
      const t = ev.target;
      if (!t || typeof t.closest !== "function") return;
      if (t.classList.contains("dbgrid-check")) return;
      if (t.closest(".dbgrid-editable")) return;
      // 点哪一格就选哪一格（键盘的起点）。preventScroll 避免"点一下页面跳一下"。
      if (typeof root.focus === "function") root.focus({ preventScroll: true });
      const cell = t.closest(".dbgrid-cell[data-c]");
      const rowEl = t.closest(".dbgrid-row[data-i]");
      if (cell && rowEl && !rowEl.hidden) {
        const r = Number(rowEl.dataset.i);
        const c = Number(cell.dataset.c);
        if (isFinite(r) && isFinite(c)) setCursor(r, c, false);
      }
    }

    function onDblClick(ev) {
      const t = ev.target;
      if (!t || typeof t.closest !== "function") return;
      const cell = t.closest(".dbgrid-cell[data-c]");
      const rowEl = t.closest(".dbgrid-row[data-i]");
      if (!cell || !rowEl || rowEl.dataset.kind === "new") return;
      if (!editable) return;
      const r = Number(rowEl.dataset.i);
      const c = Number(cell.dataset.c);
      if (isFinite(r) && isFinite(c)) beginEdit(r, c, null);
    }

    function onChange(ev) {
      const t = ev.target;
      if (!t || !t.classList) return;
      if (t.classList.contains("dbgrid-check")) {
        const rowEl = t.closest(".dbgrid-row[data-i]");
        if (!rowEl || rowEl.dataset.kind === "new") return;
        const r = Number(rowEl.dataset.i);
        if (isFinite(r)) toggleRow(r, t.checked);
        paintHeadState();
        return;
      }
      if (t.dataset && t.dataset.act === "select-all") {
        selectAll(t.checked);
      }
    }

    function onHeaderClick(ev) {
      const t = ev.target;
      if (!t || typeof t.closest !== "function") return;
      if (swallowClick) {
        swallowClick = false;
        return;
      }
      if (t.closest(".dbgrid-rz") || t.classList.contains("dbgrid-check")) return;
      const th = t.closest(".dbgrid-th[data-c]");
      if (!th) return;
      cycleSort(Number(th.dataset.c));
    }

    function onHeaderDblClick(ev) {
      const t = ev.target;
      if (!t || typeof t.closest !== "function") return;
      const rz = t.closest(".dbgrid-rz");
      if (rz) {
        ev.preventDefault();
        autoFit(Number(rz.dataset.c));
        return;
      }
      // 双击表头空白处不做事（避免误触发排序）
      if (t.closest(".dbgrid-th[data-c]")) ev.preventDefault();
    }

    function onHeaderDown(ev) {
      const t = ev.target;
      if (!t || typeof t.closest !== "function") return;
      const rz = t.closest(".dbgrid-rz");
      if (rz) {
        beginResize(ev, Number(rz.dataset.c));
        return;
      }
      // 在没有把手的地方按下：清掉"吞掉下一次点击"的标记
      swallowClick = false;
    }

    function onFilterInput(ev) {
      const t = ev.target;
      if (!t || !t.dataset || t.dataset.c == null) return;
      const i = Number(t.dataset.c);
      const col = st.columns[i];
      if (!col) return;
      clearTimeout(filterTimer);
      const val = t.value;
      filterTimer = setTimeout(() => {
        st.filters[col.name] = String(val).trim();
        reload();
      }, FILTER_DEBOUNCE);
    }

    function onFilterKey(ev) {
      if (ev.key !== "Enter") return;
      ev.preventDefault();
      ev.stopPropagation();
      clearTimeout(filterTimer);
      const t = ev.target;
      const i = Number(t.dataset.c);
      const col = st.columns[i];
      if (col) {
        st.filters[col.name] = String(t.value).trim();
        reload();
      }
    }

    function onKeyDown(ev) {
      // 编辑器自己处理键盘（它们会 stopPropagation，这里是第二道保险）
      if (ev.target && typeof ev.target.closest === "function" && ev.target.closest(".dbgrid-input, .dbgrid-switchcell, .dbgrid-textarea")) return;
      if (!st.rows.length) return;
      const pos = st.cursorPos || { r: 0, c: 0 };
      let handled = true;
      switch (ev.key) {
        case "ArrowDown":
          setCursor(pos.r + 1, pos.c);
          break;
        case "ArrowUp":
          setCursor(pos.r - 1, pos.c);
          break;
        case "ArrowRight":
          setCursor(pos.r, pos.c + 1);
          break;
        case "ArrowLeft":
          setCursor(pos.r, pos.c - 1);
          break;
        case "Home":
          setCursor(ev.ctrlKey ? 0 : pos.r, ev.ctrlKey ? 0 : 0);
          break;
        case "End":
          setCursor(ev.ctrlKey ? st.rows.length - 1 : pos.r, st.columns.length - 1);
          break;
        case "PageDown":
          setCursor(pos.r + Math.max(1, Math.floor((scrollEl.clientHeight - st.headH) / st.rowH)), pos.c);
          break;
        case "PageUp":
          setCursor(pos.r - Math.max(1, Math.floor((scrollEl.clientHeight - st.headH) / st.rowH)), pos.c);
          break;
        case "Enter":
        case "F2":
          beginEdit(pos.r, pos.c, null);
          break;
        case " ":
        case "Spacebar":
          toggleRow(pos.r);
          break;
        case "Escape":
          if (st.selection.size) {
            st.selection.clear();
            render(true);
          }
          break;
        case "a":
        case "A":
          if (ev.ctrlKey || ev.metaKey) selectAll(true);
          else handled = false;
          break;
        case "c":
        case "C":
          if (ev.ctrlKey || ev.metaKey) copyCell(pos.r, pos.c);
          else handled = false;
          break;
        case "Delete":
        case "Backspace":
          if (st.selection.size) deleteSelected();
          else handled = false;
          break;
        default:
          if (ev.key.length === 1 && !ev.ctrlKey && !ev.metaKey && !ev.altKey) {
            // 打字即改写（表格软件的通用手势）：不用先双击
            beginEdit(pos.r, pos.c, ev.key);
          } else {
            handled = false;
          }
      }
      if (handled) {
        ev.preventDefault();
        // 网格处理掉的键不要再往上冒：应用层可能有自己的全局快捷键
        ev.stopPropagation();
      }
    }

    function copyCell(r, c) {
      const col = st.columns[c];
      if (!col) return;
      const v = cellOf(st.rows[r], col, c);
      // NULL 与空串在剪贴板上必须还是两种东西：NULL 是一个空单元格，
      // 空串也是 —— 但用户想复制的往往是"看得见的那个值"，
      // 所以 NULL 复制成空字符串（Excel 里粘出来就是空），并且明说一句。
      const text = v == null ? "" : typeof v === "object" ? JSON.stringify(v) : String(v);
      const done = () => toast(v == null ? "这一格是 NULL，已复制成空" : "已复制这一格的内容", "success");
      try {
        if (navigator.clipboard && navigator.clipboard.writeText) {
          navigator.clipboard.writeText(text).then(done, () => fallbackCopy(text, done));
        } else {
          fallbackCopy(text, done);
        }
      } catch (e) {
        fallbackCopy(text, done);
      }
    }

    function fallbackCopy(text, done) {
      const ta = el("textarea", { class: "dbgrid-copybuf" });
      ta.value = text;
      document.body.appendChild(ta);
      ta.select();
      let ok = false;
      try {
        ok = document.execCommand("copy");
      } catch (e) {
        ok = false;
      }
      ta.remove();
      if (ok) done();
      else toast("复制失败：当前环境不允许访问剪贴板", "error");
    }

    function onFocusIn() {
      root.dataset.focus = "true";
      if (!st.cursorPos && st.rows.length) setCursor(0, 0, false);
      else paintCursor();
    }

    function onFocusOut(ev) {
      const next = ev.relatedTarget;
      if (next && root.contains(next)) return;
      delete root.dataset.focus;
      paintCursor();
    }

    function onWindowResize() {
      measure();
      render(true);
    }

    function onDocClick(ev) {
      // 点空白处收起光标（保持"焦点在哪一目了然"）
      if (!root.contains(ev.target) && st.cursorPos) {
        st.cursorPos = null;
        paintCursor();
      }
    }

    // 主题/密度/动效档位变了 → --row-h 可能变 → 行位置全部要重算。
    // 用 MutationObserver 而不是定时轮询：主题切换是"事件"。
    const themeMo = new MutationObserver(() => {
      const before = st.rowH;
      measure();
      render(true);
      if (before !== st.rowH) paintHeadState();
    });

    const ro = typeof ResizeObserver === "function" ? new ResizeObserver(() => {
      measure();
      render(true);
    }) : null;

    // ---------- 挂监听 ----------
    scrollEl.addEventListener("scroll", onScroll, { passive: true });
    root.addEventListener("pointerdown", onPointerDown);
    rowsWrap.addEventListener("dblclick", onDblClick);
    rowsWrap.addEventListener("change", onChange);
    hrow.addEventListener("click", onHeaderClick);
    hrow.addEventListener("dblclick", onHeaderDblClick);
    hrow.addEventListener("pointerdown", onHeaderDown);
    frow.addEventListener("input", onFilterInput);
    frow.addEventListener("keydown", onFilterKey);
    root.addEventListener("keydown", onKeyDown);
    root.addEventListener("focusin", onFocusIn);
    root.addEventListener("focusout", onFocusOut);
    btnReload.addEventListener("click", () => reload());
    btnAdd.addEventListener("click", () => {
      if (!newRow) buildNewRow();
      syncBodyHeight();
      scrollEl.scrollTop = scrollEl.scrollHeight;
      const f = newFields[0];
      if (f && f.focus) f.focus({ preventScroll: true });
    });
    btnDel.addEventListener("click", () => deleteSelected());
    window.addEventListener("resize", onWindowResize);
    document.addEventListener("pointerdown", onDocClick, true);
    if (ro) ro.observe(root);
    themeMo.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme", "data-motion", "data-texture", "data-density", "data-heading"] });

    function syncDirty() {
      // dirty = "有还没落库的东西"：在途的提交 + 表尾新增行里填过的内容。
      st.dirty = st.pending > 0 || st.newTouched;
    }
    st.dirty = false;

    // ============================================================
    // 启动
    // ============================================================
    loadColWidths();
    buildHeader();
    buildNewRow();
    measure();
    loadFirst();

    // ============================================================
    // 实例
    // ============================================================
    return {
      /** 当前状态快照（rows/columns 是内部数组的引用，不要就地改） */
      state() {
        return {
          table: st.table,
          columns: st.columns,
          rows: st.rows,
          cursor: st.cursor,
          hasMore: st.hasMore,
          selection: Array.from(st.selection),
          dirty: st.dirty,
        };
      },
      /** 重新拉第一页（筛选/排序变化后也走它） */
      reload() {
        return reload();
      },
      /** 拉下一页。分页参数（after/limit）与游标都由调用方负责 */
      loadMore() {
        return loadMore();
      },
      /** 滚动并选中某一行 */
      goto(rowIndex) {
        const want = clamp(Math.round(Number(rowIndex) || 0), 0, Math.max(0, st.rows.length - 1));
        const jump = () => {
          const rowH = st.rowH;
          // 让它落在视口中部：贴着上下边缘选中，用户看不清前后文
          const target = Math.max(0, want * rowH - Math.floor((scrollEl.clientHeight - st.headH) / 2 - rowH / 2));
          scrollEl.scrollTop = target;
          setCursor(want, st.cursorPos ? st.cursorPos.c : 0, false);
          render(false);
        };
        if (want < st.rows.length || !st.hasMore) {
          jump();
          return Promise.resolve(want);
        }
        // 目标还没加载出来：一页页拉到它出现为止（有上限，别把界面卡死）
        let guard = 0;
        const pump = () => {
          if (destroyed || want < st.rows.length || !st.hasMore || guard > 200) {
            jump();
            return Promise.resolve(Math.min(want, Math.max(0, st.rows.length - 1)));
          }
          guard++;
          return loadMore().then(pump);
        };
        return pump();
      },
      /** 卸载：摘掉全部监听与观察者，把 DOM 还回去 */
      destroy() {
        if (destroyed) return;
        destroyed = true;
        loadSeq++;
        inflight = null;
        if (rafId) cancelAnimationFrame(rafId);
        rafId = 0;
        if (edit) endEdit(false);
        clearTimeout(filterTimer);
        clearTimeout(veilTimer);
        clearVeilContent();
        themeMo.disconnect();
        if (ro) ro.disconnect();
        onResizeEnd();
        window.removeEventListener("resize", onWindowResize);
        document.removeEventListener("pointerdown", onDocClick, true);
        scrollEl.removeEventListener("scroll", onScroll);
        root.removeEventListener("pointerdown", onPointerDown);
        rowsWrap.removeEventListener("dblclick", onDblClick);
        hrow.removeEventListener("click", onHeaderClick);
        hrow.removeEventListener("dblclick", onHeaderDblClick);
        hrow.removeEventListener("pointerdown", onHeaderDown);
        frow.removeEventListener("input", onFilterInput);
        frow.removeEventListener("keydown", onFilterKey);
        root.removeEventListener("keydown", onKeyDown);
        root.removeEventListener("focusin", onFocusIn);
        root.removeEventListener("focusout", onFocusOut);
        root.remove();
        pool = [];
        newFields = [];
        st.rows = [];
        st.rowIds = [];
        st.selection.clear();
      },
    };
  }

  // ============================================================
  // 导出
  // ============================================================
  window.DeskBaseGrid = { mount: mount };
})();
