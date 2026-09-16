//! 极简认证。
//!
//! 目标是单用户 NAS 自部署场景，因此不做账号体系、不做 session 存储：
//! 服务启动时从环境变量读一个固定 Token（没配就随机生成并打印到日志），
//! 之后所有请求必须带上它。同时兼容两种常见携带方式：
//!
//! - `Authorization: Bearer <token>`（前端 `fetch` 用这个）
//! - HTTP Basic Auth（浏览器直接访问 / 反向代理弹窗用这个）
//!
//! 多用户隔离明确不在本期范围内。

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::Engine as _;

/// 认证配置。
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// 访问后台所需的固定 Token。
    pub token: Arc<str>,
    /// Basic Auth 的用户名。默认 `admin`。
    pub username: Arc<str>,
    /// 是否关闭认证。由 `PICA_AUTH_DISABLED` 控制，为 true 时所有请求直接放行。
    pub disabled: bool,
}

impl AuthConfig {
    /// 从环境变量读取。
    ///
    /// - `PICA_AUTH_TOKEN`：不设置则随机生成（返回值里的 `generated` 为 `true`）。
    /// - `PICA_AUTH_USER`：默认 `admin`。
    pub fn from_env() -> (Self, bool) {
        let (token, generated) = match std::env::var("PICA_AUTH_TOKEN") {
            Ok(token) if !token.trim().is_empty() => (token, false),
            _ => (random_token(), true),
        };
        let username =
            std::env::var("PICA_AUTH_USER").unwrap_or_else(|_| String::from("admin"));

        // PICA_AUTH_DISABLED=true/1 时关闭认证，所有请求直接放行。
        let disabled = matches!(
            std::env::var("PICA_AUTH_DISABLED").as_deref(),
            Ok("true") | Ok("1") | Ok("TRUE") | Ok("True")
        );

        (
            Self {
                token: Arc::from(token.as_str()),
                username: Arc::from(username.as_str()),
                disabled,
            },
            generated,
        )
    }

    /// 校验一个候选 Token 是否正确。用固定时间比较，避免时序侧信道。
    fn token_matches(&self, candidate: &str) -> bool {
        let expected = self.token.as_bytes();
        let candidate = candidate.as_bytes();
        if expected.len() != candidate.len() {
            return false;
        }
        // 长度已经相等，这里逐字节异或累加，不提前 return。
        expected
            .iter()
            .zip(candidate)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    }

    /// 校验 Basic Auth 的 `user:pass`。密码即 Token。
    fn basic_matches(&self, decoded: &str) -> bool {
        let Some((user, pass)) = decoded.split_once(':') else {
            return false;
        };
        user == self.username.as_ref() && self.token_matches(pass)
    }
}

/// 生成一个随机 Token。没有 `rand` 之外的依赖，够用。
fn random_token() -> String {
    use rand::Rng as _;
    let mut rng = rand::thread_rng();
    (0..32)
        .map(|_| {
            let idx = rng.gen_range(0..36);
            char::from_digit(idx, 36).unwrap_or('0')
        })
        .collect()
}

/// 无需认证的端点（相对于 API 挂载点的路径）。
///
/// 注意这里是**挂载后的相对路径**：`routes::router` 会被 `main.rs`
/// 用 `.nest("/api", ..)` 挂上去，而 axum 的 `nest` 会在进入内层路由
/// 之前把前缀剥掉，中间件看到的 `uri().path()` 是 `/health` 而不是
/// `/api/health`。所以匹配必须用相对路径。
const PUBLIC_PATHS: [&str; 2] = ["/health", "/auth/check"];

/// axum 中间件：拒绝未认证请求。
///
/// 放行规则：
/// 1. `/health` 永远放行（给 Docker healthcheck 用）。
/// 2. `/auth/check` 放行，用于前端探测自己是否已登录。
/// 3. 其余路径一律必须带合法凭证。
///
/// 这里**只按相对路径判断**，不依赖外层挂载前缀，也不会因为将来
/// 改了挂载点就静默失效。静态资源不经过这个中间件（它在 `main.rs`
/// 里是 `/api` 之外的另一条 fallback 分支）。
pub async fn require_auth(
    State(auth): State<AuthConfig>,
    req: Request,
    next: Next,
) -> Response {
    // 认证被显式关闭时（PICA_AUTH_DISABLED），所有请求直接放行。
    if auth.disabled {
        return next.run(req).await;
    }

    let path = req.uri().path();

    // 健康检查与登录态探测不需要认证。
    if PUBLIC_PATHS.contains(&path) {
        return next.run(req).await;
    }

    if let Some(credential) = extract_credential(&req) {
        let ok = match credential {
            Credential::Bearer(token) => auth.token_matches(token),
            Credential::Basic(decoded) => auth.basic_matches(&decoded),
        };
        if ok {
            return next.run(req).await;
        }
    }

    // 兜底：`?token=xxx`。浏览器 WebSocket 发不了请求头，只能走这里。
    if let Some(token) = extract_query_token(&req) {
        if auth.token_matches(token) {
            return next.run(req).await;
        }
    }

    unauthorized()
}

enum Credential<'a> {
    Bearer(&'a str),
    Basic(String),
}

/// 从请求头里取出凭证。Bearer 优先于 Basic。
fn extract_credential(req: &Request) -> Option<Credential<'_>> {
    let value = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;

    if let Some(token) = value.strip_prefix("Bearer ") {
        return Some(Credential::Bearer(token.trim()));
    }

    if let Some(encoded) = value.strip_prefix("Basic ") {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .ok()?;
        let decoded = String::from_utf8(decoded).ok()?;
        return Some(Credential::Basic(decoded));
    }

    None
}

/// 从 URL 查询串里取 Token：`?token=xxx`。
///
/// 浏览器的 `WebSocket` 构造函数**无法携带自定义请求头**，所以
/// `/api/ws` 不能走 `Authorization`。这是 WebSocket 协议本身的限制，
/// 不是偷懒——所有需要鉴权的浏览器 WS 都是这么做的。
///
/// 安全权衡：查询串可能出现在反向代理的访问日志里。对单用户自部署的
/// NAS 场景可以接受；若要进一步收紧，应在反代层关闭对 `/api/ws` 的
/// query 记录，或改用一次性票据（不在本期范围）。
fn extract_query_token(req: &Request) -> Option<&str> {
    let query = req.uri().query()?;
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix("token=") {
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// 统一的 401 响应。带 `WWW-Authenticate` 让浏览器弹出登录框。
fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"pica-server\"")],
        axum::Json(serde_json::json!({
            "errTitle": "未认证",
            "errMessage": "缺少或无效的访问凭证，请在请求头带上 Authorization: Bearer <token>",
        })),
    )
        .into_response()
}