# WebSocket 事件协议

> 本文描述 `pica-server` 与前端之间的事件推送通道。
> 所有结论均来自当前代码，未实现的设计不在此列。
>
> 实现位置：
> - 服务端连接处理：`src-server/src/api/ws.rs`
> - 事件总线与主题名：`src-server/src/event_bus.rs`
> - 事件体定义：`src-server/src/events.rs`
> - 前端客户端：`src/bindings.ts`

---

## 1. 为什么是 WebSocket

原桌面版用 `tauri::event` 把后端事件直接推给 WebView。Web 版没有这层 IPC，改为：

```
后端 EventBus (tokio broadcast)  →  GET /api/ws  →  浏览器 WebSocket
```

事件结构体完全复用 `src-server/src/events.rs`，只是投递方式从「发给单个 WebView」变成「广播给所有连接」。

---

## 2. 连接

### 2.1 端点

```
GET /api/ws
```

握手成功后升级为 WebSocket。该路由处在认证中间件 `require_auth` 的覆盖范围内（见 [API.md](./API.md)）。

### 2.2 认证

浏览器的 `WebSocket` 构造函数**无法携带自定义请求头**，因此 `/api/ws` 用不了 `Authorization: Bearer`。服务端在 `require_auth` 里为此留了查询串兜底分支：

| 方式 | 取值位置 | 说明 |
|---|---|---|
| `Authorization: Bearer <token>` | 请求头 | 中间件正常分支，非浏览器客户端可用 |
| `Authorization: Basic <base64>` | 请求头 | 同上，`user:pass` 形式 |
| `?token=<token>` | 查询串 | **兜底分支**，浏览器 WS 唯一可用路径 |

前端实际使用查询串形式（`src/bindings.ts` 的 `wsUrl()`）：

```ts
const proto = location.protocol === "https:" ? "wss:" : "ws:";
const query = token ? `?token=${encodeURIComponent(token)}` : "";
return `${proto}//${location.host}/api/ws${query}`;
```

几点行为说明（均来自代码）：

- `token` 为空时**仍然建立连接**，只是不带凭证。用于后端显式关闭认证（`PICA_AUTH_DISABLED`）的场景。
- `PICA_AUTH_DISABLED` 为真时，`require_auth` 在入口直接放行，所有请求不校验凭证。
- 未通过认证时握手失败，前端 `onclose` 触发并进入重连退避。
- 查询串里 `token=` 的值为空字符串时视为不存在（`extract_query_token` 里有 `!value.is_empty()` 判断）。

> **安全权衡**：查询串可能出现在反向代理的访问日志里。对单用户自部署的 NAS 场景可接受；若要收紧，应在反代层关闭对 `/api/ws` 的 query 记录，或改用一次性票据（不在本期范围）。

---

## 3. 消息格式

### 3.1 服务端 → 客户端

每条消息是一个 JSON 文本帧，对应 Rust 侧的 `BusMessage`：

```json
{ "topic": "download-task-event", "payload": { } }
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `topic` | `string` | 主题名，前端按它分发 |
| `payload` | `object` \| `array` | 已序列化的事件体 |

序列化时机：事件在**入队时就序列化成 `(topic, json)`**——`BusMessage.payload` 的类型是 `serde_json::Value`，因此每个 WebSocket 连接转发时只需再 `to_string` 一次，不必各自重新序列化事件结构体。

### 3.2 客户端 → 服务端

| 帧类型 | 服务端行为 |
|---|---|
| 文本消息（任意内容，含 `"ping"`） | **忽略** |
| `Message::Ping(payload)` | 回 `Message::Pong(payload)` |
| `Message::Close` 或流结束 | 退出循环，关闭连接 |
| 读取出错 | 退出循环，关闭连接 |

注意区分两件事：前端发的文本 `"ping"` 只是保活探针，服务端**不解析、不回应**；真正维持连接的是协议层 Ping/Pong（见 5.2）。

---

## 4. 主题（topic）

主题名集中定义在 `src-server/src/event_bus.rs` 的 `topics` 模块：

