//! 配置读写。与原桌面版逻辑一致，只是把 `app.path().app_data_dir()`
//! 换成显式传入的 `config_path`，从而不再依赖 Tauri。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::types::DownloadFormat;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub token: String,
    pub download_dir: PathBuf,
    pub export_dir: PathBuf,
    pub enable_file_logger: bool,
    pub download_format: DownloadFormat,
    pub dir_fmt: String,
    pub proxy_mode: ProxyMode,
    pub proxy_host: String,
    pub proxy_port: u16,
    pub chapter_concurrency: usize,
    pub chapter_download_interval_sec: u64,
    pub img_concurrency: usize,
    pub img_download_interval_sec: u64,
    pub should_download_cover: bool,
    pub api_base_url: String,
}

impl Config {
    /// 从指定路径加载配置。文件不存在则用默认值创建。
    ///
    /// 反序列化失败时走 `merge_config`：把新版本默认值里缺失的键补进去，
    /// 再重新解析——保证旧版本配置文件升级后不丢数据。
    pub fn load(config_path: &Path) -> anyhow::Result<Self> {
        let data_dir = config_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        let config = if config_path.exists() {
            let config_string = std::fs::read_to_string(config_path)?;
            match serde_json::from_str(&config_string) {
                Ok(config) => config,
                Err(_) => Config::merge_config(&config_string, &data_dir),
            }
        } else {
            Config::default_at(&data_dir)
        };
        config.save(config_path)?;
        Ok(config)
    }

    /// 写回配置文件。
    pub fn save(&self, config_path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let config_string = serde_json::to_string_pretty(self)?;
        std::fs::write(config_path, config_string)?;
        Ok(())
    }

    fn merge_config(config_string: &str, data_dir: &Path) -> Config {
        let Ok(mut json_value) = serde_json::from_str::<serde_json::Value>(config_string) else {
            return Config::default_at(data_dir);
        };
        let serde_json::Value::Object(ref mut map) = json_value else {
            return Config::default_at(data_dir);
        };
        let Ok(default_config_value) = serde_json::to_value(Config::default_at(data_dir)) else {
            return Config::default_at(data_dir);
        };
        let serde_json::Value::Object(default_map) = default_config_value else {
            return Config::default_at(data_dir);
        };
        for (key, value) in default_map {
            map.entry(key).or_insert(value);
        }
        let Ok(config) = serde_json::from_value(json_value) else {
            return Config::default_at(data_dir);
        };
        config
    }

    /// 默认配置。`data_dir` 是数据根目录（容器里通常是 `/data`）。
    pub fn default_at(data_dir: &Path) -> Config {
        Config {
            token: String::new(),
            download_dir: data_dir.join("漫画下载"),
            export_dir: data_dir.join("漫画导出"),
            enable_file_logger: true,
            download_format: DownloadFormat::default(),
            dir_fmt: "{comic_id}/{order}".to_string(),
            proxy_mode: ProxyMode::System,
            proxy_host: "127.0.0.1".to_string(),
            proxy_port: 7890,
            chapter_concurrency: 3,
            chapter_download_interval_sec: 0,
            img_concurrency: 20,
            img_download_interval_sec: 0,
            should_download_cover: true,
            api_base_url: "https://picaapi.go2778.com".to_string(),
        }
    }
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ProxyMode {
    #[default]
    System,
    NoProxy,
    Custom,
}