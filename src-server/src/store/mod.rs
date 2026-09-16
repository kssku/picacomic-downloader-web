//! 持久化层：把下载任务状态从进程内存搬到 SQLite。
//!
//! 这一层的存在是为了解决两个根本问题：
//!
//! 1. **D1 任务状态纯内存** —— 容器一重启，所有 `Pending` / `Downloading`
//!    任务凭空消失，用户不知道哪些下了哪些没下。
//! 2. **D3 章节完整性判定过粗** —— 原先靠「已下载张数 == 总张数」判断章节是否
//!    成功，导致单张图超时要整章重下。有了图片级的 `download_image` 表，
//!    恢复时只需要重下 `state != 'done'` 的图片。
//!
//! 设计约束：
//!
//! - **不引入外部组件**。NAS 上多一个 Redis / 消息队列就多一个故障点，
//!   而 SQLite 已经在青龙侧跑着，运维熟悉。
//! - **单写连接 + WAL**。章节并发 3 + 图片并发 20，写操作全部集中在状态迁移点，
//!   不在图片下载热路径上逐张写盘。
//! - **数据库文件独立**（`PICA_DATA_DIR/pica_server.db`），**不与青龙的
//!   `bica_comics.db` 混用**，避免两套 schema 互相干扰。

pub mod migrations;
pub mod repo;
pub mod types;

pub use repo::{ImageRepo, TaskRepo, TaskStats};
pub use types::{DbImage, DbImageState, DbTask, DbTaskState, Store};