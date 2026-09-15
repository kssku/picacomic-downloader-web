//! 业务命令层：原 `src-tauri/src/commands.rs` 的 Web 版对应物。
//!
//! 与桌面版的差异：
//! - `#[tauri::command]` / `#[specta::specta]` 全部去掉，改成普通 `async fn`，
//!   由 `api::routes` 里的 axum handler 调用。
//! - `AppHandle` → `AppContext`。
//! - `event.emit(&app)` → `app.events().emit(topic, &event)`。
//! - 导出 CBZ/PDF、排行榜、章节图片预览、文件管理器定位等命令已按需求删除。

use std::time::Duration;

use anyhow::{anyhow, Context as _};
use tokio::task::JoinSet;
use tokio::time::sleep;
use walkdir::WalkDir;

use crate::config::Config;
use crate::context::AppContext;
use crate::errors::{CommandError, CommandResult};
use crate::event_bus::topics;
use crate::events::{DownloadAllFavoritesEvent, UpdateDownloadedComicsEvent};
use crate::extensions::{AnyhowErrorToStringChain as _, AppContextExt, WalkDirEntryExt};
use crate::responses::UserProfileDetailRespData;
use crate::types::{
    ChapterInfo, Comic, ComicInFavorite, ComicInSearch, GetFavoriteResult, GetFavoriteSort,
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

pub async fn get_favorite(
    app: &AppContext,
    sort: GetFavoriteSort,
    page: i64,
) -> CommandResult<GetFavoriteResult> {
    let pica_client = app.get_pica_client();

    let get_favorite_resp_data = pica_client
        .get_favorite(sort, page)
        .await
        .map_err(|err| CommandError::from("获取收藏的漫画失败", err))?;

    let get_favorite_result = GetFavoriteResult::from_resp_data(app, get_favorite_resp_data)
        .map_err(|err| CommandError::from("获取收藏的漫画失败", err))?;

    Ok(get_favorite_result)
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

/// 下载整个收藏夹。
///
/// 收藏夹页并发拉取，随后逐本串行处理并按 `download_all_favorites_interval_sec`
/// 休息，避免触发服务端风控。
#[allow(clippy::cast_possible_wrap)]
pub async fn download_all_favorites(app: &AppContext) -> CommandResult<()> {
    let config = app.config_read();
    let pica_client = app.get_pica_client();
    let download_manager = app.get_download_manager();
    let events = app.events();

    let mut favorite_comics = Vec::new();
    events.emit(
        topics::DOWNLOAD_ALL_FAVORITES,
        &DownloadAllFavoritesEvent::GettingFavorites,
    );

    // 获取收藏夹第一页
    let first_page = pica_client
        .get_favorite(GetFavoriteSort::TimeNewest, 1)
        .await
        .context("获取收藏夹的第`1`页失败")
        .map_err(|err| CommandError::from("下载收藏夹失败", err))?;

    favorite_comics.extend(first_page.comics.docs);
    let page_count = first_page.comics.pages;

    // 并发获取剩余页
    let mut join_set = JoinSet::new();
    for page in 2..=page_count {
        let pica_client = pica_client.clone();
        join_set.spawn(async move {
            let page = pica_client
                .get_favorite(GetFavoriteSort::TimeNewest, page)
                .await
                .context(format!("获取收藏夹的第`{page}`页失败"))?;
            Ok::<_, anyhow::Error>(page)
        });
    }

    while let Some(joined) = join_set.join_next().await {
        let page = joined
            .context("收藏夹分页任务 panic")
            .map_err(|err| CommandError::from("下载收藏夹失败", err))?
            .map_err(|err| CommandError::from("下载收藏夹失败", err))?;
        favorite_comics.extend(page.comics.docs);
    }

    // 至此收藏夹已全部获取完毕
    let total = favorite_comics.len() as i64;
    let interval_sec = config.download_all_favorites_interval_sec;

    for (i, favorite_comic) in favorite_comics.into_iter().enumerate() {
        let comic_title = &favorite_comic.title;
        let comic_id = &favorite_comic.id;

        let comic = match utils::get_comic(app, comic_id)
            .await
            .context(format!("获取ID为`{comic_id}`的漫画失败"))
        {
            Ok(comic) => comic,
            Err(err) => {
                let err_title = format!("下载收藏夹过程中，获取漫画`{comic_title}`失败，已跳过");
                let err = err.context("可能是频率太高，请手动去`配置`里调整`下载整个收藏夹时，每处理完一个收藏夹中的漫画后休息`");
                tracing::error!(err_title, message = err.to_string_chain());
                sleep(Duration::from_secs(interval_sec)).await;
                continue;
            }
        };

        let current = (i + 1) as i64;
        events.emit(
            topics::DOWNLOAD_ALL_FAVORITES,
            &DownloadAllFavoritesEvent::GettingComics { current, total },
        );

        let chapter_infos: Vec<&ChapterInfo> = comic
            .chapter_infos
            .iter()
            .filter(|chapter_info| chapter_info.is_downloaded != Some(true))
            .collect();

        if chapter_infos.is_empty() {
            sleep(Duration::from_secs(interval_sec)).await;
            continue;
        }

        events.emit(
            topics::DOWNLOAD_ALL_FAVORITES,
            &DownloadAllFavoritesEvent::StartCreateDownloadTasks {
                comic_id: comic.id.clone(),
                comic_title: comic.title.clone(),
                current: 0,
                total: chapter_infos.len() as i64,
            },
        );

        for (current, chapter_info) in chapter_infos.into_iter().enumerate() {
            let chapter_id = chapter_info.chapter_id.clone();
            let current = current as i64 + 1;

            // 与原版一致：单个任务创建失败只记日志，不中断整个收藏夹。
            if let Err(err) = download_manager.create_download_task(comic.clone(), chapter_id) {
                tracing::warn!(
                    comic_id = comic.id.as_str(),
                    message = err.to_string_chain(),
                    "创建下载任务失败，已跳过"
                );
            }

            events.emit(
                topics::DOWNLOAD_ALL_FAVORITES,
                &DownloadAllFavoritesEvent::CreatingDownloadTask {
                    comic_id: comic.id.clone(),
                    current,
                },
            );

            sleep(Duration::from_millis(100)).await;
        }

        events.emit(
            topics::DOWNLOAD_ALL_FAVORITES,
            &DownloadAllFavoritesEvent::EndCreateDownloadTasks {
                comic_id: comic.id.clone(),
            },
        );

        sleep(Duration::from_secs(interval_sec)).await;
    }

    events.emit(
        topics::DOWNLOAD_ALL_FAVORITES,
        &DownloadAllFavoritesEvent::EndGetComics,
    );

    Ok(())
}

// ════════════════════════════════════════════════════════════════
// 已下载库存
// ════════════════════════════════════════════════════════════════

/// 扫描下载目录，读出所有漫画元数据。
///
/// 注意：这是同步 `WalkDir` 全量遍历，磁盘慢时会阻塞当前线程，
/// 因此路由层用 `spawn_blocking` 包一层。
#[allow(clippy::cast_possible_wrap, clippy::too_many_lines)]
pub fn get_downloaded_comics(app: &AppContext) -> Vec<Comic> {
    let download_dir = app.config_read().download_dir.clone();

    // 遍历下载目录，收集所有漫画元数据的路径和修改时间
    let mut metadata_path_with_modify_time = Vec::new();
    for entry in WalkDir::new(&download_dir)
        .into_iter()
        .filter_map(Result::ok)
    {
        let path = entry.path();

        if !entry.is_comic_metadata() {
            continue;
        }

        let metadata = match path
            .metadata()
            .map_err(anyhow::Error::from)
            .context(format!("获取`{}`的metadata失败", path.display()))
        {
            Ok(metadata) => metadata,
            Err(err) => {
                let err_title = "获取已下载漫画的过程中遇到错误，已跳过";
                let string_chain = err.to_string_chain();
                tracing::error!(err_title, message = string_chain);
                continue;
            }
        };

        let modify_time = match metadata
            .modified()
            .map_err(anyhow::Error::from)
            .context(format!("获取`{}`的修改时间失败", path.display()))
        {
            Ok(modify_time) => modify_time,
            Err(err) => {
                let err_title = "获取已下载漫画的过程中遇到错误，已跳过";
                let string_chain = err.to_string_chain();
                tracing::error!(err_title, message = string_chain);
                continue;
            }
        };

        metadata_path_with_modify_time.push((path.to_path_buf(), modify_time));
    }

    // 按修改时间倒序，新下载的排在前面
    metadata_path_with_modify_time.sort_by(|a, b| b.1.cmp(&a.1));

    let mut comics = Vec::new();
    for (metadata_path, _) in metadata_path_with_modify_time {
        let metadata_str = match std::fs::read_to_string(&metadata_path)
            .map_err(anyhow::Error::from)
            .context(format!("读取`{}`失败", metadata_path.display()))
        {
            Ok(s) => s,
            Err(err) => {
                let err_title = "获取已下载漫画的过程中遇到错误，已跳过";
                let string_chain = err.to_string_chain();
                tracing::error!(err_title, message = string_chain);
                continue;
            }
        };

        let comic = match serde_json::from_str::<Comic>(&metadata_str)
            .map_err(anyhow::Error::from)
            .context(format!(
                "将`{}`反序列化为Comic失败",
                metadata_path.display()
            )) {
            Ok(comic) => comic,
            Err(err) => {
                let err_title = "获取已下载漫画的过程中遇到错误，已跳过";
                let string_chain = err.to_string_chain();
                tracing::error!(err_title, message = string_chain);
                continue;
            }
        };

        comics.push(comic);
    }

    // 同一个漫画 ID 可能有多个版本目录，只保留修改时间最新的那个
    let mut seen = std::collections::HashSet::new();
    let mut unique_comics = Vec::new();
    for comic in comics {
        if seen.insert(comic.id.clone()) {
            unique_comics.push(comic);
        } else {
            tracing::warn!(
                comic_id = comic.id.as_str(),
                title = comic.title.as_str(),
                "存在多个版本的已下载漫画，已忽略较旧的"
            );
        }
    }

    unique_comics
}

/// 更新库存：遍历已下载漫画，把新增章节加入下载队列。
#[allow(clippy::cast_possible_wrap)]
pub async fn update_downloaded_comics(app: &AppContext) -> CommandResult<()> {
    let config = app.config_read();
    let download_manager = app.get_download_manager();
    let events = app.events();

    // 从下载目录中获取已下载的漫画（同步扫描，放到阻塞线程池）
    let app_for_scan = app.clone();
    let downloaded_comics = tokio::task::spawn_blocking(move || get_downloaded_comics(&app_for_scan))
        .await
        .context("扫描已下载漫画的任务 panic")
        .map_err(|err| CommandError::from("更新库存失败", err))?;

    let total = downloaded_comics.len() as i64;
    let interval_sec = config.update_downloaded_comics_interval_sec;

    events.emit(
        topics::UPDATE_DOWNLOADED_COMICS,
        &UpdateDownloadedComicsEvent::GetComicStart { total },
    );

    for (i, downloaded_comic) in downloaded_comics.into_iter().enumerate() {
        let comic_title = &downloaded_comic.title;
        let comic_id = &downloaded_comic.id;
        let current = (i + 1) as i64;

        events.emit(
            topics::UPDATE_DOWNLOADED_COMICS,
            &UpdateDownloadedComicsEvent::GetComicProgress { current, total },
        );

        let comic = match utils::get_comic(app, comic_id)
            .await
            .context(format!("获取ID为`{comic_id}`的漫画失败"))
        {
            Ok(comic) => comic,
            Err(err) => {
                let err_title = format!("更新库存过程中，获取漫画`{comic_title}`失败，已跳过");
                let err = err.context("可能是频率太高，请手动去`配置`里调整`更新库存时，每处理完一个已下载的漫画后休息`");
                let string_chain = err.to_string_chain();
                tracing::error!(err_title, message = string_chain);
                sleep(Duration::from_secs(interval_sec)).await;
                continue;
            }
        };

        // 至少有一个章节已下载才认为这本漫画在库存里
        let has_downloaded_chapter = comic
            .chapter_infos
            .iter()
            .any(|chapter_info| chapter_info.is_downloaded == Some(true));

        if !has_downloaded_chapter {
            sleep(Duration::from_secs(interval_sec)).await;
            continue;
        }

        let chapter_infos: Vec<&ChapterInfo> = comic
            .chapter_infos
            .iter()
            .filter(|chapter| chapter.is_downloaded != Some(true))
            .collect();

        if chapter_infos.is_empty() {
            sleep(Duration::from_secs(interval_sec)).await;
            continue;
        }

        events.emit(
            topics::UPDATE_DOWNLOADED_COMICS,
            &UpdateDownloadedComicsEvent::CreateDownloadTasksStart {
                comic_id: comic.id.clone(),
                comic_title: comic.title.clone(),
                current: 0,
                total: chapter_infos.len() as i64,
            },
        );

        for (i, chapter_info) in chapter_infos.into_iter().enumerate() {
            let chapter_id = chapter_info.chapter_id.clone();
            let current = (i + 1) as i64;

            if let Err(err) = download_manager.create_download_task(comic.clone(), chapter_id) {
                tracing::warn!(
                    comic_id = comic.id.as_str(),
                    message = err.to_string_chain(),
                    "创建下载任务失败，已跳过"
                );
            }

            events.emit(
                topics::UPDATE_DOWNLOADED_COMICS,
                &UpdateDownloadedComicsEvent::CreateDownloadTaskProgress {
                    comic_id: comic.id.clone(),
                    current,
                },
            );

            sleep(Duration::from_millis(100)).await;
        }

        events.emit(
            topics::UPDATE_DOWNLOADED_COMICS,
            &UpdateDownloadedComicsEvent::CreateDownloadTasksEnd {
                comic_id: comic.id.clone(),
            },
        );

        sleep(Duration::from_secs(interval_sec)).await;
    }

    events.emit(
        topics::UPDATE_DOWNLOADED_COMICS,
        &UpdateDownloadedComicsEvent::GetComicEnd,
    );

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

pub fn get_synced_comic_in_favorite(
    app: &AppContext,
    mut comic: ComicInFavorite,
) -> CommandResult<ComicInFavorite> {
    let id_to_dir_map = utils::create_id_to_dir_map(app)
        .context("创建漫画ID到下载目录映射失败")
        .map_err(|err| {
            let err_title = format!("漫画`{}`同步ComicInFavorite的字段失败", comic.title);
            CommandError::from(&err_title, err)
        })?;

    comic.update_fields(&id_to_dir_map);

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