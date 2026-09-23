const fs = require('fs');
function need(F, from, to, label) {
  let s = fs.readFileSync(F, 'utf8');
  if (!s.includes(from)) { console.error('!! 未命中[' + F + '] ' + label); process.exit(1); }
  fs.writeFileSync(F, s.split(from).join(to), 'utf8');
  const a = fs.readFileSync(F, 'utf8');
  console.log('ok ' + label + '（' + (a.includes(to.slice(0, 40)) ? '已落盘' : '未落盘！') + '）');
}

// ---------- 1) main.rs：note.exportMd ----------
need(
  'app/src/main.rs',
  `        "note.delete" => {`,
  `        // ---------- 文档方向第一步：写完能带走 ----------
        // 为什么先做导出而不是富文本：笔记能存、能自动保存，但**出不去** ——
        // 写好的东西被锁在程序里。导出 Markdown 是最低限度的一份自由：通用、
        // 纯文本、任何编辑器都能打开，而且不做格式转换就不会丢内容。
        "note.exportMd" => {
            let Some(nid) = req.args.get("id").and_then(|v| v.as_str()) else {
                return err(id, "缺少参数 id");
            };
            let note = match state.db.lock() {
                Ok(d) => match db::get_note(&d, nid) {
                    Ok(Some(n)) => n,
                    Ok(None) => return err(id, "找不到这篇笔记"),
                    Err(e) => return err(id, e),
                },
                Err(_) => return err(id, "数据库锁失败"),
            };
            // 标题里可能有文件名用不了的符号，先换成下划线 ——
            // 否则保存对话框拿到一个非法名字，失败得莫名其妙
            let safe: String = note
                .title
                .chars()
                .map(|c| if "/\\\\:*?\\"<>|".contains(c) { '_' } else { c })
                .collect();
            let name = if safe.trim().is_empty() {
                "未命名".to_string()
            } else {
                safe.trim().to_string()
            };
            let Some(dst) = rfd::FileDialog::new()
                .set_title("导出为 Markdown")
                .set_file_name(format!("{name}.md"))
                .add_filter("Markdown", &["md"])
                .save_file()
            else {
                return ok(id, serde_json::json!({ "cancelled": true }));
            };
            let mut body = String::new();
            // 标题写成一级标题 —— 导出去之后还看得出这是哪一篇
            if !note.title.trim().is_empty() {
                body.push_str("# ");
                body.push_str(note.title.trim());
                body.push_str("\\n\\n");
            }
            body.push_str(&note.content);
            if !body.ends_with('\\n') {
                body.push('\\n');
            }
            match std::fs::write(&dst, body.as_bytes()) {
                Ok(()) => {
                    log_line(&state.data_dir, &format!("导出笔记：{}", dst.display()));
                    ok(id, serde_json::json!({ "path": dst.to_string_lossy() }))
                }
                Err(e) => err(id, format!("写文件失败：{e}")),
            }
        }

        "note.delete" => {`,
  'note.exportMd IPC'
);

// ---------- 2) index.html：笔记编辑区加两个按钮 ----------
need(
  'app/ui/index.html',
  `              <input class="title-input" id="note-title" placeholder="标题" />
            </div>`,
  `              <input class="title-input" id="note-title" placeholder="标题" />
              <div class="editor-actions">
                <select class="select select-sm" id="note-tpl" title="套用一个常用文档骨架">
                  <option value="">套用模板…</option>
                </select>
                <button class="btn btn-ghost db-mini" id="btn-note-export" type="button"
                        title="把这篇导出成 .md 文件（Markdown，任何编辑器都能打开）">
                  导出 .md
                </button>
              </div>
            </div>`,
  '笔记编辑区加导出与模板'
);

fs.writeFileSync('app/ui/app.js', fs.readFileSync('app/ui/app.js', 'utf8'));
console.log('done');
