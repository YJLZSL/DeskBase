//! AI 表格（v0.3.0 · P2）：配置与厂商清单。
//!
//! **隐私边界遵循 ADR-0017**（这份 ADR 早就把外部 AI 服务的规则写好了）：
//!   · AI 能力**默认关闭**，且是网络总开关之下的独立开关（关闭时网络 IO 必须为 0）；
//!   · **默认只发结构**（列名与类型），要发数据行必须**逐次显式授权 + 展示将发送的内容**；
//!   · 全部调用（含被拒）**记审计**；数据最小化（只发当前操作涉及的那一列）。
//!
//! 本轮（P2 第一步）只做配置地基：开关、厂商、endpoint、模型、Key 的读写。
//! **调用与授权闸门在下一步** —— 先让"能配"落地，再让"能发"落地。

use crate::model::Db;

/// 主流厂商清单。**加一家只加一行**（ADR-0020 的要求：厂商名单进配置数据，
/// 不散落在代码里）。只放"名字 + OpenAI 兼容 base_url"，不引入任何厂商 SDK ——
/// 这样新增一家不需要改代码逻辑，也避免把厂商凭据逻辑散到各处。
pub const PROVIDERS: &[(&str, &str, &str)] = &[
    ("deepseek", "深度求索 DeepSeek", "https://api.deepseek.com/v1"),
    ("qwen", "阿里通义千问", "https://dashscope.aliyuncs.com/compatible-mode/v1"),
    ("zhipu", "智谱 GLM", "https://open.bigmodel.cn/api/paas/v4"),
    ("kimi", "月之暗面 Kimi", "https://api.moonshot.cn/v1"),
    ("doubao", "字节豆包（方舟）", "https://ark.cn-beijing.volces.com/api/v3"),
    ("openai", "OpenAI", "https://api.openai.com/v1"),
    ("anthropic", "Anthropic Claude", "https://api.anthropic.com/v1"),
    ("gemini", "Google Gemini", "https://generativelanguage.googleapis.com/v1beta/openai"),
    ("custom", "自定义 / 本机 Ollama", "http://127.0.0.1:11434/v1"),
];

const K_ENABLED: &str = "ai.enabled";
const K_PROVIDER: &str = "ai.provider";
const K_BASE: &str = "ai.base_url";
const K_MODEL: &str = "ai.model";
const K_KEY: &str = "ai.api_key";

/// AI 配置。**默认全关**（enabled=false）—— 与 ADR-0017 的"默认关闭"一致。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AiSettings {
    pub enabled: bool,
    pub provider: String,
    pub base_url: String,
    pub model: String,
    /// 用户自己的 API Key。**存本机数据库（sys_meta），不写日志、不外传**。
    pub api_key: String,
}

impl Default for AiSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: "deepseek".into(),
            base_url: PROVIDERS[0].2.into(),
            model: String::new(),
            api_key: String::new(),
        }
    }
}

fn get(db: &Db, key: &str) -> Option<String> {
    db.meta_get(key)
}

pub fn load(db: &Db) -> AiSettings {
    let mut s = AiSettings::default();
    if let Some(v) = get(db, K_ENABLED) {
        s.enabled = v == "1" || v.eq_ignore_ascii_case("true");
    }
    if let Some(v) = get(db, K_PROVIDER) {
        s.provider = v;
    }
    if let Some(v) = get(db, K_BASE) {
        s.base_url = v;
    }
    if let Some(v) = get(db, K_MODEL) {
        s.model = v;
    }
    if let Some(v) = get(db, K_KEY) {
        s.api_key = v;
    }
    s
}

pub fn save(db: &mut Db, s: &AiSettings) -> Result<(), String> {
    // 选了预设厂商就把 base_url 归一到该厂商（除非用户手动改成了自定义）
    for (k, v) in [
        (K_ENABLED, if s.enabled { "1".to_string() } else { "0".to_string() }),
        (K_PROVIDER, s.provider.clone()),
        (K_BASE, s.base_url.clone()),
        (K_MODEL, s.model.clone()),
        (K_KEY, s.api_key.clone()),
    ] {
        db.meta_set(k, &v)
            .map_err(|e| format!("保存 AI 设置失败：{e}"))?;
    }
    Ok(())
}

/// 给前端下拉用的厂商清单。
pub fn providers() -> serde_json::Value {
    serde_json::Value::Array(
        PROVIDERS
            .iter()
            .map(|(id, label, base)| {
                serde_json::json!({ "id": id, "label": label, "base_url": base })
            })
            .collect(),
    )
}

// ---------------- 审计（ADR-0017 的第三条要求） ----------------
//
// 为什么**单独一份文件**而不是混在主日志里：审计要能被单独查看与留存，
// 而主日志是「排障用」的流水。混在一起，用户想回答「AI 到底往外发过什么」时，
// 得先在一屏排障信息里捞。
//
// 三条纪律（写进代码，别靠自觉）：
//   1. **只记元信息**：时间 / 厂商 / 动作 / 列名 / 行数 / 结果。**绝不记数据本体**；
//   2. **被拒的也要记** —— 用户点"取消"同样是审计事件（ADR-0017 明写）；
//   3. **绝不记 API Key** —— 连掩码都不记（掩码也是泄露面）。

