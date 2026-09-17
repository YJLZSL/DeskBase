//! 构建期工具：把 `app/ui/brand/icon.svg` 光栅化成各尺寸 PNG。
//!
//! ## 为什么这件事放在应用里做
//!
//! 生成图标需要把 SVG 渲染成位图。本机没有 ImageMagick、没有 Python、没有 sharp；
//! 系统自带的 Edge **无法以 headless 方式工作**（实测：只要用户已经开着 Edge，
//! `--headless --screenshot` 就会被现有会话接管，进程 70ms 内退出且不产出任何文件，
//! 换 profile、换 `--headless=old`、关沙箱都一样）。
//!
//! 而 WebView2 是这个项目已经跑通的东西。借它来光栅化：零额外依赖，且结果
//! 与真实运行时（`app/ui/` 的界面）用的是同一个渲染引擎。
//!
//! ## 怎么触发
//!
//! ```text
//! set DESKBASE_RENDER=<输出目录> && deskbase.exe
//! ```
//!
//! 正常启动完全不会走到这里 —— `main` 只在环境变量存在时才调用本模块。
//! 由 `tools/icon/build-icons.cjs` 负责调用与打包。
//!
//! ## 为什么日志写文件而不是 stdout
//!
//! 这是 GUI 子系统程序（`windows_subsystem = "windows"`），没有控制台，
//! `println!` 无处可去。所以进度写进输出目录的 `render.log`，由打包脚本读取。

use std::path::{Path, PathBuf};

use tao::{
    dpi::LogicalSize,
    event_loop::{ControlFlow, EventLoop},
    window::WindowBuilder,
};
use wry::WebViewBuilder;

