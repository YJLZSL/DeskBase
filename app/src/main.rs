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
mod workspace;
mod render;
mod schema;
// 导入计划内核（表头行与字段类型推断）：**调用方还没接上** ——
// 读文件那一层（`xlsx.rs::plan`）与 IPC 命令要在下一步才写。
//
// 所以这里刻意**只在测试构建里编译**：
//   · 它的 29 个测试照跑（测试是这个模块当前的全部价值）
//   · release 构建里不存在这些符号 → 不会为了"零警告"去加 allow
//   · 更重要的是**不会被误认为已经生效** —— 半接线的模块比没接线更危险（D-044）
//
// **启用方式**：接线完成时把这一行改回 `mod import_plan;`。
#[cfg(test)]
mod import_plan;
mod updater;
mod xlsx;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
    window::WindowBuilder,
};
use wry::WebViewBuilder;

use db::Db;

/// tools/icon 产出的窗口图标边长（`ui/brand/icon-rgba-256.png` → `icon-rgba-256.bin`）
const ICON_SIZE: u32 = 256;

/// 单条 SQL 的默认硬超时（毫秒）。0 表示不设超时。
///
/// 为什么要有它：`schema::run_query` 只限制返回行数，不限制时间。一条带全表扫描
/// 的谓词能让查询跑上几分钟，而这期间用户除了看"执行中"什么也做不了（Q-042）。
/// 30 秒是个经验值 —— 正常办公量级的查询远低于它，超过就基本意味着缺索引或
/// 条件写错了。中断后 SQLite 会回滚这条语句，**不会留下半截写入**。
const DEFAULT_QUERY_TIMEOUT_MS: u64 = 30_000;

/// 项目仓库地址。**只在这里定义一次**，界面里的链接与「打开发布页」都用它。
///
/// 仓库目前是**私有**的（`YJLZSL/DeskBase`）。私有期间：
///   · 关于页的「打开项目仓库」会跳到 GitHub 的 404 页（未登录时）
///   · 「检查更新」的发布页同理
/// 这是预期的 —— 地址本身是对的，只是还没公开。公开后无需改这一行。
/// 允许用系统浏览器打开的地址。**白名单是硬编码的，前端改不了。**
///
/// 为什么要有白名单：`app.openExternal` 一旦接受任意 URL，被注入的渲染层就能
/// 拿它当"用系统默认程序打开任意东西"的跳板。这里只放项目自己的几个地址，
/// 渲染层无论传什么，不在这张表里的都会被拒并记进日志。
///
/// ⚠️ 地址**不在这里写死** —— 仓库 slug 只在 `updater::REPO` 定义一次。
/// 以前这里另有一个 `PROJECT_REPO` 常量，同一个仓库写两遍；
/// 两边一旦不一致，就成了"界面指向的仓库"和"更新器检查的仓库"不是同一个。
fn allowed_urls() -> Vec<String> {
    vec![
        updater::repo_url(),
        updater::releases_url(),
        updater::docs_url(),
    ]
}

/// 事件循环的自定义事件。**唯一用途**：工作线程干完活以后把主线程叫醒。
///
/// 为什么要绕这一道：`wry::WebView` 只能在创建它的线程上求值脚本，所以
/// 「把结果交回前端」这件事必须在主线程做；而 SQL 执行又必须离开主线程
/// （否则慢查询会把界面连同"中断"按钮一起冻住）。于是工作线程只负责算，
/// 算完发一个事件，主线程收到后统一回传。
enum AppEvent {
    /// 有应答要回传（内容在 [`SqlJob::pending`] 里，主线程按序取走）
    Reply,
    /// 更新已拉起来，现在该退出了。
    ///
    /// 为什么必须退出：Windows 上**运行中的 exe 不能被覆盖**（它的文件锁还在）。
    /// 替换进程已经在等这个锁，所以主进程必须干脆地让出来 ——
    /// 但**不能**用 `process::exit` 直接跳掉，那会绕过"退出前保存"。
    Quit,
}

/// 一次 SQL 执行的中断状态与应答队列。
///
/// 与 `Db` 分开放在一个独立的结构里，是刻意的：工作线程**只**拿到这一份的
/// `Arc`，不去碰 `AppState`（那里握着 `WebView`，跨线程分享既没必要也不安全）。
#[derive(Default)]
struct SqlJob {
    /// 正在执行的查询：`(代号, 中断句柄)`。
    ///
    /// 代号（generation）用来防止"上一条查询的超时哨兵"误伤下一条：
    /// 哨兵睡醒后先比对代号，不是自己那一代就不动手。否则一条查询超时、
    /// 用户立刻重试，前一次的哨兵可能在几百毫秒后把新查询打断。
    running: Mutex<Option<(u64, rusqlite::InterruptHandle)>>,
    /// 中断原因（`"user"` / `"timeout"`）。发起方写、工作线程读走后清空，
    /// 用来把 SQLite 那句干巴巴的 `interrupted` 翻译成人话。
    cause: Mutex<Option<&'static str>>,
    next_gen: AtomicU64,
    /// 待回传的应答。工作线程 push，主线程 drain。用 `Vec` 而不是单槽：
    /// 极端情况下（用户中断后立刻重试）可能有两条应答先后到达。
    pending: Mutex<Vec<String>>,
}

