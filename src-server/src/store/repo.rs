//! 查询与写入。调度层和 API 层都只通过这里碰数据库。
//!
//! 所有写操作都显式维护 `updated_at`，因为青龙侧要靠它做增量拉取。

use anyhow::Context;

use super::types::{
    now_ts, DbImage, DbImageState, DbTask, DbTaskState, Store,
};

/// 各状态的任务计数。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStats {
    pub pending: i64,
    pub downloading: i64,
    pub paused: i64,
    pub cancelled: i64,
    pub completed: i64,
    pub failed: i64,
    pub total: i64,
}

/// 章节级任务的读写。
pub struct TaskRepo;

impl TaskRepo {
    /// 写入一个新任务。已存在则**保留原状态**（幂等），避免重复提交把
    /// 一个正在下载的任务打回 `pending`。
    pub fn upsert_new(store: &Store, task: &DbTask) -> anyhow::Result<()> {
        store.with_conn(|conn| {
            conn.execute(
                r#"
                INSERT INTO download_task (
                    chapter_id, comic_id, comic_title, chapter_title, chapter_order,
                    state, total_img_count, done_img_count, retry_count, last_error,
                    dir_fmt, created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                ON CONFLICT(chapter_id) DO UPDATE SET
                    -- 只刷新展示用的元信息，状态与进度保持不动
                    comic_title   = excluded.comic_title,
                    chapter_title = excluded.chapter_title,
                    chapter_order = excluded.chapter_order,
                    dir_fmt       = excluded.dir_fmt,
                    updated_at    = excluded.updated_at
                "#,
                rusqlite::params![
                    task.chapter_id,
                    task.comic_id,
                    task.comic_title,
                    task.chapter_title,
                    task.chapter_order,
                    task.state.as_str(),
                    task.total_img_count,
                    task.done_img_count,
                    task.retry_count,
                    task.last_error,
                    task.dir_fmt,
                    task.created_at,
                    task.updated_at,
                ],
            )
            .context("写入 download_task 失败")?;
            Ok(())
        })
    }

    /// 迁移状态，同时写入 `last_error` 与章节级 `retry_count`。
    /// 返回是否确实发生了变更（用于避免无谓的事件推送）。
    ///
    /// 注意这里**没有** `AND state != ?2` 的短路：同一个章节第二次失败时
    /// 状态仍是 `Failed`，但 `last_error` 和 `retry_count` 都变了，必须落库，
    /// 否则「反复失败」的章节在 DB 里永远停在第一次的错误信息上。
    pub fn set_state(
        store: &Store,
        chapter_id: &str,
        state: DbTaskState,
        last_error: Option<&str>,
        retry_count: i64,
    ) -> anyhow::Result<bool> {
        store.with_conn(|conn| {
            let changed = conn
                .execute(
                    r#"
                    UPDATE download_task
                       SET state = ?2,
                           last_error = COALESCE(?3, last_error),
                           retry_count = ?4,
                           updated_at = ?5
                     WHERE chapter_id = ?1
                       AND (state != ?2 OR ?4 > retry_count
                            OR (?3 IS NOT NULL AND ?3 IS NOT last_error))
                    "#,
                    rusqlite::params![
                        chapter_id,
                        state.as_str(),
                        last_error,
                        retry_count,
                        now_ts()
                    ],
                )
                .context("更新任务状态失败")?;
            Ok(changed > 0)
        })
    }

    /// 只更新章节级 `retry_count`，不动状态。
    ///
    /// 用于「重启恢复」与「重试计数单独变化」的场景：这些情况下状态没有迁移，
    /// 但计数必须落库，否则重启后计数归零、退避策略失效。
    pub fn set_retry_count(
        store: &Store,
        chapter_id: &str,
        retry_count: i64,
    ) -> anyhow::Result<bool> {
        store.with_conn(|conn| {
            let changed = conn
                .execute(
                    r#"
                    UPDATE download_task
                       SET retry_count = ?2,
                           updated_at  = ?3
                     WHERE chapter_id = ?1
                    "#,
                    rusqlite::params![chapter_id, retry_count, now_ts()],
                )
                .context("更新重试计数失败")?;
            Ok(changed > 0)
        })
    }

    /// 更新进度计数。
    pub fn set_progress(
        store: &Store,
        chapter_id: &str,
        total: i64,
        done: i64,
    ) -> anyhow::Result<()> {
        store.with_conn(|conn| {
            conn.execute(
                r#"
                UPDATE download_task
                   SET total_img_count = ?2,
                       done_img_count  = ?3,
                       updated_at      = ?4
                 WHERE chapter_id = ?1
                "#,
                rusqlite::params![chapter_id, total, done, now_ts()],
            )
            .context("更新任务进度失败")?;
            Ok(())
        })
    }

    /// 按 `done_img_count` 重算并写回。
    ///
    /// 恢复流程用它把计数与图片表对齐——计数只是缓存，
    /// 图片表才是真相。
    pub fn resync_progress(store: &Store, chapter_id: &str) -> anyhow::Result<(i64, i64)> {
        store.with_conn(|conn| {
            let total: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM download_image WHERE chapter_id = ?1",
                    [chapter_id],
                    |row| row.get(0),
                )
                .context("统计图片总数失败")?;
            let done: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM download_image WHERE chapter_id = ?1 AND state = 'done'",
                    [chapter_id],
                    |row| row.get(0),
                )
                .context("统计已完成图片数失败")?;
            conn.execute(
                r#"
                UPDATE download_task
                   SET total_img_count = ?2,
                       done_img_count  = ?3,
                       updated_at      = ?4
                 WHERE chapter_id = ?1
                "#,
                rusqlite::params![chapter_id, total, done, now_ts()],
            )
            .context("回写任务进度失败")?;
            Ok((total, done))
        })
    }

    pub fn get(store: &Store, chapter_id: &str) -> anyhow::Result<Option<DbTask>> {
        store.with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    r#"
                    SELECT chapter_id, comic_id, comic_title, chapter_title, chapter_order,
                           state, total_img_count, done_img_count, retry_count, last_error,
                           dir_fmt, created_at, updated_at
                      FROM download_task
                     WHERE chapter_id = ?1
                    "#,
                )
                .context("准备查询任务失败")?;
            let mut rows = stmt.query([chapter_id]).context("查询任务失败")?;
            match rows.next().context("读取任务行失败")? {
                Some(row) => Ok(Some(DbTask::from_row(row).context("解析任务行失败")?)),
                None => Ok(None),
            }
        })
    }

    /// 列出任务。`state` 为 `None` 表示不过滤。
    ///
    /// 按 `updated_at DESC` 排序——前端最关心最近有动静的任务。
    pub fn list(
        store: &Store,
        state: Option<DbTaskState>,
        comic_id: Option<&str>,
        since: Option<i64>,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<DbTask>> {
        store.with_conn(|conn| {
            // 用 COALESCE 做可选过滤，省得拼 SQL 字符串。
            let mut stmt = conn
                .prepare(
                    r#"
                    SELECT chapter_id, comic_id, comic_title, chapter_title, chapter_order,
                           state, total_img_count, done_img_count, retry_count, last_error,
                           dir_fmt, created_at, updated_at
                      FROM download_task
                     WHERE (?1 IS NULL OR state = ?1)
                       AND (?2 IS NULL OR comic_id = ?2)
                       AND (?3 IS NULL OR updated_at >= ?3)
                     ORDER BY updated_at DESC
                     LIMIT ?4 OFFSET ?5
                    "#,
                )
                .context("准备查询任务列表失败")?;
            let rows = stmt
                .query_map(
                    rusqlite::params![
                        state.map(DbTaskState::as_str),
                        comic_id,
                        since,
                        limit,
                        offset
                    ],
                    DbTask::from_row,
                )
                .context("查询任务列表失败")?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.context("解析任务行失败")?);
            }
            Ok(out)
        })
    }

    /// 统计满足同样过滤条件的任务总数（分页用）。
    ///
    /// 过滤条件必须与 [`TaskRepo::list`] 逐字对齐，否则分页会出现
    /// 「总数 50 但翻到第 2 页就空了」这种自相矛盾的结果。
    pub fn count(
        store: &Store,
        state: Option<DbTaskState>,
        comic_id: Option<&str>,
        since: Option<i64>,
    ) -> anyhow::Result<i64> {
        store.with_conn(|conn| {
            let n = conn
                .query_row(
                    r#"
                    SELECT COUNT(*)
                      FROM download_task
                     WHERE (?1 IS NULL OR state = ?1)
                       AND (?2 IS NULL OR comic_id = ?2)
                       AND (?3 IS NULL OR updated_at >= ?3)
                    "#,
                    rusqlite::params![state.map(DbTaskState::as_str), comic_id, since],
                    |row| row.get::<_, i64>(0),
                )
                .context("统计任务列表失败")?;
            Ok(n)
        })
    }

    /// 所有未完结的任务。恢复流程用。
    pub fn list_unfinished(store: &Store) -> anyhow::Result<Vec<DbTask>> {
        store.with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    r#"
                    SELECT chapter_id, comic_id, comic_title, chapter_title, chapter_order,
                           state, total_img_count, done_img_count, retry_count, last_error,
                           dir_fmt, created_at, updated_at
                      FROM download_task
                     WHERE state IN ('pending', 'downloading', 'paused', 'failed')
                     ORDER BY created_at ASC
                    "#,
                )
                .context("准备查询未完结任务失败")?;
            let rows = stmt
                .query_map([], DbTask::from_row)
                .context("查询未完结任务失败")?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.context("解析任务行失败")?);
            }
            Ok(out)
        })
    }

    /// 统计各状态数量。
    pub fn stats(store: &Store) -> anyhow::Result<TaskStats> {
        store.with_conn(|conn| {
            let mut stmt = conn
                .prepare("SELECT state, COUNT(*) FROM download_task GROUP BY state")
                .context("准备统计任务失败")?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .context("统计任务失败")?;

            let mut stats = TaskStats::default();
            for row in rows {
                let (state, count) = row.context("读取统计行失败")?;
                match state.as_str() {
                    "pending" => stats.pending = count,
                    "downloading" => stats.downloading = count,
                    "paused" => stats.paused = count,
                    "cancelled" => stats.cancelled = count,
                    "completed" => stats.completed = count,
                    "failed" => stats.failed = count,
                    _ => {}
                }
                stats.total += count;
            }
            Ok(stats)
        })
    }

    /// 删除任务（图片行靠外键级联删除）。
    pub fn delete(store: &Store, chapter_id: &str) -> anyhow::Result<bool> {
        store.with_conn(|conn| {
            let n = conn
                .execute("DELETE FROM download_task WHERE chapter_id = ?1", [chapter_id])
                .context("删除任务失败")?;
            Ok(n > 0)
        })
    }

    /// 清理旧的终态任务，避免表无限增长（D7）。
    ///
    /// 只删 `completed` / `cancelled`，且早于 `before_ts` 的。
    /// `failed` 不删——用户可能还要手动重试。
    pub fn purge_terminal(store: &Store, before_ts: i64) -> anyhow::Result<usize> {
        store.with_conn(|conn| {
            let n = conn
                .execute(
                    r#"
                    DELETE FROM download_task
                     WHERE state IN ('completed', 'cancelled')
                       AND updated_at < ?1
                    "#,
                    [before_ts],
                )
                .context("清理历史任务失败")?;
            Ok(n)
        })
    }
}

