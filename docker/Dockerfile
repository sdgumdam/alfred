# alfred-executor 沙箱镜像：pi coding agent 的执行环境。
# executor（crates/alfred-executor）以 `docker run alfred-executor:latest -p "<prompt>"` 驱动，
# 工作区固定挂载 /workspace（rw），会话日志经 --session-dir 卷回收到宿主机。
FROM node:22-slim

RUN npm install -g @earendil-works/pi-coding-agent

WORKDIR /workspace

ENTRYPOINT ["pi"]
