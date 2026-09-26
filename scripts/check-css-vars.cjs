// 扫描 app/ui/*.css 里**用到了但没定义**的 CSS 变量。
//
// 为什么值得单独一个脚本：CSS 自定义属性写错了**不会报错** —— 浏览器退化成
// "该声明无效"，元素默默拿了继承值或初值。结果就是"看着不太对但说不上哪里不对"。
// 真踩到过：`theme.css` 里 `.settings-nav-item` 用了 `var(--fs-sm)`，而
// `--fs-sm` 从来没定义过 —— 那批胶囊一直用的是浏览器默认字号。
// 门禁（check-motion / check-contrast）都不查这个，所以单独扫一遍。
//
// 用法：node local-docs/tools/check-css-vars.cjs
const fs = require("fs");
const path = require("path");

const UI = path.resolve(__dirname, "..", "app", "ui");
const files = fs.readdirSync(UI).filter((f) => f.endsWith(".css"));

const defined = new Set();
const used = []; // { name, file, line }

for (const f of files) {
  const text = fs.readFileSync(path.join(UI, f), "utf8");
  text.split("\n").forEach((line, i) => {
    // 定义：--x: 值
    for (const m of line.matchAll(/(--[a-z0-9-]+)\s*:/gi)) defined.add(m[1]);
    // 使用：var(--x) 或 var(--x, fallback)
    for (const m of line.matchAll(/var\(\s*(--[a-z0-9-]+)\s*[,)]/gi)) {
      used.push({ name: m[1], file: f, line: i + 1 });
    }
  });
}

// 也要把 JS 里 setProperty 的变量算作"定义"，否则会误报
for (const f of fs.readdirSync(UI).filter((x) => x.endsWith(".js"))) {
  const text = fs.readFileSync(path.join(UI, f), "utf8");
  for (const m of text.matchAll(/setProperty\(\s*["'](--[a-z0-9-]+)["']/gi)) defined.add(m[1]);
  for (const m of text.matchAll(/["'](--[a-z0-9-]+)["']\s*:/gi)) defined.add(m[1]);
}

const missing = used.filter((u) => !defined.has(u.name));
const byName = new Map();
for (const m of missing) {
  if (!byName.has(m.name)) byName.set(m.name, []);
  byName.get(m.name).push(`${m.file}:${m.line}`);
}

console.log(`扫描 ${files.length} 份样式表 · 已定义变量 ${defined.size} 个 · var() 引用 ${used.length} 处`);
if (!byName.size) {
  console.log("\n✔ 通过（没有引用未定义的 CSS 变量）");
  process.exit(0);
}
console.log(`\n⚠ ${byName.size} 个变量被引用但从未定义（会静默退化成无效声明）：`);
for (const [name, where] of [...byName.entries()].sort()) {
  const uniq = [...new Set(where)];
  console.log(`  ${name}  ← ${uniq.length} 处：${uniq.slice(0, 6).join(", ")}${uniq.length > 6 ? " …" : ""}`);
}
process.exitCode = 1;
