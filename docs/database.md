# picacomic-downloader-web — 数据库契约

> 本文档描述 `src-server` 的 SQLite 持久层。
> 逐条对照 `src-server/src/store/migrations.rs`（97 行）、`src-server/src/store/types.rs`。
> 最后核对版本：`SCHEMA_VERSION = 1`，2 张表，4 个索引。

---

## 一、数据库文件

| 项 | 值 | 出处 |
|---|---|---|
| 路径 | `$PICA_DATA_DIR/pica_server.db` | `context.rs:45-46` |
| 引擎 | SQLite（`rusqlite`，bundled） | `Cargo.toml` |
| 打开入口 | `Store::open_or_recover()` | `types.rs:60` |
| 连接持有 | `Arc<parking_lot::Mutex<Connection>>`（单连接串行化） | `types.rs:21-24` |

**文件独立**：数据库不与青龙脚本共享任何文件，青龙只通过 HTTP `/api/tasks*` 读写
（见 `docs/API.md` 第五节）。因此本项目不承担青龙的 schema 兼容义务。

---

## 二、连接级 PRAGMA

`Store::tune()`（`types.rs:96-110`）在**迁移之前**执行，四条：

```sql
PRAGMA journal_mode = WAL;      -- 读不阻塞写
PRAGMA synchronous = NORMAL;    -- WAL 下的安全/性能平衡点
PRAGMA foreign_keys = ON;       -- download_image 的级联删除依赖它
PRAGMA busy_timeout = 5000;     -- 写冲突时等 5s 而不是立刻报错
```

> ⚠️ `foreign_keys = ON` 是**连接级**的，SQLite 默认关闭。
> 也就是说 `ON DELETE CASCADE` 只在经由本程序打开的连接上生效；
> 用外部 sqlite3 CLI 直接操作时，级联不会触发。

---

## 三、版本化迁移

机制：`PRAGMA user_version` 记录 schema 版本，逐版本向上迁移。

```rust
// src-server/src/store/migrations.rs:11
pub const SCHEMA_VERSION: i64 = 1;
```

### 启动流程（`migrations.rs:14-37`）

1. 读 `PRAGMA user_version` → `current`
2. `current > SCHEMA_VERSION` → **`bail!` 拒绝启动**，提示「升级 pica-server，或备份后删除 `pica_server.db` 重建」
3. `current < 1` → 执行 `migrate_v1()`
4. 写回 `user_version = SCHEMA_VERSION`

### 为什么不用 refinery / sqlx-migrate

源码注释（`migrations.rs:4-5`）明确写了取舍：
> 这个项目的迁移就两张表，引一套迁移框架的编译开销和心智负担都不划算。

### 加一次迁移的正确做法

1. `SCHEMA_VERSION += 1`
2. 新增 `fn migrate_vN(conn) -> anyhow::Result<()>`
3. 在 `run()` 里补一个 `if current < N { migrate_vN(conn)?; }`

> ⚠️ 降级（DB 版本 > 程序版本）是**硬失败**，不是自动降级。
> 用户换回旧镜像会起不来，错误信息里给了手工出路。

---

## 四、表结构（逐字）

### 4.1 `download_task` —— 章节级任务（13 列）

```sql
CREATE TABLE IF NOT EXISTS download_task (
    chapter_id      TEXT PRIMARY KEY,        -- 主键，业务 ID
    comic_id        TEXT NOT NULL,
    comic_title     TEXT NOT NULL,
    chapter_title   TEXT NOT NULL,
    chapter_order   INTEGER NOT NULL DEFAULT 0,
    state           TEXT NOT NULL,           -- DbTaskState::as_str() 小写

    total_img_count INTEGER NOT NULL DEFAULT 0,
    done_img_count  INTEGER NOT NULL DEFAULT 0,

    retry_count     INTEGER NOT NULL DEFAULT 0,
    last_error      TEXT,                    -- 可空

    dir_fmt         TEXT NOT NULL DEFAULT '',-- 目录格式快照
    created_at      INTEGER NOT NULL,        -- Unix 秒
    updated_at      INTEGER NOT NULL         -- Unix 秒
);
```

**列清单（13）**：
`chapter_id`, `comic_id`, `comic_title`, `chapter_title`, `chapter_order`, `state`,
`total_img_count`, `done_img_count`, `retry_count`, `last_error`, `dir_fmt`,
`created_at`, `updated_at`

