#!/usr/bin/env node
/* ============================================================
   DeskBase 导入压力测试（tests/stress-import.cjs）
   ============================================================
   干什么：拿**真实的大文件**压导入链路，测出可复现的数字。

   为什么必须有它：功能测试只能证明"小文件能导"。而这个软件要承接的是
   用户那堆**几万到几十万行**的台账与流水 —— 那里才会暴露真问题：
     · 内存峰值（一次性读进来会不会把机器压垮）
     · 分批写入是否真的分批（还是一次性提交）
     · 进度是否真的在推进（还是几十秒不动然后突然完成）
     · 大表建好之后还能不能翻页、还能不能查

   规模档位（默认跑 [1万, 10万]，可用 --sizes 指定）：
     1 万行   —— 常见台账的量级，应当"秒级"
     10 万行  —— 大流水的量级，应当"十秒级"
     20 万行  —— 单次导入上限（MAX_IMPORT_ROWS），边界

   用法：
     node tests/stress-import.cjs                     # 默认 1万 + 10万
     node tests/stress-import.cjs --sizes 1e4,2e5      # 指定规模
     node tests/stress-import.cjs --keep               # 保留临时目录

   退出码：0 = 全过；1 = 有失败；2 = 跑不起来

   ⚠️ 数据由脚本现造（不依赖网络、不依赖仓库里的大文件）。造出来的 CSV
   是真实的形态：中文列名、金额两位小数、11 位电话、前导零工号、日期。
   ============================================================ */

const fs = require("fs");
const os = require("os");
const path = require("path");
const { spawn } = require("child_process");

const ROOT = path.resolve(__dirname, "..");
const EXE = path.join(ROOT, "app", "target", "release", "deskbase.exe");
const PAGE = path.join(__dirname, "stress-import.page.js");
const KEEP = process.argv.includes("--keep");
const TIMEOUT_MS = 900000; // 15 分钟：20 万行要留足余量

const sizesArg = process.argv.indexOf("--sizes");
const SIZES =
  sizesArg >= 0 && process.argv[sizesArg + 1]
    ? process.argv[sizesArg + 1].split(",").map((s) => Number(s.trim())).filter((n) => n > 0)
    : [10000, 100000];

if (!fs.existsSync(EXE)) {
  console.error(`✘ 找不到产物：${EXE}\n  先跑 node scripts/build.cjs 再试。`);
  process.exit(2);
}
if (!fs.existsSync(PAGE)) {
  console.error(`✘ 找不到页面脚本：${PAGE}`);
  process.exit(2);
}

/**
 * 造一份指定行数的真实形态 CSV。
 *
 * 刻意包含这几类"最容易在规模上来之后炸掉"的数据：
 *   · 中文列名与中文内容（UTF-8 多字节，行长度不均匀）
 *   · 金额两位小数（要过 money_parse，不是直接塞数字）
 *   · 11 位电话号码（必须当文本，不能走数字解析）
 *   · 前导零工号（同理）
 *   · 日期（要过日期规范化）
 * 纯数字的均匀表测不出这些问题 —— 它太"顺"了。
 */
function makeCsv(file, rows) {
    const out = fs.createWriteStream(file, { encoding: "utf8" });
    // error 只在流上挂 **一次**。写成"每次 write 都挂一个"的话，20 万行会挂出
    // 上百个监听器，Node 直接刷 MaxListenersExceededWarning（"Possible EventEmitter
    // memory leak"）—— 而它其实只是写得太快、还没 drain。挂一次就够了。
    let streamErr = null;
    out.on("error", (e) => {
      streamErr = e;
    });
    const write = (s) =>
      new Promise((res, rej) => {
        if (streamErr) return rej(streamErr);
        if (out.write(s)) res();
        else out.once("drain", res);
      });

    return (async () => {
      await write("客户台账\n导出日期：2026-09-19\n");
      await write("客户名称,联系电话,工号,金额,是否结清,签单日期,备注\n");
      const names = ["北京甲贸易", "上海乙实业", "广州丙科技", "成都丁商贸", "杭州戊电子"];
      const CHUNK = 5000;
      let buf = [];
      for (let i = 1; i <= rows; i++) {
        buf.push(
          [
            names[i % 5] + i,
            "138" + String(10000000 + i),
            "00" + (1000 + (i % 900000)),
            (i * 13.37).toFixed(2),
            i % 2 ? "是" : "否",
            "2026-0" + (1 + (i % 9)) + "-1" + (i % 9),
            i % 7 === 0 ? "" : "第" + i + "笔", // 故意留空值：必填判断与空值处理都要过一遍
          ].join(",") + "\n"
        );
        if (buf.length >= CHUNK) {
          await write(buf.join(""));
          buf = [];
        }
      }
      if (buf.length) await write(buf.join(""));
      await new Promise((res) => out.end(res));
    })();
}