/// 图片级任务的读写。
pub struct ImageRepo;

impl ImageRepo {
    /// 批量登记某章节的图片清单。
    ///
    /// 已存在的行**保留原状态**——这是断点续传的关键：重跑一个章节时，
    /// 已经 `done` 的图片不能被重置回 `pending`。
    pub fn register_batch(
        store: &Store,
        chapter_id: &str,
        urls: &[String],
    ) -> anyhow::Result<()> {
        let ts = now_ts();
        store.with_tx(|tx| {
            let mut stmt = tx
                .prepare(
                    r#"
                    INSERT INTO download_image (chapter_id, img_index, url, state, updated_at)
                    VALUES (?1, ?2, ?3, 'pending', ?4)
                    ON CONFLICT(chapter_id, img_index) DO UPDATE SET
                        -- URL 可能因为签名刷新而变，更新它；
                        -- 但 state 绝不覆盖，否则断点续传就失效了
                        url        = excluded.url,
                        updated_at = excluded.updated_at
                    "#,
                )
                .context("准备登记图片失败")?;

            for (i, url) in urls.iter().enumerate() {
                #[allow(clippy::cast_possible_wrap)]
                let idx = i as i64;
                stmt.execute(rusqlite::params![chapter_id, idx, url, ts])
                    .context("登记图片失败")?;
            }
            Ok(())
        })
    }

