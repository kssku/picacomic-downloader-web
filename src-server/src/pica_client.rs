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

        let format = image::guess_format(&image_data)
            .context("无法从图片数据中猜测出图片格式，可能图片数据不完整或已损坏")?;

        Ok((image_data, format))
    }
}

// ===================== 辅助函数 =====================

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
        .build_with_total_retry_duration(Duration::from_secs(3));

    let client = reqwest::ClientBuilder::new()
        .timeout(Duration::from_secs(2))
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

trait ClientBuilderExt {
    fn set_proxy(self, app: &AppContext, client_name: &str) -> Self;
}

impl ClientBuilderExt for reqwest::ClientBuilder {
    fn set_proxy(self, app: &AppContext, client_name: &str) -> reqwest::ClientBuilder {
        let proxy_mode = app.get_config().read().proxy_mode;
        match proxy_mode {
            ProxyMode::System => self,
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