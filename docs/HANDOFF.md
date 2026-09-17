# HANDOFF - 项目交接文档

> 最后更新：2026-09-17（验收 1/2/3/4 通过）
> 当前提交：`9288941`（本地 / origin / NAS 三方一致）
> NAS 部署镜像：`5791c2076b87`

---

## 一、项目简介

哔咔漫画（pica）下载器 Web 版。Rust 后端（axum）+ 前端 SPA，部署在 NAS 的 Docker 容器中，
与青龙（QingLong）后处理流水线对接：下载 → 打 CBZ → 上传 115 网盘归档。

### 仓库结构

```
src-server/            Rust 后端
  src/
    api/               HTTP 路由与命令层（routes.rs / commands.rs）
    responses/         皮卡 API 响应结构
    store/             SQLite 持久化（migrations/repo/types）
    types/             领域类型（comic.rs 等）
    download_manager.rs  下载调度核心
    pica_client.rs     皮卡 API 客户端（代理、签名、超时、重试）
    config.rs          配置结构
src/                   前端（TS + Vite）
docs/                  设计/交接文档
docker-compose.yml     部署编排
```

---

## 二、部署环境（NAS）

| 项 | 值 |
|---|---|
| 源码仓库 | `/vol1/1000/github/picacomic-downloader-web` |
| 运行时数据 | `/vol1/1000/pica-server/`（config.json、pica_server.db、日志/） |
| compose 文件 | `docker-compose.yml`（`build: context: .`，镜像 `pica-server:latest`） |
| 下载目录（宿主） | `/vol1/1000/comic/pika/comic`（容器内 `/comic-download`） |
| CBZ 输出 | `/vol1/1000/comic/pika/cbz` |
| 115 归档 | `/vol1/1000/115/115open/comic/pika/{年份}/{comic_id}/` |
| 青龙脚本 | `/vol1/1000/docker/qinglong/data/scripts/comic/pika/` |
| API 端口 | 容器 `8080`，健康检查 `GET /api/health` |

### 部署流程

```bash
# 1. 本地提交并推送
git add -A && git commit -m "..." && git push origin main

# 2. NAS 拉取 + 构建 + 重启
ssh nas "cd /vol1/1000/github/picacomic-downloader-web && \
  git pull && \
  docker compose build pica-server && \
  docker compose up -d pica-server"

# 3. 验证
ssh nas "curl -sS http://127.0.0.1:8080/api/health"
```

### NAS 的 git remote（已改 SSH）

- remote：`git@github.com:kssku/picacomic-downloader-web.git`（曾用 HTTPS+PAT，已废弃）
- SSH key：`~/.ssh/id_ed25519_github`（公钥已加为 GitHub Deploy Key，只读）

---

## 三、网络环境（关键约束）

**NAS 出口 IP 在中国大陆（如 `112.27.182.119`），直连国外站点被墙。**

| 目标 | NAS 直连 | 说明 |
|---|---|---|
| 百度 / QQ | ✅ | 国内正常 |
| github.com | ❌ 超时 | 被墙 |
| `img.picacomic.com` (199.96.58.157) | ❌ | 被墙 |
| `storage-b.picacomic.com` (31.13.69.245) | ❌ | 被墙 + DNS 污染 |
| `storage1.picacomic.com` (168.143.162.42) | ❌ | 被墙 |

**结论：下载器必须走代理**（mihomo `host.docker.internal:7890`）。
皮卡 API（`picaapi.go2778.com`）与图片 CDN 均需代理；
网页版 `manhuapica.com` 无备用直连域名，与下载器共用同一后端。

> 代理抖动会导致偶发 `tls handshake eof`，由图片重试 + API 超时放宽吸收。

---

## 四、功能清单

- 认证：登录、用户资料、token 中间件
- 搜索 / 详情 / 收藏夹
- 下载：按章节 / 整本 / 按 ID；暂停 / 恢复 / 取消 / 重试
- 并发：章节并发 + 图片并发；限速（间隔秒）
- 持久化：SQLite（download_task + download_image），崩溃/重启恢复，断点续传
- 配置热更新：并发 / 代理 / api_base_url / 文件日志，无需重启
- 任务查询（青龙契约）：`/api/tasks`、`/api/tasks/stats`、字段同步
- 日志：文件日志 + `/api/logs`
- WebSocket：`/api/ws` 推送任务事件

