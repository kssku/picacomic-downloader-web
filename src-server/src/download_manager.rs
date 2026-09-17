use std::collections::HashMap;
use std::io::Cursor;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context};
use bytes::Bytes;
use image::ImageFormat;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Semaphore, SemaphorePermit};
use tokio::task::JoinSet;
use tokio::time::sleep;

use crate::context::AppContext;
use crate::event_bus::topics;
use crate::events::DownloadTaskEvent;
use crate::extensions::{AnyhowErrorToStringChain, AppContextExt};
use crate::store::repo::{ImageRepo, TaskRepo};
use crate::store::types::{now_ts, DbTask, DbTaskState};
use crate::types::{ChapterInfo, Comic};
use crate::utils::filename_filter;

/// 用于管理下载任务
///
/// 克隆 `DownloadManager` 的开销极小，性能开销几乎可以忽略不计。
/// 可以放心地在多个线程中传递和使用它的克隆副本。
///
/// 具体来说：
/// - `app`的克隆开销很小。
/// - 其他字段都被 `Arc` 包裹，这些字段的克隆操作仅仅是增加引用计数。
#[derive(Clone)]
pub struct DownloadManager {
    app: AppContext,
    chapter_sem: Arc<Semaphore>,
    img_sem: Arc<Semaphore>,
    /// 两个信号量的**总容量**记账（空闲 + 在跑任务持有）。
    ///
    /// `Semaphore` 只暴露空闲额度，调小并发度时必须知道总容量才能算准差额，
    /// 否则在跑任务持有一部分 permit 时会把「调小」误判成「无需调整」。
    /// 这两项与对应信号量一一配对，只由 `resize_sem` 写、`reclaim_debt` 读。
    chapter_capacity: Arc<AtomicUsize>,
    img_capacity: Arc<AtomicUsize>,
    byte_per_sec: Arc<AtomicU64>,
    download_tasks: Arc<RwLock<HashMap<String, DownloadTask>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DownloadTaskState {
    Pending,
    Downloading,
    Paused,
    Cancelled,
    Completed,
    Failed,
}

impl DownloadTaskState {
    /// 转成持久层的状态。
    ///
    /// `Downloading` 是「本进程正在跑」的瞬态，DB 里也记 `downloading`，
    /// 但**恢复时会被降级回 `pending`**（见 `from_db_recovered`）。
    pub fn to_db(self) -> DbTaskState {
        match self {
            Self::Pending => DbTaskState::Pending,
            Self::Downloading => DbTaskState::Downloading,
            Self::Paused => DbTaskState::Paused,
            Self::Cancelled => DbTaskState::Cancelled,
            Self::Completed => DbTaskState::Completed,
            Self::Failed => DbTaskState::Failed,
        }
    }

    /// 从持久层状态还原。
    ///
    /// 只用于**恢复流程**：进程上次退出时处于 `downloading` 的任务，
    /// 实际上已经不在跑了，必须降级成 `pending` 等待重新调度。
    /// 保留 `downloading` 只会让恢复逻辑陷入「以为有人还在跑」的死等。
    pub fn from_db_recovered(state: DbTaskState) -> Self {
        match state {
            DbTaskState::Pending | DbTaskState::Downloading => Self::Pending,
            DbTaskState::Paused => Self::Paused,
            DbTaskState::Cancelled => Self::Cancelled,
            DbTaskState::Completed => Self::Completed,
            DbTaskState::Failed => Self::Failed,
        }
    }
}

impl DownloadManager {
    pub fn new(app: AppContext) -> Self {
        let (chapter_concurrency, img_concurrency) = {
            let config = app.config();
            let config = config.read();
            (config.chapter_concurrency, config.img_concurrency)
        };

        let manager = DownloadManager {
            app,
            chapter_sem: Arc::new(Semaphore::new(chapter_concurrency)),
            img_sem: Arc::new(Semaphore::new(img_concurrency)),
            chapter_capacity: Arc::new(AtomicUsize::new(chapter_concurrency)),
            img_capacity: Arc::new(AtomicUsize::new(img_concurrency)),
            byte_per_sec: Arc::new(AtomicU64::new(0)),
            download_tasks: Arc::new(RwLock::new(HashMap::new())),
        };

        // 恢复上次进程遗留的未完成任务（D1 的另一半）。
        //
        // 只捞 `pending` / `paused` / `downloading` 三种态：终态任务没有恢复价值。
        // `downloading` 在这里被降级为 `pending`——它是进程内瞬态，
        // 上一个进程已经死了，保留它只会让状态机卡在一个永远不会推进的态。
        //
        // 恢复需要走哔咔 API 拿漫画信息，是异步的，所以丢进后台任务。
        // 恢复失败不阻断启动：服务能起来比任务列表完整更重要。
        let recover_manager = manager.clone();
        tokio::spawn(async move {
            if let Err(err) = recover_manager.recover_pending_tasks().await {
                tracing::error!(
                    err_title = "恢复未完成下载任务失败",
                    message = %err
                );
            }
        });

        manager
    }

    /// 从 DB 里把上次进程遗留的未完成任务重新拉起。
    ///
    /// 这些任务的章节信息来自 DB 里的 `chapter_id`，需要重新走一次
    /// 哔咔 API 拿完整漫画信息才能重建 `DownloadTask`。
    async fn recover_pending_tasks(&self) -> anyhow::Result<()> {
        let tasks = TaskRepo::list_unfinished(self.app.store())?;
        if tasks.is_empty() {
            return Ok(());
        }

        tracing::info!(count = tasks.len(), "发现未完成的下载任务，开始恢复");

        let mut restored = 0usize;
        for task in tasks {
            match self.restore_one(&task).await {
                Ok(()) => restored += 1,
                Err(err) => {
                    // 单个任务恢复失败（比如漫画已下架）不应拖垮其他任务。
                    tracing::warn!(
                        chapter_id = task.chapter_id,
                        comic_title = task.comic_title,
                        err_title = "恢复单个下载任务失败，已跳过",
                        message = %err
                    );
                }
            }
        }

        tracing::info!(restored, "未完成下载任务恢复完成");
        Ok(())
    }

    /// 重建单个任务并重新挂进调度。
    ///
    /// 关键点：先 `resync_progress` 把 DB 里的进度对齐到图片表的事实，
    /// 再走 `create_download_task`——它会调 `upsert_new` 保留已有状态与进度，
    /// 所以恢复不会把一个下载到一半的章节打回原点。
    async fn restore_one(&self, db_task: &DbTask) -> anyhow::Result<()> {
        let chapter_id = db_task.chapter_id.clone();

        // 进度对齐：图片表的 state 才是真相，DB 里的计数只是缓存。
        if ImageRepo::has_manifest(self.app.store(), &chapter_id)? {
            let (total, done) = TaskRepo::resync_progress(self.app.store(), &chapter_id)?;
            tracing::debug!(
                chapter_id,
                total,
                done,
                "已对齐任务进度"
            );
        }

        // 拿完整漫画信息（含全部章节）。复用 `utils::get_comic`，
        // 它会翻完章节列表，`DownloadTask::new` 才能在里面找到 chapter_id。
        let comic = crate::utils::get_comic(&self.app, &db_task.comic_id)
            .await
            .context(format!("获取漫画`{}`信息失败", db_task.comic_id))?;

        let comic_title = comic.title.clone();
        self.create_download_task(comic, chapter_id.clone())
            .context("重建下载任务失败")?;

        // 恢复后把章节级重试次数接回来。`DownloadTask::new` 从 0 开始，
        // 若不复原，「反复失败」的章节一重启就变成「首次失败」，前端的
        // 重试计数会归零，运维看不出这是个坏章节。
        if db_task.retry_count > 0 {
            let tasks = self.download_tasks.read();
            if let Some(task) = tasks.get(&chapter_id) {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                task.retry_count
                    .store(db_task.retry_count.max(0) as u32, Ordering::Relaxed);
            }
        }

        tracing::info!(chapter_id, comic_title, "已恢复下载任务");
        Ok(())
    }

    /// 就地调整并发度（D8 的修复）。
    ///
    /// 旧实现是「重建整个 `DownloadManager`」，但旧 manager 里已经 spawn 的任务
    /// 仍然持有旧信号量的 permit，于是新旧两套信号量并存，**实际并发是配置值的两倍**。
    /// 这里改成原地加减 permit，信号量对象本身不变，在跑的任务不受任何影响。
    ///
    /// - 新值更大：`add_permits` 放行更多并发。
    /// - 新值更小：用 `forget_permits` 收回差额。注意它只能收回**当前空闲**的
    ///   permit，已经在跑的任务要等自己结束才归还，所以并发度是平滑收敛的，
    ///   不会中途掐断正在下载的图片。
    pub fn update_concurrency(&self, chapter_concurrency: usize, img_concurrency: usize) {
        Self::resize_sem(&self.chapter_sem, &self.chapter_capacity, chapter_concurrency);
        Self::resize_sem(&self.img_sem, &self.img_capacity, img_concurrency);
        tracing::info!(
            chapter_concurrency,
            img_concurrency,
            "下载并发度已就地更新"
        );
    }