> ⚠️ **`total_img_count` / `done_img_count` 不再是完整性判据。**
> 源码注释（`migrations.rs:52-54`）说明：保留它们只是让前端继续用现有的
> `downloaded/total` 渲染逻辑；**真正的完整性判定改为查 `download_image`
> 里还有没有 `state != 'done'` 的行**（`repo.rs` 的 `is_chapter_complete`）。
> 即：这两个计数是**展示用的派生值**，不是事实来源。

> ⚠️ **`dir_fmt` 是快照，不是引用。**
> 源码注释（`migrations.rs:61-62`）：恢复时必须用任务自己当初的快照，
> 而不是当前配置，否则用户中途改了 `dir_fmt`，恢复时就会找不到已下载的文件。

### 4.2 `download_image` —— 图片级任务（8 列）

```sql
CREATE TABLE IF NOT EXISTS download_image (
    chapter_id  TEXT NOT NULL,
    img_index   INTEGER NOT NULL,
    url         TEXT NOT NULL,
    state       TEXT NOT NULL,           -- DbImageState::as_str() 小写
    retry_count INTEGER NOT NULL DEFAULT 0,
    last_error  TEXT,                    -- 可空
    bytes       INTEGER,                 -- 可空，下载完成后回填
    updated_at  INTEGER NOT NULL,        -- Unix 秒

    PRIMARY KEY (chapter_id, img_index),
    FOREIGN KEY (chapter_id) REFERENCES download_task(chapter_id) ON DELETE CASCADE
);
```

**列清单（8）**：
`chapter_id`, `img_index`, `url`, `state`, `retry_count`, `last_error`, `bytes`, `updated_at`

- **复合主键** `(chapter_id, img_index)` —— 断点续传的最小定位单位。
- **外键** `chapter_id → download_task(chapter_id) ON DELETE CASCADE`。
  删任务行会连带删掉该章节所有图片行（前提：连接开了 `foreign_keys`）。

---

## 五、索引（4 个）

| 索引 | 表 | 列 | 用途 |
|---|---|---|---|
| `idx_task_state` | `download_task` | `state` | 按状态筛任务 |
| `idx_task_comic` | `download_task` | `comic_id` | 按漫画聚合章节 |
| `idx_task_updated` | `download_task` | `updated_at` | **青龙按完成时间增量拉取**（`migrations.rs:71` 注释） |
| `idx_image_pending` | `download_image` | `(chapter_id, state)` | **恢复时高频查询：某章节还有哪些图没下完**（`migrations.rs:89`） |

---

## 六、表 → Rust 结构体映射

| 表 | 行结构体 | 出处 |
|---|---|---|
| `download_task` | `DbTask` | `store/types.rs:222` |
| `download_image` | `DbImage` | `store/types.rs:240` |

`DbTask` 字段与列一一对应，`state` 在结构体里已是 `DbTaskState` 枚举
（读取时经 `DbTaskState::parse()` 转换，非 serde）。

---

## 七、状态枚举（两层设计）

> **这是本模块最容易踩的设计点**：DB 层与内存层是**两套独立的枚举**，
> 都不走 serde，靠手写的 `as_str()` / `parse()` 转换。

### 7.1 `DbTaskState` —— 持久事实（6 态）

```rust
// src-server/src/store/types.rs:154
#[derive(Debug, Clone, Copy, PartialEq, Eq)]   // ← 无 Serialize/Deserialize
pub enum DbTaskState {
    Pending, Downloading, Paused, Cancelled, Completed, Failed,
}
```

| 枚举 | `as_str()` / DB 存储值 |
|---|---|
| `Pending` | `"pending"` |
| `Downloading` | `"downloading"` |
| `Paused` | `"paused"` |
| `Cancelled` | `"cancelled"` |
| `Completed` | `"completed"` |
| `Failed` | `"failed"` |

- `as_str()`（`types.rs:164`）、`parse()`（`types.rs:175`）**全部手写 match**。
- `parse()` 遇未知值 `bail!("未知的章节任务状态 `{other}`")`。
- `is_terminal()`：`Completed | Cancelled` —— 不会再自动变化。

### 7.2 `DbImageState` —— 图片级（3 态）

