use std::{io::Write, sync::OnceLock};

use anyhow::Context;
use notify::{RecommendedWatcher, Watcher};
use tracing::{Level, Subscriber};
use tracing_appender::{
    non_blocking::WorkerGuard,
    rolling::{RollingFileAppender, Rotation},
};
use tracing_subscriber::{
    filter::{filter_fn, FilterExt, Targets},
    fmt::{layer, time::LocalTime},
    layer::SubscriberExt,
    registry::LookupSpan,
    util::SubscriberInitExt,
    Layer, Registry,
};

use crate::{
    context::AppContext,
    event_bus::topics,
    events::LogEvent,
    extensions::{AnyhowErrorToStringChain, AppContextExt},
};

struct LogEventWriter {
    app: AppContext,
}

impl Write for LogEventWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let log_string = String::from_utf8_lossy(buf);
        match serde_json::from_str::<LogEvent>(&log_string) {
            Ok(log_event) => {
                // Web 版：日志通过事件总线广播给所有 WebSocket 订阅者
                self.app.events().emit(topics::LOG, &log_event);
            }
            Err(err) => {
                let log_string = log_string.to_string();
                let err_msg = err.to_string();
                tracing::error!(log_string, err_msg, "将日志字符串解析为LogEvent失败");
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

static RELOAD_FN: OnceLock<Box<dyn Fn() -> anyhow::Result<()> + Send + Sync>> = OnceLock::new();
static GUARD: OnceLock<parking_lot::Mutex<Option<WorkerGuard>>> = OnceLock::new();

/// 由环境变量决定日志级别。**默认 INFO。**
///
/// 曾用 TRACE，但每张图片下载都打日志，单日日志会涨到 20GB 撑爆磁盘。
/// INFO 保留关键事件（下载完成/失败/章节状态），足够排查问题；
/// 需要更详细日志时用 `PICA_LOG_LEVEL=TRACE/DEBUG` 覆盖。
///
/// 抽成纯函数是为了可测：这是**磁盘不被撑爆的唯一防线**，
/// 一旦默认值回退成 TRACE，故障要等到磁盘写满才暴露。
fn resolve_level(raw: Option<&str>) -> Level {
    match raw {
        Some("trace") | Some("TRACE") => Level::TRACE,
        Some("debug") | Some("DEBUG") => Level::DEBUG,
        Some("warn") | Some("WARN") => Level::WARN,
        Some("error") | Some("ERROR") => Level::ERROR,
        // 未设置 / 空串 / 拼错的大小写 / 无法识别的值，一律回落到 INFO。
        _ => Level::INFO,
    }
}

pub fn init(app: &AppContext) -> anyhow::Result<()> {
    let lib_module_path = module_path!();
    let lib_target = lib_module_path.split("::").next().context(format!(
        "解析lib_target失败: lib_module_path={lib_module_path}"
    ))?;
    // 过滤掉来自其他库的日志。
    let level = resolve_level(std::env::var("PICA_LOG_LEVEL").ok().as_deref());
    let target_filter = Targets::new().with_target(lib_target, level);
    // 输出到文件
    let (file_layer, guard) = create_file_layer(app)?;
    let (reloadable_file_layer, reload_handle) = tracing_subscriber::reload::Layer::new(file_layer);
    // 输出到控制台
    let console_layer = layer()
        .with_writer(std::io::stdout)
        .with_timer(LocalTime::rfc_3339())
        .with_file(true)
        .with_line_number(true);
    // 发送到前端
    let log_event_writer = std::sync::Mutex::new(LogEventWriter { app: app.clone() });
    let log_event_layer = layer()
        .with_writer(log_event_writer)
        .with_timer(LocalTime::rfc_3339())
        .with_file(true)
        .with_line_number(true)
        .json()
        // 过滤掉来自这个文件的日志(LogEvent解析失败的日志)，避免无限递归
        .with_filter(target_filter.clone().and(filter_fn(|metadata| {
            metadata.module_path() != Some(lib_module_path)
        })));

    Registry::default()
        .with(target_filter)
        .with(reloadable_file_layer)
        .with(console_layer)
        .with(log_event_layer)
        .init();

    GUARD.get_or_init(|| parking_lot::Mutex::new(guard));
    RELOAD_FN.get_or_init(move || {
        let app = app.clone();
        Box::new(move || {
            let (file_layer, guard) = create_file_layer(&app)?;
            reload_handle.reload(file_layer).context("reload失败")?;
            *GUARD.get().context("GUARD未初始化")?.lock() = guard;
            Ok(())
        })
    });
    tokio::spawn(file_log_watcher(app.clone()));

    Ok(())
}

pub fn reload_file_logger() -> anyhow::Result<()> {
    RELOAD_FN.get().context("RELOAD_FN未初始化")?()
}

pub fn disable_file_logger() -> anyhow::Result<()> {
    if let Some(guard) = GUARD.get().context("GUARD未初始化")?.lock().take() {
        drop(guard);
    };
    Ok(())
}

fn create_file_layer<S>(
    app: &AppContext,
) -> anyhow::Result<(Box<dyn Layer<S> + Send + Sync>, Option<WorkerGuard>)>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let enable_file_logger = app.get_config().read().enable_file_logger;
    // 如果不启用文件日志，则返回一个占位用的sink layer，不创建也不输出日志文件
    if !enable_file_logger {
        let sink_layer = layer()
            .with_writer(std::io::sink)
            .with_timer(LocalTime::rfc_3339())
            .with_ansi(false)
            .with_file(true)
            .with_line_number(true);
        return Ok((Box::new(sink_layer), None));
    }
    let logs_dir = logs_dir(app).context("获取日志目录失败")?;
    let file_appender = RollingFileAppender::builder()
        .filename_prefix("picacomic-downloader")
        .filename_suffix("log")
        .rotation(Rotation::DAILY)
        .build(&logs_dir)
        .context("创建RollingFileAppender失败")?;
    let (non_blocking_appender, guard) = tracing_appender::non_blocking(file_appender);
    let file_layer = layer()
        .with_writer(non_blocking_appender)
        .with_timer(LocalTime::rfc_3339())
        .with_ansi(false)
        .with_file(true)
        .with_line_number(true);
    Ok((Box::new(file_layer), Some(guard)))
}

async fn file_log_watcher(app: AppContext) {
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);

    // 关键：必须在当前 async 上下文里先把 Handle 取出来。
    //
    // notify 的 event_handler 闭包运行在 notify 自己创建的后台线程
    // （inotify 事件循环）里，那个线程不在 Tokio 运行时内。若在闭包内部
    // 调用 Handle::current()，会 panic：
    //     there is no reactor running, must be called from the context
    //     of a Tokio 1.x runtime
    // 而 panic 发生在一个独立线程里，不会终止主服务，因此表现为
    // 「服务看着正常，但实时日志功能静默失效」——很难发现。
    let handle = tokio::runtime::Handle::current();

    let event_handler = move |res| {
        // receiver 已关闭（服务正在退出）时 send 会失败，这是正常关闭路径，
        // 不是错误。早期版本在这里 unwrap 会让 notify 线程二次 panic。
        if let Err(err) = handle.block_on(sender.send(res)) {
            tracing::debug!(message = "日志文件 watcher 通道已关闭，忽略事件", error = %err);
        }
    };

    let mut watcher = match RecommendedWatcher::new(event_handler, notify::Config::default())
        .map_err(anyhow::Error::from)
    {
        Ok(watcher) => watcher,
        Err(err) => {
            let err_title = "创建日志文件watcher失败";
            let string_chain = err.to_string_chain();
            tracing::error!(err_title, message = string_chain);
            return;
        }
    };

    let logs_dir = match logs_dir(&app) {
        Ok(logs_dir) => logs_dir,
        Err(err) => {
            let err_title = "日志文件watcher获取日志目录失败";
            let string_chain = err.to_string_chain();
            tracing::error!(err_title, message = string_chain);
            return;
        }
    };

    if let Err(err) = std::fs::create_dir_all(&logs_dir) {
        let err_title = "创建日志目录失败";
        let string_chain = anyhow::Error::from(err).to_string_chain();
        tracing::error!(err_title, message = string_chain);
        return;
    }

    if let Err(err) = watcher
        .watch(&logs_dir, notify::RecursiveMode::NonRecursive)
        .map_err(anyhow::Error::from)
    {
        let err_title = "日志文件watcher监听日志目录失败";
        let string_chain = err.to_string_chain();
        tracing::error!(err_title, message = string_chain);
        return;
    }

    while let Some(res) = receiver.recv().await {
        match res.map_err(anyhow::Error::from) {
            Ok(event) => {
                if let notify::EventKind::Remove(_) = event.kind {
                    if let Err(err) = reload_file_logger() {
                        let err_title = "重置日志文件失败";
                        let string_chain = err.to_string_chain();
                        tracing::error!(err_title, message = string_chain);
                    }
                }
            }
            Err(err) => {
                let err_title = "接收日志文件watcher事件失败";
                let string_chain = err.to_string_chain();
                tracing::error!(err_title, message = string_chain);
            }
        }
    }
}