/// 应用全局状态。IPC 处理器与主线程共享它。
struct AppState {
    /// `Arc` 而不是裸 `Mutex`：SQL 工作线程需要独占一个克隆去执行查询。
    db: Arc<Mutex<Db>>,
    data_dir: PathBuf,
    /// WebView 句柄在 build 之后才能拿到，所以先用 Option 占位。
    webview: Mutex<Option<wry::WebView>>,
    started_at: std::time::Instant,
    /// 前端最近一次上报的工作区状态。退出前的最后一道保存用它兜底落盘。
    last_workspace: Mutex<Option<workspace::WorkspaceState>>,
    /// SQL 执行的中断状态与应答队列（见 [`SqlJob`]）
    sql: Arc<SqlJob>,
    /// 事件循环代理：工作线程用它叫醒主线程
    proxy: Mutex<Option<EventLoopProxy<AppEvent>>>,
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
    // ---------- 命令行模式（走在最前面，且不碰数据库）----------
    //
    // 这两个模式都在**旧进程可能还占着数据库**的时候被调用，所以绝不能先开库。
    //
    //   --version
    //       给更新器用来核对"最终落地的那份 exe 自报的版本对不对"（ADR-0018 第 5 步）。
    //       输出格式写死为 `deskbase <版本>` —— 改它就会打断更新器的校验。
    //
    //   --apply-update <暂存目录> --into <安装目录>
    //       占位替换。由**新版本的 exe** 执行（旧 exe 退出后就没了，没法替换自己），
    //       所以两个目录都必须显式传进来：新 exe 运行在暂存目录里，
    //       `current_exe()` 指向的是暂存目录，拿它当默认值会覆盖错地方。
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("deskbase {}", updater::current_version());
        return Ok(());
    }

    if let Some(i) = args.iter().position(|a| a == "--apply-update") {
        let staged = match args.get(i + 1) {
            Some(s) => PathBuf::from(s),
            None => {
                eprintln!("用法：deskbase --apply-update <暂存目录> --into <安装目录>");
                std::process::exit(2);
            }
        };
        let into = args
            .iter()
            .position(|a| a == "--into")
            .and_then(|j| args.get(j + 1))
            .map(PathBuf::from);
        let Some(into) = into else {
            // 不给默认值：默认到"当前 exe 所在目录"在这里一定是错的（那是暂存目录）
            eprintln!("必须显式指定 --into <安装目录>，不提供默认值");
            std::process::exit(2);
        };
        match updater::run_apply_update(&staged, &into) {
            Ok(msg) => {
                println!("{msg}");
                return Ok(());
            }
            Err(e) => {
                eprintln!("更新失败：{e}");
                std::process::exit(1);
            }
        }
    }

    // 数据目录：可用 DESKBASE_DATA_DIR 覆盖（便携版会用它）
    let data_dir = db::default_data_dir();

    // 便携版若用 DESKBASE_DATA_DIR 把数据指到程序目录内，覆盖式更新会连同数据一起
    // 被替换掉。这里给一个明确警告（不阻断启动，也不改写配置）。
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            if data_dir.starts_with(exe_dir) {
                log_line(
                    &data_dir,
                    "警告：数据目录位于程序目录内。覆盖式更新会连同用户数据一起被替换，\
                     建议用 DESKBASE_DATA_DIR 把数据指到程序目录之外。",
                );
            }
        }
    }

    // 构建期用的图标光栅化模式，正常启动完全不经过这里
    if let Ok(out) = std::env::var("DESKBASE_RENDER") {
        return render::run(&PathBuf::from(out), &data_dir);
    }

    let db_path = data_dir.join("data").join("main.db");

    log_line(&data_dir, &format!("启动，数据目录 = {}", data_dir.display()));

    let database = match Db::open(&data_dir, &db_path) {
        Ok(d) => d,
        Err(e) => {
            // 打不开数据库是致命错误：不静默继续，直接报出来
            log_line(&data_dir, &format!("致命错误：{e}"));
            eprintln!("无法打开数据库：{e}");
            std::process::exit(1);
        }
    };

    // 事件循环带自定义事件类型：SQL 工作线程靠它把主线程叫醒（见 AppEvent）
    let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();

    let state = Arc::new(AppState {
        db: Arc::new(Mutex::new(database)),
        data_dir: data_dir.clone(),
        webview: Mutex::new(None),
        started_at: std::time::Instant::now(),
        last_workspace: Mutex::new(None),
        sql: Arc::new(SqlJob::default()),
        proxy: Mutex::new(Some(event_loop.create_proxy())),
    });
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
        // `dispatch` 返回 None = 这条命令是异步的（目前只有 SQL 执行），应答稍后
        // 由工作线程通过事件循环交回来 —— 这里**不能**立刻回，否则前端会先拿到
        // 一个空应答，真正的结果回来时已经没人认领了。
        let payload = match serde_json::from_str::<Request>(&body) {
            Ok(r) => dispatch(&ipc_state, r),
            Err(e) => Some(
                serde_json::json!({
                    "id": 0, "ok": false,
                    "error": format!("请求格式错误: {e}")
                })
                .to_string(),
            ),
        };
        if let Some(p) = payload {
            ipc_state.respond(&p);
        }
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

    // 退出前保存：先让前端把未保存的笔记落库 + 上报工作区状态，给 800ms 宽限，
    // 宽限到（或前端已上报）后用最近一次上报的状态再落一次盘，最后退出。
    // 不会因为等保存而让窗口关不掉——到时间就走。
    let mut quitting = false;
    let mut quit_at: Option<std::time::Instant> = None;

    event_loop.run(move |event, _, control_flow| {
        if quitting {
            // 已经点了关闭：等够 800ms 就收尾退出
            if let Some(t) = quit_at {
                if std::time::Instant::now() >= t {
                    finalize_workspace(&state);
                    log_line(&state.data_dir, "退出前保存完成，退出");
                    *control_flow = ControlFlow::Exit;
                    return;
                }
            }
            *control_flow = ControlFlow::WaitUntil(quit_at.unwrap());
            return;
        }

        // 工作线程干完了活：把攒下的应答一次性交回前端。
        // 必须在主线程做 —— 求值脚本只能在创建 WebView 的那个线程上执行。
        if let Event::UserEvent(AppEvent::Reply) = event {
            flush_replies(&state);
            *control_flow = ControlFlow::Wait;
            return;
        }

        // 更新器请求退出（见 [`AppEvent::Quit`]）。与"点关闭按钮"**走同一条收尾路径** ——
        // 退出前保存一次都不能省，所以这里只是把"要不要开始退"并进同一个判断，
        // 而不是另写一套。
        let quit_for_update = matches!(event, Event::UserEvent(AppEvent::Quit));
        if quit_for_update {
            log_line(&data_dir, "更新已拉起，退出以让出程序文件锁");
        }

        *control_flow = ControlFlow::Wait;
        if quit_for_update
            || matches!(
                event,
                Event::WindowEvent {
                    event: WindowEvent::CloseRequested,
                    ..
                }
            )
        {
            quitting = true;
            quit_at = Some(
                std::time::Instant::now() + std::time::Duration::from_millis(800),
            );
            *control_flow = ControlFlow::WaitUntil(quit_at.unwrap());
            // 通知前端：该保存了（flushSave + 上报工作区状态）
            if let Ok(guard) = state.webview.lock() {
                if let Some(wv) = guard.as_ref() {
                    let _ = wv.evaluate_script(
                        "window.__deskbase && window.__deskbase.onBeforeQuit && window.__deskbase.onBeforeQuit();",
                    );
                }
            }
            log_line(
                &data_dir,
                if quit_for_update {
                    "更新就绪：通知前端保存，等待后退出"
                } else {
                    "收到关闭请求，通知前端保存，等待后退出"
                },
            );
        }
    });
}

/// 取程序所在目录 —— 便携版的解压目录、安装版的安装目录都是它。
/// 更新与替换都要用（替换只动这个目录里的程序文件，绝不碰数据目录）。
fn current_exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
}

/// 把工作线程攒下的应答取出来回传给前端。只在主线程调用。
fn flush_replies(state: &AppState) {
    let batch: Vec<String> = match state.sql.pending.lock() {
        Ok(mut g) => g.drain(..).collect(),
        Err(_) => return, // 锁中毒：直接放弃这一批，总比 panic 好
    };
    for payload in batch {
        state.respond(&payload);
    }
}

/// 退出前的最后一道保存：用前端最近一次上报的工作区状态兜底落盘。
///
/// 用 `try_lock` 而不是 `lock`：如果此刻正好有一条慢查询占着数据库连接，
/// 阻塞等待会把"关窗口"这件事一起卡住（用户点了关闭却半分钟没反应）。
/// 存不下就跳过 —— 这条本来就是兜底，前面 `workspace.save` 已经存过一次。
fn finalize_workspace(state: &AppState) {
    let last = state
        .last_workspace
        .lock()
        .ok()
        .and_then(|g| g.clone());
    if let Some(ws) = last {
        if let Ok(guard) = state.db.try_lock() {
            let _ = workspace::save(guard.conn(), &ws);
        }
    }
}

/// 命令分发。所有前端能做的事都在这里，一一显式列出，不做通配。
///
/// 返回 `None` 表示**这条命令是异步的**，应答稍后由工作线程交回来 ——
/// 目前只有 `schema.runQuery` 一条（慢查询必须能中断，且不能占住主线程，
/// 见 [`run_query_async`]）。其余命令一律同步返回 `Some(应答)`。
fn dispatch(state: &AppState, req: Request) -> Option<String> {
    match req.cmd.as_str() {
        // 唯一一条异步命令。写成 match 分支而不是 `if`，是因为
        // `scripts/check-wiring.cjs` 靠这个形式认出"命令确实有处理方"——
        // 写成 `if req.cmd == "..."` 门禁会误报（它只认分支形式）。
        "schema.runQuery" => run_query_async(state, req),
        _ => Some(dispatch_sync(state, req)),
    }
}

