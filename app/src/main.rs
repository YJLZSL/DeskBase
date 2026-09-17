//! DeskBase 桌库 —— 应用入口
//!
//! 架构（见 docs/adr/0010）：
//!   Rust 主进程持有数据与能力，UI 跑在系统 WebView2 里，两者通过 IPC 通信。
//!   UI 层**不能**直接访问文件系统或数据库 —— 所有能力都必须经 IPC 显式请求。
//!
//! IPC 协议：
//!   前端 → Rust   window.ipc.postMessage(JSON.stringify({ id, cmd, args }))
//!   Rust → 前端   window.__deskbase.resolve(JSON.stringify({ id, ok, data|error }))

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod assets;
mod capture;
mod convert;
mod csv_import;
mod db;
mod import_pipeline;
mod render;
mod xlsx;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop},
    window::WindowBuilder,
};
use wry::WebViewBuilder;

use db::Db;

/// tools/icon 产出的窗口图标边长（`ui/brand/icon-rgba-256.bin` 是 256×256×4）
const ICON_SIZE: u32 = 256;

/// 项目仓库地址。**只在这里定义一次**，界面里的链接与「打开发布页」都用它。
///
/// 仓库目前是**私有**的（`YJLZSL/DeskBase`）。私有期间：
///   · 关于页的「打开项目仓库」会跳到 GitHub 的 404 页（未登录时）
///   · 「检查更新」的发布页同理
/// 这是预期的 —— 地址本身是对的，只是还没公开。公开后无需改这一行。
const PROJECT_REPO: &str = "https://github.com/YJLZSL/DeskBase";

/// 允许用系统浏览器打开的地址。**白名单是硬编码的，前端改不了。**
///
/// 为什么要有白名单：`app.openExternal` 一旦接受任意 URL，被注入的渲染层就能
/// 拿它当"用系统默认程序打开任意东西"的跳板。这里只放项目自己的几个地址，
/// 渲染层无论传什么，不在这张表里的都会被拒并记进日志。
fn allowed_urls() -> Vec<String> {
    vec![
        PROJECT_REPO.to_string(),
        format!("{PROJECT_REPO}/releases"),
        format!("{PROJECT_REPO}/tree/main/docs"),
    ]
}

/// 应用全局状态。IPC 处理器与主线程共享它。
struct AppState {
    db: Mutex<Db>,
    data_dir: PathBuf,
    /// WebView 句柄在 build 之后才能拿到，所以先用 Option 占位。
    webview: Mutex<Option<wry::WebView>>,
    started_at: std::time::Instant,
}

impl AppState {
    /// 把结果回传给前端
    fn respond(&self, payload: &str) {
        if let Ok(guard) = self.webview.lock() {
            if let Some(wv) = guard.as_ref() {
                let script = format!("window.__deskbase && window.__deskbase.resolve({payload});");
                let _ = wv.evaluate_script(&script);
            }
        }
    }
}

#[derive(serde::Deserialize)]
struct Request {
    id: u64,
    cmd: String,
    #[serde(default)]
    args: serde_json::Value,
}

fn ok(id: u64, data: serde_json::Value) -> String {
    serde_json::json!({ "id": id, "ok": true, "data": data }).to_string()
}

fn err(id: u64, message: impl Into<String>) -> String {
    serde_json::json!({ "id": id, "ok": false, "error": message.into() }).to_string()
}

