#!/usr/bin/env node
/* ============================================================
   DeskBase 崩溃恢复端到端（crash-recovery.cjs）
   ============================================================
   验证的是三条底线里「不丢数据」的那句话：
   **崩溃必进恢复向导，且已提交的数据一列不少。**

   三个阶段（每个阶段都是一次真实启动，共用同一个数据目录）：
     1) writer：真实 exe + 持续写入 → 等日志出现进度 → **强杀**
        （TerminateProcess，模拟任务管理器结束进程 / 断电）；
     2) verify：同一数据目录重启 → 页面脚本断言「报了未清理 + 自检通过 +
        向导出现 + 数据活着 + 快照能出文件」，然后正常退出；
     3) clean：再启动一次 → 断言「不再报未清理 + 向导不再出现」
        （证明退出路径确实清了标记）。

   为什么三个阶段缺一不可：只做 1+2 证明"能进向导"；没有 3 就可能做出一个
   **每次启动都弹向导**的东西 —— 用户会学会无视它，那比没有还糟。

   用法：
     node tests/crash-recovery.cjs [<exe 路径>]
   退出码：0 = 全过；1 = 有失败；2 = 跑不起来（缺产物 / 超时）
   ============================================================ */

const fs = require("fs");
const os = require("os");
const path = require("path");
const { spawn, spawnSync } = require("child_process");

const ROOT = path.resolve(__dirname, "..");
const argExe = process.argv[2] && !process.argv[2].startsWith("--") ? process.argv[2] : null;
const EXE = argExe ? path.resolve(argExe) : path.join(ROOT, "app", "target", "release", "deskbase.exe");
const WRITE_PAGE = path.join(__dirname, "crash-recovery.write.page.js");
const VERIFY_PAGE = path.join(__dirname, "crash-recovery.verify.page.js");
const CLEAN_PAGE = path.join(__dirname, "crash-recovery.clean.page.js");

for (const f of [EXE, WRITE_PAGE, VERIFY_PAGE, CLEAN_PAGE]) {
  if (!fs.existsSync(f)) {
    console.error(`✘ 找不到：${f}${f === EXE ? "\n  先跑 node scripts/build.cjs 生成产物。" : ""}`);
    process.exit(2);
  }
}

const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "deskbase-crash-"));
const logPath = path.join(dataDir, "logs", "app.log");
const stderrPath = path.join(dataDir, "stderr.log");
const reportPath = path.join(dataDir, "smoke-report.json");

console.log(`崩溃恢复端到端 · ${EXE}`);
console.log(`临时数据目录：${dataDir}`);
console.log("（窗口会短暂出现多次，属正常）\n");

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const readLog = () => {
  try { return fs.readFileSync(logPath, "utf8"); } catch (_) { return ""; }
};
const readReport = () => {
  try { return JSON.parse(fs.readFileSync(reportPath, "utf8")); } catch (_) { return null; }
};
const lastWritten = () => {
  const all = [...readLog().matchAll(/crashtest-writer: 已写入 (\d+)/g)];
  return all.length ? Number(all[all.length - 1][1]) : 0;
};
const tail = (file, n) => {
  try { return fs.readFileSync(file, "utf8").trim().split("\n").slice(-n); } catch (_) { return []; }
};

function launch(pagePath) {
  const env = { ...process.env, DESKBASE_DATA_DIR: dataDir, DESKBASE_UI_SMOKE: pagePath };
  delete env.DESKBASE_RENDER; // 别把构建期钩子带进来
  const fd = fs.openSync(stderrPath, "a");
  return spawn(EXE, [], { env, stdio: ["ignore", "ignore", fd], windowsHide: false });
}
function waitExit(child, ms) {
  return new Promise((res) => {
    if (child.exitCode !== null) return res(true);
    const t = setTimeout(() => res(false), ms);
    child.once("exit", () => { clearTimeout(t); res(true); });
  });
}
async function waitReport(ms) {
  const t0 = Date.now();
  for (;;) {
    const r = readReport();
    if (r && Array.isArray(r.steps)) return r;
    if (Date.now() - t0 > ms) return null;
    await sleep(250);
  }
}

function printSteps(rep) {
  for (const s of rep.steps || []) {
    console.log(`${s.ok ? "✔" : "✖"} ${s.name}${s.detail ? `　（${s.detail}）` : ""}`);
  }
}

