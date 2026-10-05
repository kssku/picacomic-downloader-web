# picacomic-downloader-web — HTTP API 契约

> 本文档描述 `src-server` 暴露的 HTTP / WebSocket 接口。
> 路由表逐条对照 `src-server/src/api/routes.rs`，字段类型对照 `src-server/src/types.rs` 与 `src-server/src/api/commands.rs`。
> 最后核对版本：`SCHEMA_VERSION = 1`，路由注册 26 条。

---

## 一、认证

### 机制

认证由一层 axum 中间件 `require_auth`（`src-server/src/auth.rs:116`）统一施加，
**包裹了全部路由**（包括 `/ws`）：

```rust
protected
    .merge(public)          // public 也在这层 layer 之下
    .route("/ws", get(ws::handler))
    .layer(axum::middleware::from_fn_with_state(auth.clone(), require_auth))
```

中间件内部靠白名单放行，而不是靠路由分组：

```rust
// src-server/src/auth.rs:104
const PUBLIC_PATHS: [&str; 2] = ["/health", "/auth/check"];
```

即：**`/health` 与 `/auth/check` 免认证，其余端点全部需要凭证。**

### 凭证传递

| 方式 | 适用 | 说明 |
|---|---|---|
| `Authorization: Bearer <token>` | 全部受保护端点 | 前端默认方式 |
| `?token=<token>` 查询参数 | `/api/ws` | 浏览器的 `WebSocket` 构造函数无法自定义请求头，后端 `require_auth` 为此保留了 `?token=` 兜底分支 |

`token` 由 `POST /api/login` 返回，前端登录成功后存入本地并在后续请求中携带。

### 未认证响应

`401 Unauthorized`，带 `WWW-Authenticate: Basic realm="pica-server"` 响应头，
响应体为手写的 `json!`（`src-server/src/auth.rs:203`），形状与 `CommandError` 一致：

```json
{
  "errTitle": "未认证",
  "errMessage": "缺少或无效的访问凭证，请在请求头带上 Authorization: Bearer <token>"
}
```

---

## 二、端点表

> 共 **33 个 method+path 组合**，注册在 **26 条 `.route()` 调用**中（同一路径多方法共用一条 `route()`）。
> 前缀 `/api` 由前端拼接，此处按后端注册的原始路径列出。

| method | path | handler | 认证 |
|---|---|---|---|
| GET | `/health` | `health` | 白名单放行 |
| GET | `/auth/check` | `auth_check` | 白名单放行 |
| GET | `/config` | `get_config` | 🔒 |
| POST | `/config` | `save_config` | 🔒 |
| GET | `/server/info` | `server_info` | 🔒 |
| POST | `/server/info` | `post_server_info` | 🔒 |
| POST | `/login` | `login` | 🔒 |
| GET | `/user/profile` | `user_profile` | 🔒 |
| POST | `/user/profile` | `post_user_profile` | 🔒 |
| GET | `/search` | `search_comic` | 🔒 |
| POST | `/search` | `post_search` | 🔒 |
| GET | `/comic/:comic_id` | `get_comic` | 🔒 |
| POST | `/comic` | `post_comic` | 🔒 |
| POST | `/download/task` | `create_download_task` | 🔒 |
| POST | `/download/task/:chapter_id/pause` | `pause_download_task` | 🔒 |
| POST | `/download/task/:chapter_id/resume` | `resume_download_task` | 🔒 |
| POST | `/download/task/:chapter_id/cancel` | `cancel_download_task` | 🔒 |
| POST | `/download/comic` | `download_comic` | 🔒 |
| POST | `/download/by-id` | `download_by_id` | 🔒 |
| GET | `/download/tasks` | `list_download_tasks` | 🔒 |
| GET | `/tasks` | `query_tasks` | 🔒 |
| GET | `/tasks/stats` | `task_stats` | 🔒 |
| POST | `/tasks/purge` | `purge_tasks` | 🔒 |
| GET | `/tasks/:chapter_id` | `get_task` | 🔒 |
| DELETE | `/tasks/:chapter_id` | `delete_task` | 🔒 |
| POST | `/tasks/:chapter_id/retry` | `retry_task` | 🔒 |
| POST | `/sync/comic` | `sync_comic` | 🔒 |
| POST | `/sync/comic-in-search` | `sync_comic_in_search` | 🔒 |
| GET | `/logs/size` | `get_logs_dir_size` | 🔒 |
| POST | `/logs/size` | `post_logs_dir_size` | 🔒 |
| GET | `/logs` | `get_logs` | 🔒 |
| POST | `/logs` | `post_logs` | 🔒 |
| GET | `/ws` | `ws::handler` | 🔒（`?token=` 兜底） |

