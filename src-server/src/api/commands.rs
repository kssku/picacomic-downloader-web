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
// 已下载库存
// ════════════════════════════════════════════════════════════════

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