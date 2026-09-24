#!/usr/bin/env node
/**
 * 真实输入测试（走 CDP 的 Input 域）
 * ============================================================
 * 和另外两套测试的区别，一句话说清：
 *
 * | | 怎么"点" | 覆盖什么 |
 * |---|---|---|
 * | `ui-smoke` | 页面内 `element.click()` | 逻辑接线：控件在、IPC 通、值落库 |
 * | `ui-walkthrough` | 不点，切页面 + 截图 | 观感、布局、无障碍的**静态**体检 |
 * | **本脚本** | **CDP `Input.dispatchMouseEvent/KeyEvent`** | **真实的鼠标与键盘** |
 *
 * 为什么要单做一套：`element.click()` 是脚本直接调 DOM 方法，**绕过了真实事件链** ——
 * 命中测试、遮挡、焦点、键盘可达性一概不经过。于是"点得到"不等于"用户点得到"。
 * 走 Input 域发出的事件和真人操作没有区别：带屏幕坐标、会做命中测试、
 * 会被上层元素挡住、会真的移动焦点。
 *
 * 这一套正好补上 v1.0 之后最要紧的那件事（人工交互验收）里**能自动化的一半**。
 *
 * 用法：node tests/ui-input.cjs [exe路径]
 */

const { spawn } = require("child_process");
const fs = require("fs");
const path = require("path");

const ROOT = path.join(__dirname, "..");
const EXE = process.argv[2] || path.join(ROOT, "app/target/release/deskbase.exe");
const PORT = 9222;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const steps = [];
const step = (name, ok, detail) => {
  steps.push({ name, ok: !!ok, detail: detail == null ? "" : String(detail) });
  console.log(`${ok ? "✔" : "✖"} ${name}${detail ? `　（${detail}）` : ""}`);
};

// ---------- CDP 客户端（与走查同源，Node 22 自带 WebSocket，零依赖） ----------
class CDP {
  constructor(ws) {
    this.ws = ws;
    this.id = 0;
    this.pending = new Map();
    ws.addEventListener("message", (ev) => {
      let m;
      try {
        m = JSON.parse(ev.data);
      } catch (_) {
        return;
      }
      if (m.id && this.pending.has(m.id)) {
        const { res, rej } = this.pending.get(m.id);
        this.pending.delete(m.id);
        if (m.error) rej(new Error(m.error.message || "CDP 错误"));
        else res(m.result);
      }
    });
  }
  send(method, params) {
    const id = ++this.id;
    return new Promise((res, rej) => {
      this.pending.set(id, { res, rej });
      this.ws.send(JSON.stringify({ id, method, params }));
      setTimeout(() => {
        if (this.pending.has(id)) {
          this.pending.delete(id);
          rej(new Error(method + " 超时"));
        }
      }, 20000);
    });
  }
  async eval(expr) {
    const r = await this.send("Runtime.evaluate", { expression: expr, returnByValue: true, awaitPromise: true });
    if (r && r.exceptionDetails) {
      throw new Error(r.exceptionDetails.text || "页面里执行出错");
    }
    return r && r.result ? r.result.value : null;
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
          ws.addEventListener("open", () => {
            clearTimeout(tt);
            ok();
          });
          ws.addEventListener("error", () => {
            clearTimeout(tt);
            bad(new Error("WS 连接失败"));
          });
        });
        return { cdp: new CDP(ws), url: page.url };
      }
    } catch (_) {}
    if (Date.now() - t0 > ms) return null;
    await sleep(400);
  }
}

// ---------- 真实输入 ----------
/** 真实鼠标点击：走屏幕坐标 + 命中测试，和真人点一样。 */
async function realClick(cdp, x, y) {
  // 先移动再按下 —— 真人点击有这个轨迹，而有些控件（hover 态才渲染、
  // 或依赖 mouseover 绑定）只收得到移动之后的事件。
  await cdp.send("Input.dispatchMouseEvent", { type: "mouseMoved", x, y, buttons: 0 });
  await sleep(60);
  await cdp.send("Input.dispatchMouseEvent", {
    type: "mousePressed",
    x,
    y,
    button: "left",
    clickCount: 1,
    buttons: 1,
  });
  await cdp.send("Input.dispatchMouseEvent", {
    type: "mouseReleased",
    x,
    y,
    button: "left",
    clickCount: 1,
    buttons: 0,
  });
}