---

## 五、目录格式与青龙对接

**默认 `dir_fmt = "{comic_id}/{order}"`**（2026-09-17 由 `{comic_title}/{order} {chapter_title}` 改来）。

下载器落盘：
```
{downloadDir}/{comic_id}/{order}/{图片}.jpg
```

后处理脚本 `pica_postprocess.py`：
- 读漫画目录的 `元数据.json` 取 `id`
- 章节 = 子目录（跳过 `.` 开头与含下载标记的），读 `章节元数据.json` 的 `order`
- 打包为 `{cbz_output_dir}/{年份}/{comic_id}/{order:03d}.cbz`
- 打包成功后删源目录

上传脚本 `pica_upload.py`：监控 `cbz/`，移动到 115 归档后删本地 CBZ。

**端到端已验证**：下载 → 打 CBZ → 归档 115 全通。

---

## 六、修复历史

| 提交 | 内容 |
|---|---|
| `5e82038` | fix(proxy): System 模式显式读取环境变量代理，修复图片下载 tls handshake eof |
| `628f9c0` | fix(retry): 链接阶段失败的章节不再被误判为 completed |
| `7a9a60a` | fix(api): api_client 超时 2s→15s，重试预算 3s→30s |
| `1c84bc1`→`9288941` | feat(dir-fmt): 默认目录格式改为 {comic_id}/{order} |

### 缺陷 1：失败被误判 completed

- 根因：`retry_task` 用 `unfinished == 0` 判断完成；链接阶段失败时 `download_image` 无记录，`total=0, done=0 → unfinished=0` → 误判完成。
- 修复：抽 `should_finalize_as_completed(total, unfinished)`，要求 `total > 0 && unfinished == 0`；`total==0` 落回重调度。

### 缺陷 3：api_client 超时过紧

- 根因：`timeout(2s)`，并发拉全部页时任一页抖动整章失败。
- 修复：`API_REQUEST_TIMEOUT_SECS=15`、`API_RETRY_TOTAL_SECS=30`。

### 缺陷 2：img 主机间歇不可达

- 定性：**非代码缺陷**，是 NAS 网络需代理（见第三节）。

---

## 七、验收标准现状

| 标准 | 状态 |
|---|---|
| 1 重启不丢（恢复） | ✅ 通过（重启后任务恢复、进度保留 84→继续增长） |
| 2 不重复下载 | ✅ 通过（重启前文件 mtime 未变，85+163=248 精确吻合，0 重复） |
| 3 单图失败不炸章节 | ✅ 通过（代码审查 + 单测 `single_image_failure_does_not_affect_siblings_and_retry_targets_only_it`） |
| 4 热更新 imgConcurrency 20→10 | ✅ 通过（热更新生效、不中断、单调推进） |
| 5 日志可控 | ✅ 通过（120 页章节 INFO 下仅 17 行，远低于 200 上限） |
| 6 API 契约 | ✅ 通过（`/api/tasks` 支持 state/comicId/since/limit 过滤；注：为 30 天任务视图，非永久台账） |

---

## 八、运维常用命令

```bash
# 健康
curl -sS http://127.0.0.1:8080/api/health

# 任务列表
curl -sS http://127.0.0.1:8080/api/tasks

# 读配置（含 token，慎用）
curl -sS http://127.0.0.1:8080/api/config

# 改配置（整体读-改-写）
# GET /api/config -> 改字段 -> POST /api/config

# 容器状态 / 日志
ssh nas "docker ps --filter name=pica-server"
ssh nas "tail -f /vol1/1000/pica-server/日志/picacomic-downloader.$(date +%F).log"
```

---

## 九、注意事项 / 待办

- **配置持久化**：改 `config.rs` 默认值不影响已有 `config.json`，需另行改配置文件（或用 API）。
- **dir_fmt 影响恢复**：DB 里持久化了 `dir_fmt`，中途改格式会导致旧任务恢复时找不到文件。
- **PAT**：NAS 曾用 HTTPS+PAT，已吊销并改 SSH。
- 待办：验收标准 2/3 的覆盖确认。