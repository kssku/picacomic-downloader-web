//! pica-server 入口。
//!
//! 职责：
//! 1. 初始化路径 / 配置 / 日志 / 运行期组件（`AppContext`）。
//! 2. 组装 axum Router：REST API + WebSocket + 前端静态资源。
//! 3. 监听 HTTP 端口常驻。
//!
//! 环境变量：
//! - `PICA_DATA_DIR`  数据根目录，默认 `./data`。Docker 里挂到 `/data`。
//! - `PICA_PORT`      监听端口，默认 `8080`。
//! - `PICA_BIND`      监听地址，默认 `0.0.0.0`。
//! - `PICA_AUTH_TOKEN` 访问令牌，不设则随机生成并打印。
//! - `PICA_AUTH_USER`  Basic Auth 用户名，默认 `admin`。
//! - `PICA_STATIC_DIR` 前端静态资源目录，默认 `./dist`。

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Context as _;
use axum::Router;
use pica_server::api::routes;
use pica_server::auth::AuthConfig;
use pica_server::context::{AppContext, Paths};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let paths = Paths::from_env().context("初始化路径失败")?;
    let app = AppContext::new(paths).context("加载配置失败")?;

    // 日志系统需要 AppContext 才能拿到日志目录，因此放在上下文之后。
    pica_server::logger::init(&app).context("初始化日志失败")?;

    app.init_runtime().context("初始化运行期组件失败")?;

    // 返回的 bool 表示 Token 是否为随机生成（true = 没配环境变量）。
    let (auth, token_generated) = AuthConfig::from_env();
    let (bind, port) = bind_addr();
    let static_dir = static_dir();

    let router = build_router(app.clone(), auth.clone(), &static_dir);

    let addr: SocketAddr = format!("{bind}:{port}")
        .parse()
        .with_context(|| format!("解析监听地址 `{bind}:{port}` 失败"))?;

    // 随机生成的 Token 只在这里打印一次，之后不再出现在任何日志里。
    print_startup_banner(&addr, &auth, token_generated, &static_dir, &app);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("监听 `{addr}` 失败"))?;

    tracing::info!(%addr, "HTTP 服务已启动");

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("HTTP 服务异常退出")?;

    tracing::info!("HTTP 服务已停止");
    Ok(())
}

/// 组装全部路由。
///
/// 层级顺序（从外到内）：
/// 1. `TraceLayer` —— 请求日志。
/// 2. 静态资源 fallback —— 非 `/api/*` 的路径交给前端 SPA。
/// 3. `/api/*` 路由（内部自带认证中间件）。
fn build_router(app: AppContext, auth: AuthConfig, static_dir: &PathBuf) -> Router {
    // SPA fallback：找不到的路径一律回 index.html，让前端路由处理。
    //
    // 这里必须用 `.fallback(ServeFile)` 而不是 `.not_found_service(ServeFile)`。
    // 在 tower-http 0.5 下，`not_found_service` 只在 ServeDir 自身判定 404 时
    // 才被调用，像 `/some/spa/route` 这种既不是文件、又不是目录的路径会
    // 直接透出 404，SPA 路由全部失效（实测确认）。`.fallback` 才能兜住。
    //
    // 反例警告：不要写成两个 `fallback_service(...)` 串联——
    // 后者会**整个替换**前者，导致真实 JS/CSS 资源也被替换成 index.html。
    let index = static_dir.join("index.html");
    let serve_dir = ServeDir::new(static_dir).fallback(ServeFile::new(index));

    // `routes::router` 已经带好 state 与认证中间件（含 `/ws`）。
    let api = Router::new().nest("/api", routes::router(app, auth));

    api.fallback_service(serve_dir)
        .layer(TraceLayer::new_for_http())
}

/// 监听地址，来自 `PICA_BIND` / `PICA_PORT`。
fn bind_addr() -> (String, u16) {
    let bind = std::env::var("PICA_BIND").unwrap_or_else(|_| String::from("0.0.0.0"));
    let port = std::env::var("PICA_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8080);
    (bind, port)
}

/// 前端静态资源目录。
fn static_dir() -> PathBuf {
    std::env::var("PICA_STATIC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./dist"))
}

/// 启动横幅。随机 Token 只在这里出现一次。
fn print_startup_banner(
    addr: &SocketAddr,
    auth: &AuthConfig,
    token_generated: bool,
    static_dir: &PathBuf,
    app: &AppContext,
) {
    let data_dir = app.paths().data_dir.display().to_string();

    println!();
    println!("  ┌─────────────────────────────────────────────────────────┐");
    println!("  │  pica-server  已启动                                    │");
    println!("  └─────────────────────────────────────────────────────────┘");
    println!("    访问地址   http://{addr}/");
    println!("    数据目录   {data_dir}");
    println!("    静态资源   {}", static_dir.display());
    println!("    用户名     {}", auth.username);
    if token_generated {
        println!("    访问令牌   {}  （随机生成，请立即保存）", auth.token);
    } else {
        println!("    访问令牌   来自环境变量 PICA_AUTH_TOKEN");
    }
    println!();
    println!("    首次登录：在网页里填入上面的「访问令牌」即可。");
    println!();
}

/// Ctrl-C 或 SIGTERM（Docker stop 发的是 SIGTERM）时优雅退出。
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(err) => {
                tracing::warn!(%err, "注册 SIGTERM 处理器失败");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("收到 Ctrl-C，开始优雅退出"),
        _ = terminate => tracing::info!("收到 SIGTERM，开始优雅退出"),
    }
}