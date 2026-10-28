# PHP 源码包里的测试（Zend、tests 和几个常用扩展），用装好的 php 跑，不联网
cd /tmp; v=$(php -r 'echo PHP_VERSION;')
[ -d php-$v ] || curl -fsSL "https://www.php.net/distributions/php-$v.tar.xz" | xz -dc | tar x
cd php-$v || exit 1
TEST_PHP_EXECUTABLE=$(command -v php) php run-tests.php -j4 -q --offline --no-color -g FAIL,BORK,LEAK \
    Zend/tests tests ext/standard/tests ext/spl/tests ext/date/tests ext/pcre/tests ext/json/tests \
    ext/sockets/tests ext/pdo_sqlite/tests 2>&1
echo "run-tests: $?"
