# alfred 执行者沙箱镜像（R1）
# 来源：.plans/r0-lab/Dockerfile.pi（R0 已验证：network-none 容器内经桥调通 zhipu）
# 内容：inspect 默认沙箱镜像 + Node 22 二进制 + pi-coding-agent（全局 npm）
# 说明：容器 network_mode: none，所有依赖必须在构建期烤进镜像（无运行期网络）。
FROM aisiuk/inspect-tool-support

# pi 要求 node >= 22.19；Debian bookworm apt 只有 node 18，
# 故安装官方 Node 22 二进制（当前机器为 arm64）。
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl ca-certificates xz-utils \
    && curl -fsSL https://nodejs.org/dist/v22.23.2/node-v22.23.2-linux-arm64.tar.xz -o /tmp/node.tar.xz \
    && tar -xJf /tmp/node.tar.xz -C /usr/local --strip-components=1 \
    && rm /tmp/node.tar.xz \
    && node --version \
    && npm --version \
    && npm install -g @earendil-works/pi-coding-agent@0.84.3 \
    && npm cache clean --force \
    && pi --version
