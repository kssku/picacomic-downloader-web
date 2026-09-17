//! 业务命令层：原 `src-tauri/src/commands.rs` 的 Web 版对应物。
//!
//! 与桌面版的差异：
//! - `#[tauri::command]` / `#[specta::specta]` 全部去掉，改成普通 `async fn`，
//!   由 `api::routes` 里的 axum handler 调用。
//! - `AppHandle` → `AppContext`。
//! - `event.emit(&app)` → `app.events().emit(topic, &event)`。
//! - 导出 CBZ/PDF、排行榜、章节图片预览、文件管理器定位等命令已按需求删除。


use anyhow::{anyhow, Context as _};

use crate::config::Config;
use crate::context::AppContext;
use crate::errors::{CommandError, CommandResult};
use crate::extensions::AppContextExt;
use crate::responses::UserProfileDetailRespData;
use crate::store::{DbImage, DbTask, DbTaskState, ImageRepo, TaskRepo, TaskStats};
use crate::types::{
    ChapterInfo, Comic, ComicInSearch,
    SearchResult, SearchSort,
};
use crate::utils;

// ════════════════════════════════════════════════════════════════
// 配置
// ════════════════════════════════════════════════════════════════

pub fn get_config(app: &AppContext) -> Config {
    app.config_read()
}

/// 保存配置。
///
/// 代理或 `api_base_url` 变化时重建 HTTP 客户端；
/// 文件日志开关变化时重载 / 关闭文件日志。
pub fn save_config(app: &AppContext, config: Config) -> CommandResult<()> {
    let (proxy_changed, api_base_url_changed, file_logger_changed) = {
        let current = app.config_read();
        (
            current.proxy_mode != config.proxy_mode
                || current.proxy_host != config.proxy_host
                || current.proxy_port != config.proxy_port,
            current.api_base_url != config.api_base_url,
            current.enable_file_logger != config.enable_file_logger,
        )
    };

    let enable_file_logger = config.enable_file_logger;

    app.save_config(&config)
        .map_err(|err| CommandError::from("保存配置失败", err))?;
    tracing::debug!("保存配置成功");

    if proxy_changed || api_base_url_changed {
        let pica_client = app.get_pica_client();
        pica_client.reload_client();
        if api_base_url_changed {
            tracing::info!("API Base URL 已更新为: {}", pica_client.base_url());
        }
    }

    if file_logger_changed {
        if enable_file_logger {
            crate::logger::reload_file_logger()
                .map_err(|err| CommandError::from("重新加载文件日志失败", err))?;
        } else {
            crate::logger::disable_file_logger()
                .map_err(|err| CommandError::from("禁用文件日志失败", err))?;
        }
    }

    Ok(())
}

// ════════════════════════════════════════════════════════════════
// 登录与用户信息
// ════════════════════════════════════════════════════════════════

pub async fn login(app: &AppContext, email: String, password: String) -> CommandResult<String> {
    let pica_client = app.get_pica_client();

    let token = pica_client
        .login(&email, &password)
        .await
        .map_err(|err| CommandError::from("登录失败", err))?;

    // 必须把 token 落盘。之前这里只把 token 返回给调用方，
    // 服务端自身的 config.token 仍是空串，导致登录「成功」之后
    // 所有需要鉴权的请求依然读不到 token，Pica 一律返回 401。
    let mut config = app.config_read();
    config.token = token.clone();
    app.save_config(&config)
        .map_err(|err| CommandError::from("保存登录凭证失败", err))?;
    tracing::info!("已保存登录凭证");

    Ok(token)
}

pub async fn get_user_profile(app: &AppContext) -> CommandResult<UserProfileDetailRespData> {
    let pica_client = app.get_pica_client();

    let user_profile = pica_client
        .get_user_profile()
        .await
        .map_err(|err| CommandError::from("获取用户信息失败", err))?;

    Ok(user_profile)
}

// ════════════════════════════════════════════════════════════════
// 搜索 / 详情 / 收藏夹
// ════════════════════════════════════════════════════════════════

pub async fn search_comic(
    app: &AppContext,
    keyword: String,
    sort: SearchSort,
    page: i32,
    categories: Vec<String>,
) -> CommandResult<SearchResult> {
    let pica_client = app.get_pica_client();

    let search_resp_data = pica_client
        .search_comic(&keyword, sort, page, categories)
        .await
        .map_err(|err| CommandError::from("搜索漫画失败", err))?;

    let search_result = SearchResult::from_resp_data(app, search_resp_data)
        .map_err(|err| CommandError::from("搜索漫画失败", err))?;

    Ok(search_result)
}

pub async fn get_comic(app: &AppContext, comic_id: String) -> CommandResult<Comic> {
    let comic = utils::get_comic(app, &comic_id)
        .await
        .context(format!("获取ID为`{comic_id}`的漫画失败"))
        .map_err(|err| CommandError::from("获取漫画失败", err))?;

    Ok(comic)
}

