# Go 标准库的测试（-short），4 个包并行；只打印失败的部分和通过的包数。Alpine 的 go 包
# 删掉了 src 里的 _test.go，所以用 go.dev 的官方构建（静态链接）作为 GOROOT
cd /tmp; v=$(go env GOVERSION)
[ -x go/bin/go ] || curl -fsSL "https://go.dev/dl/$v.linux-arm64.tar.gz" | tar xz
export GOROOT=/tmp/go PATH=/tmp/go/bin:$PATH; go version
go test -short -p 4 std >/tmp/go-test.log 2>&1; echo "go test: $?"
grep -E '^(--- FAIL|FAIL|panic:|fatal error:)' /tmp/go-test.log
echo "ok packages: $(grep -c '^ok ' /tmp/go-test.log)"
grep -B2 -A25 -E '^--- FAIL' /tmp/go-test.log | head -400
