/* ============================================================
   DeskBase 动效运行时 · window.DeskBaseMotion
   ============================================================
   自包含：不 import、不依赖任何库，一个 <script src="motion.js"> 就能用。

   它只做一件事：把「动效档位 / 系统减少动效 / CSS token」这三件事读出来，
   给 JS 侧的动画一个统一的入口。**它不负责改 data-motion** —— 那是
   app.js 的事（app.js 已经实现了「系统减少动效是封顶而不是覆盖」那套
   逻辑，见它的 applyMotion / effectiveMotion）。这里再监听一次
   matchMedia 去写属性，就会出现两个写者抢同一个属性，谁最后写谁生效，
   而用户的选择会被悄悄改掉。所以：本文件只**读**，只**订阅**。

   接口：
     tier()                       当前档位 "off"|"minimal"|"standard"|"rich"
     dur(name)                    "instant"|"fast"|"normal"|"slow"|"slower" → 毫秒
     ease(name)                   缓动字符串（"standard"|"enter"|… 或弹簧三档）
     spring(kind)                 "smooth"|"snappy"|"bouncy" → linear() 曲线
     lift(el, fn, ms)             临时提升合成层，执行 fn，ms 后撤销
     onTierChange(cb)             档位变化订阅，返回退订函数
     prefersReduced()             系统是否要求减少动效

   约定（docs/18 7.1#2）：所有动画只动 transform 与 opacity。
   本文件里的 will-change 也只提升 transform —— 它是这两者共同的合成层，
   不需要额外声明 opacity。
   ============================================================ */