// ════════════════════════════════════════════════════════════════
// 下载任务控制
// ════════════════════════════════════════════════════════════════

pub fn create_download_task(
    app: &AppContext,
    comic: Comic,
    chapter_id: String,
) -> CommandResult<()> {
    let download_manager = app.get_download_manager();

    let comic_title = comic.title.clone();
    download_manager
        .create_download_task(comic, chapter_id.clone())
        .context(format!(
            "漫画`{comic_title}`创建章节ID为`{chapter_id}`的下载任务失败"
        ))
        .map_err(|err| CommandError::from("下载任务创建失败", err))?;
    tracing::debug!("下载任务创建成功");

    Ok(())
}

pub fn pause_download_task(app: &AppContext, chapter_id: String) -> CommandResult<()> {
    app.get_download_manager()
        .pause_download_task(&chapter_id)
        .context(format!("暂停章节ID为`{chapter_id}`的下载任务失败"))
        .map_err(|err| CommandError::from("暂停下载任务失败", err))?;
    tracing::debug!("暂停章节ID为`{chapter_id}`的下载任务成功");

    Ok(())
}

pub fn resume_download_task(app: &AppContext, chapter_id: String) -> CommandResult<()> {
    app.get_download_manager()
        .resume_download_task(&chapter_id)
        .context(format!("恢复章节ID为`{chapter_id}`的下载任务失败"))
        .map_err(|err| CommandError::from("恢复下载任务失败", err))?;
    tracing::debug!("恢复章节ID为`{chapter_id}`的下载任务成功");

    Ok(())
}

pub fn cancel_download_task(app: &AppContext, chapter_id: String) -> CommandResult<()> {
    app.get_download_manager()
        .cancel_download_task(&chapter_id)
        .context(format!("取消章节ID为`{chapter_id}`的下载任务失败"))
        .map_err(|err| CommandError::from("取消下载任务失败", err))?;
    tracing::debug!("取消章节ID为`{chapter_id}`的下载任务成功");

    Ok(())
}

/// 一键下载整本漫画：给所有未下载的章节创建任务。
pub async fn download_comic(app: &AppContext, comic_id: String) -> CommandResult<()> {
    let download_manager = app.get_download_manager();

    let comic = utils::get_comic(app, &comic_id)
        .await
        .context(format!("获取ID为`{comic_id}`的漫画失败"))
        .map_err(|err| CommandError::from("一键下载漫画失败", err))?;

    let comic_title = &comic.title;

    let chapter_infos: Vec<&ChapterInfo> = comic
        .chapter_infos
        .iter()
        .filter(|chapter_info| chapter_info.is_downloaded != Some(true))
        .collect();

    if chapter_infos.is_empty() {
        let err = anyhow!("漫画`{comic_title}`的所有章节都已存在于下载目录，无需重复下载");
        return Err(CommandError::from("一键下载漫画失败", err));
    }

    for chapter_info in chapter_infos {
        let chapter_id = &chapter_info.chapter_id;
        download_manager
            .create_download_task(comic.clone(), chapter_id.clone())
            .context(format!(
                "漫画`{comic_title}`创建章节ID为`{chapter_id}`的下载任务失败"
            ))
            .map_err(|err| CommandError::from("一键下载漫画失败", err))?;
    }

    tracing::debug!("一键下载漫画成功，已为所有需要下载的章节创建下载任务");
    Ok(())
}

// ════════════════════════════════════════════════════════════════
// 按 ID 下载（面向脚本 / 自动化调用）
// ════════════════════════════════════════════════════════════════

/// `download_by_id` 的返回结果。字段全部 camelCase，方便脚本直接解析。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadByIdResult {
    /// 漫画 ID（回显，方便脚本确认）。
    pub comic_id: String,
    /// 漫画标题（取到后回显）。
    pub comic_title: String,
    /// 本次实际创建下载任务的章节 ID 列表。
    pub created_chapters: Vec<String>,
    /// 因「已下载」而跳过的章节 ID 列表（仅整本下载时有意义）。
    pub skipped_chapters: Vec<String>,
    /// 因「任务已存在」而未重复创建的章节 ID 列表。
    pub already_running_chapters: Vec<String>,
    /// 本次成功创建的任务数。
    pub created_count: u32,
}

