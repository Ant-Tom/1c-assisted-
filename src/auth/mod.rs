//! Пользователи и роли. Права проверяет код, а не модель.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Только поиск контрагентов.
    Viewer,
    /// Поиск контрагентов и неоплаченные счета.
    Manager,
    /// Все функции чтения, включая сальдо и обороты.
    Accountant,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Manager => "manager",
            Role::Accountant => "accountant",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserIdentity {
    pub user_id: String,
    pub role: Role,
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("пользователь «{0}» не найден")]
    UnknownUser(String),
    #[error("не удалось прочитать файл пользователей {path}: {reason}")]
    Load { path: String, reason: String },
    #[error("ошибка в файле пользователей: {0}")]
    Invalid(String),
}

/// Сопоставляет внешний идентификатор пользователя с ролью.
/// Сейчас реализация на файле; позже сюда подключится Битрикс24.
#[async_trait]
pub trait IdentityResolver: Send + Sync {
    async fn resolve(&self, user_id: &str) -> Result<UserIdentity, AuthError>;
}

#[derive(Debug, Deserialize)]
struct UsersFile {
    #[serde(default)]
    users: Vec<UserEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UserEntry {
    id: String,
    role: Role,
}

/// Пользователи из `users.toml`:
///
/// ```toml
/// [[users]]
/// id = "ivanova"
/// role = "accountant"
/// ```
#[derive(Debug, Clone, Default)]
pub struct FileIdentityResolver {
    users: HashMap<String, Role>,
}

impl FileIdentityResolver {
    pub fn from_file(path: &Path) -> Result<Self, AuthError> {
        let raw = std::fs::read_to_string(path).map_err(|e| AuthError::Load {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        Self::from_toml_str(&raw)
    }

    pub fn from_toml_str(raw: &str) -> Result<Self, AuthError> {
        let parsed: UsersFile =
            toml::from_str(raw).map_err(|e| AuthError::Invalid(e.to_string()))?;
        let mut users = HashMap::with_capacity(parsed.users.len());
        for entry in parsed.users {
            let id = entry.id.trim().to_string();
            if id.is_empty() {
                return Err(AuthError::Invalid("пустой id пользователя".into()));
            }
            if users.insert(id.clone(), entry.role).is_some() {
                return Err(AuthError::Invalid(format!(
                    "пользователь «{id}» указан дважды"
                )));
            }
        }
        Ok(Self { users })
    }

    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, Role)>) -> Self {
        Self {
            users: pairs
                .into_iter()
                .map(|(id, role)| (id.to_string(), role))
                .collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.users.len()
    }

    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
    }
}

#[async_trait]
impl IdentityResolver for FileIdentityResolver {
    async fn resolve(&self, user_id: &str) -> Result<UserIdentity, AuthError> {
        self.users
            .get(user_id)
            .map(|role| UserIdentity {
                user_id: user_id.to_string(),
                role: *role,
            })
            .ok_or_else(|| AuthError::UnknownUser(user_id.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolves_users_from_toml() {
        let resolver = FileIdentityResolver::from_toml_str(
            r#"
            [[users]]
            id = "ivanova"
            role = "accountant"

            [[users]]
            id = "petrov"
            role = "manager"
            "#,
        )
        .unwrap();
        assert_eq!(resolver.len(), 2);
        let user = resolver.resolve("ivanova").await.unwrap();
        assert_eq!(user.role, Role::Accountant);
        assert!(matches!(
            resolver.resolve("nobody").await,
            Err(AuthError::UnknownUser(_))
        ));
    }

    #[test]
    fn rejects_bad_files() {
        assert!(
            FileIdentityResolver::from_toml_str("[[users]]\nid = \"a\"\nrole = \"admin\"\n")
                .is_err()
        );
        assert!(FileIdentityResolver::from_toml_str(
            "[[users]]\nid = \"a\"\nrole = \"viewer\"\n[[users]]\nid = \"a\"\nrole = \"manager\"\n"
        )
        .is_err());
    }
}
