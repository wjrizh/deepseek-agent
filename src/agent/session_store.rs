//! 会话持久化：把 `session_id` + `parent_message_id` 存盘，
//! 下次启动可恢复上次对话继续聊（服务端已存历史，本地只需这两个值）。
//!
//! 文件：`~/.deepseek-agent/session.json`（0600 权限），
//! 结构兼容后续「多会话选择」扩展（sessions 数组 + active）。

use crate::error::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 单个会话记录。续接上下文只需 session_id + parent_message_id。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_id: String,
    /// 上一轮 response_message_id，作为下轮 parent_message_id
    #[serde(default)]
    pub parent_message_id: Option<i64>,
    /// 会话标题（来自 SSE Title 事件，可能为空）
    #[serde(default)]
    pub title: Option<String>,
    /// 最后活跃时间（Unix 秒）
    #[serde(default)]
    pub updated_at: u64,
}

impl SessionRecord {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            parent_message_id: None,
            title: None,
            updated_at: now_secs(),
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 持久化文件整体结构。第一步只用 sessions[0]/active，第二步扩展多会话。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionStore {
    /// 当前活跃会话 id
    #[serde(default)]
    pub active: Option<String>,
    /// 已知会话列表
    #[serde(default)]
    pub sessions: Vec<SessionRecord>,
    /// 文件路径（不序列化）
    #[serde(skip)]
    path: PathBuf,
}

impl SessionStore {
    /// 从文件加载；文件不存在/损坏则返回空 store（不报错）。
    pub fn load(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        match std::fs::read_to_string(&path) {
            Ok(data) => match serde_json::from_str::<SessionStore>(&data) {
                Ok(mut s) => {
                    s.path = path;
                    s
                }
                Err(_) => Self {
                    path,
                    ..Default::default()
                },
            },
            Err(_) => Self {
                path,
                ..Default::default()
            },
        }
    }

    /// 返回当前活跃会话（按 active 查，找不到则取最后一个）。
    pub fn active_record(&self) -> Option<&SessionRecord> {
        if let Some(id) = &self.active
            && let Some(r) = self.sessions.iter().find(|s| &s.session_id == id)
        {
            return Some(r);
        }
        self.sessions.last()
    }

    /// 记录/更新一个会话并设为活跃，落盘。
    pub fn upsert_active(&mut self, rec: SessionRecord) -> Result<()> {
        if let Some(existing) = self
            .sessions
            .iter_mut()
            .find(|s| s.session_id == rec.session_id)
        {
            *existing = rec.clone();
        } else {
            self.sessions.push(rec.clone());
        }
        self.active = Some(rec.session_id);
        self.save()
    }

    /// 更新活跃会话的 parent_message_id（每轮回复后调用），落盘。
    pub fn update_parent(&mut self, session_id: &str, parent_message_id: Option<i64>) -> Result<()> {
        if let Some(r) = self
            .sessions
            .iter_mut()
            .find(|s| s.session_id == session_id)
        {
            r.parent_message_id = parent_message_id;
            r.updated_at = now_secs();
            self.save()?;
        }
        Ok(())
    }

    /// 更新活跃会话的标题（收到 Title 事件时调用），落盘。
    pub fn update_title(&mut self, session_id: &str, title: &str) -> Result<()> {
        if let Some(r) = self
            .sessions
            .iter_mut()
            .find(|s| s.session_id == session_id)
            && r.title.as_deref() != Some(title)
        {
            r.title = Some(title.to_string());
            self.save()?;
        }
        Ok(())
    }

    pub fn set_active(&mut self, session_id: &str) -> Result<()> {
        self.active = Some(session_id.to_string());
        self.save()
    }

    pub fn parent_of(&self, session_id: &str) -> Option<i64> {
        self.sessions
            .iter()
            .find(|s| s.session_id == session_id)
            .and_then(|s| s.parent_message_id)
    }

    pub fn contains(&self, session_id: &str) -> bool {
        self.sessions.iter().any(|s| s.session_id == session_id)
    }

    pub fn remove(&mut self, session_id: &str) -> Result<()> {
        self.sessions.retain(|s| s.session_id != session_id);
        if self.active.as_deref() == Some(session_id) {
            self.active = self.sessions.last().map(|s| s.session_id.clone());
        }
        self.save()
    }

    pub fn activate(&mut self, rec: SessionRecord) -> Result<()> {
        self.upsert_active(rec)
    }

    fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let data = serde_json::to_string_pretty(self)?;
        std::fs::write(&self.path, data)?;
        // 权限收紧（会话 id 也算敏感）
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}