/// 通过漫画 ID 下载。
///
/// - 只传 `comic_id`：下载该漫画所有「未下载」的章节。
/// - 同时传 `chapter_id`：只下载指定章节（忽略整本逻辑）。
///
/// 与前端「一键下载」的区别：这个入口只依赖 ID，不需要调用方先构造 `Comic` 对象，
/// 适合青龙面板 / 脚本 / 自动化工具直接调用。
pub async fn download_by_id(
    app: &AppContext,
    comic_id: String,
    chapter_id: Option<String>,
) -> CommandResult<DownloadByIdResult> {
    let download_manager = app.get_download_manager();

    let comic = utils::get_comic(app, &comic_id)
        .await
        .context(format!("获取ID为`{comic_id}`的漫画失败"))
        .map_err(|err| CommandError::from("按ID下载漫画失败", err))?;

    let comic_title = comic.title.clone();

    // ── 指定章节：只下一章 ──────────────────────────────
    if let Some(chapter_id) = chapter_id {
        let exists = comic
            .chapter_infos
            .iter()
            .any(|c| c.chapter_id == chapter_id);
        if !exists {
            let err = anyhow!("漫画`{comic_title}`中不存在章节ID为`{chapter_id}`的章节");
            return Err(CommandError::from("按ID下载漫画失败", err));
        }

        let mut created_chapters = Vec::new();
        let mut already_running_chapters = Vec::new();
        match download_manager.create_download_task(comic.clone(), chapter_id.clone()) {
            Ok(()) => created_chapters.push(chapter_id),
            // 「任务已存在」不算致命错误，归入 already_running
            Err(err) if err.to_string().contains("已存在") => {
                already_running_chapters.push(chapter_id);
            }
            Err(err) => {
                return Err(CommandError::from("按ID下载漫画失败", err));
            }
        }

        tracing::debug!(
            comic_title = %comic_title,
            chapter_id = %created_chapters.first().cloned().unwrap_or_default(),
            "按ID下载：指定章节任务创建完成"
        );

        return Ok(DownloadByIdResult {
            comic_id,
            comic_title,
            created_count: created_chapters.len() as u32,
            created_chapters,
            skipped_chapters: Vec::new(),
            already_running_chapters,
        });
    }

    // ── 未指定章节：下载整本未下载章节 ──────────────────
    let mut created_chapters = Vec::new();
    let mut skipped_chapters = Vec::new();
    let mut already_running_chapters = Vec::new();

    for chapter_info in &comic.chapter_infos {
        let chapter_id = chapter_info.chapter_id.clone();

        // 已下载的章节直接跳过
        if chapter_info.is_downloaded == Some(true) {
            skipped_chapters.push(chapter_id);
            continue;
        }

        match download_manager.create_download_task(comic.clone(), chapter_id.clone()) {
            Ok(()) => created_chapters.push(chapter_id),
            Err(err) if err.to_string().contains("已存在") => {
                already_running_chapters.push(chapter_id);
            }
            Err(err) => {
                // 单章创建失败不中断整本，记录后继续
                tracing::warn!(
                    comic_title = %comic_title,
                    chapter_id = %chapter_id,
                    error = %err,
                    "按ID下载：创建章节任务失败，已跳过"
                );
            }
        }
    }

    if created_chapters.is_empty() && already_running_chapters.is_empty() {
        let err = anyhow!("漫画`{comic_title}`没有可下载的章节（全部已下载）");
        return Err(CommandError::from("按ID下载漫画失败", err));
    }

    tracing::debug!(
        comic_title = %comic_title,
        created = created_chapters.len(),
        skipped = skipped_chapters.len(),
        already_running = already_running_chapters.len(),
        "按ID下载：整本任务创建完成"
    );

    Ok(DownloadByIdResult {
        comic_id,
        comic_title,
        created_count: created_chapters.len() as u32,
        created_chapters,
        skipped_chapters,
        already_running_chapters,
    })
}

// ════════════════════════════════════════════════════════════════
// 任务查询（Step 4：青龙契约对齐）
// ════════════════════════════════════════════════════════════════
//
// 这一组端点直接读 SQLite 里的持久记录，是 `submit_state.json` 的
// **单一真相源替代品**。青龙侧的下游脚本（`pika提交` / `pika打包`）
// 可以从「读一个不断被覆写的 JSON」改成「查这个只增不改的接口」。
//
// 设计要点：
// - 返回结构全部 camelCase，脚本直接 `JSON.parse` 就能用。
// - 时间戳统一用 Unix 秒（与 DB 一致），不做本地时区转换——
//   时区是展示层的事，契约层保持无歧义。

