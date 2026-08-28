# alfred 沙箱基础镜像：Debian 12 slim + node 22 + pi coding agent。
# 三容器（planner/executor/reviewer）共用，差异只在 compose 挂载面（可见范围矩阵）。
# apt 源用阿里云镜像（国内网络直连可靠），node 从 npmmirror 拉。
FROM debian:12-slim

RUN sed -i 's|deb.debian.org|mirrors.aliyun.com|g' /etc/apt/sources.list.d/debian.sources 2>/dev/null; \
    sed -i 's|deb.debian.org|mirrors.aliyun.com|g' /etc/apt/sources.list 2>/dev/null; \
    apt-get update && apt-get install -y --no-install-recommends \
      curl ca-certificates xz-utils procps git \
    && rm -rf /var/lib/apt/lists/*

# node 22（pi 要求 >=22.19），npmmirror 拉 pi
RUN ARCH=$(dpkg --print-architecture) \
    && NODE_VER=v22.19.0 \
    && curl -fsSL https://npmmirror.com/mirrors/node/${NODE_VER}/node-${NODE_VER}-linux-${ARCH}.tar.xz \
       | tar -xJ -C /usr/local --strip-components=1 \
    && node --version

RUN npm config set registry https://registry.npmmirror.com \
    && npm install -g @earendil-works/pi-coding-agent

WORKDIR /workspace
