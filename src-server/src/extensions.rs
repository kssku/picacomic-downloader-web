//! 通用扩展 trait。
//!
//! 原桌面版的 `AppHandleExt`（`app.get_config()` 等）被 `AppContextExt` 取代，
//! 实现在 `context.rs` 里。

use parking_lot::RwLock;

use crate::{
    config::Config, context::AppContext, download_manager::DownloadManager, pica_client::PicaClient,
    types::ChapterInfo,
};

pub trait AnyhowErrorToStringChain {
    /// 将 `anyhow::Error` 转换为chain格式  
    /// # Example  
    /// 0: error message\
    /// 1: error message\
    /// 2: error message  
    fn to_string_chain(&self) -> String;
}

impl AnyhowErrorToStringChain for anyhow::Error {
    fn to_string_chain(&self) -> String {
        use std::fmt::Write;
        self.chain()
            .enumerate()
            .fold(String::new(), |mut output, (i, e)| {
                let _ = writeln!(output, "{i}: {e}");
                output
            })
    }
}

pub trait PathIsImg {
    /// 判断路径是否为图片(jpg/png/webp/gif)
    fn is_img(&self) -> bool;

    /// 判断路径是否为普通图片(jpg/png/webp)
    fn is_common_img(&self) -> bool;
}

impl PathIsImg for std::path::Path {
    fn is_img(&self) -> bool {
        self.extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_lowercase)
            .is_some_and(|ext| matches!(ext.as_str(), "jpg" | "png" | "webp" | "gif"))
    }

    fn is_common_img(&self) -> bool {
        self.extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_lowercase)
            .is_some_and(|ext| matches!(ext.as_str(), "jpg" | "png" | "webp"))
    }
}

pub trait WalkDirEntryExt {
    fn is_comic_metadata(&self) -> bool;
    fn is_chapter_metadata(&self) -> bool;
}
impl WalkDirEntryExt for walkdir::DirEntry {
    fn is_comic_metadata(&self) -> bool {
        let path = self.path();
        if !self.file_type().is_file() {
            return false;
        }
        if self.file_name() != "元数据.json" {
            return false;
        }

        // TODO: 这部分是为了兼容v0.6.0及之前的版本，计划在v0.8.0之后移除
        let Ok(metadata_str) = std::fs::read_to_string(path) else {
            return false;
        };
        if serde_json::from_str::<ChapterInfo>(&metadata_str).is_ok() {
            // 如果能反序列化为 `ChapterInfo`，说明是章节元数据
            // 重命名 `元数据.json` 为 `章节元数据.json`
            let new_path = path.with_file_name("章节元数据.json");
            let _ = std::fs::rename(path, new_path);
            return false;
        }

        true
    }

    fn is_chapter_metadata(&self) -> bool {
        if !self.file_type().is_file() {
            return false;
        }
        if self.file_name() != "章节元数据.json" {
            return false;
        }

        true
    }
}

/// 原 `AppHandleExt` 的 Web 版对应物。
///
/// 与桌面版不同，这里直接返回具体类型而不是 Tauri 的 `State<T>`，
/// 因为 `AppContext` 内部已用 `Arc` 管理生命周期，不需要 `State` 包装。
pub trait AppContextExt {
    fn get_config(&self) -> &std::sync::Arc<RwLock<Config>>;
    fn get_pica_client(&self) -> PicaClient;
    fn get_download_manager(&self) -> DownloadManager;
}

impl AppContextExt for AppContext {
    fn get_config(&self) -> &std::sync::Arc<RwLock<Config>> {
        self.config()
    }

    fn get_pica_client(&self) -> PicaClient {
        self.pica_client()
    }

    fn get_download_manager(&self) -> DownloadManager {
        self.download_manager()
    }
}