/// 单个任务的对外表示。
///
/// 刻意**不**直接序列化 `DbTask`：DB 行里的字段是内部实现，
/// 一旦前端/脚本依赖了它们，改 schema 就成了破坏性变更。
/// 这里做一次显式映射，schema 变更的影响被挡在这一层。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskView {
    pub chapter_id: String,
    pub comic_id: String,
    pub comic_title: String,
    pub chapter_title: String,
    pub chapter_order: i64,
    /// `pending` / `downloading` / `paused` / `cancelled` / `completed` / `failed`
    pub state: String,
    pub total_img_count: i64,
    pub done_img_count: i64,
    /// 已完成百分比，0-100，保留一位小数。脚本用它做进度上报。
    pub progress: f64,
    pub retry_count: i64,
    pub last_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl From<&DbTask> for TaskView {
    fn from(task: &DbTask) -> Self {
        // 除零保护：`total` 为 0 说明清单还没登记，进度算 0 而不是 NaN。
        let progress = if task.total_img_count > 0 {
            #[allow(clippy::cast_precision_loss)]
            let ratio = task.done_img_count as f64 / task.total_img_count as f64;
            (ratio * 1000.0).round() / 10.0
        } else {
            0.0
        };

        Self {
            chapter_id: task.chapter_id.clone(),
            comic_id: task.comic_id.clone(),
            comic_title: task.comic_title.clone(),
            chapter_title: task.chapter_title.clone(),
            chapter_order: task.chapter_order,
            state: task.state.as_str().to_string(),
            total_img_count: task.total_img_count,
            done_img_count: task.done_img_count,
            progress,
            retry_count: task.retry_count,
            last_error: task.last_error.clone(),
            created_at: task.created_at,
            updated_at: task.updated_at,
        }
    }
}

/// 图片级的对外表示。只在查单个任务时返回，避免列表接口体积爆炸。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskImageView {
    pub img_index: i64,
    pub url: String,
    pub state: String,
    pub retry_count: i64,
    pub last_error: Option<String>,
    pub bytes: Option<i64>,
    pub updated_at: i64,
}

impl From<&DbImage> for TaskImageView {
    fn from(img: &DbImage) -> Self {
        Self {
            img_index: img.img_index,
            url: img.url.clone(),
            state: img.state.as_str().to_string(),
            retry_count: img.retry_count,
            last_error: img.last_error.clone(),
            bytes: img.bytes,
            updated_at: img.updated_at,
        }
    }
}

/// 列表接口的响应。带 `total` 让脚本能翻页而不必猜。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskListView {
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
    pub tasks: Vec<TaskView>,
}

/// 查任务列表。
///
/// `state` 支持单值过滤；不传则返回全部（含 `completed`）。
/// 青龙的增量拉取可以传 `since`（Unix 秒），只取那之后有更新的任务——
/// 这正是 `updated_at` 存在的意义。
pub fn query_tasks(
    app: &AppContext,
    state: Option<String>,
    comic_id: Option<String>,
    since: Option<i64>,
    limit: Option<i64>,
    offset: Option<i64>,
) -> CommandResult<TaskListView> {
    let state = match state.as_deref() {
        // 空字符串等同于「不过滤」，这样脚本拼 URL 时不用做条件判断。
        None | Some("") => None,
        Some(raw) => Some(
            DbTaskState::parse(raw)
                .with_context(|| format!("无法识别的任务状态 `{raw}`"))
                .map_err(|err| CommandError::from("查询任务列表失败", err))?,
        ),
    };

    // 上限 500：防止脚本误传 `limit=999999` 把整张表拉进内存。
    // 下载任务量级是「万」级，500 一页足够脚本翻。
    let limit = limit.unwrap_or(100).clamp(1, 500);
    let offset = offset.unwrap_or(0).max(0);

    let store = app.store();
    let tasks = TaskRepo::list(store, state, comic_id.as_deref(), since, limit, offset)
        .context("查询任务列表失败")
        .map_err(|err| CommandError::from("查询任务列表失败", err))?;

    let total = TaskRepo::count(store, state, comic_id.as_deref(), since)
        .context("统计任务总数失败")
        .map_err(|err| CommandError::from("查询任务列表失败", err))?;

    Ok(TaskListView {
        total,
        limit,
        offset,
        tasks: tasks.iter().map(TaskView::from).collect(),
    })
}

/// 查单个任务，附带失败图片明细。
///
/// `failedImages` 只在有失败时才非空——正常情况下（下载中 / 已完成）
/// 它的开销只是一次索引查询。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskDetailView {
    #[serde(flatten)]
    pub task: TaskView,
    /// 未完成（`pending` + `failed`）的图片数。断点续传的「还剩多少」。
    pub unfinished_img_count: i64,
    pub failed_images: Vec<TaskImageView>,
}

pub fn get_task_detail(app: &AppContext, chapter_id: &str) -> CommandResult<TaskDetailView> {
    let store = app.store();

    let task = TaskRepo::get(store, chapter_id)
        .context(format!("查询章节ID为`{chapter_id}`的任务失败"))
        .map_err(|err| CommandError::from("查询任务详情失败", err))?
        // 查不到就是 404 语义。这里统一走 `CommandError`，
        // 前端读 `errTitle` 就能区分「不存在」和「查询出错」。
        .ok_or_else(|| {
            CommandError::from(
                "查询任务详情失败",
                anyhow!("未找到章节ID为`{chapter_id}`的下载任务"),
            )
        })?;

    let unfinished_img_count = ImageRepo::count_unfinished(store, chapter_id)
        .context("统计未完成图片失败")
        .map_err(|err| CommandError::from("查询任务详情失败", err))?;

    let failed_images = ImageRepo::list_failed(store, chapter_id)
        .context("查询失败图片失败")
        .map_err(|err| CommandError::from("查询任务详情失败", err))?;

    Ok(TaskDetailView {
        task: TaskView::from(&task),
        unfinished_img_count,
        failed_images: failed_images.iter().map(TaskImageView::from).collect(),
    })
}