| 常量 | 主题字符串 | 当前是否发出 | 说明 |
|---|---|---|---|
| `DOWNLOAD_TASK` | `download-task-event` | ✅ | 下载任务创建 / 更新 |
| `TASK_SNAPSHOT` | `task-snapshot-event` | ✅ | 连接建立时的任务列表快照 |
| `LOG` | `log-event` | ✅ | 后端日志转发 |
| `DOWNLOAD_ALL_FAVORITES` | `download-all-favorites-event` | ❌ | **已定义但全库无 emit 调用点** |
| `UPDATE_DOWNLOADED_COMICS` | `update-downloaded-comics-event` | ❌ | **已定义但全库无 emit 调用点** |

后两个常量是历史遗留：它们在 `topics` 模块里声明，但代码库中没有任何 `emit` 调用，前端也没有订阅。**保留不影响功能；删除前需确认无外部消费者。**

实际 emit / 发送点（全量）：

| 位置 | 主题 | 方式 |
|---|---|---|
| `src-server/src/logger.rs:36` | `LOG` | 走总线 `emit` |
| `src-server/src/download_manager.rs:1175` | `DOWNLOAD_TASK` | 走总线 `emit` |
| `src-server/src/download_manager.rs:1191` | `DOWNLOAD_TASK` | 走总线 `emit` |
| `src-server/src/api/ws.rs:53` | `TASK_SNAPSHOT` | **不走总线**，每连接直接 `send` |

`TASK_SNAPSHOT` 之所以绕过总线：快照是「每个新连接各自一份」的，若走广播，每个新连接都会让所有老连接重收一遍。

---

## 5. 连接生命周期

### 5.1 快照先于实时事件

`handle_socket` 的顺序是**刻意的**：

```
1. 发 task-snapshot-event（当前所有任务的 DownloadTaskEvent[]）
2. app.events().subscribe()      ← 订阅发生在快照发送之后
3. 进入 select! 循环，转发实时事件
```

原因（代码注释原文）：前端收到快照后会把任务列表**整体替换**，随后到达的 `Update` 事件才有正确的基线。

副作用：「发快照」与「订阅」之间存在一个极小窗口，窗口内产生的事件不会被这个连接看到。由于快照已包含该时刻的完整状态，且窗口在微秒级，实践中不构成问题。若要严格无缝，需要改为先订阅再发快照并在前端去重——当前未实现。

### 5.2 心跳

| 方向 | 间隔 | 内容 |
|---|---|---|
| 服务端 → 客户端 | 30s | `Message::Ping(空 payload)` |
| 客户端 → 服务端 | 25s | `Message::Text("ping")` |

服务端常量 `HEARTBEAT_INTERVAL = Duration::from_secs(30)`。心跳定时器与事件转发放在**同一个 `tokio::select!`** 里，不额外开任务。定时器**第一次 tick 被显式跳过**（`heartbeat.tick().await;`），避免刚连上就发一个 Ping。

目的：让中间的反向代理（飞牛的 Nginx 等）不掐断长连接。

前端在 `onopen` 时立刻发一次 `"ping"`，随后起 25s `setInterval`。选 25s 是为了比服务端 30s 更早出手，保证代理侧始终有流量。`onclose` 时 `clearInterval`。

### 5.3 慢消费者

广播通道容量 `CHANNEL_CAPACITY = 1024`。客户端消费不过来时 `broadcast` 返回 `RecvError::Lagged(skipped)`：

```rust
Err(RecvError::Lagged(skipped)) => {
    tracing::warn!(skipped, "WebSocket 客户端消费过慢，已丢弃部分事件");
    // 不 break，跳过这条继续
}
```

选择**丢消息保连接**而不是断开。`DOWNLOAD_TASK` 的高频 `Update`（每张图片一次）在 20 并发下会很密集，断开重连的代价更高。

对应地，`EventBus` 提供了 `emit_throttled(topic, key, min_interval, event)` 按 key 合并高频事件，以及 `prune_throttle(older_than)` 清理节流表防止无限增长。

> ⚠️ **当前状态**：`emit_throttled` 与 `prune_throttle` 已实现，但**全库没有任何调用点**。`download_manager.rs:1175` 使用的是普通 `emit`。也就是说 1024 的容量上限目前是唯一的背压手段。若将来并发数上调，这里应优先接入 `emit_throttled`，key 用 `chapter_id`。

---

## 6. 事件体

### 6.1 `DownloadTaskEvent`（`download-task-event`）

Rust 侧用 `#[serde(tag = "event", content = "data")]`，即**内部标签枚举**，线上形态：

