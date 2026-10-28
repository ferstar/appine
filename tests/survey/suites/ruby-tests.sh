# Ruby 源码包里的测试（核心、socket、fiber、fileutils），用装好的 ruby 跑
cd /tmp; v=$(ruby -e 'print RUBY_VERSION')
[ -d ruby-$v ] || curl -fsSL "https://cache.ruby-lang.org/pub/ruby/${v%.*}/ruby-$v.tar.gz" | tar xz
cd ruby-$v || exit 1
ruby -I tool/lib tool/test/runner.rb -j4 test/ruby test/socket test/fiber test/fileutils 2>&1
echo "runner: $?"