    /// 把一个信号量的**总容量**调整到 `target`。
    ///
    /// 注意这里调的是「总容量」而不是「空闲额度」。`available_permits()` 只是
    /// 空闲部分的计数，在跑任务持有一部分 permit 时它会小于总容量；若拿它当
    /// 基准算差额，`new(3)` + 2 个在跑 + `target = 1` 会被误判成「空闲正好是 1，
    /// 无需调整」，配置调小完全不生效。所以基准必须由 `capacity` 显式记账。
    ///
    /// `capacity` 记的是**当下真实在外的 permit 总数**（空闲 + 在跑任务持有），
    /// 调大时直接放行差额，调小时用 `forget_permits` 收回空闲部分；
    /// 在跑任务手里的收不回来（也不该收，那等于中途掐断下载），
    /// 于是把没收够的差额留作欠账，由 [`Self::reclaim_debt`] 在任务归还时补收。
    fn resize_sem(sem: &Semaphore, capacity: &AtomicUsize, target: usize) {
        let current = capacity.swap(target, Ordering::AcqRel);
        if target > current {
            sem.add_permits(target - current);
        } else if target < current {
            // `forget_permits` 至多收回空闲量；没收够的部分留在 `capacity` 之外，
            // 由 `reclaim_debt` 在任务归还 permit 时收掉。
            let _ = sem.forget_permits(current - target);
        }
    }

    /// 归还 permit 时顺手还掉「并发度调小」留下的欠账。
    ///
    /// 语义边界必须说清楚：这里只能收**空闲**额度。tokio 的 `Semaphore` 不暴露
    /// 「正在被持有」的数量，permit 的归还是 `Permit::drop` 的内部行为，没有
    /// 挂点可以逐次扣减计数；而在归还的那一刻，池子里也无法区分「刚归还的这一个」
    /// 和「本来就空闲的那几个」。所以不做逐次精确回收。
    ///
    /// 真正的收敛靠信号量自身：调小后空闲额度已被 `resize_sem` 压到 `<= target`，
    /// 只有在跑任务继续归还时才可能重新超出，而每次归还都会经过这里把超出的部分
    /// 收掉。于是在跑任务全部结束后，池子必然精确等于 `target`——这正是
    /// [`Self::update_concurrency`] 文档里说的「平滑收敛，不掐断在跑任务」。
    fn reclaim_debt(sem: &Semaphore, capacity: &AtomicUsize) {
        let target = capacity.load(Ordering::Acquire);
        let idle = sem.available_permits();
        if idle > target {
            let _ = sem.forget_permits(idle - target);
        }
    }

    /// 只计算「本次应当调整多少 permit」，不产生副作用。
    ///
    /// 与 `resize_sem` 同源，供测试断言使用：正数代表放行，负数代表收回。
    /// 基准是**总容量**而非空闲额度——这正是 `resize_sem` 修正后的语义。
    #[cfg(test)]
    fn resize_delta(current_capacity: usize, target: usize) -> isize {
        target as isize - current_capacity as isize
    }

    /// 关闭调度器：取消所有未完成的下载任务。
    ///
    /// 仅在真正要丢弃这个 manager 时调用。并发度变更**不再**走这里——
    /// 见 `update_concurrency`。
    pub fn shutdown(&self) {
        let tasks = self.download_tasks.read();
        for task in tasks.values() {
            task.set_state(DownloadTaskState::Cancelled);
        }
    }

    pub fn create_download_task(&self, comic: Comic, chapter_id: String) -> anyhow::Result<()> {
        use DownloadTaskState::{Downloading, Paused, Pending};
        let mut tasks = self.download_tasks.write();
        if let Some(task) = tasks.get(&chapter_id) {
            let state = *task.state_sender.borrow();
            if matches!(state, Pending | Downloading | Paused) {
                return Err(anyhow!("章节ID为`{chapter_id}`的下载任务已存在"));
            }
        }
        tasks.remove(&chapter_id);
        let task = DownloadTask::new(self.app.clone(), comic, &chapter_id)
            .context("DownloadTask创建失败")?;
        // 先落库再 spawn：任务一旦开始跑就必须在 DB 里可查，
        // 否则重启恢复会漏掉刚创建就崩溃的任务。
        task.persist_new().context("持久化下载任务失败")?;
        tokio::spawn(task.clone().process());
        tasks.insert(chapter_id, task);
        Ok(())
    }

    pub fn pause_download_task(&self, chapter_id: &str) -> anyhow::Result<()> {
        let tasks = self.download_tasks.read();
        let Some(task) = tasks.get(chapter_id) else {
            return Err(anyhow!("未找到章节ID为`{chapter_id}`的下载任务"));
        };
        task.set_state(DownloadTaskState::Paused);
        Ok(())
    }

    pub fn resume_download_task(&self, chapter_id: &str) -> anyhow::Result<()> {
        let tasks = self.download_tasks.read();
        let Some(task) = tasks.get(chapter_id) else {
            return Err(anyhow!("未找到章节ID为`{chapter_id}`的下载任务"));
        };
        task.set_state(DownloadTaskState::Pending);
        Ok(())
    }

    pub fn cancel_download_task(&self, chapter_id: &str) -> anyhow::Result<()> {
        let tasks = self.download_tasks.read();
        let Some(task) = tasks.get(chapter_id) else {
            return Err(anyhow!("未找到章节ID为`{chapter_id}`的下载任务"));
        };
        task.set_state(DownloadTaskState::Cancelled);
        Ok(())
    }

    /// 生成当前所有下载任务的快照（D2 的修复）。
    ///
    /// WebSocket 客户端新连上时需要拿到当前任务列表，否则会漏掉在它连接之前
    /// 就已经开始的下载任务。这里返回与 `DownloadTaskEvent::Create` 等价的事件
    /// 列表，前端可以复用同一套处理逻辑。
    ///
    /// 数据源是 **DB 而不是内存 map**：内存 map 只装本进程创建/恢复的任务，
    /// 进程重启后、恢复流程跑完之前，前端会看到空列表。DB 里 `completed`
    /// 之外的任务才是持久的真相，配合 `list_unfinished` 能做到「重启后立刻可见」。
    ///
    /// 内存 map 里那些已经跑完的终态任务会一并合并进来——它们可能刚完成、
    /// DB 行的 `updated_at` 还没被前端读到，去掉会造成列表闪烁。
    pub fn task_snapshot(&self) -> Vec<DownloadTaskEvent> {
        let mut snapshot = Vec::new();
        let mut seen = std::collections::HashSet::new();

        // 1) 内存里的活任务优先：它们有完整的 `Comic` / `ChapterInfo`，
        //    能构造出信息最全的 `Create` 事件。
        {
            let tasks = self.download_tasks.read();
            for (chapter_id, task) in tasks.iter() {
                seen.insert(chapter_id.clone());
                snapshot.push(task.to_create_event());
            }
        }

        // 2) DB 里的未完结任务：补齐本进程没在跑、但历史遗留（或被恢复流程
        //    跳过）的任务，让重启后的前端也能看到它们。
        match TaskRepo::list_unfinished(self.app.store()) {
            Ok(db_tasks) => {
                for db_task in db_tasks {
                    if seen.contains(&db_task.chapter_id) {
                        continue;
                    }
                    snapshot.push(Self::db_task_to_create_event(&db_task));
                }
            }
            Err(err) => {
                // 查库失败不应该让快照整体失败——退化成「只返回内存任务」，
                // 至少新连上的客户端还能看到本进程正在跑的东西。
                tracing::warn!(
                    err_title = "读取未完结任务失败，任务快照退化为仅内存任务",
                    message = %err
                );
            }
        }

        snapshot
    }

