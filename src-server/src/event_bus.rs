//! 事件总线：替代原桌面版的 `tauri_specta::Event::emit`。
//!
//! 原实现把事件直接发给 WebView 窗口；Web 版改成广播给所有 WebSocket 连接。
//! 事件结构体本身完全复用（见 `events.rs`），只是投递方式变了。
//!
//! 设计要点：
//! - 用 `tokio::sync::broadcast`，慢订阅者只丢自己的消息，不阻塞发布方。
//! - 事件在入队时就序列化成 `(topic, json)`，避免每个连接各序列化一遍。
//! - `DownloadTaskEvent::Update` 是高频事件（每张图片一次），
//!   在 20 并发下会非常密集，因此这里提供 `emit_throttled` 做按 key 合并。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::broadcast;

/// 广播通道容量。慢客户端积压超过这个数就会开始丢旧消息。
const CHANNEL_CAPACITY: usize = 1024;

/// 一条待投递的事件。
#[derive(Debug, Clone, Serialize)]
pub struct BusMessage {
    /// 事件主题，前端按它分发（如 `download-task-event`）。
    pub topic: String,
    /// 已序列化的事件体（`#[serde(tag = "event", content = "data")]` 的形式）。
    pub payload: serde_json::Value,
}

/// 全局事件总线。廉价可克隆。
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<BusMessage>,
    /// 节流状态：key -> 上次发送时间。
    throttle: Arc<Mutex<HashMap<String, Instant>>>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(CHANNEL_CAPACITY);
        Self {
            tx,
            throttle: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 订阅事件流。每个 WebSocket 连接订阅一次。
    pub fn subscribe(&self) -> broadcast::Receiver<BusMessage> {
        self.tx.subscribe()
    }

    /// 当前活跃订阅者数量。为 0 时可跳过序列化开销。
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }

    /// 发布一条事件。没有任何订阅者时静默丢弃（不报错）。
    pub fn emit<T: Serialize>(&self, topic: &str, event: &T) {
        if self.tx.receiver_count() == 0 {
            return;
        }
        let Ok(payload) = serde_json::to_value(event) else {
            tracing::warn!(topic, "事件序列化失败，已丢弃");
            return;
        };
        // 发送失败只意味着当前没有订阅者，是正常情况。
        let _ = self.tx.send(BusMessage {
            topic: topic.to_string(),
            payload,
        });
    }

    /// 与 `emit` 相同，但接受已构造好的 JSON 值，避免重复序列化。
    pub fn emit_value(&self, topic: &str, payload: serde_json::Value) {
        if self.tx.receiver_count() == 0 {
            return;
        }
        let _ = self.tx.send(BusMessage {
            topic: topic.to_string(),
            payload,
        });
    }

    /// 带节流的事件发布。
    ///
    /// 同一个 `key` 在 `min_interval` 内只会真正发出第一条，
    /// 用于压制 `DownloadTaskEvent::Update` 这类高频事件。
    /// 返回值表示这次是否真的发出去了。
    pub fn emit_throttled<T: Serialize>(
        &self,
        topic: &str,
        key: &str,
        min_interval: Duration,
        event: &T,
    ) -> bool {
        {
            let mut guard = self.throttle.lock();
            let now = Instant::now();
            match guard.get(key) {
                Some(last) if now.duration_since(*last) < min_interval => return false,
                _ => {
                    guard.insert(key.to_string(), now);
                }
            }
        }
        self.emit(topic, event);
        true
    }

    /// 清理节流表中已经过期的条目，防止长期运行时无限增长。
    pub fn prune_throttle(&self, older_than: Duration) {
        let now = Instant::now();
        self.throttle
            .lock()
            .retain(|_, last| now.duration_since(*last) < older_than);
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

/// 所有事件主题名的集中定义。前端按同一组字符串做分发。
pub mod topics {
    pub const DOWNLOAD_TASK: &str = "download-task-event";
    pub const DOWNLOAD_ALL_FAVORITES: &str = "download-all-favorites-event";
    pub const UPDATE_DOWNLOADED_COMICS: &str = "update-downloaded-comics-event";
    pub const LOG: &str = "log-event";
    /// 后端新增：任务列表快照，供前端首次加载时补齐状态。
    pub const TASK_SNAPSHOT: &str = "task-snapshot-event";
}