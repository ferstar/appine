# docker：dockerd + containerd + runc。vfs 存储驱动、不建网桥、不用 iptables，容器用 host
# 网络。仓库要另建，不要和 podman 的共用（Alpine 的 runc 与 crun 冲突）：
#   scripts/survey -s target/survey/docker docker
# 原生对照：scripts/survey -n <装了 docker 的 alpine 镜像> docker，SURVEY_DOCKER_OPTS=--privileged
exec 2>&1
[ -x /usr/bin/dockerd ] || apk add -q docker
mkdir -p /etc/docker
echo '{"storage-driver":"vfs","iptables":false,"ip6tables":false,"bridge":"none"}' > /etc/docker/daemon.json
step() { echo; echo "== $*"; }
HTTP_PROXY=${http_proxy:-} HTTPS_PROXY=${https_proxy:-} NO_PROXY=127.0.0.1,localhost dockerd >/tmp/dockerd.log 2>&1 &
for i in $(seq 60); do docker info >/dev/null 2>&1 && break; sleep 1; done
step version; docker version --format '{{.Client.Version}} {{.Server.Version}}'; runc --version | head -1
step info; docker info --format '{{.Driver}} {{.CgroupDriver}} {{.CgroupVersion}} {{.DefaultRuntime}}'
docker image inspect alpine:3.24 >/dev/null 2>&1 || docker pull -q alpine:3.24 >/dev/null
step run; docker run --rm --network host alpine:3.24 sh -c 'echo hello; cat /etc/alpine-release'
echo "exit=$?"
step dockerd errors; grep -E 'level=(error|fatal)' /tmp/dockerd.log | sed 's/^time="[^"]*" //; s/ id=[0-9a-f]*//' | cut -c1-300 | sort -u | head -10
kill $(pidof dockerd); sleep 3