/// 光栅化页面的 HTML。占位符：
///   `{{SVG}}`  —— 图标源文件（JSON 字符串）
///   `{{FONT}}` —— 得意黑的 WOFF2，base64（SVG 当图片渲染时加载不了外部字体，
///                 所以印文由 canvas 的 fillText 重画一遍，字体得先喂进页面）
///
/// 页面逻辑：
///   1. 摘掉 SVG 里的 `<text id="db-glyph">`，避免被光栅化成回退字形
///   2. SVG 包成 data URI 交给 `<img>`（自包含，不会污染 canvas）
///   3. 逐尺寸画进 canvas，再按 SVG 里完全相同的变换把「库」字画上去
///   4. 取 PNG data URL 与原始 RGBA 回传；每个尺寸校验四角透明
const PAGE: &str = r#"<!DOCTYPE html>
<html lang="zh-CN"><head><meta charset="utf-8"><title>icon render</title>
<style>
  html,body{margin:0;background:#F6F2E9;font:12px/1.6 monospace;color:#2B2823}
</style>
</head><body><pre id="log" style="margin:8px"></pre>
<script>
(function () {
  var SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256, 512];
  var SVG = {{SVG}};
  var GLYPH = '库';
  var FONT = '400 286px "Smiley Sans Oblique"';
  var FONT_URL = 'url(data:font/woff2;base64,{{FONT}}) format("woff2")';

  function post(cmd, args) {
    try { window.ipc.postMessage(JSON.stringify({ cmd: cmd, args: args || {} })); } catch (e) {}
  }
  function say(s) {
    document.getElementById('log').textContent += s + "\n";
    post('render.log', { msg: String(s) });
  }
  window.onerror = function (m, src, line) { post('render.fail', { error: 'onerror: ' + m + ' @' + line }); };
  window.addEventListener('unhandledrejection', function (e) {
    post('render.fail', { error: 'unhandled: ' + String((e.reason && e.reason.message) || e.reason) });
  });

  function bytesToBase64(u8) {
    var s = '';
    for (var i = 0; i < u8.length; i++) s += String.fromCharCode(u8[i]);
    return btoa(s);
  }
  function sealScale(size) { return size <= 24 ? 1.38 : 1; }

  /* 按 SVG 里 <g id="db-seal"> 的 transform 逐条复刻，顺序必须一致：
       translate(512 508) scale(sc) translate(-512 -508) rotate(-3 512 508)  */
  function drawGlyph(ctx, sc) {
    ctx.save();
    ctx.translate(512, 508);
    ctx.scale(sc, sc);
    ctx.translate(-512, -508);
    ctx.translate(512, 508);
    ctx.rotate(-3 * Math.PI / 180);
    ctx.translate(-512, -508);
    ctx.font = FONT;
    ctx.textAlign = 'center';
    ctx.textBaseline = 'middle';
    ctx.fillStyle = '#FFFDF8';
    ctx.fillText(GLYPH, 512, 516);
    ctx.restore();
  }

  async function render(size) {
    var sc = sealScale(size);
    // 摘掉 <text>：SVG 当图片渲染时不加载外部字体，留着会画成回退字形
    var svg = SVG.replace(/<text id="db-glyph"[\s\S]*?<\/text>/, '');
    if (svg === SVG) throw new Error('icon.svg 里找不到 <text id="db-glyph">，无法摘除');
    var marker = 'id="db-seal" transform="rotate(-3 512 508)"';
    if (sc !== 1) {
      if (svg.indexOf(marker) < 0) throw new Error('icon.svg 里找不到 db-seal 的 transform 标记');
      svg = svg.replace(marker,
        'id="db-seal" transform="translate(512 508) scale(' + sc + ') translate(-512 -508) rotate(-3 512 508)"');
    }

    var img = new Image();
    img.src = 'data:image/svg+xml;charset=utf-8,' + encodeURIComponent(svg);
    await img.decode();

    var S = size / 1024;
    var cv = document.createElement('canvas');
    cv.width = cv.height = size;
    var ctx = cv.getContext('2d');
    ctx.setTransform(S, 0, 0, S, 0, 0);   // 之后坐标即 SVG 的 1024 空间
    ctx.clearRect(0, 0, 1024, 1024);
    ctx.drawImage(img, 0, 0, 1024, 1024);
    drawGlyph(ctx, sc);

    var px = ctx.getImageData(0, 0, size, size).data;
    function alpha(x, y) { return px[(y * size + x) * 4 + 3]; }
    var corners = [alpha(0, 0), alpha(size - 1, 0), alpha(0, size - 1), alpha(size - 1, size - 1)];
    if (corners.some(function (a) { return a > 8; })) {
      throw new Error(size + 'px 四角不透明（' + corners.join(',') + '）：背景没透明');
    }

    post('render.save', { name: 'icon-' + size + '.png', data: cv.toDataURL('image/png') });
    if (size === 256) post('render.save', { name: 'icon-rgba-256.bin', data: bytesToBase64(px) });
    say('  ' + size + 'px  ok');
  }

  (async function () {
    try {
      say('脚本已启动');
      document.fonts.add(new FontFace('Smiley Sans Oblique', FONT_URL, { weight: '400' }));
      say('FontFace 已注册，开始加载');
      await document.fonts.load(FONT, GLYPH);
      if (!document.fonts.check(FONT, GLYPH)) throw new Error('得意黑没加载成功，印文会不对');
      say('字体就绪');
      for (var i = 0; i < SIZES.length; i++) await render(SIZES[i]);
      say('全部完成，共 ' + (SIZES.length + 1) + ' 个文件');
      post('render.done');
    } catch (e) {
      post('render.fail', { error: String((e && e.message) || e) });
    }
  })();
})();
</script></body></html>
"#;

/// 进度与错误写文件 —— GUI 程序没有控制台，stdout 无处可去。
fn log_to(out_dir: &Path, msg: &str) {
    let _ = std::fs::create_dir_all(out_dir);
    let path = out_dir.join("render.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        use std::io::Write;
        let _ = writeln!(f, "{msg}");
    }
}

