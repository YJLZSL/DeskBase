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
  /* 两套几何：小尺寸用单独绘制的一版（见 icon-small.svg 的说明）。
     这里把两套都渲染出来，交给 tools/icon 去挑，顺便能出对比图。 */
  var VARIANTS = [
    { prefix: 'icon',       svg: {{SVG}} },
    { prefix: 'icon-small', svg: {{SVG_SMALL}} }
  ];
  var GLYPH = '库';
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

  /* 字形的位置/字号/颜色从 SVG 里读 —— 改 icon.svg 不用动 Rust。
     约定：<text id="db-glyph" x=".." y=".." font-size=".." fill="..">，属性顺序任意。 */
  function glyphSpec(svg) {
    var tag = /<text id="db-glyph"[^>]*>/.exec(svg);
    if (!tag) return null;
    function attr(n) {
      var m = new RegExp('\\b' + n + '="([^"]+)"').exec(tag[0]);
      return m ? m[1] : null;
    }
    var x = parseFloat(attr('x')), y = parseFloat(attr('y')), fs = parseFloat(attr('font-size'));
    if (isNaN(x) || isNaN(y) || isNaN(fs)) return null;
    return { x: x, y: y, size: fs, color: attr('fill') || '#FFFDF8' };
  }

  /* 按 SVG 里 <g id="db-seal"> 的 transform 复刻旋转。
     约定：印面绕 (512,508) 旋转 -3°；没有该分组时不旋转。 */
  function drawGlyph(ctx, spec, rotated) {
    if (rotated) {
      ctx.transform(1, 0, 0, 1, 512, 508);
      ctx.transform(1, 0, 0, 1, -512, -508);
      ctx.rotate(-3 * Math.PI / 180);
      ctx.transform(1, 0, 0, 1, 512, 508);
      ctx.transform(1, 0, 0, 1, -512, -508);
    }
    ctx.font = '400 ' + spec.size + 'px "Smiley Sans Oblique"';
    ctx.textAlign = 'center';
    ctx.textBaseline = 'middle';
    ctx.fillStyle = spec.color;
    ctx.fillText(GLYPH, spec.x, spec.y);
  }

  function build(variant) {
    var svg = variant.svg;
    var spec = glyphSpec(svg);
    if (svg.indexOf('id="db-glyph"') >= 0 && !spec) {
      throw new Error(variant.prefix + '：有 db-glyph 但读不到 x / y / font-size');
    }
    var rotated = svg.indexOf('db-seal') >= 0;
    // 摘掉 <text>：SVG 当图片渲染时不加载外部字体，留着会画成回退字形
    var clean = svg.replace(/<text id="db-glyph"[\s\S]*?<\/text>/, '');
    return { prefix: variant.prefix, clean: clean, spec: spec, rotated: rotated };
  }

  async function renderOne(v, size) {
    var img = new Image();
    img.src = 'data:image/svg+xml;charset=utf-8,' + encodeURIComponent(v.clean);
    await img.decode();

    var S = size / 1024;
    var cv = document.createElement('canvas');
    cv.width = cv.height = size;
    var ctx = cv.getContext('2d');
    ctx.setTransform(S, 0, 0, S, 0, 0);   // 之后坐标即 SVG 的 1024 空间
    ctx.clearRect(0, 0, 1024, 1024);
    ctx.drawImage(img, 0, 0, 1024, 1024);
    if (v.spec) drawGlyph(ctx, v.spec, v.rotated);

    var px = ctx.getImageData(0, 0, size, size).data;
    function alpha(x, y) { return px[(y * size + x) * 4 + 3]; }
    var corners = [alpha(0, 0), alpha(size - 1, 0), alpha(0, size - 1), alpha(size - 1, size - 1)];
    if (corners.some(function (a) { return a > 8; })) {
      throw new Error(v.prefix + ' ' + size + 'px 四角不透明（' + corners.join(',') + '）：背景没透明');
    }
    post('render.save', { name: v.prefix + '-' + size + '.png', data: cv.toDataURL('image/png') });
    if (size === 256 && v.prefix === 'icon') {
      post('render.save', { name: 'icon-rgba-256.bin', data: bytesToBase64(px) });
    }
  }

  (async function () {
    try {
      say('脚本已启动');
      document.fonts.add(new FontFace('Smiley Sans Oblique', FONT_URL, { weight: '400' }));
      await document.fonts.load('400 286px "Smiley Sans Oblique"', GLYPH);
      if (!document.fonts.check('400 286px "Smiley Sans Oblique"', GLYPH)) {
        throw new Error('得意黑没加载成功，印文会不对');
      }
      say('字体就绪');
      var built = VARIANTS.map(build);
      for (var vi = 0; vi < built.length; vi++) {
        for (var si = 0; si < SIZES.length; si++) {
          await renderOne(built[vi], SIZES[si]);
        }
        say('  ' + built[vi].prefix + ' 已渲染 ' + SIZES.length + ' 档');
      }
      say('全部完成');
      post('render.done');
    } catch (e) {
      post('render.fail', { error: String((e && e.message) || e) });
    }
  })();
})();
</script></body></html>
"#;

