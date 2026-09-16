# pica-server —— 哔咔漫画下载器（Web 版）

哔咔漫画下载器的 **Web 后台版**：后端是一个自包含的 Rust HTTP 服务（axum），
前端为 Vue 3 单页应用。整个应用跑在一个 Docker 容器里，用浏览器访问和控制，
无需安装桌面客户端。

> 本项目是基于 [lanyeeee/picacomic-downloader](https://github.com/lanyeeee/picacomic-downloader)
> 的 **Web 化改造**：把原来的 Tauri 桌面端拆分为「Vue 前端 + Rust 服务端」，
> 以容器方式部署在 NAS / 服务器上，通过浏览器远程使用。

## 特性

- **多线程下载**：并发下载漫画章节图片，速度飞快
- **收藏夹**：支持漫画收藏、搜索
- **图形界面**：Vue 3 + Naive UI，功能与桌面版一致
- **单容器部署**：前端静态资源已打包进镜像，一个容器搞定
- **开箱即用**：支持 Docker Compose，适配飞牛 NAS 等环境
- **实时进度**：通过 WebSocket 推送下载进度与日志

## 技术栈

| 层 | 技术 |
| --- | --- |
| 前端 | Vue 3 · Vite · Naive UI · Pinia · UnoCSS |
| 后端 | Rust · axum · tokio · reqwest |
| 部署 | Docker · Docker Compose（三阶段构建） |

## 快速开始（Docker Compose）

```bash
# 1. 准备环境变量
cp .env.example .env
#    编辑 .env，至少设置 PICA_AUTH_TOKEN 和 PICA_DATA_PATH

# 2. 构建并启动
docker compose up -d --build

# 3. 查看启动日志（若 .env 未填 Token，首次会自动生成并打印）
docker compose logs -f pica-server
```

浏览器打开 `http://<主机 IP>:8080`，用 `.env` 中的 Token 登录。

停止：`docker compose down`（数据卷不会被删除）。

> **首次构建较慢**（约 20 分钟以上）：需要从零编译 Rust 依赖
> （axum、image、tokio 等）。之后只要 `Cargo.toml` / `package.json` 未改动，
> 重建就是分钟级。

## 环境变量

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `PICA_DATA_DIR` | `./data` | 数据根目录（镜像内固定为 `/data`） |
| `PICA_AUTH_TOKEN` | 随机生成 | 访问口令。留空则首次启动生成并打印到日志 |
| `PICA_AUTH_USER` | `admin` | Basic Auth 用户名 |
| `PICA_BIND` | `0.0.0.0` | 监听地址 |
| `PICA_PORT` | `8080` | 监听端口 |
| `PICA_STATIC_DIR` | `./dist` | 前端静态资源目录（镜像内为 `/app/dist`） |
| `PICA_HOST_PORT` | `8080` | Compose 中宿主机对外暴露的端口 |
| `PICA_DATA_PATH` | `./data` | Compose 中宿主机数据目录 |
| `PICA_HTTP_PROXY` / `PICA_HTTPS_PROXY` | 空 | 运行期代理（国内访问哔咔 API 通常需要） |
| `BUILD_HTTP_PROXY` / `BUILD_HTTPS_PROXY` | 空 | 构建期代理（拉取 npm / crates.io 依赖） |
| `TZ` | `Asia/Shanghai` | 时区 |

数据目录结构：

```
/data/
├── config.json          # 全部配置（下载目录、并发、代理等）
├── 日志/                # 服务日志
│   └── picacomic-downloader.<日期>.log
└── 漫画下载/            # 下载的漫画，按配置模板分目录（首次下载后出现）
```

## 使用方法

1. 打开页面，使用访问 Token 登录
2. 使用「漫画搜索」或「漫画收藏」，选择漫画进入「章节详情」
3. 在「章节详情」勾选要下载的章节，点击「下载勾选章节」开始下载
4. 下载完成后可在数据目录中查看结果

## 认证方式

单用户、固定口令，支持两种方式：

**Bearer Token**（网页登录用）

```http
Authorization: Bearer <PICA_AUTH_TOKEN>
```

**HTTP Basic**（便于脚本 / 第三方客户端）

```http
Authorization: Basic base64(admin:<PICA_AUTH_TOKEN>)
```

`/api/health` 与静态资源不需要认证，方便健康检查与反向代理。

> 服务本身为明文 HTTP。若要暴露到公网，请置于反向代理（Nginx / Caddy 等）
> 之后并启用 HTTPS，不要直接暴露 8080 端口。

## API 调用（面向脚本 / 自动化）

所有接口挂在 `/api` 前缀下，除 `/api/health` 外都需要认证（见上一节）。

### 按 ID 下载

最简单的下载入口：只需要漫画 ID，无需先构造漫画对象，适合青龙面板、
定时任务、脚本等自动化场景。

```http
POST /api/download/by-id
Content-Type: application/json
Authorization: Bearer <PICA_AUTH_TOKEN>

{
  "comicId": "5f3c8d2a1b9e4f7c6a0d3e8b",
  "chapterId": "a1b2c3d4e5f6"
}
```

| 请求 | 结果 |
|---|---|
| 只传 `comicId` | 下载该漫画所有未下载的章节 |
| `comicId` + `chapterId` | 只下载指定章节 |

返回：

```json
{
  "comicId": "5f3c8d2a1b9e4f7c6a0d3e8b",
  "comicTitle": "漫画标题",
  "createdChapters": ["chapter-id-1", "chapter-id-2"],
  "skippedChapters": ["chapter-id-3"],
  "alreadyRunningChapters": [],
  "createdCount": 2
}
```

- `createdChapters`：本次实际创建任务的章节
- `skippedChapters`：已下载、被跳过的章节（仅整本下载时有值）
- `alreadyRunningChapters`：任务已存在、未重复创建的章节
- `createdCount`：本次创建的任务数

失败时返回 HTTP 4xx/5xx + `{ "errTitle": "...", "errMessage": "..." }`。

示例（curl）：

```bash
# 下载整本
curl -X POST http://192.168.31.124:8080/api/download/by-id \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $PICA_AUTH_TOKEN" \
  -d '{"comicId": "5f3c8d2a1b9e4f7c6a0d3e8b"}'

# 只下载指定章节
curl -X POST http://192.168.31.124:8080/api/download/by-id \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $PICA_AUTH_TOKEN" \
  -d '{"comicId": "5f3c8d2a1b9e4f7c6a0d3e8b", "chapterId": "a1b2c3d4e5f6"}'
```

> 若已在 `.env` 里设置 `PICA_AUTH_DISABLED=true`，可省略 `Authorization` 头。

### 下载任务控制

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/api/download/tasks` | 当前所有任务快照 |
| `POST` | `/api/download/task/{chapterId}/pause` | 暂停 |
| `POST` | `/api/download/task/{chapterId}/resume` | 继续 |
| `POST` | `/api/download/task/{chapterId}/cancel` | 取消 |

实时进度通过 WebSocket `/api/ws` 推送（topic `download-task-event`）。
## 本地开发（不使用 Docker）

前端：

```bash
pnpm install
pnpm dev          # Vite 开发服务器，/api 自动代理到 127.0.0.1:8080
```

后端（另开一个终端）：

```bash
cd src-server
cargo run
```

## 构建说明

`Dockerfile` 为三阶段构建：

| 阶段 | 基础镜像 | 产物 |
| --- | --- | --- |
| `web` | `node:22-bookworm-slim` | `dist/`（Vue 前端） |
| `server` | `rust:1-bookworm` | `pica-server` 静态二进制 |
| `runtime` | `debian:bookworm-slim` | 最终镜像（约 100 MB） |

依赖层单独缓存：只要 `package.json` / `Cargo.toml` 未改动，
重新构建时不会重新编译全部依赖。

镜像构建阶段**不做** `vue-tsc` 类型检查（省内存和时间），
类型检查请在本地或 CI 中运行 `pnpm build`。

## 部署到 NAS

容器以 **uid=1000 / gid=1000** 的非 root 用户运行。挂载目录若属主不对，
在宿主机执行：

```bash
sudo chown -R 1000:1000 /path/to/pica-data
```

端口被占用时，修改 `.env` 中的 `PICA_HOST_PORT`（如 `18080`），
容器内部端口无需改动。

更详细的飞牛 NAS 部署说明见 [DOCKER.md](./DOCKER.md)。

## 常见问题

**打开页面 404 / 空白**
镜像中 `dist/` 未打包进去。确认构建时未跳过 `web` 阶段。

**所有请求返回 401**
Token 不正确。注意 `.env` 中 `PICA_AUTH_TOKEN` 留空时，每次重建容器都会
生成新 Token，请从日志中获取最新值，或直接填入固定值。

**下载全部失败 / 图片 403**
多为网络出口 IP 被哔咔限流，或容器内缺少 CA 证书（镜像已安装）。
也可在配置页设置代理。

**健康检查一直 unhealthy**
在宿主机执行 `curl http://127.0.0.1:<端口>/api/health` 排查。
若修改过 `PICA_PORT`，注意 `HEALTHCHECK` 中写死的是 8080。

## 免责声明

- 本工具仅作学习、研究、交流使用，使用本工具的用户应自行承担风险
- 作者不对使用本工具导致的任何损失、法律纠纷或其他后果负责
- 作者不对用户使用本工具的行为负责，包括但不限于用户违反法律或任何第三方权益的行为

## 致谢

- 原项目：[lanyeeee/picacomic-downloader](https://github.com/lanyeeee/picacomic-downloader)