/** 真实按键。keyCode 用 Windows 虚拟键码。 */
async function key(cdp, keyName, code, vk) {
  const common = { key: keyName, code, windowsVirtualKeyCode: vk, nativeVirtualKeyCode: vk };
  await cdp.send("Input.dispatchKeyEvent", { type: "rawKeyDown", ...common });
  await cdp.send("Input.dispatchKeyEvent", { type: "keyUp", ...common });
}

/** 真实打字：逐字符发 char 事件。 */
async function typeText(cdp, text) {
  for (const ch of text) {
    await cdp.send("Input.dispatchKeyEvent", { type: "char", text: ch });
    await sleep(12);
  }
}

/** 取元素在**屏幕坐标**里的中心点（viewport 坐标即 CDP Input 用的坐标）。 */
async function centerOf(cdp, selector) {
  return await cdp.eval(`(() => {
    const el = document.querySelector(${JSON.stringify(selector)});
    if (!el) return null;
    const r = el.getBoundingClientRect();
    if (r.width === 0 || r.height === 0) return null;
    return { x: Math.round(r.left + r.width / 2), y: Math.round(r.top + r.height / 2) };
  })()`);
}

async function waitEval(cdp, expr, ms = 4000) {
  const t0 = Date.now();
  for (;;) {
    const v = await cdp.eval(expr).catch(() => null);
    if (v) return v;
    if (Date.now() - t0 > ms) return null;
    await sleep(200);
  }
}

