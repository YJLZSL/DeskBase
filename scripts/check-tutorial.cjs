// 检查教程（app/ui/help.js）与实现是否还对得上。
//
// ## 为什么值得一个脚本
// `db.js` 里钉着一条注释："改模板/控件必须同步改教程"。但那条约束**只靠人记** ——
// 而 v1.10/v1.11 连着加了 AI 对话、打印视图、安装版快捷方式、三栏分组侧栏，
// 教程**一个字都没跟上**（还停在九节）。这类"文档与实现脱节"在项目里
// 已经发生过好几次，靠自觉是靠不住的。
//
// 这个脚本做两件机器能做的事：
//   1. **结构对账**：教程用 `「」` 引起来的界面名，必须在 index.html 里真的找得到
//      （找不到 = 教程在教用户点一个不存在的按钮）；
//   2. **覆盖提醒**：把"界面上有、但教程从没提过"的主要功能列出来，
//      让作者自己判断要不要补 —— **不自动判定失败**，因为不是每个功能都值得写进教程。
//
// 用法：node local-docs/tools/check-tutorial.cjs
const fs = require("fs");
const path = require("path");

const ROOT = path.resolve(__dirname, "..");
const helpPath = path.join(ROOT, "app", "ui", "help.js");
const indexPath = path.join(ROOT, "app", "ui", "index.html");
const uiDir = path.join(ROOT, "app", "ui");
const help = fs.readFileSync(helpPath, "utf8");
const html = fs.readFileSync(indexPath, "utf8");

// ⚠️ 判据**不能只看 index.html**：这个项目里大量界面文字是**运行时由 JS 建的**
// （网格的「加载更多」/「设为 NULL」/「删除所选」在 grid.js，AI 面板的授权勾选项在
// ui-chat.js，命令面板的条目在 palette.js……）。
// 第一版只比 index.html，于是 9 条"教程引用的界面名不存在"里**全是误报** ——
// 一条会把正确的东西报成错的检查，比没有检查更坏：下次真出问题时没人会信它。
const uiSources = fs
  .readdirSync(uiDir)
  .filter((f) => /\.(js|html|css)$/.test(f))
  .map((f) => ({ file: f, text: fs.readFileSync(path.join(uiDir, f), "utf8") }));
const inUi = (name) => uiSources.some((s) => s.text.includes(name));

// ---------- 1) 结构对账 ----------
const quoted = [...new Set([...help.matchAll(/「([^」]{2,16})」/g)].map((m) => m[1]))];
const missing = quoted.filter((n) => !inUi(n));

const sections = [...help.matchAll(/help-h">([^<]+)</g)].map((m) => m[1]);

console.log(`教程：${sections.length} 节`);
for (const [i, s] of sections.entries()) console.log(`  ${i + 1}. ${s}`);
console.log(`\n教程引用的界面名 ${quoted.length} 个，其中整个 app/ui 里都找不到的：${missing.length}`);
if (missing.length) {
  console.log("  ⚠ 下面这些名字在界面上不存在 —— 教程在教用户点一个找不到的东西：");
  for (const m of missing) console.log(`     · 「${m}」`);
}

// ---------- 2) 覆盖提醒（信息性，不算失败） ----------
// 每一项 = [功能名, index.html 里用来判定"这个功能存在"的标记, 教程里该出现的关键词]
const FEATURES = [
  ["AI 对话", 'id="ai-enabled"', ["AI"]],
  ["打印视图", 'id="btn-db-print"', ["打印"]],
  ["单表导出 Excel", 'id="btn-db-export"', ["导出当前表格", "导出"]],
  ["全库导出", 'id="btn-export-all"', ["导出全部数据"]],
  ["备份", 'id="btn-db-backup"', ["备份"]],
  ["安装到本机", 'id="btn-install"', ["安装到本机"]],
  ["表结构编辑", 'id="btn-db-schema"', ["表结构", "加列", "删列"]],
  ["关系与同步", 'id="btn-db-relations"', ["关系与同步"]],
  ["视图", 'id="btn-db-views"', ["视图"]],
  ["历史（回退）", 'id="btn-db-history"', ["历史"]],
  ["从 Excel 导入", 'id="btn-db-import"', ["导入"]],
];

const uncovered = [];
for (const [name, marker, keywords] of FEATURES) {
  if (!html.includes(marker)) continue; // 功能不在界面上，跳过
  const touched = keywords.some((k) => help.includes(k));
  if (!touched) uncovered.push(name);
}

console.log(`\n界面上有 ${FEATURES.length} 个主要入口，教程完全没提的：${uncovered.length}`);
for (const u of uncovered) console.log(`  · ${u}`);

if (missing.length) {
  console.log(`\n✘ 有 ${missing.length} 个教程引用的界面名不存在`);
  process.exitCode = 1;
} else {
  console.log(`\n✔ 教程引用的界面名都能在界面上找到`);
}
if (uncovered.length) {
  console.log(`（上面 ${uncovered.length} 项是**提醒**不是失败：不是每个功能都值得写进教程，` +
    `但"加了功能忘了改教程"这件事该被看见。）`);
}
