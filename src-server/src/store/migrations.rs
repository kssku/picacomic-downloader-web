//! 版本化建表。
//!
//! 用 `user_version` pragma 记录 schema 版本，逐版本向上迁移。
//! 之所以不引 `refinery` / `sqlx-migrate`：这个项目的迁移就两张表，
//! 引一套迁移框架的编译开销和心智负担都不划算。

use anyhow::Context;
use rusqlite::Connection;

/// 当前 schema 版本。每次改表结构都要 +1 并补一个 `migrate_vN` 函数。
pub const SCHEMA_VERSION: i64 = 1;

/// schema 版本高于程序支持。
///
/// **与「数据库损坏」是不同性质**：这是逻辑错误（库被更新版本的程序建过），
/// 库本身完好、数据也完好，不该触发重建 —— 否则会静默删除用户数据。
/// [`crate::store::Store::open_or_recover`] 用 `downcast_ref` 识别它并
/// 直接向上传播，不进入损坏重建路径。
#[derive(Debug)]
pub struct SchemaTooNew {
    pub current: i64,
    pub supported: i64,
}

impl std::fmt::Display for SchemaTooNew {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "数据库 schema 版本为 `{}`，高于本程序支持的 `{}`。\
             请升级 pica-server，或备份后删除 `pica_server.db` 重建。",
            self.current, self.supported
        )
    }
}

impl std::error::Error for SchemaTooNew {}

/// 把连接迁移到最新 schema。
pub fn run(conn: &Connection) -> anyhow::Result<()> {
    let current: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .context("读取 `user_version` 失败")?;

    if current > SCHEMA_VERSION {
        // 降级场景：DB 是更新版本的程序建的。库完好，只是程序太旧。
        // 返回专属错误类型而非 anyhow!，好让 open_or_recover 区分
        // 「版本过高」和「真损坏」—— 前者拒绝启动，后者才重建。
        return Err(anyhow::Error::new(SchemaTooNew {
            current,
            supported: SCHEMA_VERSION,
        }));
    }

    if current < 1 {
        migrate_v1(conn).context("执行 v1 迁移失败")?;
    }

    conn.pragma_update(None, "user_version", SCHEMA_VERSION)
        .context("写入 `user_version` 失败")?;

    Ok(())
}

/// v1：任务表 + 图片表。
fn migrate_v1(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        r#"
        -- ── 章节级任务 ──────────────────────────────────────────
        CREATE TABLE IF NOT EXISTS download_task (
            chapter_id      TEXT PRIMARY KEY,
            comic_id        TEXT NOT NULL,
            comic_title     TEXT NOT NULL,
            chapter_title   TEXT NOT NULL,
            chapter_order   INTEGER NOT NULL DEFAULT 0,
            -- 取值见 DbTaskState::as_str()：pending / downloading / paused /
            -- cancelled / completed / failed
            state           TEXT NOT NULL,

            -- 进度计数。保留是为了让前端继续用现有的 `downloaded/total` 渲染逻辑，
            -- 但**完整性判定不再依赖它们**，而是查 download_image 里
            -- 还有没有 state != 'done' 的行。见 repo.rs 的 `is_chapter_complete`。
            total_img_count INTEGER NOT NULL DEFAULT 0,
            done_img_count  INTEGER NOT NULL DEFAULT 0,

            retry_count     INTEGER NOT NULL DEFAULT 0,
            last_error      TEXT,

            -- 目录格式快照。恢复时必须用任务自己当初的快照，而不是当前配置，
            -- 否则用户中途改了 dir_fmt，恢复时就会找不到已下载的文件。
            dir_fmt         TEXT NOT NULL DEFAULT '',

            created_at      INTEGER NOT NULL,
            updated_at      INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_task_state ON download_task(state);
        CREATE INDEX IF NOT EXISTS idx_task_comic ON download_task(comic_id);
        -- 青龙侧按完成时间增量拉取，需要这个索引
        CREATE INDEX IF NOT EXISTS idx_task_updated ON download_task(updated_at);

        -- ── 图片级任务（断点续传的核心）────────────────────────
        CREATE TABLE IF NOT EXISTS download_image (
            chapter_id  TEXT NOT NULL,
            img_index   INTEGER NOT NULL,
            url         TEXT NOT NULL,
            state       TEXT NOT NULL,
            retry_count INTEGER NOT NULL DEFAULT 0,
            last_error  TEXT,
            bytes       INTEGER,
            updated_at  INTEGER NOT NULL,

            PRIMARY KEY (chapter_id, img_index),
            FOREIGN KEY (chapter_id) REFERENCES download_task(chapter_id) ON DELETE CASCADE
        );

        -- 恢复时的高频查询：某章节还有哪些图没下完
        CREATE INDEX IF NOT EXISTS idx_image_pending
            ON download_image(chapter_id, state);
        "#,
    )
    .context("建表失败")?;

    Ok(())
}