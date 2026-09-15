use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::{
    download_manager::DownloadTaskState,
    types::{ChapterInfo, Comic, LogLevel},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data")]
pub enum DownloadTaskEvent {
    #[serde(rename_all = "camelCase")]
    Create {
        state: DownloadTaskState,
        comic: Box<Comic>,
        chapter_info: Box<ChapterInfo>,
        downloaded_img_count: u32,
        total_img_count: u32,
    },

    #[serde(rename_all = "camelCase")]
    Update {
        chapter_id: String,
        state: DownloadTaskState,
        downloaded_img_count: u32,
        total_img_count: u32,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEvent {
    pub timestamp: String,
    pub level: LogLevel,
    pub fields: HashMap<String, serde_json::Value>,
    pub target: String,
    pub filename: String,
    #[serde(rename = "line_number")]
    pub line_number: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data")]
pub enum DownloadAllFavoritesEvent {
    #[serde(rename_all = "camelCase")]
    GettingFavorites,

    #[serde(rename_all = "camelCase")]
    GettingComics { current: i64, total: i64 },

    #[serde(rename_all = "camelCase")]
    EndGetComics,

    #[serde(rename_all = "camelCase")]
    StartCreateDownloadTasks {
        comic_id: String,
        comic_title: String,
        current: i64,
        total: i64,
    },

    #[serde(rename_all = "camelCase")]
    CreatingDownloadTask { comic_id: String, current: i64 },

    #[serde(rename_all = "camelCase")]
    EndCreateDownloadTasks { comic_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data")]
pub enum UpdateDownloadedComicsEvent {
    #[serde(rename_all = "camelCase")]
    GetComicStart { total: i64 },

    #[serde(rename_all = "camelCase")]
    GetComicProgress { current: i64, total: i64 },

    #[serde(rename_all = "camelCase")]
    CreateDownloadTasksStart {
        comic_id: String,
        comic_title: String,
        current: i64,
        total: i64,
    },

    #[serde(rename_all = "camelCase")]
    CreateDownloadTaskProgress { comic_id: String, current: i64 },

    #[serde(rename_all = "camelCase")]
    CreateDownloadTasksEnd { comic_id: String },

    #[serde(rename_all = "camelCase")]
    GetComicEnd,
}