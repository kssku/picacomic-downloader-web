//! REST 路由。把 `api::commands` 里的业务函数接到 HTTP 端点上。
//!
//! 路径设计原则：与原前端 `bindings.ts` 里的命令名一一对应，
//! 这样前端数据层只需要把 `commands.xxx(args)` 换成 `api.post("/api/xxx", args)`，
//! 语义不用重新理解。

use axum::{
    extract::{Path, Query, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;

use crate::api::{commands, error::ApiError, ws};
use crate::auth::{require_auth, AuthConfig};
use crate::config::Config;
use crate::context::AppContext;
use crate::errors::CommandError;
use crate::events::DownloadTaskEvent;
use crate::responses::UserProfileDetailRespData;
use crate::types::{Comic, ComicInSearch, SearchResult, SearchSort};

/// 路由共享状态。
#[derive(Clone)]
pub struct AppState {
    pub app: AppContext,
}

/// 把 `CommandResult<T>` 适配成 handler 的返回类型。
macro_rules! handle {
    ($title:expr, $expr:expr) => {
        $expr.map_err(ApiError::from)
    };
}

/// 组装全部路由。
pub fn router(app: AppContext, auth: AuthConfig) -> Router {
    let state = AppState { app };

    // 不需要认证的端点
    let public = Router::new()
        .route("/health", get(health))
        .route("/auth/check", get(auth_check));

    // 需要认证的端点
    let protected = Router::new()
        // ── 配置 ──────────────────────────────────────────
        .route("/config", get(get_config).post(save_config))
        .route("/server/info", get(server_info).post(post_server_info))
        // ── 登录 ──────────────────────────────────────────
        .route("/login", post(login))
        .route("/user/profile", get(user_profile).post(post_user_profile))
        // ── 搜索 / 详情 / 收藏夹 ─────────────────────────
        .route("/search", get(search_comic).post(post_search))
        .route("/comic/:comic_id", get(get_comic))
        .route("/comic", post(post_comic))
        // ── 下载任务 ──────────────────────────────────────
        .route("/download/task", post(create_download_task))
        .route("/download/task/:chapter_id/pause", post(pause_download_task))
        .route(
            "/download/task/:chapter_id/resume",
            post(resume_download_task),
        )
        .route(
            "/download/task/:chapter_id/cancel",
            post(cancel_download_task),
        )
        .route("/download/comic", post(download_comic))
        .route("/download/by-id", post(download_by_id))
        .route("/download/tasks", get(list_download_tasks))
        // ── 任务查询（Step 4：青龙契约对齐）──────────────
        // 这些端点查的是 SQLite 里的持久记录，与内存调度状态无关。
        // 青龙的 `pika提交` 可以用它们逐步取代 `submit_state.json`。
        .route("/tasks", get(query_tasks))
        .route("/tasks/stats", get(task_stats))
        // `/tasks/purge` 是静态段，matchit 里静态优先于 `/tasks/:chapter_id`，
        // 所以两者不冲突，`purge` 不会被当成 chapter_id。
        .route("/tasks/purge", post(purge_tasks))
        // 同一条路径上挂多个方法，必须一次 `route()` 注册完：
        // axum 对同一路径重复 `route()` 会在启动时 panic。
        .route(
            "/tasks/:chapter_id",
            get(get_task).delete(delete_task),
        )
        .route("/tasks/:chapter_id/retry", post(retry_task))
        // ── 字段同步 ──────────────────────────────────────
        .route("/sync/comic", post(sync_comic))
        .route("/sync/comic-in-search", post(sync_comic_in_search))
        // ── 日志 ──────────────────────────────────────────
        .route("/logs/size", get(get_logs_dir_size).post(post_logs_dir_size))
        .route("/logs", get(get_logs).post(post_logs));

    protected
        .merge(public)
        .route("/ws", get(ws::handler))
        .layer(axum::middleware::from_fn_with_state(
            auth.clone(),
            require_auth,
        ))
        .with_state(state)
}

// ════════════════════════════════════════════════════════════════
// 公开端点
// ════════════════════════════════════════════════════════════════

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

/// 前端用它探测自己的凭证是否还有效。走到这里说明中间件已放行。
async fn auth_check() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

// ════════════════════════════════════════════════════════════════
// 配置
// ════════════════════════════════════════════════════════════════

async fn get_config(State(state): State<AppState>) -> Json<Config> {
    Json(commands::get_config(&state.app))
}

async fn save_config(
    State(state): State<AppState>,
    Json(config): Json<Config>,
) -> Result<Json<()>, ApiError> {
    handle!("保存配置失败", commands::save_config(&state.app, config))?;
    Ok(Json(()))
}

async fn server_info(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(commands::get_server_info(&state.app))
}

/// 前端走的是 `POST /api/server/info` + `{}`。
async fn post_server_info(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(commands::get_server_info(&state.app))
}

// ════════════════════════════════════════════════════════════════
// 登录
// ════════════════════════════════════════════════════════════════

#[derive(Deserialize)]
struct LoginRequest {
    email: String,
    password: String,
}

async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<String>, ApiError> {
    let token = commands::login(&state.app, req.email, req.password)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(token))
}

async fn user_profile(
    State(state): State<AppState>,
) -> Result<Json<UserProfileDetailRespData>, ApiError> {
    let profile = commands::get_user_profile(&state.app)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(profile))
}