```json
{ "event": "Create", "data": { } }
{ "event": "Update", "data": { } }
```

#### `Create`

```jsonc
{
  "event": "Create",
  "data": {
    "state": "Downloading",
    "comic": { },              // Box<Comic>
    "chapterInfo": { },        // Box<ChapterInfo>
    "downloadedImgCount": 0,
    "totalImgCount": 42
  }
}
```

字段全部 camelCase。`Create` 携带完整的 `Comic` / `ChapterInfo`，因此**只有它能支撑前端渲染出完整条目**——这也是快照必须复用 `to_create_event()` 的原因。

#### `Update`

```jsonc
{
  "event": "Update",
  "data": {
    "chapterId": "...",
    "state": "Downloading",
    "downloadedImgCount": 7,
    "totalImgCount": 42,
    "retryCount": 2,        // 可选，为 None 时不出现
    "lastError": "timeout"  // 可选，为 None 时不出现
  }
}
```

`retryCount` / `lastError` 带 `#[serde(default, skip_serializing_if = "Option::is_none")]`，**成功时不出现**。这是刻意设计：旧前端不认识新字段会直接忽略，因此加字段是向后兼容的。

#### `state` 的取值

`state` 是 `DownloadTaskState`，**serde 默认命名，即 PascalCase**：

```
Pending | Downloading | Paused | Completed | Failed
```

其中 `Downloading` 是**瞬时态**，只存在于内存事件中。

> ⚠️ **大小写约定（by design，本期不改）**
>
> 数据库持久化用的是另一套枚举 `DbTaskState`，手写 `as_str()` / `parse()`、**没有 serde**、值是 **lowercase**。两者通过 `DownloadTaskState::to_db` / `from_db` 显式转换。
>
> 也就是说：**WS 上是 PascalCase，REST `/api/tasks*` 上是 lowercase。**
>
> 目前两条路径完全不交叉：前端所有进度状态都来自 WS（`src/types.ts:5` 用类型层面的 `Extract<DownloadTaskEvent, {event:'Create'}>` 把 WS 锁成唯一状态源），REST `TaskView.state` 从未被前端读取，`/api/tasks*` 只服务于青龙脚本的 HTTP 契约。
>
> **接缝警告**：若将来把 REST tasks 接入前端、并与 WS 数据合并，会立刻踩到大小写不一致。届时应统一在边界处转换，而不是改 `DownloadTaskState` 的 serde 命名（那会破坏已部署前端的兼容性）。

### 6.2 `LogEvent`（`log-event`）

```jsonc
{
  "timestamp": "2026-01-01T00:00:00Z",
  "level": "Info",
  "fields": { },
  "target": "pica_server::download_manager",
  "filename": "download_manager.rs",
  "line_number": 1175
}
```

结构体整体 `rename_all = "camelCase"`，但 `line_number` 字段被**单独** `#[serde(rename = "line_number")]` 改回 snake_case，故它是唯一的下划线字段。这是既有行为，前端按 `line_number` 取值。

### 6.3 `task-snapshot-event`

payload 是 `DownloadTaskEvent[]`（数组，不是单个对象）。

内容由 `DownloadManager::task_snapshot()` 构造，分两部分拼接并用 `HashSet` 去重：

1. **内存中的活任务优先** —— 它们持有完整的 `Comic` / `ChapterInfo`，能构造出信息最全的 `Create` 事件。
2. **DB 中的未完结任务补齐** —— 覆盖本进程没在跑、但历史遗留（或被恢复流程跳过）的任务，让重启后的前端也能看到它们。

因此快照里的元素**全部是 `Create` 形态**，与后续 `Update` 事件按 `chapterId` 对应。

---

## 7. 前端客户端行为

实现在 `src/bindings.ts`。对外保持与 tauri-specta 相同的 `events.xxx.listen(cb)` 形状——原版返回 `Promise<UnlistenFn>`，这里返回同步的取消订阅函数；调用方普遍写成 `await events.x.listen(...)`，await 一个函数同样安全。

### 7.1 主题映射

```ts
const TOPIC_MAP: Record<keyof EventMap, string> = {
  downloadTaskEvent: "download-task-event",
  logEvent:          "log-event",
  taskSnapshot:      "task-snapshot-event",
};
```

前端只订阅上表 3 个主题，恰好是服务端**实际发出**的 3 个。`download-all-favorites-event` 与 `update-downloaded-comics-event` 两端都没有消费者。