const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), "deskbase-stress-"));
const reportPath = path.join(dataDir, "smoke-report.json");
const logPath = path.join(dataDir, "logs", "app.log");

console.log(`导入压力测试 · ${EXE}`);
console.log(`规模：${SIZES.map((n) => n.toLocaleString("zh-CN") + " 行").join(" · ")}`);
console.log(`临时数据目录：${dataDir}`);
console.log("（窗口会短暂出现，属正常）\n");

/** 一次跑一个规模：造数据 → 起应用 → 跑完退出 → 读报告 */
async function runOnce(rows) {
  const csv = path.join(dataDir, `客户台账-${rows}.csv`);
  process.stdout.write(`造数据（${rows.toLocaleString("zh-CN")} 行）… `);
  const t0 = Date.now();
  await makeCsv(csv, rows);
  const size = fs.statSync(csv).size;
  console.log(
    `${(size / 1048576).toFixed(1)} MB，用时 ${((Date.now() - t0) / 1000).toFixed(1)}s`
  );

  // 每个规模用独立的数据目录：上一轮的表不能影响下一轮（重名保护会挡住）
  const dir = fs.mkdtempSync(path.join(dataDir, `db-${rows}-`));
  const rpt = path.join(dir, "smoke-report.json");
  if (fs.existsSync(reportPath)) fs.unlinkSync(reportPath);

  const env = {
    ...process.env,
    DESKBASE_DATA_DIR: dir,
    DESKBASE_UI_SMOKE: PAGE,
    DESKBASE_E2E_SOURCE: csv,
    DESKBASE_E2E_ROWS: String(rows), // 期望行数（独立于后端）
  };
  delete env.DESKBASE_RENDER;
  delete env.DESKBASE_SMOKE_ARGS;

  return new Promise((resolve) => {
    const errFd = fs.openSync(path.join(dir, "stderr.log"), "w");
    const child = spawn(EXE, [], { env, stdio: ["ignore", "ignore", errFd], windowsHide: false });
    let killed = false;
    const timer = setTimeout(() => {
      killed = true;
      try {
        child.kill();
      } catch (_) {}
    }, TIMEOUT_MS);
    child.on("exit", () => {
      clearTimeout(timer);
      setTimeout(() => {
        let report = null;
        try {
          report = JSON.parse(fs.readFileSync(path.join(dir, "smoke-report.json"), "utf8"));
        } catch (_) {}
        resolve({ rows, report, dir, csv, size, killed });
      }, 200);
    });
  });
}

function showReport(r) {
  const steps = (r.report && r.report.steps) || [];
  const fails = (r.report && r.report.failures) || [];
  for (const s of steps) {
    console.log(`  ${s.ok ? "✔" : "✖"} ${s.name}${s.detail ? `　（${s.detail}）` : ""}`);
  }
  return { pass: steps.filter((s) => s.ok).length, total: steps.length, fails };
}

(async () => {
  const results = [];
  for (const rows of SIZES) {
    console.log(`\n${"=".repeat(64)}`);
    console.log(`规模 ${rows.toLocaleString("zh-CN")} 行`);
    console.log("=".repeat(64));
    const r = await runOnce(rows);
    if (!r.report) {
      console.log(`  ✘ 没有报告${r.killed ? "（超时）" : ""} —— 见 ${r.dir}`);
      const tail = (() => {
        try {
          return fs
            .readFileSync(path.join(r.dir, "logs", "app.log"), "utf8")
            .trim()
            .split("\n")
            .slice(-15);
        } catch (_) {
          return [];
        }
      })();
      tail.forEach((l) => console.log("    " + l));
      results.push({ rows, ok: false, note: "无报告" });
      continue;
    }
    const { pass, total, fails } = showReport(r);
    console.log(`  → 通过 ${pass} / ${total}${fails.length ? `，失败 ${fails.length}` : ""}`);
    fails.forEach((f) => console.log(`     ✖ ${f}`));
    results.push({ rows, ok: fails.length === 0, note: `${pass}/${total}` });
  }

  console.log(`\n${"=".repeat(64)}`);
  console.log("汇总");
  console.log("=".repeat(64));
  for (const r of results) {
    console.log(
      `${r.ok ? "✔" : "✘"} ${r.rows.toLocaleString("zh-CN").padStart(9)} 行　${r.note}`
    );
  }
  const bad = results.filter((r) => !r.ok);
  if (bad.length) {
    console.log(`\n临时目录保留供排查：${dataDir}`);
    process.exit(1);
  }
  if (KEEP) console.log(`\n临时目录（--keep）：${dataDir}`);
  else {
    try {
      fs.rmSync(dataDir, { recursive: true, force: true });
    } catch (_) {}
  }
  process.exit(0);
})();