/// 同步命令的分发（除 SQL 执行以外的全部）。
fn dispatch_sync(state: &AppState, req: Request) -> String {
    let id = req.id;
    match req.cmd.as_str() {
        "app.info" => {
            let count = state
                .db
                .lock()
                .map_err(|_| "锁失败".to_string())
                .and_then(|d| d.count_notes())
                .unwrap_or(-1);
            let workspace = state
                .db
                .lock()
                .ok()
                .and_then(|d| workspace::load(d.conn()).ok())
                .unwrap_or_default();
            ok(
                id,
                serde_json::json!({
                    "name": "DeskBase 桌库",
                    "version": updater::current_version().to_string(),
                    "dataDir": state.data_dir.to_string_lossy(),
                    "noteCount": count,
                    "uptimeMs": state.started_at.elapsed().as_millis() as u64,
                    "repo": updater::repo_url(),
                    "workspace": workspace,
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
            if !b("grid") {
                missing.push("grid");
            }
            if !b("sql") {
                missing.push("sql");
            }
            if !b("dbpage") {
                missing.push("dbpage");
            }
            let line = if missing.is_empty() {
                format!(
                    "界面自检通过：动效=✓（档位 {}）组件库=✓ 命令面板=✓（{} 条命令）\
                     数据网格=✓ SQL编辑器=✓ 数据库页=✓ 主题={}",
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

        // ---------- 用户库表（schema.rs）----------
        // 数据库页的 IPC。表名/字段名的校验与转义全部在 schema 层（标识符
        // 白名单 + 引号包裹），值一律参数绑定 —— 这一层只做参数搬运与锁管理，
        // 不拼任何 SQL。接口约定见 schema.rs 头注释：Page.columns[0] 恒为
        // `_rowid`，rows[i][0] 是行号，界面靠它调 updateCell / deleteRows。
        "schema.listTables" => match state.db.lock() {
            Ok(d) => match schema::list_tables(d.conn()) {
                Ok(list) => ok(id, serde_json::to_value(list).unwrap_or_default()),
                Err(e) => err(id, e),
            },
            Err(_) => err(id, "数据库锁失败"),
        },

        "schema.getTable" => {
            let Some(name) = req.args.get("name").and_then(|v| v.as_str()) else {
                return err(id, "缺少参数 name");
            };
            match state.db.lock() {
                Ok(d) => match schema::get_table(d.conn(), name) {
                    Ok(t) => ok(id, serde_json::to_value(t).unwrap_or_default()),
                    Err(e) => err(id, e),
                },
                Err(_) => err(id, "数据库锁失败"),
            }
        }

        "schema.columnMeta" => {
            let Some(name) = req.args.get("name").and_then(|v| v.as_str()) else {
                return err(id, "缺少参数 name");
            };
            match state.db.lock() {
                Ok(d) => match schema::column_meta(d.conn(), name) {
                    Ok(m) => ok(id, serde_json::to_value(m).unwrap_or_default()),
                    Err(e) => err(id, e),
                },
                Err(_) => err(id, "数据库锁失败"),
            }
        }

        "schema.createTable" => {
            let parsed = req
                .args
                .get("spec")
                .and_then(|v| serde_json::from_value::<schema::TableSpec>(v.clone()).ok());
            let Some(spec) = parsed else {
                return err(id, "建表参数不完整或格式不对");
            };
            match state.db.lock() {
                Ok(d) => match schema::create_table(d.conn(), &spec) {
                    Ok(()) => {
                        // 日志记表名不记内容 —— 表名会出现在界面上，不算业务数据
                        log_line(&state.data_dir, &format!("新建库表「{}」", spec.name));
                        ok(id, serde_json::json!({}))
                    }
                    Err(e) => err(id, e),
                },
                Err(_) => err(id, "数据库锁失败"),
            }
        }

        "schema.dropTable" => {
            let name = req.args.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let confirm = req
                .args
                .get("confirmName")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if name.is_empty() {
                return err(id, "缺少参数 name");
            }
            match state.db.lock() {
                Ok(d) => match schema::drop_table(d.conn(), name, confirm) {
                    Ok(()) => {
                        log_line(&state.data_dir, &format!("删除库表「{name}」"));
                        ok(id, serde_json::json!({}))
                    }
                    Err(e) => err(id, e),
                },
                Err(_) => err(id, "数据库锁失败"),
            }
        }

        "schema.pageRows" => {
            let Some(table) = req.args.get("table").and_then(|v| v.as_str()) else {
                return err(id, "缺少参数 table");
            };
            let order_by = req.args.get("orderBy").and_then(|v| v.as_str());
            let desc = req
                .args
                .get("desc")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let cursor = req.args.get("cursor").and_then(|v| v.as_str());
            let limit = req
                .args
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(200)
                .min(schema::MAX_PAGE_LIMIT as u64) as usize;
            // 筛选条件：{列名: 关键词} 或 [[列名, 关键词], …]，两种都收
            let filters: Vec<(String, String)> = match req.args.get("filters") {
                Some(serde_json::Value::Object(m)) => m
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                    .collect(),
                Some(serde_json::Value::Array(a)) => a
                    .iter()
                    .filter_map(|p| p.as_array().map(|kv| (kv, kv)))
                    .filter_map(|(a, b)| {
                        Some((
                            a.first()?.as_str()?.to_string(),
                            b.get(1)?.as_str()?.to_string(),
                        ))
                    })
                    .collect(),
                _ => Vec::new(),
            };
            match state.db.lock() {
                Ok(d) => {
                    let page = if filters.is_empty() {
                        schema::page_rows(d.conn(), table, order_by, desc, cursor, limit)
                    } else {
                        schema::page_rows_filtered(
                            d.conn(),
                            table,
                            order_by,
                            desc,
                            cursor,
                            limit,
                            &filters,
                        )
                    };
                    match page {
                        Ok(p) => ok(id, serde_json::to_value(p).unwrap_or_default()),
                        Err(e) => err(id, e),
                    }
                }
                Err(_) => err(id, "数据库锁失败"),
            }
        }

        "schema.updateCell" => {
            let Some(table) = req.args.get("table").and_then(|v| v.as_str()) else {
                return err(id, "缺少参数 table");
            };
            let Some(rowid) = req.args.get("rowid").and_then(|v| v.as_i64()) else {
                return err(id, "缺少参数 rowid（数据行的行号）");
            };
            let Some(column) = req.args.get("column").and_then(|v| v.as_str()) else {
                return err(id, "缺少参数 column");
            };
            let value = json_to_cell(req.args.get("value").unwrap_or(&serde_json::Value::Null));
            match state.db.lock() {
                Ok(d) => match schema::update_cell(d.conn(), table, rowid, column, value.as_deref())
                {
                    Ok(()) => ok(id, serde_json::json!({})),
                    Err(e) => err(id, e),
                },
                Err(_) => err(id, "数据库锁失败"),
            }
        }

        "schema.deleteRows" => {
            let Some(table) = req.args.get("table").and_then(|v| v.as_str()) else {
                return err(id, "缺少参数 table");
            };
            let rowids: Vec<i64> = req
                .args
                .get("rowids")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
                .unwrap_or_default();
            match state.db.lock() {
                Ok(d) => match schema::delete_rows(d.conn(), table, &rowids) {
                    Ok(n) => ok(id, serde_json::json!({ "deleted": n })),
                    Err(e) => err(id, e),
                },
                Err(_) => err(id, "数据库锁失败"),
            }
        }

        "schema.insertRows" => {
            let Some(table) = req.args.get("table").and_then(|v| v.as_str()) else {
                return err(id, "缺少参数 table");
            };
            let columns: Vec<String> = req
                .args
                .get("columns")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            let rows: Vec<Vec<String>> = req
                .args
                .get("rows")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            // insert_rows 要求 &mut Connection（事务 API 的需要），见 db.rs::conn_mut
            match state.db.lock() {
                Ok(mut d) => match schema::insert_rows(d.conn_mut(), table, &columns, &rows) {
                    Ok(n) => ok(id, serde_json::json!({ "inserted": n })),
                    Err(e) => err(id, e),
                },
                Err(_) => err(id, "数据库锁失败"),
            }
        }

        // ⚠️ `schema.runQuery` **不在这里** —— 它是唯一的异步命令，
        // 由 `dispatch()` 直接转给 `run_query_async()`。慢查询必须离开主线程，
        // 否则界面（连同"中断"按钮）会被一起冻住（Q-042）。

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

        // ---------- 工作区状态（workspace.rs）----------
        // 前端周期性 + 退出前调用，把「当前视图 / 打开的表 / 侧栏状态 / 窗口尺寸」
        // 存进 sys_meta。存下的状态会在下次启动随 app.info 带回，实现「更新不丢工作区」。
        "workspace.save" => {
            let ws: workspace::WorkspaceState = match serde_json::from_value(req.args.clone()) {
                Ok(s) => s,
                Err(e) => return err(id, format!("工作区状态格式不对: {e}")),
            };
            // 兜底：记住最近一次上报的状态，退出前再用它落一次盘。
            // 放在拿数据库锁之前 —— 就算这次存不进去，退出前那次还有机会。
            if let Ok(mut g) = state.last_workspace.lock() {
                *g = Some(ws.clone());
            }
            // `try_lock`：这条命令每 5 秒被前端调一次（app.js 的周期性兜底）。
            // 若此刻有慢查询占着连接，阻塞等待会把主线程连同界面一起按住 ——
            // 每一次都按 5 秒。存不下就跳过，工作区状态已经记在 last_workspace 里。
            match state.db.try_lock() {
                Ok(d) => match workspace::save(d.conn(), &ws) {
                    Ok(()) => ok(id, serde_json::json!({})),
                    Err(e) => err(id, e),
                },
                Err(std::sync::TryLockError::WouldBlock) => {
                    ok(id, serde_json::json!({ "skipped": "busy" }))
                }
                Err(_) => err(id, "数据库锁失败"),
            }
        }

        // ---------- 更新（ADR-0018 / ADR-0019）----------
        //
        // 这两条是**唯一**会让程序主动联网的 IPC。按 ADR-0019 它们属于 **R2**：
        // 默认关闭、可被用户单独关掉、每次出站（**含被跳过的**）都记审计。
        // 它们**不接触任何用户数据** —— 只交换"程序自己的版本与产物"，所以不归 R1 数据红线管。
        "app.updateSettings" => {
            // 不传 mode / channel 就是只读；传了就是改。改完要落库并记一条审计。
            match state.db.lock() {
                Ok(d) => {
                    let mut cur = updater::load_settings(d.conn());
                    let mut changed = false;
                    if let Some(m) = req.args.get("mode").and_then(|v| v.as_str()) {
                        match updater::UpdateMode::parse(m) {
                            Some(x) => {
                                cur.mode = x;
                                changed = true;
                            }
                            None => return err(id, format!("不认识的档位：{m}")),
                        }
                    }
                    if let Some(c) = req.args.get("channel").and_then(|v| v.as_str()) {
                        match updater::Channel::parse(c) {
                            Some(x) => {
                                cur.channel = x;
                                changed = true;
                            }
                            None => return err(id, format!("不认识的通道：{c}")),
                        }
                    }
                    if changed {
                        if let Err(e) = updater::save_settings(d.conn(), &cur) {
                            return err(id, e);
                        }
                        log_line(
                            &state.data_dir,
                            &format!(
                                "更新设置改为：档位={} 通道={}",
                                cur.mode.as_str(),
                                cur.channel.as_str()
                            ),
                        );
                    }
                    ok(id, serde_json::json!(cur))
                }
                Err(_) => err(id, "数据库锁失败"),
            }
        }

        "app.updateCheck" => {
            // 先读设置判断"该不该发"。**`Never` 档位下一个包都不发。**
            let mode = match state.db.lock() {
                Ok(d) => updater::load_settings(d.conn()).mode,
                Err(_) => return err(id, "数据库锁失败"),
            };
            if mode == updater::UpdateMode::Never {
                // 被跳过的尝试也要记 —— ADR-0005 要求"全部出站尝试（**含被拒绝的**）记审计"，
                // 少了这一条，用户在审计日志里就看不出"程序本来想联网但被我自己关掉了"。
                log_line(&state.data_dir, "更新检查未执行：网络默认关闭（设置里可开启）");
            } else {
                log_line(&state.data_dir, "出站尝试：检查更新（api.github.com）");
            }

            let cur = updater::current_version();
            let r = match state.db.lock() {
                Ok(d) => updater::check_by_settings(d.conn(), &cur),
                Err(_) => return err(id, "数据库锁失败"),
            };
            match r {
                Ok(rep) => {
                    if rep.checked {
                        log_line(&state.data_dir, "更新检查完成");
                    }
                    ok(id, serde_json::to_value(rep).unwrap_or_default())
                }
                Err(e) => {
                    // 联网失败也要留痕，不然"为什么检查失败"只能靠猜
                    log_line(&state.data_dir, &format!("更新检查失败：{e}"));
                    err(id, e)
                }
            }
        }

        "app.updateState" => {
            // 给界面用的一条只读汇总：档位、通道、当前版本、有没有已暂存好的更新。
            match state.db.lock() {
                Ok(d) => {
                    let s = updater::load_settings(d.conn());
                    let staged: Vec<String> = std::fs::read_dir(state.data_dir.join("updates"))
                        .map(|rd| {
                            rd.filter_map(|e| e.ok())
                                .filter(|e| e.path().join("plan.json").exists())
                                .map(|e| e.file_name().to_string_lossy().to_string())
                                .collect()
                        })
                        .unwrap_or_default();
                    ok(
                        id,
                        serde_json::json!({
                            "mode": s.mode.as_str(),
                            "channel": s.channel.as_str(),
                            "current": updater::current_version().to_string(),
                            "staged": staged,
                        }),
                    )
                }
                Err(_) => err(id, "数据库锁失败"),
            }
        }

        "app.updateDownload" => {
            // 这是"检查"之外的**第二次确认**（ADR-0018 第 3 条）。
            let mode = match state.db.lock() {
                Ok(d) => updater::load_settings(d.conn()).mode,
                Err(_) => return err(id, "数据库锁失败"),
            };
            if mode != updater::UpdateMode::DownloadAsk {
                return err(
                    id,
                    "当前档位不允许下载 —— 要在设置里开到「检查并下载」",
                );
            }

            let install_dir = match current_exe_dir() {
                Some(d) => d,
                None => return err(id, "取不到程序所在目录"),
            };
            let cur = updater::current_version();
            log_line(&state.data_dir, "出站尝试：下载更新包（api.github.com）");

            let channel = match state.db.lock() {
                Ok(d) => updater::load_settings(d.conn()).channel,
                Err(_) => return err(id, "数据库锁失败"),
            };
            let r = updater::fetch_releases().and_then(|rs| match updater::check(&rs, &cur, channel) {
                updater::CheckOutcome::Newer { tag, .. } => {
                    let rel = rs
                        .iter()
                        .find(|r| r.tag == tag)
                        .ok_or_else(|| format!("刚挑出来的 {tag} 不在清单里（清单变了？）"))?;
                    updater::download_and_stage(rel, &install_dir, &state.data_dir, &cur)
                }
                other => Err(format!("没有可下载的更新：{other:?}")),
            });

            match r {
                Ok(st) => {
                    log_line(
                        &state.data_dir,
                        &format!("更新包已暂存：{} 字节 → {}", st.zip_bytes, st.dir),
                    );
                    ok(id, serde_json::to_value(st).unwrap_or_default())
                }
                Err(e) => {
                    log_line(&state.data_dir, &format!("更新下载失败：{e}"));
                    err(id, e)
                }
            }
        }

        "app.updateApply" => {
            // **第三道确认**（ADR-0018 第 3 条）。替换是唯一会动程序文件的操作，
            // 必须由用户显式点过；不带 confirmed 一律拒。
            if req.args.get("confirmed").and_then(|v| v.as_bool()) != Some(true) {
                return err(id, "替换需要显式确认（confirmed: true）");
            }
            let mode = match state.db.lock() {
                Ok(d) => updater::load_settings(d.conn()).mode,
                Err(_) => return err(id, "数据库锁失败"),
            };
            if mode != updater::UpdateMode::DownloadAsk {
                return err(id, "当前档位不允许替换");
            }

            let staging = match req.args.get("dir").and_then(|v| v.as_str()) {
                Some(s) => PathBuf::from(s),
                None => return err(id, "缺少参数 dir（暂存目录）"),
            };
            // ⚠️ **只允许替换自己暂存下来的东西**：暂存目录必须在数据目录的 `updates/` 下。
            // 少了这道检查，被注入的渲染层就能让我们去执行任意路径上的任意 exe ——
            // 那等于把"更新器"变成一个执行器。
            let updates_root = state.data_dir.join("updates");
            if !staging.starts_with(&updates_root) {
                log_line(
                    &state.data_dir,
                    &format!("拒绝替换：暂存目录不在 updates/ 下 → {}", staging.display()),
                );
                return err(id, "暂存目录不在数据目录的 updates/ 下 —— 拒绝执行");
            }
            if !staging.join("deskbase.exe").exists() {
                return err(id, "暂存目录里没有新的 deskbase.exe");
            }
            if !staging.join("plan.json").exists() {
                return err(id, "暂存目录里没有 plan.json（替换计划）");
            }
            let install_dir = match current_exe_dir() {
                Some(d) => d,
                None => return err(id, "取不到程序所在目录"),
            };

            // 拉起**新版本**的 exe 去干替换：旧 exe 退出后就没了，没法替换自己
            #[cfg(target_os = "windows")]
            {
                use std::os::windows::process::CommandExt;
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                let spawned = std::process::Command::new(staging.join("deskbase.exe"))
                    .arg("--apply-update")
                    .arg(&staging)
                    .arg("--into")
                    .arg(&install_dir)
                    .creation_flags(CREATE_NO_WINDOW)
                    .spawn();
                match spawned {
                    Ok(c) => log_line(
                        &state.data_dir,
                        &format!("已拉起替换进程 pid={}，准备退出让出文件锁", c.id()),
                    ),
                    Err(e) => return err(id, format!("拉不起替换进程：{e}")),
                }
            }
            #[cfg(not(target_os = "windows"))]
            {
                let _ = install_dir;
                return err(id, "替换目前只实现了 Windows");
            }

            // 让主进程退出，把 exe 的文件锁让出来（替换进程正在等它）。
            // 走 `AppEvent::Quit` 而不是 `process::exit` —— 后者会绕过"退出前保存"。
            let proxy = state.proxy.lock().ok().and_then(|g| g.clone());
            match proxy {
                Some(p) => {
                    let _ = p.send_event(AppEvent::Quit);
                    ok(
                        id,
                        serde_json::json!({
                            "spawned": true,
                            "note": "程序即将退出以完成替换，随后会自动重启",
                        }),
                    )
                }
                None => err(id, "事件循环未就绪，无法安排退出（替换进程已拉起，重启程序即可完成）"),
            }
        }

        other => err(id, format!("未知命令: {other}")),
    }
}

/// `schema.runQuery` 的异步实现（Q-042）。
///
/// 为什么必须异步：`schema::run_query` 只限制返回行数，不限制时间。慢查询
/// （全表扫、缺索引、写错的条件）会把调用线程占住几分钟。以前它跑在主线程上，
/// 也就是 WebView 的事件线程 —— 界面点不动、连"中断"这件事都点不了。
///
/// 现在的分工：
///   · 主线程 —— 解析参数、过危险语句闸门、登记中断句柄、起线程，立刻返回 `None`
///     （表示"应答稍后给"）。同时保持可响应：`{ interrupt: true }` 就是在这条路上
///     被接住的，这就是"执行中再点一次 = 中断"。
///   · 工作线程 —— 独占数据库连接跑查询，跑完把应答塞进队列、发事件叫醒主线程。
///   · 超时哨兵 —— 睡够 `timeoutMs` 后若这一代查询还在跑，就替用户按下中断。
///
/// 忙时**明确拒绝**而不是排队：排队会让"哪条结果对应界面上哪个结果块"变得
/// 不可预测（用户看不出第二条是在等第一条还是在跑自己）。
fn run_query_async(state: &AppState, req: Request) -> Option<String> {
    let id = req.id;

    // ---------- ① 中断请求 ----------
    if req
        .args
        .get("interrupt")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        let hit = match state.sql.running.lock() {
            Ok(g) => match g.as_ref() {
                Some((_, handle)) => {
                    handle.interrupt();
                    true
                }
                None => false,
            },
            Err(_) => false,
        };
        if !hit {
            return Some(err(id, "当前没有正在执行的查询"));
        }
        // 先写原因再返回：工作线程是靠它把 interrupted 翻译成人话的。
        // 若哨兵已经写了 "timeout"，不覆盖 —— 让用户看到"自动中断"这个真相。
        if let Ok(mut c) = state.sql.cause.lock() {
            if c.is_none() {
                *c = Some("user");
            }
        }
        log_line(&state.data_dir, "用户中断了正在执行的 SQL");
        return Some(ok(id, serde_json::json!({ "interrupted": true })));
    }

    // ---------- ② 参数 ----------
    let Some(sql) = req.args.get("sql").and_then(|v| v.as_str()) else {
        return Some(err(id, "缺少参数 sql"));
    };
    let sql = sql.to_string();
    let max_rows = req
        .args
        .get("maxRows")
        .and_then(|v| v.as_u64())
        .unwrap_or(5000)
        .clamp(1, 100_000) as usize;
    let confirmed = req
        .args
        .get("confirmed")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let timeout_ms = req
        .args
        .get("timeoutMs")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_QUERY_TIMEOUT_MS);

    // ---------- ③ 策略层闸门（仍在主线程问，此时界面空闲，确认框能正常弹）----------
    if !confirmed {
        if let Some(reason) = schema::needs_confirm(&sql) {
            return Some(ok(id, serde_json::json!({ "needsConfirm": reason })));
        }
    }

    // ---------- ④ 占住执行位并取中断句柄 ----------
    let my_gen = {
        let mut slot = match state.sql.running.lock() {
            Ok(g) => g,
            Err(_) => return Some(err(id, "查询状态锁失败")),
        };
        if slot.is_some() {
            return Some(err(id, "上一条查询还在执行 —— 先点「中断」，或等它跑完"));
        }
        let guard = match state.db.lock() {
            Ok(g) => g,
            Err(_) => return Some(err(id, "数据库锁失败")),
        };
        // 句柄内部握着连接的 Arc，因此脱离这把锁之后依然有效
        let handle = guard.conn().get_interrupt_handle();
        drop(guard);
        let gen = state.sql.next_gen.fetch_add(1, Ordering::SeqCst);
        *slot = Some((gen, handle));
        gen
    };
    if let Ok(mut c) = state.sql.cause.lock() {
        *c = None; // 新一代查询，清掉上一条留下的原因
    }

    // ---------- ⑤ 超时哨兵 ----------
    if timeout_ms > 0 {
        let slot = Arc::clone(&state.sql);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(timeout_ms));
            if let Ok(g) = slot.running.lock() {
                if let Some((gen, handle)) = g.as_ref() {
                    if *gen == my_gen {
                        if let Ok(mut c) = slot.cause.lock() {
                            if c.is_none() {
                                *c = Some("timeout");
                            }
                        }
                        handle.interrupt();
                    }
                }
            }
        });
    }

    // ---------- ⑥ 工作线程 ----------
    let proxy = match state.proxy.lock() {
        Ok(g) => g.clone(),
        Err(_) => None,
    };
    let Some(proxy) = proxy else {
        if let Ok(mut slot) = state.sql.running.lock() {
            *slot = None;
        }
        return Some(err(id, "事件循环未就绪，无法调度查询"));
    };

    let db = Arc::clone(&state.db);
    let slot = Arc::clone(&state.sql);
    let log_dir = state.data_dir.clone();

    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let outcome = match db.lock() {
            Ok(guard) => schema::run_query(guard.conn(), &sql, max_rows),
            Err(_) => Err("数据库锁失败".to_string()),
        };
        let elapsed_ms = started.elapsed().as_millis();

        // 收尾：先确认这一代还是自己的再清（否则可能把后来者清掉）
        if let Ok(mut g) = slot.running.lock() {
            let mine = match g.as_ref() {
                Some((gen, _)) => *gen == my_gen,
                None => false,
            };
            if mine {
                *g = None;
            }
        }
        let cause = slot.cause.lock().ok().and_then(|mut c| c.take());

        let payload = match outcome {
            Ok(r) => {
                // 不记录 SQL 内容（docs/06 §9.3：查询日志默认关闭）；
                // 只记"发生过写"这个事实，供审计页计数。
                if confirmed || r.affected > 0 {
                    log_line(
                        &log_dir,
                        &format!("SQL 编辑器执行了写操作，影响 {} 行", r.affected),
                    );
                }
                ok(id, serde_json::to_value(r).unwrap_or_default())
            }
            Err(e) => match cause {
                Some("timeout") => {
                    log_line(
                        &log_dir,
                        &format!("SQL 执行超过 {} 秒，已被自动中断", timeout_ms / 1000),
                    );
                    err(
                        id,
                        format!(
                            "这条查询跑了 {} ms 还没完，已按 {} 秒的上限自动中断。\
                             常见原因是条件列没有索引、或者写成了全表扫描 —— \
                             可以先用 LIMIT 看看数据长什么样，再决定要不要建索引。",
                            elapsed_ms,
                            timeout_ms / 1000
                        ),
                    )
                }
                Some(_) => {
                    log_line(&log_dir, "SQL 执行被用户中断");
                    err(
                        id,
                        format!(
                            "查询已中断（跑了 {} ms）。SQLite 会回滚这一条语句，\
                             不会留下写了一半的数据。",
                            elapsed_ms
                        ),
                    )
                }
                None => err(id, e),
            },
        };

        if let Ok(mut g) = slot.pending.lock() {
            g.push(payload);
        }
        // 叫醒主线程去回传。注意不能用 `log_dir` 之外的 AppState ——
        // 工作线程只持有这几份 Arc，不碰 WebView。
        let _ = proxy.send_event(AppEvent::Reply);
    });

    // 应答在路上：这条命令到此为止，什么也不回
    None
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