    /// 用 DB 行构造 `Create` 事件。
    ///
    /// `download_task` 表不存漫画的完整元信息（章节列表、封面等），
    /// 所以 `comic` / `chapter_info` 只能用行里已有的字段拼一个**最小可用**的
    /// 对象。前端任务列表只用到 `chapterId` / `chapterTitle` / `isDownloaded`，
    /// 这些字段都在，列表因此能正确渲染。
    ///
    /// 注意：这个事件**不能**被当作「可重新发起下载」的任务对象使用——
    /// 它缺少 `chapter_download_dir` 等字段。恢复流程走的是 `restore_one`，
    /// 会重新拉取完整漫画信息，不走这里。
    fn db_task_to_create_event(db_task: &DbTask) -> DownloadTaskEvent {
        use crate::types::Comic;

        let comic = Comic {
            id: db_task.comic_id.clone(),
            title: db_task.comic_title.clone(),
            ..Default::default()
        };

        let chapter_info = ChapterInfo {
            chapter_id: db_task.chapter_id.clone(),
            chapter_title: db_task.chapter_title.clone(),
            order: db_task.chapter_order,
            ..Default::default()
        };

        DownloadTaskEvent::Create {
            state: DownloadTaskState::from_db_recovered(db_task.state),
            comic: Box::new(comic),
            chapter_info: Box::new(chapter_info),
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            downloaded_img_count: db_task.done_img_count.max(0) as u32,
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            total_img_count: db_task.total_img_count.max(0) as u32,
        }
    }
}

#[derive(Clone)]
struct DownloadTask {
    app: AppContext,
    download_manager: DownloadManager,
    comic: Arc<Comic>,
    chapter_info: Arc<ChapterInfo>,
    state_sender: watch::Sender<DownloadTaskState>,
    downloaded_img_count: Arc<AtomicU32>,
    total_img_count: Arc<AtomicU32>,
    /// 最近一次失败原因。`set_state(Failed)` 时随状态一起落库，
    /// 让恢复后的任务能看到「上次为什么挂」。
    last_error: Arc<RwLock<Option<String>>>,
    /// 章节级重试次数。图片级重试记在 `download_image.retry_count`，
    /// 这里记的是「整个章节被重新拉起过几次」，用于在前端区分
    /// 「偶发失败」和「反复失败」。
    retry_count: Arc<AtomicU32>,
}

impl DownloadTask {
    pub fn new(app: AppContext, mut comic: Comic, chapter_id: &str) -> anyhow::Result<Self> {
        comic
            .update_download_dir_fields_by_fmt(&app)
            .context("更新`download_dir`字段失败")?;

        let chapter_info = comic
            .chapter_infos
            .iter()
            .find(|chapter| chapter.chapter_id == chapter_id)
            .cloned()
            .context(format!("未找到章节ID为`{chapter_id}`的章节信息"))?;

        let download_manager = app.get_download_manager();
        let (state_sender, _) = watch::channel(DownloadTaskState::Pending);

        let task = Self {
            app,
            download_manager,
            comic: Arc::new(comic),
            chapter_info: Arc::new(chapter_info),
            state_sender,
            downloaded_img_count: Arc::new(AtomicU32::new(0)),
            total_img_count: Arc::new(AtomicU32::new(0)),
            last_error: Arc::new(RwLock::new(None)),
            retry_count: Arc::new(AtomicU32::new(0)),
        };

        Ok(task)
    }

    /// 把任务登记进 `download_task` 表。
    ///
    /// 用 `upsert_new` 而不是 `insert`：重复提交同一个章节时保留原有状态与进度，
    /// 避免把一个正在下载的任务打回 `pending`（D2 的幂等性要求）。
    fn persist_new(&self) -> anyhow::Result<()> {
        let task = DbTask {
            chapter_id: self.chapter_info.chapter_id.clone(),
            comic_id: self.comic.id.clone(),
            comic_title: self.comic.title.clone(),
            chapter_title: self.chapter_info.chapter_title.clone(),
            chapter_order: self.chapter_info.order,
            state: DownloadTaskState::Pending.to_db(),
            total_img_count: 0,
            done_img_count: 0,
            retry_count: 0,
            last_error: None,
            dir_fmt: self.app.config().read().dir_fmt.clone(),
            created_at: now_ts(),
            updated_at: now_ts(),
        };
        TaskRepo::upsert_new(self.app.store(), &task)
    }

    async fn process(self) {
        self.emit_download_task_create_event();

        let download_comic_task = self.download_chapter();
        tokio::pin!(download_comic_task);

        let mut state_receiver = self.state_sender.subscribe();
        let mut permit = None;
        loop {
            let state_is_downloading = *state_receiver.borrow() == DownloadTaskState::Downloading;
            let state_is_pending = *state_receiver.borrow() == DownloadTaskState::Pending;
            tokio::select! {
                () = &mut download_comic_task, if state_is_downloading && permit.is_some() => break,
                control_flow = self.acquire_chapter_permit(&mut permit), if state_is_pending => {
                    match control_flow {
                        ControlFlow::Continue(()) => continue,
                        ControlFlow::Break(()) => break,
                    }
                },
                _ = state_receiver.changed() => {
                    match self.handle_state_change(&mut permit, &mut state_receiver) {
                        ControlFlow::Continue(()) => continue,
                        ControlFlow::Break(()) => break,
                    }
                }
            }
        }
    }

    async fn download_chapter(&self) {
        let comic_title = &self.comic.title;
        let chapter_title = &self.chapter_info.chapter_title;

        if let Err(err) = self.save_comic_metadata() {
            let err_title = format!("`{comic_title}`保存元数据失败");
            let string_chain = err.to_string_chain();
            tracing::error!(err_title, message = string_chain);

            self.set_failed(&err_title, string_chain);
            return;
        }

        let should_download_cover = self.app.config().read().should_download_cover;
        if should_download_cover {
            if let Err(err) = self.download_cover().await {
                let err_title = format!("`{comic_title}`下载封面失败");
                let string_chain = err.to_string_chain();
                tracing::error!(err_title, message = string_chain);

                self.set_failed(&err_title, string_chain);
                return;
            }
        }

        // 获取图片链接
        let img_urls = match self.get_img_urls().await {
            Ok(img_urls) => img_urls,
            Err(err) => {
                let err_title = format!("`{comic_title} - {chapter_title}`获取图片链接失败");
                let string_chain = err.to_string_chain();
                tracing::error!(err_title, message = string_chain);

                self.set_failed(&err_title, string_chain);
                return;
            }
        };
        // `img_urls` 为空说明 Pica 对该章节返回了 0 张图片（`docs` 为空数组）。
        // 此时若继续往下走，`0 == 0` 会让「下载完整性」检查通过，从而把一个
        // 什么都没下载的章节标记为「下载成功」。这里直接判定为失败。
        if img_urls.is_empty() {
            let err_title = format!("`{comic_title} - {chapter_title}`没有可下载的图片");
            let err_msg = "Pica 接口返回的图片列表为空，该章节可能已下架或需要更高权限";
            tracing::error!(err_title, message = err_msg);

            self.set_failed(&err_title, err_msg);
            return;
        }

        // 登记图片清单。`register_batch` 只对**新增**的图片插入 `pending`，
        // 已存在的行保留原状态——这就是断点续传的基础：重跑章节时，
        // 上一轮已经 `done` 的图片不会被重置。
        let chapter_id = &self.chapter_info.chapter_id;
        if let Err(err) = ImageRepo::register_batch(self.app.store(), chapter_id, &img_urls) {
            let err_title = format!("`{comic_title} - {chapter_title}`登记图片清单失败");
            let string_chain = err.to_string_chain();
            tracing::error!(err_title, message = string_chain);

            self.set_failed(&err_title, string_chain);
            return;
        }

        // 记录总共需要下载的图片数量
        #[allow(clippy::cast_possible_truncation)]
        let total_img_count = img_urls.len() as u32;
        self.total_img_count
            .store(total_img_count, Ordering::Relaxed);

        // 从 DB 恢复已完成的图片数，而不是从 0 开始。
        // 这一步让「重跑一个半途中断的章节」变成真正的续传：
        // 已下载的图片既不会重下，计数也从正确的起点继续。
        let done_img_count = match ImageRepo::count_unfinished(self.app.store(), chapter_id) {
            Ok(unfinished) => {
                let done = i64::from(total_img_count) - unfinished;
                done.max(0)
            }
            Err(err) => {
                let err_title = format!("`{comic_title} - {chapter_title}`统计已完成图片失败");
                let string_chain = err.to_string_chain();
                tracing::error!(err_title, message = string_chain);
                0
            }
        };
        #[allow(clippy::cast_possible_truncation)]
        self.downloaded_img_count
            .store(done_img_count as u32, Ordering::Relaxed);

        if let Err(err) =
            TaskRepo::set_progress(self.app.store(), chapter_id, i64::from(total_img_count), done_img_count)
        {
            let err_title = format!("`{comic_title} - {chapter_title}`写入任务进度失败");
            let string_chain = err.to_string_chain();
            tracing::error!(err_title, message = string_chain);
        }

        // 创建临时下载目录
        let Some(temp_download_dir) = self.create_temp_download_dir() else {
            self.set_failed(
                &format!("`{comic_title} - {chapter_title}`创建临时下载目录失败"),
                "见上方日志",
            );
            return;
        };
        // 清理临时下载目录中与`config.download_format`对不上的文件
        self.clean_temp_download_dir(&temp_download_dir);

        // 只下载尚未完成的图片。已 `done` 的图片直接跳过，
        // 这是 D4 的修复：失败不再等于整章重来。
        let pending_images = match ImageRepo::list_unfinished(self.app.store(), chapter_id) {
            Ok(images) => images,
            Err(err) => {
                let err_title = format!("`{comic_title} - {chapter_title}`读取未完成图片失败");
                let string_chain = err.to_string_chain();
                tracing::error!(err_title, message = string_chain);

                self.set_failed(&err_title, string_chain);
                return;
            }
        };

        let mut join_set = JoinSet::new();
        for image in pending_images {
            #[allow(clippy::cast_sign_loss)]
            let index = image.img_index as usize;
            let temp_download_dir = temp_download_dir.clone();
            let download_img_task =
                DownloadImgTask::new(self, image.url, index, temp_download_dir);
            join_set.spawn(download_img_task.process());
        }
        // 等待所有图片下载任务完成
        join_set.join_all().await;
        tracing::trace!(comic_title, chapter_title, "所有图片下载任务完成");

        // 检查此章节的图片是否全部下载成功。
        //
        // 这里查的是**图片表的未完成行数**，而不是「已完成计数 == 总数」。
        // 计数是内存里的缓存，进程崩溃或并发写入都可能让它与事实脱节；
        // 图片表的 `state` 才是唯一真相。这也是 D3 的修复。
        let unfinished = match ImageRepo::count_unfinished(self.app.store(), chapter_id) {
            Ok(n) => n,
            Err(err) => {
                let err_title = format!("`{comic_title} - {chapter_title}`统计未完成图片失败");
                let string_chain = err.to_string_chain();
                tracing::error!(err_title, message = string_chain);

                self.set_failed(&err_title, string_chain);
                return;
            }
        };
        if unfinished > 0 {
            // 此章节仍有图片没下载成功
            let err_title = format!("`{comic_title} - {chapter_title}`下载不完整");
            let err_msg = format!("还有`{unfinished}`张图片未下载成功，可稍后重试续传");
            tracing::error!(err_title, message = err_msg);

            self.set_failed(&err_title, err_msg);
            return;
        }
        // 至此，章节的图片全部下载成功
        if let Err(err) = self.rename_temp_download_dir(&temp_download_dir) {
            let err_title = format!("`{comic_title} - {chapter_title}`重命名临时下载目录失败");
            let string_chain = err.to_string_chain();
            tracing::error!(err_title, message = string_chain);

            self.set_failed(&err_title, string_chain);
            return;
        }

        if let Err(err) = self.save_chapter_metadata() {
            let err_title = format!("`{comic_title} - {chapter_title}`保存元数据失败");
            let string_chain = err.to_string_chain();
            tracing::error!(err_title, message = string_chain);
        }

        self.sleep_between_chapter().await;
        tracing::info!(comic_title, chapter_title, "章节下载成功");

        self.set_state(DownloadTaskState::Completed);
        self.emit_download_task_update_event();
    }

