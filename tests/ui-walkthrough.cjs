#!/usr/bin/env node
/* ============================================================
   DeskBase 自动点击走查（ui-walkthrough.cjs）
   ============================================================
   它回答的是那个一直没人答的问题：**"界面看着怎么样？"**
   烟测（ui-smoke）证明的是"控件在、点了有反应、值真的落库"，
   但它一张截图都不留 —— 观感、文案、窄屏布局这些只有人眼能判断的东西，
   它帮不上忙。

   这个脚本的做法：把真实产物起起来（烟测模式 → CDP 端口 9222），
   从**外部**用 DevTools 协议去驱动浏览器 —— 真点击、真读取、真截图：
     · 每个主要页面留一张截图（自动拼成一份自带图的 HTML 报告）；
     · 导出每页的界面文案（供逐字审阅，不用开应用）；
     · 在 1000 / 800 / 640 / 520 四档宽度下做**布局体检**：
       横向溢出、元素跑出视口、单行文本被裁 —— 疑似问题逐条列出。

   它**不做**价值判断（"这个按钮丑"、"这句话该改"）—— 那是人做的事。
   它把"需要人看的东西"整理好摆在面前：截图 + 文案 + 疑似清单。

   用法：
     node tests/ui-walkthrough.cjs [<exe 路径>] [--out <目录>] [--keep]
   退出码：0 = 走查执行完整；1 = 有步骤失败；2 = 跑不起来（缺产物 / 连不上）
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
const outArgIdx = argv.indexOf("--out");
const stamp = new Date().toISOString().replace(/[:T]/g, "-").slice(0, 16);
const OUT = outArgIdx >= 0 && argv[outArgIdx + 1]
  ? path.resolve(argv[outArgIdx + 1])
  : path.join(ROOT, "local-docs", "runs", "walkthrough-" + stamp);
const BOOT_PAGE = path.join(__dirname, "walkthrough-boot.page.js");
const PORT = 9222;

if (!fs.existsSync(EXE)) {
  console.error(`✘ 找不到产物：${EXE}\n  先跑 node scripts/build.cjs 再试。`);
  process.exit(2);
}

const shotsDir = path.join(OUT, "shots");
const textDir = path.join(OUT, "text");
fs.mkdirSync(shotsDir, { recursive: true });
fs.mkdirSync(textDir, { recursive: true });

const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "deskbase-walk-"));
const logPath = path.join(dataDir, "logs", "app.log");

console.log(`自动点击走查 · ${EXE}`);
console.log(`输出目录：${OUT}`);
console.log("（窗口会出现在屏幕上若干秒，属正常）\n");

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const readLog = () => {
  try { return fs.readFileSync(logPath, "utf8"); } catch (_) { return ""; }
};

// ---------- CDP 客户端（Node 22 自带 WebSocket，零依赖） ----------
class CDP {
  constructor(ws) {
    this.ws = ws;
    this.seq = 0;
    this.pending = new Map();
    ws.addEventListener("message", (ev) => {
      let m;
      try { m = JSON.parse(ev.data); } catch (_) { return; }
      if (m.id && this.pending.has(m.id)) {
        const { res, rej } = this.pending.get(m.id);
        this.pending.delete(m.id);
        if (m.error) rej(new Error(m.error.message || "CDP 错误"));
        else res(m.result);
      }
    });
  }
  send(method, params) {
    const id = ++this.seq;
    return new Promise((res, rej) => {
      this.pending.set(id, { res, rej });
      this.ws.send(JSON.stringify({ id, method, params: params || {} }));
    });
  }
  async eval(expression) {
    const r = await this.send("Runtime.evaluate", {
      expression,
      returnByValue: true,
      awaitPromise: true,
    });
    if (r.exceptionDetails) {
      const d = r.exceptionDetails;
      throw new Error((d.exception && d.exception.description) || d.text || "页面抛异常");
    }
    return r.result ? r.result.value : undefined;
  }
}

async function connectCdp(ms) {
  const t0 = Date.now();
  for (;;) {
    try {
      const res = await fetch(`http://127.0.0.1:${PORT}/json/list`);
      const list = await res.json();
      const page =
        list.find((t) => t.type === "page" && /deskbase|localhost/.test(t.url || "")) ||
        list.find((t) => t.type === "page");
      if (page && page.webSocketDebuggerUrl) {
        const ws = new WebSocket(page.webSocketDebuggerUrl);
        await new Promise((ok, bad) => {
          const tt = setTimeout(() => bad(new Error("WS 连接超时")), 8000);
          ws.addEventListener("open", () => { clearTimeout(tt); ok(); });
          ws.addEventListener("error", () => { clearTimeout(tt); bad(new Error("WS 连接失败")); });
        });
        return { cdp: new CDP(ws), url: page.url };
      }
    } catch (_) {}
    if (Date.now() - t0 > ms) return null;
    await sleep(400);
  }
}

// ---------- 无障碍体检（在页面上下文里执行） ----------
//
// v1.0 门槛第 ① 条里有「M8 无障碍」。这一节就是它的机器核对 ——
// 和布局体检一样，**只查能确定判定对错的东西**，不做主观打分。
//
// 查的是四类"屏幕阅读器和键盘用户会直接卡住"的问题：
//   1. 可点元素没有可读名字 → 屏幕阅读器只会念"按钮"
//   2. 表单控件没有 label  → 不知道这一栏要填什么
//   3. 打开的对话框没有标题 → 进去了不知道自己在哪
//   4. aria-label 是空串    → 比没有更糟（有的实现会直接念空）
const A11Y_JS = `(() => {
  const problems = [];
  const selOf = (el) => {
    let s = el.tagName.toLowerCase();
    if (el.id) s += "#" + el.id;
    else if (typeof el.className === "string" && el.className.trim())
      s += "." + el.className.trim().split(/\\s+/).slice(0, 2).join(".");
    return s;
  };
  const nameOf = (el) => {
    const al = (el.getAttribute("aria-label") || "").trim();
    if (al) return al;
    const lb = el.getAttribute("aria-labelledby");
    if (lb) {
      const ref = document.getElementById(lb);
      if (ref && (ref.textContent || "").trim()) return ref.textContent.trim();
    }
    const txt = (el.textContent || "").trim();
    if (txt) return txt;
    return (el.getAttribute("title") || "").trim();
  };
  const visible = (el) => {
    const cs = getComputedStyle(el);
    if (cs.display === "none" || cs.visibility === "hidden") return false;
    if (el.getAttribute("aria-hidden") === "true") return false;
    return true;
  };

  for (const el of document.querySelectorAll("button, a[href], [role=button]")) {
    if (!visible(el)) continue;
    if (!nameOf(el)) {
      problems.push({ type: "no-accessible-name", sel: selOf(el), detail: "可点元素没有可读名字" });
    }
  }
  for (const el of document.querySelectorAll("input, select, textarea")) {
    if (el.type === "hidden") continue;
    if (!visible(el)) continue;
    const id = el.id;
    const hasFor = id && document.querySelector('label[for="' + id + '"]');
    const wrapped = el.closest("label");
    const aria = (el.getAttribute("aria-label") || "").trim();
    if (!hasFor && !wrapped && !aria) {
      problems.push({
        type: "control-no-label",
        sel: selOf(el),
        detail: "控件没有关联的 label（placeholder 不算）",
      });
    }
  }
  for (const d of document.querySelectorAll("dialog[open]")) {
    if (!nameOf(d)) {
      problems.push({ type: "dialog-no-title", sel: selOf(d), detail: "打开的对话框没有可读标题" });
    }
  }
  for (const el of document.querySelectorAll("[aria-label]")) {
    if (!(el.getAttribute("aria-label") || "").trim()) {
      problems.push({ type: "empty-aria-label", sel: selOf(el), detail: "aria-label 是空串" });
    }
  }
  return { count: problems.length, problems: problems.slice(0, 40) };
})()`;

// ---------- 页面里的布局体检（在页面上下文里执行） ----------
const AUDIT_JS = `(() => {
  const issues = [];
  const de = document.documentElement;
  const vw = window.innerWidth;
  if (de.scrollWidth > de.clientWidth + 1) {
    issues.push({ type: "page-h-overflow", sel: "html", detail: de.scrollWidth + " > " + de.clientWidth });
  }
  const sel = (el) => {
    let s = el.tagName.toLowerCase();
    if (el.id) s += "#" + el.id;
    else if (typeof el.className === "string" && el.className.trim())
      s += "." + el.className.trim().split(/\\s+/).slice(0, 2).join(".");
    return s;
  };
  for (const el of document.querySelectorAll("body *")) {
    const cs = getComputedStyle(el);
    if (cs.display === "none" || cs.visibility === "hidden" || cs.opacity === "0") continue;
    const r = el.getBoundingClientRect();
    if (r.width <= 0 || r.height <= 0) continue;
    if (r.right > vw + 2 && cs.position !== "fixed") {
      issues.push({ type: "out-of-viewport", sel: sel(el), detail: Math.round(r.left) + "…" + Math.round(r.right) + " / vw " + vw });
    }
    // 横跨左边界 = **一部分露在屏外**。抽屉类元素（窄屏隐藏的左栏）本该
    // 整块移出去，只露一条边说明「没关干净」—— 用户看到的是半截内容，
    // 比彻底看不见更糟。原来只查右边界超出，抓不到这一种。
    // 横跨左边界 = 可能有内容露在屏外。
    //
    // ⚠️ 判据不能只看几何位置：getBoundingClientRect 返回的是**布局位置**，
    // 祖先设了 overflow: hidden 时它照样往外报。实测踩过 —— 抽屉已经
    // 干净地移出屏幕、也加了 overflow-x: hidden，元素几何却仍是 -287…92，
    // 于是 40 条误报把真问题埋了。
    // 真正的判据是"那一块到底能不能被用户碰到"：拿可见部分的中点去问
    // elementFromPoint，点得到才算真露出来。
    if (r.left < -2 && r.right > 2 && cs.position !== "fixed") {
      const px = Math.min(r.right - 4, vw - 1);
      const py = Math.max(0, Math.min(r.top + r.height / 2, window.innerHeight - 1));
      let reallyVisible = false;
      if (px > 0 && py >= 0) {
        const hit = document.elementFromPoint(px, py);
        reallyVisible = !!hit && (hit === el || el.contains(hit) || hit.contains(el));
      }
      if (reallyVisible) {
        issues.push({
          type: "partially-offscreen",
          sel: sel(el),
          detail: Math.round(r.left) + "…" + Math.round(r.right) + "（露出 " + Math.round(r.right) + "px）",
        });
      }
    }
    // 窄屏下的**可用性**：目标太小就点不准。
    // 布局溢出类的问题上面两条能抓，但「按钮被压成 8px 宽」不算溢出 ——
    // 它老老实实待在视口里，只是没法用。
    if (vw < 720 && el.matches("button, input, select, textarea, a[href]")) {
      // 被 <label> 包着的 checkbox / radio：真正能点的是整个 label，
      // 13×13 是浏览器的方框尺寸，不是点击目标。按 label 的量。
      const box = el.closest("label") || el;
      const br = box.getBoundingClientRect();
      const tiny = br.width < 24 || br.height < 20;
      if (tiny) {
        issues.push({
          type: "target-too-small",
          sel: sel(el),
          detail: Math.round(br.width) + "×" + Math.round(br.height) + "  " + (el.textContent || "").trim().slice(0, 14),
        });
      }
    }
    // 文字**画到盒子外面**（内容溢出）。
    //
    // 这一种 rect 查不出来：元素盒子老老实实待在容器里，是里面的字越界了 ——
    // 实测踩过：侧栏里的引导正文横向捅出去 70px、压在右边的表格区上，
    // 而所有元素的 rect 都规规矩矩。overflow: visible 时 scrollWidth 也不反映，
    // 得用 Range 量**文字实际占了多宽**。
    if (el.children.length === 0) {
      const tx = (el.textContent || "").trim();
      if (tx.length > 1) {
        try {
          const rg = document.createRange();
          rg.selectNodeContents(el);
          const rr = rg.getBoundingClientRect();
          if (rr.width > 0 && rr.right > r.right + 3) {
            issues.push({
              type: "text-overflow",
              sel: sel(el),
              detail: tx.slice(0, 18) + "… 越界 " + Math.round(rr.right - r.right) + "px",
            });
          }
        } catch (_) {
          /* Range 不可用就算了 */
        }
      }
    }
    if (el.children.length === 0) {
      const tx = (el.textContent || "").trim();
      if (tx && el.scrollWidth > el.clientWidth + 2 && cs.overflow === "hidden" && cs.textOverflow !== "ellipsis") {
        issues.push({ type: "text-clipped", sel: sel(el), detail: tx.slice(0, 24) });
      }
    }
    if (issues.length > 60) break;
  }
  return { count: issues.length, issues: issues.slice(0, 60) };
})()`;

// ---------- 走查主体 ----------
(async () => {
  spawnSync("taskkill", ["/IM", "deskbase.exe", "/F"], { windowsHide: true });
  await sleep(700);

  const steps = [];
  const audits = [];
  const a11yIssues = [];
  const texts = {};
  const notes = [];
  const embeds = []; // { name, b64 }
  let shotNo = 0;
  let app = null;
  let appInfo = null;

  const step = (name, ok, detail) => {
    steps.push({ name, ok: !!ok, detail: detail == null ? "" : String(detail) });
    console.log(`${ok ? "✔" : "✖"} ${name}${detail ? `　（${detail}）` : ""}`);
  };

  try {
    const env = { ...process.env, DESKBASE_DATA_DIR: dataDir, DESKBASE_UI_SMOKE: BOOT_PAGE };
    delete env.DESKBASE_RENDER;
    const errFd = fs.openSync(path.join(dataDir, "stderr.log"), "a");
    app = spawn(EXE, [], { env, stdio: ["ignore", "ignore", errFd], windowsHide: false });

    const conn = await connectCdp(45000);
    if (!conn) throw new Error("45 秒内连不上 CDP（9222）—— 应用没起来或端口没开");
    const cdp = conn.cdp;
    await cdp.send("Page.enable");
    await cdp.send("Runtime.enable");
    console.log(`已连上页面：${conn.url}\n`);

    async function shot(name) {
      shotNo += 1;
      const tag = String(shotNo).padStart(2, "0") + "-" + name;
      const png = await cdp.send("Page.captureScreenshot", { format: "png" });
      fs.writeFileSync(path.join(shotsDir, tag + ".png"), Buffer.from(png.data, "base64"));
      const jpg = await cdp.send("Page.captureScreenshot", { format: "jpeg", quality: 84 });
      embeds.push({ name: tag, b64: jpg.data });
      return tag;
    }
    async function waitPage(expr, ms) {
      const t0 = Date.now();
      for (;;) {
        let v = false;
        try { v = await cdp.eval(expr); } catch (_) {}
        if (v) return true;
        if (Date.now() - t0 > ms) return false;
        await sleep(120);
      }
    }
    const PAGE_NAMES = { workbench: "工作台", notes: "笔记", database: "数据库", settings: "设置" };
    const PAGES = ["workbench", "notes", "database", "settings"];

    async function navTo(t) {
      await cdp.eval(`(function(){ var b = document.querySelector('.nav-item[data-target="${t}"]'); if (b) b.click(); return !!b; })()`);
      return waitPage(`(function(){ var v = document.querySelector('section.view[data-view="${t}"]'); return !!(v && v.dataset.active === "true"); })()`, 4000);
    }
    async function dumpText(name) {
      try {
        const tx = await cdp.eval("document.body.innerText");
        texts[name] = (tx || "").length;
        fs.writeFileSync(path.join(textDir, name + ".txt"), tx || "");
      } catch (_) {}
    }

    // ---------- 0. 就绪 ----------
    const ready = await waitPage(
      `!!(window.__deskbase && window.__deskbase.call) && !!document.querySelector('.nav-item')`,
      30000
    );
    step("界面就绪（IPC 桥 + 导航都在）", ready, ready ? "" : "30 秒内没就绪");

    // 应用自述（版本等）进报告头
    try { appInfo = await cdp.eval(`window.__deskbase.call("app.info")`); } catch (_) {}
    await shot("boot-首屏");

    // ---------- 1. 每个主要页面：截图 + 文案 ----------
    for (const t of PAGES) {
      const ok = await navTo(t);
      step(`切到「${PAGE_NAMES[t]}」`, ok, ok ? "" : "视图没激活");
      if (ok) {
        await sleep(350);
        await shot("page-" + t);
        await dumpText("page-" + t);
      }
    }

    // ---------- 2. 导入向导（真实打开、截图、关闭） ----------
    await navTo("database");
    const opened = await cdp.eval(
      `(function(){ if (window.DeskBaseDb && window.DeskBaseDb.openImportDialog) { window.DeskBaseDb.openImportDialog({ autoPick: false }); return true; } return false; })()`
    );
    if (opened) {
      const shown = await waitPage(`!!document.getElementById("db-dialog-import")`, 5000);
      step("导入向导能打开（autoPick:false）", shown);
      if (shown) {
        await shot("dialog-import");
        await dumpText("dialog-import");
        await cdp.eval(`(function(){ var d = document.getElementById("db-dialog-import"); if (d) d.close("cancel"); return true; })()`);
        await sleep(250);
      }
    } else {
      step("导入向导能打开（autoPick:false）", false, "找不到 DeskBaseDb.openImportDialog");
    }

    // 建表向导（选择器不保证存在 —— 存在就点，不存在只记录，不算失败）
    const hasNew = await cdp.eval(`!!document.getElementById("btn-db-new")`);
    if (hasNew) {
      await cdp.eval(`document.getElementById("btn-db-new").click()`);
      const dlgOpen = await waitPage(`!!document.getElementById("db-dialog-new")`, 4000);
      step("建表向导能打开", dlgOpen);
      if (dlgOpen) {
        await shot("dialog-newtable");
        await cdp.eval(`(function(){ var d = document.getElementById("db-dialog-new"); if (d) d.close(); return true; })()`);
        await sleep(250);
      }
    } else {
      notes.push("建表向导：找不到 #btn-db-new，本次跳过（不影响其它步骤）");
    }

    // ---------- 3. 窄屏布局体检 ----------
    const WIDTHS = [
      { w: 1000, shot: true },
      { w: 800, shot: true },
      { w: 640, shot: true },
      { w: 520, shot: false },
    ];
    let emuOk = true;
    try {
      await cdp.send("Emulation.setDeviceMetricsOverride", {
        width: 1000, height: 780, deviceScaleFactor: 1, mobile: false,
      });
    } catch (e) {
      emuOk = false;
      notes.push("窄屏模拟不可用（Emulation 域被拒）：" + (e && e.message));
    }
    if (emuOk) {
      for (const { w, shot: doShot } of WIDTHS) {
        await cdp.send("Emulation.setDeviceMetricsOverride", {
          width: w, height: 780, deviceScaleFactor: 1, mobile: false,
        });
        for (const t of PAGES) {
          const ok = await navTo(t);
          // 等这一页**真的活过来**再量。
          //
          // 踩过两次坑，都是"量了个寂寞"却报 0 处问题：
          //  1. 视图刚切、还没激活（.view 非 active 时是 display: none）——
          //     量到的每个元素 rect 都是 0，体检自然全过；
          //  2. 表格页的列表是懒加载的（切过去才调 IPC 拉表），空态引导块
          //     还要等 MutationObserver 回调，固定等 320ms 仍然拍不到。
          // 所以：先等视图可见，再等异步内容落地。
          await waitPage(
            "(function(){var v=document.querySelector('.view[data-view=\"' + t + '\"]');" +
              "return !!v && getComputedStyle(v).display !== 'none'})()",
            1200
          ).catch(function () {});
          // 只有表格页的列表是懒加载 + 异步插引导块的，别的页不用等
          if (t === "database") {
            await waitPage(
              "(function(){var l=document.getElementById('db-table-list');" +
                "return !!l && l.children.length > 0})()",
              1500
            ).catch(function () {});
          }
          await sleep(100);
          try {
            await cdp.eval(
              "new Promise(function(r){requestAnimationFrame(function(){requestAnimationFrame(function(){r(true)})})})"
            );
          } catch (_) {}
          if (!ok) continue;
          await sleep(250);
          let audit = null;
          try { audit = await cdp.eval(AUDIT_JS); } catch (_) {}
          // 无障碍体检跟着布局体检一起跑 —— 都是"切到这一页、等它活过来、再量"
          let a11y = null;
          try { a11y = await cdp.eval(A11Y_JS); } catch (_) {}
          if (a11y && a11y.count > 0) {
            a11y.problems.forEach((x) => a11yIssues.push({ page: t, width: w, ...x }));
          }
          audits.push({ width: w, page: t, count: audit ? audit.count : -1, issues: audit ? audit.issues : [] });
          if (doShot) await shot(`w${w}-${t}`);
        }
      }
      await cdp.send("Emulation.clearDeviceMetricsOverride");
      const totalIssues = audits.reduce((s, a) => s + Math.max(0, a.count), 0);
      step(`窄屏布局体检完成（4 档宽度 × 4 页，疑似问题 ${totalIssues} 处）`, true);

      // 无障碍体检汇总：按「问题类型 + 元素」去重 —— 同一处问题在 4 档宽度里
      // 会各报一次，不去重的话 4 倍噪音会把真问题淹掉。
      const uniqA11y = new Map();
      for (const x of a11yIssues) {
        const k = x.type + "|" + x.sel;
        if (!uniqA11y.has(k)) uniqA11y.set(k, x);
      }
      const a11yList = [...uniqA11y.values()];
      if (a11yList.length) {
        console.log(`\n无障碍体检：${a11yList.length} 类问题（已按元素去重）`);
        a11yList.slice(0, 20).forEach((x) =>
          console.log(`  · [${x.type}] ${x.sel}（${x.page}）：${x.detail}`)
        );
      } else {
        console.log("\n无障碍体检：✔ 无疑似问题");
      }
    }

    // 回到原生尺寸，收尾
    await navTo("workbench");

    // ---------- 4. 正常退出（走 smokeReport → 与"关闭窗口"同一条收尾路径） ----------
    try {
      await cdp.eval(`window.__deskbase.call("app.smokeReport", { steps: [], failures: [] })`);
    } catch (_) {}
  } catch (e) {
    step("走查过程出现异常", false, (e && e.message) || String(e));
  }

  // ---------- 汇总输出 ----------
  const meta = {
    stamp,
    exe: EXE,
    version: appInfo && appInfo.version ? appInfo.version : "?",
    pages: ["workbench", "notes", "database", "settings"],
  };
  const failed = steps.filter((s) => !s.ok).length;
  const auditTotal = audits.reduce((s, a) => s + Math.max(0, a.count), 0);

  fs.writeFileSync(
    path.join(OUT, "report.json"),
    JSON.stringify({ meta, notes, steps, audits, texts }, null, 2)
  );
  fs.writeFileSync(path.join(OUT, "index.html"), buildHtml({ meta, steps, audits, embeds, texts }));

  // 等应用退出（给宽限 800ms + 保存时间）
  if (app) {
    await new Promise((res) => {
      const t = setTimeout(res, 12000);
      if (app.exitCode !== null) { clearTimeout(t); return res(); }
      app.once("exit", () => { clearTimeout(t); res(); });
    });
    if (app.exitCode === null) {
      try { app.kill(); } catch (_) {}
    }
  }

  console.log("");
  if (notes.length) {
    console.log("备注：");
    notes.forEach((n) => console.log("  · " + n));
  }
  console.log(`走查完成：${steps.length - failed} / ${steps.length} 步通过` + (failed ? `，失败 ${failed}` : ""));
  console.log(`布局体检：疑似问题 ${auditTotal} 处（0 处 = 各档宽度都没有溢出/裁切）`);
  console.log(`报告：${path.join(OUT, "index.html")}`);
  console.log(`截图：${shotsDir}（${embeds.length} 张）· 文案：${textDir}`);

  if (failed || auditTotal > 0) {
    console.log("\n--- app.log 尾部 ---");
    readLog().trim().split("\n").slice(-10).forEach((l) => console.log("  " + l));
  }
  try {
    if (!KEEP && !failed) fs.rmSync(dataDir, { recursive: true, force: true });
    else console.log(`临时数据目录保留：${dataDir}`);
  } catch (_) {}

  process.exit(failed ? 1 : 0);
})();

// ---------- HTML 报告 ----------
function esc(s) {
  return String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

function buildHtml({ meta, steps, audits, embeds, texts }) {
  const rows = steps
    .map(
      (s) =>
        `<tr class="${s.ok ? "ok" : "bad"}"><td>${s.ok ? "✔" : "✖"}</td><td>${esc(s.name)}</td><td>${esc(s.detail)}</td></tr>`
    )
    .join("\n");

  const shots = embeds
    .map(
      (e) =>
        `<figure class="shot"><img src="data:image/jpeg;base64,${e.b64}" alt="${esc(e.name)}"><figcaption>${esc(e.name)}</figcaption></figure>`
    )
    .join("\n");

  const auditSections = [1000, 800, 640, 520]
    .map((w) => {
      const rows2 = audits
        .filter((a) => a.width === w)
        .map((a) => {
          const list = (a.issues || [])
            .map((i) => `<li><code>${esc(i.type)}</code> ${esc(i.sel)} — ${esc(i.detail)}</li>`)
            .join("");
          return `<details ${a.count > 0 ? "open" : ""}><summary>宽度 ${w} · ${esc(a.page)}：疑似 ${a.count} 处</summary><ul>${list || "<li>（无）</li>"}</ul></details>`;
        })
        .join("\n");
      const tot = audits.filter((a) => a.width === w).reduce((s, a) => s + Math.max(0, a.count), 0);
      return `<h3>宽度 ${w}px（合计疑似 ${tot} 处）</h3>\n${rows2 || "<p>（未采集）</p>"}`;
    })
    .join("\n");

  return `<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<title>DeskBase 界面走查报告 · ${esc(meta.stamp)}</title>