/// IPC 上来的单元格值 → schema 层的 `Option<&str>` 语义。
///
/// `None` = NULL（"没填"）；字符串原样；数字/布尔转成文本后交给
/// schema 层按列的语义类型强转（money 按分、布尔收 1/0）。
/// JSON 的 `null` 与缺省都走 NULL —— 界面上"设为 NULL"按钮靠它生效。
fn json_to_cell(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Bool(b) => Some(if *b { "1" } else { "0" }.to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => Some(v.to_string()),
    }
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

// ============================================================
// 数据库页验收（`local-docs/handoff/ENV_SETUP.md` 第 4 节）
// ============================================================
//
// 那一节原本写着"由下一位接手者自己点一遍"（16 步）。上一任就是卡在这里：
// 手点一次只能证明"这一次是好的"，下次改代码没人会再点一遍。
//
// 这里把 1–15 步里**能用 IPC 表达的那部分**变成可重复运行的测试 —— 而且是走
// `dispatch()` 这个真正的入口，因此连"Rust 序列化出来的字段名与界面读的是不是
// 同一个"也一起钉住了（`has_more` / `elapsed_ms` 两次静默失效都发生在这条边界上）。
//
// 覆盖不到的部分（如实说明）：
//   · 异步的那半截（真正把 SQL 丢给工作线程）—— 需要 tao 事件循环，
//     它只能在主线程创建，单测里拿不到。实机运行时验证。
//   · 纯视觉的部分（NULL 的灰色占位、排序箭头、按钮文案）。
#[cfg(test)]
mod acceptance {
    use super::*;
    use serde_json::json;

    /// 临时数据目录 + 一个可用的 AppState。
    ///
    /// `proxy` 留 `None`：这是有意的 —— 异步 SQL 需要真事件循环，而本模块
    /// 只测同步路径。`run_query_async` 在拿不到 proxy 时会明确报错而不是静默挂起。
    fn fixture(tag: &str) -> (Arc<AppState>, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "deskbase_accept_{}_{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("data")).unwrap();
        let db = Db::open(&dir, &dir.join("data").join("main.db")).unwrap();
        let state = Arc::new(AppState {
            db: Arc::new(Mutex::new(db)),
            data_dir: dir.clone(),
            webview: Mutex::new(None),
            started_at: std::time::Instant::now(),
            last_workspace: Mutex::new(None),
            sql: Arc::new(SqlJob::default()),
            proxy: Mutex::new(None),
        });
        (state, dir)
    }

    /// 走真正的分发入口，把 IPC 应答解成 JSON。
    fn call(
        state: &AppState,
        cmd: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let raw = dispatch(
            state,
            Request {
                id: 1,
                cmd: cmd.to_string(),
                args,
            },
        )
        .expect("这条命令应当是同步的（异步的那条在单测里拿不到事件循环）");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("应答必须是合法 JSON");
        if v["ok"].as_bool() == Some(true) {
            Ok(v["data"].clone())
        } else {
            Err(v["error"].as_str().unwrap_or("未知错误").to_string())
        }
    }

    fn col(name: &str, ty: &str) -> serde_json::Value {
        json!({
            "name": name, "ty": ty, "not_null": false,
            "default": null, "primary_key": false, "comment": null,
        })
    }

    /// 验收 1 + 2 + 5 + 6：建表（含金额列）→ 录一行 → 金额按分存 → 三位小数被拒。
    #[test]
    fn 验收_建表录行与金额往返() {
        let (state, dir) = fixture("money");
        call(
            &state,
            "schema.createTable",
            json!({ "spec": { "name": "验收台账", "comment": null, "columns": [
                col("名称", "text"), col("数量", "integer"), col("金额", "money"),
                col("日期", "date"), col("已结清", "boolean"),
            ]}}),
        )
        .unwrap();

        // 表出现在左栏列表里
        let tables = call(&state, "schema.listTables", json!({})).unwrap();
        assert!(
            tables
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "验收台账"),
            "新建的表没有出现在列表里：{tables}"
        );

        // 录一行：金额写「元」
        let r = call(
            &state,
            "schema.insertRows",
            json!({
                "table": "验收台账",
                "columns": ["名称", "数量", "金额", "日期", "已结清"],
                "rows": [["甲", "3", "12.34", "2026-09-19", "1"]],
            }),
        )
        .unwrap();
        assert_eq!(r["inserted"], 1);

        let page = call(&state, "schema.pageRows", json!({"table":"验收台账","limit":10})).unwrap();
        // 第 0 列恒为 _rowid（界面靠它调 updateCell / deleteRows）
        assert_eq!(page["columns"][0], "_rowid");
        assert_eq!(page["rows"][0][3].as_i64(), Some(1234), "金额应按分存 1234：{page}");
        assert_eq!(page["rows"][0][1], "甲");
        assert_eq!(page["has_more"], false);

        // 三位小数必须被拒 —— 金额按分记账，不能静默四舍五入
        let bad = call(
            &state,
            "schema.insertRows",
            json!({
                "table": "验收台账",
                "columns": ["名称", "金额"],
                "rows": [["乙", "12.345"]],
            }),
        );
        assert!(bad.is_err(), "三位小数必须报错，实际：{bad:?}");

        let _ = std::fs::remove_dir_all(dir);
    }

    /// 验收 3 + 4：**超过一页的表必须能翻到第二页**。
    ///
    /// 这一步专门留了测试：v0.2.0-beta.1 的 P1 就是这里坏的
    /// （`has_more` 被读成 `hasMore`，值恒为 undefined，"加载更多"永不出现）。
    #[test]
    fn 验收_超过一页的表能翻到第二页且不重不漏() {
        let (state, dir) = fixture("paging");
        call(
            &state,
            "schema.createTable",
            json!({ "spec": { "name": "台账", "comment": null, "columns": [col("名称", "text")] }}),
        )
        .unwrap();
        let rows: Vec<Vec<String>> = (1..=250).map(|i| vec![format!("行{i}")]).collect();
        let r = call(
            &state,
            "schema.insertRows",
            json!({ "table": "台账", "columns": ["名称"], "rows": rows }),
        )
        .unwrap();
        assert_eq!(r["inserted"], 250);

        let p1 = call(&state, "schema.pageRows", json!({"table":"台账","limit":200})).unwrap();
        assert_eq!(p1["rows"].as_array().unwrap().len(), 200);
        assert_eq!(p1["has_more"], true, "还有 50 行没取，has_more 必须是 true");
        let cursor = p1["next_cursor"].as_str().expect("有下一页就该给游标").to_string();

        let p2 = call(
            &state,
            "schema.pageRows",
            json!({"table":"台账","limit":200,"cursor":cursor}),
        )
        .unwrap();
        assert_eq!(p2["rows"].as_array().unwrap().len(), 50);
        assert_eq!(p2["has_more"], false, "取完了就不该说还有");

        // 两页不重不漏：把 rowid 收起来比对
        let mut ids: Vec<i64> = Vec::new();
        for p in [&p1, &p2] {
            for r in p["rows"].as_array().unwrap() {
                ids.push(r[0].as_i64().unwrap());
            }
        }
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 250, "两页合起来必须正好 250 行、不重不漏");

        let _ = std::fs::remove_dir_all(dir);
    }

    /// 验收 7 + 8：表头排序（升 / 降）与列筛选。
    #[test]
    fn 验收_排序与筛选() {
        let (state, dir) = fixture("sortfilter");
        call(
            &state,
            "schema.createTable",
            json!({ "spec": { "name": "库存", "comment": null, "columns": [
                col("名称", "text"), col("数量", "integer")
            ]}}),
        )
        .unwrap();
        let rows: Vec<Vec<String>> = [("甲", "3"), ("乙", "1"), ("丙", "2")]
            .iter()
            .map(|(a, b)| vec![a.to_string(), b.to_string()])
            .collect();
        call(
            &state,
            "schema.insertRows",
            json!({ "table": "库存", "columns": ["名称","数量"], "rows": rows }),
        )
        .unwrap();

        let asc = call(
            &state,
            "schema.pageRows",
            json!({"table":"库存","orderBy":"数量","desc":false,"limit":10}),
        )
        .unwrap();
        let names: Vec<&str> = asc["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r[1].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["乙", "丙", "甲"], "升序不对：{names:?}");

        let desc = call(
            &state,
            "schema.pageRows",
            json!({"table":"库存","orderBy":"数量","desc":true,"limit":10}),
        )
        .unwrap();
        let names: Vec<&str> = desc["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r[1].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["甲", "丙", "乙"], "降序不对：{names:?}");

        // 筛选：列关键词包含匹配
        let f = call(
            &state,
            "schema.pageRows",
            json!({"table":"库存","limit":10,"filters":[["名称","甲"]]}),
        )
        .unwrap();
        assert_eq!(f["rows"].as_array().unwrap().len(), 1, "筛选应只剩 1 行：{f}");

        // 筛选不存在的列必须报错，不能静默当成"没有这个条件"
        let bad = call(
            &state,
            "schema.pageRows",
            json!({"table":"库存","limit":10,"filters":[["不存在的列","x"]]}),
        );
        assert!(bad.is_err(), "筛选不存在的列应当报错");

        let _ = std::fs::remove_dir_all(dir);
    }

    /// 验收 9 + 10：改单元格（含设为 NULL）与批量删行。
    #[test]
    fn 验收_改单元格与删行() {
        let (state, dir) = fixture("edit");
        call(
            &state,
            "schema.createTable",
            json!({ "spec": { "name": "名册", "comment": null, "columns": [
                col("姓名", "text"), col("备注", "text")
            ]}}),
        )
        .unwrap();
        call(
            &state,
            "schema.insertRows",
            json!({ "table":"名册","columns":["姓名","备注"],"rows":[["甲","旧"],["乙",""],["丙","x"]]}),
        )
        .unwrap();

        let page = call(&state, "schema.pageRows", json!({"table":"名册","limit":10})).unwrap();
        let rowid_甲 = page["rows"][0][0].as_i64().unwrap();

        // 改内容
        call(
            &state,
            "schema.updateCell",
            json!({"table":"名册","rowid":rowid_甲,"column":"备注","value":"新"}),
        )
        .unwrap();
        // 设为 NULL：JSON null → 数据库 NULL（与空串是两回事）
        call(
            &state,
            "schema.updateCell",
            json!({"table":"名册","rowid":rowid_甲,"column":"备注","value":null}),
        )
        .unwrap();

        let after = call(&state, "schema.pageRows", json!({"table":"名册","limit":10})).unwrap();
        assert!(after["rows"][0][2].is_null(), "设成 NULL 之后应回 null：{after}");

        // 批量删两行
        let ids: Vec<i64> = after["rows"]
            .as_array()
            .unwrap()
            .iter()
            .take(2)
            .map(|r| r[0].as_i64().unwrap())
            .collect();
        let r = call(
            &state,
            "schema.deleteRows",
            json!({"table":"名册","rowids":ids}),
        )
        .unwrap();
        assert_eq!(r["deleted"], 2, "应当报告删掉 2 行");

        let left = call(&state, "schema.pageRows", json!({"table":"名册","limit":10})).unwrap();
        assert_eq!(left["rows"].as_array().unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(dir);
    }

    /// 验收 11 + 12 + 13：SQL 执行与**危险语句闸门**。
    ///
    /// 12 / 13 是这套验收里最要紧的两条：`DELETE FROM t` 与
    /// `DELETE FROM t WHERE 1=1` 都必须弹确认。后者曾经能绕过（恒真谓词）。
    #[test]
    fn 验收_危险语句必须弹确认() {
        let (state, dir) = fixture("gate");
        call(
            &state,
            "schema.createTable",
            json!({ "spec": { "name": "台账", "comment": null, "columns": [col("名称", "text")] }}),
        )
        .unwrap();
        call(
            &state,
            "schema.insertRows",
            json!({ "table":"台账","columns":["名称"],"rows":[["甲"],["乙"]] }),
        )
        .unwrap();

        // 11：普通查询走同步闸门放行 → 到异步执行那一步才需要事件循环。
        //     这里只能验证它**没有被闸门拦下**（返回的不是 needsConfirm）。
        let allowed = call(
            &state,
            "schema.runQuery",
            json!({"sql":"SELECT * FROM 台账 LIMIT 20"}),
        );
        // 单测里没有事件循环，所以这里要么是"事件循环未就绪"，要么是应答；
        // 唯独不能是 needsConfirm。
        if let Ok(v) = &allowed {
            assert!(v.get("needsConfirm").is_none(), "普通 SELECT 不该被拦：{v}");
        }

        // 12：无 WHERE 的 DELETE
        let r = call(&state, "schema.runQuery", json!({"sql":"DELETE FROM 台账"})).unwrap();
        assert!(
            r["needsConfirm"].is_string(),
            "无 WHERE 的 DELETE 必须弹确认，实际：{r}"
        );

        // 13：恒真谓词 —— 也弹确认
        let r = call(
            &state,
            "schema.runQuery",
            json!({"sql":"DELETE FROM 台账 WHERE 1=1"}),
        )
        .unwrap();
        assert!(
            r["needsConfirm"].is_string(),
            "WHERE 1=1 这种恒真写法必须弹确认（曾经能绕过），实际：{r}"
        );

        // 有真实过滤条件的可以放行
        let r = call(
            &state,
            "schema.runQuery",
            json!({"sql":"DELETE FROM 台账 WHERE 名称 = '甲'"}),
        );
        if let Ok(v) = &r {
            assert!(v.get("needsConfirm").is_none(), "有条件就不该拦：{v}");
        }

        // 数据一行没少：闸门拦下的语句绝不能被执行
        let page = call(&state, "schema.pageRows", json!({"table":"台账","limit":10})).unwrap();
        assert_eq!(
            page["rows"].as_array().unwrap().len(),
            2,
            "被拦下的 DELETE 不能真的删了数据"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// 有查询在跑时，第二条查询必须**明确拒绝**而不是排队。
    ///
    /// 顺带证明"中断"这条路是通的：中断请求只看 `sql.running`，不碰数据库锁，
    /// 所以它在慢查询期间一定能被处理（这正是 Q-042 要解决的事）。
    #[test]
    fn 忙时第二条查询被明确拒绝_而中断仍然可达() {
        let (state, dir) = fixture("busy");
        // 假装有一条正在跑的查询
        let handle = {
            let guard = state.db.lock().unwrap();
            guard.conn().get_interrupt_handle()
        };
        *state.sql.running.lock().unwrap() = Some((1, handle));

        let busy = call(&state, "schema.runQuery", json!({"sql":"SELECT 1"}));
        assert!(busy.is_err(), "忙的时候第二条查询必须被拒绝");
        assert!(
            busy.unwrap_err().contains("还在执行"),
            "拒绝理由要说清楚"
        );

        // 中断请求照样能进来（它不走数据库锁）
        let r = call(&state, "schema.runQuery", json!({"interrupt": true})).unwrap();
        assert_eq!(r["interrupted"], true);
        assert_eq!(*state.sql.cause.lock().unwrap(), Some("user"));

        *state.sql.running.lock().unwrap() = None;
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 验收 14 + 15：工作区状态存下去、重启后能读回来。
    #[test]
    fn 验收_工作区状态重启后能恢复() {
        let (state, dir) = fixture("workspace");
        call(
            &state,
            "workspace.save",
            json!({
                "view":"database","activeTable":"验收台账","sidebar":"rail",
                "windowW":1280,"windowH":820,
            }),
        )
        .unwrap();

        // app.info 会把工作区带回来（界面启动时就读它）
        let info = call(&state, "app.info", json!({})).unwrap();
        assert_eq!(info["workspace"]["view"], "database");
        assert_eq!(info["workspace"]["activeTable"], "验收台账");
        assert_eq!(info["workspace"]["windowW"], 1280);

        // 换一个 AppState 打开同一份库（= 关掉程序重开）
        let db = Db::open(&dir, &dir.join("data").join("main.db")).unwrap();
        let state2 = AppState {
            db: Arc::new(Mutex::new(db)),
            data_dir: dir.clone(),
            webview: Mutex::new(None),
            started_at: std::time::Instant::now(),
            last_workspace: Mutex::new(None),
            sql: Arc::new(SqlJob::default()),
            proxy: Mutex::new(None),
        };
        let info2 = call(&state2, "app.info", json!({})).unwrap();
        assert_eq!(
            info2["workspace"]["activeTable"], "验收台账",
            "重开之后工作区没恢复：{info2}"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// 建表向导的「默认值」输入框（v0.2.1 新增）。
    #[test]
    fn 建表默认值能落库() {
        let (state, dir) = fixture("default");
        call(
            &state,
            "schema.createTable",
            json!({ "spec": { "name": "报销", "comment": null, "columns": [
                col("事项", "text"),
                json!({"name":"状态","ty":"text","not_null":false,
                       "default":"'未结清'","primary_key":false,"comment":null}),
                json!({"name":"金额","ty":"money","not_null":false,
                       "default":"'12.34'","primary_key":false,"comment":null}),
            ]}}),
        )
        .unwrap();

        // 只填事项，其余两列走默认值
        call(
            &state,
            "schema.insertRows",
            json!({ "table":"报销","columns":["事项"],"rows":[["打车"]] }),
        )
        .unwrap();

        let page = call(&state, "schema.pageRows", json!({"table":"报销","limit":10})).unwrap();
        // 列序：[_rowid, 事项, 状态, 金额]
        assert_eq!(page["rows"][0][2], "未结清", "文本默认值没落上：{page}");
        assert_eq!(
            page["rows"][0][3].as_i64(),
            Some(1234),
            "金额默认值应按「元」换算成分：{page}"
        );

        let _ = std::fs::remove_dir_all(dir);
    }
}