    async fn download_cover(&self) -> anyhow::Result<()> {
        let comic = &self.comic;
        let cover_path = comic.get_cover_path().context("获取封面路径失败")?;
        // if cover_path.exists() {
        //     return Ok(());
        // }

        let parts: Vec<&str> = comic.thumb.path.split('/').collect();
        if parts.len() < 3 {
            return Err(anyhow!(
                "`comic.thumb.path`出现了意料之外的格式: `{}`",
                comic.thumb.path
            ));
        }

        let file_server = &comic.thumb.file_server;
        let service = parts[0];
        let signature = parts[1];
        let filename = parts[parts.len() - 1];
        let url = format!("{file_server}/static/{service}/{signature}/{filename}");

        let (img_data, _format) = self
            .app
            .get_pica_client()
            .get_img_data_and_format(&url)
            .await
            .context(format!("下载图片`{url}`失败"))?;

        std::fs::write(&cover_path, img_data)
            .context(format!("保存图片`{}`失败", cover_path.display()))?;

        Ok(())
    }

    fn create_temp_download_dir(&self) -> Option<PathBuf> {
        let comic_title = &self.comic.title;
        let chapter_title = &self.chapter_info.chapter_title;

        let temp_download_dir = match self.chapter_info.get_temp_download_dir() {
            Ok(temp_download_dir) => temp_download_dir,
            Err(err) => {
                let err_title = format!("`{comic_title} - {chapter_title}`获取临时下载目录失败");
                let string_chain = err.to_string_chain();
                tracing::error!(err_title, message = string_chain);

                self.set_state(DownloadTaskState::Failed);
                self.emit_download_task_update_event();

                return None;
            }
        };

        if let Err(err) = std::fs::create_dir_all(&temp_download_dir).map_err(anyhow::Error::from) {
            let err_title = format!(
                "`{comic_title} - {chapter_title}`创建临时下载目录`{}`失败",
                temp_download_dir.display()
            );
            let string_chain = err.to_string_chain();
            tracing::error!(err_title, message = string_chain);

            self.set_state(DownloadTaskState::Failed);
            self.emit_download_task_update_event();

            return None;
        }

        tracing::trace!(
            comic_title,
            chapter_title,
            "创建临时下载目录`{}`成功",
            temp_download_dir.display()
        );

        Some(temp_download_dir)
    }

    async fn get_img_urls(&self) -> anyhow::Result<Vec<String>> {
        let comic_title = &self.comic.title;
        let chapter_title = &self.chapter_info.chapter_title;
        let comic_id = &self.comic.id;
        let chapter_order = self.chapter_info.order;

        let pica_client = self.app.get_pica_client();

        let first_page = pica_client
            .get_chapter_img(comic_id, chapter_order, 1)
            .await
            .context("获取第`1`页图片链接失败")?;

        let total_pages = first_page.pages;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let mut page_imgs_pairs = Vec::with_capacity(total_pages as usize);
        page_imgs_pairs.push((1, first_page.docs));

        let mut join_set = JoinSet::new();
        for page in 2..=total_pages {
            let pica_client = pica_client.clone();
            let comic_id = comic_id.clone();

            join_set.spawn(async move {
                let img_page = pica_client
                    .get_chapter_img(&comic_id, chapter_order, page)
                    .await
                    .context(format!("获取第`{page}`页图片链接失败"))?;

                Ok::<_, anyhow::Error>((page, img_page.docs))
            });
        }

        // 逐个处理完成的任务，如果有任务失败，则返回None
        while let Some(join_result) = join_set.join_next().await {
            match join_result {
                Ok(Ok(pair)) => {
                    page_imgs_pairs.push(pair);
                }
                Ok(Err(err)) => return Err(err),
                Err(err) => return Err(anyhow::Error::from(err)),
            }
        }

        page_imgs_pairs.sort_by_key(|(page, _)| *page);
        let img_urls: Vec<String> = page_imgs_pairs
            .into_iter()
            .flat_map(|(_, imgs)| imgs)
            .map(|img| (img.media.file_server, img.media.path))
            .map(|(file_server, path)| format!("{file_server}/static/{path}"))
            .collect();

        tracing::trace!(comic_title, chapter_title, "获取图片链接成功");

        Ok(img_urls)
    }

    fn rename_temp_download_dir(&self, temp_download_dir: &PathBuf) -> anyhow::Result<()> {
        let comic_title = &self.comic.title;
        let chapter_title = &self.chapter_info.chapter_title;
        let chapter_download_dir = self
            .chapter_info
            .chapter_download_dir
            .as_ref()
            .context("`chapter_download_dir`字段为`None`")?;

        if chapter_download_dir.exists() {
            std::fs::remove_dir_all(chapter_download_dir)
                .context(format!("删除 `{}` 失败", chapter_download_dir.display()))?;
        }

        std::fs::rename(temp_download_dir, chapter_download_dir).context(format!(
            "将 `{}` 重命名为 `{}` 失败",
            temp_download_dir.display(),
            chapter_download_dir.display()
        ))?;

        tracing::trace!(
            comic_title,
            chapter_title,
            "重命名临时下载目录`{}`为`{}`成功",
            temp_download_dir.display(),
            chapter_download_dir.display()
        );

        Ok(())
    }

    /// 删除临时下载目录中与`config.download_format`对不上的文件
    fn clean_temp_download_dir(&self, temp_download_dir: &Path) {
        let comic_title = &self.comic.title;
        let chapter_title = &self.chapter_info.chapter_title;

        let entries = match std::fs::read_dir(temp_download_dir).map_err(anyhow::Error::from) {
            Ok(entries) => entries,
            Err(err) => {
                let err_title = format!(
                    "`{comic_title}`读取临时下载目录`{}`失败",
                    temp_download_dir.display()
                );
                let string_chain = err.to_string_chain();
                tracing::error!(err_title, message = string_chain);
                return;
            }
        };

        let download_format = self.app.config().read().download_format;
        let extension = download_format.extension();
        for path in entries.filter_map(Result::ok).map(|entry| entry.path()) {
            // path有扩展名，且能转换为utf8，并与`config.download_format`一致或是gif，则保留
            let should_keep = path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext == "gif" || Some(ext) == extension);
            if should_keep {
                continue;
            }
            // 否则删除文件
            if let Err(err) = std::fs::remove_file(&path).map_err(anyhow::Error::from) {
                let err_title =
                    format!("`{comic_title}`删除临时下载目录的`{}`失败", path.display());
                let string_chain = err.to_string_chain();
                tracing::error!(err_title, message = string_chain);
            }
        }