    pub fn mark_done(
        store: &Store,
        chapter_id: &str,
        img_index: i64,
        bytes: Option<i64>,
    ) -> anyhow::Result<()> {
        store.with_conn(|conn| {
            conn.execute(
                r#"
                UPDATE download_image
                   SET state = 'done',
                       bytes = ?3,
                       last_error = NULL,
                       updated_at = ?4
                 WHERE chapter_id = ?1 AND img_index = ?2
                "#,
                rusqlite::params![chapter_id, img_index, bytes, now_ts()],
            )
            .context("标记图片完成失败")?;
            Ok(())
        })
    }

    /// 记录一次失败。`retry_count` 自增。
    pub fn mark_failed(
        store: &Store,
        chapter_id: &str,
        img_index: i64,
        error: &str,
    ) -> anyhow::Result<i64> {
        store.with_conn(|conn| {
            conn.execute(
                r#"
                UPDATE download_image
                   SET state = 'failed',
                       retry_count = retry_count + 1,
                       last_error = ?3,
                       updated_at = ?4
                 WHERE chapter_id = ?1 AND img_index = ?2
                "#,
                rusqlite::params![chapter_id, img_index, error, now_ts()],
            )
            .context("标记图片失败失败")?;

            let retry_count: i64 = conn
                .query_row(
                    "SELECT retry_count FROM download_image WHERE chapter_id = ?1 AND img_index = ?2",
                    rusqlite::params![chapter_id, img_index],
                    |row| row.get(0),
                )
                .context("读取重试次数失败")?;
            Ok(retry_count)
        })
    }

    /// 把某章节所有非 `done` 的图片重置为 `pending`，供手动重试使用。
    pub fn reset_failed(store: &Store, chapter_id: &str) -> anyhow::Result<usize> {
        store.with_conn(|conn| {
            let n = conn
                .execute(
                    r#"
                    UPDATE download_image
                       SET state = 'pending',
                           last_error = NULL,
                           updated_at = ?2
                     WHERE chapter_id = ?1 AND state != 'done'
                    "#,
                    rusqlite::params![chapter_id, now_ts()],
                )
                .context("重置失败图片失败")?;
            Ok(n)
        })
    }

