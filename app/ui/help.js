/* ============================================================
   DeskBase 数据库使用教程（设置页）
   ============================================================
   面向**不懂数据库的普通人**（会计、行政、小工厂仓管、个体店主）。

   设计约束（来自项目发起人的硬性要求）：
     · 不跟用户说"数据库"，改说"台账 / 客户名单"；术语用「列 / 记录」
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
    '<h3>办公套件使用教程</h3>',
    '<p class="help-lead">DeskBase 不是 Office 的替代品 —— 它是<strong>补在旁边的那一块</strong>：' +
      'Office 嫌小不做的杂事（长截图、批量转格式、把台账从 Excel 里搬出来自己管、本机跑的 AI），' +
      '它做；做完的东西只留在你自己手里。</p>',
    '<p class="help-note">本教程只讲<strong>已经做好</strong>的功能，照着做就行。</p>',

    // ① 这是什么
    '<h4 class="help-h">一、它到底是什么</h4>',
    '<p>一句话：<b>一个放在桌上的办公增强层</b>。断网能干完，关掉不上传，卸载不留痕。</p>',
    '<ul class="help-ul">',
    '<li><b>不替代 Office</b>：文档 / 表格 / 演示这些大家伙它不抢，只在旁边补空档。</li>',
    '<li><b>三件套方向</b>：笔记（记）已可用，<b>表格</b>已经能做正事，演示排在最后。</li>',
    '<li><b>底座是硬差异</b>：本地优先、隐私优先、数据就一个文件、零遥测。</li>',
    '</ul>',

    // ② 记与截
    '<h4 class="help-h">二、记与截</h4>',
    '<ul class="help-ul">',
    '<li><b>笔记</b>：首页直接写，自动保存。适合随手记、会议纪要、临时草稿。</li>',
    '<li><b>截图与长图</b>：截屏工具里能拼接长图 —— 整页聊天记录、一屏装不下的表单，一次截完。</li>',
    '<li><b>命令面板</b>：<code>Ctrl+K</code> 唤起，支持拼音首字母（打 <code>xjbj</code> 就能找到「新建笔记」）。</li>',
    '</ul>',

    // ③ 台账
    '<h4 class="help-h">三、台账（表格）：三步建一张</h4>',
    '<p>把"表"当成你熟悉的台账：客户名单、库存表、费用明细，都是一张表。</p>',
    '<ol class="help-steps">',
    '<li><b>起名</b>：进「表格」页（左侧导航从上往下第三项），点「新建表格」，给表起个名，如"费用明细"。</li>',
    '<li><b>加列</b>：列就是表头。想清楚要记哪几项，逐个加上（如：项目、金额、日期）。</li>',
    '<li><b>录数据</b>：直接在网格里填 —— 滚到表尾那一行，按回车就提交。</li>',
    '</ol>',
    '<p>手上已经有 Excel？直接在「从 Excel 导入」里选文件，它会认表头、认类型，',
      '还会<strong>先告诉你可能会丢什么</strong>再让你确认。</p>',
    '<p class="help-note">新建时不用挑列类型 —— 默认按文本存，什么都能装，先记起来再说。',
      '等真的要按数字排序、按日期算了，再回头改。</p>',

    // ④ 让几张表连起来（v0.4.0 的新能力）
    '<h4 class="help-h">四、让几张表连起来</h4>',
    '<p>同一件事散在好几张表里时（客户名既在客户表、又在订单表、又在收款表），',
      '最烦的是改了一处、别处没跟着变。下面三样就是为这个准备的，入口都在',
      '数据页侧栏的<strong>「关系与同步」</strong>里。</p>',
    '<ul class="help-ul">',
    '<li><b>共通字段</b>：一次定义、多表引用。改一处，所有引用它的字段一起变 —— 界面上会打上 <code>⇄ 共通</code> 标记，让你一眼看出"这个是共享的"。</li>',
    '<li><b>同步规则</b>：改一张表，关联表按规则一起更新。规则<strong>随时可以关掉</strong>：关掉之后目标字段立刻恢复可编辑，出问题能一键止血。方向可选单向镜像 / 双向 / 只给建议；冲突时可选以源为准、以目标为准、或以最后改动为准。</li>',
    '<li><b>关联字段</b>：一条记录指向另一张表的一条或多条记录，用来把两张表扣在一起。</li>',
    '</ul>',
    '<p class="help-note">被同步写进去的值会带上来源标记 —— 你能一路跳回源记录，知道这个数是哪儿来的。</p>',

    // ⑤ 查找与筛选
    '<h4 class="help-h">五、查找与筛选（命名视图）</h4>',
    '<ul class="help-ul">',
    '<li><b>排序</b>：点表头，在「升序 → 降序 → 不排」之间循环。</li>',
    '<li><b>列筛选</b>：表头下方那行输入框，输入关键词就只显示包含它的行。</li>',
    '<li><b>存成视图</b>：把当前的筛选、排序、隐藏列存成一个有名字的视角（比如"未收款的订单"），下次点一下就回来 —— 入口在侧栏「视图」。<strong>不需要写任何查询语句。</strong></li>',
    '<li><b>翻页</b>：先显示 200 行，下面有「加载更多」。数据再多也不卡。</li>',
    '</ul>',

    // ⑥ 改与删
    '<h4 class="help-h">六、改与删</h4>',
    '<ul class="help-ul">',
    '<li><b>改</b>：双击格子就能改，改完点别处自动保存。</li>',
    '<li><b>"空"和"没填"是两回事</b>：空格子是空字符串；想表示没填过，用「设为 NULL」。两者在界面上分开显示。</li>',
    '<li><b>删</b>：勾选要删的行（可多选），点工具条「删除所选」。删了找不回，会先让你确认。</li>',
    '<li><b>改错了想退回</b>：侧栏「历史」里能看到最近改了什么、删了什么，逐条退回去（每张表最多留 500 条）。</li>',
    '</ul>',

    // ⑦ 拿出去（v1.11.0 新增的两个出口）
    '<h4 class="help-h">七、把表格拿出去</h4>',
    '<p>数据页侧栏有三个出口，<strong>它们不是重复的</strong>，按用途挑一个：</p>',
    '<table class="help-table">',
    '<thead><tr><th>出口</th><th>得到什么</th><th>什么时候用它</th></tr></thead>',
    '<tbody>',
    '<tr><td>导出当前表格</td><td>一个 <code>.xlsx</code> 文件</td><td>还要继续拿 Excel 算数、发给同事改</td></tr>',
    '<tr><td>打印当前表格</td><td>一个网页（打印视图）</td><td><strong>打出来贴墙上</strong>、或发给不看电子表格的人</td></tr>',
    '<tr><td>导出全部数据</td><td>整个数据目录的 CSV + 笔记 + 说明</td><td>搬家、留底、换电脑</td></tr>',
    '</tbody></table>',
    '<p class="help-note">「打印当前表格」生成的文件会用你的浏览器打开 —— 在那里按 <code>Ctrl+P</code> 就能打印，',
      '或者"另存为 PDF"。打印视图已经排好：<strong>A4 横向、表头每页重复、去掉了底色（省墨）</strong>，',
      '不用自己调。三种出口都<strong>只写新文件、绝不覆盖</strong>已有的东西。</p>',

    // ⑧ AI（v1.10.0 打通）
    '<h4 class="help-h">八、AI 帮忙（可选，默认关闭）</h4>',
    '<p>AI 在<strong>设置页 → AI 表格</strong>里开。开之前它一个字都不会发出去。</p>',
    '<ul class="help-ul">',
    '<li><b>默认走本机模型</b>：如果你装了 Ollama 或 LM Studio，模型跑在你自己的电脑上，' +
      '<strong>数据不出本机</strong>，也不会有任何费用。</li>',
    '<li><b>用云端服务时，默认只发"结构"</b>：表名、列名、类型会被发出去，' +
      '<strong>一行数据都不带</strong>。想让 AI 看到具体数据，要在对话框里<strong>自己勾选</strong>' +
      '「连同当前表格的数据行一起发送」，而且<strong>每次都要重新勾</strong>。</li>',
    '<li><b>点发送前能看见要发什么</b>：会先弹出一份<strong>完整原文</strong>（带字节数和行数），' +
      '你确认了才真的发。这条是刻意的 —— 不然"展示将发送的内容"就只是句空话。</li>',
    '<li><b>面板上永远写着数据去哪了</b>：是本机、是云端、还是关着，以及这次会话已经发出过多少行数据。</li>',
    '<li><b>聊天记录只存在本机</b>，跟着数据目录走，面板底部可以一键清空。</li>',
    '</ul>',
    '<p class="help-note">AI 在 DeskBase 里<strong>只给建议，不会自己动你的数据</strong> —— ' +
      '它不会替你改表、删行、导出。要动手的活永远由你点。</p>',

    // ⑨ 数据在哪 / 隐私
    '<h4 class="help-h">九、我的数据在哪、安全吗</h4>',
    '<ul class="help-ul">',
    '<li><b>就一个文件</b>：数据存在数据目录里（设置页「关于」能看到具体路径），复制到 U 盘就能带走。</li>',
    '<li><b>不联网</b>：程序自主联网<strong>默认是关的</strong>。AI、更新检查各自是独立开关，不开就一个包都不发。</li>',
    '<li><b>没有遥测</b>：埋点、崩溃上报这些东西不存在 —— 不是开关，是压根没写。</li>',
    '<li><b>备份</b>：数据页侧栏「备份数据」，生成的每一份都会<strong>立刻校验</strong>（非空 / 能还原 / 表数一致），不合格会直接告诉你。</li>',
    '<li><b>装到本机（可选）</b>：不点「安装到本机」就是一个字节的注册表都不写的便携版。' +
      '装了之后能在开始菜单找到、也能在 Windows 的「应用和功能」里正常卸载 —— ' +
      '<strong>卸载不会动你的数据目录</strong>。</li>',
    '</ul>',
    '<p class="help-note">崩溃也不怕：万一被强杀，下次启动会告诉你"上次没正常退出"，并把自检结果摆给你看 —— 不会默默吞掉。</p>',

    // ⑩ 常见错误对照
    '<h4 class="help-h">十、常见错误对照</h4>',
    '<table class="help-table">',
    '<thead><tr><th>现象</th><th>原因</th><th>怎么办</th></tr></thead>',
    '<tbody>',
    '<tr><td>金额填不进去 / 变奇怪</td><td>写成了"十二元"，或小数超过两位</td><td>用数字：12.34、¥1,234.50、12元 都行；金额最小到分</td></tr>',
    '<tr><td>提示"列名不允许"</td><td>用了空格、标点，或叫了 rowid</td><td>改名：用中文、字母、数字、下划线，且别以数字开头</td></tr>',
    '<tr><td>某一行点不动、改不了</td><td>这张表没有主键列，系统分不清第几行</td><td>新建表格时给一列打上「主键」</td></tr>',
    '<tr><td>目标字段灰着不能改</td><td>它被某条同步规则接管了</td><td>到「关系与同步」里把那条规则关掉，立刻恢复可编辑</td></tr>',
    '<tr><td>表打不开</td><td>可能损坏，或被别的程序占用</td><td>点「刷新」；还不行就重启程序</td></tr>',
    '<tr><td>AI 报"连不上"或"模型服务没启动"</td><td>本机模型（Ollama / LM Studio）没开着，或还在加载</td><td>先把它启动起来；或到设置页把服务商换成别家</td></tr>',
    '<tr><td>AI 说看不到数据</td><td>用云端服务时默认只发结构，没勾"连同数据行"</td><td>在对话框里勾上那个选项再问一次（本机模型不需要勾）</td></tr>',
    '<tr><td>导出很慢 / 提示行数很大</td><td>表很大，导出要一页页读完再写文件</td><td>等它跑完；期间程序其余部分仍可用</td></tr>',
    '</tbody></table>',

    // ⑪ 术语表
    '<h4 class="help-h">十一、术语对照</h4>',
    '<table class="help-table">',
    '<thead><tr><th>你看到的</th><th>意思</th></tr></thead>',
    '<tbody>',
    '<tr><td>列</td><td>一列，比如"名称""金额"</td></tr>',
    '<tr><td>记录</td><td>一行，一条完整的数据</td></tr>',
    '<tr><td>行号（rowid）</td><td>系统内部给每行编的号，<strong>不显示</strong>给你</td></tr>',
    '<tr><td>金额按"分"存</td><td>你写 12.34 元，里面对应 1234 分；显示时自动变回元</td></tr>',
    '<tr><td>视图</td><td>筛选 + 排序 + 隐藏列的组合，存下来一键回到这个视角</td></tr>',
    '<tr><td>本机模型 / 云端服务</td><td>模型跑在<strong>你自己电脑上</strong>（数据不出本机）/ 跑在厂商服务器上（数据会离开本机）</td></tr>',
    '<tr><td>结构信息</td><td>表名、列名、列类型 —— 不含任何一行具体数据</td></tr>',
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