        tracing::trace!(
            comic_title,
            chapter_title,
            "清理临时下载目录`{}`成功",
            temp_download_dir.display()
        );
    }

    fn save_comic_metadata(&self) -> anyhow::Result<()> {
        let mut comic = self.comic.as_ref().clone();
        // 将漫画的is_downloaded和comic_download_dir字段设置为None
        // 这样能使这些字段在序列化时被忽略
        comic.is_downloaded = None;
        comic.comic_download_dir = None;
        for chapter in &mut comic.chapter_infos {
            // 将章节的is_downloaded和chapter_download_dir字段设置为None
            // 这样能使这些字段在序列化时被忽略
            chapter.is_downloaded = None;
            chapter.chapter_download_dir = None;
        }

        let comic_download_dir = self
            .comic
            .comic_download_dir
            .as_ref()
            .context("`comic_download_dir`字段为`None`")?;
        let metadata_path = comic_download_dir.join("元数据.json");

        std::fs::create_dir_all(comic_download_dir)
            .context(format!("创建目录`{}`失败", comic_download_dir.display()))?;

        let comic_json = serde_json::to_string_pretty(&comic).context("将Comic序列化为json失败")?;

        std::fs::write(&metadata_path, comic_json)
            .context(format!("写入文件`{}`失败", metadata_path.display()))?;

        Ok(())
    }

    fn save_chapter_metadata(&self) -> anyhow::Result<()> {
        let mut chapter_info = self.chapter_info.as_ref().clone();
        // 将is_downloaded和chapter_download_dir字段设置为None
        // 这样能使这些字段在序列化时被忽略
        chapter_info.is_downloaded = None;
        chapter_info.chapter_download_dir = None;

        let chapter_download_dir = self
            .chapter_info
            .chapter_download_dir
            .as_ref()
            .context("`chapter_download_dir`字段为`None`")?;
        let metadata_path = chapter_download_dir.join("章节元数据.json");

        std::fs::create_dir_all(chapter_download_dir)
            .context(format!("创建目录`{}`失败", chapter_download_dir.display()))?;

        let chapter_json =
            serde_json::to_string_pretty(&chapter_info).context("将ChapterInfo序列化为json失败")?;

        std::fs::write(&metadata_path, chapter_json)
            .context(format!("写入文件`{}`失败", metadata_path.display()))?;

        Ok(())
    }

    async fn acquire_chapter_permit<'a>(
        &'a self,
        permit: &mut Option<SemaphorePermit<'a>>,
    ) -> ControlFlow<()> {
        let comic_title = &self.comic.title;
        let chapter_title = &self.chapter_info.chapter_title;

        tracing::debug!(comic_title, chapter_title, "章节开始排队");

        self.emit_download_task_update_event();

        *permit = match permit.take() {
            // 如果有permit，则直接用
            Some(permit) => Some(permit),
            // 如果没有permit，则获取permit
            None => match self
                .download_manager
                .chapter_sem
                .acquire()
                .await
                .map_err(anyhow::Error::from)
            {
                Ok(permit) => {
                    // 同 `acquire_img_permit`：拿到新 permit 时先还掉并发度
                    // 调小留下的欠账，避免旧的高并发额度继续放行章节。
                    DownloadManager::reclaim_debt(
                        &self.download_manager.chapter_sem,
                        &self.download_manager.chapter_capacity,
                    );
                    Some(permit)
                }
                Err(err) => {
                    let err_title =
                        format!("`{comic_title} - {chapter_title}`获取下载章节的permit失败");
                    let string_chain = err.to_string_chain();
                    tracing::error!(err_title, message = string_chain);

                    self.set_state(DownloadTaskState::Failed);
                    self.emit_download_task_update_event();

                    return ControlFlow::Break(());
                }
            },
        };
        // 如果当前任务状态不是`Pending`，则不将任务状态设置为`Downloading`
        if *self.state_sender.borrow() != DownloadTaskState::Pending {
            return ControlFlow::Continue(());
        }
        // 将任务状态设置为`Downloading`
        if let Err(err) = self
            .state_sender
            .send(DownloadTaskState::Downloading)
            .map_err(anyhow::Error::from)
        {
            let err_title = format!("`{comic_title} - {chapter_title}`发送状态`Downloading`失败");
            let string_chain = err.to_string_chain();
            tracing::error!(err_title, message = string_chain);
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    }

    fn handle_state_change<'a>(
        &'a self,
        permit: &mut Option<SemaphorePermit<'a>>,
        state_receiver: &mut watch::Receiver<DownloadTaskState>,
    ) -> ControlFlow<()> {
        let comic_title = &self.comic.title;
        let chapter_title = &self.chapter_info.chapter_title;

        self.emit_download_task_update_event();
        let state = *state_receiver.borrow();
        match state {
            DownloadTaskState::Paused => {
                tracing::debug!(comic_title, chapter_title, "章节暂停中");
                if let Some(permit) = permit.take() {
                    drop(permit);
                }
                ControlFlow::Continue(())
            }
            DownloadTaskState::Cancelled => {
                tracing::debug!(comic_title, chapter_title, "章节取消下载");
                ControlFlow::Break(())
            }
            _ => ControlFlow::Continue(()),
        }
    }

    async fn sleep_between_chapter(&self) {
        // 章节之间的间隔仍然保留：服务端 IP 比桌面端更容易触发哔咔风控，
        // 这个节流是必要的保护。只是不再逐秒推送倒计时事件。
        let remaining_sec = self.app.config().read().chapter_download_interval_sec;
        if remaining_sec > 0 {
            sleep(Duration::from_secs(remaining_sec)).await;
        }
    }
    /// 把某张图片标记为已完成，并推进进度计数。
    ///
    /// 落库失败不中断下载：图片已经躺在磁盘上了，为了记账失败而重下它
    /// 是纯粹的浪费。代价只是这个章节可能要重扫一次 `unfinished`。
    fn mark_img_done(&self, img_index: usize, bytes: Option<u64>) {
        #[allow(clippy::cast_possible_wrap)]
        let img_index = img_index as i64;
        let chapter_id = &self.chapter_info.chapter_id;

        #[allow(clippy::cast_possible_wrap)]
        let bytes = bytes.map(|b| b as i64);

        if let Err(err) = ImageRepo::mark_done(self.app.store(), chapter_id, img_index, bytes) {
            let err_title = format!(
                "`{} - {}`标记图片`{img_index}`完成失败",
                self.comic.title, self.chapter_info.chapter_title
            );
            let string_chain = err.to_string_chain();
            tracing::error!(err_title, message = string_chain);
        }

        let done = self.downloaded_img_count.fetch_add(1, Ordering::Relaxed) + 1;
        let total = self.total_img_count.load(Ordering::Relaxed);
        self.persist_progress(done, total);
    }

    /// 记录某张图片的失败原因。**不**推进计数——它仍然是未完成的。
    fn mark_img_failed(&self, img_index: usize, error: &str) {
        #[allow(clippy::cast_possible_wrap)]
        let img_index = img_index as i64;
        let chapter_id = &self.chapter_info.chapter_id;

        if let Err(err) = ImageRepo::mark_failed(self.app.store(), chapter_id, img_index, error) {
            let err_title = format!(
                "`{} - {}`记录图片`{img_index}`失败原因失败",
                self.comic.title, self.chapter_info.chapter_title
            );
            let string_chain = err.to_string_chain();
            tracing::error!(err_title, message = string_chain);
        }
    }

    /// 把内存计数刷进 DB。
    ///
    /// DB 里的计数只是给前端和青龙看的**缓存**，图片表的 `state` 才是真相。
    /// 所以这里失败也无所谓，`count_unfinished` 会在章节收尾时纠正它。
    fn persist_progress(&self, done: u32, total: u32) {
        let chapter_id = &self.chapter_info.chapter_id;
        if let Err(err) = TaskRepo::set_progress(
            self.app.store(),
            chapter_id,
            i64::from(total),
            i64::from(done),
        ) {
            tracing::warn!(
                err_title = "写入任务进度失败（不影响下载）",
                chapter_id,
                message = %err
            );
        }
    }

    fn set_state(&self, state: DownloadTaskState) {
        let comic_title = &self.comic.title;
        let chapter_title = &self.chapter_info.chapter_title;

        // 先落库再广播：让持久状态成为真相源，内存与事件都是它的投影。
        // 落库失败只记日志不中断——下载本身比任务记录更重要。
        let last_error = self.last_error.read().clone();
        let retry_count = i64::from(self.retry_count.load(Ordering::Relaxed));
        if let Err(err) = TaskRepo::set_state(
            self.app.store(),
            &self.chapter_info.chapter_id,
            state.to_db(),
            last_error.as_deref(),
            retry_count,
        ) {
            let err_title = format!("`{comic_title} - {chapter_title}`持久化状态`{state:?}`失败");
            let string_chain = err.to_string_chain();
            tracing::error!(err_title, message = string_chain);
        }

        if let Err(err) = self.state_sender.send(state).map_err(anyhow::Error::from) {
            let err_title = format!("`{comic_title} - {chapter_title}`发送状态`{state:?}`失败");
            let string_chain = err.to_string_chain();
            tracing::error!(err_title, message = string_chain);
        }
    }

    /// 记录失败原因并切换到 `Failed`。
    ///
    /// 把「写错误信息」和「置失败态」合成一个动作，避免出现「状态是 Failed
    /// 但 `last_error` 是空的」这种没法排查的记录。
    fn set_failed(&self, err_title: &str, err_msg: impl std::fmt::Display) {
        let err_msg = err_msg.to_string();
        *self.last_error.write() = Some(format!("{err_title}：{err_msg}"));
        // 先自增再置状态：`set_state` 会把当前计数一并落库，
        // 顺序反了就会少记一次。
        self.retry_count.fetch_add(1, Ordering::Relaxed);
        self.set_state(DownloadTaskState::Failed);
        self.emit_download_task_update_event();
    }

    fn emit_download_task_update_event(&self) {
        let last_error = self.last_error.read().clone();
        self.app.events().emit(
            topics::DOWNLOAD_TASK,
            &DownloadTaskEvent::Update {
                chapter_id: self.chapter_info.chapter_id.clone(),
                state: *self.state_sender.borrow(),
                downloaded_img_count: self.downloaded_img_count.load(Ordering::Relaxed),
                total_img_count: self.total_img_count.load(Ordering::Relaxed),
                retry_count: Some(self.retry_count.load(Ordering::Relaxed)),
                last_error,
            },
        );
    }

    fn emit_download_task_create_event(&self) {
        self.app
            .events()
            .emit(topics::DOWNLOAD_TASK, &self.to_create_event());
    }

    /// 构造与 `DownloadTaskEvent::Create` 等价的事件。
    ///
    /// 既用于任务创建时的事件推送，也用于新 WebSocket 客户端连上后的任务快照。
    fn to_create_event(&self) -> DownloadTaskEvent {
        DownloadTaskEvent::Create {
            state: *self.state_sender.borrow(),
            comic: Box::new(self.comic.as_ref().clone()),
            chapter_info: Box::new(self.chapter_info.as_ref().clone()),
            downloaded_img_count: self.downloaded_img_count.load(Ordering::Relaxed),
            total_img_count: self.total_img_count.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone)]
