use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context};
use bytes::Bytes;
use chrono::Local;
use hmac::{Hmac, Mac};
use image::ImageFormat;
use parking_lot::RwLock;
use reqwest_middleware::ClientWithMiddleware;
use reqwest_retry::policies::ExponentialBackoff;
use reqwest_retry::{Jitter, RetryTransientMiddleware};
use reqwest::StatusCode;
use serde_json::json;
use sha2::Sha256;

use crate::config::ProxyMode;
use crate::context::AppContext;
use crate::extensions::{AnyhowErrorToStringChain, AppContextExt};
use crate::responses::{
    ChapterImageRespData, ChapterRespData, ComicRespData, GetChapterImageRespData,
    GetChapterRespData, GetComicRespData, GetFavoriteRespData, LoginRespData, Pagination, PicaResp,
    SearchRespData, UserProfileDetailRespData, UserProfileRespData,
};
use crate::types::{GetFavoriteSort, SearchSort};

const API_KEY: &str = "C69BAF41DA5ABD1FFEDC6D2FEA56B";
const NONCE: &str = "ptxdhmjzqtnrtwndhbxcpkjamb33w837";
const DIGEST_KEY: &str = r"~d}$Q7$eIni=V)9\RK/P.RM4;9[7|@/CA}b~OW!3?EV`:<>M7pddUBL5n|0/*Cn";

/// `api_client` 的请求总超时。
///
/// 早期设 2 秒。在 NAS + 代理链路下这个值太紧：获取章节图片链接时会并发
/// 拉起全部页（几十个请求），任一页在 2s 内没回就被判失败，整章随之失败。
/// 实测经代理单次约 0.8–0.9s，2s 属边际易崩。放宽到 15s，配合下面的重试
/// 预算，覆盖偶发抖动。
const API_REQUEST_TIMEOUT_SECS: u64 = 15;

/// `api_client` 的重试总时长预算。
///
/// 必须大于单次请求超时，否则一次请求还没超时、预算就耗尽，重试没有机会发生。
const API_RETRY_TOTAL_SECS: u64 = 30;

#[derive(Clone)]
pub struct PicaClient {
    app: AppContext,
    api_client: Arc<RwLock<ClientWithMiddleware>>,
    img_client: Arc<RwLock<ClientWithMiddleware>>,
    base_url: Arc<RwLock<String>>,
}

impl PicaClient {
    pub fn new(app: AppContext) -> Self {
        let base_url = app.get_config().read().api_base_url.clone();

        let api_client = create_api_client(&app);
        let img_client = create_img_client(&app);

        Self {
            app,
            api_client: Arc::new(RwLock::new(api_client)),
            img_client: Arc::new(RwLock::new(img_client)),
            base_url: Arc::new(RwLock::new(base_url)),
        }
    }

    pub fn reload_client(&self) {
        let api_client = create_api_client(&self.app);
        *self.api_client.write() = api_client;
        let img_client = create_img_client(&self.app);
        *self.img_client.write() = img_client;

        let base_url = self.app.get_config().read().api_base_url.clone();
        *self.base_url.write() = base_url;
    }

    pub fn base_url(&self) -> String {
        self.base_url.read().clone()
    }

    async fn pica_request(
        &self,
        method: reqwest::Method,
        path: &str,
        payload: Option<serde_json::Value>,
    ) -> anyhow::Result<reqwest::Response> {
        let time = Local::now().timestamp().to_string();
        let signature = create_signature(path, &method, &time)?;
        let token = self.app.get_config().read().token.clone();

        let base_url = self.base_url.read().clone();
        // base_url 与 path 之间必须恰好有一个斜杠。
        //
        // 不能简单写 format!("{base_url}{path}")：那样要求 base_url 必须以
        // "/" 结尾、且 path 不能以 "/" 开头，全凭配置自觉。实际踩到的坑是
        // 用户把 apiBaseUrl 配成 "https://picaapi.go2778.com"（无结尾斜杠），
        // 拼出 "...go2778.comusers/profile" 这种畸形 URL——某些反代会接受
        // 并返回 422，看起来像"签名错误"，排查方向完全被带偏。
        // 这里统一规整，不依赖配置的书写习惯。
        let url = format!("{}/{}", base_url.trim_end_matches('/'), path.trim_start_matches('/'));

        let request = self
            .api_client
            .read()
            .request(method.clone(), &url)
            .header("api-key", API_KEY)
            .header("accept", "application/vnd.picacomic.com.v1+json")
            .header("app-channel", "2")
            .header("time", time)
            .header("nonce", NONCE)
            .header("app-version", "2.2.1.2.3.3")
            .header("app-uuid", "defaultUuid")
            .header("app-platform", "android")
            .header("app-build-version", "44")
            .header("Content-Type", "application/json; charset=UTF-8")
            .header("User-Agent", "okhttp/3.8.1")
            .header("authorization", token)
            .header("image-quality", "original")
            .header("signature", signature);

        let http_resp = match payload {
            Some(body) => request.json(&body).send().await,
            None => request.send().await,
        }
        .map_err(|e| {
            if e.is_timeout() {
                anyhow::Error::from(e).context("连接超时，请使用代理或换条线路重试")
            } else {
                anyhow::Error::from(e)
            }
        })?;

        Ok(http_resp)
    }