/// 一条审计记录。字段刻意全是"元信息"：没有任何一格能装下一行数据。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AuditEntry {
    /// UTC 毫秒
    pub at_ms: i64,
    /// 动作：settings / prepare / call / denied / error
    pub action: String,
    /// 厂商 id（不记 base_url —— 那可能带查询串里的密钥）
    pub provider: String,
    /// 表名与列名（结构信息，ADR-0017 允许：默认只发结构）
    pub table: String,
    pub column: String,
    /// 涉及的行数（数据最小化的证据）
    pub rows: usize,
    /// 结果：ok / denied / failed / skipped
    pub result: String,
    /// 人话说明（失败原因等）。**调用方不得把数据塞进来**。
    pub note: String,
}

fn audit_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("logs").join("ai-audit.log")
}

/// 追加一条审计（JSON Lines：一行一条，方便 tail 与机器读）。
pub fn audit_append(data_dir: &std::path::Path, e: &AuditEntry) -> Result<(), String> {
    let p = audit_path(data_dir);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|x| format!("创建审计目录失败：{x}"))?;
    }
    let line = serde_json::to_string(e).map_err(|x| format!("审计序列化失败：{x}"))?;
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
        .map_err(|x| format!("打开审计文件失败：{x}"))?;
    writeln!(f, "{line}").map_err(|x| format!("写审计失败：{x}"))?;
    Ok(())
}

/// 读最近 n 条（新的在前）。文件不存在就返回空 —— "没记过"和"读不到"对界面是一件事。
pub fn audit_tail(data_dir: &std::path::Path, n: usize) -> Vec<AuditEntry> {
    let Ok(text) = std::fs::read_to_string(audit_path(data_dir)) else {
        return Vec::new();
    };
    let mut out: Vec<AuditEntry> = text
        .lines()
        .rev()
        .take(n)
        .filter_map(|l| serde_json::from_str::<AuditEntry>(l).ok())
        .collect();
    out.reverse();   // 调用方拿到的是时间正序，界面自行决定怎么显示
    out
}
#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        let d = std::env::temp_dir().join(format!("dkb_ai_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Db::open(&d).unwrap()
    }

    #[test]
    fn ai设置_默认是关闭的() {
        // ADR-0017：AI 能力默认关闭、默认不联网。这条必须有测试钉住。
        let s = load(&db());
        assert!(!s.enabled, "AI 默认必须是关的");
        assert!(s.api_key.is_empty(), "默认不该有任何凭据");
    }

    #[test]
    fn ai设置_存了能读回来() {
        let mut conn = db();
        let s = AiSettings {
            enabled: true,
            provider: "qwen".into(),
            base_url: "https://example.test/v1".into(),
            model: "qwen-turbo".into(),
            api_key: "sk-test".into(),
        };
        save(&mut conn, &s).unwrap();
        let back = load(&conn);
        assert!(back.enabled);
        assert_eq!(back.provider, "qwen");
        assert_eq!(back.base_url, "https://example.test/v1");
        assert_eq!(back.model, "qwen-turbo");
        assert_eq!(back.api_key, "sk-test");
        // 再存一次不炸（UPSERT）
        save(&mut conn, &back).unwrap();
        assert_eq!(load(&conn).provider, "qwen");
    }

    #[test]
    fn ai设置_厂商清单里有主流那几家() {
        let p = providers();
        let arr = p.as_array().unwrap();
        assert!(arr.len() >= 8, "主流厂商不该少于 8 家");
        let ids: Vec<String> = arr
            .iter()
            .map(|x| x["id"].as_str().unwrap_or("").to_string())
            .collect();
        for want in ["deepseek", "qwen", "zhipu", "kimi", "openai", "custom"] {
            assert!(ids.contains(&want.to_string()), "缺厂商 {want}");
        }
    }
    #[test]
    fn ai审计_写得进读得回且不含数据本体() {
        let dir = std::env::temp_dir().join(format!("deskbase-ai-audit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let e = AuditEntry {
            at_ms: 1_700_000_000_000,
            action: "prepare".into(),
            provider: "deepseek".into(),
            table: "客户".into(),
            column: "电话".into(),
            rows: 12,
            result: "ok".into(),
            note: "等待用户确认".into(),
        };
        audit_append(&dir, &e).unwrap();
        // 被拒的也要记（ADR-0017）
        audit_append(&dir, &AuditEntry { result: "denied".into(), ..e.clone() }).unwrap();
        let got = audit_tail(&dir, 10);
        assert_eq!(got.len(), 2, "两条都要在");
        assert_eq!(got[0].action, "prepare");
        assert_eq!(got[1].result, "denied", "被拒的也要留痕");
        // 数据本体绝不出现在审计里：整份文件不该含有"值"
        let raw = std::fs::read_to_string(dir.join("logs").join("ai-audit.log")).unwrap();
        assert!(!raw.contains("13800138000"), "审计不得含数据本体");
        assert!(!raw.contains("api_key") && !raw.contains("sk-"), "审计不得含 Key");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ai审计_没文件时返回空而不是报错() {
        let dir = std::env::temp_dir().join(format!("deskbase-ai-audit-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(audit_tail(&dir, 5).is_empty(), "没审计过就是空表，不是错误");
    }

}
