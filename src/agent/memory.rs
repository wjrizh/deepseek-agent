//! 对话记忆 —— 扩展点。当前为内存实现，未来可换 SQLite/Redis。

#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Role {
    User,
    Assistant,
}

/// 记忆存储 trait
pub trait Memory: Send + Sync {
    fn push(&mut self, msg: Message);
    fn history(&self) -> &[Message];
    fn clear(&mut self);
}

/// 内存实现
#[derive(Default)]
pub struct InMemory {
    messages: Vec<Message>,
}

impl InMemory {
    pub fn new() -> Self {
        Self { messages: Vec::new() }
    }
}

impl Memory for InMemory {
    fn push(&mut self, msg: Message) {
        self.messages.push(msg);
    }

    fn history(&self) -> &[Message] {
        &self.messages
    }

    fn clear(&mut self) {
        self.messages.clear();
    }
}