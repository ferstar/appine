# Go：goroutine、channel 和 select，死循环里的异步抢占（SIGURG）和 GC，nil 解引用转成
# panic，os/signal，os/exec 和管道，浮点格式化
set -e
mkdir -p /tmp/diff-go
cd /tmp/diff-go
cat > main.go <<'GO'
package main

import (
	"crypto/sha256"
	"fmt"
	"math"
	"os"
	"os/exec"
	"os/signal"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"
)

func spin(n int) int {
	s := 0
	for i := 0; i < n; i++ {
		s += i % 7
	}
	return s
}

func main() {
	var wg sync.WaitGroup
	results := make([]int, 8)
	for i := range results {
		wg.Add(1)
		go func() {
			defer wg.Done()
			results[i] = spin(2000000 + i)
		}()
	}
	wg.Wait()
	fmt.Println("goroutines", results)

	ch, quit := make(chan int), make(chan struct{})
	go func() {
		for i := 0; i < 5; i++ {
			ch <- i * i
		}
		close(quit)
	}()
	total := 0
loop:
	for {
		select {
		case v := <-ch:
			total += v
		case <-quit:
			break loop
		}
	}
	fmt.Println("select", total)

	// 不调用任何函数的死循环只能靠信号异步抢占，否则 GC 的 stop-the-world 停不下来
	var stop int32
	go func() {
		x := 0
		for stop == 0 {
			x++
		}
	}()
	time.Sleep(20 * time.Millisecond)
	runtime.GC()
	stop = 1
	fmt.Println("gc with a tight loop running")

	sigs := make(chan os.Signal, 1)
	signal.Notify(sigs, syscall.SIGUSR1)
	syscall.Kill(os.Getpid(), syscall.SIGUSR1)
	fmt.Println("signal", <-sigs)

	out, err := exec.Command("/bin/sh", "-c", "echo from $0; exit 3", "sh").Output()
	fmt.Printf("exec %q %v\n", out, err)

	words := strings.Fields("the quick brown fox jumps over the lazy dog")
	sort.Strings(words)
	fmt.Println(strings.Join(words, ","))
	fmt.Printf("%x\n", sha256.Sum256([]byte("appine")))
	fmt.Println(math.Sqrt(2), math.Exp(1), strconv.FormatFloat(1e21, 'g', -1, 64), math.MaxInt64, -0.0)

	defer func() {
		fmt.Println("recovered:", recover())
	}()
	var p *int
	fmt.Println(*p)
}
GO
cat > go.mod <<'MOD'
module diffgo

go 1.22
MOD
go build -o main .
./main