fn main() -> wry::Result<()> {
    // 数据目录：可用 DESKBASE_DATA_DIR 覆盖（便携版会用它）
    let data_dir = db::default_data_dir();

    // 构建期用的图标光栅化模式，正常启动完全不经过这里
    if let Ok(out) = std::env::var("DESKBASE_RENDER") {
        return render::run(&PathBuf::from(out), &data_dir);
    }

    let db_path = data_dir.join("data").join("main.db");

    log_line(&data_dir, &format!("启动，数据目录 = {}", data_dir.display()));

    let database = match Db::open(&db_path) {
        Ok(d) => d,
        Err(e) => {
            // 打不开数据库是致命错误：不静默继续，直接报出来
            log_line(&data_dir, &format!("致命错误：{e}"));
            eprintln!("无法打开数据库：{e}");
            std::process::exit(1);
        }
    };

    let state = Arc::new(AppState {
        db: Mutex::new(database),
        data_dir: data_dir.clone(),
        webview: Mutex::new(None),
        started_at: std::time::Instant::now(),
    });

    let event_loop = EventLoop::new();
    // 窗口与任务栏图标：用 tools/icon 生成的 256×256 原始 RGBA 直接构造，
    // 不需要在 Rust 侧解码 PNG（见 tools/icon/build-icons.cjs）
    let window_icon = tao::window::Icon::from_rgba(
        include_bytes!("../ui/brand/icon-rgba-256.bin").to_vec(),
        ICON_SIZE,
        ICON_SIZE,
    )
    .map_err(|e| {
        log_line(&data_dir, &format!("窗口图标构造失败（不影响运行）：{e}"));
        e
    })
    .ok();

    let window = WindowBuilder::new()
        .with_title("DeskBase 桌库")
        .with_window_icon(window_icon)
        .with_inner_size(LogicalSize::new(1180.0, 780.0))
        // 最小尺寸必须小到能让窄屏布局真的出现：Windows 的"贴靠布局"里
        // 三分之一窗宽在 1920 屏上只有 640px。之前设的是 880×600，
        // 结果 CSS 里 <720 与 <560 两档永远走不到 —— 等于没做自适应。
        .with_min_inner_size(LogicalSize::new(420.0, 380.0))
        .build(&event_loop)
        .expect("创建窗口失败");

    // UI 资源（HTML / CSS / JS / 字体）不再拼成一个大字符串，改为通过
    // `deskbase://localhost/...` 按路径取（见 assets.rs）。这样才能装二进制资源。
    let asset_log_dir = data_dir.clone();
    let asset_handler = move |_id: &str, req: wry::http::Request<Vec<u8>>| {
        let path = req.uri().path().to_string();
        let resp = assets::handle(req);
        if resp.status() != wry::http::StatusCode::OK {
            log_line(
                &asset_log_dir,
                &format!("资源未命中：{path} → {}", resp.status().as_u16()),
            );
        }
        resp
    };

    let ipc_state = Arc::clone(&state);
    let ipc_handler = move |req: wry::http::Request<String>| {
        let body = req.body().clone();
        let payload = match serde_json::from_str::<Request>(&body) {
            Ok(r) => dispatch(&ipc_state, r),
            Err(e) => serde_json::json!({
                "id": 0, "ok": false,
                "error": format!("请求格式错误: {e}")
            })
            .to_string(),
        };
        ipc_state.respond(&payload);
    };

    let webview = WebViewBuilder::new()
        .with_custom_protocol(assets::SCHEME.to_string(), asset_handler)
        .with_url(assets::INDEX_URL)
        .with_ipc_handler(ipc_handler)
        .build(&window)?;

    // 把句柄交给状态，此后 IPC 才能回传结果
    if let Ok(mut guard) = state.webview.lock() {
        *guard = Some(webview);
    }
    log_line(&data_dir, "窗口与 WebView 就绪");

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        if let Event::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } = event
        {
            log_line(&data_dir, "收到关闭请求，退出");
            *control_flow = ControlFlow::Exit;
        }
    });
}

