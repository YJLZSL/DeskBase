const fs = require('fs');
function need(F, from, to, label) {
  let s = fs.readFileSync(F, 'utf8');
  if (!s.includes(from)) { console.error('!! 未命中[' + F + '] ' + label); process.exit(1); }
  fs.writeFileSync(F, s.split(from).join(to), 'utf8');
  const again = fs.readFileSync(F, 'utf8');
  console.log('ok ' + label + '（' + (again.includes(to) ? '已落盘' : '未落盘！') + '）');
}

// ---------- index.html：一级导航改名 ----------
// 去掉 SQL 之后「数据库」这个词是误导（界面上已经没有数据库了，只有表格）；
// 「工作台」太含糊，里面其实放的是各类办公工具。
need(
  'app/ui/index.html',
  `<button class="nav-item" data-target="workbench" data-label="工作台"><svg class="ico"><use href="#i-bench"/></svg><span>工作台</span></button>`,
  `<button class="nav-item" data-target="workbench" data-label="工具"><svg class="ico"><use href="#i-bench"/></svg><span>工具</span></button>`,
  '导航 工作台→工具'
);
need(
  'app/ui/index.html',
  `<button class="nav-item" data-target="database" data-label="数据库"><svg class="ico"><use href="#i-db"/></svg><span>数据库</span></button>`,
  `<button class="nav-item" data-target="database" data-label="表格"><svg class="ico"><use href="#i-db"/></svg><span>表格</span></button>`,
  '导航 数据库→表格'
);
need(
  'app/ui/index.html',
  `      <!-- ---------- 工作台 ---------- -->`,
  `      <!-- ---------- 工具（原「工作台」；2026-09-22 按 ADR-0023 改名） ---------- -->`,
  '工具区注释'
);
need(
  'app/ui/index.html',
  `      <!-- ---------- 数据库 ---------- -->`,
  `      <!-- ---------- 表格（原「数据库」；去掉 SQL 后这个名字是误导，2026-09-22 改） ---------- -->`,
  '表格区注释'
);

// ---------- app.js：顶栏标题映射 ----------
need('app/ui/app.js', `    database: "数据库",`, `    database: "表格",`, '顶栏标题 表格');
need('app/ui/app.js', `    workbench: "工作台",`, `    workbench: "工具",`, '顶栏标题 工具');

// ---------- 引导里的教程名称 ----------
need(
  'app/ui/db-onboard.js',
  `"看不懂？读设置页里的「数据库使用教程」。",`,
  `"看不懂？读设置页底部的「办公套件使用教程」。",`,
  '引导里的教程名'
);

// ---------- 备份对话框里"数据库"的说法 ----------
{
  const F = 'app/ui/db.js';
  let s = fs.readFileSync(F, 'utf8');
  const pairs = [
    ['"备份数据库"', '"备份数据"'],
    ['"备份是当前数据的一份完整快照', '"备份是当前数据的一份完整快照'],
    ['dlg.append(el("h3", null, "备份数据库"));', 'dlg.append(el("h3", null, "备份数据"));'],
  ];
  let n = 0;
  for (const [a, b] of pairs) {
    if (s.includes(a) && a !== b) { s = s.split(a).join(b); n++; }
  }
  fs.writeFileSync(F, s, 'utf8');
  console.log('ok db.js 备份文案（改了 ' + n + ' 处）');
}

// index.html 的备份按钮文案
need(
  'app/ui/index.html',
  `            <button class="btn btn-ghost db-import" id="btn-db-backup" type="button"
                    title="把当前数据库整份备份到数据目录的 backups/ 并做完整性校验">
              备份数据库
            </button>`,
  `            <button class="btn btn-ghost db-import" id="btn-db-backup" type="button"
                    title="把当前数据整份备份到数据目录的 backups/ 并做完整性校验">
              备份数据
            </button>`,
  '备份按钮文案'
);

console.log('done');
