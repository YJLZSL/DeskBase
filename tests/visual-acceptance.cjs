/* ============================================================
   人工交互验收 · 视觉走查（截图给"人"看）
   ============================================================
   ## 这份脚本解决的问题

   交接文档里长期挂着一条："**人工交互验收仍是最大的窟窿** ——
   烟测通过 ≠ 人看着好用，覆盖不到观感、文案、窄屏可用性，
   以及'这个功能到底该不该这么做'。"

   它没做的原因是**没人看**：截图躺在临时目录里，谁也不会去翻。
   这份脚本把"看"变成一件**有清单、有产出、能复核**的事：

     1. 用**真实数据**造景（`visual-seed.page.js`：两张表 + 三篇笔记，
        含长表名/长备注/金额/空值 —— 空状态下什么毛病都看不出来）；
     2. 把**每一个用户能到达的状态**都截一张（含面板打开、对话框、
        空状态、有数据、滚动中……）；
     3. 在**四个有代表性的主题**下重复一遍（浅色 / 深色 / 高对比 / 极简，
        因为主题改的是**结构**不只是色相，一套主题下的排版问题
        在另一套里可能才显形）；
     4. 把每张截图**编号 + 标注"该看什么"**，生成一份 Markdown 清单，
        交给会看图的人（或 AI）逐张给结论。

   ## 为什么"看一眼"比再加 100 条断言有用

   v1.9.1 的教训写在交接文档里：**界面上写着 `连表里的**内容**一起搜`**
   （Markdown 记号原样露给用户）—— **光看代码是发现不了的，代码里那行完全正常**。
   那次就是靠看截图一次性抓到 7 处。机器断言只验"元素在不在、值对不对"，
   验不了"这句话读起来顺不顺、这个按钮是不是放错了地方"。

   ## 用法
     node tests/visual-acceptance.cjs [exe] [--keep] [--out <目录>]
   产出：
     <out>/shots/NN-<state>-<theme>.png    截图
     <out>/REVIEW.md                       逐张清单（给看的人勾）
   ============================================================ */
const fs = require("fs");
const os = require("os");
const path = require("path");
const { spawn, spawnSync } = require("child_process");

const ROOT = path.resolve(__dirname, "..");
const argv = process.argv.slice(2);
const argExe = argv.find((a) => !a.startsWith("--"));
const EXE = argExe
  ? path.resolve(argExe)
  : path.join(ROOT, "app", "target", "release", "deskbase.exe");
const KEEP = argv.includes("--keep");
const outIdx = argv.indexOf("--out");
const stamp = new Date().toISOString().replace(/[:T]/g, "-").slice(0, 16);
const OUT =
  outIdx >= 0 && argv[outIdx + 1]
    ? path.resolve(argv[outIdx + 1])
    : path.join(ROOT, "local-docs", "runs", "visual-" + stamp);

const SEED_PAGE = path.join(__dirname, "visual-seed.page.js");
/**
 * CDP 端口。**必须与 `app/src/main.rs` 的 `SMOKE_DEBUG_PORT` 一致** ——
 * 那里是写死的 9222（只有烟测模式才开这个端口）。
 *
 * ⚠️ 因此这份脚本**不能与 `ui-walkthrough.cjs` / `ui-smoke.cjs` 同时跑**：
 * 它们抢同一个端口，而且都会 `taskkill deskbase.exe`（会把对方的应用一起杀掉）。
 * 串行跑就行 —— 这也是所有界面测试的既有约定。
 */
const PORT = 9222;

if (!fs.existsSync(EXE)) {
  console.error(`✘ 找不到产物：${EXE}\n  先跑 node scripts/build.cjs 再试。`);
  process.exit(2);
}