```rust
// src-server/src/store/types.rs:196
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbImageState { Pending, Done, Failed }
```

| 枚举 | DB 存储值 |
|---|---|
| `Pending` | `"pending"` |
| `Done` | `"done"` |
| `Failed` | `"failed"` |

源码注释：**刻意只有三态 —— 断点续传只关心「要不要重下」**（`types.rs:193`）。

### 7.3 与内存层 `DownloadTaskState` 的关系

源码注释（`types.rs:150-152`）：
> 与内存里的 `DownloadTaskState` 刻意分开：DB 层的状态是**跨进程的持久事实**，
> 而内存层还要额外承载 `Downloading` 这种「本进程正在跑」的瞬态。

两者转换：`DownloadTaskState::to_db` / `from_db`。

| | `DbTaskState` | `DownloadTaskState`（内存/WS） |
|---|---|---|
| serde | ❌ 手写 `as_str()`/`parse()` | ✅ derive，**无 `rename_all`** |
| 线上格式 | 小写 `"pending"` | PascalCase `"Pending"` |
| 消费方 | 青龙脚本（经 REST） | 前端 UI（经 WS） |
| 出处 | `store/types.rs:154` | `download_manager.rs:51-59` |

> ⚠️ **大小写不一致是 by design，不是 bug**，但两条路径**互不交叉**
> （前端只消费 WS，REST tasks 只服务青龙）。
> 详细论证与接缝警示见 `docs/API.md` 第六节。

> **设计说明**：前端**不消费** `/api/tasks`，任务列表仅由 WebSocket 驱动
> （`download-task-event` 增量 + `task-snapshot-event` 首屏补齐）。
> 这是**有意的设计**，不是遗漏 —— 因此本仓库没有也不需要
> jmcomic 那样的 `src/api/state-adapter.ts`。
>
> ⚠️ **防坑提示**：若将来为前端新增 `/api/tasks` 的 REST 消费，**必须同时
> 引入归一化**（把 REST 小写 state 转成 PascalCase 再写入 store），否则状态
> 比较会静默失效 —— 已完成任务会永远留在「未完成」tab，不报错、类型检查
> 也过。jmcomic 已踩过此坑，见其 `src/api/state-adapter.ts` 顶部注释。

---

## 八、损坏恢复

`Store::open_or_recover()`（`types.rs:60-104`）是**启动路径唯一该用的入口**。

### `Store::open()` 里的检查（`types.rs:42-46`）

```rust
let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
if integrity != "ok" {
    anyhow::bail!("数据库 `{}` 完整性检查未通过：{integrity}", db_path.display());
}
```

注意 `integrity_check` 在**迁移之前**执行。

### 恢复动作（顺序）

1. `tracing::error!` 记录「数据库损坏，将备份旧库并重建」
2. 若 `pica_server.db` 存在 → `rename` 为 `pica_server.db.corrupt-<now_ts()>`
   （`with_extension(format!("db.corrupt-{now_ts}"))`）
3. **删除 `-wal` 与 `-shm` 两个附属文件** —— 注释（`types.rs:87`）明确：
   「否则重建的库会继承旧 WAL」
4. 重新 `Self::open()` 建空库

### 设计取向

源码注释（`types.rs:40-42`）：
> 损坏时**不阻塞启动** —— 下载服务本身的可用性比历史任务记录更重要。

> ⚠️ 代价：**历史任务记录会静默丢失**（只在日志里，不出现在 UI）。
> 损坏的旧库不会被删除，留在数据目录里可人工捞取。

---

## 九、交叉核对基线

改动本文档时，以下数字必须与代码一致：

| 断言 | 值 | 核对命令 |
|---|---|---|
| `SCHEMA_VERSION` | `1` | `grep -n "SCHEMA_VERSION" src-server/src/store/migrations.rs` |
| 表数量 | `2` | `grep -c "CREATE TABLE" src-server/src/store/migrations.rs` |
| `download_task` 列数 | `13` | 见 4.1 列清单 |
| `download_image` 列数 | `8` | 见 4.2 列清单 |
| 索引数量 | `4` | `grep -c "CREATE INDEX" src-server/src/store/migrations.rs` |
| `DbTaskState` 变体 | `6` | `store/types.rs:154` |
| `DbImageState` 变体 | `3` | `store/types.rs:196` |