pub fn run(out_dir: &PathBuf, _data_dir: &Path) -> wry::Result<()> {
    let _ = std::fs::remove_file(out_dir.join("render.log"));
    log_to(out_dir, &format!("光栅化输出目录：{}", out_dir.display()));

    let svg = include_str!("../ui/brand/icon.svg");
    // 用 JSON 编码塞进 JS —— 避免 SVG 里的引号、反斜杠、换行把脚本打断
    let svg_json = serde_json::to_string(svg).unwrap_or_else(|_| "\"\"".into());
    // 字体编进页面：SVG 当图片渲染时加载不了外部字体，印文得由 canvas 重画
    let font_b64 = b64_encode(include_bytes!("../ui/fonts/smiley-sans-oblique.woff2"));
    let html = PAGE
        .replace("{{SVG}}", &svg_json)
        .replace("{{FONT}}", &font_b64);
    log_to(out_dir, &format!("页面大小 {} 字节（含字体 base64）", html.len()));

    let event_loop = EventLoop::new();
    // 隐藏窗口：canvas 绘制不需要窗口可见，也避免打扰正在用电脑的人
    let window = WindowBuilder::new()
        .with_title("DeskBase 图标光栅化")
        .with_inner_size(LogicalSize::new(360.0, 240.0))
        .with_visible(false)
        .build(&event_loop)
        .expect("创建窗口失败");

    let out = out_dir.clone();
    let (tx, rx) = std::sync::mpsc::channel::<i32>();
    let tx = std::sync::Mutex::new(tx);

    let handler = move |req: wry::http::Request<String>| {
        let v: serde_json::Value = match serde_json::from_str(req.body()) {
            Ok(v) => v,
            Err(e) => {
                log_to(&out, &format!("请求格式错误：{e}"));
                return;
            }
        };
        let cmd = v["cmd"].as_str().unwrap_or("");
        match cmd {
            "render.save" => {
                let name = v["args"]["name"].as_str().unwrap_or("");
                let data = v["args"]["data"].as_str().unwrap_or("");
                let path = out.join(name);
                if name.ends_with(".png") {
                    write_data_url(&path, data);
                } else {
                    // 原始 RGBA：base64 解码后原样写出
                    if let Some(bytes) = b64_decode(data) {
                        let _ = std::fs::write(&path, &bytes);
                        log_to(&out, &format!("写出 {name}  {} 字节", bytes.len()));
                    }
                }
            }
            "render.log" => {
                log_to(&out, &format!("  [页面] {}", v["args"]["msg"].as_str().unwrap_or("")));
            }
            "render.done" => {
                log_to(&out, "完成");
                if let Ok(t) = tx.lock() {
                    let _ = t.send(0);
                }
            }
            "render.fail" => {
                log_to(&out, &format!("失败：{}", v["args"]["error"].as_str().unwrap_or("?")));
                if let Ok(t) = tx.lock() {
                    let _ = t.send(1);
                }
            }
            other => log_to(&out, &format!("未知命令：{other}")),
        }
    };

    let _webview = WebViewBuilder::new()
        .with_html(html)
        .with_ipc_handler(handler)
        .build(&window)?;

    // 事件循环必须真的跑起来：WebView2 的 IPC 回调要靠它所在的线程泵消息，
    // 如果这里只是阻塞等待，回调永远不触发（这个坑踩过一次）。
    // 所以用短周期 WaitUntil 轮询结果通道，顺便做超时兜底。
    //
    // 注意 `EventLoop::run` 的返回类型是 `!`（永不返回），所以退出必须发生在
    // 闭包内部 —— 写在循环后面是死代码。
    let started = std::time::Instant::now();
    let timeout_dir = out_dir.clone();
    event_loop.run(move |_event, _, control_flow| {
        if let Ok(c) = rx.try_recv() {
            std::process::exit(c);
        }
        if started.elapsed() > std::time::Duration::from_secs(60) {
            log_to(&timeout_dir, "超时：60 秒内没有拿到结果");
            std::process::exit(2);
        }
        *control_flow = ControlFlow::WaitUntil(
            std::time::Instant::now() + std::time::Duration::from_millis(80),
        );
    });
}

fn write_data_url(path: &Path, data_url: &str) {
    let b64 = data_url.split(',').nth(1).unwrap_or("");
    match b64_decode(b64) {
        Some(bytes) => {
            let _ = std::fs::write(path, &bytes);
            log_to(path.parent().unwrap_or(Path::new(".")),
                   &format!("写出 {}  {} 字节", path.file_name().unwrap_or_default().to_string_lossy(), bytes.len()));
        }
        None => log_to(path.parent().unwrap_or(Path::new(".")),
                       &format!("base64 解码失败：{}", path.display())),
    }
}

/// 最小 base64 编码（标准字母表 + `=` 补位）。
fn b64_encode(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// 最小 base64 解码（标准字母表 + `=` 补位）。不引依赖，够用即可。
fn b64_decode(s: &str) -> Option<Vec<u8>> {    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lut = [255u8; 256];
    for (i, c) in T.iter().enumerate() {
        lut[*c as usize] = i as u8;
    }
    let bytes: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for b in bytes {
        if b == b'=' {
            break;
        }
        let v = lut[b as usize];
        if v == 255 {
            return None;
        }
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}
