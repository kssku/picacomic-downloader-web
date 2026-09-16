# syntax=docker/dockerfile:1

# ════════════════════════════════════════════════════════════════════
#  pica-server —— 哔咔漫画下载器的 Web 版（NAS / Docker 部署）
#
#  三阶段构建：
#    1. web     : 编译 Vue 前端，产出 dist/
#    2. server  : 编译 Rust 后端（axum），产出 pica-server 静态二进制
#    3. runtime : 只带二进制 + dist + CA 证书，跑在 debian-slim 上
#
#  最终镜像不含 Node / Rust / 源码，体积约 100 MB 上下。
#
#  可选构建参数（构建期 HTTP 代理）：
#    国内直连 npm registry / crates.io / github 通常不通，构建会卡在拉依赖。
#    若宿主机有代理（例如 192.168.31.124:7890），这样构建即可：
#      docker compose build \
#        --build-arg HTTP_PROXY=http://192.168.31.124:7890 \
#        --build-arg HTTPS_PROXY=http://192.168.31.124:7890
#    容器内不能用 127.0.0.1 指向宿主机代理，必须用宿主机的局域网 IP。
#    注意代理是「构建期」参数：运行期会把它们清掉，见 runtime 阶段的注释。
# ════════════════════════════════════════════════════════════════════

# 声明为全局 ARG，三个阶段都能引用（每个 FROM 之后仍会被重置，故各阶段重新 ENV）
ARG HTTP_PROXY=""
ARG HTTPS_PROXY=""


# ── 阶段 1：前端 ────────────────────────────────────────────────────
FROM node:22-bookworm-slim AS web

# 空值是无害的：未传 --build-arg 时，这些变量为空串，
# pnpm / npm 会按「未配置代理」正常直连。
ARG HTTP_PROXY
ARG HTTPS_PROXY
ENV HTTP_PROXY=$HTTP_PROXY \
    HTTPS_PROXY=$HTTPS_PROXY \
    http_proxy=$HTTP_PROXY \
    https_proxy=$HTTPS_PROXY

ENV PNPM_HOME=/pnpm \
    PATH=/pnpm:$PATH \
    CI=1

# PNPM_HOME 必须真实存在：corepack 的 shim 要写进去，
# 后面 --store-dir=/pnpm/store 也依赖它。
RUN mkdir -p /pnpm/store

# corepack 按 package.json 的 packageManager 字段自动装 pnpm@9.5.0
RUN corepack enable

WORKDIR /build

# 先只拷依赖清单，让依赖层可以单独缓存
COPY package.json pnpm-lock.yaml ./
RUN --mount=type=cache,id=pnpm-store,target=/pnpm/store \
    pnpm install --frozen-lockfile --store-dir=/pnpm/store

# 再拷源码。这里显式列出，避免 .dockerignore 之外的意外文件影响缓存
COPY index.html vite.config.ts tsconfig.json tsconfig.node.json uno.config.ts ./
COPY public ./public
COPY src ./src

# 只做 vite build：类型检查（vue-tsc）在 CI / 本地做，
# 镜像构建阶段没必要为它多花一份内存和时间。
RUN pnpm exec vite build


# ── 阶段 2：后端 ────────────────────────────────────────────────────
# 用 `rust:1-bookworm`（大版本滚动 tag）而不是钉死的 `rust:1.97-bookworm`：
# 滚动 tag 上游不会清理，构建不会某天突然拉不到镜像。
# 本 crate 是 edition 2021，1.x 全系都能编译，钉小版本没有必要。
FROM rust:1-bookworm AS server

# cargo 不读 HTTP_PROXY，只认 CARGO_HTTP_PROXY；
# crates.io 索引走 git 拉取，还需给 git 单独配代理。
# 下面统一由 CARGO_HTTP_PROXY 驱动，git 用它拼出 http.proxy。
ARG HTTP_PROXY
ARG HTTPS_PROXY
ENV HTTP_PROXY=$HTTP_PROXY \
    HTTPS_PROXY=$HTTPS_PROXY \
    http_proxy=$HTTP_PROXY \
    https_proxy=$HTTPS_PROXY \
    CARGO_HTTP_PROXY=$HTTPS_PROXY

# 只有当代理非空时才写 git 代理配置，避免留下一个空值的 http.proxy
# 让 git 报 "Proxy CONNECT aborted"。
RUN if [ -n "$HTTPS_PROXY" ]; then \
        git config --global http.proxy "$HTTPS_PROXY"; \
    fi

WORKDIR /build