    pub async fn login(&self, email: &str, password: &str) -> anyhow::Result<String> {
        let payload = json!({
            "email": email,
            "password": password,
        });
        let http_resp = self.pica_request(reqwest::Method::POST, "auth/sign-in", Some(payload)).await?;

        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status == StatusCode::BAD_REQUEST {
            return Err(anyhow!("用户名或密码错误({status}): {body}"));
        } else if status != StatusCode::OK {
            return Err(anyhow!("预料之外的状态码({status}): {body}"));
        }

        let pica_resp = serde_json::from_str::<PicaResp>(&body)
            .context(format!("将body解析为PicaResp失败: {body}"))?;
        if pica_resp.code != 200 {
            return Err(anyhow!("预料之外的code: {pica_resp:?}"));
        }
        let Some(data) = pica_resp.data else {
            return Err(anyhow!("data字段不存在: {pica_resp:?}"));
        };
        let data_str = data.to_string();
        let login_resp_data = serde_json::from_str::<LoginRespData>(&data_str)
            .context(format!("将data解析为LoginRespData失败: {data_str}"))?;

        Ok(login_resp_data.token)
    }

    pub async fn get_user_profile(&self) -> anyhow::Result<UserProfileDetailRespData> {
        let http_resp = self.pica_request(reqwest::Method::GET, "users/profile", None).await?;

        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status == StatusCode::UNAUTHORIZED {
            return Err(anyhow!("Authorization无效或已过期，请重新登录({status}): {body}"));
        } else if status != StatusCode::OK {
            return Err(anyhow!("预料之外的状态码({status}): {body}"));
        }

        let pica_resp = serde_json::from_str::<PicaResp>(&body)
            .context(format!("将body解析为PicaResp失败: {body}"))?;
        if pica_resp.code != 200 {
            return Err(anyhow!("预料之外的code: {pica_resp:?}"));
        }
        let Some(data) = pica_resp.data else {
            return Err(anyhow!("data字段不存在: {pica_resp:?}"));
        };
        let data_str = data.to_string();
        let user_profile_resp_data = serde_json::from_str::<UserProfileRespData>(&data_str)
            .context(format!("将data解析为UserProfileRespData失败: {data_str}"))?;

        Ok(user_profile_resp_data.user)
    }

    pub async fn search_comic(
        &self,
        keyword: &str,
        sort: SearchSort,
        page: i32,
        categories: Vec<String>,
    ) -> anyhow::Result<SearchRespData> {
        let payload = json!({
            "keyword": keyword,
            "sort": sort.as_str(),
            "categories": categories,
        });
        let path = format!("comics/advanced-search?page={page}");
        let http_resp = self.pica_request(reqwest::Method::POST, &path, Some(payload)).await?;

        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status == StatusCode::UNAUTHORIZED {
            return Err(anyhow!("Authorization无效或已过期，请重新登录({status}): {body}"));
        } else if status != StatusCode::OK {
            return Err(anyhow!("预料之外的状态码({status}): {body}"));
        }

        let pica_resp = serde_json::from_str::<PicaResp>(&body)
            .context(format!("将body解析为PicaResp失败: {body}"))?;
        if pica_resp.code != 200 {
            return Err(anyhow!("预料之外的code: {pica_resp:?}"));
        }
        let Some(data) = pica_resp.data else {
            return Err(anyhow!("data字段不存在: {pica_resp:?}"));
        };
        let data_str = data.to_string();
        let search_resp_data = serde_json::from_str::<SearchRespData>(&data_str)
            .context(format!("将data解析为SearchRespData失败: {data_str}"))?;

        Ok(search_resp_data)
    }

