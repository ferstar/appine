# Ruby 源码包里的测试（核心、socket、fiber、fileutils），用装好的 ruby 跑
cd /tmp; v=$(ruby -e 'print RUBY_VERSION')
[ -d ruby-$v ] || curl -fsSL "https://cache.ruby-lang.org/pub/ruby/${v%.*}/ruby-$v.tar.gz" | tar xz
cd ruby-$v || exit 1
# 时限放宽 5 倍：*_memory_leak 这些测试的时限是 10 秒，appine 约慢 3 倍就超时
# （docs/rounds/84-debt-checks.md）；原生一侧也照样放宽
ruby -I tool/lib tool/test/runner.rb -j4 --timeout-scale 5 test/ruby test/socket test/fiber test/fileutils 2>&1
echo "runner: $?"
