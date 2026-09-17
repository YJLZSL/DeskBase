/* ============================================================
   DeskBase SQL 编辑器（高级入口）
   ============================================================
   对外只暴露 window.DeskBaseSql = { mount(container, opts) }。
   实例方法：getValue / setValue / focus / destroy / history。

   ## 为什么是自包含的一个文件

   零依赖、零构建 —— 整个 UI 是原生 HTML/CSS/JS 经 deskbase:// 加载，没有
   打包器也没有 npm。所以语法高亮、语句切分、危险操作识别全部自己写，
   一个第三方库都不引（含 CodeMirror / Monaco）。DOM 全部运行时创建，
   宿主页面上不需要任何挂载标记。

   ## 高亮为什么是「textarea 叠在 pre 上」而不是 contenteditable

   contenteditable 会把浏览器的编辑行为整个接管：选区、输入法、撤销栈、
   拼写检查，全都要自己实现一遍，中文输入法下尤其容易崩。textarea 是唯一
   能白拿这些的平台原生输入控件。于是：

     .dbsql-code
       ├─ <pre class="dbsql-hl">          ← 着色后的同一段文字（不吃指针事件）
       └─ <textarea class="dbsql-input">  ← 真正的输入控件：文字透明、光标可见

   两层的字体/行高/内边距/white-space 必须逐像素一致（这些属性在 sql.css
   里写在同一条组内选择器里，改一处等于同时改两层）。滚动由 textarea 负责，
   pre 只跟随它的 scrollTop / scrollLeft。

   ## 一条语句 = 一次 runQuery

   用户会整段粘贴。默认只执行**光标所在的那一条**（用 `;` 切分；字符串里的
   分号、注释里的分号都不算分号），另给「执行全部」。光标所在的那条在编辑区
   带底色高亮，行号槽同步标出它的行范围 —— "要执行的是哪条"必须看得见。

   ## 危险操作拦截是最后一道闸

   DROP / TRUNCATE / 无 WHERE 的 DELETE / 无 WHERE 的 UPDATE 在真正送出去
   之前一律二次确认，并把命中的那个词在弹窗里显式标出来（SQL 预览里也标在
   它出现的位置上）。识别基于词法分析，所以字符串和注释里的 "delete" 不会
   误报；反过来，拿不准的时候宁可多问一次，不能少问。

   ## history() 的形状

   [{ sql, at, ok, ms, rows }]，时间倒序，最多 50 条，存在 localStorage 的
   deskbase.sql.history 下。存储不可用时退回内存（并 console.warn，
   不静默）。

   ## 已知不足（写在最前面，免得被当成 bug）

   1. 自动补全 / 括号匹配 / 格式化 / 多光标 / 参数化弹窗 / 结果文本视图
      （docs/06 §3.3 里的进阶项）都没有做 —— 本次只交付"能安全地跑 SQL"。
   2. 危险操作只做二次确认，没有按 docs/06 §11 的要求额外"要求输入对象名"。
   3. 高亮是**词法级**的：不建语法树。所以表名/字段名只在"与 listTables
      给的名字完全一致"时才着色；别名、CTE 名、派生列不会被识别成列。
   4. 危险判定只看语句文本：`UPDATE t SET x=1 WHERE 1=1` 也算"有 WHERE"，
      静态文本判断不出条件恒真。子查询里出现 WHERE 也可能让 UPDATE 漏判。
   5. 单条 SQL 超过 60000 字符时关闭语法高亮（显示纯文本），避免每次按键
      都做全量词法分析导致输入卡顿。
   ============================================================ */
