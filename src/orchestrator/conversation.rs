//! История диалогов в памяти процесса. После перезапуска диалоги начинаются заново.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use thiserror::Error;
use uuid::Uuid;

use crate::llm::{ChatRole, Message};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConversationError {
    /// Диалог принадлежит другому пользователю. Наружу отдаём как «не найден».
    #[error("диалог не найден")]
    NotFound,
    #[error("неверный conversation_id")]
    InvalidId,
}

struct Conversation {
    owner: String,
    messages: Vec<Message>,
    last_used: u64,
}

#[derive(Default)]
struct Inner {
    map: HashMap<String, Conversation>,
    tick: u64,
}

pub struct ConversationStore {
    inner: Mutex<Inner>,
    max_conversations: usize,
    max_messages: usize,
}

impl Default for ConversationStore {
    fn default() -> Self {
        Self::new(1000, 40)
    }
}

impl ConversationStore {
    pub fn new(max_conversations: usize, max_messages: usize) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            max_conversations: max_conversations.max(1),
            max_messages: max_messages.max(2),
        }
    }

    /// Возвращает id диалога и его историю (без системного сообщения).
    /// Неизвестный id начинает новый диалог с этим id; чужой диалог недоступен.
    pub fn load(
        &self,
        id: Option<&str>,
        owner: &str,
    ) -> Result<(String, Vec<Message>), ConversationError> {
        let Some(id) = id.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok((Uuid::new_v4().to_string(), Vec::new()));
        };
        if id.len() > 64
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(ConversationError::InvalidId);
        }
        let guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        match guard.map.get(id) {
            Some(conv) if conv.owner != owner => Err(ConversationError::NotFound),
            Some(conv) => Ok((id.to_string(), conv.messages.clone())),
            None => Ok((id.to_string(), Vec::new())),
        }
    }

    pub fn save(&self, id: &str, owner: &str, messages: Vec<Message>) {
        let messages = trim_history(messages, self.max_messages);
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.map.get(id).is_some_and(|c| c.owner != owner) {
            return;
        }
        guard.tick += 1;
        let tick = guard.tick;
        guard.map.insert(
            id.to_string(),
            Conversation {
                owner: owner.to_string(),
                messages,
                last_used: tick,
            },
        );
        while guard.map.len() > self.max_conversations {
            let oldest = guard
                .map
                .iter()
                .min_by_key(|(_, c)| c.last_used)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(key) => {
                    guard.map.remove(&key);
                }
                None => break,
            }
        }
    }
}

/// Оставляет последние сообщения так, чтобы история начиналась с сообщения пользователя
/// и не разрывала пары «вызов функции → результат».
fn trim_history(messages: Vec<Message>, max: usize) -> Vec<Message> {
    if messages.len() <= max {
        return messages;
    }
    let start = messages.len() - max;
    let cut = messages[start..]
        .iter()
        .position(|m| m.role == ChatRole::User)
        .map(|p| start + p);
    match cut {
        Some(idx) => messages[idx..].to_vec(),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_and_existing_conversations() {
        let store = ConversationStore::default();
        let (id, history) = store.load(None, "u1").unwrap();
        assert!(history.is_empty());
        store.save(&id, "u1", vec![Message::user("q"), Message::assistant("a")]);
        let (_, history) = store.load(Some(&id), "u1").unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(
            store.load(Some(&id), "u2").unwrap_err(),
            ConversationError::NotFound
        );
        assert_eq!(
            store.load(Some("bad id!"), "u1").unwrap_err(),
            ConversationError::InvalidId
        );
    }

    #[test]
    fn trims_from_user_boundary() {
        let msgs = vec![
            Message::user("1"),
            Message::assistant_tool_calls(vec![]),
            Message::tool_result("x", "{}"),
            Message::assistant("a1"),
            Message::user("2"),
            Message::assistant("a2"),
        ];
        let trimmed = trim_history(msgs, 3);
        assert_eq!(trimmed.len(), 2);
        assert_eq!(trimmed[0].role, ChatRole::User);
    }

    #[test]
    fn evicts_least_recently_used() {
        let store = ConversationStore::new(2, 10);
        store.save("a", "u", vec![Message::user("1")]);
        store.save("b", "u", vec![Message::user("2")]);
        store.save("c", "u", vec![Message::user("3")]);
        let (_, a) = store.load(Some("a"), "u").unwrap();
        assert!(a.is_empty(), "самый старый диалог вытеснен");
        let (_, c) = store.load(Some("c"), "u").unwrap();
        assert_eq!(c.len(), 1);
    }
}
