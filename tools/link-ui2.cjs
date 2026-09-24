const fs = require('fs');

// ---------- db.js：列表里显示关联信息 ----------
{
  const F = 'app/ui/db.js';
  let s = fs.readFileSync(F, 'utf8');

  // 1) 顺便取 columnMeta（里面有 link）
  const from1 = `    // 表注释
    let info;
    try {
      info = await call("schema.getTable", { name: tname });
    } catch (e) {
      toast("读表结构失败：" + errText(e), "error");
      return;
    }`;
  if (!s.includes(from1)) { console.error('!! 未命中取数'); process.exit(1); }
  const to1 = `    // 表注释
    let info;
    try {
      info = await call("schema.getTable", { name: tname });
    } catch (e) {
      toast("读表结构失败：" + errText(e), "error");
      return;
    }
    // 列语义（含 link / 共通 / 同步）在 columnMeta 里，getTable 只有裸结构。
    // 取不到不影响改结构，所以失败就当成空 —— 不必为此挡住整个对话框。
    let metas = [];
    try {
      metas = (await call("schema.columnMeta", { name: tname })) || [];
    } catch (_) {
      metas = [];
    }
    const metaOf = (n) => metas.find((m) => m.name === n) || null;`;
  s = s.split(from1).join(to1);

  // 2) 列行上显示"→ 目标表"
  const from2 = `      row.append(
        el("span", { class: "n" }, c.name),
        el("span", { class: "t" }, fmtColType(c.decl_type) + (tags ? "（" + tags + "）" : ""))
      );`;
  if (!s.includes(from2)) { console.error('!! 未命中列行'); process.exit(1); }
  const to2 = `      const meta = metaOf(c.name);
      row.append(
        el("span", { class: "n" }, c.name),
        el("span", { class: "t" }, fmtColType(c.decl_type) + (tags ? "（" + tags + "）" : ""))
      );
      // 关联列要说清"连到哪张表" —— 光看列名和类型看不出来，
      // 而这一列的意义全在目标表上。columnMeta.link 早就返回了，界面以前没用。
      if (meta && meta.link) {
        row.append(
          el(
            "span",
            { class: "s db-link-tag" },
            "→ " + meta.link.target + (meta.link.many ? "（可多条）" : "")
          )
        );
      }
      if (meta && meta.shared) {
        row.append(el("span", { class: "s" }, "⇄ 共通"));
      }`;
  s = s.split(from2).join(to2);

  fs.writeFileSync(F, s, 'utf8');
  const a = fs.readFileSync(F, 'utf8');
  console.log('ok 表结构显示关联（回读：' + (a.includes('db-link-tag') ? '已落盘' : '未落盘！') + '）');
}

// ---------- CSS ----------
{
  const F = 'app/ui/database.css';
  let s = fs.readFileSync(F, 'utf8');
  const anchor = `/* 侧栏里的东西一律不得超过侧栏。`;
  const block = `/* 关联列的标记（表结构列表里）。
   关联列的意义全在"连到哪张表"上 —— 只显示列名和类型等于没说。 */
.db-link-tag {
  color: var(--accent);
  white-space: nowrap;
}

/* 「加列」表单里的关联选项：缩进一块，表示它从属于"列角色"这个选择 */
.db-schema-link {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--sp-2);
  padding: var(--sp-2);
  border-left: 2px solid var(--line);
  margin-block: 2px;
}
.db-schema-link .hint {
  margin: 0;
}

/* 侧栏里的东西一律不得超过侧栏。`;
  if (!s.includes(anchor)) { console.error('!! 未命中 CSS 锚点'); process.exit(1); }
  s = s.split(anchor).join(block);
  fs.writeFileSync(F, s, 'utf8');
  console.log('ok CSS（回读：' + (fs.readFileSync(F, 'utf8').includes('db-schema-link') ? '已落盘' : '未落盘！') + '）');
}
console.log('done');