(async () => {
  const failures = [];

  // 清掉可能开着的旧实例（手动开的、上次测试残留的）
  spawnSync("taskkill", ["/IM", "deskbase.exe", "/F"], { windowsHide: true });
  await sleep(700);

  // ---------- 阶段 1：写入 → 强杀 ----------
  console.log("阶段 1/3 · 持续写入，写到一半强杀");
  const w = launch(WRITE_PAGE);
  {
    const t0 = Date.now();
    while (lastWritten() < 150) {
      if (Date.now() - t0 > 60000) {
        console.error("✘ 等待写入进度超时。app.log 尾部：");
        tail(logPath, 20).forEach((l) => console.error("  " + l));
        tail(stderrPath, 10).forEach((l) => console.error("  stderr: " + l));
        console.error(`临时目录保留供排查：${dataDir}`);
        process.exit(2);
      }
      await sleep(200);
    }
  }
  const committedFloor = lastWritten(); // 已提交下界（播报在提交之后）
  console.log(`  已提交下界 = ${committedFloor} 行；执行强杀（不经任何收尾）`);
  w.kill();
  if (!(await waitExit(w, 8000))) {
    spawnSync("taskkill", ["/F", "/PID", String(w.pid)], { windowsHide: true });
    await waitExit(w, 5000);
  }
  await sleep(700);

  // ---------- 阶段 2：重启 → 验尸 ----------
  console.log("\n阶段 2/3 · 同一数据目录重启，恢复向导应该出现");
  try { fs.unlinkSync(reportPath); } catch (_) {}
  const v = launch(VERIFY_PAGE);
  const rep2 = await waitReport(90000);
  if (!rep2) {
    console.error("✘ 阶段 2 没有写出报告（进程活着但零产出）。app.log 尾部：");
    tail(logPath, 25).forEach((l) => console.error("  " + l));
    console.error(`临时目录保留供排查：${dataDir}`);
    process.exit(2);
  }
  await waitExit(v, 15000);
  printSteps(rep2);
  if (rep2.failures && rep2.failures.length) {
    rep2.failures.forEach((f) => failures.push("阶段2 · " + f));
  }
  // 驱动侧的额外断言：
  if (!/检测到上次未正常退出/.test(readLog())) {
    failures.push("阶段2 · app.log 里没有「检测到上次未正常退出」这行（后端没记这次崩溃）");
  }
  const d = rep2.data || {};
  if (!(Number(d.max) >= committedFloor)) {
    failures.push(`阶段2 · 数据少于此前的已提交下界：max=${d.max} < ${committedFloor}`);
  }
  console.log(`  已提交下界 ${committedFloor} → 重启后读到 max=${d.max}` + (d.max >= committedFloor ? "（△ 不丢）" : "（✘ 丢了）"));
  try { fs.copyFileSync(reportPath, path.join(dataDir, "report-phase2.json")); } catch (_) {}

  // ---------- 阶段 3：正常退出后再启动 → 复检 ----------
  console.log("\n阶段 3/3 · 正常退出之后再启动，不应再报");
  try { fs.unlinkSync(reportPath); } catch (_) {}
  const c = launch(CLEAN_PAGE);
  const rep3 = await waitReport(60000);
  if (!rep3) {
    console.error("✘ 阶段 3 没有写出报告。app.log 尾部：");
    tail(logPath, 25).forEach((l) => console.error("  " + l));
    console.error(`临时目录保留供排查：${dataDir}`);
    process.exit(2);
  }
  await waitExit(c, 15000);
  printSteps(rep3);
  if (rep3.failures && rep3.failures.length) {
    rep3.failures.forEach((f) => failures.push("阶段3 · " + f));
  }

  // ---------- 汇总 ----------
  const total = (rep2.steps || []).length + (rep3.steps || []).length;
  console.log("");
  if (failures.length) {
    console.error(`✘ 失败 ${failures.length} 项（共 ${total} 步）：`);
    failures.forEach((f) => console.error("  - " + f));
    console.log("\n--- app.log 尾部 ---");
    tail(logPath, 15).forEach((l) => console.log("  " + l));
    console.log(`\n临时目录保留供排查：${dataDir}`);
    process.exit(1);
  }
  console.log(`✔ 全过（${total} 步）—— 崩溃必进恢复向导，已提交数据无丢失，正常退出不再误报`);
  try { fs.rmSync(dataDir, { recursive: true, force: true }); } catch (_) {}
  process.exit(0);
})();
