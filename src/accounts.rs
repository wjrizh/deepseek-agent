//! 多账号管理：账号清单、增删、切换、每账号路径解析。

use crate::error::{AgentError, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountEntry {
    pub name: String,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub last_used: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AccountsFile {
    #[serde(default)]
    pub accounts: Vec<AccountEntry>,
    #[serde(default)]
    pub default: Option<String>,
}

pub struct AccountManager {
    root: PathBuf,
    file: PathBuf,
    data: AccountsFile,
}

impl AccountManager {
    pub fn load_default() -> Result<Self> {
        let home = dirs::home_dir().ok_or_else(|| AgentError::Config("无 HOME".into()))?;
        Self::load(home.join(".deepseek-agent"))
    }

    pub fn load(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let file = root.join("accounts.json");
        let data = match std::fs::read_to_string(&file) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => AccountsFile::default(),
        };
        Ok(Self { root, file, data })
    }

    pub fn list(&self) -> &[AccountEntry] {
        &self.data.accounts
    }

    pub fn exists(&self, name: &str) -> bool {
        self.data.accounts.iter().any(|a| a.name == name)
    }

    pub fn validate_name(name: &str) -> Result<()> {
        if name.is_empty()
            || name.contains('/')
            || name.contains('\\')
            || name.contains("..")
            || name.contains(char::is_whitespace)
        {
            return Err(AgentError::Config(format!("非法账号名: {name}")));
        }
        Ok(())
    }

    pub fn add(&mut self, name: &str) -> Result<()> {
        Self::validate_name(name)?;
        if self.exists(name) {
            return Err(AgentError::Config(format!("账号已存在: {name}")));
        }
        let is_first = self.data.accounts.is_empty();
        self.data.accounts.push(AccountEntry {
            name: name.to_string(),
            created_at: now_secs(),
            last_used: now_secs(),
        });
        if is_first {
            self.data.default = Some(name.to_string());
        }
        std::fs::create_dir_all(self.account_dir(name))?;
        self.save()?;
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> Result<()> {
        let dir = self.account_dir(name);
        self.data.accounts.retain(|a| a.name != name);
        if self.data.default.as_deref() == Some(name) {
            self.data.default = self.data.accounts.first().map(|a| a.name.clone());
        }
        self.save()?;
        let _ = std::fs::remove_dir_all(dir);
        Ok(())
    }

    pub fn rename(&mut self, old: &str, new: &str) -> Result<()> {
        Self::validate_name(new)?;
        if !self.exists(old) {
            return Err(AgentError::Config(format!("账号不存在: {old}")));
        }
        if self.exists(new) {
            return Err(AgentError::Config(format!("账号已存在: {new}")));
        }
        let old_dir = self.account_dir(old);
        let new_dir = self.account_dir(new);
        if old_dir.exists() {
            std::fs::rename(&old_dir, &new_dir)
                .map_err(|e| AgentError::Config(format!("重命名目录失败: {e}")))?;
        }
        if let Some(entry) = self.data.accounts.iter_mut().find(|a| a.name == old) {
            entry.name = new.to_string();
        }
        if self.data.default.as_deref() == Some(old) {
            self.data.default = Some(new.to_string());
        }
        self.save()?;
        let la = self.root.join("last_account");
        if let Ok(s) = std::fs::read_to_string(&la)
            && s.trim() == old {
                let _ = std::fs::write(&la, new);
            }
        Ok(())
    }

    pub fn touch(&mut self, name: &str) -> Result<()> {
        if let Some(a) = self.data.accounts.iter_mut().find(|a| a.name == name) {
            a.last_used = now_secs();
        }
        self.data.default = Some(name.to_string());
        self.save()?;
        let _ = std::fs::write(self.root.join("last_account"), name);
        Ok(())
    }

    pub fn default_account(&self) -> Option<String> {
        if let Ok(s) = std::fs::read_to_string(self.root.join("last_account")) {
            let s = s.trim().to_string();
            if !s.is_empty() && self.exists(&s) {
                return Some(s);
            }
        }
        self.data.default.clone()
    }

    pub fn account_dir(&self, name: &str) -> PathBuf {
        self.root.join("accounts").join(name)
    }

    pub fn token_file(&self, name: &str) -> PathBuf {
        self.account_dir(name).join("token.json")
    }

    pub fn session_file(&self, name: &str) -> PathBuf {
        self.account_dir(name).join("session.json")
    }

    pub fn browser_profile(&self, name: &str) -> PathBuf {
        self.account_dir(name).join("browser")
    }

    fn save(&self) -> Result<()> {
        std::fs::create_dir_all(&self.root)?;
        let data = serde_json::to_string_pretty(&self.data)?;
        std::fs::write(&self.file, data)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.file, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}