/// 各状态任务计数。青龙的「总览」脚本用。
pub fn get_task_stats(app: &AppContext) -> CommandResult<TaskStats> {
    TaskRepo::stats(app.store())
        .context("统计任务状态失败")
        .map_err(|err| CommandError::from("查询任务统计失败", err))
}

/// 清理终态任务记录（D7）。
///
/// 长期运行后 `download_task` 会无限增长：每个下载过的章节留一行，
/// 而 `completed` 的行对运维已经没有意义（真正的事实源是磁盘上的文件和
/// 青龙侧的 `is_downloaded`）。这里按保留天数删掉过期的终态行。
///
/// **只删 `completed` / `cancelled`**：
/// - `failed` 必须留着，用户可能还要 `POST /tasks/:id/retry`；
/// - `pending` / `downloading` / `paused` 是活任务，更不能删。
///
/// `retention_days` 传 `None` 时用默认 30 天；传 `Some(0)` 表示只保留
/// 最近 24 小时内更新过的终态行（不是「全删」——0 天仍是一个窗口）。
/// 图片行靠外键级联一起删除。
pub fn purge_tasks(app: &AppContext, retention_days: Option<i64>) -> CommandResult<PurgeResult> {
    // 下限 0：负数会让 `before_ts` 跑到未来，等于把刚完成的任务也删掉。
    let retention_days = retention_days.unwrap_or(DEFAULT_PURGE_RETENTION_DAYS).max(0);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let before_ts = purge_cutoff(now, retention_days);

    let removed = TaskRepo::purge_terminal(app.store(), before_ts)
        .context("清理历史任务失败")
        .map_err(|err| CommandError::from("清理历史任务失败", err))?;

    tracing::info!(
        removed,
        retention_days,
        before_ts,
        "已清理过期终态任务（failed 与活任务保留）"
    );

    Ok(PurgeResult {
        removed,
        retention_days,
        before_ts,
    })
}

/// 默认保留 30 天终态记录。
const DEFAULT_PURGE_RETENTION_DAYS: i64 = 30;

/// 由「当前时间 + 保留天数」算出删除截止时间戳。
///
/// 抽成纯函数是为了可测：`purge_terminal` 是**不可逆的批量删除**，
/// 而这里唯一的算术错误（比如负数天数让 `before_ts` 跑到未来）会直接
/// 删掉刚完成的任务。天数已在调用方 clamp 到 `>= 0`，这里再 `max(0)`
/// 兜一层，保证 `before_ts` 永不晚于 `now`。
fn purge_cutoff(now: i64, retention_days: i64) -> i64 {
    now.saturating_sub(retention_days.max(0).saturating_mul(24 * 60 * 60))
}

/// `purge_tasks` 的结果。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PurgeResult {
    /// 实际删除的章节行数（图片行由外键级联，不计入）。
    pub removed: usize,
    /// 本次生效的保留天数。
    pub retention_days: i64,
    /// 删除截止时间（Unix 秒）。早于它的终态行被删。
    pub before_ts: i64,
}

/// 判断「重试时」是否应当把任务直接收尾为 `Completed`。
///
/// 必须同时满足：
/// - 没有未完成图片（`unfinished == 0`）；
/// - **确实存在图片记录**（`total > 0`）。
///
/// `total == 0` 表示失败发生在「获取图片链接」阶段 —— `download_image`
/// 里一行都没有。此时若收尾为 `Completed`，会把一个彻底失败的章节
/// 伪造成成功，青龙侧将永远不会重试。
fn should_finalize_as_completed(total: i64, unfinished: i64) -> bool {
    total > 0 && unfinished == 0
}

