//! Named accounts: user-pasted per-provider keys with a custom label, the
//! omp-style explicit credential store. Every account renders its own card
//! (`<provider>@<id>`) named by the user ("Kimi — 公司号"), independent of
//! the auto-discovered login of the same family.
//!
//! Storage: %APPDATA%\Pane\accounts\<provider>.json —
//! `[{ "id", "name", "key", "createdAt" }]`, written atomically with
//! owner-only permissions (same guarantees as the One/New API key store).

use serde::{Deserialize, Serialize};

/// Providers that support named accounts. glm_cn has no implicit source at
/// all — named accounts are its only credentials.
pub const PROVIDERS: &[&str] = &["kimi", "opencode", "glm_cn"];

#[derive(Serialize, Deserialize, Clone)]
pub struct NamedAccount {
    pub id: String,
    pub name: String,
    pub key: String,
    pub created_at: i64,
}

/// Frontend view of a stored account — never carries the raw key back to
/// the webview, only a masked hint so the user can tell keys apart.
#[derive(Serialize, Clone)]
pub struct AccountDto {
    pub id: String,
    pub name: String,
    pub key_hint: String,
    pub created_at: i64,
}

fn store_path(provider: &str) -> Result<std::path::PathBuf, String> {
    if !PROVIDERS.contains(&provider) {
        return Err(format!("unknown account provider: {provider}"));
    }
    Ok(super::config_dir().join("accounts").join(format!("{provider}.json")))
}

pub fn load(provider: &str) -> Result<Vec<NamedAccount>, String> {
    let path = store_path(provider)?;
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Ok(Vec::new());
    };
    serde_json::from_str(&raw).map_err(|e| format!("parse {provider} accounts: {e}"))
}

/// Card-name prefix per account provider ("Kimi — 工作"), shared by the
/// snapshot builders and the cached-snapshot rename on account rename.
pub fn display_prefix(provider: &str) -> Option<&'static str> {
    match provider {
        "kimi" => Some("Kimi"),
        "opencode" => Some("OpenCode"),
        "glm_cn" => Some("GLM CN"),
        _ => None,
    }
}

pub fn list(provider: &str) -> Result<Vec<AccountDto>, String> {
    Ok(load(provider)?
        .into_iter()
        .map(|a| AccountDto {
            id: a.id.clone(),
            name: a.name.clone(),
            key_hint: mask_key(&a.key),
            created_at: a.created_at,
        })
        .collect())
}

fn save(provider: &str, accounts: &[NamedAccount]) -> Result<(), String> {
    let path = store_path(provider)?;
    if accounts.is_empty() {
        let _ = std::fs::remove_file(&path);
        return Ok(());
    }
    let dir = path
        .parent()
        .ok_or_else(|| "accounts dir path".to_string())?;
    std::fs::create_dir_all(dir).map_err(|e| format!("create accounts dir: {e}"))?;
    let raw =
        serde_json::to_string_pretty(accounts).map_err(|e| format!("serialize accounts: {e}"))?;
    super::onenewapi::store::atomic_write(&path, &raw)
        .map_err(|e| format!("write accounts: {e}"))
}

fn mask_key(key: &str) -> String {
    let trimmed = key.trim();
    if trimmed.len() <= 8 {
        return "••••".into();
    }
    format!("{}…{}", &trimmed[..4], &trimmed[trimmed.len() - 4..])
}

/// Short unique id for card ids (`kimi@ab12cd34`). Time + pid + counter
/// hashed to 8 hex chars — no dependency on a rng crate for something that
/// only needs to be unique per store file.
fn fresh_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0) as u64;
    let mix = nanos
        ^ (std::process::id() as u64).rotate_left(32)
        ^ SEQ.fetch_add(1, Ordering::Relaxed).rotate_left(17);
    // FxHash-style finalizer for decent spread.
    let mixed = (mix.wrapping_mul(0x517c_c1b7_2722_0a95)) >> 32;
    format!("{mixed:08x}")
}

pub fn add(provider: &str, name: &str, key: &str) -> Result<AccountDto, String> {
    let name = name.trim();
    let key = key.trim();
    if name.is_empty() {
        return Err("account name is required".into());
    }
    if key.is_empty() {
        return Err("account key is required".into());
    }
    if name.contains('@') {
        return Err("account name cannot contain '@'".into());
    }
    let mut accounts = load(provider)?;
    if accounts.iter().any(|a| a.name == name) {
        return Err(format!("an account named \"{name}\" already exists"));
    }
    let account = NamedAccount {
        id: fresh_id(),
        name: name.to_string(),
        key: key.to_string(),
        created_at: chrono::Utc::now().timestamp_millis(),
    };
    let dto = AccountDto {
        id: account.id.clone(),
        name: account.name.clone(),
        key_hint: mask_key(&account.key),
        created_at: account.created_at,
    };
    accounts.push(account);
    save(provider, &accounts)?;
    Ok(dto)
}

pub fn rename(provider: &str, id: &str, name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("account name is required".into());
    }
    if name.contains('@') {
        return Err("account name cannot contain '@'".into());
    }
    let mut accounts = load(provider)?;
    if accounts.iter().any(|a| a.name == name && a.id != id) {
        return Err(format!("an account named \"{name}\" already exists"));
    }
    let account = accounts
        .iter_mut()
        .find(|a| a.id == id)
        .ok_or_else(|| "account not found".to_string())?;
    account.name = name.to_string();
    save(provider, &accounts)
}

pub fn delete(provider: &str, id: &str) -> Result<(), String> {
    let mut accounts = load(provider)?;
    let before = accounts.len();
    accounts.retain(|a| a.id != id);
    if accounts.len() == before {
        return Err("account not found".into());
    }
    save(provider, &accounts)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The store is a plain JSON file under the real config dir; tests
    // exercise the pure pieces (masking, id uniqueness) only.
    #[test]
    fn mask_keeps_ends_hides_middle() {
        assert_eq!(mask_key("short"), "••••");
        assert_eq!(mask_key("sk-1234567890abcdef"), "sk-1…cdef");
    }

    #[test]
    fn fresh_ids_are_unique_and_hex() {
        let a = fresh_id();
        let b = fresh_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 8);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