    pub fn list_by_chapter(store: &Store, chapter_id: &str) -> anyhow::Result<Vec<DbImage>> {
        store.with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    r#"
                    SELECT chapter_id, img_index, url, state, retry_count, last_error,
                           bytes, updated_at
                      FROM download_image
                     WHERE chapter_id = ?1
                     ORDER BY img_index ASC
                    "#,
                )
                .context("准备查询图片失败")?;
            let rows = stmt
                .query_map([chapter_id], DbImage::from_row)
                .context("查询图片失败")?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.context("解析图片行失败")?);
            }
            Ok(out)
        })
    }

    /// 某章节还剩多少张没下完。**这是章节完整性的真正判据。**
    ///
    /// 替代原先的 `downloaded_img_count != total_img_count`：
    /// 计数会因为并发自增的顺序问题出现瞬时偏差，而查表是精确且幂等的。
    pub fn count_unfinished(store: &Store, chapter_id: &str) -> anyhow::Result<i64> {
        store.with_conn(|conn| {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM download_image WHERE chapter_id = ?1 AND state != 'done'",
                    [chapter_id],
                    |row| row.get(0),
                )
                .context("统计未完成图片失败")?;
            Ok(n)
        })
    }

    /// 只取还需要下载的图片（`state != 'done'`）。
    pub fn list_unfinished(store: &Store, chapter_id: &str) -> anyhow::Result<Vec<DbImage>> {
        store.with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    r#"
                    SELECT chapter_id, img_index, url, state, retry_count, last_error,
                           bytes, updated_at
                      FROM download_image
                     WHERE chapter_id = ?1 AND state != 'done'
                     ORDER BY img_index ASC
                    "#,
                )
                .context("准备查询未完成图片失败")?;
            let rows = stmt
                .query_map([chapter_id], DbImage::from_row)
                .context("查询未完成图片失败")?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.context("解析图片行失败")?);
            }
            Ok(out)
        })
    }

    /// 章节是否已登记过图片清单。
    ///
    /// 用来区分「首次下载」和「断点续传」：首次要拉取图片列表并登记，
    /// 续传则直接复用 DB 里的 URL 清单，省一次 API 往返。
    pub fn has_manifest(store: &Store, chapter_id: &str) -> anyhow::Result<bool> {
        store.with_conn(|conn| {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM download_image WHERE chapter_id = ?1",
                    [chapter_id],
                    |row| row.get(0),
                )
                .context("查询图片清单失败")?;
            Ok(n > 0)
        })
    }

    /// 失败图片的明细，供 API 暴露给前端。
    pub fn list_failed(store: &Store, chapter_id: &str) -> anyhow::Result<Vec<DbImage>> {
        store.with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    r#"
                    SELECT chapter_id, img_index, url, state, retry_count, last_error,
                           bytes, updated_at
                      FROM download_image
                     WHERE chapter_id = ?1 AND state = 'failed'
                     ORDER BY img_index ASC
                    "#,
                )
                .context("准备查询失败图片失败")?;
            let rows = stmt
                .query_map([chapter_id], DbImage::from_row)
                .context("查询失败图片失败")?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.context("解析图片行失败")?);
            }
            Ok(out)
        })
    }
}

