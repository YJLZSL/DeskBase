#!/usr/bin/env node
/* ============================================================
   端到端验收用的表格夹具生成器

   为什么要它：tests/e2e-import.cjs 一直没跑，原因写的是"缺真实表格夹具"
   （testdata/ 里只有边界脏数据语料，几行一个，不能代表真实业务表）。
   这个脚本造一张**业务味**的表：中文表头、金额、日期、前导零工号、
   空值、超长备注 —— 都是真实台账会有的东西。

   约束：不依赖任何第三方包，仅用 Node 内置模块；可重复运行（同参数同结果）。
   用法：node tools/make-e2e-fixture.cjs [--rows 1200] [--out <文件>]
   ============================================================ */
const fs = require('fs');
const path = require('path');

function getArg(name, def) {
  const i = process.argv.indexOf(name);
  return i >= 0 && process.argv[i + 1] ? process.argv[i + 1] : def;
}

const ROWS = Number(getArg('--rows', 1200));
const OUT = getArg(
  '--out',
  path.join(__dirname, '..', 'testdata', 'e2e-expense-fixture.csv')
);

// 固定种子，保证每次生成同一份数据（测试要可重复）
let seed = 20260923;
function rnd() {
  seed = (seed * 1103515245 + 12345) & 0x7fffffff;
  return seed / 0x7fffffff;
}
const pick = (arr) => arr[Math.floor(rnd() * arr.length)];

const 项目 = [
  '差旅费', '办公用品', '招待费', '快递费', '培训费', '维修费', '通讯费', '交通费',
];
const 部门 = ['销售部', '技术部', '市场部', '行政部', '财务部'];
const 城市 = ['上海', '北京', '深圳', '杭州', '成都', '西安', '武汉'];

const lines = [];
// BOM：让编码探测走"UTF-8 with BOM"这条路，同时也顺带验一次探测
lines.push('工号,日期,项目,金额,部门,是否已结清,备注');

for (let i = 1; i <= ROWS; i++) {
  const 工号 = String(100000 + i).padStart(6, '0'); // 前导零：不能被当成数字
  const m = String(((i - 1) % 12) + 1).padStart(2, '0');
  const d = String(((i * 7) % 28) + 1).padStart(2, '0');
  const 日期 = `2026-${m}-${d}`;
  const 金额 = ((i * 137) % 500000 + 100) / 100; // 两位小数
  const 已结清 = i % 3 === 0 ? '是' : '否';
  // 每 50 行留一个空备注 —— 用来验 NULL 与空串的区别
  const 备注 = i % 50 === 0 ? '' : `${pick(城市)}${pick(项目)}报销（第${i}笔）`;
  lines.push(
    [
      工号,
      日期,
      pick(项目),
      金额.toFixed(2),
      pick(部门),
      已结清,
      备注.includes(',') ? `"${备注}"` : 备注,
    ].join(',')
  );
}

fs.mkdirSync(path.dirname(OUT), { recursive: true });
// EF BB BF = UTF-8 BOM
fs.writeFileSync(OUT, '﻿' + lines.join('\r\n') + '\r\n', 'utf8');

const stat = fs.statSync(OUT);
console.log('端到端夹具已生成');
console.log('  文件 : ' + OUT);
console.log('  行数 : ' + ROWS + ' 行数据（+1 行表头）');
console.log('  大小 : ' + (stat.size / 1024).toFixed(1) + ' KB');
console.log('  字段 : 工号(前导零) / 日期 / 项目 / 金额 / 部门 / 是否已结清 / 备注(含空值)');