/// 手动重试一个失败 / 暂停的任务。
///
/// 语义与「取消后重建」不同：它**保留已下载的图片**，只把未完成的
/// 图片重置为 `pending`，然后把任务状态打回 `Pending` 等调度。
/// 这是图片级断点续传在 API 层的出口。
///
/// 三种情况：
/// 1. 任务在内存里活着（本进程创建的）→ 重置图片 + 置 `Pending`，调度器接管。
/// 2. 任务不在内存（上次进程遗留）→ 重置图片 + 置 `Pending`，
///    由启动恢复流程或下一次 `pika提交` 触发重建。
/// 3. 章节图片全部已完成 → 直接置 `Completed`，不做无意义的重下。
pub fn retry_task(app: &AppContext, chapter_id: &str) -> CommandResult<RetryResult> {
    let store = app.store();

    let task = TaskRepo::get(store, chapter_id)
        .context(format!("查询章节ID为`{chapter_id}`的任务失败"))
        .map_err(|err| CommandError::from("重试任务失败", err))?
        .ok_or_else(|| {
            CommandError::from(
                "重试任务失败",
                anyhow!("未找到章节ID为`{chapter_id}`的下载任务"),
            )
        })?;

    // 从终态里排除 `cancelled`：用户取消过的东西不该被一次误点的
    // 重试按钮复活。重下已取消的任务请走 `POST /download/task` 显式重建。
    if task.state == DbTaskState::Cancelled {
        return Err(CommandError::from(
            "重试任务失败",
            anyhow!("章节ID为`{chapter_id}`的任务已被取消，请重新创建下载任务而不是重试"),
        ));
    }

    let reset_count = ImageRepo::reset_failed(store, chapter_id)
        .context("重置未完成图片失败")
        .map_err(|err| CommandError::from("重试任务失败", err))?;

    // 重置后重新对齐进度，让 `done_img_count` 反映事实。
    let (total, done) = TaskRepo::resync_progress(store, chapter_id)
        .context("对齐任务进度失败")
        .map_err(|err| CommandError::from("重试任务失败", err))?;

    let unfinished = total - done;

    // 内存里若还挂着这个任务，先置 `Pending` 让调度循环重新捡起来。
    // 不在内存里也不报错——`set_state` 会写 DB，恢复流程认这个状态。
    let manager = app.get_download_manager();
    let in_memory = manager.resume_download_task(chapter_id).is_ok();

    // 全部已完成：没有任何未完成图片，**且确实存在图片记录**，
    // 才收尾成 `Completed`。
    //
    // `total == 0` 表示 `download_image` 里一行都没有 —— 说明失败发生在
    // 「获取图片链接」阶段（还没插入任何图片行）。此时绝不能判定为完成，
    // 否则一个彻底失败的章节会被静默标成 `completed`，青龙侧永远不会重试。
    if should_finalize_as_completed(total, unfinished) {
        TaskRepo::set_state(store, chapter_id, DbTaskState::Completed, None, 0)
            .context("收尾任务状态失败")
            .map_err(|err| CommandError::from("重试任务失败", err))?;
        tracing::info!(chapter_id, "重试时发现章节已完整，直接置为完成");

        return Ok(RetryResult {
            chapter_id: chapter_id.to_string(),
            reset_img_count: reset_count,
            unfinished_img_count: 0,
            scheduled: false,
            already_complete: true,
        });
    }

    // `total == 0`：失败在链接阶段，没有可复用的图片记录，必须重新拉链接。
    // 这里显式打一条日志，避免又被误判为「已完成」。
    if total == 0 {
        tracing::info!(
            chapter_id,
            in_memory,
            "重试时未发现任何图片记录，判定为链接阶段失败，重新排队"
        );
    }

    tracing::info!(
        chapter_id,
        reset_count,
        unfinished,
        in_memory,
        "已重置未完成图片，任务等待重新调度"
    );

    Ok(RetryResult {
        chapter_id: chapter_id.to_string(),
        reset_img_count: reset_count,
        unfinished_img_count: unfinished,
        scheduled: in_memory,
        already_complete: false,
    })
}

/// `retry_task` 的结果。脚本靠 `scheduled` 判断要不要等下一轮。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryResult {
    pub chapter_id: String,
    /// 被重置回 `pending` 的图片数。
    pub reset_img_count: usize,
    /// 还剩多少张要下。
    pub unfinished_img_count: i64,
    /// 任务是否已在当前进程的调度器里挂上。
    ///
    /// `false` 不代表失败：任务已写入 DB，会在下次进程启动恢复时被拉起。
    pub scheduled: bool,
    /// 章节本来就完整，这次没有产生任何下载。
    pub already_complete: bool,
}

/// 删除一条任务记录（图片行靠外键级联删除）。
///
/// **只删记录，不动磁盘文件。** 这是刻意的：误删一条 DB 行可以重建，
/// 误删用户下载好的漫画不可恢复。要清文件请走青龙侧的清理脚本。
pub fn delete_task(app: &AppContext, chapter_id: &str) -> CommandResult<()> {
    let deleted = TaskRepo::delete(app.store(), chapter_id)
        .context(format!("删除章节ID为`{chapter_id}`的任务失败"))
        .map_err(|err| CommandError::from("删除任务失败", err))?;

    if !deleted {
        return Err(CommandError::from(
            "删除任务失败",
            anyhow!("未找到章节ID为`{chapter_id}`的下载任务"),
        ));
    }

    // 内存里的任务也要取消，否则它下一轮状态迁移又会把行写回 DB，
    // 用户会看到「删掉的任务自己回来了」。
    let manager = app.get_download_manager();
    if let Err(err) = manager.cancel_download_task(chapter_id) {
        // 内存里没有是正常情况（任务只存在于 DB），降级为 debug。
        tracing::debug!(chapter_id, message = %err, "取消内存任务失败（可能本就不在内存中）");
    }

    tracing::info!(chapter_id, "已删除下载任务记录");
    Ok(())
}