/// 把 `DbImageState` 转成字符串，方便日志。
impl std::fmt::Display for DbImageState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::fmt::Display for DbTaskState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ════════════════════════════════════════════════════════════════
// 测试：对照设计文档 §7 的验收标准
// ════════════════════════════════════════════════════════════════
//
// 这些测试直接打真实的 SQLite 文件（不是内存库），因为要验证的
// 恰恰是「进程死了状态还在」这件事——内存库证明不了这一点。
// 临时目录靠计数器去重，避免引入 `tempfile` 依赖（本地缓存里没有，
// 加进来会让离线构建失败）。

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::types::{DbTaskState, Store};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// 一个独占的临时目录，Drop 时清理。
    struct TempDb {
        path: PathBuf,
    }

    impl TempDb {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!(
                "pica-store-test-{}-{}-{}",
                std::process::id(),
                now_ts(),
                n
            ));
            std::fs::create_dir_all(&dir).expect("创建临时目录失败");
            Self {
                path: dir.join("pica_server.db"),
            }
        }

        fn open(&self) -> Store {
            Store::open(&self.path).expect("打开测试库失败")
        }

        /// 模拟进程重启：丢掉旧连接，重新打开同一个文件。
        fn reopen(&self) -> Store {
            Store::open(&self.path).expect("重新打开测试库失败")
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            if let Some(dir) = self.path.parent() {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }

    fn make_task(chapter_id: &str, state: DbTaskState, total: i64) -> DbTask {
        DbTask {
            chapter_id: chapter_id.to_string(),
            comic_id: "comic-1".to_string(),
            comic_title: "测试漫画".to_string(),
            chapter_title: "第 1 话".to_string(),
            chapter_order: 1,
            state,
            total_img_count: total,
            done_img_count: 0,
            retry_count: 0,
            last_error: None,
            dir_fmt: "{comic}/{chapter}".to_string(),
            created_at: now_ts(),
            updated_at: now_ts(),
        }
    }

    fn urls(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("https://cdn.example.com/{i}.jpg")).collect()
    }

    // ── 验收标准 1：任务状态跨重启存活 ──────────────────────────

    #[test]
    fn task_survives_restart() {
        let db = TempDb::new();

        {
            let store = db.open();
            TaskRepo::upsert_new(&store, &make_task("ch-1", DbTaskState::Pending, 10)).unwrap();
            ImageRepo::register_batch(&store, "ch-1", &urls(10)).unwrap();
            ImageRepo::mark_done(&store, "ch-1", 0, Some(1024)).unwrap();
            ImageRepo::mark_done(&store, "ch-1", 1, Some(2048)).unwrap();
            TaskRepo::resync_progress(&store, "ch-1").unwrap();
        }
        // 连接已丢弃 —— 等价于进程退出

        let store = db.reopen();
        let task = TaskRepo::get(&store, "ch-1").unwrap().expect("任务应当还在");

        assert_eq!(task.state, DbTaskState::Pending);
        assert_eq!(task.total_img_count, 10);
        assert_eq!(task.done_img_count, 2, "进度必须跨重启保留");
    }

    #[test]
    fn unfinished_tasks_are_listed_for_recovery() {
        let db = TempDb::new();
        let store = db.open();

        for (id, state) in [
            ("ch-pending", DbTaskState::Pending),
            ("ch-downloading", DbTaskState::Downloading),
            ("ch-paused", DbTaskState::Paused),
            ("ch-failed", DbTaskState::Failed),
            ("ch-done", DbTaskState::Completed),
            ("ch-cancelled", DbTaskState::Cancelled),
        ] {
            TaskRepo::upsert_new(&store, &make_task(id, state, 3)).unwrap();
        }

        let unfinished = TaskRepo::list_unfinished(&store).unwrap();
        let ids: Vec<&str> = unfinished.iter().map(|t| t.chapter_id.as_str()).collect();

        assert_eq!(ids.len(), 4, "只有非终态任务需要恢复：{ids:?}");
        assert!(ids.contains(&"ch-pending"));
        assert!(ids.contains(&"ch-downloading"));
        assert!(ids.contains(&"ch-paused"));
        assert!(ids.contains(&"ch-failed"));
        assert!(!ids.contains(&"ch-done"), "完成的任务不该被重新拉起");
        assert!(!ids.contains(&"ch-cancelled"), "取消的任务不该被复活");
    }

    // ── 验收标准 2：图片级断点续传 ──────────────────────────────

    #[test]
    fn register_batch_preserves_done_state() {
        let db = TempDb::new();
        let store = db.open();

        // 图片表有指向 download_task 的外键，父任务必须先存在
        TaskRepo::upsert_new(&store, &make_task("ch-1", DbTaskState::Downloading, 5)).unwrap();
        ImageRepo::register_batch(&store, "ch-1", &urls(5)).unwrap();
        ImageRepo::mark_done(&store, "ch-1", 2, Some(100)).unwrap();
        ImageRepo::mark_failed(&store, "ch-1", 3, "超时").unwrap();

        // 重跑同一个章节：URL 清单重新登记，但状态不能被冲掉
        ImageRepo::register_batch(&store, "ch-1", &urls(5)).unwrap();

        let imgs = ImageRepo::list_by_chapter(&store, "ch-1").unwrap();
        assert_eq!(imgs.len(), 5, "重复登记不应产生重复行");
        assert_eq!(
            imgs[2].state,
            DbImageState::Done,
            "断点续传的核心：已完成的图不能被重置"
        );
        assert_eq!(imgs[3].state, DbImageState::Failed);
        assert_eq!(ImageRepo::count_unfinished(&store, "ch-1").unwrap(), 4);
    }

    #[test]
    fn manifest_reuse_is_detectable() {
        let db = TempDb::new();
        let store = db.open();

        assert!(
            !ImageRepo::has_manifest(&store, "ch-1").unwrap(),
            "首次下载应当判定为「无清单」，需要拉 API"
        );

        TaskRepo::upsert_new(&store, &make_task("ch-1", DbTaskState::Downloading, 2)).unwrap();
        ImageRepo::register_batch(&store, "ch-1", &urls(2)).unwrap();
        assert!(
            ImageRepo::has_manifest(&store, "ch-1").unwrap(),
            "已登记后应当复用 DB 里的 URL，省一次 API 往返"
        );
    }

    // ── 验收标准 3：单图重试 ────────────────────────────────────

    #[test]
    fn retry_resets_only_unfinished() {
        let db = TempDb::new();
        let store = db.open();

        TaskRepo::upsert_new(&store, &make_task("ch-1", DbTaskState::Failed, 5)).unwrap();
        ImageRepo::register_batch(&store, "ch-1", &urls(5)).unwrap();

        ImageRepo::mark_done(&store, "ch-1", 0, Some(1)).unwrap();
        ImageRepo::mark_done(&store, "ch-1", 1, Some(1)).unwrap();
        ImageRepo::mark_failed(&store, "ch-1", 2, "超时").unwrap();
        ImageRepo::mark_failed(&store, "ch-1", 3, "超时").unwrap();
        ImageRepo::mark_failed(&store, "ch-1", 4, "超时").unwrap();

        // 重试前的明细：3 张失败
        let failed = ImageRepo::list_failed(&store, "ch-1").unwrap();
        assert_eq!(failed.len(), 3);

        let reset = ImageRepo::reset_failed(&store, "ch-1").unwrap();
        assert_eq!(reset, 3, "只重置未完成的 3 张");

        let (total, done) = TaskRepo::resync_progress(&store, "ch-1").unwrap();
        assert_eq!(total, 5);
        assert_eq!(done, 2, "已完成的 2 张不能在重试中丢失");

        // 重置后失败的明细应当清空，且错误信息被抹掉
        assert!(ImageRepo::list_failed(&store, "ch-1").unwrap().is_empty());
        assert_eq!(ImageRepo::count_unfinished(&store, "ch-1").unwrap(), 3);
    }

    #[test]
    fn mark_failed_accumulates_retry_count() {
        let db = TempDb::new();
        let store = db.open();

        TaskRepo::upsert_new(&store, &make_task("ch-1", DbTaskState::Downloading, 1)).unwrap();
        ImageRepo::register_batch(&store, "ch-1", &urls(1)).unwrap();

        assert_eq!(ImageRepo::mark_failed(&store, "ch-1", 0, "第一次").unwrap(), 1);
        assert_eq!(ImageRepo::mark_failed(&store, "ch-1", 0, "第二次").unwrap(), 2);

        let img = &ImageRepo::list_failed(&store, "ch-1").unwrap()[0];
        assert_eq!(img.retry_count, 2);
        assert_eq!(img.last_error.as_deref(), Some("第二次"), "保留最后一次错误");
    }

    #[test]
    fn mark_done_clears_previous_error() {
        let db = TempDb::new();
        let store = db.open();

        TaskRepo::upsert_new(&store, &make_task("ch-1", DbTaskState::Downloading, 1)).unwrap();
        ImageRepo::register_batch(&store, "ch-1", &urls(1)).unwrap();
        ImageRepo::mark_failed(&store, "ch-1", 0, "超时").unwrap();
        ImageRepo::mark_done(&store, "ch-1", 0, Some(999)).unwrap();

        let imgs = ImageRepo::list_by_chapter(&store, "ch-1").unwrap();
        assert_eq!(imgs[0].state, DbImageState::Done);
        assert!(imgs[0].last_error.is_none(), "成功后不该残留错误信息");
        assert_eq!(imgs[0].bytes, Some(999));
    }

    // ── 验收标准 4：完成判定以图片表为准（D3）────────────────────

    #[test]
    fn completion_is_decided_by_image_table_not_counter() {
        let db = TempDb::new();
        let store = db.open();

        // 计数谎报「10 张全下完」，但图片表里只有 1 张 done
        let mut task = make_task("ch-1", DbTaskState::Downloading, 10);
        task.done_img_count = 10;
        TaskRepo::upsert_new(&store, &task).unwrap();

        ImageRepo::register_batch(&store, "ch-1", &urls(10)).unwrap();
        ImageRepo::mark_done(&store, "ch-1", 0, Some(1)).unwrap();

        let unfinished = ImageRepo::count_unfinished(&store, "ch-1").unwrap();
        assert_eq!(
            unfinished, 9,
            "D3：完成与否必须数图片表，不能信 done_img_count"
        );

        // resync 之后计数被纠正回事实
        let (total, done) = TaskRepo::resync_progress(&store, "ch-1").unwrap();
        assert_eq!((total, done), (10, 1));
    }

    // ── 验收标准 3：单图失败不炸章节 ──────────────────────────

    #[test]
    fn single_image_failure_does_not_affect_siblings_and_retry_targets_only_it() {
        let db = TempDb::new();
        let store = db.open();

        TaskRepo::upsert_new(&store, &make_task("ch-1", DbTaskState::Downloading, 5)).unwrap();
        ImageRepo::register_batch(&store, "ch-1", &urls(5)).unwrap();

        // 4 张成功，仅第 2 张（index=2）失败
        ImageRepo::mark_done(&store, "ch-1", 0, Some(1)).unwrap();
        ImageRepo::mark_done(&store, "ch-1", 1, Some(1)).unwrap();
        ImageRepo::mark_failed(&store, "ch-1", 2, "HTTP 500").unwrap();
        ImageRepo::mark_done(&store, "ch-1", 3, Some(1)).unwrap();
        ImageRepo::mark_done(&store, "ch-1", 4, Some(1)).unwrap();

        // 章节收尾：未完成数 == 1，只指向失败那张，兄弟图不受影响
        assert_eq!(
            ImageRepo::count_unfinished(&store, "ch-1").unwrap(),
            1,
            "只有失败的那 1 张算未完成，其余 4 张 done 不受影响"
        );
        let failed = ImageRepo::list_failed(&store, "ch-1").unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].img_index, 2);
        assert_eq!(failed[0].last_error.as_deref(), Some("HTTP 500"));

        // 重试：只重置失败的那 1 张，已 done 的 4 张不动
        let reset = ImageRepo::reset_failed(&store, "ch-1").unwrap();
        assert_eq!(reset, 1, "重试只补失败的那 1 张");
        assert_eq!(ImageRepo::count_unfinished(&store, "ch-1").unwrap(), 1);
        let imgs = ImageRepo::list_by_chapter(&store, "ch-1").unwrap();
        for img in &imgs {
            if img.img_index == 2 {
                assert_eq!(img.state, DbImageState::Pending, "失败张被重置为待下");
            } else {
                assert_eq!(img.state, DbImageState::Done, "已成功的兄弟图保持 done");
            }
        }
    }
    // ── 验收标准 5：幂等重提交（D5 过渡期）──────────────────────

    #[test]
    fn upsert_new_is_idempotent_and_preserves_progress() {
        let db = TempDb::new();
        let store = db.open();

        TaskRepo::upsert_new(&store, &make_task("ch-1", DbTaskState::Pending, 5)).unwrap();
        ImageRepo::register_batch(&store, "ch-1", &urls(5)).unwrap();
        ImageRepo::mark_done(&store, "ch-1", 0, Some(1)).unwrap();
        TaskRepo::resync_progress(&store, "ch-1").unwrap();
        TaskRepo::set_state(&store, "ch-1", DbTaskState::Downloading, None, 0).unwrap();

        // 青龙重复提交同一个章节（过渡期两套状态源并存时的常见情况）
        TaskRepo::upsert_new(&store, &make_task("ch-1", DbTaskState::Pending, 5)).unwrap();

        let task = TaskRepo::get(&store, "ch-1").unwrap().unwrap();
        assert_eq!(
            task.state,
            DbTaskState::Downloading,
            "重复提交不能把正在下载的任务打回 pending"
        );
        assert_eq!(task.done_img_count, 1, "进度不能被重置");
    }

    // ── 验收标准 6：清理终态但不误删失败任务（D7）────────────────

    #[test]
    fn purge_terminal_keeps_failed_tasks() {
        let db = TempDb::new();
        let store = db.open();

        for (id, state) in [
            ("ch-done", DbTaskState::Completed),
            ("ch-cancelled", DbTaskState::Cancelled),
            ("ch-failed", DbTaskState::Failed),
        ] {
            let mut task = make_task(id, state, 2);
            // 把时间推到很久以前，确保落进清理窗口
            task.created_at = now_ts() - 100_000;
            task.updated_at = now_ts() - 100_000;
            TaskRepo::upsert_new(&store, &task).unwrap();
        }

        let removed = TaskRepo::purge_terminal(&store, now_ts() - 1000).unwrap();
        assert_eq!(removed, 2, "只清理 completed / cancelled");

        assert!(TaskRepo::get(&store, "ch-done").unwrap().is_none());
        assert!(TaskRepo::get(&store, "ch-cancelled").unwrap().is_none());
        assert!(
            TaskRepo::get(&store, "ch-failed").unwrap().is_some(),
            "D7：失败任务要保留，用户可能还要手动重试"
        );
    }

    #[test]
    fn delete_cascades_to_images() {
        let db = TempDb::new();
        let store = db.open();

        TaskRepo::upsert_new(&store, &make_task("ch-1", DbTaskState::Completed, 3)).unwrap();
        ImageRepo::register_batch(&store, "ch-1", &urls(3)).unwrap();

        assert!(TaskRepo::delete(&store, "ch-1").unwrap());
        assert!(!TaskRepo::delete(&store, "ch-1").unwrap(), "重复删除返回 false");

        assert!(
            ImageRepo::list_by_chapter(&store, "ch-1").unwrap().is_empty(),
            "图片行靠外键级联删除，不能留孤儿"
        );
    }

    // ── 分页与过滤（Step 4 的 API 契约）─────────────────────────

    #[test]
    fn list_filters_and_paginates() {
        let db = TempDb::new();
        let store = db.open();

        for i in 0..5 {
            let mut task = make_task(&format!("ch-{i}"), DbTaskState::Pending, 1);
            task.comic_id = if i < 3 { "comic-a" } else { "comic-b" }.to_string();
            TaskRepo::upsert_new(&store, &task).unwrap();
        }

        // 按 state 过滤
        let pending = TaskRepo::list(&store, Some(DbTaskState::Pending), None, None, 100, 0).unwrap();
        assert_eq!(pending.len(), 5);

        let completed =
            TaskRepo::list(&store, Some(DbTaskState::Completed), None, None, 100, 0).unwrap();
        assert!(completed.is_empty());

        // 按 comic_id 过滤
        let a = TaskRepo::list(&store, None, Some("comic-a"), None, 100, 0).unwrap();
        assert_eq!(a.len(), 3);

        // 分页
        let page1 = TaskRepo::list(&store, None, None, None, 2, 0).unwrap();
        let page2 = TaskRepo::list(&store, None, None, None, 2, 2).unwrap();
        assert_eq!(page1.len(), 2);
        assert_eq!(page2.len(), 2);
        assert_ne!(page1[0].chapter_id, page2[0].chapter_id);

        // count 必须与不带分页的 list 长度一致
        assert_eq!(TaskRepo::count(&store, None, None, None).unwrap(), 5);
        assert_eq!(
            TaskRepo::count(&store, None, Some("comic-b"), None).unwrap(),
            2
        );
    }

    #[test]
    fn since_cursor_returns_only_recent() {
        let db = TempDb::new();
        let store = db.open();

        let mut old = make_task("ch-old", DbTaskState::Completed, 1);
        old.updated_at = now_ts() - 10_000;
        TaskRepo::upsert_new(&store, &old).unwrap();

        let fresh = make_task("ch-fresh", DbTaskState::Pending, 1);
        TaskRepo::upsert_new(&store, &fresh).unwrap();

        let recent = TaskRepo::list(&store, None, None, Some(now_ts() - 60), 100, 0).unwrap();
        assert_eq!(recent.len(), 1, "青龙增量拉取只应看到新记录");
        assert_eq!(recent[0].chapter_id, "ch-fresh");
    }

    #[test]
    fn stats_counts_each_state() {
        let db = TempDb::new();
        let store = db.open();

        TaskRepo::upsert_new(&store, &make_task("a", DbTaskState::Pending, 1)).unwrap();
        TaskRepo::upsert_new(&store, &make_task("b", DbTaskState::Pending, 1)).unwrap();
        TaskRepo::upsert_new(&store, &make_task("c", DbTaskState::Completed, 1)).unwrap();
        TaskRepo::upsert_new(&store, &make_task("d", DbTaskState::Failed, 1)).unwrap();

        let stats = TaskRepo::stats(&store).unwrap();
        assert_eq!(stats.pending, 2);
        assert_eq!(stats.completed, 1);
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.total, 4);
    }

    // ── 状态机细节 ──────────────────────────────────────────────

    #[test]
    fn set_state_reports_real_transitions_only() {
        let db = TempDb::new();
        let store = db.open();

        TaskRepo::upsert_new(&store, &make_task("ch-1", DbTaskState::Pending, 1)).unwrap();

        assert!(
            TaskRepo::set_state(&store, "ch-1", DbTaskState::Downloading, None, 0).unwrap(),
            "pending → downloading 是一次真实迁移"
        );
        assert!(
            !TaskRepo::set_state(&store, "ch-1", DbTaskState::Downloading, None, 0).unwrap(),
            "同状态重复写入不算迁移，避免无谓的 WS 事件"
        );
        assert!(
            !TaskRepo::set_state(&store, "ch-missing", DbTaskState::Failed, None, 0).unwrap(),
            "不存在的任务返回 false 而不是报错"
        );
    }

    #[test]
    fn set_state_records_last_error() {
        let db = TempDb::new();
        let store = db.open();

        TaskRepo::upsert_new(&store, &make_task("ch-1", DbTaskState::Downloading, 1)).unwrap();
        TaskRepo::set_state(&store, "ch-1", DbTaskState::Failed, Some("CDN 超时"), 1).unwrap();

        let task = TaskRepo::get(&store, "ch-1").unwrap().unwrap();
        assert_eq!(task.state, DbTaskState::Failed);
        assert_eq!(task.last_error.as_deref(), Some("CDN 超时"));
        assert_eq!(task.retry_count, 1, "retry_count 必须随状态一起落库");
    }

    /// 回归测试：`Failed` → `Failed` 这种「同状态重复写入」以前被
    /// `AND state != ?` 守卫挡掉，导致第二次失败的错误信息永远写不进去，
    /// 排查时只能看到第一次的原因。
    #[test]
    fn repeated_failure_refreshes_last_error_and_retry_count() {
        let db = TempDb::new();
        let store = db.open();

        TaskRepo::upsert_new(&store, &make_task("ch-1", DbTaskState::Downloading, 1)).unwrap();

        TaskRepo::set_state(&store, "ch-1", DbTaskState::Failed, Some("第一次：连接超时"), 1)
            .unwrap();
        TaskRepo::set_state(&store, "ch-1", DbTaskState::Failed, Some("第二次：404"), 2).unwrap();

        let task = TaskRepo::get(&store, "ch-1").unwrap().unwrap();
        assert_eq!(
            task.last_error.as_deref(),
            Some("第二次：404"),
            "重复失败必须刷新 last_error"
        );
        assert_eq!(task.retry_count, 2, "重复失败必须刷新 retry_count");
    }

    /// 回归测试：只更新重试计数（不动状态）的路径。
    #[test]
    fn set_retry_count_updates_without_touching_state() {
        let db = TempDb::new();
        let store = db.open();

        TaskRepo::upsert_new(&store, &make_task("ch-1", DbTaskState::Downloading, 1)).unwrap();

        assert!(TaskRepo::set_retry_count(&store, "ch-1", 7).unwrap());
        let task = TaskRepo::get(&store, "ch-1").unwrap().unwrap();
        assert_eq!(task.retry_count, 7);
        assert_eq!(
            task.state,
            DbTaskState::Downloading,
            "只改计数不应该影响状态"
        );

        assert!(
            !TaskRepo::set_retry_count(&store, "ch-missing", 3).unwrap(),
            "不存在的任务返回 false 而不是报错"
        );
    }

    #[test]
    fn task_states_roundtrip_through_db() {
        let db = TempDb::new();
        let store = db.open();

        for (i, state) in [
            DbTaskState::Pending,
            DbTaskState::Downloading,
            DbTaskState::Paused,
            DbTaskState::Completed,
            DbTaskState::Failed,
            DbTaskState::Cancelled,
        ]
        .into_iter()
        .enumerate()
        {
            let id = format!("ch-{i}");
            TaskRepo::upsert_new(&store, &make_task(&id, state, 1)).unwrap();
            let back = TaskRepo::get(&store, &id).unwrap().unwrap();
            assert_eq!(back.state, state, "状态 {state} 存取后应保持一致");
        }
    }

    #[test]
    fn terminal_states_are_classified() {
        assert!(DbTaskState::Completed.is_terminal());
        assert!(DbTaskState::Cancelled.is_terminal());
        assert!(!DbTaskState::Pending.is_terminal());
        assert!(!DbTaskState::Downloading.is_terminal());
        assert!(!DbTaskState::Paused.is_terminal());
        assert!(!DbTaskState::Failed.is_terminal(), "失败可重试，不算终态");
    }

    // ── 容错 ────────────────────────────────────────────────────

    #[test]
    fn open_or_recover_rebuilds_corrupt_db() {
        let db = TempDb::new();

        // 写一个不是 SQLite 的文件冒充数据库
        std::fs::write(&db.path, b"this is definitely not a sqlite file").unwrap();

        let store = Store::open_or_recover(&db.path).expect("损坏时应重建而不是让服务起不来");
        TaskRepo::upsert_new(&store, &make_task("ch-1", DbTaskState::Pending, 1)).unwrap();
        assert!(TaskRepo::get(&store, "ch-1").unwrap().is_some());
    }
}