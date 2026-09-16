//! picacomic-downloader Web 服务端
//!
//! 该 crate 把原桌面版（Tauri）的下载核心逻辑剥离出来，改造成一个独立的
//! HTTP + WebSocket 服务，方便在 NAS 上以 Docker 方式部署，通过网页后台控制。

pub mod api;
pub mod auth;
pub mod config;
pub mod context;
pub mod download_manager;
pub mod errors;
pub mod event_bus;
pub mod events;
pub mod extensions;
pub mod logger;
pub mod pica_client;
pub mod responses;
pub mod store;
pub mod types;
pub mod utils;