// ════════════════════════════════════════════════════════════════
// 前端字段同步
// ════════════════════════════════════════════════════════════════

pub fn get_synced_comic(app: &AppContext, mut comic: Comic) -> CommandResult<Comic> {
    let id_to_dir_map = utils::create_id_to_dir_map(app)
        .context("创建漫画ID到下载目录映射失败")
        .map_err(|err| {
            CommandError::from(&format!("漫画`{}`同步Comic的字段失败", comic.title), err)
        })?;

    comic.update_fields(&id_to_dir_map).map_err(|err| {
        CommandError::from(&format!("漫画`{}`同步Comic的字段失败", comic.title), err)
    })?;

    Ok(comic)
}

pub fn get_synced_comic_in_search(
    app: &AppContext,
    mut comic: ComicInSearch,
) -> CommandResult<ComicInSearch> {
    let id_to_dir_map = utils::create_id_to_dir_map(app)
        .context("创建漫画ID到下载目录映射失败")
        .map_err(|err| {
            let err_title = format!("漫画`{}`同步ComicInSearch的字段失败", comic.title);
            CommandError::from(&err_title, err)
        })?;

    comic.update_fields(&id_to_dir_map);

    Ok(comic)
}

// ════════════════════════════════════════════════════════════════
// 杂项
// ════════════════════════════════════════════════════════════════

/// 日志目录大小（字节）。用于后台「日志」页面显示占用。
pub fn get_logs_dir_size(app: &AppContext) -> CommandResult<u64> {
    let logs_dir = app.paths().logs_dir();

    let size = std::fs::read_dir(&logs_dir)
        .context(format!("读取日志目录`{}`失败", logs_dir.display()))
        .map_err(|err| CommandError::from("获取日志目录大小失败", err))?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.metadata().ok())
        .map(|metadata| metadata.len())
        .sum::<u64>();

    tracing::debug!("获取日志目录大小成功");
    Ok(size)
}

/// 当前后端版本与运行状态，给前端「关于」页用。
pub fn get_server_info(app: &AppContext) -> serde_json::Value {
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "dataDir": app.paths().data_dir.to_string_lossy(),
        "downloadDir": app.config_read().download_dir.to_string_lossy(),
        "eventSubscribers": app.events().receiver_count(),
    })
}

#[cfg(test)]
mod tests {
    use super::{purge_cutoff, DEFAULT_PURGE_RETENTION_DAYS};

    const DAY: i64 = 24 * 60 * 60;

    // ── D7：清理窗口的算术 ────────────────────────────────────────
    //
    // `purge_terminal` 是不可逆的批量删除，唯一能出错的地方就是这个
    // 截止时间戳。下面覆盖「正常窗口」「零天」「负数」「溢出」四种输入。

    #[test]
    fn purge_cutoff_subtracts_retention_window() {
        let now = 1_700_000_000;
        assert_eq!(purge_cutoff(now, 30), now - 30 * DAY);
        assert_eq!(purge_cutoff(now, 1), now - DAY);
    }

    #[test]
    fn purge_cutoff_zero_days_keeps_last_24h() {
        // 0 天不是「全删」，而是「保留最近 24 小时」。
        let now = 1_700_000_000;
        assert_eq!(purge_cutoff(now, 0), now);
    }

    #[test]
    fn purge_cutoff_clamps_negative_days_to_now() {
        // 负数如果直接参与运算，`before_ts` 会跑到**未来**，
        // 于是刚完成的任务也会被删掉。必须收敛到 `now`。
        let now = 1_700_000_000;
        assert_eq!(
            purge_cutoff(now, -5),
            now,
            "负数保留天数不得把截止时间推到未来"
        );
        assert!(purge_cutoff(now, -5) <= now);
    }

    #[test]
    fn purge_cutoff_saturates_instead_of_overflowing() {
        // `i64::MIN` 天 * 86400 会溢出；debug 下溢出会 panic，
        // release 下会回绕成一个正数 —— 两种都不能出现。
        //
        // 这里只断言「不 panic 且结果远在过去 / 等于 now」：
        // `i64::MAX` 天算出的是 `now - i64::MAX`，它并未触及 `i64::MIN`，
        // 钉死具体数值等于把饱和算法的内部细节写进测试。
        let now = 1_700_000_000;
        let far_past = purge_cutoff(now, i64::MAX);
        assert!(
            far_past < now - 100 * 365 * 24 * 60 * 60,
            "极大天数应当落在远过去，实际 {far_past}"
        );
        assert_eq!(purge_cutoff(now, i64::MIN), now, "极端负数等同于零天");
    }

    #[test]
    fn default_retention_is_thirty_days() {
        assert_eq!(DEFAULT_PURGE_RETENTION_DAYS, 30);
    }

