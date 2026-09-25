/* ============================================================
   Markdown 渲染器（极简、零依赖）
   ============================================================
   为什么自己写而不引 marked / markdown-it：
     ① 项目坚持零前端依赖（引一个就是几十 KB，还得为它做体积与供应链评估）；
     ② 我们只要"够写文档"的那部分语法，不需要完整 CommonMark；
     ③ 自写的最大好处是**转义顺序由我们自己定** —— 见下面"安全"那节。

   支持的语法（够用了）：
     # ## ### 标题 · **粗体** · *斜体* · `行内代码` · ``` 代码块 ```
     - 无序列表 · 1. 有序列表 · > 引用 · [文字](链接) · 空行分段

   ⚠️ **安全：先转义，再套 Markdown 规则。**
   顺序反了就会变成 XSS —— 用户写一段 `<img onerror=...>` 就执行了。
   这里先把 `< & "` 全部转成实体，之后产生的一切标签都是我们自己加的，
   所以不可能夹带用户写的可执行内容。
   ============================================================ */
(function () {
  "use strict";

  function escapeHtml(s) {
    return String(s)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#39;");
  }

  /** 行内语法。**入参必须是已经转义过的文本**。 */
  function inline(escaped) {
    let s = escaped;
    // 行内代码优先（里面的 * _ 不该被当成强调）
    s = s.replace(/`([^`]+)`/g, function (_m, code) {
      return "<code>" + code + "</code>";
    });
    // 链接：只接受 http/https，其它一律不转成 a —— 不给人 javascript: 的机会
    s = s.replace(/\[([^\]]+)\]\((https?:\/\/[^)\s]+)\)/g, function (_m, text, url) {
      return '<a href="' + url + '" rel="noopener noreferrer" target="_blank">' + text + "</a>";
    });
    s = s.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
    s = s.replace(/(^|[^*])\*([^*\n]+)\*/g, "$1<em>$2</em>");
    return s;
  }

  function render(md) {
    const lines = String(md == null ? "" : md).split(/\r?\n/);
    const out = [];
    let inCode = false;
    let inList = false;
    let listTag = "";
    let inQuote = false;
    let para = [];

    const flushPara = function () {
      if (para.length) {
        out.push("<p>" + inline(escapeHtml(para.join("\n"))) + "</p>");
        para = [];
      }
    };
    const closeList = function () {
      if (inList) {
        out.push("</" + listTag + ">");
        inList = false;
        listTag = "";
      }
    };
    const closeQuote = function () {
      if (inQuote) {
        out.push("</blockquote>");
        inQuote = false;
      }
    };
    const closeAll = function () {
      flushPara();
      closeList();
      closeQuote();
    };

    for (let i = 0; i < lines.length; i++) {
      const raw = lines[i];
      const t = raw.trim();

      // 代码块
      if (/^```/.test(t)) {
        closeAll();
        if (inCode) {
          out.push("</code></pre>");
          inCode = false;
        } else {
          out.push("<pre><code>");
          inCode = true;
        }
        continue;
      }
      if (inCode) {
        out.push(escapeHtml(raw) + "\n");
        continue;
      }

      // 空行分段
      if (t === "") {
        closeAll();
        continue;
      }

      // 标题
      const h = /^(#{1,6})\s+(.*)$/.exec(t);
      if (h) {
        closeAll();
        const lv = Math.min(h[1].length, 6);
        out.push("<h" + lv + ">" + inline(escapeHtml(h[2])) + "</h" + lv + ">");
        continue;
      }

      // 引用
      if (/^>\s?/.test(t)) {
        flushPara();
        closeList();
        if (!inQuote) {
          out.push("<blockquote>");
          inQuote = true;
        }
        out.push("<p>" + inline(escapeHtml(t.replace(/^>\s?/, ""))) + "</p>");
        continue;
      }
      closeQuote();

      // 列表
      const ul = /^[-*+]\s+(.*)$/.exec(t);
      const ol = /^\d+[.)]\s+(.*)$/.exec(t);
      if (ul || ol) {
        flushPara();
        const want = ul ? "ul" : "ol";
        if (!inList || listTag !== want) {
          closeList();
          out.push("<" + want + ">");
          inList = true;
          listTag = want;
        }
        const item = ul ? ul[1] : ol[1];
        out.push("<li>" + inline(escapeHtml(item)) + "</li>");
        continue;
      }
      closeList();

      // 普通段落
      para.push(raw);
    }
    closeAll();
    if (inCode) {
      out.push("</code></pre>");
    }
    return out.join("\n");
  }

  window.DeskBaseMarkdown = { render: render, escapeHtml: escapeHtml };
})();
