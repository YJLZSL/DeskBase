#!/usr/bin/env node
// ============================================================
// 检查 exe 里有没有真正的图标资源（PE 的 RT_ICON / RT_GROUP_ICON）
// ============================================================
// 为什么要单独有这个检查：
//
// v1.11.1 排查"快捷方式没图标"时挖出的根因是 —— **exe 里根本没有图标资源**。
// 窗口和任务栏上看到的图标是 `tao::window::Icon::from_rgba()` 运行时设的，
// 它只进内存、不进 PE 资源段。所以**光看程序能跑、窗口有图标，是不够的** ——
// 资源管理器里看这个 exe 文件还是白纸一张。
//
// 而这类问题**没有任何运行时症状**：程序照跑、窗口照有图标，
// 只有"去看那个 exe 文件"才看得出来。所以必须有一个**能断言**的检查，
// 否则下次谁把 build.rs 删了或改坏了，没人会发现。
//
// 用法：
//   node scripts/check-icon.cjs                       # 查默认的 release exe
//   node scripts/check-icon.cjs <exe 路径>
// ============================================================

const fs = require('fs');
const path = require('path');

const exe = process.argv[2] || path.join(__dirname, '..', 'app', 'target', 'release', 'deskbase.exe');

if (!fs.existsSync(exe)) {
  console.error('✖ 找不到 exe：' + exe);
  console.error('  先构建：node scripts/build.cjs');
  process.exit(1);
}

const buf = fs.readFileSync(exe);

/** 读 PE 头，拿到资源段（.rsrc）的位置 */
function findRsrcSection(b) {
  if (b.readUInt16LE(0) !== 0x5a4d) return null; // 'MZ'
  const peOff = b.readUInt32LE(0x3c);
  if (b.readUInt32LE(peOff) !== 0x00004550) return null; // 'PE\0\0'
  const numSections = b.readUInt16LE(peOff + 6);
  const optSize = b.readUInt16LE(peOff + 20);
  const secStart = peOff + 24 + optSize;
  for (let i = 0; i < numSections; i++) {
    const off = secStart + i * 40;
    const name = b.toString('ascii', off, off + 8).replace(/\0+$/, '');
    if (name === '.rsrc') {
      return {
        va: b.readUInt32LE(off + 12),
        size: b.readUInt32LE(off + 16),
        raw: b.readUInt32LE(off + 20),
      };
    }
  }
  return null;
}

const rsrc = findRsrcSection(buf);
if (!rsrc) {
  console.error('✖ 这个 exe 里没有 .rsrc 段 —— 说明没有任何资源（图标也一定没有）');
  console.error('  检查 app/build.rs 是否存在、有没有被跳过。');
  process.exit(1);
}

/** 走资源目录的第一层，列出有哪些类型（RT_ICON=3, RT_GROUP_ICON=14） */
function resourceTypes(b, rsrc) {
  const base = rsrc.raw;
  const named = b.readUInt16LE(base + 12);
  const idCount = b.readUInt16LE(base + 14);
  const types = [];
  const total = named + idCount;
  for (let i = 0; i < total; i++) {
    const e = base + 16 + i * 8;
    const id = b.readUInt32LE(e);
    types.push(id & 0x80000000 ? 'named:' + i : id);
  }
  return types;
}

const types = resourceTypes(buf, rsrc);
const hasIcon = types.includes(3); // RT_ICON
const hasGroup = types.includes(14); // RT_GROUP_ICON

console.log('exe：' + path.relative(process.cwd(), exe).replace(/\\/g, '/'));
console.log('  大小：' + (buf.length / 1024 / 1024).toFixed(2) + ' MB');
console.log('  .rsrc 段：' + (rsrc.size / 1024).toFixed(1) + ' KB');
console.log('  资源类型：' + types.join(', '));
console.log('');

let ok = true;
if (hasGroup) console.log('  ✔ RT_GROUP_ICON（图标组）在');
else { console.log('  ✖ 没有 RT_GROUP_ICON —— exe 不会显示图标'); ok = false; }

if (hasIcon) console.log('  ✔ RT_ICON（图标本体）在');
else { console.log('  ✖ 没有 RT_ICON'); ok = false; }

if (ok) {
  console.log('\n✔ 通过：exe 带真正的图标资源（资源管理器 / 快捷方式 / 安装器都能读到）');
  process.exit(0);
} else {
  console.log('\n✖ 失败：exe 没有图标资源。');
  console.log('  这类问题**没有运行时症状** —— 程序照跑、窗口照有图标，');
  console.log('  只有去看那个 exe 文件才看得出来。');
  console.log('  检查：app/build.rs 是否在、rc.exe 是否找得到（构建时会打 warning）。');
  process.exit(1);
}