<style>
  :root { color-scheme: light; }
  body { margin: 0; padding: 32px clamp(16px, 5vw, 64px); background: #f7f5f0; color: #2a2520;
         font: 14px/1.75 "PingFang SC", "Microsoft YaHei", system-ui, sans-serif; }
  h1 { font-size: 22px; margin: 0 0 4px; }
  h2 { font-size: 17px; margin: 36px 0 12px; border-bottom: 2px solid #d9d2c5; padding-bottom: 6px; }
  h3 { font-size: 15px; margin: 20px 0 8px; }
  .meta { color: #6b6154; font-size: 12.5px; margin-bottom: 24px; }
  .meta code { background: #efeae0; padding: 1px 6px; border-radius: 4px; }
  .hint { background: #fff8e6; border: 1px solid #e6d9b8; border-radius: 8px; padding: 12px 16px; font-size: 13px; margin: 16px 0; }
  table.steps { border-collapse: collapse; width: 100%; font-size: 13px; }
  table.steps td { border-bottom: 1px solid #e5ded2; padding: 6px 10px; vertical-align: top; }
  tr.bad td { background: #fdeeee; }
  table.steps td:first-child { width: 28px; text-align: center; }
  .grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(320px, 1fr)); gap: 16px; }
  .shot { margin: 0; background: #fff; border: 1px solid #e0d9cc; border-radius: 10px; overflow: hidden; }
  .shot img { display: block; width: 100%; height: auto; }
  .shot figcaption { padding: 6px 10px; font-size: 12px; color: #6b6154; background: #fbf9f4; }
  details { margin: 6px 0; background: #fff; border: 1px solid #e5ded2; border-radius: 8px; padding: 8px 12px; }
  summary { cursor: pointer; font-size: 13px; }
  details ul { margin: 8px 0 4px; font-size: 12.5px; color: #7a4b3a; }
  code { font-family: Consolas, monospace; background: #f2ede3; padding: 0 4px; border-radius: 3px; }
</style>
</head>
<body>
<h1>DeskBase 界面走查报告</h1>
<div class="meta">
  时间 <code>${esc(meta.stamp)}</code> · 版本 <code>v${esc(meta.version)}</code><br>
  产物 <code>${esc(meta.exe)}</code>
</div>

<div class="hint">
  <b>怎么读这份报告</b>：截图是真实 WebView 里的真实界面（外部通过 DevTools 协议点击与抓取）。
  布局体检是<b>启发式</b>的 —— 它只找三类机械问题（横向溢出 / 跑出视口 / 单行文本被裁），
  标为"疑似"的都要人眼复核；观感与文案好不好，机器不发表意见，请直接看截图与 text/ 目录的文案导出口。
</div>

<h2>一、走查步骤</h2>
<table class="steps">${rows}</table>

<h2>二、截图（${embeds.length} 张）</h2>
<div class="grid">${shots}</div>

<h2>三、窄屏布局体检</h2>
${auditSections}

</body>
</html>`;
}
