//! Responses 会话持久化（`previous_response_id` 支持）
//!
//! OpenAI Responses API 允许客户端只发本轮输入、用 `previous_response_id`
//! 引用上一轮，由服务端补齐历史。本模块把每轮的输入与输出落到磁盘，
//! 下一轮再沿 `previous_response_id` 链回溯展开。
//!
//! 落盘而非纯内存：codex 这类客户端会跨进程重启续用同一条链。
//!
//! 隔离：条目按客户端 Key id 分目录存放，且 [`ResponseStore::load`] 只接受
//! 同一 `key_id` 的条目。否则任意租户拿到别人的 response id 就能读出其
//! 完整对话历史。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 默认 TTL：30 天（与 OpenAI 官方 Responses 保留窗口一致）
pub const DEFAULT_TTL_SECS: u64 = 30 * 24 * 60 * 60;

/// 单条链最多回溯的轮数，防止坏链或环导致无限展开
pub const MAX_HISTORY_DEPTH: usize = 64;

/// 一轮 Responses 的持久化快照
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredResponse {
    pub id: String,
    /// 归属的客户端 Key id（跨 Key 读取一律拒绝）
    #[serde(default)]
    pub key_id: u64,
    pub model: String,
    /// 本轮的 Responses `output` 数组（原样保存，回放时复用进方向翻译）
    #[serde(default)]
    pub output: Vec<Value>,
    /// 上一轮 id，构成会话链
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    /// 本轮请求的 `instructions`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// 本轮请求的原始 `input`（回放时复用进方向翻译）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    /// 落盘时间（unix 秒），用于 TTL 判定
    #[serde(default)]
    pub stored_at: i64,
}

#[derive(Debug)]
pub enum StoreError {
    MissingId,
    NotFound,
    Expired,
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingId => write!(f, "response id 为空"),
            Self::NotFound => write!(f, "response 不存在"),
            Self::Expired => write!(f, "response 已过期"),
            Self::Io(e) => write!(f, "io 错误: {e}"),
            Self::Json(e) => write!(f, "json 错误: {e}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// 磁盘 response store
#[derive(Debug, Clone)]
pub struct ResponseStore {
    dir: PathBuf,
    ttl_secs: u64,
}

impl ResponseStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            ttl_secs: DEFAULT_TTL_SECS,
        }
    }

    pub fn with_ttl(mut self, ttl_secs: u64) -> Self {
        if ttl_secs > 0 {
            self.ttl_secs = ttl_secs;
        }
        self
    }

    /// 保存一轮快照。失败只记日志：会话续传是增强能力，不该让本轮请求失败。
    pub fn save(&self, resp: &StoredResponse) {
        if let Err(e) = self.try_save(resp) {
            tracing::warn!(
                response_id = %resp.id,
                "responses: 保存会话快照失败，本轮不可续传: {}",
                e
            );
        }
    }

    fn try_save(&self, resp: &StoredResponse) -> Result<(), StoreError> {
        if resp.id.is_empty() {
            return Err(StoreError::MissingId);
        }
        let path = self.path_for(resp.key_id, &resp.id)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(StoreError::Io)?;
        }

        let mut doc = resp.clone();
        if doc.stored_at == 0 {
            doc.stored_at = now_unix();
        }
        let data = serde_json::to_vec(&doc).map_err(StoreError::Json)?;

        // 先写临时文件再 rename：避免并发读到写了一半的 JSON。
        // 临时名带进程 id + 纳秒，避免同 id 并发写互相截断。
        let tmp = path.with_extension(format!(
            "tmp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(&tmp, data).map_err(StoreError::Io)?;
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(StoreError::Io(e));
        }
        Ok(())
    }

    /// 读取一轮快照。仅返回属于 `key_id` 的条目；过期条目顺手删除。
    pub fn load(&self, key_id: u64, id: &str) -> Result<StoredResponse, StoreError> {
        if id.is_empty() {
            return Err(StoreError::MissingId);
        }
        let path = self.path_for(key_id, id)?;
        let data = std::fs::read(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StoreError::NotFound
            } else {
                StoreError::Io(e)
            }
        })?;
        let doc: StoredResponse = serde_json::from_slice(&data).map_err(StoreError::Json)?;

        // 目录已按 key_id 隔离，这里再校验一次文档内容，防止手工搬动文件绕过隔离。
        if doc.key_id != key_id {
            return Err(StoreError::NotFound);
        }
        if doc.stored_at > 0 {
            let age = now_unix().saturating_sub(doc.stored_at).max(0) as u64;
            if age > self.ttl_secs {
                let _ = std::fs::remove_file(&path);
                return Err(StoreError::Expired);
            }
        }
        Ok(doc)
    }

    /// 删除所有超过 TTL 的条目（后台周期调用）
    pub fn purge_expired(&self) {
        let cutoff = now_unix().saturating_sub(self.ttl_secs as i64);
        let Ok(key_dirs) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let mut removed = 0usize;
        for key_dir in key_dirs.flatten() {
            let Ok(entries) = std::fs::read_dir(key_dir.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                // 以文档内 stored_at 为准，mtime 会被备份/拷贝工具改写。
                let expired = match std::fs::read(&path) {
                    Ok(data) => serde_json::from_slice::<StoredResponse>(&data)
                        .map(|doc| doc.stored_at > 0 && doc.stored_at < cutoff)
                        .unwrap_or(true),
                    Err(_) => false,
                };
                if expired && std::fs::remove_file(&path).is_ok() {
                    removed += 1;
                }
            }
        }
        if removed > 0 {
            tracing::debug!("responses: 清理 {} 条过期会话快照", removed);
        }
    }

    /// 启动后台周期清理任务
    pub fn spawn_background(self: std::sync::Arc<Self>) {
        let weak = std::sync::Arc::downgrade(&self);
        tokio::spawn(async move {
            let day = std::time::Duration::from_secs(24 * 3600);
            loop {
                tokio::time::sleep(day).await;
                let Some(store) = weak.upgrade() else { return };
                store.purge_expired();
            }
        });
    }

    #[cfg(test)]
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    fn path_for(&self, key_id: u64, id: &str) -> Result<PathBuf, StoreError> {
        let name = sanitize_id(id);
        if name.is_empty() {
            return Err(StoreError::MissingId);
        }
        Ok(self
            .dir
            .join(key_id.to_string())
            .join(format!("{name}.json")))
    }
}

