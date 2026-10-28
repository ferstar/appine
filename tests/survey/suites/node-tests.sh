# Node.js 源码包里的 test/parallel，用装好的 node 跑（不解 deps 里的 V8、ICU、OpenSSL）。
# 失败的测试各有一行 "Path: parallel/<名字>"
cd /tmp; v=$(node --version)
[ -d node-$v ] || curl -fsSL "https://nodejs.org/dist/$v/node-$v.tar.xz" | xz -dc | \
    tar x --exclude "node-$v/deps/v8" --exclude "node-$v/deps/icu-small" --exclude "node-$v/deps/openssl"
cd node-$v || exit 1
python3 tools/test.py --shell="$(command -v node)" -j4 -p dots --timeout=600 parallel 2>&1 | tail -n +1
echo "test.py: $?"
