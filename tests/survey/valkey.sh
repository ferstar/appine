# valkey：和 redis 一样，arm64 上启动时检查 fork 之后 smaps 的 Shared_Dirty
rm -f /tmp/valkey.log
valkey-server --port 6390 --daemonize yes --dir /tmp --logfile /tmp/valkey.log --save ''; echo "start: $?"; sleep 0.5
valkey-cli -p 6390 ping; valkey-cli -p 6390 shutdown nosave 2>/dev/null
grep -E '#' /tmp/valkey.log | head -5