pub fn logs_dir(app: &AppContext) -> anyhow::Result<std::path::PathBuf> {
    Ok(app.paths().logs_dir())
}

#[cfg(test)]
mod tests {
    use super::resolve_level;
    use tracing::Level;

    // ── 验收标准 5：日志可控 ──────────────────────────────────────
    //
    // 规格是「默认 INFO 下一次完整章节下载 ≤200 行日志」。日志行数取决于
    // 运行时，但**行数的唯一决定因素是这个默认级别**：TRACE 下每张图都会
    // 打日志（曾导致单日 20GB），INFO 下只剩章节级事件。因此这里钉住级别
    // 解析规则，就钉住了日志量级。

    #[test]
    fn level_defaults_to_info_when_env_absent() {
        // 这是磁盘不被撑爆的关键断言：默认值绝不能回退成 TRACE/DEBUG。
        assert_eq!(
            resolve_level(None),
            Level::INFO,
            "未设置 PICA_LOG_LEVEL 时必须默认 INFO"
        );
    }

    #[test]
    fn level_is_overridable_via_env() {
        // 验收标准 5 的后半句：设 PICA_LOG_LEVEL=trace 可恢复详细日志。
        assert_eq!(resolve_level(Some("trace")), Level::TRACE);
        assert_eq!(resolve_level(Some("TRACE")), Level::TRACE);
        assert_eq!(resolve_level(Some("debug")), Level::DEBUG);
        assert_eq!(resolve_level(Some("DEBUG")), Level::DEBUG);
        assert_eq!(resolve_level(Some("warn")), Level::WARN);
        assert_eq!(resolve_level(Some("error")), Level::ERROR);
    }

    #[test]
    fn level_unknown_values_fall_back_to_info() {
        // 拼错的大小写、空串、无法识别的值都不能意外放开到 TRACE。
        for raw in ["", " ", "Trace", "TRace", "verbose", "3", "off", "info"] {
            assert_eq!(
                resolve_level(Some(raw)),
                Level::INFO,
                "无法识别的 PICA_LOG_LEVEL={raw:?} 必须回落到 INFO"
            );
        }
    }

    #[test]
    fn level_never_defaults_to_trace() {
        // 回归防线：历史上默认 TRACE 曾让单日日志涨到 20GB。
        assert_ne!(resolve_level(None), Level::TRACE);
        for raw in ["", "Trace", "tr", "ttrace"] {
            assert_ne!(
                resolve_level(Some(raw)),
                Level::TRACE,
                "只有精确的 trace/TRACE 才允许放开到 TRACE，实际输入 {raw:?}"
            );
        }
    }
}