### 7.2 迟到订阅的回放

`dispatch()` 把每个 topic 的**最近一条** payload 存进 `lastPayload`。`subscribe()` 注册回调后立刻检查缓存并同步回调一次：

```ts
const cached = lastPayload.get(topic);
if (cached !== undefined) {
  try { cb({ payload: cached }); } catch (e) { /* 已记录，不影响其它订阅者 */ }
}
```

这是为了修「组件挂载晚于事件到达」的竞态——典型场景是任务快照：连接在应用启动时就建立，而进度面板可能晚几百毫秒才挂载。

注意缓存的是**最后一条**而非全部历史。对 `taskSnapshot` 与 `Update`（本身就是全量状态）成立；对 `logEvent`（追加语义）会丢中间行。

### 7.3 重连

```
onclose → scheduleReconnect() → 1000ms 后 connect()
          reconnectDelay = min(reconnectDelay * 2, 15000)
onopen  → reconnectDelay = 1000   （重置）
```

指数退避：1000 → 2000 → 4000 → 8000 → 15000 → 15000 …

- `onerror` **不做处理**，注释说明 `onclose` 会紧随其后，统一在那里重连。
- `manualClose` 标志位在 `disconnectEvents()`（登出）时置位，`scheduleReconnect()` 见到它直接返回，避免登出后仍在后台重连。
- `reconnectEvents()`（登录成功后调用）把 `manualClose` 复位并强制重建连接。
- `disconnectEvents()` 额外 `lastPayload.clear()`，防止下个账号拿到上个账号的缓存状态。
- `connect()` 幂等：处于 `OPEN` / `CONNECTING` 时直接返回。

### 7.4 消息解析

```ts
socket.onmessage = (ev) => {
  if (!ev.data || ev.data === "pong") return;
  const msg = JSON.parse(ev.data);   // 解析失败静默 return
  if (!msg.topic) return;
  dispatch(msg.topic, msg.payload);
};
```

单条消息解析失败只丢弃该条，不断连接。回调抛异常被 `try/catch` 包住并 `console.error`，不影响同 topic 的其它订阅者。

---

## 8. 与 REST 的分工

| | REST `/api/*` | WebSocket `/api/ws` |
|---|---|---|
| 用途 | 命令（登录、搜索、建任务、改配置） | 状态推送（进度、日志、快照） |
| 认证 | `Authorization: Bearer` | `?token=` 查询串 |
| 任务状态大小写 | lowercase（`DbTaskState`） | PascalCase（`DownloadTaskState`） |
| 前端消费者 | 部分（多个端点前端未用，属青龙契约） | 全部 3 个主题 |

设计上**前端只从 WS 读任务状态**，REST 只发命令。这样避免了双源合并与大小写转换。

---

## 9. 已知问题

### 9.1 `saveConfig` 请求体包装不一致 ⚠️

**与 WebSocket 无关，但同属前后端契约问题，在此登记。**

- 前端 `src/bindings.ts`：`post("/api/config", { config })` —— 多包了一层 `{ config: ... }`
- 后端 `src-server/src/api/routes.rs`：`save_config(Json(config): Json<Config>)` —— 期望**裸** `Config`

服务端会返回 400 / 422，配置保存从未成功过。**修复方案**（单独提交，不在文档提交内）：把前端改为 `post("/api/config", config)`。

### 9.2 `emit_throttled` 未接入

见 5.3。已实现，但无调用点。

### 9.3 未 emit 的主题常量

见第 4 节。`DOWNLOAD_ALL_FAVORITES` / `UPDATE_DOWNLOADED_COMICS` 为历史遗留。

---

## 10. 变更本协议时

1. **加字段**：一律带 `#[serde(default)]`，保持向后兼容。参照 `Update.retry_count` 的做法。
2. **改主题名**：同时改 `event_bus.rs` 的 `topics` 常量与 `bindings.ts` 的 `TOPIC_MAP`，两处必须一致。
3. **改 state 命名**：不要动 `DownloadTaskState` 的 serde 默认命名，已部署的前端依赖 PascalCase。需要区分持久化形态就加新枚举 + 显式转换，照 `DbTaskState` 的先例。
4. **提高并发**：先接入 `emit_throttled`，再考虑调 `CHANNEL_CAPACITY`。