/// 前端走的是 `POST /api/user/profile` + `{}`。
async fn post_user_profile(
    State(state): State<AppState>,
) -> Result<Json<UserProfileDetailRespData>, ApiError> {
    user_profile(State(state)).await
}

// ════════════════════════════════════════════════════════════════
// 搜索 / 详情 / 收藏夹
// ════════════════════════════════════════════════════════════════

#[derive(Deserialize)]
struct SearchQuery {
    keyword: String,
    sort: SearchSort,
    page: i32,
    #[serde(default)]
    categories: Vec<String>,
}

async fn search_comic(
    State(state): State<AppState>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<SearchResult>, ApiError> {
    let result =
        commands::search_comic(&state.app, q.keyword, q.sort, q.page, q.categories).await?;
    Ok(Json(result))
}

/// 前端走的是 `POST /api/search` + `{ keyword, sort, page, categories }`。
async fn post_search(
    State(state): State<AppState>,
    Json(q): Json<SearchQuery>,
) -> Result<Json<SearchResult>, ApiError> {
    let result =
        commands::search_comic(&state.app, q.keyword, q.sort, q.page, q.categories).await?;
    Ok(Json(result))
}

async fn get_comic(
    State(state): State<AppState>,
    Path(comic_id): Path<String>,
) -> Result<Json<Comic>, ApiError> {
    let comic = commands::get_comic(&state.app, comic_id).await?;
    Ok(Json(comic))
}

/// 前端走的是 `POST /api/comic` + `{ comicId }`，这里做一层适配。
async fn post_comic(
    State(state): State<AppState>,
    Json(req): Json<ComicIdRequest>,
) -> Result<Json<Comic>, ApiError> {
    let comic = commands::get_comic(&state.app, req.comic_id).await?;
    Ok(Json(comic))
}

// ════════════════════════════════════════════════════════════════
// 下载任务
// ════════════════════════════════════════════════════════════════

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateTaskRequest {
    comic: Comic,
    chapter_id: String,
}

async fn create_download_task(
    State(state): State<AppState>,
    Json(req): Json<CreateTaskRequest>,
) -> Result<Json<()>, ApiError> {
    commands::create_download_task(&state.app, req.comic, req.chapter_id)?;
    Ok(Json(()))
}

async fn pause_download_task(
    State(state): State<AppState>,
    Path(chapter_id): Path<String>,
) -> Result<Json<()>, ApiError> {
    commands::pause_download_task(&state.app, chapter_id)?;
    Ok(Json(()))
}

async fn resume_download_task(
    State(state): State<AppState>,
    Path(chapter_id): Path<String>,
) -> Result<Json<()>, ApiError> {
    commands::resume_download_task(&state.app, chapter_id)?;
    Ok(Json(()))
}

async fn cancel_download_task(
    State(state): State<AppState>,
    Path(chapter_id): Path<String>,
) -> Result<Json<()>, ApiError> {
    commands::cancel_download_task(&state.app, chapter_id)?;
    Ok(Json(()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ComicIdRequest {
    comic_id: String,
}

async fn download_comic(
    State(state): State<AppState>,
    Json(req): Json<ComicIdRequest>,
) -> Result<Json<()>, ApiError> {
    commands::download_comic(&state.app, req.comic_id).await?;
    Ok(Json(()))
}

/// 按 ID 下载。`chapterId` 可选：不传则下载整本未下载章节。
///
/// 面向脚本 / 自动化（如青龙面板）调用，只依赖 comicId，无需构造 Comic 对象。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadByIdRequest {
    comic_id: String,
    #[serde(default)]
    chapter_id: Option<String>,
}

async fn download_by_id(
    State(state): State<AppState>,
    Json(req): Json<DownloadByIdRequest>,
) -> Result<Json<commands::DownloadByIdResult>, ApiError> {
    let result = commands::download_by_id(&state.app, req.comic_id, req.chapter_id).await?;
    Ok(Json(result))
}

/// 当前所有下载任务的快照，供前端首次加载补齐状态。
///
/// WebSocket 推的是增量事件，客户端刷新后会丢掉中间过程；
/// 连上 WS 之后先拉一次这个端点，就能把整张任务表补齐。
async fn list_download_tasks(
    State(state): State<AppState>,
) -> Json<Vec<DownloadTaskEvent>> {
    Json(state.app.download_manager().task_snapshot())
}

// ════════════════════════════════════════════════════════════════
// 字段同步
// ════════════════════════════════════════════════════════════════

/// 前端把 comic 包在 `{ comic }` 里发过来，所以这里需要一层 wrapper。
#[derive(Deserialize)]
struct ComicWrapper<T> {
    comic: T,
}

async fn sync_comic(
    State(state): State<AppState>,
    Json(req): Json<ComicWrapper<Comic>>,
) -> Result<Json<Comic>, ApiError> {
    let synced = commands::get_synced_comic(&state.app, req.comic)?;
    Ok(Json(synced))
}


async fn sync_comic_in_search(
    State(state): State<AppState>,
    Json(req): Json<ComicWrapper<ComicInSearch>>,
) -> Result<Json<ComicInSearch>, ApiError> {
    let synced = commands::get_synced_comic_in_search(&state.app, req.comic)?;
    Ok(Json(synced))
}

// ════════════════════════════════════════════════════════════════
// 任务查询（Step 4：青龙契约对齐）
// ════════════════════════════════════════════════════════════════

/// `GET /api/tasks` 的分页与过滤参数。
///
/// 全部可选：不带任何参数时返回最近 100 条。
#[derive(Deserialize, Default)]
struct TasksQuery {
    /// 任务状态过滤。空字符串等同不过滤。
    state: Option<String>,
    /// 只取某个漫画下的章节。
    #[serde(rename = "comicId", alias = "comic_id")]
    comic_id: Option<String>,
    /// 增量拉取游标：只返回 `updated_at >= since` 的记录（Unix 秒）。
    since: Option<i64>,
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn query_tasks(
    State(state): State<AppState>,
    Query(q): Query<TasksQuery>,
) -> Result<Json<commands::TaskListView>, ApiError> {
    let view = handle!(
        "查询任务列表失败",
        commands::query_tasks(
            &state.app,
            q.state,
            q.comic_id,
            q.since,
            q.limit,
            q.offset,
        )
    )?;
    Ok(Json(view))
}

async fn task_stats(
    State(state): State<AppState>,
) -> Result<Json<crate::store::TaskStats>, ApiError> {
    let view = handle!("查询任务统计失败", commands::get_task_stats(&state.app))?;
    Ok(Json(view))
}

async fn get_task(
    State(state): State<AppState>,
    Path(chapter_id): Path<String>,
) -> Result<Json<commands::TaskDetailView>, ApiError> {
    let view = handle!(
        "查询任务详情失败",
        commands::get_task_detail(&state.app, &chapter_id)
    )?;
    Ok(Json(view))
}

async fn retry_task(
    State(state): State<AppState>,
    Path(chapter_id): Path<String>,
) -> Result<Json<commands::RetryResult>, ApiError> {
    let view = handle!(
        "重试下载任务失败",
        commands::retry_task(&state.app, &chapter_id)
    )?;
    Ok(Json(view))
}

/// 清理过期终态任务（D7）。
///
/// `POST /api/tasks/purge`，body 可选 `{ "retentionDays": 30 }`。
/// 用 POST 而不是 DELETE：这是个带副作用的批处理动作，且要带参数；
/// DELETE 无 body 的惯例会让 `retentionDays` 只能走 query string。
async fn purge_tasks(
    State(state): State<AppState>,
    body: Option<Json<PurgeRequest>>,
) -> Result<Json<commands::PurgeResult>, ApiError> {
    let retention_days = body.and_then(|Json(req)| req.retention_days);
    let app = state.app.clone();

    // 可能删掉成千上万行，走阻塞线程池，别占着 axum 的 async worker。
    let result = tokio::task::spawn_blocking(move || {
        commands::purge_tasks(&app, retention_days)
    })
    .await
    .map_err(|err| ApiError(CommandError::from("清理历史任务失败", err)))??;

    Ok(Json(result))
}

/// `purge_tasks` 的请求体，字段全可选。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PurgeRequest {
    #[serde(default)]
    retention_days: Option<i64>,
}

async fn delete_task(
    State(state): State<AppState>,
    Path(chapter_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    handle!(
        "删除下载任务失败",
        commands::delete_task(&state.app, &chapter_id)
    )?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ════════════════════════════════════════════════════════════════
// 日志
// ════════════════════════════════════════════════════════════════

async fn get_logs_dir_size(State(state): State<AppState>) -> Result<Json<u64>, ApiError> {
    let app = state.app.clone();
    let size = tokio::task::spawn_blocking(move || commands::get_logs_dir_size(&app))
        .await
        .map_err(|err| ApiError(CommandError::from("获取日志目录大小失败", err)))??;
    Ok(Json(size))
}

/// 前端走的是 `POST /api/logs/size` + `{}`。
async fn post_logs_dir_size(State(state): State<AppState>) -> Result<Json<u64>, ApiError> {
    get_logs_dir_size(State(state)).await
}

#[derive(Deserialize)]
struct LogsQuery {
    /// 只读最后 N 行，默认 500。
    #[serde(default = "default_tail")]
    tail: usize,
}

fn default_tail() -> usize {
    500
}

/// 前端走的是 `POST /api/logs` + `{ tail }`。
async fn post_logs(
    State(state): State<AppState>,
    Json(q): Json<LogsQuery>,
) -> Result<Json<Vec<String>>, ApiError> {
    let logs_dir = state.app.paths().logs_dir();
    let lines = tokio::task::spawn_blocking(move || read_log_tail(&logs_dir, q.tail))
        .await
        .map_err(|err| ApiError(CommandError::from("读取日志失败", err)))?
        .map_err(|err| ApiError(CommandError::from("读取日志失败", err)))?;
    Ok(Json(lines))
}

/// 读取日志文件尾部若干行，用于网页日志面板的初始加载。
/// 后续增量日志通过 WebSocket 的 `log-event` 推送。
async fn get_logs(
    State(state): State<AppState>,
    Query(q): Query<LogsQuery>,
) -> Result<Json<Vec<String>>, ApiError> {
    let logs_dir = state.app.paths().logs_dir();
    let app = state.app.clone();
    let lines = tokio::task::spawn_blocking(move || read_log_tail(&logs_dir, q.tail))
        .await
        .map_err(|err| ApiError(CommandError::from("读取日志失败", err)))?
        .map_err(|err| ApiError(CommandError::from("读取日志失败", err)))?;

    let _ = &app;
    Ok(Json(lines))
}

/// 找出日志目录里最新的 `.log` 文件，返回它最后 `tail` 行。
fn read_log_tail(logs_dir: &std::path::Path, tail: usize) -> anyhow::Result<Vec<String>> {
    use anyhow::Context as _;

    if !logs_dir.exists() {
        return Ok(Vec::new());
    }

    // 找最新的 .log 文件
    let mut newest: Option<(std::path::PathBuf, std::time::SystemTime)> = None;
    for entry in std::fs::read_dir(logs_dir)
        .with_context(|| format!("读取日志目录`{}`失败", logs_dir.display()))?
        .filter_map(Result::ok)
    {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("log") {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if newest.as_ref().is_none_or(|(_, t)| modified > *t) {
            newest = Some((path, modified));
        }
    }

    let Some((path, _)) = newest else {
        return Ok(Vec::new());
    };

    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("读取`{}`失败", path.display()))?;
    let all: Vec<&str> = content.lines().collect();
    let start = all.len().saturating_sub(tail);
    Ok(all[start..].iter().map(|s| (*s).to_string()).collect())
}