    pub async fn get_comic(&self, comic_id: &str) -> anyhow::Result<ComicRespData> {
        let path = format!("comics/{comic_id}");
        let http_resp = self.pica_request(reqwest::Method::GET, &path, None).await?;

        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status == StatusCode::UNAUTHORIZED {
            return Err(anyhow!("Authorization无效或已过期，请重新登录({status}): {body}"));
        } else if status != StatusCode::OK {
            return Err(anyhow!("预料之外的状态码({status}): {body}"));
        }

        let pica_resp = serde_json::from_str::<PicaResp>(&body)
            .context(format!("将body解析为PicaResp失败: {body}"))?;
        if pica_resp.code != 200 {
            return Err(anyhow!("预料之外的code: {pica_resp:?}"));
        }
        let Some(data) = pica_resp.data else {
            return Err(anyhow!("data字段不存在: {pica_resp:?}"));
        };
        let data_str = data.to_string();
        let get_comic_resp_data = serde_json::from_str::<GetComicRespData>(&data_str)
            .context(format!("将data解析为GetComicRespData失败: {data_str}"))?;

        Ok(get_comic_resp_data.comic)
    }

    pub async fn get_chapter(
        &self,
        comic_id: &str,
        page: i64,
    ) -> anyhow::Result<Pagination<ChapterRespData>> {
        let path = format!("comics/{comic_id}/eps?page={page}");
        let http_resp = self.pica_request(reqwest::Method::GET, &path, None).await?;

        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status == StatusCode::UNAUTHORIZED {
            return Err(anyhow!("Authorization无效或已过期，请重新登录({status}): {body}"));
        } else if status != StatusCode::OK {
            return Err(anyhow!("预料之外的状态码({status}): {body}"));
        }

        let pica_resp = serde_json::from_str::<PicaResp>(&body)
            .context(format!("将body解析为PicaResp失败: {body}"))?;
        if pica_resp.code != 200 {
            return Err(anyhow!("预料之外的code: {pica_resp:?}"));
        }
        let Some(data) = pica_resp.data else {
            return Err(anyhow!("data字段不存在: {pica_resp:?}"));
        };
        let data_str = data.to_string();
        let get_chapter_resp_data = serde_json::from_str::<GetChapterRespData>(&data_str)
            .context(format!("将data解析为GetChapterRespData失败: {data_str}"))?;

        Ok(get_chapter_resp_data.eps)
    }

    pub async fn get_chapter_img(
        &self,
        comic_id: &str,
        chapter_order: i64,
        page: i64,
    ) -> anyhow::Result<Pagination<ChapterImageRespData>> {
        let path = format!("comics/{comic_id}/order/{chapter_order}/pages?page={page}");
        let http_resp = self.pica_request(reqwest::Method::GET, &path, None).await?;

        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status == StatusCode::UNAUTHORIZED {
            return Err(anyhow!("Authorization无效或已过期，请重新登录({status}): {body}"));
        } else if status != StatusCode::OK {
            return Err(anyhow!("预料之外的状态码({status}): {body}"));
        }

        let pica_resp = serde_json::from_str::<PicaResp>(&body)
            .context(format!("将body解析为PicaResp失败: {body}"))?;
        if pica_resp.code != 200 {
            return Err(anyhow!("预料之外的code: {pica_resp:?}"));
        }
        let Some(data) = pica_resp.data else {
            return Err(anyhow!("data字段不存在: {pica_resp:?}"));
        };
        let data_str = data.to_string();
        let get_chapter_image_resp_data = serde_json::from_str::<GetChapterImageRespData>(&data_str)
            .context(format!("将data解析为GetChapterImageRespData失败: {data_str}"))?;

        Ok(get_chapter_image_resp_data.pages)
    }

    pub async fn get_favorite(
        &self,
        sort: GetFavoriteSort,
        page: i64,
    ) -> anyhow::Result<GetFavoriteRespData> {
        let sort = sort.as_str();
        let path = format!("users/favourite?s={sort}&page={page}");
        let http_resp = self.pica_request(reqwest::Method::GET, &path, None).await?;

        let status = http_resp.status();
        let body = http_resp.text().await?;
        if status == StatusCode::UNAUTHORIZED {
            return Err(anyhow!("Authorization无效或已过期，请重新登录({status}): {body}"));
        } else if status != StatusCode::OK {
            return Err(anyhow!("预料之外的状态码({status}): {body}"));
        }

        let pica_resp: PicaResp = serde_json::from_str(&body)
            .context(format!("将body解析为PicaResp失败: {body}"))?;
        if pica_resp.code != 200 {
            return Err(anyhow!("预料之外的code: {pica_resp:?}"));
        }
        let Some(data) = pica_resp.data else {
            return Err(anyhow!("data字段不存在: {pica_resp:?}"));
        };
        let data_str = data.to_string();
        let get_favorite_resp_data = serde_json::from_str::<GetFavoriteRespData>(&data_str)
            .context(format!("将data解析为GetFavoriteRespData失败: {data_str}"))?;

        Ok(get_favorite_resp_data)
    }

