//! 用 `AppContext` 替代 Tauri 的 `AppHandle`。
//!
//! 原桌面版通过 `app.state::<T>()` / `app.path().app_data_dir()` / `event.emit(&app)`
//! 访问全局资源。Web 版把这些收敛到一个显式、可克隆的上下文对象里，
//! 从而让 `pica_client` / `download_manager` / `types` 等核心模块完全脱离 Tauri。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use parking_lot::RwLock;

use crate::config::Config;
use crate::download_manager::DownloadManager;
use crate::event_bus::EventBus;
use crate::pica_client::PicaClient;
use crate::store::Store;

/// 运行期路径。全部来自环境变量，Docker 里挂一个卷到 `/data` 即可。
#[derive(Debug, Clone)]
pub struct Paths {
    /// 数据根目录（配置、日志、默认下载目录都在它下面）
    pub data_dir: PathBuf,
}

impl Paths {
    pub fn from_env() -> anyhow::Result<Self> {
        let data_dir = std::env::var("PICA_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("./data"));
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("创建数据目录 `{}` 失败", data_dir.display()))?;
        Ok(Self { data_dir })
    }

    pub fn config_path(&self) -> PathBuf {
        self.data_dir.join("config.json")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join("日志")
    }

    /// 下载任务持久化数据库。与青龙的 `bica_comics.db` 完全独立。
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("pica_server.db")
    }
}

/// 全局应用上下文。廉价可克隆（内部全是 `Arc`）。
#[derive(Clone)]
pub struct AppContext {
    paths: Paths,
    config: Arc<RwLock<Config>>,
    pica_client: Arc<RwLock<Option<PicaClient>>>,
    download_manager: Arc<RwLock<Option<DownloadManager>>>,
    /// 任务持久化。构造阶段就打开——它没有循环依赖，不像
    /// `PicaClient` / `DownloadManager` 那样需要两段式构造。
    store: Store,
    events: EventBus,
}

impl AppContext {
    /// 第一阶段构造：只加载配置与路径。
    /// `PicaClient` / `DownloadManager` 在之后通过 `init_runtime` 注入，
    /// 因为它们自身持有 `AppContext`，会形成循环引用。
    pub fn new(paths: Paths) -> anyhow::Result<Self> {
        let config = Config::load(&paths.config_path())?;

        // 数据库损坏时 `open_or_recover` 会备份旧库并重建空库，
        // 而不是让整个服务起不来。
        let store = Store::open_or_recover(&paths.db_path())?;

        Ok(Self {
            paths,
            config: Arc::new(RwLock::new(config)),
            pica_client: Arc::new(RwLock::new(None)),
            download_manager: Arc::new(RwLock::new(None)),
            store,
            events: EventBus::new(),
        })
    }

    /// 第二阶段构造：创建 `PicaClient` 与 `DownloadManager` 并注入。
    pub fn init_runtime(&self) -> anyhow::Result<()> {
        let client = PicaClient::new(self.clone());
        *self.pica_client.write() = Some(client);

        let manager = DownloadManager::new(self.clone());
        *self.download_manager.write() = Some(manager);

        Ok(())
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn config(&self) -> &Arc<RwLock<Config>> {
        &self.config
    }

    /// 读配置快照。
    pub fn config_read(&self) -> Config {
        self.config.read().clone()
    }

    pub fn save_config(&self, config: &Config) -> anyhow::Result<()> {
        config.save(&self.paths.config_path())?;
        *self.config.write() = config.clone();
        Ok(())
    }

    pub fn events(&self) -> &EventBus {
        &self.events
    }

    /// 取任务持久化层。廉价可克隆（内部是 `Arc`）。
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// 取 `PicaClient`。初始化后必然存在。
    pub fn pica_client(&self) -> PicaClient {
        self.pica_client
            .read()
            .clone()
            .expect("PicaClient 尚未初始化")
    }

    /// 取 `DownloadManager`。初始化后必然存在。
    pub fn download_manager(&self) -> DownloadManager {
        self.download_manager
            .read()
            .clone()
            .expect("DownloadManager 尚未初始化")
    }

    /// 配置变更后重建 HTTP 客户端（代理/API 地址变了需要重建）。
    pub fn reload_pica_client(&self) {
        self.pica_client().reload_client();
    }

    /// 配置变更后应用新的下载并发度（D8 的修复）。
    ///
    /// 旧实现会 `shutdown()` 旧 manager 再 `DownloadManager::new()` 一个新的。
    /// 问题在于 `shutdown()` 只是把任务标记成 `Cancelled`，**已经 spawn 出去、
    /// 正在跑的下载任务依然活着**，并继续持有旧信号量的 permit。于是旧信号量
    /// 不会释放、新信号量又被创建，实际并发变成配置值的两倍。
    ///
    /// 改成在同一个 manager 上原地调整 permit：信号量对象不变，
    /// 在跑的任务无感，并发度立刻生效。
    pub fn reload_download_manager(&self) -> anyhow::Result<()> {
        let (chapter_concurrency, img_concurrency) = {
            let config = self.config.read();
            (config.chapter_concurrency, config.img_concurrency)
        };

        if let Some(manager) = self.download_manager.read().clone() {
            manager.update_concurrency(chapter_concurrency, img_concurrency);
        }

        Ok(())
    }
}

/// 兼容原代码里的 `download_dir` 等路径取用。
impl AppContext {
    pub fn download_dir(&self) -> PathBuf {
        self.config.read().download_dir.clone()
    }

    pub fn logs_dir(&self) -> anyhow::Result<PathBuf> {
        Ok(self.paths.logs_dir())
    }
}

/// 让 `&Path` 上的 join 更顺手（避免到处写 `.to_path_buf()`）。
pub fn join(base: &Path, child: impl AsRef<Path>) -> PathBuf {
    base.join(child.as_ref())
}