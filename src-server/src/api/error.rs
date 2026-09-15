//! REST 错误映射。
//!
//! 复用原有的 `CommandError { err_title, err_message }` 结构体作为响应体，
//! 这样前端的错误处理逻辑（读 `err.errTitle` / `err.errMessage`）不需要改动。

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

use crate::errors::CommandError;

/// 包装 `CommandError`，让它能直接从 axum handler 里 `?` 出来。
pub struct ApiError(pub CommandError);

impl From<CommandError> for ApiError {
    fn from(err: CommandError) -> Self {
        Self(err)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // 业务失败统一用 400：前端只关心 body 里的 errTitle/errMessage，
        // 不需要在 HTTP 状态码上做细分。
        (StatusCode::BAD_REQUEST, Json(self.0)).into_response()
    }
}

/// 便捷宏：把 `anyhow::Result<T>` 转成 `Result<T, ApiError>`，
/// 失败时带上标题（与原 `CommandError::from(title, err)` 语义一致）。
#[macro_export]
macro_rules! api_try {
    ($title:expr, $expr:expr) => {
        $expr.map_err(|err| {
            $crate::api::error::ApiError($crate::errors::CommandError::from($title, err))
        })?
    };
}