(function (win) {
  "use strict";

  // 非浏览器环境（Node 里跑静态检查、或在构建脚本里被 require）直接退出，
  // 而不是在 window 上抛异常 —— 这个文件应该永远可以被安全地加载。
  if (!win || !win.document || !win.document.documentElement) return;

  var doc = win.document;
  var root = doc.documentElement;

  /** 四档，下标即强弱。与 app.js 的 TIERS 一致；改动必须两边一起改。 */
  var TIERS = ["off", "minimal", "standard", "rich"];
  var DEFAULT_TIER = "standard";

  /** 三个弹簧档位名，对应 motion.css 的三条 linear() 曲线 */
  var SPRING_KINDS = ["smooth", "snappy", "bouncy"];

  /** 时长兜底：与 theme.css 的「标准」档一致。真正的常量表是 CSS 自定义
   *  属性，这里只有「theme.css 都没加载」时才会走到 —— 那种情况下整个
   *  界面本来就是坏的，能返回一个合理的数就行，不必抛错。 */
  var DUR_FALLBACK = { instant: 90, fast: 150, normal: 200, slow: 320, slower: 460 };

  /** 曲线兜底：linear() 不被支持、或 motion.css 没加载时用的近似。
   *  取值全部来自 theme.css 已有的 token，不引入新数字。
   *  它们只是「像」弹簧：贝塞尔做不出往复余振，过冲量也是凑的 ——
   *  为什么以及差在哪，motion.css 开头写清楚了。 */
  var BEZIER_FALLBACK = {
    smooth: "cubic-bezier(0.22, 1, 0.36, 1)", // = --ease-glide
    snappy: "cubic-bezier(0.34, 1.4, 0.64, 1)", // = --ease-spring
    bouncy: "cubic-bezier(0.34, 1.56, 0.64, 1)", // = 「丰富」档的 --ease-spring
  };

  var cache = Object.create(null);
  var warned = Object.create(null);
  var reduceQuery = null;
  var linearOk = null;

  // ============================================================
  // 基础设施
  // ============================================================

  /** 同一个问题只喊一次。动效代码会在循环里被调用，日志刷屏比不报警更糟。 */
  function warn(key, msg) {
    if (warned[key]) return;
    warned[key] = true;
    if (win.console && typeof win.console.warn === "function") {
      win.console.warn("[DeskBaseMotion] " + msg);
    }
  }

  function clearCache() {
    cache = Object.create(null);
  }

  /** 读一个自定义属性。两种情况都当「没读到」：
   *   · 空 —— CSS 没加载或变量名写错
   *   · 值里还有没解析的 var() —— 说明规则用 var() 引了别的 token 而那
   *     个 token 缺失。这种字符串交给 CSS 用还行，交给 WAAPI 的 easing
   *     会直接报错，所以宁可在这里就判定为「没有」。 */
  function readVar(name) {
    var v = "";
    try {
      v = win.getComputedStyle(root).getPropertyValue(name);
    } catch (err) {
      v = "";
    }
    v = (v || "").trim();
    return !v || v.indexOf("var(") !== -1 ? "" : v;
  }

  /** 带缓存的 token 读取。
   *  缓存键里带档位和「系统是否要求减少动效」：这两者任一变化，同一个
   *  变量名解析出来的值就可能不同（theme.css 按 [data-motion] 覆盖时长与
   *  曲线）。让键自己去失效，比维护一张「在哪儿清缓存」的清单可靠。
   *  注意：如果以后时长也开始跟着**主题**变，键里要再加主题 —— 目前
   *  theme.css 里只有 data-motion 和 data-texture 会改 --dur-* / --ease-*。 */
  function token(name) {
    var key = tier() + "|" + (prefersReduced() ? "r" : "n") + "|" + name;
    if (key in cache) return cache[key];
    // 空值也缓存：CSS 没加载时否则每次调用都要走一遍 getComputedStyle
    // （它是会触发样式解析的，放在循环里很贵）。
    cache[key] = readVar(name);
    return cache[key];
  }

  /** "90ms" / "0.09s" / "1ms" → 毫秒；认不出来返回 null */
  function parseTime(raw) {
    var m = /^([0-9]*\.?[0-9]+)(ms|s)?$/i.exec(raw);
    if (!m) return null;
    var n = parseFloat(m[1]);
    if (!isFinite(n)) return null;
    return /^s$/i.test(m[2] || "") ? n * 1000 : n;
  }

  function has(obj, key) {
    return Object.prototype.hasOwnProperty.call(obj, key);
  }

  /** 调一次回调，但回调可以不是函数（调用方传了 undefined 不该炸） */
  function invoke(fn) {
    return typeof fn === "function" ? fn() : undefined;
  }

  // ============================================================
  // 查询
  // ============================================================

  /** 当前档位。属性缺失或写了个不认识的值都退回 standard：
   *  界面在动、代码拿到 undefined 是最难受的组合。 */
  function tier() {
    var t = root.dataset ? root.dataset.motion : root.getAttribute("data-motion");
    return TIERS.indexOf(t) === -1 ? DEFAULT_TIER : t;
  }

  /** 系统是否要求减少动效。
   *  这里**只读 .matches、绝不监听变化去改 data-motion** —— app.js 已经
   *  在处理「封顶」与「系统设置运行中变化」这两件事。再监听一遍就是
   *  两个写者抢一个属性。
   *  返回值是实时的：matchMedia 每次读 .matches 都是当下的系统状态，
   *  不要把它缓存成布尔量。 */
  function prefersReduced() {
    if (reduceQuery === null) {
      reduceQuery =
        typeof win.matchMedia === "function"
          ? win.matchMedia("(prefers-reduced-motion: reduce)")
          : false;
    }
    return !!reduceQuery && reduceQuery.matches === true;
  }

  /** linear() 缓动是否可用（Chromium 113+）。不可用时弹簧要退成 cubic-bezier，
   *  否则整条 transition 简写会因为值无效而被丢掉 —— 那时候元素会「瞬移」，
   *  比动画不好看严重得多。 */
  function linearSupported() {
    if (linearOk === null) {
      linearOk = false;
      try {
        var css = win.CSS;
        if (css && typeof css.supports === "function") {
          linearOk =
            css.supports("transition-timing-function", "linear(0, 1)") ||
            css.supports("easing-function", "linear(0, 1)");
        }
      } catch (err) {
        linearOk = false;
      }
    }
    return linearOk;
  }

  // ============================================================
  // API
  // ============================================================

  /** 时长（毫秒）。名字固定五个：instant / fast / normal / slow / slower。
   *  值来自 theme.css 的 --dur-*，会跟着档位变（「关」档是 1ms）。
   *
   *  这里**不**为「系统减少动效」做特殊处理：CSS 侧已经用
   *  @media (prefers-reduced-motion) 把 transition-duration 压到 1ms，
   *  app.js 又把档位封顶成「精简」，再在这里降一次就是第三处判据。
   *  例外是你用 dur() 去驱动 JS 动画（WAAPI 不走 CSS 的 @media）——
   *  那种场景请先问 prefersReduced()，或干脆把时长也交给 CSS 变量。 */
  function dur(name) {
    var key = name == null ? "" : String(name);
    if (!has(DUR_FALLBACK, key)) {
      warn(
        "dur:" + key,
        "dur('" + key + "') 不认识这个名字，按 normal 处理。可用：" + Object.keys(DUR_FALLBACK).join(" / ")
      );
      key = "normal";
    }
    var ms = parseTime(token("--dur-" + key));
    return ms === null ? DUR_FALLBACK[key] : ms;
  }

  /** 弹簧曲线。
   *  · 正常路径：读 motion.css 的 --ease-spring-<kind>，那里是唯一的真源
   *    （曲线文本只有一份，JS 不复制 —— 复制必然有一天会跟 CSS 漂移）。
   *  · 「关」档 / 系统减少动效：返回 linear。不是「干脆不动」，而是「别弹」：
   *    档位此时已经把时长压到 1ms，即使有人手动把时长放回来，也不该回弹。
   *  · 「精简」档：CSS 里那条规则已经把 --ease-spring-* 指向 --ease-standard，
   *    这里读到什么就是什么，不额外判断（单一真源的好处）。
   *  · 读不到（motion.css 没加载）/ linear() 不支持：退回 cubic-bezier 近似
   *    并告警一次，宁可弹得不真，也不能让 transition 整条失效。 */
  function spring(kind) {
    var k = normalizeKind(kind);
    if (tier() === "off" || prefersReduced()) return "linear";
    var v = token("--ease-spring-" + k);
    if (v) return v;
    if (linearSupported()) {
      warn(
        "spring-css",
        "读不到 --ease-spring-" + k + "：motion.css 没加载？已退回 cubic-bezier 近似（它会丢掉余振）。"
      );
    }
    return BEZIER_FALLBACK[k];
  }

  /** 把 "smooth" / "Spring-Snappy" / "--ease-spring-bouncy" 都归一到三个档位名。
   *  认不出来退回 smooth：它没有过冲，是三个里最不打扰人的那个 ——
   *  名字写错时宁可不弹，也不要突然蹦一下。 */
  function normalizeKind(kind) {
    var s = String(kind == null ? "" : kind)
      .trim()
      .toLowerCase()
      .replace(/^--ease-/, "")
      .replace(/^spring-/, "");
    if (SPRING_KINDS.indexOf(s) === -1) {
      warn("kind:" + s, "spring('" + s + "') 不认识，按 smooth 处理。可用：" + SPRING_KINDS.join(" / "));
      return "smooth";
    }
    return s;
  }

  /** 缓动曲线。名字对应 theme.css 的 --ease-<name>（standard / enter / exit /
   *  glide / spring / press…）。三个弹簧档位名会转发给 spring()，
   *  这样调用方只需要记住一套名字。
   *  读不到就 linear 并告警 —— 这个名字只存在于 theme.css，motion.css
   *  给不了兜底（不想把同一批控制点复制第二遍），所以「读不到」基本等于
   *  名字打错了，值得喊一声。 */
  function ease(name) {
    var n = String(name == null ? "" : name)
      .trim()
      .replace(/^--ease-/, "");
    var m = /^(?:spring-)?(smooth|snappy|bouncy)$/.exec(n);
    if (m) return spring(m[1]);
    if (!n) return "linear";
    var v = token("--ease-" + n);
    if (v) return v;
    warn("ease:" + n, "读不到 --ease-" + n + "（theme.css 没加载？名字写错了？），已退回 linear。");
    return "linear";
  }

  // ============================================================
  // lift：临时提升合成层
  // ============================================================
  var lifts = new WeakMap(); // el → { n: 未结束的 lift 数, prev: 元素原本的 will-change }

  /** 给 el 加上 will-change: transform，执行 fn，ms 毫秒后撤销。
   *
   *  为什么不是直接写进样式表：will-change 的语义是「我**即将**要动这个属性」，
   *  浏览器据此提前分层、把该元素从主线程绘制里摘出去。写进样式表意味着
   *  这个承诺永久有效 —— 每个元素都占一层显存，长列表滚起来反而更慢，
   *  而真正需要它的那 300ms 反而混在噪音里。它是最后手段、按需借按需还。
   *
   *  三个细节：
   *  · try/finally 保证撤销：fn 抛异常时 will-change 不能留在元素上。
   *  · 引用计数：同一个元素可能有两次 lift 重叠（比如快速连按两次按钮），
   *    谁最后结束谁负责撤，先结束的那次不能提前撤。
   *  · 恢复而不是盲删：元素上可能本来就有别人设的 will-change，撤的时候
   *    写回原值，不是一把 removeProperty。
   *
   *  ms 默认取 dur("normal")。「关」档下它就是 1ms —— 于是提升几乎立刻
   *  结束，这正确：没有动画就不需要合成层。
   */
  function lift(el, fn, ms) {
    var wait = typeof ms === "number" && ms >= 0 ? ms : dur("normal");
    if (!el || !el.style) return invoke(fn); // 拿不到元素就只执行动作，不 pretend

    var st = lifts.get(el);
    if (!st) {
      st = { n: 0, prev: el.style.willChange || "" };
      lifts.set(el, st);
    }
    st.n++;
    el.style.willChange = "transform";

    try {
      return invoke(fn);
    } finally {
      win.setTimeout(function () {
        st.n--;
        if (st.n > 0) return; // 还有别的 lift 挂在同一个元素上
        if (st.prev) el.style.willChange = st.prev;
        else el.style.removeProperty("will-change");
        lifts.delete(el);
      }, wait);
    }
  }

  // ============================================================
  // onTierChange：档位订阅
  // ============================================================
  var subs = [];
  var observer = null;
  var lastTier = null;

  /** 订阅档位变化，返回退订函数。
   *
   *  只在下一次变化时触发，**不会**立刻用当前档位调一次 —— 想拿当前值直接
   *  tier()，别让「订阅」顺便干「取值」的活（否则回调会在最难受的时机被调，
   *  比如订阅方自己还没初始化完）。
   *  回调参数是新档位，纯顺手：省得回调里再调一次 tier()。
   *
   *  用 MutationObserver 而不是让 app.js 喊一声：app.js 是按需改属性的，
   *  它不需要知道谁在听。观察属性也让「别的代码改了 data-motion」同样有效，
   *  不会出现「只有走 app.js 那条路才通知」的隐形约定。 */
  function onTierChange(cb) {
    if (typeof cb !== "function") return function () {};
    subs.push(cb);

    if (!observer) {
      if (typeof win.MutationObserver !== "function") {
        warn("no-mo", "环境没有 MutationObserver，onTierChange 的订阅不会触发。");
      } else {
        // 观察本身失败（环境古怪、root 不可观察）时不能让 onTierChange 抛出去：
        // 订阅是个锦上添花的能力，不该拖垮调用方的初始化。
        try {
          observer = new win.MutationObserver(function () {
            var now = tier();
            if (now === lastTier) return; // 属性被写成了同一个值，算没变
            lastTier = now;
            clearCache(); // 档位换了，时长与曲线都要重读
            // 复制一份再遍历：回调里退订（很常见）不应该打乱这次派发
            var list = subs.slice();
            for (var i = 0; i < list.length; i++) {
              try {
                list[i](now);
              } catch (err) {
                // 一个订阅者抛错不能拖垮其他订阅者，也不能让 observer 死掉
                if (win.console && typeof win.console.error === "function") win.console.error(err);
              }
            }
          });
          observer.observe(root, { attributes: true, attributeFilter: ["data-motion"] });
        } catch (err) {
          observer = null;
          warn("mo-failed", "MutationObserver 建立失败，onTierChange 的订阅不会触发。");
        }
      }
    }

    return function off() {
      var i = subs.indexOf(cb);
      if (i !== -1) subs.splice(i, 1);
      // 没人听了就把 observer 拆了：它持有回调引用，留着等于泄漏
      if (!subs.length && observer) {
        observer.disconnect();
        observer = null;
        lastTier = null;
      }
    };
  }

  // CSS 可能在脚本之后才解析完（<script> 放在 <link> 前面、或样式表是异步的），
  // 那时第一次读 token 会读到空值并被缓存。到了这两个时点，样式表一定就位了，
  // 丢掉缓存重来一次。只清两次，不是常驻监听。
  if (doc.readyState === "loading") {
    doc.addEventListener("DOMContentLoaded", clearCache, { once: true });
  }
  win.addEventListener("load", clearCache, { once: true });

  win.DeskBaseMotion = {
    tier: tier,
    dur: dur,
    ease: ease,
    spring: spring,
    lift: lift,
    onTierChange: onTierChange,
    prefersReduced: prefersReduced,
  };
})(typeof window !== "undefined" ? window : null);