/// 只保留字母数字、`_` 和 `-`，杜绝 `../` 等路径穿越。
fn sanitize_id(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(128)
        .collect()
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_store() -> (ResponseStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "kiro-rs-resp-store-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        (ResponseStore::new(&dir), dir)
    }

    fn sample(id: &str, key_id: u64) -> StoredResponse {
        StoredResponse {
            id: id.to_string(),
            key_id,
            model: "claude-opus-5".to_string(),
            output: vec![json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "hi"}]
            })],
            previous_response_id: None,
            instructions: Some("be terse".to_string()),
            input: Some(json!("hello")),
            stored_at: 0,
        }
    }

    #[test]
    fn save_then_load_roundtrip() {
        let (store, dir) = tmp_store();
        store.save(&sample("resp_abc", 7));

        let got = store.load(7, "resp_abc").unwrap();
        assert_eq!(got.id, "resp_abc");
        assert_eq!(got.model, "claude-opus-5");
        assert_eq!(got.instructions.as_deref(), Some("be terse"));
        assert_eq!(got.input, Some(json!("hello")));
        assert_eq!(got.output.len(), 1);
        assert!(got.stored_at > 0, "save 应补齐 stored_at");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_rejects_other_key_id() {
        let (store, dir) = tmp_store();
        store.save(&sample("resp_secret", 1));

        // 另一个 Key 拿到 id 也读不出来
        assert!(matches!(
            store.load(2, "resp_secret"),
            Err(StoreError::NotFound)
        ));
        assert!(store.load(1, "resp_secret").is_ok());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_missing_and_empty_id() {
        let (store, dir) = tmp_store();
        assert!(matches!(store.load(1, ""), Err(StoreError::MissingId)));
        assert!(matches!(store.load(1, "nope"), Err(StoreError::NotFound)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn expired_entry_is_rejected_and_removed() {
        let (store, dir) = tmp_store();
        let store = store.with_ttl(10);
        let mut doc = sample("resp_old", 3);
        doc.stored_at = now_unix() - 1_000;
        store.save(&doc);

        assert!(matches!(
            store.load(3, "resp_old"),
            Err(StoreError::Expired)
        ));
        // 过期读取应顺手删除，再读变成 NotFound
        assert!(matches!(
            store.load(3, "resp_old"),
            Err(StoreError::NotFound)
        ));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn purge_expired_removes_only_stale_entries() {
        let (store, dir) = tmp_store();
        let store = store.with_ttl(10);

        let mut old = sample("resp_stale", 1);
        old.stored_at = now_unix() - 1_000;
        store.save(&old);
        store.save(&sample("resp_fresh", 1));

        store.purge_expired();

        assert!(matches!(
            store.load(1, "resp_stale"),
            Err(StoreError::NotFound)
        ));
        assert!(store.load(1, "resp_fresh").is_ok());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn id_traversal_is_sanitized() {
        assert_eq!(sanitize_id("../../etc/passwd"), "etcpasswd");
        assert_eq!(sanitize_id("resp_a-1"), "resp_a-1");
        assert_eq!(sanitize_id("../.."), "");

        // 纯分隔符 id 被清空后必须报错，而不是落到目录本身
        let (store, dir) = tmp_store();
        assert!(matches!(store.load(1, "../.."), Err(StoreError::MissingId)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn save_keeps_entries_within_key_scoped_dir() {
        let (store, dir) = tmp_store();
        store.save(&sample("resp_x", 42));
        assert!(store.dir().join("42").join("resp_x.json").is_file());
        let _ = std::fs::remove_dir_all(dir);
    }
}
