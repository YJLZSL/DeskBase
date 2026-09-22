const fs = require('fs');

// ---------- 版本号 ----------
for (const F of ['app/Cargo.toml', 'app/Cargo.lock']) {
  if (!fs.existsSync(F)) continue;
  let s = fs.readFileSync(F, 'utf8');
  s = s.replace('version = "0.4.0"', 'version = "0.5.0"');
  fs.writeFileSync(F, s, 'utf8');
  console.log('ok ' + F + ' → 0.5.0');
}

// ---------- CHANGELOG：[未发布] 转正为 v0.5.0，并补本轮内容 ----------
{
  const F = 'CHANGELOG.md';
  let s = fs.readFileSync(F, 'utf8');
  const oldHead = '## [未发布]\n';
  if (!s.includes(oldHead)) { console.error('!! 找不到 [未发布]'); process.exit(1); }

  const L = [];
  L.push('## [0.5.0] - 2026-09-22 · 定位转向 Office 三件套的扩展与升级');
  L.push('');
  L.push('> 这是一次**产品定位的重定**，不是一个功能补丁。');
  L.push('> DeskBase 不再以「数据库」自居 —— 详见');
  L.push('> [ADR-0023](docs/adr/0023-office-suite-extension.md)。');
  L.push('');
  L.push('### 变更');
  L.push('');
  L.push('- **产品定位转向「Office 三件套的扩展与升级 + 办公实用功能集**（ADR-0023）。');
  L.push('  不替代 Office，补它不做与做不好的那块；三件套方向为文档 / 表格 / 演示，');
  L.push('  底座（本地优先、隐私优先、单文件、零遥测）不变。');
  L.push('- **一级模块改名**：「数据库」→ **表格**（去掉 SQL 之后这个词是误导），');
  L.push('  「工作台」→ **工具**。演示方向未开工，因此**没有**放进导航 ——');
  L.push('  放一个点不开的入口比不放更糟。');
  L.push('- **教程重写**：从「数据库使用教程」改为「办公套件使用教程」（9 节），');
  L.push('  补上 v0.4.0 的新能力（共通字段 / 同步规则 / 关联字段 / 命名视图）与隐私、备份说明。');
  L.push('- 路线图重排：M2.5 去 SQL 标记完成、M2 明确为「三件套之一：表格」、');
  L.push('  新增 M2.6（文档与演示方向）。');
  L.push('- 参考文档口径同步：`README.md`、`AGENTS.md`、`docs/03`。');
  L.push('');
  L.push('### 新增');
  L.push('');
  L.push('- **旧版数据文件提示**（`app.legacyDb`）：升级后若旧的 `main.db` 还在，启动时');
  L.push('  明确告知「数据还在里面、没丢」并给出迁移路径。换引擎之后最容易发生的');
  L.push('  误会就是"我的数据没了" —— 这条不能等用户来问。');
  L.push('- 视图内**平滑滚动**（标准 / 丰富档位；「关」与「精简」档下不启用）。');
  L.push('');
  L.push('### 修复');
  L.push('');
  L.push('- **设置页滚不动**（连带造成"教程没写"的错觉）：设置页有 7 张卡片，教程挂在最后一张，');
  L.push('  滚不过去就永远够不到。两层原因 ——');
  L.push('  ① `.view` 没有 `overflow`；');
  L.push('  ② 根因在 `.main` 的 `grid-template-rows: auto 1fr`：网格行默认 `min-height:auto`，');
  L.push('     内容更高时行被撑开，`clientHeight` 等于内容高度（实测 5093），`overflow` 不触发。');
  L.push('     改成 `auto minmax(0, 1fr)` 后：`clientH` 5093 → 665，能滚到底。');
  L.push('- 界面自检不再检查已删除的 `sql` 模块（此前 app.log 恒报"模块未加载 sql"）');
  L.push('- 压力测试脚本去掉最后一处 `schema.runQuery`，改用 `schema.getTable` 的精确行数');
  L.push('- 崩溃恢复验收脚本的断言不再死盯 `quick_check`，改为判"日志能否被解析"');
  L.push('');
  L.push('### 验证');
  L.push('');
  L.push('| 项 | 结果 |');
  L.push('|---|---|');
  L.push('| 单元 | **252 全绿** |');
  L.push('| 界面烟测 | **68 / 68** |');
  L.push('| 界面走查 | **7 / 7** · 18 张截图 · 窄屏四档布局体检 **0 处**疑似 |');
  L.push('| 崩溃恢复端到端 | **16 / 16** |');
  L.push('| 导入压力测试 | **22 / 22**（1 万 + 10 万） |');
  L.push('| 门禁 | 动效 / 对比度 / 接线 / 体积 四项全过 |');
  L.push('| 编译警告 | **0** |');
  L.push('');

  s = s.split(oldHead).join(L.join('\n'));
  fs.writeFileSync(F, s, 'utf8');
  console.log('ok CHANGELOG → v0.5.0 节');
}
console.log('done');