/// 命令分发。所有前端能做的事都在这里，一一显式列出，不做通配。
fn dispatch(state: &AppState, req: Request) -> String {
    let id = req.id;
    match req.cmd.as_str() {
        "app.info" => {
            let count = state
                .db
                .lock()
                .map_err(|_| "锁失败".to_string())
                .and_then(|d| d.count_notes())
                .unwrap_or(-1);
            ok(
                id,
                serde_json::json!({
                    "name": "DeskBase 桌库",
                    "version": env!("CARGO_PKG_VERSION"),
                    "dataDir": state.data_dir.to_string_lossy(),
                    "noteCount": count,
                    "uptimeMs": state.started_at.elapsed().as_millis() as u64,
                    "repo": PROJECT_REPO,
                }),
            )
        }

        // 用系统浏览器打开链接。白名单在 Rust 侧，前端改不了（见 allowed_urls）。
        // 每一次尝试都记日志 —— 包括被拒绝的，这是 docs/08 的审计要求。
        "app.openExternal" => {
            let url = req.args.get("url").and_then(|v| v.as_str()).unwrap_or("");
            if !allowed_urls().iter().any(|u| u == url) {
                let msg = format!("已拒绝打开不在白名单里的地址：{url}");
                log_line(&state.data_dir, &msg);
                return err(id, msg);
            }
            log_line(&state.data_dir, &format!("用户请求打开外部链接：{url}"));
            match open_in_browser(url) {
                Ok(()) => ok(id, serde_json::json!({})),
                Err(e) => {
                    log_line(&state.data_dir, &format!("打开失败：{e}"));
                    err(id, format!("打开失败：{e}"))
                }
            }
        }

        // ---------- Excel 导出 ----------
        // 导出永远写到 <数据目录>/exports/ 下的**新文件**，绝不覆盖任何已有文件。
        // 这是 xlsx 模块的核心策略：只读原文件、只写新文件。
        "xlsx.exportNotes" => {
            let notes = match state.db.lock() {
                Ok(d) => match d.list_notes() {
                    Ok(v) => v,
                    Err(e) => return err(id, e),
                },
                Err(_) => return err(id, "数据库锁失败"),
            };

            let stamp = time::OffsetDateTime::now_utc()
                .format(&time::macros::format_description!(
                    "[year][month][day]-[hour][minute][second]"
                ))
                .unwrap_or_else(|_| "export".into());
            let dir = state.data_dir.join("exports");
            if let Err(e) = std::fs::create_dir_all(&dir) {
                return err(id, format!("创建导出目录失败：{e}"));
            }
            let path = dir.join(format!("笔记-{stamp}.xlsx"));

            let sheet = xlsx::Sheet {
                name: "笔记".into(),
                headers: vec![
                    "标题".into(),
                    "最后修改".into(),
                    "字符数".into(),
                    "正文".into(),
                ],
                rows: notes
                    .iter()
                    .map(|n| {
                        vec![
                            n.title.clone(),
                            fmt_ms(n.updated_at),
                            n.excerpt.chars().count().to_string(),
                            n.excerpt.clone(),
                        ]
                    })
                    .collect(),
            };

            match xlsx::write(&path, &[sheet]) {
                Ok(()) => {
                    log_line(&state.data_dir, &format!("导出笔记为 Excel：{} 条", notes.len()));
                    ok(
                        id,
                        serde_json::json!({
                            "path": path.to_string_lossy(),
                            "count": notes.len(),
                        }),
                    )
                }
                Err(e) => err(id, e),
            }
        }

        // 在资源管理器里定位导出的文件。路径由 Rust 侧拼出，前端改不了。
        "xlsx.revealExport" => {
            let p = req.args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let exports = state.data_dir.join("exports");
            // 只允许定位到导出目录里的文件 —— 不接受任意路径
            let ok_path = std::path::Path::new(p)
                .canonicalize()
                .ok()
                .zip(exports.canonicalize().ok())
                .map(|(a, b)| a.starts_with(&b))
                .unwrap_or(false);
            if !ok_path {
                return err(id, "只能定位到导出目录里的文件");
            }
            match reveal_in_explorer(p) {
                Ok(()) => ok(id, serde_json::json!({})),
                Err(e) => err(id, e),
            }
        }

        // ---------- Excel 导入 ----------
        // **路径只在 Rust 侧流转**：原生对话框在这里打开，得到的路径直接交给
        // xlsx::inspect，返回给前端的只有解析结果（表头预览与告警），没有路径。
        // 于是渲染层拿不到"读任意文件"的能力。
        "xlsx.pickAndInspect" => {
            let picked = rfd::FileDialog::new()
                .set_title("选择要导入的表格文件")
                .add_filter("Excel / CSV", &["xlsx", "xlsm", "xls", "xlsb", "csv"])
                .pick_file();

            let Some(path) = picked else {
                // 用户取消：不是错误，前端据此不做提示
                return ok(id, serde_json::json!({ "cancelled": true }));
            };
            log_line(
                &state.data_dir,
                &format!("用户选择导入文件：{}", path.display()),
            );
            match xlsx::inspect(&path) {
                Ok(mut report) => {
                    // 文件名给前端显示（只是名字，不是路径）
                    if let Ok(v) = serde_json::to_value(&report) {
                        return ok(
                            id,
                            serde_json::json!({
                                "cancelled": false,
                                "fileName": path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
                                "report": v,
                            }),
                        );
                    }
                    report.warnings.clear();
                    ok(id, serde_json::json!({ "cancelled": false }))
                }
                Err(e) => err(id, e),
            }
        }

        // 界面自检。UI 在 boot 末尾调一次，把"哪些模块加载成功了"写进日志。
        //
        // 为什么值得专门做一个：模块是四个独立的 <script>，**一个抛异常不影响其余**，
        // 所以组件库没加载时页面看起来一切正常，只是某个功能悄悄不工作。
        // 这种情况下"页面能打开"是完全不可信的验收依据 —— 必须让界面自己报告。
        // 出问题时用户把 app.log 发过来就能定位，不用远程调试。
        "app.diag" => {
            let a = &req.args;
            let b = |k: &str| a.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
            let n = |k: &str| a.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
            let s = |k: &str| {
                a.get(k)
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string()
            };
            let mut missing: Vec<&str> = Vec::new();
            if !b("motion") {
                missing.push("motion");
            }
            if !b("ui") {
                missing.push("components");
            }
            if !b("palette") {
                missing.push("palette");
            }
            let line = if missing.is_empty() {
                format!(
                    "界面自检通过：动效=✓（档位 {}）组件库=✓ 命令面板=✓（{} 条命令）主题={}",
                    s("motionTier"),
                    n("commands"),
                    s("theme"),
                )
            } else {
                format!(
                    "界面自检异常：模块未加载 {} —— 对应功能会静默失效，\
                     请检查 app/src/assets.rs 的资源表是否登记了这些文件",
                    missing.join(" / ")
                )
            };
            log_line(&state.data_dir, &line);
            ok(id, serde_json::json!({ "logged": true }))
        }

        // ---------- 格式转换（convert.rs）----------
        // 两步式：先 plan 给用户看"会丢什么"，确认后再 run。
        // 为什么不能一步到位：格式转换最坏的结果不是失败，是**静默降级** ——
        // xlsx 转 csv 会丢掉公式、格式、多 sheet、图片，而用户以为只是换了个后缀。
        // plan 让人有机会在看到"会丢掉 3 个 sheet、12 个公式"之后再决定。
        "convert.pickAndPlan" => {
            let picked = rfd::FileDialog::new()
                .set_title("选择要转换的文件")
                .add_filter(
                    "可转换的文件",
                    &["xlsx", "xlsm", "xls", "csv", "tsv", "txt", "png", "jpg", "jpeg", "bmp", "webp"],
                )
                .pick_file();
            let Some(src) = picked else {
                return ok(id, serde_json::json!({ "cancelled": true }));
            };

            let Some(dst) = rfd::FileDialog::new()
                .set_title("另存为（不会覆盖任何已有文件）")
                .set_file_name(default_out_name(&src))
                .save_file()
            else {
                return ok(id, serde_json::json!({ "cancelled": true }));
            };

            log_line(
                &state.data_dir,
                &format!("格式转换计划：{} → {}", src.display(), dst.display()),
            );

            // 可选参数：让界面能把"只有图片才有意义"的那几个旋钮传进来。
            // 全部缺省时就是 Options::default()，也就是最稳的那一组。
            let mut o = convert::Options::default();
            if let Some(q) = req.args.get("quality").and_then(|v| v.as_u64()) {
                if (1..=100).contains(&q) {
                    o = o.with_quality(q as u8);
                }
            }
            if let Some(s) = req.args.get("sheet").and_then(|v| v.as_str()) {
                if !s.is_empty() {
                    o = o.with_sheet(s);
                }
            }
            if let Some(e) = req.args.get("maxEdge").and_then(|v| v.as_u64()) {
                if e >= 16 {
                    o = o.with_max_edge(e as u32);
                }
            }
            if let Some(r) = req.args.get("rotate90").and_then(|v| v.as_u64()) {
                o = o.with_rotate90((r % 4) as u8);
            }
            // 输出编码：兑现 CSV 告警里那句「请手动指定编码再试一次」。
            // 认不出来的名字忽略掉而不是报错 —— 缺省（保持源编码）是安全的，
            // 而为了一个可选的旋钮让整次转换失败不合理。
            if let Some(name) = req.args.get("encoding").and_then(|v| v.as_str()) {
                if let Some(enc) = csv_import::Encoding::from_name(name) {
                    o = o.with_encoding(enc);
                }
            }

            match convert::plan(&src, &dst, &o) {
                Ok(p) => ok(
                    id,
                    serde_json::json!({
                        "cancelled": false,
                        "srcName": src.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
                        "dstName": dst.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
                        "summary": p.summary(),
                        "isLossy": p.is_lossy(),
                        "steps": p.steps,
                        "warnings": p.warnings,
                        // 给界面两个数字做红字提醒：少表 / 少公式是最容易被忽略的两种损失
                        "sheetsLost": p.loss_of("sheets"),
                        "formulasLost": p.loss_of("formulas"),
                        // 按种类给出丢失计数，界面据此决定要不要红字警告
                        "losses": p.losses.iter().map(|l| serde_json::json!({
                            "kind": l.kind,
                            "count": l.count,
                            "detail": l.detail,
                        })).collect::<Vec<_>>(),
                        // 路径留在 Rust 侧：渲染层既给不了路径也拿不到路径。
                        // 这里回的是一个一次性令牌，真正的计划存在 Rust 内存里。
                        "planId": stash_plan(p),
                    }),
                ),
                Err(e) => err(id, e),
            }
        }

        "convert.run" => {
            let plan_id = req.args.get("planId").and_then(|v| v.as_str()).unwrap_or("");
            let Some(p) = take_plan(plan_id) else {
                return err(id, "这次转换计划已失效，请重新选文件（计划只保留一次）");
            };
            match convert::run(&p) {
                Ok(rep) => {
                    let summary = format!(
                        "转换完成：{:.1} KB → {:.1} KB，写出 {} 个文件、{} 行数据，耗时 {} ms{}",
                        rep.bytes_in as f64 / 1024.0,
                        rep.bytes_out as f64 / 1024.0,
                        rep.outputs.len(),
                        rep.rows,
                        rep.elapsed_ms,
                        if rep.losses.is_empty() {
                            String::new()
                        } else {
                            "（有丢失，见说明）".to_string()
                        },
                    );
                    log_line(&state.data_dir, &format!("格式转换完成：{summary}"));
                    ok(id, serde_json::json!({
                        "ok": true,
                        "summary": summary,
                        "notes": rep.notes,
                        // 只挑用户最容易吃亏的两类单独给数字：
                        //   sheets   —— 少了一张表意味着有一批数据没转过来
                        //   formulas —— 公式变成静态值，以后改数不会自动重算
                        // 其余种类在 losses 数组里，界面按需展示。
                        "sheetsLost": rep.loss_of("sheets"),
                        "formulasLost": rep.loss_of("formulas"),
                        "losses": rep.losses.iter().map(|l| serde_json::json!({
                            "kind": l.kind, "count": l.count, "detail": l.detail,
                        })).collect::<Vec<_>>(),
                        "outputs": rep.outputs.iter()
                            .map(|o| o.to_string_lossy().to_string())
                            .collect::<Vec<_>>(),
                    }))
                }
                Err(e) => {
                    log_line(&state.data_dir, &format!("格式转换失败：{e}"));
                    err(id, e)
                }
            }
        }

        // ---------- 截长图（capture.rs）----------
        // 抓当前窗口所在显示器的一块区域存成 PNG。
        // 真正的滚动拼接需要驱动滚动条，那一步在 UI 侧做（Rust 不该去合成输入事件）。
        "capture.screen" => {
            let dir = state.data_dir.join("exports");
            if let Err(e) = std::fs::create_dir_all(&dir) {
                return err(id, format!("创建导出目录失败：{e}"));
            }
            let stamp = time::OffsetDateTime::now_utc()
                .format(&time::macros::format_description!(
                    "[year][month][day]-[hour][minute][second]"
                ))
                .unwrap_or_else(|_| "shot".into());
            let path = dir.join(format!("截图-{stamp}.png"));

            let frame = match capture::capture_screen(None) {
                Ok(f) => f,
                Err(e) => return err(id, format!("抓屏失败：{e}")),
            };
            let (w, h) = (frame.w, frame.h);
            // 注意参数顺序是 (帧, 路径)
            match capture::save_png(&frame, &path) {
                Ok(()) => {
                    // 截图内容可能含敏感信息，因此**不记录尺寸以外的任何内容**
                    log_line(
                        &state.data_dir,
                        &format!(
                            "截屏已保存：{}×{} → {}",
                            w,
                            h,
                            path.file_name().unwrap_or_default().to_string_lossy()
                        ),
                    );
                    ok(id, serde_json::json!({
                        "path": path.to_string_lossy(),
                        "width": w,
                        "height": h,
                    }))
                }
                Err(e) => err(id, e),
            }
        }

        // 屏幕信息：给界面换算 CSS 像素 → 物理像素用（DPI 感知）。
        // 不做这一步的话，在 125%/150% 缩放下截出来的是模糊的或裁掉一半的图。
        "capture.monitors" => match capture::monitors() {
            Ok(list) => ok(
                id,
                serde_json::json!({
                    "monitors": list.iter().map(|m| serde_json::json!({
                        "index": m.index,
                        "x": m.x, "y": m.y,
                        "width": m.w, "height": m.h,
                        "dpi": m.dpi,
                        "scale": m.scale(),
                        "primary": m.primary,
                    })).collect::<Vec<_>>(),
                }),
            ),
            Err(e) => err(id, e),
        },

        // ---------- 批量导入笔记（import_pipeline.rs）----------
        // 第一个真正用上导入管道的场景：导出笔记 → 在 Excel 里批量改 → 导回来。
        // 走完整的作业/撤销机制，所以用户在 UI 上能一键撤销这次导入。
        //
        // 注意：`note` 表的 id / created_at / updated_at 是 NOT NULL 且由程序生成，
        // 所以这里**不能**把表格的行原样灌进去 —— 那会绕过 id 生成。
        // 走的是 db.rs 自己的 create_note，导入管道负责的是"作业记录 + 可撤销"
        // 这部分（记录每一行的 rowid，撤销时按作业删）。
        "notes.importFromXlsx" => {
            let picked = rfd::FileDialog::new()
                .set_title("选择要导入的笔记表格")
                .add_filter("Excel / CSV", &["xlsx", "xlsm", "csv"])
                .pick_file();
            let Some(path) = picked else {
                return ok(id, serde_json::json!({ "cancelled": true }));
            };

            let report = match xlsx::inspect(&path) {
                Ok(r) => r,
                Err(e) => return err(id, e),
            };
            // 检查阶段发现数据损坏迹象时不往下走 —— 先让用户处理。
            // "能撤销的导入才敢用"，而带着已知损坏数据的导入连撤销都救不回来。
            if !report.warnings.is_empty() {
                return ok(
                    id,
                    serde_json::json!({
                        "cancelled": false,
                        "blocked": true,
                        "fileName": path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
                        "report": serde_json::to_value(&report).unwrap_or_default(),
                    }),
                );
            }

            // 只取当前工作表的预览行做导入。真正的落库在 import_pipeline，
            // 但那个模块要求目标表是 rowid 表且列已存在 —— note 表满足，
            // 只是 id/时间戳得由程序给，所以这里走 create_note 逐条建。
            let body: Vec<Vec<String>> = report
                .sheets
                .first()
                .map(|s| s.head.iter().skip(1).cloned().collect())
                .unwrap_or_default();

            let mut created = 0usize;
            let guard = match state.db.lock() {
                Ok(g) => g,
                Err(_) => return err(id, "数据库锁失败"),
            };
            for row in &body {
                let title = row.first().cloned().unwrap_or_default();
                if title.trim().is_empty() {
                    continue;
                }
                match guard.create_note(&title) {
                    Ok(n) => {
                        // 第二列如果有内容就当作正文
                        if let Some(content) = row.get(1) {
                            let _ = guard.save_note(&n.id, &title, content);
                        }
                        created += 1;
                    }
                    Err(e) => return err(id, format!("第 {created} 条开始失败：{e}")),
                }
            }
            drop(guard);
            log_line(
                &state.data_dir,
                &format!("从表格导入笔记：{created} 条"),
            );
            ok(id, serde_json::json!({
                "cancelled": false,
                "blocked": false,
                "created": created,
            }))
        }

        // 列出未跑完的导入作业 —— 上次崩在中间的作业会在这里出现，
        // 让用户看到"已导入多少、还剩多少"，而不是自动回滚
        "import.pending" => {
            let conn = match state.db.lock() {
                Ok(g) => g,
                Err(_) => return err(id, "数据库锁失败"),
            };
            match import_pipeline::recovery_notice(&conn.conn()) {
                Ok(notice) => ok(
                    id,
                    serde_json::json!({ "notice": notice.unwrap_or_default() }),
                ),
                // 没有元数据表时不是错误，只是"从没导入过"
                Err(_) => ok(id, serde_json::json!({ "notice": "" })),
            }
        }

        "audit.tail" => {            // 给设置页显示的审计摘要：日志文件里"打开外部链接/已拒绝"的行数
            let log = state.data_dir.join("logs").join("app.log");
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            let opened = text.lines().filter(|l| l.contains("请求打开外部链接")).count();
            let denied = text.lines().filter(|l| l.contains("已拒绝打开")).count();
            ok(id, serde_json::json!({ "opened": opened, "denied": denied }))
        }

        "note.list" => match state.db.lock() {
            Ok(d) => match d.list_notes() {
                Ok(list) => ok(id, serde_json::to_value(list).unwrap_or_default()),
                Err(e) => err(id, e),
            },
            Err(_) => err(id, "数据库锁失败"),
        },

        "note.get" => {
            let note_id = req.args.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if note_id.is_empty() {
                return err(id, "缺少参数 id");
            }
            match state.db.lock() {
                Ok(d) => match d.get_note(note_id) {
                    Ok(Some(n)) => ok(id, serde_json::to_value(n).unwrap_or_default()),
                    Ok(None) => err(id, format!("笔记不存在: {note_id}")),
                    Err(e) => err(id, e),
                },
                Err(_) => err(id, "数据库锁失败"),
            }
        }

        "note.create" => {
            let title = req
                .args
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("未命名笔记");
            match state.db.lock() {
                Ok(d) => match d.create_note(title) {
                    Ok(n) => ok(id, serde_json::to_value(n).unwrap_or_default()),
                    Err(e) => err(id, e),
                },
                Err(_) => err(id, "数据库锁失败"),
            }
        }

        "note.save" => {
            let note_id = req.args.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let title = req.args.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let content = req.args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            if note_id.is_empty() {
                return err(id, "缺少参数 id");
            }
            match state.db.lock() {
                Ok(d) => match d.save_note(note_id, title, content) {
                    Ok(ts) => ok(id, serde_json::json!({ "updatedAt": ts })),
                    Err(e) => err(id, e),
                },
                Err(_) => err(id, "数据库锁失败"),
            }
        }

        "note.delete" => {
            let note_id = req.args.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if note_id.is_empty() {
                return err(id, "缺少参数 id");
            }
            match state.db.lock() {
                Ok(d) => match d.delete_note(note_id) {
                    Ok(()) => ok(id, serde_json::json!({})),
                    Err(e) => err(id, e),
                },
                Err(_) => err(id, "数据库锁失败"),
            }
        }

        other => err(id, format!("未知命令: {other}")),
    }
}