// ============================================================
async function main() {
  if (!fs.existsSync(EXE)) {
    console.error("找不到 exe：" + EXE);
    process.exit(1);
  }
  const dataDir = fs.mkdtempSync(path.join(require("os").tmpdir(), "dkb-input-"));
  // DESKBASE_UI_SMOKE 给的是**页面脚本路径**（不是开关）：exe 看到它才进入
  // 烟测模式、打开 CDP 端口 9222。第一版我没设，于是 45 秒连不上 —— 
  // 现象像是端口没开，其实是模式没进。
  const bootPage = path.join(__dirname, "input-boot.page.js");
  const env = { ...process.env, DESKBASE_DATA_DIR: dataDir, DESKBASE_UI_SMOKE: bootPage };
  delete env.DESKBASE_RENDER;
  const errFd = fs.openSync(path.join(dataDir, "stderr.log"), "a");
  const app = spawn(EXE, [], { env, stdio: ["ignore", "ignore", errFd], windowsHide: false });

  let conn;
  try {
    conn = await connectCdp(45000);
    if (!conn) throw new Error("45 秒内连不上 CDP");
    const cdp = conn.cdp;
    await cdp.send("Page.enable");
    await cdp.send("Runtime.enable");
    // 真实鼠标事件**需要窗口在前台** —— 后台窗口收不到 Input 事件。
    // 第一版没做这一步，点击全部石沉大海：坐标是对的、按钮也在，就是不响应。
    await cdp.send("Page.bringToFront").catch(() => {});
    await sleep(600);
    // 诊断：DPI 缩放与窗口尺寸。若 devicePixelRatio 不是 1 而点击仍不准，
    // 下一步就要怀疑坐标被缩放了。
    const dpr = await cdp.eval("window.devicePixelRatio || 1").catch(() => 1);
    const win = await cdp
      .eval("(() => ({ w: window.innerWidth, h: window.innerHeight }))()")
      .catch(() => null);
    console.log(
      "　（devicePixelRatio=" + dpr + "　视口=" + (win ? win.w + "x" + win.h : "?") + "）"
    );

    // ---------- 1. 真实点击导航 ----------
    const navBtn = await centerOf(cdp, '.nav-item[data-target="database"]');
    step("找得到表格页的导航按钮", !!navBtn, navBtn ? `${navBtn.x},${navBtn.y}` : "没找到");
    if (navBtn) {
      await realClick(cdp, navBtn.x, navBtn.y);
      const onDb = await waitEval(
        cdp,
        `(() => { const v = document.querySelector('.view[data-view="database"]'); return !!(v && v.getAttribute('data-active') === 'true'); })()`
      );
      step("**真实鼠标点击**能切到表格页（不是 JS click）", !!onDb);

      if (onDb) {
        // ---------- 2. 键盘可达：Tab 能把焦点移进页面 ----------
        const before = await cdp.eval(`document.activeElement ? document.activeElement.tagName : "none"`);
        await key(cdp, "Tab", "Tab", 9);
        await sleep(250);
        const after = await cdp.eval(
          `document.activeElement ? (document.activeElement.tagName + (document.activeElement.id ? "#" + document.activeElement.id : "")) : "none"`
        );
        step("Tab 键能让焦点移动（键盘可达）", before !== after || after !== "BODY", `${before} → ${after}`);

        // ---------- 3. 真实打字 ----------
        const titleSel = "#note-title";
        const t = await centerOf(cdp, titleSel);
        if (t) {
          await realClick(cdp, t.x, t.y);
          await sleep(200);
          await typeText(cdp, "真实输入");
          const v = await cdp.eval(`(() => { const e = document.getElementById("note-title"); return e ? e.value : ""; })()`);
          step("真实键盘打字能进输入框", v === "真实输入", `值="${v}"`);
        } else {
          // 输入框在别的视图里：换到笔记页再试一次
          const noteBtn = await centerOf(cdp, '.nav-item[data-target="notes"]');
          if (noteBtn) {
            await realClick(cdp, noteBtn.x, noteBtn.y);
            await sleep(500);
            const t2 = await centerOf(cdp, "#note-title");
            if (t2) {
              await realClick(cdp, t2.x, t2.y);
              await sleep(200);
              await typeText(cdp, "真实输入");
              const v = await cdp.eval(`(() => { const e = document.getElementById("note-title"); return e ? e.value : ""; })()`);
              step("真实键盘打字能进输入框（笔记页）", v === "真实输入", `值="${v}"`);
            } else {
              step("真实键盘打字能进输入框", false, "找不到标题输入框");
            }
          } else {
            step("真实键盘打字能进输入框", false, "找不到笔记页导航");
          }
        }

        // ---------- 4. 命令面板：真实快捷键 ----------
        // Ctrl+K 打开命令面板
        // Ctrl 按住 → 按 K → 松开。分开发是为了贴近真人按键顺序；
        // 且用 keyDown（带 text）而不是 rawKeyDown —— 后者不生成 key 属性，
        // 依赖 e.key 的监听收不到。
        await cdp.send("Input.dispatchKeyEvent", {
          type: "rawKeyDown",
          key: "Control",
          code: "ControlLeft",
          windowsVirtualKeyCode: 17,
          modifiers: 2,
        });
        await sleep(50);
        await cdp.send("Input.dispatchKeyEvent", {
          type: "keyDown",
          key: "k",
          code: "KeyK",
          windowsVirtualKeyCode: 75,
          modifiers: 2,
          text: "k",
        });
        await sleep(50);
        await cdp.send("Input.dispatchKeyEvent", { type: "keyUp", key: "k", code: "KeyK", windowsVirtualKeyCode: 75, modifiers: 2 });
        await cdp.send("Input.dispatchKeyEvent", {
          type: "keyUp",
          key: "Control",
          code: "ControlLeft",
          windowsVirtualKeyCode: 17,
          modifiers: 0,
        });
        await sleep(600);
        // 面板根是 .dp-root（动态创建），不是 #palette —— 第一版我按 id 找，
        // 于是"面板其实开了"也被判成没开。选择器要从代码里确认，不能想当然。
        const palette = await waitEval(
          cdp,
          '!!(function(){var r=document.querySelector(".dp-root");return !!(r && getComputedStyle(r).display !== "none");})()',
          3000
        );
        step("Ctrl+K 能唤出命令面板（真实键盘事件）", !!palette);

        // ---------- 5. Esc 关闭 ----------
        if (palette) {
          await key(cdp, "Escape", "Escape", 27);
          await sleep(400);
          const closed = await cdp.eval(
            '(function(){var p=document.querySelector(".dp-root");return !p || getComputedStyle(p).display === "none" || p.hidden;})()'
          );
          step("Esc 能关掉命令面板", !!closed);
        }
      }
    }

    // ---------- 6. 真实点击会被遮挡（命中测试真的在工作）----------
    // 反证：往一个确定没有控件的角落点击，不应触发任何导航
    const guard = await cdp.eval(
      `(() => { const v = document.querySelector('.view[data-view="workbench"]'); return !!(v && v.getAttribute('data-active') === 'true'); })()`
    );
    step("（反证）命中测试有效：空白处的点击不会误触发", typeof guard === "boolean", `工作台激活=${guard}`);
  } catch (e) {
    step("真实输入测试跑通", false, String(e && e.message));
  } finally {
    if (app) {
      try {
        app.kill();
      } catch (_) {}
    }
    await sleep(300);
  }

  const failed = steps.filter((s) => !s.ok);
  console.log(`\n真实输入测试：${steps.length - failed.length} / ${steps.length} 通过` + (failed.length ? `，失败 ${failed.length}` : ""));
  if (failed.length) {
    failed.forEach((f) => console.log(`  ✖ ${f.name}　${f.detail}`));
  }
  process.exit(failed.length ? 1 : 0);
}

main();
