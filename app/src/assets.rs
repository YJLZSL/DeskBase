//! 内嵌静态资源与 `deskbase://` 协议
//!
//! ## 为什么需要这一层
//!
//! 之前 UI 是由 `include_str!` 把 HTML / CSS / JS 拼成一个大字符串塞进 `with_html`。
//! 这在只有三个文本文件时够用，但从「字体」开始就不成立了：
//!
//!   - WOFF2 是二进制，`include_str!` 读不了；
//!   - 转成 base64 内联进 CSS 要多付 33% 体积，而且每次改 CSS 都要重新编码；
//!   - 后面还有 P2-14 图标集、P3 缩略图等更多资源要进来。
//!
//! 所以改成按路径取资源：UI 用普通的 `<link>` / `url()` 引用，主进程用
//! `include_bytes!` 把资源编进 exe，再通过自定义协议回给 WebView。
//!
//! ## 安全约定（重要）
//!
//! **本模块只服务编译期就固定下来的资源表，不读文件系统、不做路径拼接。**
//! 未登记的路径一律 404。这一点是刻意的：即使渲染层被注入，`deskbase://`
//! 也无法变成读取本机文件的通道——`docs/08` 要求的「渲染层无文件系统直访」
//! 靠的就是这里。
//!
//! ## Windows 上的 URL 映射
//!
//! WebView2 不支持非标准协议，wry 走了一个固定的 workaround：
//! `deskbase://localhost/x` → `http://deskbase.localhost/x`，
//! 处理器侧再还原回来。所以**主机段必须写 `localhost`**，
//! 取 `request.uri().path()` 就是要的资源路径。

use std::borrow::Cow;

use wry::http::{header, Request, Response, StatusCode};

/// 自定义协议名
pub const SCHEME: &str = "deskbase";

/// 首页地址。相对引用会解析到同一个 origin（`deskbase://localhost/`），
/// 因此 CSS / JS / 字体都能用相对路径引用。
pub const INDEX_URL: &str = "deskbase://localhost/index.html";

struct Asset {
    bytes: &'static [u8],
    mime: &'static str,
}

const HTML: &str = "text/html; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";
const TEXT: &str = "text/plain; charset=utf-8";
const WOFF2: &str = "font/woff2";
const SVG: &str = "image/svg+xml";
const PNG: &str = "image/png";

/// 资源表。新增资源就在这里加一行——加进来的东西才会被服务。
fn lookup(path: &str) -> Option<Asset> {
    let a = match path {
        "/" | "/index.html" => Asset {
            bytes: include_bytes!("../ui/index.html"),
            mime: HTML,
        },
        "/theme.css" => Asset {
            bytes: include_bytes!("../ui/theme.css"),
            mime: CSS,
        },
        "/app.js" => Asset {
            bytes: include_bytes!("../ui/app.js"),
            mime: JS,
        },

        // ---------- 动效运行时（P1） ----------
        // motion.css 只声明变量（三条 linear() 弹簧曲线），motion.js 是它的运行时。
        // 两者必须**成对**登记：只挂 CSS 时 spring() 读不到曲线，只会退化成
        // cubic-bezier 近似并在控制台告警；只挂 JS 时 lift() 无处可读时长。
        "/motion.css" => Asset {
            bytes: include_bytes!("../ui/motion.css"),
            mime: CSS,
        },
        "/motion.js" => Asset {
            bytes: include_bytes!("../ui/motion.js"),
            mime: JS,
        },

        // ---------- 基础组件库与命令面板（P2） ----------
        // 这四份都是自包含的：DOM 在运行时创建并挂到 body，
        // index.html 里只负责加 <link> / <script>，不写任何标记。
        "/components.css" => Asset {
            bytes: include_bytes!("../ui/components.css"),
            mime: CSS,
        },
        "/components.js" => Asset {
            bytes: include_bytes!("../ui/components.js"),
            mime: JS,
        },
        "/palette.css" => Asset {
            bytes: include_bytes!("../ui/palette.css"),
            mime: CSS,
        },
        "/palette.js" => Asset {
            bytes: include_bytes!("../ui/palette.js"),
            mime: JS,
        },

        // ---------- 字体（见 tools/fonts/ 的来源登记与校验脚本） ----------
        // 得意黑 Smiley Sans，SIL OFL 1.1，未修改再分发。
        // 许可原文随包分发：app/ui/fonts/OFL-smiley-sans.txt
        "/fonts/smiley-sans-oblique.woff2" => Asset {
            bytes: include_bytes!("../ui/fonts/smiley-sans-oblique.woff2"),
            mime: WOFF2,
        },

        // ---------- 应用图标（源文件与产物见 tools/icon/） ----------
        "/brand/icon.svg" => Asset {
            bytes: include_bytes!("../ui/brand/icon.svg"),
            mime: SVG,
        },
        // 界面里用 64px 那张（侧栏品牌位）；关于页显示 56px，高 DPI 下要 128px
        "/brand/icon-64.png" => Asset {
            bytes: include_bytes!("../ui/brand/icon-64.png"),
            mime: PNG,
        },
        "/brand/icon-128.png" => Asset {
            bytes: include_bytes!("../ui/brand/icon-128.png"),
            mime: PNG,
        },

        _ => return None,
    };
    Some(a)
}