const shotsDir = path.join(OUT, "shots");
fs.mkdirSync(shotsDir, { recursive: true });
const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "deskbase-visual-"));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** 要截的状态。`look` 是**给看的人看的**："这张该盯什么"。 */
const STATES = [
  { id: "wb-data", page: "workbench", look: "有数据时的首页：搜索框宽度、状态数字是否对齐、下方大片留白是否合理" },
  { id: "notes-list", page: "notes", prep: "filterBlank", look: "笔记列表 + 筛选切到「空笔记」：空态文案（“没有符合条件的笔记”）、筛选胶囊的选中态" },
  { id: "notes-edit", page: "notes", prep: "openNote", look: "笔记正文编辑区：字号/行高/中文长句换行、富文本工具栏是否拥挤" },
  { id: "db-list", page: "database", look: "**重点**：侧栏三个分组标题与按钮、长表名在 236–300px 里怎么断行、主区域表格" },
  { id: "db-grid", page: "database", prep: "openTable", look: "**重点**：网格——金额右对齐、长备注截断、表头、横向滚动条、单元格留白" },
  { id: "db-grid-wide", page: "database", prep: "openBigTable", look: "260 行表：滚动条出现后表头是否还粘住、行高是否一致" },
  { id: "set-ai", page: "settings", prep: "scrollAi", look: "**重点**：AI 卡片——服务商下拉与图标同行、接口地址长 URL 是否溢出、保存按钮位置" },
  { id: "set-install", page: "settings", prep: "scrollInstall", look: "安装卡片：说明文字层次、勾选项、按钮组" },
  { id: "set-help", page: "settings", prep: "scrollHelp", look: "**重点**：教程卡片——标题层级、表格、代码块、长段落可读性" },
  { id: "dlg-newtable", page: "database", prep: "openNewTable", look: "新建表格对话框：默认三行、列名预填、按钮组" },
  { id: "dlg-import", page: "database", prep: "openImport", look: "导入向导：标题/说明/按钮层次" },
  { id: "palette", page: "database", prep: "openPalette", look: "**重点**：命令面板——分组标题、条目标题、快捷键右对齐、层级与遮罩" },
  { id: "palette-ai", page: "database", prep: "openPaletteAi", look: "命令面板搜「AI」：只有一条时列表是否显得空" },
  { id: "aichat-off", page: "database", prep: "openAiChat", look: "**重点**：AI 对话面板——隐私条三态文字、禁用态说明、关闭按钮位置" },
  { id: "empty-db", page: "database", prep: "emptyState", look: "空状态：引导是否**垂直居中**（这是 v1.11.0 刚修的）" },
];

/** 四个有代表性的主题。顺序=浅色→深色→高对比→极简。 */
const THEMES = [
  { id: "xuan", label: "宣纸（默认浅色）" },
  { id: "yemo", label: "夜墨（深色）" },
  { id: "hc-dark", label: "高对比暗色（无障碍）" },
  { id: "brutal", label: "极简（无圆角无阴影）" },
];

/** 每个状态截完之后**必须**验的东西。
 *
 * 为什么必须有这一层（而不是截完就算）：第一轮 57 张里，
 *   · `notes-edit` 在四套主题下**从没拍到编辑画面**（笔记没点开，截的还是列表）；
 *   · `empty-db` 拍的是"已加载 200 行"的网格 —— **空状态压根没覆盖**；
 *   · `set-ai` / `set-install` 有几张停在了**别的卡片**上，而清单给它们列的
 *     "该盯什么"根本答不上。
 * 结果是一份**看起来验过、实际没验**的清单 —— 比没有清单更坏。
 * 所以：截图之后立刻断言"这个状态该出现的东西出现了"，不过就记进 `blind`，
 * 并在 REVIEW.md 里单列一节。**盲区要被看见，不能被平均掉。**
 */
const EXPECT = {
  "wb-data": `!!document.querySelector('#wb-stats') && document.querySelector('#wb-stats').children.length > 0`,
  "db-list": `document.querySelectorAll('#db-table-list .db-table-item').length > 0`,
  "db-grid": `document.querySelectorAll('#db-pane-grid .dbgrid-row').length > 0`,
  "db-grid-wide": `document.querySelectorAll('#db-pane-grid .dbgrid-row').length > 0`,
  "notes-edit": `(() => {
    const ed = document.querySelector('#note-editor, #note-body, .note-edit, textarea.note-body');
    return !!ed && ed.offsetHeight > 0;
  })()`,
  "palette": `!!document.querySelector('.dp-root[data-open="true"]')`,
  "palette-ai": `!!document.querySelector('.dp-root[data-open="true"]')`,
  "aichat-off": `(() => { const e = document.getElementById('ai-chat'); return !!e && e.dataset.open === 'true'; })()`,
  "dlg-newtable": `!!document.querySelector('#db-dialog-new[open]')`,
  "dlg-import": `!!document.querySelector('#db-dialog-import[open]')`,
  "set-ai": `(() => {
    const c = document.getElementById('card-ai'); if (!c) return false;
    const r = c.getBoundingClientRect();
    return r.top < window.innerHeight * 0.7 && r.bottom > 0;
  })()`,
  "set-install": `(() => {
    const c = document.getElementById('card-install'); if (!c) return false;
    const r = c.getBoundingClientRect();
    return r.top < window.innerHeight * 0.8 && r.bottom > 0;
  })()`,
  "set-help": `(() => {
    const c = document.getElementById('db-help-card'); if (!c) return false;
    const r = c.getBoundingClientRect();
    return r.top < window.innerHeight * 0.8 && r.bottom > 0;
  })()`,
  "empty-db": `document.querySelectorAll('#db-table-list .db-table-item').length === 0`,
};

