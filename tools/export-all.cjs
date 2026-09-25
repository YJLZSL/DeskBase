const fs = require('fs');

// ---------- 1) 登记 mod ----------
{
  const F = 'app/src/main.rs';
  let s = fs.readFileSync(F, 'utf8');
  if (s.includes('mod export_all;')) {
    console.log('(mod export_all 已存在)');
  } else {
    const a = 'mod excel_import;';
    if (!s.includes(a)) { console.error('!! 找不到 mod 锚点'); process.exit(1); }
    s = s.split(a).join(a + '\nmod export_all;');
    fs.writeFileSync(F, s, 'utf8');
    console.log('ok mod export_all');
  }
}

// ---------- 2) IPC ----------
{
  const F = 'app/src/main.rs';
  let s = fs.readFileSync(F, 'utf8');
  const anchor = `        "note.exportHtml" => {`;
  if (!s.includes(anchor)) { console.error('!! 找不到锚点'); process.exit(1); }
  const block = [
    '        // ---------- 一键全量导出（可迁移性）----------',
    '        //',
    '        // 它兑现的是「不锁定用户」这条承诺：把所有数据整成通用格式（.csv / .md）',
    '        // 放进一个目录，附带一份**人类可读**的 README.txt。',
    '        // 详见 `docs/05-office-toolbox.md` §13.3 与 `app/src/export_all.rs` 顶部。',
    '        "export.all" => {',
    '            let out_root = state.data_dir.join("exports");',
    '            if let Err(e) = std::fs::create_dir_all(&out_root) {',
    '                return err(id, format!("建导出目录失败：{e}"));',
    '            }',
    '            match state.db.lock() {',
    '                Ok(mut d) => match export_all::export_all(&mut d, &out_root) {',
    '                    Ok(r) => {',
    '                        log_line(',
    '                            &state.data_dir,',
    '                            &format!(',
    '                                "全量导出：{}（{} 张表 · {} 篇笔记 · {} 个文件 · {} 字节）",',
    '                                r.dir.display(),',
    '                                r.tables,',
    '                                r.notes,',
    '                                r.files,',
    '                                r.bytes',
    '                            ),',
    '                        );',
    '                        ok(',
    '                            id,',
    '                            serde_json::json!({',
    '                                "dir": r.dir.to_string_lossy(),',
    '                                "tables": r.tables,',
    '                                "notes": r.notes,',
    '                                "files": r.files,',
    '                                "bytes": r.bytes,',
    '                            }),',
    '                        )',
    '                    }',
    '                    Err(e) => err(id, e),',
    '                },',
    '                Err(_) => err(id, "数据库锁失败"),',
    '            }',
    '        }',
    '',
    anchor,
  ].join('\n');
  s = s.split(anchor).join(block);
  fs.writeFileSync(F, s, 'utf8');
  console.log('ok export.all IPC');
}

// ---------- 3) 按钮 ----------
{
  const F = 'app/ui/index.html';
  let s = fs.readFileSync(F, 'utf8');
  const anchor = `              备份数据
            </button>`;
  if (!s.includes(anchor)) { console.error('!! 找不到备份按钮'); process.exit(1); }
  const block = [
    anchor,
    '            <!-- 一键全量导出：把所有数据整成通用格式（.csv / .md）+ 一份人类可读的',
    '                 README。它兑现的是"不锁定用户"——就算 DeskBase 将来没有了，',
    '                 这个文件夹依然打得开、看得懂。 -->',
    '            <button class="btn btn-ghost db-import" id="btn-export-all" type="button"',
    '                    title="把所有表格与笔记导出成通用格式（CSV / Markdown）到 exports/，并附一份说明">',
    '              导出全部数据',
    '            </button>',
  ].join('\n');
  s = s.split(anchor).join(block);
  fs.writeFileSync(F, s, 'utf8');
  console.log('ok 导出按钮');
}

// ---------- 4) JS ----------
{
  const F = 'app/ui/app.js';
  let s = fs.readFileSync(F, 'utf8');
  const anchor = `  $("#btn-db-backup").addEventListener("click", async () => {`;
  if (!s.includes(anchor)) { console.error('!! 找不到备份 JS'); process.exit(1); }
  const block = [
    '  // 一键全量导出。和"备份"不是一回事，分工要跟用户说清：',
    '  //   备份 → 给 DeskBase 自己用的（恢复用），格式是它自己的',
    '  //   导出 → 给**别的软件**用的（Excel / 任何编辑器），格式是通用的',
    '  $("#btn-export-all").addEventListener("click", async () => {',
    '    const btn = $("#btn-export-all");',
    '    btn.disabled = true;',
    '    btn.textContent = "导出中…";',
    '    try {',
    '      const r = await call("export.all", {});',
    '      toast(',
    '        "已导出 " + r.tables + " 张表、" + r.notes + " 篇笔记到：" + r.dir +',
    '          "（里面有一份 README.txt，讲清了每个文件是什么）"',
    '      );',
    '    } catch (e) {',
    '      toast("导出失败：" + ((e && e.message) || e), "error");',
    '    } finally {',
    '      btn.disabled = false;',
    '      btn.textContent = "导出全部数据";',
    '    }',
    '  });',
    '',
    anchor,
  ].join('\n');
  s = s.split(anchor).join(block);
  fs.writeFileSync(F, s, 'utf8');
  console.log('ok JS 接线');
}
console.log('done');