**注册约束**：同一路径上的多个方法必须在**同一条 `route()` 调用**里注册完，
否则 axum 会在启动时 panic。`/tasks/:chapter_id` 的 GET+DELETE 即按此规则书写（`routes.rs:79-81`）。

**路由匹配优先级**：`/tasks/purge` 是静态段，在 matchit 中优先于 `/tasks/:chapter_id`，
因此 `purge` 不会被当作 `chapter_id`。

---

## 三、请求体 → 返回类型

| 端点 | 请求体 | 返回 |
|---|---|---|
| `GET /config` | — | `Config` |
| `POST /config` | `Config`（直接，无包装） | `()` |
| `GET /server/info` | — | `serde_json::Value` |
| `POST /server/info` | `{}`（空 body） | `serde_json::Value` |
| `POST /login` | `LoginRequest{email,password}` | `String`（token） |
| `GET /user/profile` | — | `UserProfileDetailRespData` |
| `POST /user/profile` | `{}`（空 body） | `UserProfileDetailRespData` |
| `GET /search` | `Query<SearchQuery>` | `SearchResult` |
| `POST /search` | `Json<SearchQuery>` | `SearchResult` |
| `GET /comic/:comic_id` | `Path<String>` | `Comic` |
| `POST /comic` | `Json<ComicIdRequest>` | `Comic` |
| `POST /download/task` | `Json<CreateTaskRequest>` | `()` |
| `POST /download/task/:id/pause` | `Path<String>` | `()` |
| `POST /download/task/:id/resume` | `Path<String>` | `()` |
| `POST /download/task/:id/cancel` | `Path<String>` | `()` |
| `POST /download/comic` | `Json<ComicIdRequest>` | `()` |
| `POST /download/by-id` | `Json<DownloadByIdRequest>` | `DownloadByIdResult` |
| `GET /download/tasks` | — | `Vec<DownloadTaskEvent>` |
| `GET /tasks` | `Query<TasksQuery>` | `TaskListView` |
| `GET /tasks/stats` | — | `TaskStats` |
| `POST /tasks/purge` | `Option<Json<PurgeRequest>>` | `PurgeResult` |
| `GET /tasks/:id` | `Path<String>` | `TaskDetailView` |
| `DELETE /tasks/:id` | `Path<String>` | `serde_json::Value`（`{"ok":true}`） |
| `POST /tasks/:id/retry` | `Path<String>` | `RetryResult` |
| `POST /sync/comic` | `Json<ComicWrapper<Comic>>` | `Comic` |
| `POST /sync/comic-in-search` | `Json<ComicWrapper<ComicInSearch>>` | `ComicInSearch` |
| `GET /logs/size` | — | `u64` |
| `POST /logs/size` | `{}` | `u64` |
| `GET /logs` | `Query<LogsQuery>` | `Vec<String>` |
| `POST /logs` | `Json<LogsQuery>` | `Vec<String>` |

### 请求结构体（逐字）

```rust
// src-server/src/api/routes.rs
struct LoginRequest { email: String, password: String }

struct SearchQuery {
    keyword: String,
    sort: SearchSort,
    page: i32,
    #[serde(default)] categories: Vec<String>,
}

struct CreateTaskRequest { comic: Comic, chapter_id: String }

struct ComicIdRequest { comic_id: String }

struct DownloadByIdRequest {
    comic_id: String,
    #[serde(default)] chapter_id: Option<String>,
}

struct ComicWrapper<T> { comic: T }

struct TasksQuery {
    state: Option<String>,                                    // 空字符串等同不过滤
    #[serde(rename = "comicId", alias = "comic_id")]
    comic_id: Option<String>,
    since: Option<i64>,                                       // 增量拉取游标（Unix 秒）
    limit: Option<i64>,
    offset: Option<i64>,
}

struct PurgeRequest {
    #[serde(default)] retention_days: Option<i64>,
}

struct LogsQuery {
    #[serde(default = "default_tail")] tail: usize,           // 默认 500
}
```

---

## 四、错误响应格式

统一为 `CommandError`：

```ts
// src/bindings.ts:39
export type CommandError = { err_title: string; err_message: string };
```

序列化后为 **camelCase**：

```json
{ "errTitle": "查询任务详情失败", "errMessage": "未找到章节ID为`xxx`的下载任务" }
```

**无 `code` 字段。** 前端靠 `errTitle` 字符串区分错误类别
（例如「不存在」与「查询出错」共用同一个 `errTitle` 前缀，靠 `errMessage` 细分）。

