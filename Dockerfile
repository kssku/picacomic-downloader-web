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
# ════════════════════════════════════════════════════════════════════


# ── 阶段 1：前端 ────────────────────────────────────────────────────
FROM node:22-bookworm-slim AS web

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
COPY auto-imports.d.ts components.d.ts ./
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
RUN mkdir -p /data && chown -R pica:pica /app /data
VOLUME ["/data"]

USER pica

ENV PICA_DATA_DIR=/data \
    PICA_STATIC_DIR=/app/dist \
    PICA_BIND=0.0.0.0 \
    PICA_PORT=8080 \
    TZ=Asia/Shanghai

EXPOSE 8080

# 无需认证即可访问，用来判断容器是否真的活着
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/api/health || exit 1

ENTRYPOINT ["/app/pica-server"]