/// 设计对比用页面（`DESKBASE_RENDER_LAB=<目录>` 时走这条）。
///
/// 把目录下所有 .svg 按多档尺寸渲染到亮/暗两种背景上，拼成一张对照表。
/// 用途：图标改版时一次看清"哪个方案在 16px 下还站得住"。
const PAGE_LAB: &str = r#"<!DOCTYPE html>
<html lang="zh-CN"><head><meta charset="utf-8"><title>icon lab</title>
<style>html,body{margin:0;background:#8a8a8a;font:12px monospace;color:#111}</style>
</head><body><pre id="log" style="margin:6px"></pre>
<script>
(function () {
  var CANDS = {{CANDS}};                 // [{name, svg}]
  var SIZES = [16, 20, 24, 32, 48, 64, 128, 256];
  var GLYPH = '库';
  var FONT = '400 286px "Smiley Sans Oblique"';
  var FONT_URL = 'url(data:font/woff2;base64,{{FONT}}) format("woff2")';
  var PAD = 16, ROWLABEL = 132;

  function post(cmd, args) {
    try { window.ipc.postMessage(JSON.stringify({ cmd: cmd, args: args || {} })); } catch (e) {}
  }
  function say(s) {
    document.getElementById('log').textContent += s + "\n";
    post('render.log', { msg: String(s) });
  }
  window.onerror = function (m, src, l) { post('render.fail', { error: 'onerror: ' + m + ' @' + l }); };

  function stripGlyph(svg) {
    return svg.replace(/<text id="db-glyph"[\s\S]*?<\/text>/, '');
  }
  /* 字形的位置与字号从 SVG 里读 —— 改 icon.svg 不需要动 Rust。
     写法约定：<text id="db-glyph" x=".." y=".." font-size="..">（属性顺序任意） */
  function glyphSpec(svg) {
    var tag = /<text id="db-glyph"[^>]*>/.exec(svg);
    if (!tag) return null;
    function attr(n) {
      var m = new RegExp('\\b' + n + '="([-\\d.]+)"').exec(tag[0]);
      return m ? parseFloat(m[1]) : null;
    }
    var x = attr('x'), y = attr('y'), fs = attr('font-size');
    if (x === null || y === null || fs === null) return null;
    return { x: x, y: y, size: fs };
  }

  var GS = glyphSpec(SVG);
  if (SVG.indexOf('id="db-glyph"') >= 0 && !GS) {
    throw new Error('icon.svg 里有 db-glyph 但读不到 x / y / font-size');
  }
  var GLYPH_COLOR = (function () {
    var m = /<text id="db-glyph"[^>]*fill="([^"]+)"/.exec(SVG);
    return m ? m[1] : '#FFFDF8';
  })();

  /* 按 SVG 里 <g id="db-seal"> 的 transform 逐条复刻。
     约定：印面整体绕 (512,508) 旋转 -3°，字形用 SVG 里声明的 x/y/font-size。
     没有 db-seal 分组时（满版底那类设计）就直接按原点画。 */
  function drawGlyph(ctx) {
    if (!GS) return;
    var hasSeal = SVG.indexOf('db-seal') >= 0;
    if (hasSeal) {
      ctx.transform(1, 0, 0, 1, 512, 508);
      ctx.transform(1, 0, 0, 1, -512, -508);
      ctx.rotate(-3 * Math.PI / 180);
      ctx.transform(1, 0, 0, 1, 512, 508);
      ctx.transform(1, 0, 0, 1, -512, -508);
    }
    ctx.font = '400 ' + GS.size + 'px "Smiley Sans Oblique"';
    ctx.textAlign = 'center';
    ctx.textBaseline = 'middle';
    ctx.fillStyle = GLYPH_COLOR;
    ctx.fillText(GLYPH, GS.x, GS.y);
  }

  // 把某个 candidate 渲染成一张 size×size 的离屏 canvas
  function raster(cand, size, glyphScale) {
    return new Promise(async function (resolve) {
      var src = stripGlyph(cand.svg);
      if (glyphScale && glyphScale !== 1 && src.indexOf('db-seal') < 0) {
        // 没有印面分组就没法做小尺寸补偿，原样渲染
      }
      var img = new Image();
      img.src = 'data:image/svg+xml;charset=utf-8,' + encodeURIComponent(src);
      await img.decode();
      var cv = document.createElement('canvas');
      cv.width = cv.height = size;
      var ctx = cv.getContext('2d');
      var S = size / 1024;
      ctx.setTransform(S, 0, 0, S, 0, 0);
      ctx.drawImage(img, 0, 0, 1024, 1024);
      if (hasGlyph(cand.svg)) drawGlyph(ctx);
      resolve(cv);
    });
  }

  function blit(dst, src, x, y) {
    dst.getContext('2d').drawImage(src, x, y);
  }

  async function main() {
    document.fonts.add(new FontFace('Smiley Sans Oblique', FONT_URL, { weight: '400' }));
    await document.fonts.load(FONT, GLYPH);
    say('字体就绪，候选 ' + CANDS.length + ' 个');

    var cache = {};                      // key -> canvas
    for (var ci = 0; ci < CANDS.length; ci++) {
      for (var si = 0; si < SIZES.length; si++) {
        var size = SIZES[si];
        cache[ci + ':' + size] = await raster(CANDS[ci], size, 1);
        cache[ci + ':' + size + ':4'] = await raster(CANDS[ci], size * 4, 1);
      }
      say('  已渲染 ' + CANDS[ci].name);
    }

    // 布局：每个候选两块（亮底 / 暗底）；块内先 1:1 一行，再 4× 一行
    var CELL = 300;
    var rowsPerCand = 4;
    var W = ROWLABEL + SIZES.length * CELL + PAD * 2;
    var H = PAD * 2 + CANDS.length * (rowsPerCand * 210 + 40);
    var sheet = document.createElement('canvas');
    sheet.width = W; sheet.height = H;
    var g = sheet.getContext('2d');
    g.fillStyle = '#8a8a8a';
    g.fillRect(0, 0, W, H);

    var y = PAD;
    for (var c2 = 0; c2 < CANDS.length; c2++) {
      g.fillStyle = '#111';
      g.font = 'bold 22px "Microsoft YaHei", sans-serif';
      g.fillText(CANDS[c2].name, PAD, y + 26);
      y += 40;

      var bands = [{ bg: '#FCFAF5', dark: false }, { bg: '#1A1917', dark: true }];
      for (var b = 0; b < bands.length; b++) {
        g.fillStyle = bands[b].bg;
        g.fillRect(PAD, y, W - PAD * 2, 210);
        var x = PAD + ROWLABEL;
        for (var s2 = 0; s2 < SIZES.length; s2++) {
          var sz = SIZES[s2];
          // 1:1 靠底对齐
          blit(sheet, cache[c2 + ':' + sz], x, y + 200 - sz);
          // 4× 放上面（256×4 太大，跳过）
          if (sz * 4 <= 256) blit(sheet, cache[c2 + ':' + sz + ':4'], x, y + 190 - sz * 4 - 8);
          x += CELL;
        }
        g.fillStyle = bands[b].dark ? '#8B857A' : '#837D70';
        g.font = '16px "Microsoft YaHei", sans-serif';
        g.fillText(bands[b].dark ? '暗底' : '亮底', PAD + 8, y + 120);
        g.fillText('1:1 与 4×', PAD + 8, y + 146);
        y += 210;
      }
      y += 20;
    }

    post('render.save', { name: 'icon-lab.png', data: sheet.toDataURL('image/png') });
    say('完成');
    post('render.done');
  }

  (async function () {
    try { await main(); }
    catch (e) { post('render.fail', { error: String((e && e.message) || e) }); }
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

    // 设计对比模式：把指定目录下所有 .svg 一起渲染成对照表
    let html = match std::env::var("DESKBASE_RENDER_LAB") {
        Ok(lab_dir) => {
            let mut cands = Vec::new();
            let mut files: Vec<PathBuf> = std::fs::read_dir(&lab_dir)
                .map_err(|e| wry::Error::Io(std::io::Error::other(e.to_string())))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().map(|x| x == "svg").unwrap_or(false))
                .collect();
            files.sort();
            for f in files {
                let name = f
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                if let Ok(text) = std::fs::read_to_string(&f) {
                    cands.push(serde_json::json!({ "name": name, "svg": text }));
                }
            }
            log_to(out_dir, &format!("对比模式：{} 个候选", cands.len()));
            PAGE_LAB
                .replace("{{CANDS}}", &serde_json::Value::Array(cands).to_string())
                .replace("{{FONT}}", &font_b64)
        }
        Err(_) => PAGE
            .replace("{{SVG}}", &svg_json)
            .replace(
                "{{SVG_SMALL}}",
                &serde_json::to_string(include_str!("../ui/brand/icon-small.svg"))
                    .unwrap_or_else(|_| "\"\"".into()),
            )
            .replace("{{FONT}}", &font_b64),
    };
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