`GET /tasks/:id` 查不到记录时，后端也走 `CommandError` 而非 404 状态码（`commands.rs:539-546` 注释明确说明这一取舍）。

---

## 五、前端未调用的端点

以下 **14 个端点前端 `bindings.ts` 不调用**，设计上是给**青龙脚本 / 基础设施**用的：

| 端点 | 性质 |
|---|---|
| `GET /health` | 基础设施（健康检查 / Docker healthcheck） |
| `GET /auth/check` | 基础设施（前端改用 401 拦截判断凭证有效性，未直接调用） |
| `GET /comic/:comic_id` | 路径参数版本；前端统一走 `POST /comic` |
| `GET /download/tasks` | 内存任务快照；前端改走 WS `task-snapshot-event` |
| `GET /tasks` | 青龙增量拉取 |
| `GET /tasks/stats` | 青龙 |
| `POST /tasks/purge` | 青龙清理 |
| `GET /tasks/:id` | 青龙 |
| `DELETE /tasks/:id` | 青龙 |
| `POST /tasks/:id/retry` | 青龙 |
| `GET /logs/size` | 双方法端点，前端走 POST 版 |
| `GET /logs` | 双方法端点，前端走 POST 版 |
| `GET /server/info` | 双方法端点，前端走 POST 版 |

**反向（前端调、后端无）为 0 个** —— 前端 17 个方法全部有对应路由。

---

## 六、已知问题

### ⚠️ 已知 bug（待修）：`POST /api/config` 包装错位

前端 `bindings.ts` 发送 `{ config: {...} }`（多包一层），
后端 `Json<Config>` 期望裸 `Config` → 反序列化失败（400/422）。

```ts
// src/bindings.ts —— saveConfig
async saveConfig(config: Config): Promise<Result<null, CommandError>> {
    return await callResult<null>(() => post("/api/config", { config }));  // ← 多包了一层
},
```

```rust
// src-server/src/api/routes.rs:26-28
async fn save_config(
    State(state): State<AppState>,
    Json(config): Json<Config>,   // ← 期望裸 Config
) -> Result<Json<()>, ApiError> { ... }
```

**意味着「保存配置」从未成功过。**

修法：前端改为 `post("/api/config", config)`。

> 注：`HANDOFF.md` 第八节给出的运维方法是「GET /api/config -> 改字段 -> POST /api/config」，
> 即运维侧直接用 `curl` 发裸 `Config`，那条路径是通的 —— 受影响的只有 Web UI 的保存按钮。

### state 字段的大小写

两条路径的 state 格式不同，但**互不交叉**（by design）：

| 来源 | 格式 | 消费方 |
|---|---|---|
| `GET /api/tasks` 等 REST 端点 | 小写 `"pending"` | 青龙脚本 |
| WebSocket 事件 | PascalCase `"Pending"` | 前端 UI |

REST 侧由 `DbTaskState::as_str()` 产出小写（`store/types.rs:164-173`）；
WS 侧 `DownloadTaskState` 未加 `#[serde(rename_all)]`，走 serde 默认的 PascalCase（`download_manager.rs:51`）。

前端只消费 WS 的 state：`store.progresses` 的类型定义为
`Extract<DownloadTaskEvent, { event: 'Create' }>['data']`（`src/types.ts:5`），
从类型层面就排除了 REST 数据进入 UI 的可能。全项目对 state 的比较共 33 处，**无一使用小写字面量**。

> ⚠️ **接缝警示**：若将来把 REST tasks 接入前端并与 WS 数据合并，
> 会立刻踩到大小写不一致。合并前必须做归一化。

### 未类型化的 `serde_json::json!` 返回值（代码质量项，暂不处理）

共 **5 处**，返回类型在 Rust 侧是裸 `serde_json::Value`，与前端类型定义靠约定对齐，改名不会编译报错：

| 位置 | 内容 |
|---|---|
| `auth.rs:203` | 401 响应 `{errTitle, errMessage}` —— 形状同 `CommandError` 但手写，不共用类型 |
| `commands.rs:848` | `get_server_info` → `{version, dataDir, downloadDir, eventSubscribers}` |
| `routes.rs:108` | `health` → `{status:"ok"}` |
| `routes.rs:113` | `auth_check` → `{ok:true}` |
| `routes.rs:444` | `delete_task` → `{ok:true}` |

其中 `get_server_info` 最值得注意：前端有 `Promise<ServerInfo>` 类型定义，后端却是裸 `json!`。