    pub async fn get_img_data_and_format(&self, url: &str) -> anyhow::Result<(Bytes, ImageFormat)> {
        let request = self.img_client.read().get(url);
        let http_resp = request.send().await?;

        let status = http_resp.status();
        if status != StatusCode::OK {
            let text = http_resp.text().await?;
            let err = anyhow!("下载图片`{url}`失败，预料之外的状态码: {text}");
            return Err(err);
        }
        let image_data = http_resp.bytes().await?;

        // 老漫画的图片 URL 可能已失效，服务器会返回 HTML 错误页而非图片。
        // 这里提前识别，给出明确错误，而不是让 image 库报模糊的"格式不支持"。
        if looks_like_html(&image_data) {
            anyhow::bail!(
                "图片 URL 返回了 HTML 页面（该图片可能已失效或下架）: {url}"
            );
        }

        let format = image::guess_format(&image_data)
            .context("无法从图片数据中猜测出图片格式，可能图片数据不完整或已损坏")?;

        Ok((image_data, format))
    }
}

// ===================== 辅助函数 =====================

/// 判断响应体是不是 HTML（而非图片）。
///
/// 失效的图片 URL 常返回 `<!DOCTYPE html>` 或 `<html>` 错误页。
/// 用宽松的前缀匹配，能覆盖常见的错误页形态。
fn looks_like_html(data: &[u8]) -> bool {
    // 跳过前导空白
    let head: Vec<u8> = data
        .iter()
        .skip_while(|b| b.is_ascii_whitespace())
        .take(32)
        .map(|b| b.to_ascii_lowercase())
        .collect();
    head.starts_with(b"<!doctype") || head.starts_with(b"<html")
}

fn create_signature(path: &str, method: &reqwest::Method, time: &str) -> anyhow::Result<String> {
    let method = method.as_str();
    let data = format!("{path}{time}{NONCE}{method}{API_KEY}").to_lowercase();
    let signature = hmac_hex(DIGEST_KEY, &data)?;
    Ok(signature)
}

fn hmac_hex(key: &str, data: &str) -> anyhow::Result<String> {
    let key = key.as_bytes();
    let mut mac = Hmac::<Sha256>::new_from_slice(key)?;
    mac.update(data.as_bytes());
    let result = hex::encode(mac.finalize().into_bytes().as_slice());
    Ok(result)
}

pub fn create_api_client(app: &AppContext) -> ClientWithMiddleware {
    let retry_policy = ExponentialBackoff::builder()
        .base(1)
        .jitter(Jitter::Bounded)
        .build_with_total_retry_duration(Duration::from_secs(API_RETRY_TOTAL_SECS));

    let client = reqwest::ClientBuilder::new()
        .timeout(Duration::from_secs(API_REQUEST_TIMEOUT_SECS))
        .set_proxy(app, "api_client")
        .build()
        .unwrap();

    reqwest_middleware::ClientBuilder::new(client)
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build()
}

fn create_img_client(app: &AppContext) -> ClientWithMiddleware {
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(3);

    let client = reqwest::ClientBuilder::new()
        .set_proxy(app, "img_client")
        .build()
        .unwrap();

    reqwest_middleware::ClientBuilder::new(client)
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build()
}

