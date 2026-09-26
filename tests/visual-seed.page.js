/* ============================================================
   人工交互验收 · 视觉走查的"造景"脚本
   ============================================================
   为什么需要它：`ui-walkthrough.cjs` 走的是**空库**，截出来的每张页面
   都是空状态 —— 而空状态恰恰是最不容易暴露问题的状态（没有真实数据、
   没有多行文本、没有长表名、没有滚动条）。

   真正会被用户挑出来的问题几乎都出现在**有数据**的时候：
     · 长表名 / 长单元格 / 数字与金额混排时的对齐
     · 表格列很多时的横向滚动与表头粘性
     · 大量文字时卡片高度、换行、留白
     · 面板打开时的遮罩与层级
   所以这里造一份**像真的**台账数据，供外部驱动逐个状态截图。

   与 `walkthrough-boot.page.js` 同一条纪律：**这里不做任何点击与截图**，
   只负责把数据准备好。节奏由外部（tests/visual-acceptance.cjs）掌握 ——
   两套机制都管时序的话，出问题就说不清是谁的。
   ============================================================ */
(async () => {
  "use strict";
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const call = (cmd, args) => window.__deskbase.call(cmd, args || {});
  window.__visualSeeded = false;

  // 等桥就绪
  for (let i = 0; i < 80; i++) {
    if (window.__deskbase && typeof window.__deskbase.call === "function") break;
    await sleep(200);
  }

  const TEMPLATE = [
    "供应商",
    "联系人",
    "电话",
    "金额",
    "是否已结清",
    "签单日期",
    "备注",
  ];

  // 刻意混入几类"真实脏数据"：长中文名、带引号与逗号的备注、超长备注、
  // 空值、大金额、前导零编号 —— 这些是排版最容易崩的地方。
  const NAMES = [
    "北京甲贸易有限公司",
    "上海乙实业（集团）股份有限公司",
    "广州丙科技",
    "成都丁商贸有限责任公司",
    "杭州戊电子",
    "深圳市己供应链管理有限公司",
    "武汉庚机械",
    "西安辛建材经营部",
  ];
  const CONTACTS = ["张伟", "李娜", "王芳", "刘强", "陈静", "杨帆", "赵敏", "孙磊"];
  const NOTES = [
    "",
    "月结 30 天",
    "含运费，需开专票",
    "客户要求分两次发货，第二批下月初",
    "备注里带「引号」与,逗号，还有；分号",
    "这批货有质量问题，已协商换货，对方承担运费并承诺下周三之前把替换件送到仓库，届时需要仓管当面验货签字确认，未确认前不要入库",
    "已结清",
  ];

  function rowsFor(n) {
    const out = [];
    for (let i = 0; i < n; i++) {
      out.push([
        NAMES[i % NAMES.length] + (i % 7 === 0 ? "（华东区）" : ""),
        CONTACTS[i % CONTACTS.length],
        "138" + String(10000000 + i).slice(0, 8),
        (i * 137.37 + 12.5).toFixed(2),
        i % 2 ? "是" : "否",
        `2026-0${1 + (i % 9)}-${String(1 + (i % 28)).padStart(2, "0")}`,
        NOTES[i % NOTES.length],
      ]);
    }
    return out;
  }

  async function ensureTable(name, rowCount) {
    const list = await call("schema.listTables");
    if (Array.isArray(list) && list.some((t) => t.name === name)) return false;
    await call("schema.createTable", {
      spec: {
        name: name,
        comment: "视觉验收造的数据（不是真实业务数据）",
        columns: [
          { name: TEMPLATE[0], ty: "text", not_null: false, default: null, primary_key: false, comment: null, shared: null, link: null, lookup: null, rollup: null },
          { name: TEMPLATE[1], ty: "text", not_null: false, default: null, primary_key: false, comment: null, shared: null, link: null, lookup: null, rollup: null },
          { name: TEMPLATE[2], ty: "text", not_null: false, default: null, primary_key: false, comment: null, shared: null, link: null, lookup: null, rollup: null },
          { name: TEMPLATE[3], ty: "money", not_null: false, default: null, primary_key: false, comment: null, shared: null, link: null, lookup: null, rollup: null },
          { name: TEMPLATE[4], ty: "boolean", not_null: false, default: null, primary_key: false, comment: null, shared: null, link: null, lookup: null, rollup: null },
          { name: TEMPLATE[5], ty: "date", not_null: false, default: null, primary_key: false, comment: null, shared: null, link: null, lookup: null, rollup: null },
          { name: TEMPLATE[6], ty: "text", not_null: false, default: null, primary_key: false, comment: null, shared: null, link: null, lookup: null, rollup: null },
        ],
      },
    });
    // 分批插入（一批 200）：一次性插几千行会让启动慢到看不出是哪一步卡住
    const all = rowsFor(rowCount);
    for (let i = 0; i < all.length; i += 200) {
      await call("schema.insertRows", {
        table: name,
        columns: TEMPLATE.slice(),
        rows: all.slice(i, i + 200),
      });
    }
    return true;
  }

  try {
    // 一张"日常规模"的表，一张"名字长到会折行、行数多到要滚动"的表，再加几篇笔记。
    //
    // ⚠️ 表名的字符集比想象中窄（`validate_identifier`）：**只能中文/字母/数字/下划线**，
    // 且**不能以数字开头**。第一版起的是「2026年度华东区…」被拒（数字开头），
    // 第二版带全角括号「（本年度）」也被拒 —— 两条限制都是对的，
    // 所以这里改用纯汉字的长名字。**长度**才是要验的排版压力。
    await ensureTable("供应商台账", 48);
    await ensureTable("华东区供应商结算明细汇总表本年度全量", 260);

    const notes = await call("note.list");
    if (!Array.isArray(notes) || notes.length < 3) {
      const seeds = [
        ["供应商管理办法（草稿）", "# 供应商管理办法\n\n## 一、准入\n\n新供应商需提供营业执照、开户许可证与最近一年的对账单。\n\n## 二、结算\n\n- 月结 30 天\n- 需开增值税专用发票\n- 对账差异超过 5% 时暂停付款\n"],
        ["八月对账记录", "八月对账记录\n\n与「北京甲贸易有限公司」核对：\n\n| 项目 | 金额 |\n|---|---|\n| 应收 | 128,340.00 |\n| 实收 | 128,000.00 |\n| 差异 | 340.00（运费）|\n\n差异已确认，下月一并结算。"],
        ["仓库盘点注意事项", "仓库盘点注意事项\n\n1. 盘点前停止出入库\n2. 两人一组，一人清点一人记录\n3. 差异必须当天复盘，不要拖到下周\n"],
      ];
      for (const [title, body] of seeds) {
        try {
          // `note.create` 只建一个空笔记（参数只有 title），正文要再走一次 `note.save`。
          // 这里分两步写，是为了跟界面上真实的"新建 → 打字 → 自动保存"同一条路 ——
          // 造景走的路越接近真实操作，截图才越有参考价值。
          const n = await call("note.create", { title });
          const id = n && (n.id || n.note_id);
          if (id) await call("note.save", { id, title, content: body });
        } catch (_) {
          /* 接口不匹配就跳过 —— 造景失败不该让整轮走查崩掉 */
        }
      }
    }
    window.__visualSeeded = true;
    try {
      await call("app.smokeProgress", { msg: "视觉验收：造景完成（2 张表 + 3 篇笔记）" });
    } catch (_) {}
  } catch (e) {
    window.__visualSeedError = String((e && e.message) || e);
    try {
      await call("app.smokeProgress", { msg: "视觉验收：造景失败 —— " + window.__visualSeedError });
    } catch (_) {}
  }
})();