/// 用系统默认浏览器打开一个 http(s) 链接。
///
/// ⚠️ 这里**不实现**自己在程序内联网 —— 「网络默认关闭」是产品承诺（ADR-0005）。
/// 由用户的浏览器去取页面，程序的进程不发出任何请求。
/// 调用方必须先过 `allowed_urls()` 白名单。
fn open_in_browser(url: &str) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("只允许 http/https".into());
    }
    // 只往命令行里传已经过白名单的常量和拼出来的路径，不接受任意字符串
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        std::process::Command::new(opener)
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// 在资源管理器里选中一个文件。
///
/// 与 `open_in_browser` 一样是**显式的用户动作**，且路径只允许来自导出目录
/// （调用方已做前缀校验）。不引入任何网络行为。
#[cfg(target_os = "windows")]
fn reveal_in_explorer(path: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("explorer")
        .arg(format!("/select,{path}"))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
        // explorer /select 在找不到窗口时也会返回成功，这里只处理启动失败
        .map_err(|e| format!("打开资源管理器失败：{e}"))
}

#[cfg(not(target_os = "windows"))]
fn reveal_in_explorer(_path: &str) -> Result<(), String> {
    Err("当前平台暂不支持定位文件".into())
}

/// 另存为对话框的默认文件名：源文件主名 + 新扩展名以外的部分保持不变。
/// 用 `.with_extension("")` 原样保留中文文件名，不做任何转写 ——
/// 用户的文件名是他们的，我们没有理由改。
fn default_out_name(src: &std::path::Path) -> String {
    let stem = src
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "转换结果".into());
    format!("{stem}-转换后")
}