async function connectCdp(ms) {
  const t0 = Date.now();
  for (;;) {
    try {
      const r = await fetch(`http://127.0.0.1:${PORT}/json/list`);
      const list = await r.json();
      const page = list.find((t) => t.type === "page" && t.webSocketDebuggerUrl);
      if (page) {
        const ws = new WebSocket(page.webSocketDebuggerUrl);
        await new Promise((res, rej) => {
          ws.addEventListener("open", res, { once: true });
          ws.addEventListener("error", rej, { once: true });
        });
        let id = 0;
        const pend = new Map();
        ws.addEventListener("message", (ev) => {
          let m;
          try { m = JSON.parse(ev.data); } catch (_) { return; }
          if (m.id && pend.has(m.id)) {
            const { res, rej } = pend.get(m.id);
            pend.delete(m.id);
            m.error ? rej(new Error(JSON.stringify(m.error))) : res(m.result);
          }
        });
        const send = (method, params) =>
          new Promise((res, rej) => {
            const myId = ++id;
            pend.set(myId, { res, rej });
            ws.send(JSON.stringify({ id: myId, method, params: params || {} }));
          });
        const evalJs = async (expr) => {
          const r = await send("Runtime.evaluate", {
            expression: expr,
            awaitPromise: true,
            returnByValue: true,
          });
          if (r.exceptionDetails) throw new Error(r.exceptionDetails.text || "页面脚本异常");
          return r.result && r.result.value;
        };
        return { ws, send, evalJs, url: page.url };
      }
    } catch (_) {}
    if (Date.now() - t0 > ms) return null;
    await sleep(300);
  }
}

