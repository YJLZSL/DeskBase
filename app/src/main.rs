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
mod db;
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
/// 目前仓库还没有配置远端（`git remote -v` 为空），所以这里是一个占位值。
/// 仓库公开时把这一行改成真实地址即可，界面上的「待确认」标记会自动消失。
const PROJECT_REPO: &str = "https://github.com/deskbase-app/deskbase";

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

        "audit.tail" => {
            // 给设置页显示的审计摘要：日志文件里"打开外部链接/已拒绝"的行数
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

/// UTC 毫秒 → 本地可读时间。存的是 UTC（项目约定），显示用本地。
fn fmt_ms(ms: i64) -> String {
    match time::OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000) {
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