/// 读取环境变量中的代理设置（`HTTPS_PROXY` / `https_proxy` / `ALL_PROXY` / `all_proxy`）。
///
/// reqwest 在 `default-features = false` 时不会启用 `system-proxy` feature，
/// 因此 `Proxy::system()` 不会被调用，环境变量代理会被静默忽略。
/// 这里手动读取，保证 `ProxyMode::System` 与 curl / 容器环境变量的行为一致。
fn system_proxy_url() -> Option<String> {
    const VARS: [&str; 4] = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"];
    VARS.iter().find_map(|var| {
        std::env::var(var)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

trait ClientBuilderExt {
    fn set_proxy(self, app: &AppContext, client_name: &str) -> Self;
}

impl ClientBuilderExt for reqwest::ClientBuilder {
    fn set_proxy(self, app: &AppContext, client_name: &str) -> reqwest::ClientBuilder {
        let proxy_mode = app.get_config().read().proxy_mode;
        match proxy_mode {
            ProxyMode::System => match system_proxy_url() {
                Some(proxy_url) => match reqwest::Proxy::all(&proxy_url).map_err(anyhow::Error::from) {
                    Ok(proxy) => {
                        tracing::info!(client_name, proxy_url, "使用环境变量代理");
                        self.proxy(proxy)
                    }
                    Err(err) => {
                        let err_title = format!("{client_name}将`{proxy_url}`设为代理失败，将直连");
                        let string_chain = err.to_string_chain();
                        tracing::error!(err_title, message = string_chain);
                        self.no_proxy()
                    }
                },
                None => self.no_proxy(),
            },
            ProxyMode::NoProxy => self.no_proxy(),
            ProxyMode::Custom => {
                let config = app.get_config().read();
                let proxy_host = &config.proxy_host;
                let proxy_port = &config.proxy_port;
                let proxy_url = format!("http://{proxy_host}:{proxy_port}");

                match reqwest::Proxy::all(&proxy_url).map_err(anyhow::Error::from) {
                    Ok(proxy) => self.proxy(proxy),
                    Err(err) => {
                        let err_title = format!("{client_name}将`{proxy_url}`设为代理失败，将直连");
                        let string_chain = err.to_string_chain();
                        tracing::error!(err_title, message = string_chain);
                        self.no_proxy()
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{API_REQUEST_TIMEOUT_SECS, API_RETRY_TOTAL_SECS, system_proxy_url};
    use std::sync::Mutex;

    // 环境变量是进程级全局状态，串行化这些测试避免相互干扰。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const ALL_VARS: [&str; 4] = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"];

    fn clear_proxy_vars() {
        for var in ALL_VARS {
            std::env::remove_var(var);
        }
    }

    #[test]
    fn system_proxy_url_is_none_when_no_env_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_proxy_vars();

        assert_eq!(system_proxy_url(), None);
    }

    #[test]
    fn system_proxy_url_prefers_https_uppercase() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_proxy_vars();

        std::env::set_var("https_proxy", "http://lowercase:7890");
        std::env::set_var("HTTPS_PROXY", "http://uppercase:7890");

        // 这是 NAS 上的真实场景：容器同时设置了大小写两种变量。
        assert_eq!(
            system_proxy_url(),
            Some("http://uppercase:7890".to_string())
        );

        clear_proxy_vars();
    }

    #[test]
    fn system_proxy_url_falls_back_to_lowercase_and_all_proxy() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_proxy_vars();

        std::env::set_var("https_proxy", "http://lowercase:7890");
        assert_eq!(
            system_proxy_url(),
            Some("http://lowercase:7890".to_string())
        );

        clear_proxy_vars();
        std::env::set_var("all_proxy", "http://all:7890");
        assert_eq!(system_proxy_url(), Some("http://all:7890".to_string()));

        clear_proxy_vars();
    }

    #[test]
    fn system_proxy_url_ignores_blank_and_trims_whitespace() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_proxy_vars();

        // 空白值不应被当成有效代理（reqwest 的 insert_proxy 也是同样语义）。
        std::env::set_var("HTTPS_PROXY", "   ");
        assert_eq!(system_proxy_url(), None);

        std::env::set_var("HTTPS_PROXY", "  http://proxy:7890  ");
        assert_eq!(
            system_proxy_url(),
            Some("http://proxy:7890".to_string())
        );

        clear_proxy_vars();
    }

    #[test]
    fn system_proxy_url_is_parseable_by_reqwest() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_proxy_vars();

        // 关键回归点：环境变量里的值必须能被 reqwest::Proxy::all 接受，
        // 否则 System 模式会退化成直连并导致 tls handshake eof。
        std::env::set_var("HTTPS_PROXY", "http://host.docker.internal:7890");

        let url = system_proxy_url().expect("应从环境变量读取到代理");
        assert!(
            reqwest::Proxy::all(&url).is_ok(),
            "`{url}` 应能被 reqwest 解析为代理"
        );

        clear_proxy_vars();
    }

    // 回归：api_client 的超时预算必须自洽。
    //
    // 早期单次超时是 2s，在「并发拉取全部页 + 代理」场景下过紧，
    // 任意一页抖动就会让整章在链接阶段失败。这里把下限钉死，防止
    // 以后有人手滑改回一个过小的值。
    #[test]
    fn api_timeout_budget_is_not_too_tight() {
        assert!(
            API_REQUEST_TIMEOUT_SECS >= 10,
            "单次请求超时不应低于 10s，否则并发拉页时边际易崩"
        );
        assert!(
            API_RETRY_TOTAL_SECS > API_REQUEST_TIMEOUT_SECS,
            "重试总预算必须大于单次超时，否则重试没有机会发生"
        );
    }
}
