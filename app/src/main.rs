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
        .with_min_inner_size(LogicalSize::new(880.0, 600.0))
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
                }),
            )
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

/// GUI 程序没有控制台，日志写文件，便于排查。
fn log_line(data_dir: &std::path::Path, msg: &str) {
    let dir = data_dir.join("logs");
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
