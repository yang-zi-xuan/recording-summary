//! API Key 存储。
//!
//! 见技术方案 §10.4。**不落盘到数据库、不写进配置文件** ——
//! 配置文件会被同步、备份、截图。
//!
//! 解析顺序:
//!   1. `RECSUM_API_KEY` 环境变量(CI / 临时用)
//!   2. 操作系统凭据管理器(keyring)
//!   3. 都没有 → 报错并给出可行动提示

use anyhow::{anyhow, Result};

pub const KEYRING_SERVICE: &str = "recording-summary";
pub const KEYRING_USER: &str = "llm-api-key";
pub const ENV_VAR: &str = "RECSUM_API_KEY";

/// 从环境变量取 key。
pub fn api_key_from_env() -> Option<String> {
    std::env::var(ENV_VAR).ok().filter(|s| !s.trim().is_empty())
}

/// 存入系统凭据管理器。
pub fn store_api_key(key: &str) -> Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
        .map_err(|e| anyhow!("无法访问系统凭据管理器: {e}"))?;
    entry
        .set_password(key)
        .map_err(|e| anyhow!("写入凭据管理器失败: {e}"))?;
    Ok(())
}

/// 读取 API Key。环境变量优先。
pub fn load_api_key() -> Result<Option<String>> {
    if let Some(k) = api_key_from_env() {
        return Ok(Some(k));
    }
    let entry = match keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!("凭据管理器不可用: {e}");
            return Ok(None);
        }
    };
    match entry.get_password() {
        Ok(k) if !k.trim().is_empty() => Ok(Some(k)),
        Ok(_) => Ok(None),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => {
            tracing::debug!("读取凭据失败: {e}");
            Ok(None)
        }
    }
}

/// 删除已保存的 key。
pub fn delete_api_key() -> Result<bool> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
        .map_err(|e| anyhow!("无法访问系统凭据管理器: {e}"))?;
    match entry.delete_credential() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(e) => Err(anyhow!("删除凭据失败: {e}")),
    }
}

/// 掩码显示,如 `sk-1***abcd`。
pub fn mask(key: &str) -> String {
    let n = key.chars().count();
    if n <= 8 {
        return "*".repeat(n.max(1));
    }
    let head: String = key.chars().take(4).collect();
    let tail: String = key.chars().skip(n - 4).collect();
    format!("{head}****{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_hides_middle() {
        assert_eq!(mask("sk-1234567890abcdef"), "sk-1****cdef");
        assert_eq!(mask("short"), "*****");
        assert_eq!(mask(""), "*");
    }

    #[test]
    fn mask_never_reveals_full_key() {
        let k = "sk-abcdefghijklmnop";
        let m = mask(k);
        assert!(!m.contains("efghijklm"));
        assert!(m.len() < k.len() + 6);
    }

    #[test]
    fn env_var_name_is_stable() {
        assert_eq!(ENV_VAR, "RECSUM_API_KEY");
    }
}