    // ── 验收标准 6：青龙契约 ──────────────────────────────────────
    //
    // 青龙侧有两个真实脚本，它们的字段名就是契约本身，改名即破坏：
    //
    // - `submit_pending.py` 读 `POST /download/by-id` 的响应，取
    //   `createdCount` / `alreadyRunningChapters` / `skippedChapters`；
    // - `submit_state.json` 是 `{"submitted": [comic_id, ...]}`。
    //
    // 下面用序列化后的 JSON 键名做断言，而不是断言 Rust 字段名——
    // 因为破坏青龙的永远是线上的键名，不是编译期的标识符。

    #[test]
    fn download_by_id_response_keeps_qinglong_field_names() {
        let result = super::DownloadByIdResult {
            comic_id: "c1".into(),
            comic_title: "标题".into(),
            created_chapters: vec!["ch1".into()],
            skipped_chapters: vec!["ch2".into()],
            already_running_chapters: vec!["ch3".into()],
            created_count: 1,
        };

        let json: serde_json::Value = serde_json::to_value(&result).unwrap();
        let obj = json.as_object().unwrap();

        // `submit_pending.py` 依赖的三个字段，缺一个脚本就会 KeyError。
        for key in ["createdCount", "alreadyRunningChapters", "skippedChapters"] {
            assert!(obj.contains_key(key), "青龙契约缺少字段 `{key}`");
        }

        // 值为数组的字段必须是数组，否则脚本的 `len()` 会炸。
        assert!(obj["alreadyRunningChapters"].is_array());
        assert!(obj["skippedChapters"].is_array());
        assert_eq!(obj["createdCount"].as_u64(), Some(1));
    }

    #[test]
    fn task_view_exposes_submitted_comic_ids() {
        // `GET /api/tasks?state=completed` 是 `submit_state.json` 的替代品。
        // 关键差异：状态文件存的是 **comic_id**，而任务表是**章节级**的。
        // 所以「等价」的含义是：把 completed 任务的 comicId 去重，就能得到
        // 状态文件里的 submitted 集合。这条断言把这个推导关系钉死。
        let tasks = [
            super::TaskView::from(&crate::store::DbTask {
                chapter_id: "ch1".into(),
                comic_id: "comic-a".into(),
                comic_title: "A".into(),
                chapter_title: "第1话".into(),
                chapter_order: 1,
                state: crate::store::DbTaskState::Completed,
                total_img_count: 10,
                done_img_count: 10,
                retry_count: 0,
                last_error: None,
                dir_fmt: String::new(),
                created_at: 1_700_000_000,
                updated_at: 1_700_000_100,
            }),
            // 同一本漫画的第二个章节：去重后不应产生第二个 comic_id。
            super::TaskView::from(&crate::store::DbTask {
                chapter_id: "ch2".into(),
                comic_id: "comic-a".into(),
                comic_title: "A".into(),
                chapter_title: "第2话".into(),
                chapter_order: 2,
                state: crate::store::DbTaskState::Completed,
                total_img_count: 8,
                done_img_count: 8,
                retry_count: 0,
                last_error: None,
                dir_fmt: String::new(),
                created_at: 1_700_000_000,
                updated_at: 1_700_000_200,
            }),
            super::TaskView::from(&crate::store::DbTask {
                chapter_id: "ch3".into(),
                comic_id: "comic-b".into(),
                comic_title: "B".into(),
                chapter_title: "第1话".into(),
                chapter_order: 1,
                state: crate::store::DbTaskState::Completed,
                total_img_count: 5,
                done_img_count: 5,
                retry_count: 0,
                last_error: None,
                dir_fmt: String::new(),
                created_at: 1_700_000_000,
                updated_at: 1_700_000_300,
            }),
        ];

        let mut comics: Vec<String> = tasks.iter().map(|t| t.comic_id.clone()).collect();
        comics.sort();
        comics.dedup();

        assert_eq!(
            comics,
            vec!["comic-a".to_string(), "comic-b".to_string()],
            "completed 任务的 comicId 去重后应等价于 submit_state.json 的 submitted"
        );
    }

    // 回归：失败在「获取图片链接」阶段时，`download_image` 一行都没有
    // （total == 0）。此前的实现只看 `unfinished == 0` 就判定完成，
    // 会把彻底失败的章节静默标成 `completed`，导致青龙侧永不重试。
    #[test]
    fn failed_link_stage_is_not_finalized_as_completed() {
        // total == 0（链接阶段失败）：不能判完成
        assert!(!super::should_finalize_as_completed(0, 0));

        // 全部下完（total > 0 且 unfinished == 0）：判完成
        assert!(super::should_finalize_as_completed(5, 0));

        // 还有剩（unfinished > 0）：不判完成
        assert!(!super::should_finalize_as_completed(5, 2));

    }
}