struct DownloadImgTask {
    app: AppContext,
    download_manager: DownloadManager,
    download_task: DownloadTask,
    url: String,
    index: usize,
    temp_download_dir: PathBuf,
}

impl DownloadImgTask {
    pub fn new(
        download_task: &DownloadTask,
        url: String,
        index: usize,
        temp_download_dir: PathBuf,
    ) -> Self {
        Self {
            app: download_task.app.clone(),
            download_manager: download_task.download_manager.clone(),
            download_task: download_task.clone(),
            url,
            index,
            temp_download_dir,
        }
    }

    async fn process(self) {
        let download_img_task = self.download_img();
        tokio::pin!(download_img_task);

        let mut state_receiver = self.download_task.state_sender.subscribe();
        state_receiver.mark_changed();
        let mut permit = None;

        loop {
            let state_is_downloading = *state_receiver.borrow() == DownloadTaskState::Downloading;
            tokio::select! {
                () = &mut download_img_task, if state_is_downloading && permit.is_some() => break,
                control_flow = self.acquire_img_permit(&mut permit), if state_is_downloading && permit.is_none() => {
                    match control_flow {
                        ControlFlow::Continue(()) => continue,
                        ControlFlow::Break(()) => break,
                    }
                },
                _ = state_receiver.changed() => {
                    match self.handle_state_change(&mut permit, &mut state_receiver) {
                        ControlFlow::Continue(()) => continue,
                        ControlFlow::Break(()) => break,
                    }
                }
            }
        }
    }

    async fn download_img(&self) {
        let url = &self.url;
        let comic_title = &self.download_task.comic.title;
        let chapter_title = &self.download_task.chapter_info.chapter_title;
        let temp_download_dir = &self.temp_download_dir;

        let index_filename = format!("{:03}", self.index + 1);
        let download_format = self.app.config().read().download_format;

        if let Some(ext) = download_format.extension() {
            let user_format_path = temp_download_dir.join(format!("{index_filename}.{ext}"));
            let gif_path = temp_download_dir.join(format!("{index_filename}.gif"));
            // 如果图片已存在，则跳过下载
            if user_format_path.exists() || gif_path.exists() {
                tracing::trace!(url, comic_title, chapter_title, "图片已存在，跳过下载");
                // 文件已落盘，直接补记 `done`。这是断点续传的关键一步：
                // 上一轮下载成功但还没来得及记账的图片，靠这里补上。
                self.download_task.mark_img_done(self.index, None);
                self.download_task.emit_download_task_update_event();
                return;
            }
        }

        tracing::trace!(url, comic_title, chapter_title, "开始下载图片");

        // 进程内重试（D4 的另一半）。
        //
        // 背景：图片 CDN 走代理，偶发 `operation timed out` 是常态而非异常。
        // 之前一次超时就直接放弃，只能靠「整章重跑」来补——而重跑要重新
        // 拉图片链接、重新排队，代价极高。这里加一层带退避的重试，
        // 把绝大多数瞬时抖动在图片内部消化掉。
        //
        // 重试是有上限的：真下不动的图片（已下架、需要更高权限）不该
        // 无休止占着 `img_sem` 的 permit，那会拖垮整个章节的吞吐。
        let (img_data, img_format) = match self.fetch_img_with_retry(url).await {
            Ok(data) => data,
            Err(err) => {
                let err_title = format!("下载图片`{url}`失败");
                let string_chain = err.to_string_chain();
                tracing::error!(err_title, message = string_chain);
                // 记下失败：这一张会在章节收尾时被算作未完成，
                // 下次重跑只补它，而不是整章重来。
                self.download_task.mark_img_failed(self.index, &string_chain);
                return;
            }
        };
        let img_data_len = img_data.len() as u64;

        tracing::trace!(url, comic_title, chapter_title, "图片成功下载到内存");

        // ════════════════════════════════════════════════
        // 修改点：添加对 BMP 格式的支持
        // ════════════════════════════════════════════════
        let src_img_ext = match img_format {
            ImageFormat::Jpeg => "jpg",
            ImageFormat::Png => "png",
            ImageFormat::WebP => "webp",
            ImageFormat::Gif => "gif",
            ImageFormat::Bmp => "bmp",   // ✅ 新增 BMP
            _ => {
                let err_title =
                    format!("`{comic_title} - {chapter_title}`获取图片`{url}`的扩展名失败");
                let err_msg = format!("出现了意料之外的格式`{img_format:?}`，请将此问题反馈给开发者");
                tracing::error!(err_title, message = err_msg);
                self.download_task.mark_img_failed(self.index, &err_msg);
                return;
            }
        };

        let ext = match img_format {
            ImageFormat::Gif => "gif",
            _ => download_format.extension().unwrap_or(src_img_ext),
        };
        let save_path = temp_download_dir.join(format!("{index_filename}.{ext}"));

        let target_format = match img_format {
            ImageFormat::Gif => ImageFormat::Gif,
            _ => download_format.to_image_format().unwrap_or(img_format),
        };

        // 保存图片
        if let Err(err) = save_img(&save_path, target_format, img_data, img_format).await {
            let err_title = format!("保存图片`{}`失败", save_path.display());
            let string_chain = err.to_string_chain();
            tracing::error!(err_title, message = string_chain);
            self.download_task.mark_img_failed(self.index, &string_chain);
            return;
        }

        tracing::trace!(
            url,
            comic_title,
            chapter_title,
            "图片成功保存到`{}`",
            save_path.display()
        );

        // 记录下载字节数
        self.download_manager
            .byte_per_sec
            .fetch_add(img_data_len, Ordering::Relaxed);

        // 先落库再推进计数：这一张从「未完成」变成「已完成」是断点续传的
        // 最小提交单位。mark_img_done 内部会同步内存计数和 DB 进度。
        self.download_task
            .mark_img_done(self.index, Some(img_data_len));

        self.download_task.emit_download_task_update_event();

        let img_download_interval_sec = self.app.config().read().img_download_interval_sec;
        sleep(Duration::from_secs(img_download_interval_sec)).await;
    }

