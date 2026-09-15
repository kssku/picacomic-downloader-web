# pica-server —— Docker / 飞牛 NAS 部署

哔咔漫画下载器的 **Web 后台版**：后端是一个自包含的 Rust HTTP 服务（axum），
前端就是原来的 Vue 界面。整个应用跑在一个容器里，用浏览器控制。

- 数据目录：`/data`（配置、日志、下载的漫画）
- 默认端口：`8080`
- 前端静态资源：镜像内已打包，无需额外部署

---

## 一、快速开始

```bash
# 1. 准备环境变量（至少要设一个访问 Token）
cp .env.example .env
#    编辑 .env，填上 PICA_AUTH_TOKEN 和 PICA_DATA_PATH

# 2. 构建并启动
docker compose up -d --build

# 3. 看启动日志（第一次会自动生成 Token，如果 .env 里没填）
docker compose logs -f pica-server
```

浏览器打开 `http://<NAS 的 IP>:8080`，用 `.env` 里的 Token 登录。

停止：`docker compose down`（数据卷不会被删）。

> **首次构建会比较慢**（20 分钟以上）：要从零编译 Rust 依赖（axum、image、
> tokio 等）。之后只要 `Cargo.toml` / `package.json` 没改，重建就是分钟级。

---

## 二、在飞牛 NAS 上部署

飞牛的 Docker 面板（或 `docker compose`）都适用，关键是两点：

### 1. 数据目录要落在存储池上

容器内的 `/data` 必须映射到 NAS 的真实存储路径，**不要**用默认的
`./data`。比如：

```yaml
volumes:
  - /vol1/1000/docker/pica-server:/data
```

对应 `.env`：

```ini
PICA_DATA_PATH=/vol1/1000/docker/pica-server
```

这样 `/vol1/1000/docker/pica-server/漫画下载/...` 就是普通图片文件，
可以直接在飞牛的文件管理器里浏览、做快照和备份。

### 2. 端口冲突

飞牛自身的 Web 面板会占用 80/443，如果 8080 也被占了，
改 `.env` 里的 `PICA_HOST_PORT`（例如 `18080`），容器内部端口不用动。

### 3. 权限（如果挂载后写不进去）

容器以 **uid=1000 / gid=1000** 的非 root 用户运行。宿主机目录若属主不对，
在 NAS 的 SSH 里执行：

```bash
sudo chown -R 1000:1000 /vol1/1000/docker/pica-server
```

> 若你的 NAS 上 1000 已被其他服务占用，把 `Dockerfile` 里
> `useradd -u 1000` 改成空闲的 uid，重新构建即可。

### 4. 用飞牛面板而非 compose 时

在「Docker → 镜像」里构建，或先在别处 `docker build` 后导入镜像。
容器参数照着 `docker-compose.yml` 填：

| 项目 | 值 |
| --- | --- |
| 端口映射 | `8080` → `8080`（或自定义宿主端口） |
| 存储映射 | `/vol1/1000/docker/pica-server` → `/data` |
| 环境变量 | `PICA_AUTH_TOKEN`、`PICA_AUTH_USER`、`TZ` |
| 重启策略 | 除非手动停止，否则自动重启 |
| 健康检查 | `curl -fsS http://127.0.0.1:8080/api/health` |

---

## 三、环境变量一览

| 变量 | 默认值 | 说明 |
| --- | --- | --- |
| `PICA_DATA_DIR` | `./data` | 数据根目录（镜像内固定为 `/data`） |
| `PICA_AUTH_TOKEN` | 随机生成 | 访问口令。留空则首次启动生成并打印到日志 |
| `PICA_AUTH_USER` | `admin` | Basic Auth 用户名 |
| `PICA_BIND` | `0.0.0.0` | 监听地址 |
| `PICA_PORT` | `8080` | 监听端口 |
| `PICA_STATIC_DIR` | `./dist` | 前端静态资源目录（镜像内为 `/app/dist`） |

数据目录结构：

```
/data/
├── config.json          # 全部配置（下载目录、并发、代理等）
├── 日志/                # 服务日志（目录名就是中文「日志」）
│   └── picacomic-downloader.<日期>.log
└── 漫画下载/            # 下载的漫画，按配置的模板分目录
                         # 首次下载后才会出现
```

---

## 四、认证方式

单用户、固定口令，支持两种方式：

**Bearer Token**（网页登录用）

```http
Authorization: Bearer <PICA_AUTH_TOKEN>
```

**HTTP Basic**（便于脚本 / 第三方客户端）

```http
Authorization: Basic base64(admin:<PICA_AUTH_TOKEN>)
```

`/api/health` 与静态资源不需要认证，方便做健康检查和反向代理。

> 服务本身是明文 HTTP。如果要暴露到公网，请放在反向代理（Nginx /
> Caddy / 飞牛自带的反代）之后并启用 HTTPS，不要直接暴露 8080。

---

## 五、常用运维命令

```bash
docker compose logs -f pica-server        # 实时日志
docker compose up -d --build              # 改代码后重建
docker compose restart pica-server        # 重启
docker compose down                       # 停止并删除容器（数据保留）

# 查自动生成的 Token（横幅里是中文「访问令牌」）
docker compose logs pica-server | grep 访问令牌

# 进容器排查
docker compose exec pica-server sh
```

---

## 六、构建说明

`Dockerfile` 是三阶段构建：

| 阶段 | 基础镜像 | 产物 |
| --- | --- | --- |
| `web` | `node:22-bookworm-slim` | `dist/`（Vue 前端） |
| `server` | `rust:1-bookworm` | `pica-server` 静态二进制（约 20 分钟） |
| `runtime` | `debian:bookworm-slim` | 最终镜像（约 100 MB） |

依赖层单独缓存：只要 `package.json` / `Cargo.toml` 没改，
重新构建时不会重新编译全部依赖。

镜像构建阶段**不做** `vue-tsc` 类型检查（省内存和时间），
类型检查请在本地或 CI 里跑 `pnpm build`。

前端单独调试（不用 Docker）：

```bash
pnpm install
pnpm dev          # Vite 开发服务器，/api 自动代理到 127.0.0.1:8080
```

同时另开一个终端跑后端：

```bash
cd src-server
cargo run
```

---

## 七、常见问题

**打开页面是 404 / 空白**
镜像里 `dist/` 没进去。确认构建时没有跳过 `web` 阶段，且
`.dockerignore` 里没有误伤 `src` 或 `index.html`。

**所有请求都返回 401**
Token 不对。注意：`.env` 里 `PICA_AUTH_TOKEN` 留空时每次重建容器都会
换新 Token，去日志里取最新的，或者干脆填一个固定值。

**下载全部失败 / 图片 403**
多半是网络出口 IP 被哔咔限流，或容器内缺 CA 证书（镜像里已装）。
也可以在配置页里设置代理。

**容器启动了但健康检查一直 unhealthy**
宿主机上 `curl http://127.0.0.1:<端口>/api/health` 试试。
若改过 `PICA_PORT`，注意 `HEALTHCHECK` 里写死的是 8080。