/// 处理一次 `deskbase://` 请求。
pub fn handle(req: Request<Vec<u8>>) -> Response<Cow<'static, [u8]>> {
    let path = req.uri().path().to_string();

    // 只接受 GET；UI 资源没有写入语义
    if req.method() != wry::http::Method::GET {
        return plain(StatusCode::METHOD_NOT_ALLOWED, "只支持 GET");
    }

    match lookup(&path) {
        Some(asset) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, asset.mime)
            // 字体体积大且内容随版本固定，允许 WebView 缓存
            .header(header::CACHE_CONTROL, "public, max-age=86400")
            .body(Cow::Borrowed(asset.bytes))
            .unwrap_or_else(|_| plain(StatusCode::INTERNAL_SERVER_ERROR, "构造响应失败")),
        None => plain(StatusCode::NOT_FOUND, "资源不存在"),
    }
}

fn plain(status: StatusCode, msg: &'static str) -> Response<Cow<'static, [u8]>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, TEXT)
        .body(Cow::Borrowed(msg.as_bytes()))
        .expect("静态文本响应不应失败")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(path: &str) -> Response<Cow<'static, [u8]>> {
        handle(
            Request::builder()
                .method("GET")
                .uri(path)
                .body(Vec::new())
                .unwrap(),
        )
    }

    #[test]
    fn 首页与三份文本资源都在表里() {
        for p in ["/", "/index.html", "/theme.css", "/app.js"] {
            assert_eq!(get(p).status(), StatusCode::OK, "{p} 应当可服务");
        }
    }

    #[test]
    fn 字体是可服务的合法_woff2() {
        let r = get("/fonts/smiley-sans-oblique.woff2");
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()[header::CONTENT_TYPE], "font/woff2");
        let body = r.body();
        assert_eq!(&body[..4], b"wOF2", "字体文件头必须是 wOF2 魔数");
        assert!(body.len() > 100_000, "字体体积异常，疑似打包出错");
    }

    #[test]
    fn 未登记路径一律_404_不做文件系统访问() {
        for p in [
            "/../Cargo.toml",
            "/../../Cargo.toml",
            "/fonts/../src/main.rs",
            "/%2e%2e/Cargo.toml",
            "/etc/passwd",
            "/fonts/",           // 目录本身不服务
            "/fonts/missing.woff2",
        ] {
            assert_eq!(get(p).status(), StatusCode::NOT_FOUND, "{p} 不应被服务");
        }
    }

    #[test]
    fn 非_get_方法被拒绝() {
        let r = handle(
            Request::builder()
                .method("POST")
                .uri("/index.html")
                .body(Vec::new())
                .unwrap(),
        );
        assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[test]
    fn 界面引用到的每个品牌图标都在资源表里() {
        // UI 里出现过的品牌资源路径，漏登记就会变成破图 —— 这个坑踩过一次
        // （关于页写的是 icon-128.png，但资源表里只有 64 和 svg）
        let ui_html = std::str::from_utf8(include_bytes!("../ui/index.html")).unwrap();
        let ui_css = std::str::from_utf8(include_bytes!("../ui/theme.css")).unwrap();
        for src in ui_html.split('"').chain(ui_css.split('"')).chain(ui_css.split('(')) {
            let s = src.trim_matches(|c: char| c == ')' || c == ';' || c.is_whitespace());
            if !s.starts_with("brand/") && !s.starts_with("fonts/") {
                continue;
            }
            let path = format!("/{s}");
            assert!(
                lookup(&path).is_some(),
                "UI 引用了 {path}，但 assets.rs 的资源表里没有它 —— 会渲染成破图"
            );
        }
    }

    /// 上面那个测试只查 brand/ 与 fonts/，`<script src>` 和 `<link href>` 完全不在它的
    /// 视野里 —— 而新增的每个 JS/CSS 都要在资源表里登记一行，漏了就静默 404，
    /// 页面照样能开，只是某个功能悄悄不工作。这正是并行开发最容易踩的一脚。
    ///
    /// 所以这里把所有**站内**引用（相对路径、非 http/data/# ）都扫一遍。
    #[test]
    fn 首页引用的每个脚本与样式都能被服务() {
        let html = std::str::from_utf8(include_bytes!("../ui/index.html")).unwrap();

        // 取出 src="..." 与 href="..." 的值
        let mut refs: Vec<String> = Vec::new();
        for attr in ["src=\"", "href=\""] {
            for part in html.split(attr).skip(1) {
                if let Some(end) = part.find('"') {
                    refs.push(part[..end].to_string());
                }
            }
        }

        assert!(refs.len() >= 5, "一个引用都没扫到，说明这个测试本身失效了");

        let mut checked = 0;
        for r in &refs {
            // 只关心站内资源：外链、行内、锚点一律跳过
            if r.contains("://") || r.starts_with("data:") || r.starts_with('#') || r.is_empty() {
                continue;
            }
            let path = format!("/{r}");
            assert!(
                lookup(&path).is_some(),
                "index.html 引用了 {r}，但资源表里没有它 —— \
                 浏览器会拿到 404，页面不报错、功能静默失效。\
                 请在这里登记：app/src/assets.rs 的 lookup()"
            );
            checked += 1;
        }
        // 目前是 4 个 CSS + 4 个 JS + 1 个图标 = 9；留点余量防止将来删文件后测试变成空转
        assert!(checked >= 8, "只校验了 {checked} 个引用，疑似扫描逻辑失效");
    }

    #[test]
    fn 许可文本随字体一起分发_可以不在资源表里但必须在仓库里() {
        // OFL 要求许可随字体分发。资源表不服务它（UI 不需要），
        // 但文件必须存在，由 tools/fonts/fetch-fonts.cjs 校验。
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("ui")
            .join("fonts")
            .join("OFL-smiley-sans.txt");
        assert!(p.exists(), "缺少字体许可文件：{}", p.display());
    }
}
