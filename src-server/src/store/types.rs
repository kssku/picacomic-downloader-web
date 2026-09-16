//! Store 的核心类型：连接句柄 + 行映射结构。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use rusqlite::Connection;

use super::migrations;

/// 数据库句柄。
///
/// 内部用 `Mutex<Connection>` 而不是连接池：写操作全部集中在状态迁移点
/// （创建任务、图片完成、状态变更），这些点本身就不高频，单连接足够；
/// 而 SQLite 的并发写本来就要串行化，池化只会把等待点从应用层挪到锁层，
/// 并不能提高写入吞吐。
///
/// WAL 模式下读不阻塞写，所以即便是长列表查询也不会卡住下载。
#[derive(Clone)]
pub struct Store {
    conn: Arc<parking_lot::Mutex<Connection>>,
    path: Arc<PathBuf>,
}

impl Store {
    /// 打开（或创建）数据库，并跑完迁移。
    pub fn open(db_path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建数据库目录 `{}` 失败", parent.display()))?;
        }

        let conn = Connection::open(db_path)
            .with_context(|| format!("打开数据库 `{}` 失败", db_path.display()))?;

        Self::tune(&conn)?;

        // 启动时做一次完整性检查。损坏时**不阻塞启动**——下载服务本身的
        // 可用性比历史任务记录更重要。调用方拿到 `Err` 后应该重命名旧库、
        // 重建一个空库，并把状态降级为「无历史」。
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .context("执行 `integrity_check` 失败")?;
        if integrity != "ok" {
            anyhow::bail!("数据库 `{}` 完整性检查未通过：{integrity}", db_path.display());
        }

        migrations::run(&conn).context("执行数据库迁移失败")?;

        Ok(Self {
            conn: Arc::new(parking_lot::Mutex::new(conn)),
            path: Arc::new(db_path.to_path_buf()),
        })
    }

    /// 打开数据库；损坏时把旧库改名备份后重建空库。
    ///
    /// 这是启动路径应该用的入口：宁可丢历史任务记录，也不能让服务起不来。
    pub fn open_or_recover(db_path: &Path) -> anyhow::Result<Self> {
        match Self::open(db_path) {
            Ok(store) => Ok(store),
            Err(err) => {
                tracing::error!(
                    err_title = "数据库损坏，将备份旧库并重建",
                    db_path = %db_path.display(),
                    message = %err
                );

                if db_path.exists() {
                    let backup = db_path.with_extension(format!(
                        "db.corrupt-{}",
                        now_ts()
                    ));
                    std::fs::rename(db_path, &backup).with_context(|| {
                        format!(
                            "备份损坏的数据库 `{}` 到 `{}` 失败",
                            db_path.display(),
                            backup.display()
                        )
                    })?;
                    tracing::warn!(
                        err_title = "已备份损坏的数据库",
                        backup = %backup.display()
                    );
                }

                // WAL 的两个附属文件也要清掉，否则重建的库会继承旧 WAL。
                for suffix in ["-wal", "-shm"] {
                    let side = PathBuf::from(format!("{}{suffix}", db_path.display()));
                    if side.exists() {
                        let _ = std::fs::remove_file(&side);
                    }
                }

                Self::open(db_path)
            }
        }
    }

    /// 连接级 pragma。必须在迁移之前执行。
    fn tune(conn: &Connection) -> anyhow::Result<()> {
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;      -- 读不阻塞写
            PRAGMA synchronous = NORMAL;    -- WAL 下的安全/性能平衡点
            PRAGMA foreign_keys = ON;       -- download_image 的级联删除依赖它
            PRAGMA busy_timeout = 5000;     -- 写冲突时等 5s 而不是立刻报错
            "#,
        )
        .context("设置数据库 pragma 失败")?;
        Ok(())
    }

    /// 借出连接。调用方持锁期间不要做网络 IO。
    pub fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let conn = self.conn.lock();
        f(&conn)
    }

    /// 在事务里执行。闭包返回 `Err` 时自动回滚。
    pub fn with_tx<T>(
        &self,
        f: impl FnOnce(&rusqlite::Transaction) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction().context("开启事务失败")?;
        let value = f(&tx)?;
        tx.commit().context("提交事务失败")?;
        Ok(value)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// 当前 Unix 时间戳（秒）。
///
/// 统一走这里而不是各处调 `SystemTime::now()`，方便日后换成注入式时钟做测试。
pub fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 章节级任务状态。
///
/// 与内存里的 `DownloadTaskState` 刻意分开：DB 层的状态是**跨进程的持久事实**，
/// 而内存层还要额外承载 `Downloading` 这种「本进程正在跑」的瞬态。
/// 两者转换见 `DownloadTaskState::to_db` / `from_db`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbTaskState {
    Pending,
    Downloading,
    Paused,
    Cancelled,
    Completed,
    Failed,
}

