//! WebSocket 事件推送。
//!
//! 原桌面版用 `tauri::event` 把后端事件推给 WebView；Web 版改成 WebSocket：
//! 前端连上 `/api/ws` 后，后端把 `EventBus` 上所有事件原样转发过去。
//!
//! 协议设计：
//! - 服务端 → 客户端：每条消息是一个 JSON 对象 `{ "topic": "...", "payload": {...} }`，
//!   与 `BusMessage` 一一对应。前端按 `topic` 分发，与原来 `listen(topic)` 的用法一致。
//! - 客户端 → 服务端：只接受文本 `ping` 做保活探测，其余内容忽略。
//!
//! 刚连上时会先补发两样东西，避免前端漏掉连接之前就已发生的状态：
//! 1. `task-snapshot-event`：当前所有下载任务的列表。
//! 2. 之后才进入实时事件转发。

use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;

use crate::api::routes::AppState;
use crate::context::AppContext;
use crate::event_bus::topics;
use crate::extensions::AppContextExt;

/// 空闲心跳间隔：定期发 Ping，让中间的反向代理（飞牛的 Nginx 等）不会掐断长连接。
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotMessage<'a> {
    topic: &'a str,
    payload: Vec<crate::events::DownloadTaskEvent>,
}

/// `GET /api/ws` —— 升级为 WebSocket 并开始推送事件。
pub async fn handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let app = state.app.clone();
    ws.on_upgrade(move |socket| handle_socket(socket, app))
}

async fn handle_socket(socket: WebSocket, app: AppContext) {
    let (mut sender, mut receiver) = socket.split();

    // 先补发任务快照，再转发实时事件。顺序很重要：
    // 前端收到快照后会把任务列表整体替换，随后到达的 Update 事件才有正确的基线。
    let snapshot = SnapshotMessage {
        topic: topics::TASK_SNAPSHOT,
        payload: app.get_download_manager().task_snapshot(),
    };
    if let Ok(text) = serde_json::to_string(&snapshot) {
        if sender.send(Message::Text(text.into())).await.is_err() {
            return;
        }
    }

    let mut events = app.events().subscribe();

    // 心跳定时器：与事件转发放在同一个 select 里，避免额外开一个任务。
    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    // 第一次 tick 会立刻触发，跳过它，免得刚连上就发一个 Ping。
    heartbeat.tick().await;

    loop {
        tokio::select! {
            // 后端事件 → 客户端
            event = events.recv() => {
                match event {
                    Ok(msg) => {
                        let Ok(text) = serde_json::to_string(&msg) else {
                            continue;
                        };
                        if sender.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    // 客户端处理不过来，消息被丢弃。跳过这一条继续，不掐断连接。
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "WebSocket 客户端消费过慢，已丢弃部分事件");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }

            // 客户端 → 后端：只用于感知断开，以及响应 Ping。
            incoming = receiver.next() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(payload))) => {
                        if sender.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    // 文本消息当前没有语义，忽略即可。
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }

            // 保活 Ping
            _ = heartbeat.tick() => {
                if sender.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }

    tracing::debug!("WebSocket 连接已关闭");
}