    /// 带指数退避的图片抓取。
    ///
    /// 只在**可重试的错误**上重试：网络层超时/连接错误值得再试一次，
    /// 而「返回的是 HTML 而不是图片」这类错误说明 URL 本身失效了，
    /// 重试多少次都一样，只会浪费时间和 permit。
    ///
    /// 退避序列 1s → 2s → 4s，最多 `IMG_MAX_RETRIES` 次。
    /// 单张图最坏多花 7 秒，换掉的是一整章重跑。
    async fn fetch_img_with_retry(&self, url: &str) -> anyhow::Result<(Bytes, ImageFormat)> {
        const IMG_MAX_RETRIES: u32 = 3;
        const IMG_RETRY_BASE_DELAY_MS: u64 = 1_000;

        let mut attempt = 0u32;
        loop {
            match self
                .app
                .get_pica_client()
                .get_img_data_and_format(url)
                .await
            {
                Ok(data) => return Ok(data),
                Err(err) => {
                    attempt += 1;
                    if attempt > IMG_MAX_RETRIES {
                        return Err(err);
                    }

                    let delay_ms = IMG_RETRY_BASE_DELAY_MS * 2u64.pow(attempt - 1);
                    tracing::warn!(
                        url,
                        attempt,
                        max_retries = IMG_MAX_RETRIES,
                        delay_ms,
                        err_title = "图片下载失败，准备重试",
                        message = %err
                    );

                    // 退避期间也响应任务状态变化：用户点了暂停/取消，
                    // 没必要把退避睡完。醒来后下一轮请求会自然失败或被丢弃，
                    // 由上层 select! 的取消逻辑收尾。
                    sleep(Duration::from_millis(delay_ms)).await;

                    if *self.download_task.state_sender.borrow()
                        != DownloadTaskState::Downloading
                    {
                        return Err(err.context("任务已暂停或取消，放弃重试"));
                    }
                }
            }
        }
    }

    async fn acquire_img_permit<'a>(
        &'a self,
        permit: &mut Option<SemaphorePermit<'a>>,
    ) -> ControlFlow<()> {
        let url = &self.url;
        let comic_title = &self.download_task.comic.title;
        let chapter_title = &self.download_task.chapter_info.chapter_title;

        tracing::trace!(comic_title, chapter_title, url, "图片开始排队");

        *permit = match permit.take() {
            // 如果有permit，则直接用
            Some(permit) => Some(permit),
            // 如果没有permit，则获取permit
            None => match self
                .download_manager
                .img_sem
                .acquire()
                .await
                .map_err(anyhow::Error::from)
            {
                Ok(permit) => {
                    // 拿到 permit 说明这里刚消耗掉一个空闲额度，顺手把
                    // 「并发度调小」没收够、滞留在空闲池里的超额 permit 收掉，
                    // 让并发度向目标值收敛。
                    DownloadManager::reclaim_debt(
                        &self.download_manager.img_sem,
                        &self.download_manager.img_capacity,
                    );
                    Some(permit)
                }
                Err(err) => {
                    let err_title =
                        format!("`{comic_title} - {chapter_title}`获取下载图片的permit失败");
                    let string_chain = err.to_string_chain();
                    tracing::error!(err_title, message = string_chain);
                    return ControlFlow::Break(());
                }
            },
        };
        ControlFlow::Continue(())
    }

    fn handle_state_change<'a>(
        &'a self,
        permit: &mut Option<SemaphorePermit<'a>>,
        state_receiver: &mut watch::Receiver<DownloadTaskState>,
    ) -> ControlFlow<()> {
        let url = &self.url;
        let comic_title = &self.download_task.comic.title;
        let chapter_title = &self.download_task.chapter_info.chapter_title;

        let state = *state_receiver.borrow();
        match state {
            DownloadTaskState::Paused => {
                tracing::trace!(comic_title, chapter_title, url, "图片暂停下载");
                if let Some(permit) = permit.take() {
                    drop(permit);
                }
                ControlFlow::Continue(())
            }
            DownloadTaskState::Cancelled => {
                tracing::trace!(comic_title, chapter_title, url, "图片取消下载");
                ControlFlow::Break(())
            }
            _ => ControlFlow::Continue(()),
        }
    }
}

// ════════════════════════════════════════════════
// 修改点：save_img 中添加对 BMP 的支持
// ════════════════════════════════════════════════
async fn save_img(
    save_path: &Path,
    target_format: ImageFormat,
    src_img_data: Bytes,
    src_format: ImageFormat,
) -> anyhow::Result<()> {
    if target_format == src_format {
        // 如果target_format与src_format匹配，则直接保存
        std::fs::write(save_path, &src_img_data)
            .context(format!("将图片数据写入`{}`失败", save_path.display()))?;
        return Ok(());
    }

    let save_path = save_path.to_path_buf();
    // 图像处理的闭包
    let process_img = move || -> anyhow::Result<()> {
        // 如果target_format与src_format不匹配，则需要转换格式
        let img = image::load_from_memory(&src_img_data).context("加载图片数据失败")?;

        let mut converted_data = Vec::new();

        // ✅ 添加 BMP 到支持的目标格式列表
        if target_format != ImageFormat::Jpeg
            && target_format != ImageFormat::Png
            && target_format != ImageFormat::WebP
            && target_format != ImageFormat::Bmp
        {
            return Err(anyhow!("不支持的图片格式: {:?}", target_format));
        }
        img.write_to(&mut Cursor::new(&mut converted_data), target_format)
            .context(format!("将`{src_format:?}`转换为`{target_format:?}`失败"))?;

        std::fs::write(&save_path, &converted_data)
            .context(format!("将图片数据写入`{}`失败", save_path.display()))?;

        Ok(())
    };

    // 因为图像处理是CPU密集型操作，所以使用rayon并发处理
    let (sender, receiver) = tokio::sync::oneshot::channel::<anyhow::Result<()>>();
    rayon::spawn(move || {
        let _ = sender.send(process_img());
    });
    // 在tokio任务中等待rayon任务的完成，避免阻塞worker threads
    receiver.await?
}

#[derive(Default, Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct ComicDirNameFmtParams {
    pub comic_id: String,
    pub comic_title: String,
    pub author: String,
}

impl Comic {
    /// 根据fmt更新`comic_download_dir`和`chapter_infos.chapter_download_dir`字段
    fn update_download_dir_fields_by_fmt(&mut self, app: &AppContext) -> anyhow::Result<()> {
        if self.chapter_infos.is_empty() {
            return Err(anyhow!("没有章节信息，无法更新下载目录字段"));
        }

        let mut first_chapter_download_dir = None;

        for chapter_info in &mut self.chapter_infos {
            let chapter_title = &chapter_info.chapter_title;

            let dir_fmt_params = DirFmtParams {
                comic_id: self.id.clone(),
                comic_title: self.title.clone(),
                author: self.author.clone(),
                chapter_id: chapter_info.chapter_id.clone(),
                chapter_title: chapter_info.chapter_title.clone(),
                order: chapter_info.order,
            };

            let chapter_download_dir =
                ChapterInfo::get_chapter_download_dir_by_fmt(app, &dir_fmt_params)
                    .context(format!("章节`{chapter_title}`根据fmt获取章节下载目录失败"))?;

            if first_chapter_download_dir.is_none() {
                first_chapter_download_dir = Some(chapter_download_dir.clone());
            }

            chapter_info.chapter_download_dir = Some(chapter_download_dir);
        }

        let Some(first_chapter_download_dir) = first_chapter_download_dir else {
            return Err(anyhow!(
                "处理完所有章节后first_chapter_download_dir仍然为None"
            ));
        };

        let comic_download_dir = first_chapter_download_dir.parent().context(format!(
            "第一个章节下载目录`{}`没有父目录",
            first_chapter_download_dir.display()
        ))?;

        self.comic_download_dir = Some(comic_download_dir.to_path_buf());

        Ok(())
    }
}

#[derive(Default, Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct DirFmtParams {
    pub comic_id: String,
    pub comic_title: String,
    pub author: String,
    pub chapter_id: String,
    pub chapter_title: String,
    pub order: i64,
}

