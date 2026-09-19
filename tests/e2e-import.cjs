#!/usr/bin/env node
/* ============================================================
   DeskBase 端到端验收（e2e-import.cjs）
   ============================================================
   干什么：把**真实构建产物**起起来（用临时数据目录，不碰你的数据），
   交给它一个**真实的表格文件**，然后在真实 WebView 里跑完整个导入流程 ——
   认表头 → 认类型 → 落库（分批、进度是真的）→ 左栏出现 → 读回数据 → 删表。

   与 ui-smoke.cjs 的分工：
     · ui-smoke  = 界面接线（导航、控件、设置落库）—— 要快，秒级
     · e2e-import = **一条完整的业务链路**，含真实数据、真实金额换算、真实增量写入
   两者都跑在真实 WebView 里，都只断言界面行为与库里的真实数据。

   用法：
     node tests/e2e-import.cjs                       # 自动造数据 + 跑
     node tests/e2e-import.cjs <表格文件>              # 用指定的文件
     node tests/e2e-import.cjs --keep                # 失败时保留临时目录

   退出码：0 = 全过；1 = 有失败项；2 = 跑不起来（缺产物 / 超时 / 没报告）

   ⚠️ 源文件路径通过 `DESKBASE_E2E_SOURCE` 交给应用 —— **不是**通过 IPC。
   渲染层因此拿不到也传不进路径（它只有一个不接受参数的令牌命令）。
   这条边界是本项目的核心安全属性，测试不能成为它的破口。
   ============================================================ */

const fs = require("fs");
const os = require("os");
const path = require("path");
const { spawn } = require("child_process");

const ROOT = path.resolve(__dirname, "..");
const EXE = path.join(ROOT, "app", "target", "release", "deskbase.exe");
const PAGE = path.join(__dirname, "e2e-import.page.js");
const TIMEOUT_MS = 180000;
const KEEP = process.argv.includes("--keep");

if (!fs.existsSync(EXE)) {
  console.error(`✘ 找不到产物：${EXE}\n  先跑 node scripts/build.cjs 再试。`);
  process.exit(2);
}
if (!fs.existsSync(PAGE)) {
  console.error(`✘ 找不到页面脚本：${PAGE}`);
  process.exit(2);
}

/** 造一份"像真的"客户台账：带标题行、中文列名、金额、长编号、前导零工号、日期 */
function makeSample(dir) {
  const rows = [
    "客户台账",
    "导出日期：2026-09-19",
    "客户名称,联系电话,工号,金额,是否结清,签单日期",
  ];
  const names = ["北京甲贸易", "上海乙实业", "广州丙科技", "成都丁商贸", "杭州戊电子"];
  for (let i = 1; i <= 1200; i++) {
    rows.push(
      [
        names[i % 5] + i,
        "138" + String(10000000 + i), // 11 位，必须当文本（数字会被 Excel 变科学计数）
        "00" + (1000 + i), // 前导零，当数字就永久丢零
        (i * 13.37).toFixed(2), // 两位小数金额
        i % 2 ? "是" : "否",
        "2026-0" + (1 + (i % 9)) + "-1" + (i % 9),
      ].join(",")
    );
  }
  const p = path.join(dir, "客户台账.csv");
  fs.writeFileSync(p, rows.join("\n"), "utf8");
  return p;
}

const argFile = process.argv.slice(2).find((a) => !a.startsWith("--"));
const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "deskbase-e2e-"));
const source = argFile ? path.resolve(argFile) : makeSample(dataDir);
if (!fs.existsSync(source)) {
  console.error(`✘ 源文件不存在：${source}`);
  process.exit(2);
}

const reportPath = path.join(dataDir, "smoke-report.json");
const logPath = path.join(dataDir, "logs", "app.log");

console.log(`端到端验收 · ${EXE}`);
console.log(`源文件：${source}（${(fs.statSync(source).size / 1024).toFixed(1)} KB）`);
console.log(`临时数据目录：${dataDir}`);
console.log("（窗口会短暂出现，属正常）\n");

const env = {
  ...process.env,
  DESKBASE_DATA_DIR: dataDir,
  DESKBASE_UI_SMOKE: PAGE, // 复用同一条"注入页面脚本"的通道
  DESKBASE_E2E_SOURCE: source,
};
delete env.DESKBASE_RENDER;
delete env.DESKBASE_SMOKE_ARGS; // 别把上一次实验的参数带进来

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
    const lines = tail(logPath, 80);
    if (lines.length) {
      console.error(`\n--- app.log 尾部（最后 ${lines.length} 行）---`);
      lines.forEach((l) => console.error("  " + l));
    } else {
      console.error("\n（读不到 app.log —— 应用可能根本没起来）");
    }
    const errLines = tail(errPath, 30);
    if (errLines.length) {
      console.error(`\n--- stderr 尾部 ---`);
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
  if (KEEP) {
    console.log(`\n临时目录（--keep）：${dataDir}`);
  } else {
    try {
      fs.rmSync(dataDir, { recursive: true, force: true });
    } catch (_) {}
  }
  process.exit(0);
}