(async () => {
  spawnSync("taskkill", ["/IM", "deskbase.exe", "/F"], { windowsHide: true });
  await sleep(700);

  let app = null;
  const captured = [];
  try {
    const env = {
      ...process.env,
      DESKBASE_DATA_DIR: dataDir,
      DESKBASE_UI_SMOKE: SEED_PAGE,
    };
    delete env.DESKBASE_RENDER;
    const errFd = fs.openSync(path.join(dataDir, "stderr.log"), "a");
    app = spawn(EXE, [], { env, stdio: ["ignore", "ignore", errFd], windowsHide: false });

    const conn = await connectCdp(60000);
    if (!conn) throw new Error(`60 秒内连不上 CDP（${PORT}）—— 应用没起来或端口没开`);
    const { send, evalJs } = conn;
    await send("Page.enable");
    await send("Runtime.enable");
    console.log(`已连上：${conn.url}`);

    // 等造景完成
    let seeded = false;
    for (let i = 0; i < 100; i++) {
      seeded = await evalJs("!!window.__visualSeeded");
      if (seeded) break;
      const err = await evalJs("window.__visualSeedError || ''");
      if (err) throw new Error("造景失败：" + err);
      await sleep(300);
    }
    if (!seeded) console.log("⚠ 造景超时，继续（截图可能偏空）");
    else console.log("✔ 造景完成");

    // ⚠️ **造完景必须让界面重新拉一次数据。**
    //
    // 这是第一轮 `notes-edit` 一直拍到"还没有笔记"的根因，而且它瞒过了所有人：
    // 造景脚本和 app.js 的 `boot()` 是**并行**跑的（都挂在 DOMContentLoaded 上），
    // 谁先谁后不确定。当 boot 先跑完时，它已经用"空库"渲染过一次笔记列表，
    // **之后就没有任何东西会再拉一次** —— 笔记在库里（`note.list` 明明返回 3 条），
    // 界面上却写着"还没有笔记"。
    //
    // 「IPC 有数据 ≠ 界面显示了数据」 —— 这正是这个项目最忌讳的那种不一致。
    // 修法：造完景切一次页，逼界面重走一遍它自己的加载路径（不特判笔记页，
    // 因为表格页的侧栏列表同理）。
    if (seeded) {
      for (const v of ["notes", "database", "workbench"]) {
        await evalJs(`(() => {
          const b = document.querySelector('.nav-item[data-target="${v}"]');
          if (b) b.click();
          return !!b;
        })()`);
        await sleep(600);
      }
      // 回到笔记页并确认列表真的渲染出来了 —— 不确认的话下一轮还会静默拍到空列表
      await evalJs(`(() => {
        const b = document.querySelector('.nav-item[data-target="notes"]');
        if (b) b.click();
        return !!b;
      })()`);
      await sleep(900);
      const noteCount = await evalJs(`document.querySelectorAll('#note-list .note-item').length`);
      const tableCount = await evalJs(`(() => {
        const b = document.querySelector('.nav-item[data-target="database"]');
        if (b) b.click();
        return true;
      })()`);
      await sleep(900);
      const tbl = await evalJs(`document.querySelectorAll('#db-table-list .db-table-item').length`);
      await evalJs(`(() => {
        const b = document.querySelector('.nav-item[data-target="notes"]');
        if (b) b.click();
        return true;
      })()`);
      await sleep(700);
      console.log(`界面刷新后：笔记条目 ${noteCount} 条 · 表格条目 ${tbl} 条${tableCount ? "" : ""}`);
      if (!noteCount || !tbl) {
        console.log("⚠ 界面上的条目数是 0 —— 截图会拍到空态，清单里会标成盲区");
      }
    }

    /** 切主题。用 dataset.theme 直接切，与界面选主题同一条路径（theme.css 按它选 token）。 */
    async function setTheme(id) {
      await evalJs(`(() => {
        document.documentElement.dataset.theme = ${JSON.stringify(id)};
        // 关掉切换过渡：截图要的是"稳定态"，过渡中间帧没有参考价值
        document.documentElement.classList.add("theme-switching");
        return true;
      })()`);
      await sleep(320);
    }

    async function navTo(view) {
      await evalJs(`(() => {
        const b = document.querySelector('.nav-item[data-target="${view}"]');
        if (b) b.click();
        return !!b;
      })()`);
      await sleep(420);
    }

    /** 把某张卡片滚到视口里。
     *
     * 用 `scrollIntoView` 而不是自己算 `host.scrollTop`：
     * 我算错过两次 —— 先当成 `.view` 滚动（其实 `.view` 不滚动，`overflow` 在 `.content` 上），
     * 改成 `#content` 之后仍有两张卡片（安装 / 教程）断言不过。
     * `scrollIntoView` 会**自己找真正的滚动祖先**，不需要我猜层级。
     * 这次的教训与项目里那条一致：**"元素在哪、谁在滚"这种事，能让浏览器答就别自己算**。 */
    async function scrollCardIntoView(id) {
      await evalJs(`(() => {
        const c = document.getElementById(${JSON.stringify(id)});
        if (!c || typeof c.scrollIntoView !== 'function') return false;
        c.scrollIntoView({ block: 'start', behavior: 'instant' });
        return true;
      })()`);
      await sleep(450);
    }

    /** 每个状态自己的"摆姿势"。失败不致命 —— 记下来，截图仍然出。 */
    const PREPS = {
      async openNote() {
        // ⚠️ 要点**进编辑区**，不能只依赖"笔记页默认显示列表"。
        // 第一轮这里没点开任何笔记，于是 `notes-edit` 与 `notes-list` **逐字节相同** ——
        // 四套主题下编辑画面**一次都没拍到**，而清单上那两行看起来都验过了。
        //
        // 所以：等列表渲染出来 → 点第一条（点第二次以确保切换，避免"已选中所以无变化"）
        // → 等编辑器真的有内容。失败就由 `EXPECT["notes-edit"]` 把这一张标成盲区。
        for (let i = 0; i < 40; i++) {
          const n = await evalJs(`document.querySelectorAll('#note-list .note-item').length`);
          if (n > 0) break;
          await sleep(150);
        }
        await evalJs(`(() => {
          const items = [...document.querySelectorAll('#note-list .note-item')];
          if (!items.length) return false;
          items[0].click();
          return true;
        })()`);
        await sleep(700);
        // 编辑器在窄屏是抽屉式的，宽屏并排；两种情况都要让输入区可见
        await evalJs(`(() => {
          const b = document.getElementById('note-body');
          if (b && typeof b.scrollIntoView === 'function') b.scrollIntoView({ block: 'nearest' });
          return true;
        })()`);
        await sleep(300);
      },
      async openTable() {
        await evalJs(`(() => {
          const it = [...document.querySelectorAll('#db-table-list .db-table-item')]
            .find(x => /供应商台账/.test(x.textContent || ''));
          if (it) it.click();
          return !!it;
        })()`);
        await sleep(900);
      },
      async openBigTable() {
        // 与前一张（小表）拉开区别：点**长名字的大表**，并且**滚到中间**。
        // 第一轮两张都停在网格顶部，于是 `db-list` 与 `db-grid-wide` 逐字节相同 ——
        // 等于"大表滚动"这个状态没验到。
        //
        // ⚠️ 滚动容器是 **`.dbgrid-scroll`**（`grid.js:375`），不是 `#db-pane-grid`。
        // 我第一版滚的是 pane，它是空操作 —— 而"空操作"在截图上看不出来
        // （只是两张图一样），必须**断言 scrollTop 真的变了**才能发现。
        await evalJs(`(() => {
          const it = [...document.querySelectorAll('#db-table-list .db-table-item')]
            .find(x => /华东区/.test(x.textContent || ''));
          if (it) it.click();
          return !!it;
        })()`);
        await sleep(1200);
        const scrolled = await evalJs(`(() => {
          const sc = document.querySelector('#db-pane-grid .dbgrid-scroll');
          if (!sc) return -1;
          sc.scrollTop = Math.floor(sc.scrollHeight * 0.45);
          return sc.scrollTop;
        })()`);
        await sleep(520);
        return scrolled; // 供调用方记进盲区判断
      },
      async filterBlank() {
        // 切到「空笔记」筛选。**刻意选一个与"笔记列表"不同的状态** ——
        // `notes-edit` 会点开第一条笔记、而笔记页默认显示的就是那条，
        // 于是"列表页"与"编辑页"逐字节相同（第一轮的三对重复就是这么来的）。
        // 筛选一换，两张图就真的不同了，而且顺带覆盖了筛选胶囊与空态文案。
        await evalJs(`(() => {
          const fs = [...document.querySelectorAll('#filters .filter')];
          const b = fs.find(x => /空笔记/.test(x.textContent || '')) || fs[fs.length - 1];
          if (b) b.click();
          return !!b;
        })()`);
        await sleep(520);
      },
      async scrollTop() {
        // 设置页的状态之间会互相影响：上一个状态可能把页面滚到了别处。
        // 每张设置页截图**先回到顶部**，再由各自的 prep 滚到目标卡片 ——
        // 否则 `set-ai` 会拍到别的卡片（第一轮就是这么发生的）。
        //
        // ⚠️ 滚动容器是 **`#content`**，不是那个 `.view`！
        // 第一次我写的是 `.view[data-view="settings"]`，它**根本不滚动**
        // （`.view` 是 block，`overflow` 在 `.content` 上）—— 于是这一句是空操作，
        // 而"回到顶部"没生效，`set-ai`/`set-help` 继续拍到别的卡片。
        // 是**截图自检把它报出来**才发现的（7 个盲区），看代码看不出来。
        await evalJs(`(() => {
          const c = document.getElementById('content');
          if (c) c.scrollTop = 0;
          return true;
        })()`);
        await sleep(320);
      },
      async scrollInstall() {
        await scrollCardIntoView("card-install");
      },
      async scrollHelp() {
        await scrollCardIntoView("db-help-card");
      },
      async scrollAi() {
        // set-ai 这一张必须停回 AI 卡片 —— 否则它和上一张（教程/安装）长一样
        await scrollCardIntoView("card-ai");
      },
      async openNewTable() {
        await evalJs(`(() => {
          const b = document.getElementById('btn-db-new-table');
          if (b) b.click();
          return !!b;
        })()`);
        await sleep(650);
      },
      async openImport() {
        await evalJs(`(() => {
          const a = window.DeskBaseDb;
          if (a && typeof a.openImportDialog === 'function') { a.openImportDialog({ autoPick: false }); return true; }
          return false;
        })()`);
        await sleep(700);
      },
      async openPalette() {
        await evalJs(`window.DeskBasePalette && window.DeskBasePalette.open("")`);
        await sleep(520);
      },
      async openPaletteAi() {
        await evalJs(`window.DeskBasePalette && window.DeskBasePalette.open("AI")`);
        await sleep(620);
        await evalJs(`window.DeskBasePalette && window.DeskBasePalette.close()`);
        await sleep(100);
        await evalJs(`window.DeskBasePalette && window.DeskBasePalette.open("AI")`);
        await sleep(520);
      },
      async openAiChat() {
        await evalJs(`window.AiChat && window.AiChat.open()`);
        await sleep(800);
      },
      async emptyState() {},
    };

    /** 关掉上一个状态留下的浮层，避免"上一张的面板还在，这张看不清" */
    async function cleanup() {
      await evalJs(`(() => {
        try { window.DeskBasePalette && window.DeskBasePalette.close(); } catch (_) {}
        try { window.AiChat && window.AiChat.close(); } catch (_) {}
        document.querySelectorAll('dialog[open]').forEach(d => { try { d.close(); } catch (_) {} });
        const sc = document.querySelector('.dbui-scrim'); if (sc) sc.remove();
        return true;
      })()`);
      await sleep(260);
    }

    let n = 0;
    const failures = [];
    /** 截图之后断言没过的状态 —— 这些是**盲区**，必须单列，不能被平均掉。 */
    const blind = [];
    for (const theme of THEMES) {
      await setTheme(theme.id);
      for (const st of STATES) {
        await cleanup();
        // 「空状态」这一张要在删掉表之后拍，放最后单独处理（见下）
        if (st.prep === "emptyState") continue;
        await navTo(st.page);
        // 设置页的状态之间会互相影响：上一个状态可能把页面滚到了别处。
        // **每张设置页截图先回到顶部**，再由 prep 滚到它自己的目标卡片 ——
        // 否则 `set-ai` 会拍到别的卡片（第一轮就是这么发生的：四套主题里三张拍到教程卡片）。
        if (st.page === "settings") {
          try { await PREPS.scrollTop(); } catch (_) {}
        }
        if (st.prep && PREPS[st.prep]) {
          try {
            const r = await PREPS[st.prep]();
            // prep 返回数字 = 它自报了一个"该 >0 的量"（目前只有大表滚动用）。
            // **空操作必须能被发现** —— 第一版滚错了元素，不报错、只是两张图一样。
            if (typeof r === "number" && !(r > 0)) {
              blind.push(`${st.id}/${theme.id}：prep 自报的关键量没有生效（返回值 ${r}）`);
            }
          } catch (e) { failures.push(`${st.id}/${theme.id}: ${e.message}`); }
        }
        n += 1;
        const tag = String(n).padStart(2, "0") + "-" + st.id + "-" + theme.id;
        try {
          // ⚠️ **先断言，再截图** —— 顺序不能反。
          // 第一轮是截完就走，于是"没点开笔记"这种情况只表现为
          // "两张图长得一样"，而没人会去比对 57 张的哈希。
          let ok = true;
          let why = "";
          const exp = EXPECT[st.id];
          if (exp) {
            try {
              ok = !!(await evalJs(exp));
              if (!ok) why = "该状态的预期元素没出现";
            } catch (e) {
              ok = false;
              why = "断言执行失败：" + e.message;
            }
          }
          if (!ok) blind.push(`${tag}（${st.id}）：${why}`);
          const png = await send("Page.captureScreenshot", { format: "png" });
          fs.writeFileSync(path.join(shotsDir, tag + ".png"), Buffer.from(png.data, "base64"));
          captured.push({
            tag,
            state: st.id,
            theme: theme.id,
            themeLabel: theme.label,
            look: st.look,
            ok,
          });
          process.stdout.write(`  ${tag}${ok ? "" : "  ⚠ 盲区"}\n`);
        } catch (e) {
          failures.push(`${tag}: 截图失败 ${e.message}`);
        }
      }
    }

    // ---- 最后单独来一张空状态：把两张表都删掉 ----
    await cleanup();
    await navTo("database");
    await evalJs(`(async () => {
      const list = await window.__deskbase.call('schema.listTables');
      for (const t of (list || [])) {
        try { await window.__deskbase.call('schema.dropTable', { name: t.name, confirmName: t.name }); } catch (_) {}
      }
      return true;
    })()`);
    await sleep(700);
    await evalJs(`(() => {
      const b = document.getElementById('btn-db-refresh'); if (b) b.click(); return true;
    })()`);
    await sleep(900);
    await setTheme("xuan");
    n += 1;
    const emptyTag = String(n).padStart(2, "0") + "-empty-db-xuan";
    // ⚠️ 这一张**必须断言真的空了**。第一轮它拍的是"已加载 200 行"的网格 ——
    // 也就是说 v1.11.0 刚修好的"空状态居中"**从来没被验证过**，
    // 而清单上那张图看起来像是验过了。
    let emptyOk = false;
    try {
      emptyOk = !!(await evalJs(EXPECT["empty-db"]));
    } catch (_) {}
    if (!emptyOk) blind.push(`${emptyTag}（empty-db）：删表之后侧栏仍有表项 —— 空状态没真正进入`);
    const png = await send("Page.captureScreenshot", { format: "png" });
    fs.writeFileSync(path.join(shotsDir, emptyTag + ".png"), Buffer.from(png.data, "base64"));
    captured.push({
      tag: emptyTag,
      state: "empty-db",
      theme: "xuan",
      themeLabel: "宣纸（默认浅色）",
      look: "空状态：引导是否**垂直居中**（v1.11.0 刚修的）",
      ok: emptyOk,
    });

    // ---- 产出：给"看的人"的清单 ----
    //
    // 写清单之前先做一遍**截图自检**：逐字节相同的截图 = 至少有一张没拍到位。
    // （状态级的断言已经在截图前逐张做过了，结果在 `blind` 里。）
    const crypto = require("crypto");
    const dupShots = [];
    const byHash = new Map();
    for (const c of captured) {
      const p = path.join(shotsDir, c.tag + ".png");
      const h = crypto.createHash("sha256").update(fs.readFileSync(p)).digest("hex");
      if (!byHash.has(h)) byHash.set(h, []);
      byHash.get(h).push(c.tag);
    }
    for (const [, tags] of byHash) {
      if (tags.length > 1) dupShots.push(tags.join(" = "));
    }

    const byTheme = new Map();
    for (const c of captured) {
      if (!byTheme.has(c.theme)) byTheme.set(c.theme, []);
      byTheme.get(c.theme).push(c);
    }
    let md = `# 人工交互验收 · 视觉走查清单\n\n`;
    md += `> 生成于 ${new Date().toISOString()} · 产物：\`node tests/visual-acceptance.cjs\`\n`;
    md += `> **这份清单是给人看的。** 机器断言的边界是"元素在不在、值对不对"；\n`;
    md += `> 而"这句话读起来顺不顺、这个按钮是不是放错了地方、这块留白是不是空的难受"——\n`;
    md += `> 只能靠看。v1.9.1 的 7 处 Markdown 记号就是**看截图**发现的，光看代码看不出来。\n\n`;
    md += `共 **${captured.length}** 张截图，覆盖 ${THEMES.length} 个主题 × ${STATES.length} 个界面状态`
      + `（含单独补拍的空状态）。\n\n`;
    // 盲区单列 —— **这一节比清单本身更重要**：它防止"验过了"的印象覆盖了
    // "其实没验到"。第一轮就是靠它才发现 notes-edit 从未进入编辑画面。
    if (blind.length || dupShots.length) {
      md += `## ⚠ 本轮**没有真正覆盖到**的状态（盲区）\n\n`;
      md += `> 下面是**截图过程自己的自检**报出来的：状态断言没过，或两张图逐字节相同。\n`;
      md += `> 这些状态在清单里也有一行，但**不要把它当成"看过并通过了"** —— \n`;
      md += `> 它说明这一轮没拍到那个画面（通常是造景/驱动没进到那个状态）。\n\n`;
      if (blind.length) {
        md += `| 编号（状态）| 为什么算盲区 |\n|------|------|\n`;
        for (const b of blind) md += `| ${b} | 截图前断言没过 |\n`;
      }
      if (dupShots.length) {
        md += `\n**逐字节相同的截图**（说明其中至少一张没真正切到目标状态）：\n\n`;
        for (const d of dupShots) md += `- ${d}\n`;
      }
      md += `\n`;
    }
    md += `## 怎么用\n\n`;
    md += `逐张打开 \`shots/<编号>.png\`，对着「该盯什么」那一列看，把结论填进最后一列。\n`;
    md += `**发现问题就记具体现象**（"侧栏第二个按钮的文字被截成两行"），不要写"感觉不太好"。\n\n`;
    for (const [themeId, items] of byTheme) {
      md += `## 主题：${items[0].themeLabel}（\`${themeId}\`）\n\n`;
      md += `| # | 截图 | 界面状态 | 该盯什么 | 结论 |\n`;
      md += `|:-:|------|---------|---------|------|\n`;
      for (const c of items) {
        md += `| ${c.tag.split("-")[0]} | \`shots/${c.tag}.png\` | ${c.state} | ${c.look} | ☐ 通过 ☐ 有问题： |\n`;
      }
      md += `\n`;
    }
    md += `## 四个主题分别在验什么\n\n`;
    md += `| 主题 | 为什么选它 |\n|------|-----------|\n`;
    md += `| 宣纸（浅色） | 默认主题，绝大多数用户看到的就这一套 |\n`;
    md += `| 夜墨（深色） | 深色下的对比度、边框可见性、阴影是否还有意义 —— 与浅色是**两套结构** |\n`;
    md += `| 高对比暗色 | 无障碍主题：边框更重、间距更大，最容易暴露出"靠留白撑起来"的布局 |\n`;
    md += `| 极简（无圆角无阴影） | 拿掉圆角与阴影之后，靠它们区分层次的地方会露馅 |\n`;
    md += `\n> 主题改的是**结构**不只是色相（这是本项目视觉系统的一条原则），\n`;
    md += `> 所以一套主题下没问题的排版，在另一套里可能才显形。\n\n`;
    if (failures.length) {
      md += `## ⚠ 本轮未能截到的状态\n\n`;
      for (const f of failures) md += `- ${f}\n`;
      md += `\n（这些是**造景/驱动失败**，不是"界面有问题" —— 但它们让对应状态成了盲区，要单独补看。）\n`;
    }
    fs.writeFileSync(path.join(OUT, "REVIEW.md"), md, "utf8");

    // 输出目录固定，方便下一棒按路径找
    fs.writeFileSync(
      path.join(ROOT, "local-docs", "runs", "LATEST-VISUAL.txt"),
      OUT + "\n",
      "utf8"
    );

    console.log(`\n✔ 截图 ${captured.length} 张 → ${shotsDir}`);
    console.log(`✔ 清单 → ${path.join(OUT, "REVIEW.md")}`);
    if (blind.length) {
      console.log(`\n⚠ ${blind.length} 个状态**没真正覆盖到**（盲区，清单里单列了一节）：`);
      for (const b of blind.slice(0, 12)) console.log("   · " + b);
    }
    if (dupShots.length) {
      console.log(`\n⚠ ${dupShots.length} 组截图逐字节相同（其中一张没切到目标状态）：`);
      for (const d of dupShots.slice(0, 8)) console.log("   · " + d);
    }
    if (failures.length) {
      console.log(`\n⚠ ${failures.length} 个状态没截到：`);
      for (const f of failures.slice(0, 10)) console.log("   · " + f);
    }
  } catch (e) {
    console.error("✘ 视觉走查失败：" + ((e && e.message) || e));
    process.exitCode = 1;
  } finally {
    try { if (app) app.kill(); } catch (_) {}
    await sleep(400);
    spawnSync("taskkill", ["/IM", "deskbase.exe", "/F"], { windowsHide: true });
    if (!KEEP) {
      try { fs.rmSync(dataDir, { recursive: true, force: true }); } catch (_) {}
    } else {
      console.log(`（--keep）数据目录保留：${dataDir}`);
    }
    console.log(`产物目录：${OUT}`);
  }
})();