impl DbTaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Downloading => "downloading",
            Self::Paused => "paused",
            Self::Cancelled => "cancelled",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> anyhow::Result<Self> {
        Ok(match s {
            "pending" => Self::Pending,
            "downloading" => Self::Downloading,
            "paused" => Self::Paused,
            "cancelled" => Self::Cancelled,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            other => anyhow::bail!("未知的章节任务状态 `{other}`"),
        })
    }

    /// 终态：不会再自动发生变化。
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled)
    }
}

/// 图片级状态。刻意只有三态：断点续传只关心「要不要重下」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbImageState {
    Pending,
    Done,
    Failed,
}

impl DbImageState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> anyhow::Result<Self> {
        Ok(match s {
            "pending" => Self::Pending,
            "done" => Self::Done,
            "failed" => Self::Failed,
            other => anyhow::bail!("未知的图片状态 `{other}`"),
        })
    }
}

/// `download_task` 的一行。
#[derive(Debug, Clone)]
pub struct DbTask {
    pub chapter_id: String,
    pub comic_id: String,
    pub comic_title: String,
    pub chapter_title: String,
    pub chapter_order: i64,
    pub state: DbTaskState,
    pub total_img_count: i64,
    pub done_img_count: i64,
    pub retry_count: i64,
    pub last_error: Option<String>,
    pub dir_fmt: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// `download_image` 的一行。
#[derive(Debug, Clone)]
pub struct DbImage {
    pub chapter_id: String,
    pub img_index: i64,
    pub url: String,
    pub state: DbImageState,
    pub retry_count: i64,
    pub last_error: Option<String>,
    pub bytes: Option<i64>,
    pub updated_at: i64,
}

impl DbTask {
    /// 从查询结果里读一行。
    ///
    /// 列顺序必须与 `repo::TASK_COLUMNS` 一致。
    pub fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let state_str: String = row.get("state")?;
        let state = DbTaskState::parse(&state_str).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())),
            )
        })?;
        Ok(Self {
            chapter_id: row.get("chapter_id")?,
            comic_id: row.get("comic_id")?,
            comic_title: row.get("comic_title")?,
            chapter_title: row.get("chapter_title")?,
            chapter_order: row.get("chapter_order")?,
            state,
            total_img_count: row.get("total_img_count")?,
            done_img_count: row.get("done_img_count")?,
            retry_count: row.get("retry_count")?,
            last_error: row.get("last_error")?,
            dir_fmt: row.get("dir_fmt")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }
}

impl DbImage {
    pub fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let state_str: String = row.get("state")?;
        let state = DbImageState::parse(&state_str).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())),
            )
        })?;
        Ok(Self {
            chapter_id: row.get("chapter_id")?,
            img_index: row.get("img_index")?,
            url: row.get("url")?,
            state,
            retry_count: row.get("retry_count")?,
            last_error: row.get("last_error")?,
            bytes: row.get("bytes")?,
            updated_at: row.get("updated_at")?,
        })
    }
}