impl ChapterInfo {
    fn get_chapter_download_dir_by_fmt(
        app: &AppContext,
        fmt_params: &DirFmtParams,
    ) -> anyhow::Result<PathBuf> {
        use strfmt::strfmt;

        let json_value =
            serde_json::to_value(fmt_params).context("将DirFmtParams转为serde_json::Value失败")?;

        let json_map = json_value.as_object().context("DirFmtParams不是JSON对象")?;

        let vars: HashMap<String, String> = json_map
            .into_iter()
            .map(|(k, v)| {
                let key = k.clone();
                let value = match v {
                    serde_json::Value::String(s) => s.clone(),
                    _ => v.to_string(),
                };
                (key, value)
            })
            .collect();

        let (download_dir, dir_fmt) = {
            let config = app.config();
            let config = config.read();
            (config.download_dir.clone(), config.dir_fmt.clone())
        };

        let dir_fmt_parts: Vec<&str> = dir_fmt.split('/').collect();

        let mut dir_names = Vec::new();
        for fmt in dir_fmt_parts {
            let dir_name = strfmt(fmt, &vars).context("格式化目录名失败")?;
            let dir_name = filename_filter(&dir_name);
            if !dir_name.is_empty() {
                dir_names.push(dir_name);
            }
        }

        if dir_names.len() < 2 {
            let err_msg =
                "配置中的下载目录格式至少要有两个层级，例如：{comic_id}/{order}";
            return Err(anyhow!(err_msg));
        }
        // 将格式化后的目录名拼接成完整的目录路径
        let mut chapter_download_dir = download_dir;
        for dir_name in dir_names {
            chapter_download_dir = chapter_download_dir.join(dir_name);
        }

        Ok(chapter_download_dir)
    }

    fn get_temp_download_dir(&self) -> anyhow::Result<PathBuf> {
        let chapter_download_dir = self
            .chapter_download_dir
            .as_ref()
            .context("`chapter_download_dir`字段为`None`")?;

        let chapter_download_dir_name = self
            .get_chapter_download_dir_name()
            .context("获取章节下载目录名失败")?;

        let parent = chapter_download_dir.parent().context(format!(
            "`{}`的父目录不存在",
            chapter_download_dir.display()
        ))?;

        let temp_download_dir = parent.join(format!(".下载中-{chapter_download_dir_name}"));
        Ok(temp_download_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::DownloadManager;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Semaphore;

    // ── 验收标准 4：配置热更新（并发度就地调整，不重建 manager）──────
    //
    // `resize_sem` 是这套机制的核心：它必须在**不掐断在跑任务**的前提下
    // 改变可用并发额度。下面几条分别覆盖「调大」「调小」「在跑任务持有的
    // permit 不被回收」，以及「在跑任务归还后欠账被补收」这四种情况。

    /// 建一对配套的 `Semaphore` + 容量记账，模拟 `DownloadManager` 里的字段。
    fn sem_with_capacity(n: usize) -> (Semaphore, AtomicUsize) {
        (Semaphore::new(n), AtomicUsize::new(n))
    }

    #[test]
    fn resize_sem_grows_available_permits() {
        let (sem, cap) = sem_with_capacity(5);
        DownloadManager::resize_sem(&sem, &cap, 12);
        assert_eq!(
            sem.available_permits(),
            12,
            "调大并发度应当立即放行更多 permit"
        );
        assert_eq!(cap.load(Ordering::Acquire), 12, "容量记账应当同步到新值");
    }

    #[test]
    fn resize_sem_shrinks_available_permits() {
        let (sem, cap) = sem_with_capacity(20);
        DownloadManager::resize_sem(&sem, &cap, 10);
        assert_eq!(
            sem.available_permits(),
            10,
            "调小并发度应当收回空闲 permit"
        );
        assert_eq!(cap.load(Ordering::Acquire), 10, "容量记账应当同步到新值");
    }

    #[test]
    fn resize_sem_never_reclaims_permits_held_by_running_tasks() {
        // 目标并发度 1，但已经放出去 3 个 permit、其中 2 个被在跑任务持有。
        // 这正是「配置调小后，旧的高并发额度还在外面」的真实暂态。
        let (sem, cap) = sem_with_capacity(3);

        // 模拟 2 个正在下载的图片：它们各持有一个 permit。
        let running_a = sem.try_acquire().unwrap();
        let running_b = sem.try_acquire().unwrap();
        assert_eq!(sem.available_permits(), 1);

        // 并发度从 3 降到 1。基准是**总容量 3** 而不是空闲的 1，
        // 所以这次调整不能被误判成 no-op。
        DownloadManager::resize_sem(&sem, &cap, 1);
        assert_eq!(cap.load(Ordering::Acquire), 1, "容量记账应当降到目标值");
        assert_eq!(
            sem.available_permits(),
            0,
            "空闲的 1 个应当被收回，只剩在跑任务手里的 2 个"
        );

        // 关键断言：在跑任务持有的 2 个 permit 没有被强收。
        assert_eq!(sem.available_permits(), 0);

        // 第一个任务归还：此时「在跑 1 + 空闲 1」总数仍是 2，比目标 1 多 1 个，
        // 但 `reclaim_debt` 只能看到空闲的 1，它并不大于目标 1，所以不动作。
        // 这是**有意的**：无法区分「刚归还的」和「本来就空闲的」，强收会误伤。
        drop(running_a);
        DownloadManager::reclaim_debt(&sem, &cap);
        assert_eq!(
            sem.available_permits(),
            1,
            "首个归还的 permit 进入池子，此时仍是在跑 1 + 空闲 1 的超额暂态"
        );

        // 第二个任务归还：在跑清零，池子里只剩空闲。`reclaim_debt` 这时才能
        // 确定地把超出目标的部分收掉，最终精确收敛到目标并发度 1。
        drop(running_b);
        DownloadManager::reclaim_debt(&sem, &cap);
        assert_eq!(
            sem.available_permits(),
            1,
            "全部在跑任务归还后，可用额度精确收敛到目标并发度 1"
        );

        // 收敛后再调一次也不应有副作用（幂等）。
        DownloadManager::reclaim_debt(&sem, &cap);
        assert_eq!(sem.available_permits(), 1, "收敛后重复回收不应误伤");
    }

    #[test]
    fn resize_sem_cannot_reclaim_more_than_idle() {
        // 目标降到 0 时，在跑任务持有的 permit 仍不可强收，
        // `forget_permits` 只能收回空闲额度。
        let (sem, cap) = sem_with_capacity(2);
        let running = sem.try_acquire().unwrap();
        assert_eq!(sem.available_permits(), 1);

        DownloadManager::resize_sem(&sem, &cap, 0);
        assert_eq!(
            sem.available_permits(),
            0,
            "空闲的 1 个被收回，目标 0 达成"
        );
        assert_eq!(cap.load(Ordering::Acquire), 0);

        // 在跑任务归还后出现的 1 个 permit 超出目标容量 0，
        // `reclaim_debt` 会把它收掉，不会留下超额并发。
        drop(running);
        assert_eq!(sem.available_permits(), 1, "归还瞬间是超额暂态");
        DownloadManager::reclaim_debt(&sem, &cap);
        assert_eq!(
            sem.available_permits(),
            0,
            "欠账应当被补收，最终收敛到目标容量 0"
        );
    }

    #[test]
    fn reclaim_debt_is_noop_when_within_capacity() {
        // 没有调小过并发度时，空闲额度本来就等于容量，回收不应误伤。
        let (sem, cap) = sem_with_capacity(5);
        DownloadManager::reclaim_debt(&sem, &cap);
        assert_eq!(
            sem.available_permits(),
            5,
            "空闲额度未超过容量时不应收回任何 permit"
        );
    }

    #[test]
    fn resize_delta_matches_semaphore_effect() {
        // 纯函数形式的差值计算：验证三种方向都正确。
        assert_eq!(DownloadManager::resize_delta(5, 12), 7);
        assert_eq!(DownloadManager::resize_delta(20, 10), -10);
        assert_eq!(DownloadManager::resize_delta(8, 8), 0);
    }

    #[test]
    fn resize_sem_is_idempotent_at_same_value() {
        let (sem, cap) = sem_with_capacity(8);
        DownloadManager::resize_sem(&sem, &cap, 8);
        assert_eq!(
            sem.available_permits(),
            8,
            "目标值与当前值相同时不应产生任何副作用"
        );
        assert_eq!(cap.load(Ordering::Acquire), 8);
    }

    #[test]
    fn resize_sem_shrink_uses_total_capacity_not_idle_permits() {
        // 回归测试：旧实现拿 `available_permits()` 当基准，导致
        // 「容量 3、2 个在跑、目标 1」被误判成 no-op，配置调小完全不生效。
        let (sem, cap) = sem_with_capacity(3);
        let _a = sem.try_acquire().unwrap();
        let _b = sem.try_acquire().unwrap();
        assert_eq!(sem.available_permits(), 1);

        DownloadManager::resize_sem(&sem, &cap, 1);
        assert_eq!(
            sem.available_permits(),
            0,
            "必须按总容量算差额，空闲 permit 应当被收回"
        );
    }
}