(function () {
  "use strict";

  // 同一份脚本被加载两次时（WebView 里出现过重复注入），第二次直接退出
  if (window.DeskBaseSql) return;

  // ============================================================
  // 样式注入
  // ============================================================
  const STYLE_ID = "dbsql-styles";

  /* 以脚本自身的位置解析 CSS 路径（而不是 location.href）：两个文件是兄弟
     关系，用 currentScript.src 当基准，将来挪进子目录也依然找得到。
     必须在**模块求值时**取一次：mount() 是后来才被调用的，那时
     document.currentScript 已经是 null。 */
  const cssHref = (function () {
    const self = document.currentScript && document.currentScript.src;
    try {
      return new URL("sql.css", self || document.baseURI).href;
    } catch (e) {
      return "sql.css";
    }
  })();

  function injectStyles() {
    if (document.getElementById(STYLE_ID)) return; // 幂等
    const link = document.createElement("link");
    link.id = STYLE_ID;
    link.rel = "stylesheet";
    link.href = cssHref;
    link.addEventListener("error", function () {
      // 样式没加载成功时，两层会各自按 UA 默认值排布 —— 文字直接错开。
      // 这种失败必须响亮（docs/18 第 20 条：静默失败禁止）。
      console.error(
        "[DeskBaseSql] sql.css 加载失败：" + cssHref +
          "\n  deskbase:// 只服务编译期登记过的资源 —— 需要在 app/src/assets.rs 的 " +
          "lookup() 资源表里登记 /sql.css 与 /sql.js。"
      );
    });
    document.head.appendChild(link);
  }

  // ============================================================
  // 小工具
  // ============================================================
  function el(tag, cls, attrs) {
    const node = document.createElement(tag);
    if (cls) node.className = cls;
    if (attrs) {
      for (const k in attrs) {
        if (attrs[k] == null) continue;
        node.setAttribute(k, attrs[k]);
      }
    }
    return node;
  }

  /** 建元素 + 设文本。用户数据一律走 textContent，绝不 innerHTML。 */
  function cell(tag, cls, text) {
    const node = document.createElement(tag);
    if (cls) node.className = cls;
    node.textContent = text;
    return node;
  }

  /* 转义会破坏 HTML 结构的四个字符。用一次 replace 而不是链式 replace：
     链式会把前一步换出来的 & 再换一遍。 */
  const ESC_MAP = { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" };
  function esc(s) {
    return String(s).replace(/[&<>"]/g, function (c) {
      return ESC_MAP[c];
    });
  }

  function clamp(v, lo, hi) {
    return v < lo ? lo : v > hi ? hi : v;
  }

  function clip(s, n) {
    s = String(s == null ? "" : s);
    return s.length > n ? s.slice(0, n - 1) + "…" : s;
  }

  /** 把任意抛出物变成一句能显示、能拿去搜的文本 */
  function errText(err) {
    if (err == null) return "未知错误";
    if (typeof err === "string") return err;
    if (typeof err.message === "string" && err.message) return err.message;
    if (typeof err.error === "string") return err.error; // IPC 层常见的形状
    try {
      return JSON.stringify(err);
    } catch (e) {
      return String(err);
    }
  }

  function svgNode(paths) {
    const NS = "http://www.w3.org/2000/svg";
    const s = document.createElementNS(NS, "svg");
    s.setAttribute("class", "dbsql-ico");
    s.setAttribute("viewBox", "0 0 20 20");
    s.setAttribute("aria-hidden", "true");
    for (const d of paths) {
      const p = document.createElementNS(NS, "path");
      p.setAttribute("d", d);
      s.appendChild(p);
    }
    return s;
  }

  const ICON = {
    run: ["M6 4.4 15 10l-9 5.6z"],
    runAll: ["M5 5h11M5 10h11M5 15h7"],
    side: ["M3.4 4.6h13.2v10.8H3.4z", "M8 4.8v10.4"],
    chevron: ["M8 5.6 12.2 10 8 14.4"],
  };

  // ============================================================
  // 一、关键字表
  // ============================================================
  /* 抄 SQLite 官方关键字表（sqlite.org/lang_keywords.html）。自己加词的代价是
     "把用户的表名当成关键字染色"，所以只额外补了 TRUNCATE：它不是 SQLite 的
     关键字，但用户从 MySQL 粘过来时它是**危险词**，得能被着色、能被拦下。 */
  const KEYWORDS = new Set(
    (
      "ABORT ACTION ADD AFTER ALL ALTER ALWAYS ANALYZE AND AS ASC ATTACH AUTOINCREMENT " +
      "BEFORE BEGIN BETWEEN BY CASCADE CASE CAST CHECK COLLATE COLUMN COMMIT CONFLICT " +
      "CONSTRAINT CREATE CROSS CURRENT CURRENT_DATE CURRENT_TIME CURRENT_TIMESTAMP " +
      "DATABASE DEFAULT DEFERRABLE DEFERRED DELETE DESC DETACH DISTINCT DO DROP EACH ELSE " +
      "END ESCAPE EXCEPT EXCLUDE EXCLUSIVE EXISTS EXPLAIN FAIL FILTER FIRST FOLLOWING FOR " +
      "FOREIGN FROM FULL GENERATED GLOB GROUP GROUPS HAVING IF IGNORE IMMEDIATE IN INDEX " +
      "INDEXED INITIALLY INNER INSERT INSTEAD INTERSECT INTO IS ISNULL JOIN KEY LAST LEFT " +
      "LIKE LIMIT MATCH MATERIALIZED NATURAL NO NOT NOTHING NOTNULL NULL NULLS OF OFFSET " +
      "ON OR ORDER OTHERS OUTER OVER PARTITION PLAN PRAGMA PRECEDING PRIMARY QUERY RAISE " +
      "RANGE RECURSIVE REFERENCES REGEXP REINDEX RELEASE RENAME REPLACE RESTRICT RETURNING " +
      "RIGHT ROLLBACK ROW ROWS SAVEPOINT SELECT SET TABLE TEMP TEMPORARY THEN TIES TO " +
      "TRANSACTION TRIGGER UNBOUNDED UNION UNIQUE UPDATE USING VACUUM VALUES VIEW VIRTUAL " +
      "WHEN WHERE WINDOW WITH WITHOUT TRUNCATE"
    ).split(" ")
  );

  // ============================================================
  // 二、词法分析
  // ============================================================
  /* 为什么要自己写 tokenizer，而不是几条正则 replace：
     正则**同时**匹配字符串和注释里的内容 —— 会把 '-- 这是注释' 里的字染色，
     会把字符串里的 ';' 当成分号去切语句。词法分析扫一次，高亮 / 切句 /
     危险词识别三处共用同一份结果，三处逻辑不可能打架。

     token 用极短的键名（k/s/e）：一段两万字符的 SQL 会产生几千个 token，
     每个字段名省几个字节是有意义的，而且它们从不出这个闭包。 */

  const K_COM = "com";
  const K_STR = "str";
  const K_QID = "qid";
  const K_NUM = "num";
  const K_PARAM = "param";
  const K_PUNCT = "p";
  const K_IDENT = "i";

  function isSpace(c) {
    return c === 32 || c === 9 || c === 10 || c === 13 || c === 12 || c === 11;
  }
  function isDigit(c) {
    return c >= 48 && c <= 57;
  }
  function isHex(c) {
    return isDigit(c) || (c >= 97 && c <= 102) || (c >= 65 && c <= 70);
  }
  /* 标识符首字符：ASCII 字母、下划线、以及**一切非 ASCII**。
     最后一条是中文库能用的前提 —— 表名、字段名常常就是中文。 */
  function isIdentStart(c) {
    return (c >= 65 && c <= 90) || (c >= 97 && c <= 122) || c === 95 || c >= 0x80;
  }
  function isIdentPart(c) {
    return isIdentStart(c) || isDigit(c) || c === 36; // $ 可以出现在标识符里
  }

  function tokenize(src) {
    const toks = [];
    const n = src.length;
    let i = 0;

    while (i < n) {
      const c = src.charCodeAt(i);

      // 空白不产 token：渲染时按原字符拷贝，省掉几千次分配。
      // （下面 renderHighlight 依赖这条：token 之间的空隙**只可能是空白**，
      //   所以那些片段可以直接拼进 HTML，不必转义。）
      if (isSpace(c)) {
        i++;
        continue;
      }

      // -- 行注释
      if (c === 45 && src.charCodeAt(i + 1) === 45) {
        let j = src.indexOf("\n", i);
        if (j < 0) j = n;
        toks.push({ k: K_COM, s: i, e: j });
        i = j;
        continue;
      }
      // /* 块注释 */（SQLite 不支持嵌套；未闭合时吃到结尾，别把界面搞崩）
      if (c === 47 && src.charCodeAt(i + 1) === 42) {
        const j = src.indexOf("*/", i + 2);
        const end = j < 0 ? n : j + 2;
        toks.push({ k: K_COM, s: i, e: end });
        i = end;
        continue;
      }
      // 字符串与带引号的标识符。'' 与 "" 是转义写法，不能当成结束。
      if (c === 39 || c === 34 || c === 96) {
        let j = i + 1;
        while (j < n) {
          if (src.charCodeAt(j) === c) {
            if (src.charCodeAt(j + 1) === c) {
              j += 2;
              continue;
            }
            j++;
            break;
          }
          j++;
        }
        if (j > n) j = n;
        toks.push({ k: c === 39 ? K_STR : K_QID, s: i, e: j });
        i = j;
        continue;
      }
      // [方括号标识符]（SQLite 也认这种 MS Access 风格）
      if (c === 91) {
        const j = src.indexOf("]", i + 1);
        const end = j < 0 ? n : j + 1;
        toks.push({ k: K_QID, s: i, e: end });
        i = end;
        continue;
      }
      // 数字：0x1f / 12 / 1.5 / 1e-3 / .5
      if (isDigit(c) || (c === 46 && isDigit(src.charCodeAt(i + 1)))) {
        let j = i + 1;
        if (c === 48 && (src.charCodeAt(j) === 120 || src.charCodeAt(j) === 88)) {
          j++;
          while (j < n && isHex(src.charCodeAt(j))) j++;
        } else {
          while (j < n && isDigit(src.charCodeAt(j))) j++;
          if (src.charCodeAt(j) === 46) {
            j++;
            while (j < n && isDigit(src.charCodeAt(j))) j++;
          }
          const ex = src.charCodeAt(j);
          if (ex === 101 || ex === 69) {
            let k = j + 1;
            if (src.charCodeAt(k) === 43 || src.charCodeAt(k) === 45) k++;
            if (isDigit(src.charCodeAt(k))) {
              k++;
              while (k < n && isDigit(src.charCodeAt(k))) k++;
              j = k;
            }
          }
        }
        toks.push({ k: K_NUM, s: i, e: j });
        i = j;
        continue;
      }
      // 绑定参数：? :name @name $name（docs/06 §3.3 的参数化查询用 :参数名）
      if (
        c === 63 ||
        ((c === 58 || c === 64 || c === 36) && isIdentStart(src.charCodeAt(i + 1)))
      ) {
        let j = i + 1;
        while (j < n && isIdentPart(src.charCodeAt(j))) j++;
        toks.push({ k: K_PARAM, s: i, e: j });
        i = j;
        continue;
      }
      // 标识符 / 关键字（后面再按表名、列名、关键字表判定）
      if (isIdentStart(c)) {
        let j = i + 1;
        while (j < n && isIdentPart(src.charCodeAt(j))) j++;
        toks.push({ k: K_IDENT, s: i, e: j });
        i = j;
        continue;
      }
      // 其余一律单字符标点（括号、逗号、运算符、分号……）
      toks.push({ k: K_PUNCT, s: i, e: i + 1 });
      i++;
    }
    return toks;
  }

  /** 偏移 → 行号（0 基）。lineStarts 递增，二分即可。 */
  function lineOf(lineStarts, pos) {
    let lo = 0;
    let hi = lineStarts.length - 1;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (lineStarts[mid] <= pos) lo = mid;
      else hi = mid - 1;
    }
    return lo;
  }

  // ============================================================
  // 三、切分语句
  // ============================================================
  /**
   * 按顶层分号切分。三条要点：
   *   1. 只有 `p` 类 token 且是 `;`、且不在括号里才算分界 ——
   *      字符串/注释里的分号压根不是 `p` token，天然被排除。
   *   2. 每个片段记住它在原文里的**区间**：光标归属判定、当前语句底色、
   *      行号槽标范围、错误定位，全都靠它。
   *   3. 只有注释 / 空白 / 孤立分号的片段不算一条语句（SQLite 也不执行
   *      空语句），否则"第 N 条"会数错。
   */
  function splitStatements(src, toks, lineStarts) {
    const cuts = [];
    let depth = 0;
    for (let i = 0; i < toks.length; i++) {
      const t = toks[i];
      if (t.k !== K_PUNCT) continue;
      const ch = src[t.s];
      if (ch === "(") depth++;
      else if (ch === ")") {
        if (depth > 0) depth--;
      } else if (ch === ";" && depth === 0) cuts.push(t.s);
    }

    const segs = [];
    let from = 0;
    for (let i = 0; i < cuts.length; i++) {
      segs.push({ lo: from, hi: cuts[i], tokFrom: -1, tokTo: -1, hasCode: false });
      from = cuts[i] + 1;
    }
    segs.push({ lo: from, hi: src.length, tokFrom: -1, tokTo: -1, hasCode: false });

    // 把每个 token 归到它所属的片段：等于"小于它的分界个数"。
    // 分号自己归给**它结束的那一段**（分界位置 = 分号下标，cuts[ci] < t.s
    // 对分号自己不成立，所以它落在 ci 这一段里）。
    let ci = 0;
    for (let i = 0; i < toks.length; i++) {
      const t = toks[i];
      while (ci < cuts.length && cuts[ci] < t.s) ci++;
      const seg = segs[ci];
      if (!seg) break;
      if (t.k === K_COM) continue;
      if (t.k === K_PUNCT && src[t.s] === ";") {
        if (seg.tokFrom < 0) seg.tokFrom = i;
        seg.tokTo = i;
        continue;
      }
      if (seg.tokFrom < 0) seg.tokFrom = i;
      seg.tokTo = i;
      if (t.k !== K_PUNCT) seg.hasCode = true;
    }

    const out = [];
    for (let i = 0; i < segs.length; i++) {
      const s = segs[i];
      if (!s.hasCode) continue; // 空片段 / 纯注释 / 孤立分号
      const rawText = src.slice(s.lo, s.hi);
      out.push({
        lo: s.lo, // 片段起点（含前导空白），光标归属判定用
        hi: s.hi, // 片段终点（= 分号位置本身）
        tokFrom: s.tokFrom, // 高亮"当前语句"用的 token 下标范围
        tokTo: s.tokTo,
        raw: rawText, // 原样子串：危险词高亮要按它的偏移来标
        // 送去执行的文本：去掉首尾空白。中间的注释保留 —— SQLite 自己会跳过，
        // 而"我改了用户的 SQL 才发出去"这件事不值得做。
        sql: rawText.trim(),
        lineFrom: lineOf(lineStarts, s.lo),
        lineTo: lineOf(lineStarts, s.hi),
      });
    }
    return out;
  }

  function analyze(src) {
    const toks = tokenize(src);
    const lineStarts = [0];
    for (let i = 0; i < src.length; i++) {
      if (src.charCodeAt(i) === 10) lineStarts.push(i + 1);
    }
    return {
      toks: toks,
      stmts: splitStatements(src, toks, lineStarts),
      lineStarts: lineStarts,
      lineCount: lineStarts.length,
    };
  }

  /** 光标落在第几条语句上。落在语句之间的空白里算"前一条"（最符合直觉）。 */
  function statementAt(stmts, cursor) {
    let before = -1;
    for (let i = 0; i < stmts.length; i++) {
      const st = stmts[i];
      if (cursor >= st.lo && cursor <= st.hi) return i;
      if (st.hi < cursor) before = i;
    }
    if (before >= 0) return before;
    return stmts.length ? 0 : -1;
  }

  // ============================================================
  // 四、危险操作识别
  // ============================================================
  /* 判据（docs/06 §3.3 危险操作拦截 + §11 安全红线）：
       DROP（任意对象，含 ALTER ... DROP）
       TRUNCATE
       DELETE 没有 WHERE
       UPDATE 没有 WHERE
     全部只看**词法 token**：字符串和注释里的这些词不算数。否则
     `INSERT INTO log(content) VALUES('delete from 客户')` 会被拦下来，
     误拦多了用户就会习惯性点"确定"，闸就废了 —— 那比不拦更糟。 */

  const OBJECT_KINDS = new Set(["TABLE", "INDEX", "VIEW", "TRIGGER", "COLUMN"]);

  function upperOf(src, t) {
    return src.slice(t.s, t.e).toUpperCase();
  }

  function isWordToken(t) {
    return t.k !== K_COM && t.k !== K_STR && t.k !== K_QID;
  }

  /** 返回第一个匹配词的下标（只认标识符类 token），没有则 -1 */
  function hasWord(src, list, word) {
    for (let i = 0; i < list.length; i++) {
      const t = list[i];
      if (!isWordToken(t)) continue;
      if (t.k === K_IDENT && upperOf(src, t) === word) return i;
    }
    return -1;
  }

  /**
   * 判断一条语句是不是危险操作。
   * @param {string} src 整篇 SQL 原文（token 的 s/e 是这篇里的绝对偏移）
   * @param {Array} list 这条语句范围内的 token
   * @returns {null | {word, kind, at, len, target, why}}
   */
  function detectDanger(src, list) {
    for (let i = 0; i < list.length; i++) {
      const t = list[i];
      if (!isWordToken(t)) continue;
      if (t.k !== K_IDENT) continue;
      const w = upperOf(src, t);

      // 1) DROP：任何 DROP 都问一次。拿保留字当标识符（`SELECT drop FROM t`）
      //    在没有引号时 SQLite 本来也会报语法错，误报面极小。
      if (w === "DROP") {
        // 对象名：DROP 后面第一个"不是关键字"的名字（跳过 TABLE/INDEX 这类
        // 对象类型词，也跳过 IF / EXISTS / TEMP 这些修饰词）。
        let target = null;
        for (let j = i + 1; j < list.length; j++) {
          const n = list[j];
          if (n.k === K_COM) continue;
          if (n.k !== K_IDENT && n.k !== K_QID) break;
          const nw = upperOf(src, n);
          if (n.k === K_IDENT && (OBJECT_KINDS.has(nw) || KEYWORDS.has(nw))) continue;
          target = src.slice(n.s, n.e).replace(/^["`[]|["`\]]$/g, "");
          break;
        }
        return {
          word: "DROP",
          kind: "drop",
          at: t.s,
          len: t.e - t.s,
          target: target,
          why: "DROP 会把对象连同里面的数据一起删掉，而且没有撤销。",
        };
      }
      if (w === "TRUNCATE") {
        return {
          word: "TRUNCATE",
          kind: "truncate",
          at: t.s,
          len: t.e - t.s,
          target: null,
          why: "TRUNCATE 会清空整张表。",
        };
      }
    }

    // 2) DELETE / UPDATE 没有 WHERE
    const del = hasWord(src, list, "DELETE");
    if (del >= 0 && hasWord(src, list, "WHERE") < 0) {
      const hasLimit = hasWord(src, list, "LIMIT") >= 0;
      return {
        word: "DELETE",
        kind: "delete",
        at: list[del].s,
        len: list[del].e - list[del].s,
        target: null,
        why:
          "这条 DELETE 没有 WHERE，会把整张表清空。" +
          (hasLimit ? "（虽然带了 LIMIT，也只保证「最多删这么多行」）" : ""),
      };
    }
    const upd = hasWord(src, list, "UPDATE");
    if (upd >= 0 && hasWord(src, list, "WHERE") < 0) {
      return {
        word: "UPDATE",
        kind: "update",
        at: list[upd].s,
        len: list[upd].e - list[upd].s,
        target: null,
        why: "这条 UPDATE 没有 WHERE，会把表里每一行的这个字段都改掉。",
      };
    }
    return null;
  }

  // ============================================================
  // 五、错误解释
  // ============================================================
  /* 原则（docs/06 §3.3「错误提示」）：
     原始报错**原样显示**，一个字不改 —— 用户要拿它去搜。
     人话的猜测加在它上面，且只在真的猜得出来时才加；猜不出来就闭嘴。 */

  const MATCH_LIMIT = 2; // 编辑距离超过这个数就不算"像"，不猜

  /** 有界编辑距离：超过 max 立刻放弃（避免在两个长串上做全量 DP） */
  function near(a, b, max) {
    if (a === b) return 0;
    if (Math.abs(a.length - b.length) > max) return max + 1;
    const prev = new Array(b.length + 1);
    const cur = new Array(b.length + 1);
    for (let j = 0; j <= b.length; j++) prev[j] = j;
    for (let i = 1; i <= a.length; i++) {
      cur[0] = i;
      let best = i;
      for (let j = 1; j <= b.length; j++) {
        const cost = a.charCodeAt(i - 1) === b.charCodeAt(j - 1) ? 0 : 1;
        cur[j] = Math.min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + cost);
        if (cur[j] < best) best = cur[j];
      }
      if (best > max) return max + 1;
      for (let j = 0; j <= b.length; j++) prev[j] = cur[j];
    }
    return prev[b.length];
  }

  /** 在候选里找唯一一个"很像"的名字。没有唯一赢家就返回 null —— 不猜。 */
  function closest(name, candidates) {
    const lower = String(name).toLowerCase();
    let best = null;
    let bestD = MATCH_LIMIT + 1;
    let tie = false;
    for (let i = 0; i < candidates.length; i++) {
      const c = candidates[i];
      const cl = String(c).toLowerCase();
      if (cl === lower) return null; // 一模一样：不是拼错，别乱指
      const d = near(lower, cl, MATCH_LIMIT);
      if (d < bestD) {
        bestD = d;
        best = c;
        tie = false;
      } else if (d === bestD && best != null) {
        tie = true;
      }
    }
    return bestD <= MATCH_LIMIT && !tie ? best : null;
  }

  /** 把绝对偏移换算成行列号。只在出错时调用，所以按需现算行首表。 */
  function positionText(fullSrc, absPos, st) {
    let line = 1;
    let lastStart = 0;
    for (let i = 0; i < absPos && i < fullSrc.length; i++) {
      if (fullSrc.charCodeAt(i) === 10) {
        line++;
        lastStart = i + 1;
      }
    }
    const col = absPos - lastStart + 1;
    return (
      "第 " + line + " 行第 " + col + " 列" +
      (st ? "（本语句从第 " + (st.lineFrom + 1) + " 行开始）" : "")
    );
  }

  /** 在整条语句里定位一个片段。出现不止一次时不给位置 —— 指错了比不指更坏。 */
  function locateInSource(src, needle, st) {
    if (!needle) return null;
    const from = st ? st.lo : 0;
    const to = st ? st.hi : src.length;
    let hit = -1;
    let count = 0;
    let p = src.indexOf(needle, from);
    while (p >= 0 && p <= to) {
      count++;
      hit = p;
      if (count > 1) break;
      p = src.indexOf(needle, p + 1);
    }
    if (count !== 1) return null;
    return positionText(src, hit, st);
  }

  /**
   * @param {string} raw SQLite 原始报错
   * @param {{tables: string[], columns: string[], src: string, stmt: object}} ctx
   * @returns {{guess: string|null, loc: string|null}}
   */
  function explainError(raw, ctx) {
    const text = String(raw);
    let m;

    if ((m = /no such table:\s*(.+)/i.exec(text))) {
      const name = m[1].trim();
      const like = closest(name, ctx.tables);
      return {
        guess:
          "表「" + name + "」不存在。" +
          (like ? "你是想写「" + like + "」吗？" : "可能是名字拼错了，或者这张表还没建。"),
        loc: locateInSource(ctx.src, name, ctx.stmt),
      };
    }
    if ((m = /no such column:\s*(.+)/i.exec(text))) {
      const name = m[1].trim().replace(/^.*\./, "");
      const like = closest(name, ctx.columns);
      return {
        guess:
          "字段「" + name + "」不存在。" +
          (like ? "你是想写「" + like + "」吗？" : "检查一下名字，或者它属于另一张表。"),
        loc: locateInSource(ctx.src, name, ctx.stmt),
      };
    }
    if ((m = /no such function:\s*(.+)/i.exec(text))) {
      return {
        guess: "函数「" + m[1].trim() + "」不存在。SQLite 内置函数有限，名字也要拼对。",
        loc: null,
      };
    }
    if ((m = /(?:table|view|index|trigger)\s+(.+?)\s+already exists/i.exec(text))) {
      return { guess: "「" + m[1].trim() + "」已经存在了。要重建就先删掉，或者换个名字。", loc: null };
    }
    if ((m = /UNIQUE constraint failed:\s*(.+)/i.exec(text))) {
      return { guess: "唯一约束没过：「" + m[1].trim() + "」里出现了重复值。", loc: null };
    }
    if ((m = /NOT NULL constraint failed:\s*(.+)/i.exec(text))) {
      return { guess: "非空约束没过：「" + m[1].trim() + "」不能留空。", loc: null };
    }
    if ((m = /CHECK constraint failed:?\s*(.*)/i.exec(text))) {
      const what = m[1].trim();
      return {
        guess: "检查约束没过" + (what ? "：「" + what + "」" : "") + "。这条数据不满足表上定义的条件。",
        loc: null,
      };
    }
    if (/FOREIGN KEY constraint failed/i.test(text)) {
      return {
        guess: "外键约束没过：要么引用的那条记录不存在，要么这条记录还被别的表引用着，动不了。",
        loc: null,
      };
    }
    if (/database is locked/i.test(text)) {
      return {
        guess: "数据库被占用了（可能另一个窗口或另一个程序正在写）。等一两秒再试，或者把那边关掉。",
        loc: null,
      };
    }
    if (/readonly database|attempt to write a readonly/i.test(text)) {
      return { guess: "这个库是以只读方式打开的，写不进去。", loc: null };
    }
    if (/interrupted/i.test(text)) {
      return { guess: "查询被中断了（超时或被取消）。可以加上 LIMIT 再试。", loc: null };
    }
    if (/incomplete input/i.test(text)) {
      return { guess: "SQL 没写完 —— 多半是引号或括号没有闭合。", loc: null };
    }
    if ((m = /near\s+"([^"]*)":\s*syntax error/i.exec(text))) {
      const tok = m[1];
      return {
        guess:
          "语法错误：SQLite 在「" + tok + "」这里读不懂了（多半是它前面少了逗号、括号没配平，或者关键字拼错）。",
        loc: locateInSource(ctx.src, tok, ctx.stmt),
      };
    }
    if ((m = /unrecognized token:\s*"([^"]*)"/i.exec(text))) {
      return {
        guess:
          "有个字符 SQLite 不认识：「" + m[1] + "」。常见于从 Excel / Word 粘过来的弯引号、全角符号。",
        loc: locateInSource(ctx.src, m[1], ctx.stmt),
      };
    }
    if ((m = /(\d+)\s+columns?\s+but\s+(\d+)\s+values?/i.exec(text))) {
      return {
        guess: "INSERT 的值的个数（" + m[2] + "）和表的字段个数（" + m[1] + "）对不上。",
        loc: null,
      };
    }
    if (/ambiguous column name/i.test(text)) {
      return { guess: "字段名有歧义：多张表里都有同名列，要写成 表名.字段名。", loc: null };
    }
    // 认不出来就**不猜**。宁可只有一行原始报错，也不要给一句听着有理的废话。
    return { guess: null, loc: null };
  }

  // ============================================================
  // 六、执行历史
  // ============================================================
  const HISTORY_KEY = "deskbase.sql.history";
  const HISTORY_MAX = 50;

  /** @returns {Array|null} null = 存储不可用（调用方退回内存态） */
  function readHistory() {
    try {
      const raw = window.localStorage.getItem(HISTORY_KEY);
      if (!raw) return [];
      const arr = JSON.parse(raw);
      // 读的时候也截断：存里要是躺着超量的旧数据（或被别的版本改过），
      // "最多 50 条"这个承诺在界面上仍然成立。
      return Array.isArray(arr)
        ? arr.filter((x) => x && typeof x.sql === "string").slice(0, HISTORY_MAX)
        : [];
    } catch (e) {
      // 存储被禁用 / JSON 损坏都不该让编辑器打不开：退回内存态，但要说一声
      console.warn("[DeskBaseSql] 读取执行历史失败，本次会话改为只在内存里记：", e);
      return null;
    }
  }

  // ============================================================
  // 七、高亮渲染
  // ============================================================
  /** 单条 SQL 超过这个长度就关掉高亮：每次按键做全量词法分析会明显卡手 */
  const MAX_HIGHLIGHT = 60000;

  /* token 类名统一带 tok- 前缀，和组件自身的类名（dbsql-col 是侧栏的字段按钮、
     dbsql-tbl 是侧栏的表小节）**必须分开** —— 否则字段名 token 会套上侧栏
     按钮的 display:flex / width:100% / padding，一整段文字就被拆成一行一个
     单词。这个坑真的踩过一次：高亮层与输入层当场错位几百像素。

     加粗只给**纯 ASCII 的词元**，原因也是对齐：等宽英文字体的粗体推进宽度
     与常规完全一致（实测 UPDATE：45.70 vs 45.70），但中文回退字体没有真正的
     粗体，浏览器要**合成加粗**，而合成加粗会把每个字加宽约 1px（实测
     "客户" 多出 2px）。textarea 永远只画常规体，于是加粗的中文会让它后面
     整行往右漂 —— 表名全是中文的库里，这个漂移会一路累积。 */
  const ASCII_ONLY = /^[\x20-\x7e]+$/;
  function boldIfSafe(text) {
    return ASCII_ONLY.test(text) ? " dbsql-tok-b" : "";
  }
  function tokenClass(src, t, vocab) {
    switch (t.k) {
      case K_COM:
        return "dbsql-tok-com";
      case K_STR:
        return "dbsql-tok-str";
      case K_NUM:
        return "dbsql-tok-num";
      case K_PARAM:
        return "dbsql-tok-param";
      case K_PUNCT:
        return "";
      default:
        break;
    }
    const text = src.slice(t.s, t.e);
    if (t.k === K_QID) {
      // 带引号的名字：引号里的内容也要按表名/列名判定
      const inner = text.replace(/^["`[]|["`\]]$/g, "");
      if (vocab.tables.has(inner.toLowerCase())) return "dbsql-tok-tbl" + boldIfSafe(inner);
      if (vocab.cols.has(inner.toLowerCase())) return "dbsql-tok-col";
      return "dbsql-tok-qid";
    }
    /* 关键字优先于表名/列名，这是**语义上正确**的顺序：一个不加引号的
       SQL 保留字不可能是合法的表名/列名（SQLite 会直接报语法错），
       所以 ORDER / KEY 这类词只可能是关键字。反过来先查表名的话，
       库里有个叫 order 的字段就会把 `ORDER BY` 染成字段色。 */
    const up = text.toUpperCase();
    if (KEYWORDS.has(up)) return "dbsql-tok-kw" + boldIfSafe(text);
    const low = text.toLowerCase();
    if (vocab.tables.has(low)) return "dbsql-tok-tbl" + boldIfSafe(text);
    if (vocab.cols.has(low)) return "dbsql-tok-col";
    // 后面紧跟 ( 的标识符按函数着色（覆盖 count() / sum() 这类内置函数）
    let j = t.e;
    while (j < src.length && isSpace(src.charCodeAt(j))) j++;
    if (src.charCodeAt(j) === 40) return "dbsql-tok-fn";
    return "";
  }

  /* 这是全文件唯一拼 HTML 字符串的地方（前面 cell() 里写了"用户数据一律走
     textContent"）。两个前提让它安全：
       1. SQL 原文本身就来自用户的编辑区，拼进 DOM 不产生新的权限问题；
       2. 每一个 token 的文本都过 esc()，而 token 之间的空隙经 tokenize 保证
          只可能是空白字符。也就是说这里不存在"未转义的用户输入"。
     之所以用字符串而不是 createElement：一次按键要重建几千个 span，
     逐个建节点的开销在长 SQL 上肉眼可见。 */
  function renderHighlight(src, toks, vocab, active) {
    const from = active ? active.tokFrom : -1;
    const to = active ? active.tokTo : -1;
    let out = "";
    let pos = 0;
    let open = false;
    for (let i = 0; i < toks.length; i++) {
      const t = toks[i];
      // token 之间只可能是空白（见 tokenize 里"空白不产 token"的说明），
      // 所以这段原文可以原样拼进 HTML，不必转义。
      if (t.s > pos) out += src.slice(pos, t.s);
      if (i === from) {
        out += '<span class="dbsql-tok-stmt">';
        open = true;
      }
      const cls = tokenClass(src, t, vocab);
      const text = esc(src.slice(t.s, t.e));
      out += cls ? '<span class="' + cls + '">' + text + "</span>" : text;
      if (open && i === to) {
        out += "</span>";
        open = false;
      }
      pos = t.e;
    }
    if (open) out += "</span>";
    if (pos < src.length) out += src.slice(pos);
    /* 末尾补一个换行：文本以 \n 结尾时，textarea 里还有一个空行，pre 里
       没有这个换行就少渲染一行 —— 两层的滚动高度会差一行，滚到底时高亮
       文字整体上移一格。补上只会让 pre 的滚动范围**不小于** textarea
       （多出来的那点永远滚不到，因为滚动位置是从 textarea 复制的）。 */
    return out + "\n";
  }

  // ============================================================
  // 八、mount
  // ============================================================
  const RENDER_ROW_CAP = 20000; // 渲染兜底上限：调用方已按 maxRows 截过，这里只防手滑
  const SLOW_MS = 1000; // docs/06 §3.3：慢查询阈值 1 秒
  const TITLE_MAX = 120;

  function mount(container, opts) {
    const host = typeof container === "string" ? document.querySelector(container) : container;
    if (!host || typeof host.appendChild !== "function") {
      console.error("[DeskBaseSql] mount() 的第一个参数必须是元素或选择器：", container);
      return deadInstance();
    }
    injectStyles();

    const cfg = opts || {};
    const maxRows =
      typeof cfg.maxRows === "number" && cfg.maxRows > 0 ? Math.floor(cfg.maxRows) : 5000;
    /* 只读判据交给调用方：这里只读 cfg.readonly，且**每次要用时现读**，
       所以调用方拿到 opts 后直接改 opts.readonly = true 就能生效
       （不需要额外的 setReadonly API）。 */
    const isReadonly = () => cfg.readonly === true;

    const listeners = new AbortController();
    const on = (node, type, fn, opt) =>
      node.addEventListener(type, fn, Object.assign({ signal: listeners.signal }, opt || {}));

    // ---------- 状态 ----------
    const state = {
      src: "",
      toks: [],
      stmts: [],
      lineStarts: [0],
      lineCount: 1,
      active: -1, // 光标所在语句下标
      busy: false,
      plain: false, // 超长文本：关闭高亮
      disposed: false,
      totalMs: 0,
      emptyNote: "",
      vocab: { tables: new Set(), cols: new Set() },
      tables: [],
      tableNames: [],
      columnNames: [],
      hist: readHistory(),
      raf: 0,
    };
    let histAvailable = state.hist !== null;
    if (!histAvailable) state.hist = [];

    // ---------- DOM ----------
    const root = el("div", "dbsql", { "data-side": "on", "data-tab": "result" });

    // 工具栏
    const toolbar = el("div", "dbsql-toolbar");
    const btnRun = el("button", "dbsql-btn dbsql-btn-primary", { type: "button" });
    btnRun.appendChild(svgNode(ICON.run));
    btnRun.appendChild(cell("span", null, "执行"));
    btnRun.title = "执行光标所在的那一条（Ctrl+Enter）";
    const btnRunAll = el("button", "dbsql-btn", { type: "button" });
    btnRunAll.appendChild(svgNode(ICON.runAll));
    btnRunAll.appendChild(cell("span", null, "执行全部"));
    btnRunAll.title = "按顺序执行编辑区里的每一条语句（Ctrl+Shift+Enter）";

    const status = el("div", "dbsql-status", {
      "data-state": "idle",
      role: "status",
      "aria-live": "polite",
    });
    const statusDot = el("span", "dbsql-status-dot");
    const statusText = cell("span", null, "就绪");
    status.appendChild(statusDot);
    status.appendChild(statusText);

    const badgeReadonly = el("span", "dbsql-badge", { "data-kind": "calm", hidden: "" });
    badgeReadonly.textContent = "只读模式";
    const badgeTime = el("span", "dbsql-badge", { hidden: "" });

    const btnSide = el("button", "dbsql-btn dbsql-btn-ghost", {
      type: "button",
      "aria-pressed": "true",
      title: "显示 / 隐藏表名侧栏",
    });
    btnSide.appendChild(svgNode(ICON.side));
    btnSide.appendChild(cell("span", null, "表"));

    toolbar.appendChild(btnRun);
    toolbar.appendChild(btnRunAll);
    toolbar.appendChild(status);
    toolbar.appendChild(el("div", "dbsql-spacer"));
    toolbar.appendChild(badgeReadonly);
    toolbar.appendChild(badgeTime);
    toolbar.appendChild(cell("span", "dbsql-hint", "Ctrl+Enter"));
    toolbar.appendChild(btnSide);

    // 表名侧栏
    const side = el("aside", "dbsql-side");
    const sideInner = el("div", "dbsql-side-inner");
    const sideHead = el("div", "dbsql-side-head");
    sideHead.appendChild(cell("span", null, "表与字段"));
    const sideList = el("div", "dbsql-side-list");
    sideInner.appendChild(sideHead);
    sideInner.appendChild(sideList);
    side.appendChild(sideInner);

    // 编辑区
    const pane = el("div", "dbsql-pane");
    const editorBox = el("div", "dbsql-editorbox");
    const gutter = el("div", "dbsql-gutter", { "aria-hidden": "true" });
    const gutterInner = el("div", "dbsql-gutter-inner");
    gutter.appendChild(gutterInner);
    const code = el("div", "dbsql-code");
    const pre = el("pre", "dbsql-hl", { "aria-hidden": "true" });
    const preCode = el("code");
    pre.appendChild(preCode);
    const ta = el("textarea", "dbsql-input", {
      spellcheck: "false",
      autocomplete: "off",
      autocapitalize: "off",
      autocorrect: "off",
      wrap: "off", // 不软换行：行号槽才能和逻辑行一一对应
      "aria-label": "SQL 输入区",
    });
    code.appendChild(pre);
    code.appendChild(ta);
    editorBox.appendChild(gutter);
    editorBox.appendChild(code);

    const split = el("div", "dbsql-split", {
      role: "separator",
      "aria-label": "调整编辑区高度",
    });

    // 结果 / 历史
    const outwrap = el("div", "dbsql-outwrap");
    const tabs = el("div", "dbsql-tabs", { role: "tablist" });
    const tabResult = el("button", "dbsql-tab", {
      type: "button",
      role: "tab",
      "aria-selected": "true",
    });
    tabResult.textContent = "结果";
    const tabHistory = el("button", "dbsql-tab", {
      type: "button",
      role: "tab",
      "aria-selected": "false",
    });
    const tabSpacer = el("div", "dbsql-spacer");
    const stmtHint = cell("span", "dbsql-hint", "");
    tabs.appendChild(tabResult);
    tabs.appendChild(tabHistory);
    tabs.appendChild(tabSpacer);
    tabs.appendChild(stmtHint);
    const out = el("div", "dbsql-out");
    outwrap.appendChild(tabs);
    outwrap.appendChild(out);

    pane.appendChild(editorBox);
    pane.appendChild(split);
    pane.appendChild(outwrap);

    const main = el("div", "dbsql-main");
    main.appendChild(side);
    main.appendChild(pane);

    root.appendChild(toolbar);
    root.appendChild(main);
    host.appendChild(root);

    // 空态 / 结果 / 历史三个面板
    const emptyBox = el("div", "dbsql-empty");
    emptyBox.appendChild(cell("p", null, "在上面的编辑区写 SQL，Ctrl+Enter 执行光标所在的那一条。"));
    const emptySub = cell("p", null, "");
    emptyBox.appendChild(emptySub);
    const resultList = el("div", "dbsql-results");
    const historyList = el("div", "dbsql-history");

    function paintOut() {
      out.textContent = "";
      if (root.dataset.tab === "history") {
        out.appendChild(historyList);
        return;
      }
      out.appendChild(resultList.childElementCount ? resultList : emptyBox);
      emptySub.textContent = state.emptyNote || "";
      emptySub.hidden = !state.emptyNote;
    }

    function showTab(which) {
      root.dataset.tab = which;
      tabResult.setAttribute("aria-selected", which === "result" ? "true" : "false");
      tabHistory.setAttribute("aria-selected", which === "history" ? "true" : "false");
      paintOut();
      if (which === "history") renderHistory();
    }

    // ---------- 状态显示 ----------
    function setStatus(kind, text) {
      status.dataset.state = kind;
      statusText.textContent = text;
    }

    function setBusy(busy, text) {
      state.busy = busy;
      root.setAttribute("aria-busy", busy ? "true" : "false");
      const ro = isReadonly();
      btnRun.disabled = busy || ro;
      btnRunAll.disabled = busy || ro;
      if (text) setStatus(busy ? "running" : "idle", text);
    }

    function syncReadonly() {
      const ro = isReadonly();
      badgeReadonly.hidden = !ro;
      badgeReadonly.title = ro
        ? "只读模式：执行按钮被禁用，避免误改数据。要执行写操作得让上层切回可写模式。"
        : "";
      root.dataset.readonly = ro ? "true" : "false";
      if (!state.busy) {
        btnRun.disabled = ro;
        btnRunAll.disabled = ro;
      }
      state.emptyNote = ro
        ? "当前是只读模式：执行按钮被禁用了。SQL 仍然可以写、可以看着色，只是送不出去。"
        : "";
      btnRun.title = ro
        ? "只读模式：执行被禁用（判据在调用方的 runQuery）"
        : "执行光标所在的那一条（Ctrl+Enter）";
      if (!state.busy) setStatus("idle", ro ? "只读模式：执行已禁用" : "就绪");
      paintOut();
    }

    // ---------- 表名侧栏 ----------
    /** 需要引号的名字才加引号（中文、字母数字下划线都不需要） */
    function quoteIdent(name) {
      if (!name) return "";
      if (/^[A-Za-z_\u0080-\uffff][A-Za-z0-9_$\u0080-\uffff]*$/.test(name)) return name;
      return '"' + String(name).replace(/"/g, '""') + '"';
    }

    /** 把文本插到光标处，插完光标停在插入内容之后 */
    function insertText(text) {
      if (!text) return;
      ta.focus();
      const s = ta.selectionStart;
      const e = ta.selectionEnd;
      /* 优先用 execCommand("insertText")：它走浏览器的编辑命令，**原生撤销栈
         仍然有效**（直接改 ta.value 会把 Ctrl+Z 的历史清空 —— 对刚写完一屏
         SQL 的人是灾难）。不支持时退回手工拼接。 */
      let ok = false;
      try {
        ok = document.execCommand("insertText", false, text);
      } catch (err) {
        ok = false;
      }
      if (!ok) {
        ta.value = ta.value.slice(0, s) + text + ta.value.slice(e);
        ta.selectionStart = ta.selectionEnd = s + text.length;
      }
      onInput();
    }

    function buildSide() {
      sideList.textContent = "";
      if (!state.tables.length) {
        sideList.appendChild(
          cell("div", "dbsql-side-empty", "还没有读到表。库是空的，或者这个视图没有读到结构。")
        );
        return;
      }
      for (const t of state.tables) {
        const box = el("div", "dbsql-tbl", { "data-open": "false" });
        const head = el("button", "dbsql-tbl-head", { type: "button" });
        const caret = svgNode(ICON.chevron);
        caret.setAttribute("class", "dbsql-ico dbsql-tbl-caret");
        head.appendChild(caret);
        head.appendChild(cell("span", "dbsql-tbl-name", t.name));
        if (t.comment) head.appendChild(cell("span", "dbsql-tbl-comment", t.comment));
        const cols = el("div", "dbsql-tbl-cols");
        const colList = (t.columns || []).map((c) =>
          typeof c === "string" ? { name: c } : c || {}
        );
        if (!colList.length) {
          cols.appendChild(cell("div", "dbsql-side-empty", "（没读到字段）"));
        }
        for (const c of colList) {
          const btn = el("button", "dbsql-col", { type: "button" });
          btn.appendChild(cell("span", "dbsql-col-name", c.name || ""));
          if (c.type) btn.appendChild(cell("span", "dbsql-col-type", String(c.type)));
          btn.title = "插入 " + (c.name || "") + (c.type ? "（" + c.type + "）" : "");
          on(btn, "click", () => insertText(quoteIdent(c.name || "")));
          cols.appendChild(btn);
        }
        box.appendChild(head);
        box.appendChild(cols);
        head.title = "插入表名 " + t.name;
        /* 小三角负责展开/收起，名字负责插入。两个动作放在同一个点击里，
           用户就不知道刚才那一下到底干了什么 —— 所以拆开。 */
        on(head, "click", (e) => {
          const isCaret = e.target === caret || caret.contains(e.target);
          if (isCaret) {
            box.dataset.open = box.dataset.open === "true" ? "false" : "true";
            return;
          }
          insertText(quoteIdent(t.name));
          // 插入表名之后顺手把字段展开：下一步多半就是要挑字段
          box.dataset.open = "true";
        });
        sideList.appendChild(box);
      }
    }

    async function loadTables() {
      if (typeof cfg.listTables !== "function") {
        sideList.textContent = "";
        sideList.appendChild(cell("div", "dbsql-side-empty", "调用方没有提供 listTables()。"));
        return;
      }
      sideList.textContent = "";
      sideList.appendChild(cell("div", "dbsql-side-empty", "正在读取表结构…"));
      let list = null;
      try {
        list = await cfg.listTables();
      } catch (e) {
        sideList.textContent = "";
        sideList.appendChild(cell("div", "dbsql-side-empty", "读取表结构失败：" + errText(e)));
        return;
      }
      if (state.disposed) return;
      state.tables = Array.isArray(list) ? list.filter(Boolean) : [];
      // 词汇表按小写匹配（SQLite 的标识符大小写不敏感）；
      // 但给用户看的候选名保留原样，拼错提示里要显示库里的真实名字。
      state.tableNames = state.tables.map((t) => String(t.name || ""));
      state.vocab.tables = new Set(state.tableNames.map((n) => n.toLowerCase()));
      const cols = new Set();
      const colNames = [];
      for (const t of state.tables) {
        for (const c of t.columns || []) {
          const nm = typeof c === "string" ? c : c && c.name;
          if (!nm) continue;
          cols.add(String(nm).toLowerCase());
          if (colNames.indexOf(String(nm)) < 0) colNames.push(String(nm));
        }
      }
      state.vocab.cols = cols;
      state.columnNames = colNames;
      buildSide();
      render(); // 表结构到位后要重新着色（表名/列名这时才有颜色）
    }

    // ---------- 编辑区渲染 ----------
    function lineHeight() {
      const v = parseFloat(getComputedStyle(root).getPropertyValue("--dbsql-lh"));
      return v > 0 ? v : 20;
    }

    /* 行号槽按可视行数渲染：粘贴几千行也只产生几十个节点。
       因为 white-space: pre（不折行），一个逻辑行就是一个视觉行，
       位置可以纯算术算出来。

       滚动时会**每一帧**调用本函数：所以分成"重建节点"（只在可视窗口或
       高亮范围变化时做）与"移动"（每次一个 transform，合成层操作）两步。
       全量重建在高频滚动下会明显掉帧，而 transform 是免费的。 */
    let gutterKey = "";
    function paintGutter() {
      const lh = lineHeight();
      const scrollTop = ta.scrollTop;
      const first = Math.max(0, Math.floor((scrollTop - lh) / lh));
      const last = Math.min(state.lineCount - 1, first + Math.ceil(code.clientHeight / lh) + 3);
      const act = state.active >= 0 ? state.stmts[state.active] : null;
      const aFrom = act ? act.lineFrom : -1;
      const aTo = act ? act.lineTo : -1;
      const key = first + "|" + last + "|" + aFrom + "|" + aTo + "|" + state.lineCount;

      if (key !== gutterKey) {
        gutterKey = key;
        gutterInner.textContent = "";
        const frag = document.createDocumentFragment();
        for (let i = first; i <= last; i++) {
          const ln = cell("div", "dbsql-ln", String(i + 1));
          if (aFrom >= 0 && i >= aFrom && i <= aTo) ln.setAttribute("data-active", "true");
          frag.appendChild(ln);
        }
        gutterInner.appendChild(frag);
      }
      // 行号槽只跟纵向滚动：整体平移（transform），槽本身不滚
      gutterInner.style.transform = "translateY(" + (first * lh - scrollTop) + "px)";
    }

    /* 滚动：高亮层跟随偏移，行号槽跟随纵向偏移。
       两个都必须跟 —— 高亮层不跟会错位，行号槽不跟会指错行。 */
    function syncScroll() {
      pre.scrollTop = ta.scrollTop;
      pre.scrollLeft = ta.scrollLeft;
      paintGutter();
    }

    function updateStmtHint() {
      if (state.plain) {
        stmtHint.textContent = "SQL 太长，已关闭语法高亮（保证输入不卡）";
        return;
      }
      const n = state.stmts.length;
      if (!n) {
        stmtHint.textContent = "";
        return;
      }
      const st = state.stmts[state.active];
      if (!st) {
        stmtHint.textContent = "";
        return;
      }
      const range =
        st.lineFrom === st.lineTo
          ? "第 " + (st.lineFrom + 1) + " 行"
          : "第 " + (st.lineFrom + 1) + "–" + (st.lineTo + 1) + " 行";
      stmtHint.textContent = "光标在第 " + (state.active + 1) + "/" + n + " 条（" + range + "）";
    }

    function render() {
      const src = ta.value;
      const tooLong = src.length > MAX_HIGHLIGHT;
      if (state.src !== src || state.plain !== tooLong) {
        state.src = src;
        state.plain = tooLong;
        if (tooLong) {
          state.toks = [];
          state.stmts = [];
          state.lineStarts = [0];
          let n = 1;
          for (let i = 0; i < src.length; i++) if (src.charCodeAt(i) === 10) n++;
          state.lineCount = n;
        } else {
          const a = analyze(src);
          state.toks = a.toks;
          state.stmts = a.stmts;
          state.lineStarts = a.lineStarts;
          state.lineCount = a.lineCount;
        }
        root.dataset.plain = tooLong ? "true" : "false";
      }

      if (state.plain) {
        preCode.textContent = src + "\n";
      } else {
        if (state.active < 0 || state.active >= state.stmts.length) {
          state.active = statementAt(state.stmts, ta.selectionStart);
        }
        preCode.innerHTML = renderHighlight(src, state.toks, state.vocab, state.stmts[state.active]);
      }

      // 行号位数（1ch = 一个数字的宽度；等宽字体下这个换算是准的）
      root.style.setProperty("--dbsql-digits", String(Math.max(2, String(state.lineCount).length)));
      paintGutter();
      syncScroll();
      updateStmtHint();
    }

    function scheduleRender() {
      if (state.raf) return;
      state.raf = requestAnimationFrame(() => {
        state.raf = 0;
        if (!state.disposed) render();
      });
    }

    /** 有活儿要干（执行 / 取内容）之前先把待渲染的重算落定，别用上一帧的状态 */
    function flush() {
      if (state.raf) {
        cancelAnimationFrame(state.raf);
        state.raf = 0;
      }
      if (state.src !== ta.value) {
        state.active = -1;
        render();
      }
    }

    /** 光标动了：只需要重算"当前是哪条"，没必要重新做词法分析 */
    function refreshActive() {
      if (state.src !== ta.value) {
        scheduleRender();
        return;
      }
      if (state.plain) {
        updateStmtHint();
        return;
      }
      const i = statementAt(state.stmts, ta.selectionStart);
      if (i !== state.active) {
        state.active = i;
        preCode.innerHTML = renderHighlight(
          state.src,
          state.toks,
          state.vocab,
          state.stmts[state.active]
        );
        syncScroll();
        paintGutter();
      }
      updateStmtHint();
    }

    function onInput() {
      state.active = -1; // 内容变了，光标归属要重新判定
      scheduleRender();
    }

    // ---------- 结果渲染 ----------
    function colName(c) {
      return c && typeof c === "object"
        ? c.name == null
          ? ""
          : String(c.name)
        : String(c == null ? "" : c);
    }
    function colType(c) {
      return c && typeof c === "object" && c.type ? String(c.type) : "";
    }

    function displayValue(v) {
      if (v === null || v === undefined) return { text: "NULL", isNull: true };
      if (typeof v === "object") {
        try {
          return { text: JSON.stringify(v), isNull: false };
        } catch (e) {
          return { text: String(v), isNull: false };
        }
      }
      return { text: String(v), isNull: false };
    }

    function buildTable(res) {
      const cols = Array.isArray(res.columns) ? res.columns : [];
      const rows = Array.isArray(res.rows) ? res.rows : [];
      const wrap = el("div", "dbsql-res-wrap");
      const table = el("table", "dbsql-table");
      const thead = el("thead");
      const htr = el("tr");
      for (let i = 0; i < cols.length; i++) {
        const name = colName(cols[i]) || "列 " + (i + 1);
        const th = cell("th", null, name);
        const type = colType(cols[i]);
        if (type) th.title = name + "（" + type + "）";
        htr.appendChild(th);
      }
      thead.appendChild(htr);
      table.appendChild(thead);

      const tb = el("tbody");
      const frag = document.createDocumentFragment();
      const cap = Math.min(rows.length, RENDER_ROW_CAP);
      for (let r = 0; r < cap; r++) {
        const row = rows[r];
        const tr = el("tr");
        for (let c = 0; c < cols.length; c++) {
          /* rows 允许是数组行，也允许是按列名索引的对象行 —— 两种形状
             Rust 侧都可能给出来，这里都兜住。 */
          const v = Array.isArray(row)
            ? row[c]
            : row && typeof row === "object"
              ? row[colName(cols[c])]
              : undefined;
          const d = displayValue(v);
          const td = el("td");
          if (d.isNull) {
            td.appendChild(cell("span", "dbsql-null", d.text));
          } else {
            td.textContent = d.text;
            if (typeof v === "number") td.className = "is-num";
            // 超长内容由 CSS 截断，完整值放 title。太短的就不挂 title，
            // 免得几千行时白造几千个字符串。
            if (d.text.length > 40) td.title = clip(d.text, 900);
          }
          tr.appendChild(td);
        }
        frag.appendChild(tr);
      }
      tb.appendChild(frag);
      table.appendChild(tb);
      wrap.appendChild(table);
      return { wrap: wrap, shown: cap, total: rows.length };
    }

    function fmtMs(ms) {
      const v = Number(ms);
      if (!isFinite(v) || v < 0) return "";
      return v >= 1000 ? (v / 1000).toFixed(2) + " s" : Math.round(v) + " ms";
    }

    function noteNode(text, kind) {
      const n = el("div", "dbsql-result-note", kind ? { "data-kind": kind } : null);
      n.textContent = text;
      return n;
    }

    /** 一条语句的结果块 */
    function resultBlock(st, idx, total, res) {
      const box = el("div", "dbsql-result");
      const head = el("div", "dbsql-res-head");
      if (total > 1) head.appendChild(cell("span", "dbsql-res-num", "#" + (idx + 1)));
      head.appendChild(cell("span", "dbsql-res-title", clip(st.sql.replace(/\s+/g, " "), TITLE_MAX)));
      const ms = res.elapsedMs != null ? Number(res.elapsedMs) : null;
      const msText = ms == null ? "" : fmtMs(ms);
      if (msText) {
        const b = el("span", "dbsql-badge", { "data-kind": ms >= SLOW_MS ? "warn" : "" });
        b.textContent = msText;
        head.appendChild(b);
      }
      box.appendChild(head);

      const affected = typeof res.affected === "number" ? res.affected : null;
      if (Array.isArray(res.columns) && res.columns.length) {
        const t = buildTable(res);
        box.appendChild(t.wrap);
        let msg = t.total + " 行";
        if (affected != null && affected !== t.total) msg += " · 影响 " + affected + " 行";
        if (t.total > t.shown) msg += "（只画了前 " + t.shown + " 行）";
        box.appendChild(noteNode(msg, ""));
      } else if (affected != null) {
        box.appendChild(noteNode("影响 " + affected + " 行", ""));
      } else {
        box.appendChild(noteNode("执行成功，没有返回结果集", ""));
      }

      /* 两条"结果可能不完整"的提示是**必答项**，不是装饰：
         被行数上限截断、或者跑了 1 秒以上（长查询更容易被超时/上限影响），
         用户必须知道手上这份结果不是全部。 */
      if (res.truncated) {
        box.appendChild(
          noteNode(
            "结果被截断：已达到本次执行的行数上限（maxRows = " +
              maxRows +
              "），可能还有更多行没显示。",
            "warn"
          )
        );
      }
      if (ms != null && ms >= SLOW_MS) {
        box.appendChild(
          noteNode(
            "这条查询花了 " + fmtMs(ms) + "，超过 1 秒 —— 结果可能不完整，别把它当成全部数据。",
            "warn"
          )
        );
      }
      return box;
    }

    function errorBlock(raw, st, ctx) {
      const box = el("div", "dbsql-err");
      const info = explainError(raw, ctx);
      if (info.guess) box.appendChild(cell("div", "dbsql-err-guess", info.guess));
      // 原始报错一个字符都不改 —— 用户要拿它去搜
      box.appendChild(cell("pre", "dbsql-err-raw", raw));
      const tail = [];
      if (info.loc) tail.push("位置：" + info.loc);
      if (st) {
        tail.push(
          "语句起于第 " +
            (st.lineFrom + 1) +
            " 行" +
            (st.lineTo > st.lineFrom ? "，止于第 " + (st.lineTo + 1) + " 行" : "")
        );
      }
      if (tail.length) box.appendChild(cell("div", "dbsql-err-loc", tail.join(" · ")));
      return box;
    }

    // ---------- 危险操作确认 ----------
    function dangerBody(d, st) {
      const frag = document.createDocumentFragment();
      const why = el("p", "dbsql-danger-why");
      why.appendChild(document.createTextNode("这条 SQL 里含 "));
      why.appendChild(cell("b", "dbsql-danger-word", d.word));
      why.appendChild(document.createTextNode("，执行前请再确认一次。"));
      frag.appendChild(why);
      frag.appendChild(cell("p", "dbsql-danger-why", d.why));
      if (d.target) {
        const p = el("p", "dbsql-danger-why");
        p.appendChild(document.createTextNode("对象："));
        p.appendChild(cell("b", "dbsql-danger-target", d.target));
        frag.appendChild(p);
      }
      /* SQL 预览里也把命中的那个词标出来：用户要看到"是哪一句、哪个词"
         被判成了危险操作。偏移都相对 st.raw（原样子串）算，不用 trim 过的
         文本 —— trim 会把偏移整体挪掉。 */
      const sqlText = st.raw;
      const markFrom = d.at - st.lo;
      const markTo = markFrom + d.len;
      const from = Math.max(0, markFrom - 90);
      const to = Math.min(sqlText.length, from + 260);
      const pre = el("pre", "dbsql-danger-sql");
      if (from > 0) pre.appendChild(document.createTextNode("…"));
      if (markFrom >= from && markTo <= to) {
        pre.appendChild(document.createTextNode(sqlText.slice(from, markFrom)));
        pre.appendChild(cell("b", "dbsql-danger-word", sqlText.slice(markFrom, markTo)));
        pre.appendChild(document.createTextNode(sqlText.slice(markTo, to)));
      } else {
        pre.appendChild(document.createTextNode(sqlText.slice(from, to)));
      }
      if (to < sqlText.length) pre.appendChild(document.createTextNode("…"));
      frag.appendChild(pre);
      return frag;
    }

    async function confirmDanger(d, st) {
      const title = "确认要执行这条 " + d.word + " 吗？";
      if (window.DeskBaseUI && typeof window.DeskBaseUI.confirm === "function") {
        return await window.DeskBaseUI.confirm({
          title: title,
          body: dangerBody(d, st),
          confirmText: "确认执行",
          cancelText: "取消",
          danger: true,
        });
      }
      /* DeskBaseUI 不在（页面没引 components.js）时**不能跳过闸门**：
         退回系统确认框，只是没法高亮那个词。宁可难看，不能少问。 */
      console.warn("[DeskBaseSql] 没有 DeskBaseUI.confirm，退回系统确认框（无法高亮危险词）。");
      return window.confirm(
        title + "\n\n" + d.why + (d.target ? "\n对象：" + d.target : "") + "\n\n" + clip(st.sql, 300)
      );
    }

    /** 取某条语句范围内的 token（危险识别用，偏移都是全篇的绝对偏移） */
    function toksOf(st) {
      if (st.tokFrom < 0 || st.tokTo < st.tokFrom) return [];
      return state.toks.slice(st.tokFrom, st.tokTo + 1);
    }

    // ---------- 执行 ----------
    function targetsFor(which) {
      if (!state.stmts.length) return [];
      if (which === "all") return state.stmts.slice();
      const s = ta.selectionStart;
      const e = ta.selectionEnd;
      if (e > s) {
        /* 有选区：只跑与选区有交集的语句。这是编辑器里最自然的预期，
           也让"我就要跑这两条"有个直接的说法。 */
        const sel = state.stmts.filter((st) => st.hi >= s && st.lo <= e);
        if (sel.length) return sel;
      }
      const i = statementAt(state.stmts, s);
      return i >= 0 ? [state.stmts[i]] : [];
    }

    async function run(which) {
      if (state.busy) return;
      flush(); // 先把编辑区的最新内容落定，别拿上一帧的语句去执行
      if (isReadonly()) {
        // 判据在调用方（runQuery），这里只负责禁用并解释清楚
        setStatus("idle", "只读模式：执行已禁用");
        if (window.DeskBaseUI && typeof window.DeskBaseUI.toast === "function") {
          window.DeskBaseUI.toast("当前是只读模式，执行被禁用了。", { kind: "error" });
        }
        return;
      }
      if (typeof cfg.runQuery !== "function") {
        setStatus("error", "调用方没有提供 runQuery()");
        console.error("[DeskBaseSql] opts.runQuery 必须是函数 —— 它是唯一的执行入口。");
        return;
      }
      const targets = targetsFor(which);
      if (!targets.length) {
        setStatus("idle", "没有可执行的语句");
        return;
      }

      // 先把所有确认问完再开跑：一边执行一边弹确认框会让状态难以理解
      for (const st of targets) {
        const d = detectDanger(state.src, toksOf(st));
        if (!d) continue;
        setStatus("idle", "等待确认：" + d.word);
        const ok = await confirmDanger(d, st);
        if (state.disposed) return;
        if (!ok) {
          setStatus("idle", "已取消，未执行任何语句");
          return;
        }
      }

      showTab("result");
      resultList.textContent = "";
      state.totalMs = 0;
      badgeTime.hidden = true;

      let failed = false;
      for (let i = 0; i < targets.length; i++) {
        const st = targets[i];
        setBusy(
          true,
          targets.length > 1 ? "执行中 " + (i + 1) + "/" + targets.length + "…" : "执行中…"
        );
        let res = null;
        let err = null;
        try {
          res = await cfg.runQuery(st.sql, { maxRows: maxRows });
        } catch (e) {
          err = e;
        }
        if (state.disposed) return;

        if (err) {
          const raw = errText(err);
          const blk = errorBlock(raw, st, {
            tables: state.tableNames,
            columns: state.columnNames,
            src: state.src,
            stmt: st,
          });
          if (targets.length > 1) {
            const wrap = el("div");
            wrap.appendChild(cell("div", "dbsql-res-head", "#" + (i + 1) + " 出错"));
            wrap.appendChild(blk);
            resultList.appendChild(wrap);
          } else {
            resultList.appendChild(blk);
          }
          pushHistory(st.sql, false, null, null);
          /* 出错就停：批量执行里"接着往下跑"可能把前面半截的结果越改越乱，
             用户还没看到错误就先挨了后面几条。 */
          setStatus(
            "error",
            targets.length > 1
              ? "第 " + (i + 1) + " 条出错，后面的语句没有执行"
              : "执行出错"
          );
          failed = true;
          break;
        }

        const ms = res && res.elapsedMs != null ? Number(res.elapsedMs) : null;
        if (ms != null && isFinite(ms)) state.totalMs += ms;
        resultList.appendChild(resultBlock(st, i, targets.length, res || {}));
        pushHistory(st.sql, true, ms, res && Array.isArray(res.rows) ? res.rows.length : null);
      }

      if (!failed) {
        setStatus(
          "ok",
          targets.length > 1 ? "全部执行完成（" + targets.length + " 条）" : "执行完成"
        );
      }
      if (state.totalMs > 0) {
        badgeTime.textContent = fmtMs(state.totalMs);
        badgeTime.hidden = false;
        badgeTime.setAttribute("data-kind", state.totalMs >= SLOW_MS ? "warn" : "");
        badgeTime.title = "本次执行的总耗时";
      }
      setBusy(false);
      paintOut();
      renderHistory();
    }

    // ---------- 历史 ----------
    function pushHistory(sql, ok, ms, rows) {
      const text = String(sql);
      const head = state.hist[0];
      /* 连续的同样 SQL 不重复记录：直接更新第一条的时间与结果。
         （只比第一条 —— 中间隔了别的语句就不算"连续执行"。） */
      if (head && head.sql === text) {
        head.at = Date.now();
        head.ok = ok;
        head.ms = ms;
        head.rows = rows;
      } else {
        state.hist.unshift({ sql: text, at: Date.now(), ok: ok, ms: ms, rows: rows });
        if (state.hist.length > HISTORY_MAX) state.hist.length = HISTORY_MAX;
      }
      if (!histAvailable) return;
      try {
        window.localStorage.setItem(HISTORY_KEY, JSON.stringify(state.hist));
      } catch (e) {
        histAvailable = false;
        console.warn("[DeskBaseSql] 执行历史写不进 localStorage，本次会话只在内存里记：", e);
      }
    }

    function fmtTime(ts) {
      const d = new Date(ts);
      const p = (n) => (n < 10 ? "0" + n : String(n));
      return p(d.getHours()) + ":" + p(d.getMinutes()) + ":" + p(d.getSeconds());
    }

    function renderHistory() {
      tabHistory.textContent = "";
      tabHistory.appendChild(cell("span", null, "历史"));
      if (state.hist.length) {
        tabHistory.appendChild(cell("span", "dbsql-res-num", String(state.hist.length)));
      }
      if (root.dataset.tab !== "history") return;
      historyList.textContent = "";
      if (!state.hist.length) {
        historyList.appendChild(
          cell(
            "div",
            "dbsql-empty",
            "还没有执行过 SQL。执行过的语句会按时间倒序出现在这里，最多留 50 条。"
          )
        );
        return;
      }
      for (let i = 0; i < state.hist.length; i++) {
        const h = state.hist[i];
        const item = el("button", "dbsql-hitem", {
          type: "button",
          "data-failed": h.ok ? "false" : "true",
        });
        item.style.setProperty("--i", String(Math.min(i, 12))); // 入场错峰只给前十几条
        item.appendChild(
          cell("span", "dbsql-hitem-sql", clip(h.sql.replace(/\s+/g, " "), TITLE_MAX))
        );
        const meta = el("span", "dbsql-hitem-meta");
        meta.appendChild(cell("span", "dbsql-hitem-time", fmtTime(h.at)));
        if (!h.ok) meta.appendChild(cell("span", "dbsql-hitem-bad", "失败"));
        if (h.rows != null) meta.appendChild(cell("span", null, h.rows + " 行"));
        if (h.ms != null) {
          const slow = Number(h.ms) >= SLOW_MS;
          meta.appendChild(
            cell("span", slow ? "dbsql-hitem-slow" : null, fmtMs(h.ms) + (slow ? " · 慢查询" : ""))
          );
        }
        item.appendChild(meta);
        item.title = "点一下填回编辑区：" + clip(h.sql, 300);
        on(item, "click", () => {
          setValue(h.sql);
          showTab("result");
          setStatus("idle", "已把历史里的一条 SQL 填回编辑区");
        });
        historyList.appendChild(item);
      }
    }

    // ---------- 分隔条拖动 ----------
    /* 高度是**直接改写**的，全程没有 transition —— 布局属性不能过渡，
       否则拖动过程中每一帧都在重排（docs/18 原则 7.1#2）。 */
    let dragging = false;
    on(split, "pointerdown", (e) => {
      dragging = true;
      split.dataset.drag = "true";
      try {
        split.setPointerCapture(e.pointerId);
      } catch (err) {
        /* 老内核或合成事件拿不到指针 id：此时仍按普通拖动处理 */
      }
      e.preventDefault();
    });
    on(split, "pointermove", (e) => {
      if (!dragging) return;
      const top = editorBox.getBoundingClientRect().top;
      const maxH = Math.max(140, pane.clientHeight - 160);
      const h = clamp(e.clientY - top, 96, maxH);
      root.style.setProperty("--dbsql-editor-h", Math.round(h) + "px");
      paintGutter();
    });
    const endDrag = () => {
      if (!dragging) return;
      dragging = false;
      split.dataset.drag = "false";
    };
    on(split, "pointerup", endDrag);
    on(split, "pointercancel", endDrag);

    // ---------- 事件接线 ----------
    on(btnRun, "click", () => run("one"));
    on(btnRunAll, "click", () => run("all"));
    on(btnSide, "click", () => {
      const off = root.dataset.side === "off";
      root.dataset.side = off ? "on" : "off";
      btnSide.setAttribute("aria-pressed", off ? "true" : "false");
    });
    on(tabResult, "click", () => showTab("result"));
    on(tabHistory, "click", () => showTab("history"));

    on(ta, "input", onInput);
    on(ta, "scroll", syncScroll, { passive: true });
    on(ta, "keydown", (e) => {
      if (e.key !== "Enter" || !(e.ctrlKey || e.metaKey)) return;
      // 中文输入法组字过程中的回车不是"执行"
      if (e.isComposing) return;
      /* Ctrl+Enter 执行光标所在的那条；加 Shift 执行全部。
         （docs/06 §3.3 要求"只执行光标所在语句"；这里是默认行为。） */
      e.preventDefault();
      run(e.shiftKey ? "all" : "one");
    });
    on(ta, "keyup", refreshActive);
    on(ta, "mouseup", refreshActive);
    on(ta, "select", refreshActive);
    on(ta, "focus", () => {
      syncReadonly();
      refreshActive();
    });
    on(ta, "paste", () => scheduleRender());
    on(ta, "cut", () => scheduleRender());
    /* selectionchange 挂在 document 上：按住方向键连续移动光标时 keyup 不一定来，
       这是唯一可靠的通知源。只关心自己这个 textarea。 */
    on(document, "selectionchange", () => {
      if (document.activeElement === ta) refreshActive();
    });

    // 尺寸变化：行号槽按可视行数虚拟渲染，可视高度变了要重画
    if (typeof ResizeObserver === "function") {
      const ro = new ResizeObserver(() => {
        if (!state.disposed) paintGutter();
      });
      ro.observe(code);
      listeners.signal.addEventListener("abort", () => ro.disconnect());
    }

    // ---------- 实例 ----------
    function setValue(text) {
      ta.value = text == null ? "" : String(text);
      state.active = -1;
      render();
      // 光标放末尾：填回来的 SQL 通常是要接着改或直接执行的
      const n = ta.value.length;
      ta.selectionStart = ta.selectionEnd = n;
      refreshActive();
    }

    syncReadonly();
    renderHistory();
    loadTables();

    return {
      getValue() {
        return ta.value;
      },
      setValue: setValue,
      focus() {
        ta.focus();
      },
      history() {
        // 返回副本：调用方不该能改到内部状态
        return state.hist.map((h) => ({
          sql: h.sql,
          at: h.at,
          ok: h.ok,
          ms: h.ms,
          rows: h.rows,
        }));
      },
      destroy() {
        state.disposed = true;
        if (state.raf) cancelAnimationFrame(state.raf);
        listeners.abort();
        root.remove();
      },
    };
  }

  function deadInstance() {
    return {
      getValue: () => "",
      setValue() {},
      focus() {},
      history: () => [],
      destroy() {},
    };
  }

  // ============================================================
  // 导出
  // ============================================================
  window.DeskBaseSql = { mount: mount };
})();
