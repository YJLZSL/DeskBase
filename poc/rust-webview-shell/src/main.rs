//! DeskBase 技术选型 PoC —— 系统 WebView 方案（tao + wry）
//!
//! 目的：测出「Rust + 系统 WebView2」这条路线的真实开销。
//! 用 tao（窗口）+ wry（WebView2 宿主），即 Tauri 的底层，但不引入 Tauri CLI 与前端构建链，
//! 这样测出来的就是外壳自身成本。
//!
//! 两种模式：
//!   --mode cold （默认）  页面加载完成时写出 ready.txt，内容是 main 入口到该时刻的毫秒数
//!   --mode fps            页面里跑 30 秒滚动帧率测试，通过 IPC 回传结果，写 fps.json
//!
//! 外部计时（spawn 到 ready.txt 出现）由 Node 脚本负责，因为它才能反映进程创建开销。

use std::path::PathBuf;
use std::time::Instant;

use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop},
    window::WindowBuilder,
};
use wry::{PageLoadEvent, WebViewBuilder};

fn arg_value(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn main() -> wry::Result<()> {
    // 尽可能早地取时间基准
    let t0 = Instant::now();

    let args: Vec<String> = std::env::args().collect();
    let mode = arg_value(&args, "--mode").unwrap_or_else(|| "cold".to_string());
    let out_dir = PathBuf::from(
        arg_value(&args, "--out-dir")
            .unwrap_or_else(|| std::env::temp_dir().to_string_lossy().into_owned()),
    );
    let _ = std::fs::create_dir_all(&out_dir);

    // 清掉上一次的标记，避免旧文件干扰本轮测量
    let _ = std::fs::remove_file(out_dir.join("ready.txt"));
    let _ = std::fs::remove_file(out_dir.join("fps.json"));

    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title("DeskBase PoC · WebView shell")
        .with_inner_size(LogicalSize::new(1200.0, 800.0))
        .build(&event_loop)
        .expect("创建窗口失败");

    let html = include_str!("index.html");

    // IPC：页面把帧率测试结果回传，这里落盘
    let ipc_out = out_dir.clone();
    let ipc_handler = move |req: wry::http::Request<String>| {
        let _ = std::fs::write(ipc_out.join("fps.json"), req.body().clone());
    };

    // 页面加载完成 = 窗口可见且内容已渲染，作为「可交互」的判定点
    let load_out = out_dir.clone();
    let load_handler = move |ev: PageLoadEvent, _url: String| {
        if let PageLoadEvent::Finished = ev {
            let ms = t0.elapsed().as_millis();
            let _ = std::fs::write(load_out.join("ready.txt"), format!("{}", ms));
        }
    };

    let _webview = WebViewBuilder::new()
        .with_initialization_script(format!("window.__MODE = '{}';", mode))
        .with_html(html)
        .with_ipc_handler(ipc_handler)
        .with_on_page_load_handler(load_handler)
        .build(&window)?;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        if let Event::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } = event
        {
            *control_flow = ControlFlow::Exit;
        }
    });
}
