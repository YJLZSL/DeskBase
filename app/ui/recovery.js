/* ============================================================
   DeskBase 崩溃恢复向导（recovery.js）
   ============================================================
   什么时候出现：**只在上次没有正常退出时**（判定在 Rust 侧 recovery.rs，
   依据是数据目录里的 boot.lock.json 有没有被正常退出流程删掉）。
   正常启动时它什么都不做 —— 不建节点、不拉状态以外的任何东西。

   它给用户什么（顺序即重要性）：
     1) 一句话说清发生了什么；
     2) 自检结论（quick_check 过没过）—— **原样报告，不粉饰**；
     3) 三个动作：生成数据快照 / 打开数据文件夹 / 知道了。

   刻意没有的东西：**「一键修复」**。向导的职责是让用户看清现场
   （自检 + 快照 + 数据目录），不是假装能把坏数据变好 ——
   那种按钮在出事时会让人做出错误的决定。

   文案里的数字（时间 / 体积 / WAL 残留）全部来自 Rust 侧的真实测量，
   这里只做本地化格式化，不加工语义。
   ============================================================ */
(function () {
  "use strict";
  if (window.DeskBaseRecovery) return;

  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  function fmtBytes(n) {
    const v = Number(n) || 0;
    if (v < 1024) return v + " B";
    if (v < 1024 * 1024) return (v / 1024).toFixed(1) + " KB";
    return (v / 1024 / 1024).toFixed(2) + " MB";
  }

  function fmtTime(ms) {
    const t = Number(ms) || 0;
    if (!t) return "—";
    try {
      return new Date(t).toLocaleString("zh-CN", { hour12: false });
    } catch (_) {
      return String(t);
    }
  }

  function el(tag, cls, text) {
    const n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text != null) n.textContent = text;
    return n;
  }

  function row(label, value) {
    const r = el("div", "recovery-row");
    r.appendChild(el("span", "recovery-label", label));
    r.appendChild(el("span", "recovery-value", value));
    return r;
  }

  function build(st) {
    const dlg = document.createElement("dialog");
    dlg.className = "recovery-dialog";
    dlg.id = "recovery-overlay";

    dlg.appendChild(el("h3", null, "上次没有正常退出"));

    dlg.appendChild(
      el(
        "p",
        "recovery-lead",
        "DeskBase 检测到上次运行没有走到正常退出这一步（强杀 / 断电 / 崩溃都算）。" +
          "启动时已经自动做过一次完整性自检，结论如下："
      )
    );

    const verdict = el(
      "div",
      "recovery-banner " + (st.quick_check_ok ? "is-ok" : "is-bad"),
      st.quick_check_ok
        ? "数据完整性自检通过：没有发现损坏迹象。"
        : "检测到数据文件异常：" + (st.quick_check || "未通过自检")
    );
    verdict.id = "recovery-verdict";
    dlg.appendChild(verdict);

    const rows = el("div", "recovery-rows");
    if (st.last_boot) {
      rows.appendChild(
        row(
          "上次启动",
          fmtTime(st.last_boot.started_at_ms) +
            " · v" +
            st.last_boot.version +
            " · PID " +
            st.last_boot.pid
        )
      );
    } else {
      rows.appendChild(row("上次启动", "（标记文件在场，但内容读不出来）"));
    }
    rows.appendChild(row("WAL 检查", st.wal_checkpoint || "—"));
    rows.appendChild(
      row(
        "数据文件",
        fmtBytes(st.db_bytes) +
          (st.wal_bytes ? "（WAL 残留 " + fmtBytes(st.wal_bytes) + "）" : "")
      )
    );
    rows.appendChild(row("自检时间", fmtTime(st.checked_at_ms)));
    dlg.appendChild(rows);

    dlg.appendChild(
      el(
        "p",
        "recovery-advice",
        st.quick_check_ok
          ? "如果刚才那次意外让你不放心，可以先「生成数据快照」留一份现场，再继续使用。"
          : "建议先「生成数据快照」保留现场，在弄清原因之前避免在这份数据上做大量改动 —— 生成快照不会动到原数据。"
      )
    );

    const result = el("div", "recovery-result");
    result.id = "recovery-result";
    dlg.appendChild(result);

    const actions = el("div", "recovery-actions");
    const snapBtn = el("button", "btn recovery-btn-snap", "生成数据快照");
    const revealBtn = el("button", "btn-ghost recovery-btn-reveal", "打开数据文件夹");
    const okBtn = el("button", "btn-ghost recovery-btn-ok", "知道了");
    snapBtn.type = revealBtn.type = okBtn.type = "button";
    actions.append(snapBtn, revealBtn, okBtn);
    dlg.appendChild(actions);

    const say = (msg, bad) => {
      result.textContent = msg;
      result.className = "recovery-result " + (bad ? "is-bad" : "is-ok");
    };

    snapBtn.addEventListener("click", async () => {
      snapBtn.disabled = true;
      say("正在生成快照…", false);
      try {
        const r = await window.__deskbase.call("recovery.snapshot");
        say("快照已生成：" + (r && r.path ? r.path : "（未返回路径）"), false);
      } catch (e) {
        say("生成快照失败：" + ((e && e.message) || e), true);
      } finally {
        snapBtn.disabled = false;
      }
    });

    revealBtn.addEventListener("click", async () => {
      try {
        await window.__deskbase.call("recovery.reveal");
      } catch (e) {
        say("打开失败：" + ((e && e.message) || e), true);
      }
    });

    let acked = false;
    const ack = () => {
      if (acked) return;
      acked = true;
      try {
        const p = window.__deskbase.call("recovery.ack");
        if (p && p.catch) p.catch(() => {});
      } catch (_) {}
    };
    okBtn.addEventListener("click", () => {
      ack();
      dlg.close();
    });
    // 用 Esc 关掉也算"知道了"—— 但一样要记审计，不然事后对不上号
    dlg.addEventListener("close", () => {
      ack();
      dlg.remove();
    });

    return dlg;
  }

  function show(st) {
    if (document.getElementById("recovery-overlay")) return;
    const dlg = build(st);
    document.body.appendChild(dlg);
    try {
      dlg.showModal();
    } catch (_) {
      dlg.setAttribute("open", ""); // 极端兜底：拿不到 top layer 也不能不显示
    }
  }

  async function boot() {
    // 等 IPC 桥就绪 —— 本文件可能在 app.js 初始化完成之前执行
    for (let i = 0; i < 60; i++) {
      if (window.__deskbase && typeof window.__deskbase.call === "function") break;
      await sleep(200);
    }
    if (!(window.__deskbase && typeof window.__deskbase.call === "function")) return;

    let st = null;
    for (let i = 0; i < 3; i++) {
      try {
        st = await window.__deskbase.call("recovery.status");
        break;
      } catch (_) {
        await sleep(300);
      }
    }
    if (st && st.unclean) show(st);
  }

  window.DeskBaseRecovery = { show: show, boot: boot };
  boot();
})();