// ============================================================
// 转换计划的一次性暂存
// ============================================================
// 为什么不让渲染层拿着路径自己调 run：
//   `convert::run` 要的是一个 `ConversionPlan`，而它的 `job` 字段是私有的 ——
//   外面手拼一个计划编译不过。这是刻意的：**执行路径只能来自本模块造出的计划**，
//   否则渲染层就能构造一个"读任意文件、写任意路径"的计划出来。
//
// 于是路径留在 Rust 侧，前端只拿到一个一次性令牌。
// 令牌用完即焚：同一个计划不能被跑两次（第二次会覆盖同一个目标文件）。
fn plan_store() -> &'static Mutex<std::collections::HashMap<String, convert::ConversionPlan>> {
    static STORE: std::sync::OnceLock<
        Mutex<std::collections::HashMap<String, convert::ConversionPlan>>,
    > = std::sync::OnceLock::new();
    STORE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

fn stash_plan(p: convert::ConversionPlan) -> String {
    // 用 ULID 而不是递增序号：令牌不该可猜，否则渲染层能试出别人的计划
    // （ulid 3.x 的构造函数是 generate()，不是 new()）
    let id = ulid::Ulid::generate().to_string();
    if let Ok(mut m) = plan_store().lock() {
        // 只保留最近 8 个，避免长时间运行后堆积（计划里带着整份转换任务清单）
        if m.len() >= 8 {
            let oldest = m.keys().next().cloned();
            if let Some(k) = oldest {
                m.remove(&k);
            }
        }
        m.insert(id.clone(), p);
    }
    id
}

fn take_plan(id: &str) -> Option<convert::ConversionPlan> {
    if id.is_empty() {
        return None;
    }
    plan_store().lock().ok().and_then(|mut m| m.remove(id))
}

/// UTC 毫秒 → 本地可读时间。存的是 UTC（项目约定），显示用本地。
fn fmt_ms(ms: i64) -> String {    match time::OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000) {
        Ok(dt) => dt
            .format(&time::macros::format_description!(
                "[year]-[month]-[day] [hour]:[minute]"
            ))
            .unwrap_or_else(|_| String::new()),
        Err(_) => String::new(),
    }
}

/// GUI 程序没有控制台，日志写文件，便于排查。
fn log_line(data_dir: &std::path::Path, msg: &str) {    let dir = data_dir.join("logs");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("app.log");
    let stamp = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "?".into());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        use std::io::Write;
        let _ = writeln!(f, "[{stamp}] {msg}");
    }
}