# 先只拷清单 + 锁文件，编译依赖层可独立缓存。
# 用一个空 main.rs / lib.rs 骗过 cargo，把依赖预先编译好——
# 这样只要 Cargo.toml 没改，改业务代码时这一层直接命中缓存。
COPY src-server/Cargo.toml src-server/Cargo.lock ./
RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && cargo build --release \
    && rm -rf src

# 再拷真实源码，并强制刷新 mtime。
#
# 为什么必须 touch：COPY 会保留宿主机的修改时间，而上面预编译出的
# target/ 产物比这些源码**更新**。cargo 靠 mtime 判断是否重新编译，
# 一旦它认为源码没变，就会直接复用「空 lib + 空 main」的旧产物，
# 产出一个能跑但没有任何业务逻辑的二进制。
# touch 把所有源码 mtime 提到当前时刻，确保真实代码一定被重新编译。
COPY src-server/src ./src
RUN find src -type f -exec touch {} + \
    && cargo build --release \
    && test -x target/release/pica-server


# ── 阶段 3：运行期 ──────────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

# ca-certificates : 访问哔咔 API / 图片的 HTTPS 必需，缺了会全部握手失败
# tzdata          : 日志时间戳按本地时区显示
# curl            : 供 HEALTHCHECK 使用
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        tzdata \
        curl \
    && rm -rf /var/lib/apt/lists/*

# 非 root 运行。固定 uid/gid 便于宿主机上给下载目录授权
RUN groupadd -g 1000 pica \
    && useradd -u 1000 -g pica -m -s /usr/sbin/nologin pica

WORKDIR /app

COPY --from=server /build/target/release/pica-server /app/pica-server
COPY --from=web /build/dist /app/dist

# 数据目录：配置、日志、漫画全部落在这里，必须挂 volume
#
# 为什么 chown 之后还要 chmod：
#   宿主机上若启用了特殊 ACL（飞牛 fnOS 的存储池就是这样），源文件的
#   权限位可能是 000。Docker 的 COPY 会原样保留权限位，于是镜像里出现
#   一个 owner 正确但权限为 0 的文件——连 owner 自己都读不了。
#   实际症状很隐蔽：ServeDir 打不开该文件，转而触发 SPA fallback，
#   浏览器请求 /favicon.png 得到的是 index.html（200 而非 404），
#   而同一目录下权限正常的 .js/.css 却能正常返回。
#   chown 只改归属、不改权限位，所以必须补一条 chmod。
#
# a+rX：所有文件加可读；目录额外加可进入（X 只对目录和已有可执行位的文件生效）
RUN mkdir -p /data \
    && chown -R pica:pica /app /data \
    && chmod -R a+rX /app/dist /app/pica-server
VOLUME ["/data"]

USER pica

ENV PICA_DATA_DIR=/data \
    PICA_STATIC_DIR=/app/dist \
    PICA_BIND=0.0.0.0 \
    PICA_PORT=8080 \
    TZ=Asia/Shanghai

# 运行期代理与构建期代理彻底分离。
#
# 背景：上面的 HTTP_PROXY / HTTPS_PROXY 是「构建期」的，由 build args 传入。
# 而 reqwest 在运行期也会读同名环境变量 —— 如果放任构建参数渗进来，
# pica-server 访问哔咔 API 时就会莫名绕道代理：在构建机上恰好能通，
# 所以问题不会当场暴露，等镜像换环境或代理下线才爆发，且表现为
# 「全部请求超时」，极难定位。
#
# 这里把四个变量显式置空，切断继承链。真正需要运行期代理的部署
# （例如 NAS 直连哔咔不通、必须走代理），由 docker-compose 的
# environment 段传入 PICA_HTTP_PROXY / PICA_HTTPS_PROXY 覆盖。
#
# 为什么用 ENV X="" 而不是 UNSET：Dockerfile 没有 UNSET 指令，
# 空串是切断继承的唯一手段；reqwest 对空串按「未配置」处理。
ENV HTTP_PROXY="" \
    HTTPS_PROXY="" \
    http_proxy="" \
    https_proxy=""

# NO_PROXY 覆盖回环 + 私有网段：即使运行期配了代理，容器内健康检查
# （127.0.0.1:8080）和局域网互访也不该绕道代理。
ENV NO_PROXY="localhost,127.0.0.1,::1,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16,*.local" \
    no_proxy="localhost,127.0.0.1,::1,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16,*.local"

EXPOSE 8080

# 无需认证即可访问，用来判断容器是否真的活着
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/api/health || exit 1

ENTRYPOINT ["/app/pica-server"]