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
use crate::types::{
    Comic, ComicInFavorite, ComicInSearch, GetFavoriteResult, GetFavoriteSort, SearchResult,
    SearchSort,
};

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
        .route("/server/info", get(server_info))
        // ── 登录 ──────────────────────────────────────────
        .route("/login", post(login))
        .route("/user/profile", get(user_profile))
        // ── 搜索 / 详情 / 收藏夹 ─────────────────────────
        .route("/search", get(search_comic))
        .route("/comic/:comic_id", get(get_comic))
        .route("/favorite", get(get_favorite))
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
        .route("/download/favorites", post(download_all_favorites))
        .route("/download/tasks", get(list_download_tasks))
        // ── 库存 ──────────────────────────────────────────
        .route("/library/comics", get(get_downloaded_comics))
        .route("/library/update", post(update_downloaded_comics))
        // ── 字段同步 ──────────────────────────────────────
        .route("/sync/comic", post(sync_comic))
        .route("/sync/comic-in-favorite", post(sync_comic_in_favorite))
        .route("/sync/comic-in-search", post(sync_comic_in_search))
        // ── 日志 ──────────────────────────────────────────
        .route("/logs/size", get(get_logs_dir_size))
        .route("/logs", get(get_logs));

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

async fn get_comic(
    State(state): State<AppState>,
    Path(comic_id): Path<String>,
) -> Result<Json<Comic>, ApiError> {
    let comic = commands::get_comic(&state.app, comic_id).await?;
    Ok(Json(comic))
}

#[derive(Deserialize)]
struct FavoriteQuery {
    sort: GetFavoriteSort,
    page: i64,
}

async fn get_favorite(
    State(state): State<AppState>,
    Query(q): Query<FavoriteQuery>,
) -> Result<Json<GetFavoriteResult>, ApiError> {
    let result = commands::get_favorite(&state.app, q.sort, q.page).await?;
    Ok(Json(result))
}

// ════════════════════════════════════════════════════════════════
// 下载任务
// ════════════════════════════════════════════════════════════════

#[derive(Deserialize)]
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

/// 下载整个收藏夹。
///
/// 这是长耗时操作（可能要跑几十分钟），因此立刻返回 202，
/// 进度通过 WebSocket 的 `download-all-favorites-event` 推送。
async fn download_all_favorites(State(state): State<AppState>) -> Result<Json<()>, ApiError> {
    let app = state.app.clone();
    tokio::spawn(async move {
        if let Err(err) = commands::download_all_favorites(&app).await {
            tracing::error!(err_title = err.err_title, message = err.err_message);
        }
    });
    Ok(Json(()))
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
// 库存
// ════════════════════════════════════════════════════════════════

async fn get_downloaded_comics(State(state): State<AppState>) -> Result<Json<Vec<Comic>>, ApiError> {
    let app = state.app.clone();
    // 全量 WalkDir 是同步阻塞的，放到阻塞线程池，别卡住 tokio worker。
    let comics = tokio::task::spawn_blocking(move || commands::get_downloaded_comics(&app))
        .await
        .map_err(|err| ApiError(CommandError::from("获取已下载漫画失败", err)))?;
    Ok(Json(comics))
}

/// 更新库存。长耗时，立刻返回，进度走 WebSocket。
async fn update_downloaded_comics(
    State(state): State<AppState>,
) -> Result<Json<()>, ApiError> {
    let app = state.app.clone();
    tokio::spawn(async move {
        if let Err(err) = commands::update_downloaded_comics(&app).await {
            tracing::error!(err_title = err.err_title, message = err.err_message);
        }
    });
    Ok(Json(()))
}

// ════════════════════════════════════════════════════════════════
// 字段同步
// ════════════════════════════════════════════════════════════════

async fn sync_comic(
    State(state): State<AppState>,
    Json(comic): Json<Comic>,
) -> Result<Json<Comic>, ApiError> {
    let synced = commands::get_synced_comic(&state.app, comic)?;
    Ok(Json(synced))
}

async fn sync_comic_in_favorite(
    State(state): State<AppState>,
    Json(comic): Json<ComicInFavorite>,
) -> Result<Json<ComicInFavorite>, ApiError> {
    let synced = commands::get_synced_comic_in_favorite(&state.app, comic)?;
    Ok(Json(synced))
}

async fn sync_comic_in_search(
    State(state): State<AppState>,
    Json(comic): Json<ComicInSearch>,
) -> Result<Json<ComicInSearch>, ApiError> {
    let synced = commands::get_synced_comic_in_search(&state.app, comic)?;
    Ok(Json(synced))
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

#[derive(Deserialize)]
struct LogsQuery {
    /// 只读最后 N 行，默认 500。
    #[serde(default = "default_tail")]
    tail: usize,
}

fn default_tail() -> usize {
    500
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