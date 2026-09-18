/* ============================================================
   DeskBase 数据库使用教程（设置页）
   ============================================================
   面向**不懂数据库的普通人**（会计、行政、小工厂仓管、个体店主）。

   设计约束（来自项目发起人的硬性要求）：
     · 不跟用户说"数据库"，改说"台账 / 客户名单"；术语用「字段 / 记录」
       （网格语境用「列 / 行」）。
     · 只写**已经做好的**功能。写了没做的东西是最坏的教学事故 ——
       用户会去找，然后找不到，信任就没了。
     · 中文、短句、不吓人。

   组件形态与 grid.js 一致：IIFE + `window.DeskBaseHelp = { mount }`，
   自己注入 <link> 拉 help.css（带重复加载保护）。挂载点放在设置页的
   `.stack` 里，由本文件在加载时自行初始化（参考 db.js 的做法）。

   本文件不碰数据库、不碰 IPC，纯静态内容。所有说法都能在 db.js /
   grid.js / sql.js 里找到对应实现。
   ============================================================ */
(function () {
  "use strict";

  // 同一份脚本被加载两次时（WebView 里出现过重复注入），第二次直接退出。
  if (window.DeskBaseHelp) return;

  /**
   * 本脚本自身的 URL，在加载时立刻记下来（理由同 grid.js：
   * document.currentScript 只在同步执行期有值，mount 时再用就晚了）。
   */
  const SELF_SRC = (function () {
    const s = document.currentScript && document.currentScript.src;
    if (s) return s;
    const links = document.getElementsByTagName("script");
    for (let i = links.length - 1; i >= 0; i--) {
      if (/help\.js($|\?)/.test(links[i].src || "")) return links[i].src;
    }
    return null;
  })();

  const STYLE_ID = "dbhelp-styles";

  /**
   * 样式放在独立的 help.css 里，由 <script> 自己注入。index.html 的
   * <head> 已经加了一条 <link rel="stylesheet" href="help.css">，这里
   * 检测到就跳过，不会加载第二遍 —— 双保险，且能让 assets.rs 的资源表
   * 扫描（只看 index.html 的引用）覆盖到这份 CSS。
   */
  function injectStyles() {
    if (document.getElementById(STYLE_ID)) return;
    try {
      if (document.querySelector('link[rel="stylesheet"][href$="help.css"]')) return;
    } catch (e) {
      /* 选择器不支持就算了，下面照常注入 */
    }
    let href = "help.css";
    try {
      href = new URL("help.css", SELF_SRC || document.baseURI).href;
    } catch (e) {
      /* 保底用相对路径 */
    }
    const link = document.createElement("link");
    link.id = STYLE_ID;
    link.rel = "stylesheet";
    link.href = href;
    link.addEventListener("error", () => {
      // 样式没加载成功时教程会以裸样式出现（标题、表格全崩）。这类失败必须响亮。
      console.error(
        "[DeskBaseHelp] help.css 加载失败：" + href +
          "\n  deskbase:// 只服务编译期登记过的资源 —— 需要在 app/src/assets.rs 的 lookup() 登记 /help.css。"
      );
    });
    document.head.appendChild(link);
  }

  /** 教程正文。纯静态、全部来自已实现能力，无用户输入，innerHTML 安全。 */
  const TEMPLATE = [
    '<h3>数据库使用教程</h3>',
    '<p class="help-lead">这里把"数据库"换成你熟悉的说法：它就是一张能查、能备份、能带走的"台账"。',
    '本教程只讲<strong>已经做好</strong>的功能，照着做就行。</p>',

    // ① 数据库是什么
    '<h4 class="help-h">一、台账是什么</h4>',
    '<p>一张台账 = 一张表。比如"客户名单""库存表""费用明细"。</p>',
    '<ul class="help-ul">',
    '<li>和 Excel 的区别：Excel 适合一个人慢慢算；台账适合"数据多到 Excel 卡、还要反复查"的那部分。</li>',
    '<li>台账每列的类型是定死的（比如"金额"列只能填钱），填错当场拦下，不会把整张表搞乱。</li>',
    '<li>台账就一个文件，复制到 U 盘、备份到别处都方便。</li>',
    '</ul>',
    '<p class="help-note">你不需要懂"数据库"这三个字，会用表就行。</p>',

    // ② 三步建一张台账
    '<h4 class="help-h">二、三步建一张台账</h4>',
    '<ol class="help-steps">',
    '<li><b>起名</b>：进「数据库」页，点左上「新建表」，给表起个名，如"费用明细"。</li>',
    '<li><b>加字段</b>：字段就是表头那一列。想清楚要记哪几项，逐个加上（如：项目、金额、日期）。</li>',
    '<li><b>录数据</b>：建好后切到「数据」页签，滚到表尾那一行直接填，按回车就提交。</li>',
    '</ol>',
    '<p>不会填？点数据库页左上「创建示例台账」，一张现成的表立刻打开给你看。</p>',

    // ③ 字段类型怎么选
    '<h4 class="help-h">三、字段类型怎么选（9 种）</h4>',
    '<table class="help-table">',
    '<thead><tr><th>类型</th><th>一句话</th><th>什么时候用</th></tr></thead>',
    '<tbody>',
    '<tr><td>文本</td><td>任意文字</td><td>名称、备注、地址</td></tr>',
    '<tr><td>整数</td><td>没有小数点的数</td><td>数量、次数、人数</td></tr>',
    '<tr><td>小数</td><td>带小数点的数</td><td>单价、比率、重量</td></tr>',
    '<tr><td>金额</td><td>按"分"存的钱</td><td>价钱、收款、报销（写 12.34 自动存 1234 分）</td></tr>',
    '<tr><td>是 / 否</td><td>只能选是或否</td><td>是否已结清、是否发货</td></tr>',
    '<tr><td>日期</td><td>年-月-日</td><td>下单日、到期日</td></tr>',
    '<tr><td>日期时间</td><td>具体到几点</td><td>打卡时间、录入时间</td></tr>',
    '<tr><td>JSON</td><td>一段结构化文本</td><td>进阶：存一组设置、一段明细</td></tr>',
    '<tr><td>二进制</td><td>文件本体</td><td>进阶：图片、附件（表格里不能直接改）</td></tr>',
    '</tbody></table>',
    '<p class="help-note">金额列很省心：写 <code>12.34</code>、<code>¥1,234.50</code>、<code>12元</code> 都认；显示时自动变回"元"。</p>',

    // ④ 查找与筛选
    '<h4 class="help-h">四、查找与筛选</h4>',
    '<ul class="help-ul">',
    '<li><b>排序</b>：点表头，在「升序 → 降序 → 不排」之间循环。</li>',
    '<li><b>列筛选</b>：表头下方那行输入框，输入关键词就只显示包含它的行（包含匹配）。</li>',
    '<li><b>翻页</b>：先显示 200 行；下面有「加载更多」，点一下再出 200 行。数据再多也不卡。</li>',
    '</ul>',

    // ⑤ 改与删
    '<h4 class="help-h">五、改与删</h4>',
    '<ul class="help-ul">',
    '<li><b>改</b>：双击格子就能改，改完点别处自动保存。</li>',
    '<li><b>清空成"空"还是"没填"</b>：空格子是"空字符串"；想表示"没填过"，点编辑里的「设为 NULL」。两者在界面上分开显示，不会混。</li>',
    '<li><b>删</b>：勾选要删的行（可多选），点工具条「删除所选」。删了找不回，会让你确认一次。</li>',
    '</ul>',

    // ⑥ 用 SQL 查
    '<h4 class="help-h">六、用 SQL 查（进阶，可跳过）</h4>',
    '<p>切到数据库页的「SQL」页签。下面三条能直接抄，把"表名"换成你的表：</p>',
    '<pre class="help-code">SELECT * FROM 表名 LIMIT 20</pre>',
    '<pre class="help-code">SELECT * FROM 表名 WHERE 是否已结清 = \'否\' LIMIT 50</pre>',
    '<pre class="help-code">SELECT COUNT(*) FROM 表名</pre>',
    '<p class="help-note">危险操作（删表、清空、不带条件的删 / 改）会弹<strong>两次</strong>确认：',
    '第一次是编辑器拦的，第二次是系统拦的。看清楚再点。</p>',

    // ⑦ 常见错误对照表
    '<h4 class="help-h">七、常见错误对照</h4>',
    '<table class="help-table">',
    '<thead><tr><th>现象</th><th>原因</th><th>怎么办</th></tr></thead>',
    '<tbody>',
    '<tr><td>金额填不进去 / 变奇怪</td><td>写成了"十二元"或留空</td><td>用数字：12.34、¥1,234.50、12元 都行</td></tr>',
    '<tr><td>提示"字段名不允许"</td><td>用了空格、标点，或叫了 rowid</td><td>改名：用中文、字母、数字、下划线</td></tr>',
    '<tr><td>某一行点不动、改不了</td><td>这张表没有"主键"列，系统分不清第几行</td><td>建表时给一列打上「主键」就能改</td></tr>',
    '<tr><td>表打不开</td><td>可能损坏，或被别的程序占用</td><td>点「刷新」；还不行就重启程序</td></tr>',
    '</tbody></table>',

    // ⑧ 术语表
    '<h4 class="help-h">八、术语对照</h4>',
    '<table class="help-table">',
    '<thead><tr><th>你看到的</th><th>意思</th></tr></thead>',
    '<tbody>',
    '<tr><td>字段</td><td>一列，比如"名称""金额"</td></tr>',
    '<tr><td>记录</td><td>一行，一条完整的数据</td></tr>',
    '<tr><td>行号（rowid）</td><td>系统内部给每行编的号，<strong>不显示</strong>给你</td></tr>',
    '<tr><td>金额按"分"存</td><td>你写 12.34 元，里面对应 1234 分；显示时自动变回元</td></tr>',
    '</tbody></table>',
  ].join("\n");

  /**
   * @param {Element} container 挂载点（设置页的 .stack）
   */
  function mount(container) {
    if (!container || typeof container.appendChild !== "function") {
      console.error("[DeskBaseHelp] mount() 的第一个参数必须是容器元素");
      return null;
    }
    injectStyles();
    const card = document.createElement("div");
    card.className = "card";
    card.id = "db-help-card";
    card.innerHTML = TEMPLATE;
    container.appendChild(card);
    return { root: card, destroy() { card.remove(); } };
  }

  window.DeskBaseHelp = { mount: mount };

  // ---------- 自行初始化：挂到设置页 ----------
  function init() {
    const stack = document.querySelector('[data-view="settings"] .stack');
    if (!stack) return;
    if (document.getElementById("db-help-card")) return; // 重复初始化保护
    injectStyles();
    mount(stack);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", init);
  } else {
    init();
  }
})();
