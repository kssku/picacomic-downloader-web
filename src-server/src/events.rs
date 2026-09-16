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
        /// 累计重试次数。旧前端不认识这个字段会直接忽略，
        /// 所以这里加字段是向后兼容的。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_count: Option<u32>,
        /// 最近一次失败原因，成功时为 `None`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_error: Option<String>,
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

