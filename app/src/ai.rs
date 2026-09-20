//! AI 表格（v0.3.0 · P2）：配置与厂商清单。
//!
//! **隐私边界遵循 ADR-0017**（这份 ADR 早就把外部 AI 服务的规则写好了）：
//!   · AI 能力**默认关闭**，且是网络总开关之下的独立开关（关闭时网络 IO 必须为 0）；
//!   · **默认只发结构**（列名与类型），要发数据行必须**逐次显式授权 + 展示将发送的内容**；
//!   · 全部调用（含被拒）**记审计**；数据最小化（只发当前操作涉及的那一列）。
//!
//! 本轮（P2 第一步）只做配置地基：开关、厂商、endpoint、模型、Key 的读写。
//! **调用与授权闸门在下一步** —— 先让"能配"落地，再让"能发"落地。

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

fn get(conn: &rusqlite::Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM sys_meta WHERE key = ?1",
        rusqlite::params![key],
        |r| r.get::<_, String>(0),
    )
    .ok()
}

pub fn load(conn: &rusqlite::Connection) -> AiSettings {
    let mut s = AiSettings::default();
    if let Some(v) = get(conn, K_ENABLED) {
        s.enabled = v == "1" || v.eq_ignore_ascii_case("true");
    }
    if let Some(v) = get(conn, K_PROVIDER) {
        s.provider = v;
    }
    if let Some(v) = get(conn, K_BASE) {
        s.base_url = v;
    }
    if let Some(v) = get(conn, K_MODEL) {
        s.model = v;
    }
    if let Some(v) = get(conn, K_KEY) {
        s.api_key = v;
    }
    s
}

pub fn save(conn: &rusqlite::Connection, s: &AiSettings) -> Result<(), String> {
    // 选了预设厂商就把 base_url 归一到该厂商（除非用户手动改成了自定义）
    for (k, v) in [
        (K_ENABLED, if s.enabled { "1".to_string() } else { "0".to_string() }),
        (K_PROVIDER, s.provider.clone()),
        (K_BASE, s.base_url.clone()),
        (K_MODEL, s.model.clone()),
        (K_KEY, s.api_key.clone()),
    ] {
        conn.execute(
            "INSERT INTO sys_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![k, v],
        )
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

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE sys_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .unwrap();
        conn
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
        let conn = db();
        let s = AiSettings {
            enabled: true,
            provider: "qwen".into(),
            base_url: "https://example.test/v1".into(),
            model: "qwen-turbo".into(),
            api_key: "sk-test".into(),
        };
        save(&conn, &s).unwrap();
        let back = load(&conn);
        assert!(back.enabled);
        assert_eq!(back.provider, "qwen");
        assert_eq!(back.base_url, "https://example.test/v1");
        assert_eq!(back.model, "qwen-turbo");
        assert_eq!(back.api_key, "sk-test");
        // 再存一次不炸（UPSERT）
        save(&conn, &back).unwrap();
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
}
