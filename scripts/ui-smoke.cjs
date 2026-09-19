#!/usr/bin/env node
/* ============================================================
   DeskBase 界面烟测（ui-smoke.cjs）
   ============================================================
   干什么：把**真实构建产物**起起来（用临时数据目录，不碰你的数据），
   等界面自检完成后注入 scripts/ui-smoke.page.js 做真实点击测试，
   读回 smoke-report.json 按步打印 ✔/✖。

   用法：
     node scripts/ui-smoke.cjs              # 测 release 产物
     node scripts/ui-smoke.cjs <exe 路径>    # 测指定产物（如 debug）
     node scripts/build.cjs --smoke         # 构建后直接烟测（一条命令）

   退出码：0 = 全过；1 = 有失败项；2 = 跑不起来（缺产物 / 超时 / 没报告）

   为什么需要它：更新机制这种"点一下 → 联网 → 改自己"的功能，单元测试
   测不到界面接线与真实 WebView 里的行为。它补的就是这一段。
   （下载 / 替换那两步要真下几 MB、真替换文件，不放在烟测里 ——
   那两条由 Rust 测试 + 手动验收覆盖。）
   注：窗口会在屏幕上短暂出现，属正常。
   ============================================================ */

const fs = require("fs");
const os = require("os");
const path = require("path");
const { spawn } = require("child_process");

const ROOT = path.resolve(__dirname, "..");
const EXE = process.argv[2]
  ? path.resolve(process.argv[2])
  : path.join(ROOT, "app", "target", "release", "deskbase.exe");
const PAGE = path.join(__dirname, "ui-smoke.page.js");
const TIMEOUT_MS = 120000;

if (!fs.existsSync(EXE)) {
  console.error(`✘ 找不到产物：${EXE}\n  先跑 node scripts/build.cjs 再试。`);
  process.exit(2);
}
if (!fs.existsSync(PAGE)) {
  console.error(`✘ 找不到页面脚本：${PAGE}`);
  process.exit(2);
}

const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "deskbase-uismoke-"));
const reportPath = path.join(dataDir, "smoke-report.json");
const logPath = path.join(dataDir, "logs", "app.log");

console.log(`界面烟测 · ${EXE}`);
console.log(`临时数据目录：${dataDir}`);
console.log("（窗口会短暂出现，属正常）\n");

const env = { ...process.env, DESKBASE_DATA_DIR: dataDir, DESKBASE_UI_SMOKE: PAGE };
delete env.DESKBASE_RENDER; // 别把构建期钩子带进来

// stderr 落盘而不是丢弃：Rust 侧的 eprintln 与崩溃信息只从这里出来，
// "进程活着但什么都没发生"这类现象，唯一的旁证往往就在这几行里。
const errPath = path.join(dataDir, "stderr.log");
const errFd = fs.openSync(errPath, "w");
const child = spawn(EXE, [], { env, stdio: ["ignore", "ignore", errFd], windowsHide: false });
let timedOut = false;
const timer = setTimeout(() => {
  timedOut = true;
  try {
    child.kill();
  } catch (_) {}
}, TIMEOUT_MS);

child.on("error", (e) => {
  clearTimeout(timer);
  console.error(`✘ 起不来：${e.message}`);
  process.exit(2);
});
child.on("exit", (code) => {
  clearTimeout(timer);
  // 报告在退出前同步写出，这里只是防文件系统延迟
  setTimeout(() => finish(code), 150);
});

function tail(file, n) {
  try {
    return fs.readFileSync(file, "utf8").trim().split("\n").slice(-n);
  } catch (_) {
    return [];
  }
}

function finish(code) {
  let report = null;
  try {
    report = JSON.parse(fs.readFileSync(reportPath, "utf8"));
  } catch (_) {}

  if (!report) {
    console.error(
      timedOut
        ? `✘ 超时（${TIMEOUT_MS / 1000}s）：应用没有写出报告。`
        : `✘ 没有报告文件（进程退出码 ${code}）。`
    );
    // 超时排查靠的就是日志 —— 截断会正好把线索切掉，所以这里给足行数
    const lines = tail(logPath, 60);
    if (lines.length) {
      console.error(`\n--- app.log 尾部（最后 ${lines.length} 行）---`);
      lines.forEach((l) => console.error("  " + l));
    } else {
      console.error("\n（读不到 app.log —— 应用可能根本没起来）");
    }
    const errLines = tail(errPath, 30);
    if (errLines.length) {
      console.error(`\n--- stderr 尾部（最后 ${errLines.length} 行）---`);
      errLines.forEach((l) => console.error("  " + l));
    }
    console.error(`\n临时目录保留供排查：${dataDir}`);
    process.exit(2);
  }

  const steps = Array.isArray(report.steps) ? report.steps : [];
  const failures = Array.isArray(report.failures) ? report.failures : [];
  for (const s of steps) {
    console.log(`${s.ok ? "✔" : "✖"} ${s.name}${s.detail ? `　（${s.detail}）` : ""}`);
  }
  const pass = steps.filter((s) => s.ok).length;
  console.log(
    `\n${failures.length ? "✘" : "✔"} 通过 ${pass} / ${steps.length}` +
      (failures.length ? `，失败 ${failures.length}` : "")
  );
  if (failures.length) {
    console.log("");
    failures.forEach((f) => console.log("  ✖ " + f));
    console.log("\n--- app.log 尾部 ---");
    tail(logPath, 12).forEach((l) => console.log("  " + l));
    console.log(`\n临时目录保留供排查：${dataDir}`);
    process.exit(1);
  }
  // 全过：清掉临时目录
  try {
    fs.rmSync(dataDir, { recursive: true, force: true });
  } catch (_) {}
  process.exit(0);
}
