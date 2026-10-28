# CPython 自带的测试集（python3-tests），不联网；--rerun 把失败的再单独跑一次。结尾的
# "== Tests result" 之后是汇总
cd /tmp; python3 --version
python3 -m test -j4 --timeout 1200 --rerun -u all,-network,-urlfetch,-gui,-largefile,-audio,-cpu 2>&1
echo "python -m test: $?"
