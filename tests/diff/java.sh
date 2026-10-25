# Java（OpenJDK 21）：平台线程和虚拟线程、C2 编译后的隐式空指针检查和反优化、栈溢出
# 保护页（都靠 SIGSEGV）、GC 与弱引用、子进程、NIO 上的 HTTP、文件映射和文件锁、crypto，
# 信号处理线程和 SIGTERM 触发的关闭钩子；ZGC 多重映射堆；jcmd 的 attach（SIGQUIT +
# AF_UNIX）。java 经由 /usr/bin/java 这个符号链接启动，靠 /proc/self/exe 找到 libjli.so
set -e
mkdir -p /tmp/diff-java
cd /tmp/diff-java
cat > Main.java <<'JAVA'
import java.io.*;
import java.math.*;
import java.net.*;
import java.net.http.*;
import java.nio.*;
import java.nio.channels.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.*;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;
import java.util.stream.*;
import java.util.zip.*;
import javax.crypto.*;
import javax.crypto.spec.*;
import com.sun.net.httpserver.*;

public class Main {
    sealed interface Shape permits Circle, Rect {}
    record Circle(double r) implements Shape {}
    record Rect(double w, double h) implements Shape {}

    static double area(Shape s) {
        return switch (s) {
            case Circle c -> Math.PI * c.r() * c.r();
            case Rect r -> r.w() * r.h();
        };
    }

    static int hash(Object o) { return o.hashCode(); }

    static int depth;
    static void recurse() { depth++; recurse(); }

    static long spin(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) s += i % 7;
        return s;
    }

    public static void main(String[] args) throws Exception {
        // 平台线程、synchronized、wait/notify
        long[] results = new long[8];
        Thread[] ts = new Thread[8];
        Object lock = new Object();
        int[] counter = {0};
        for (int i = 0; i < ts.length; i++) {
            int k = i;
            ts[i] = new Thread(() -> {
                results[k] = spin(3_000_000 + k);
                synchronized (lock) { counter[0]++; lock.notifyAll(); }
            });
            ts[i].start();
        }
        synchronized (lock) { while (counter[0] < ts.length) lock.wait(); }
        for (Thread t : ts) t.join();
        System.out.println("threads " + Arrays.toString(results));

        // 虚拟线程：continuation 在 ForkJoinPool 的载体线程之间搬栈
        AtomicInteger vt = new AtomicInteger();
        try (var ex = Executors.newVirtualThreadPerTaskExecutor()) {
            for (int i = 0; i < 10_000; i++) {
                ex.submit(() -> { Thread.sleep(1); return vt.incrementAndGet(); });
            }
        }
        System.out.println("virtual threads " + vt.get());

        System.out.println("parallel " + IntStream.range(0, 2_000_000).parallel().mapToLong(i -> i % 13).sum());

        // 让 C2 编译 area，再用新的形状和 null 触发反优化、隐式空指针检查（SIGSEGV）
        double sum = 0;
        for (int i = 0; i < 3_000_000; i++) {
            sum += area(i % 3 == 0 ? new Circle(i % 10) : new Rect(i % 7, i % 5));
            sum += hash("x" + (i & 7));
        }
        System.out.printf("jit %.3f%n", sum);
        try { area(null); } catch (NullPointerException e) { System.out.println("npe " + e.getMessage()); }
        try { hash(null); } catch (NullPointerException e) { System.out.println("npe " + e.getMessage()); }
        int[] arr = new int[3];
        try { arr[spin(10) > 0 ? 5 : 0] = 1; } catch (ArrayIndexOutOfBoundsException e) { System.out.println("aioobe " + e.getMessage()); }
        try { System.out.println(1 / (int) (spin(1) - spin(1))); } catch (ArithmeticException e) { System.out.println("arith " + e.getMessage()); }
        // 栈溢出靠线程栈上的保护页，第二次能再抓到说明黄区重新打开了
        for (int i = 0; i < 2; i++) {
            depth = 0;
            try { recurse(); } catch (StackOverflowError e) { System.out.println("stack overflow caught " + (depth > 1000)); }
        }

        // GC：大量短命对象，弱引用在 System.gc() 之后被清掉
        List<byte[]> keep = new ArrayList<>();
        long allocated = 0;
        for (int i = 0; i < 200_000; i++) {
            byte[] b = new byte[1024 + i % 512];
            allocated += b.length;
            if (i % 1000 == 0) keep.add(b);
        }
        Object weakTarget = new Object();
        var weak = new java.lang.ref.WeakReference<>(weakTarget);
        weakTarget = null;
        System.gc();
        System.out.println("gc " + allocated + " kept " + keep.size() + " weak cleared " + (weak.get() == null));

        // 子进程（posix_spawn + jspawnhelper）
        Process p = new ProcessBuilder("/bin/sh", "-c", "echo from $0; exit 3", "sh").redirectErrorStream(true).start();
        String out = new String(p.getInputStream().readAllBytes(), StandardCharsets.UTF_8).trim();
        System.out.println("process " + out + " exit " + p.waitFor());

        // 回环上的 HTTP：服务端和 HttpClient 都用 NIO Selector（epoll）
        HttpServer server = HttpServer.create(new InetSocketAddress(InetAddress.getLoopbackAddress(), 0), 0);
        server.createContext("/", h -> {
            byte[] body = ("served " + h.getRequestURI()).getBytes(StandardCharsets.UTF_8);
            h.sendResponseHeaders(200, body.length);
            try (OutputStream os = h.getResponseBody()) { os.write(body); }
        });
        server.setExecutor(Executors.newFixedThreadPool(2));
        server.start();
        HttpClient client = HttpClient.newHttpClient();
        var resp = client.send(HttpRequest.newBuilder(URI.create("http://127.0.0.1:" + server.getAddress().getPort() + "/path?q=1")).build(),
                HttpResponse.BodyHandlers.ofString());
        System.out.println("http " + resp.statusCode() + " " + resp.body());
        server.stop(0);
        ((ExecutorService) server.getExecutor()).shutdown();

        // 文件：内存映射、文件锁、遍历
        Path dir = Files.createTempDirectory("diff-java");
        Path f = dir.resolve("data.bin");
        try (FileChannel ch = FileChannel.open(f, StandardOpenOption.CREATE, StandardOpenOption.READ, StandardOpenOption.WRITE)) {
            MappedByteBuffer mb = ch.map(FileChannel.MapMode.READ_WRITE, 0, 1 << 16);
            for (int i = 0; i < mb.capacity(); i += 4) mb.putInt(i, i * 31);
            mb.force();
            try (FileLock fl = ch.lock()) { System.out.println("lock " + fl.isValid()); }
        }
        ByteBuffer bb = ByteBuffer.wrap(Files.readAllBytes(f));
        System.out.println("mmap " + bb.getInt(4000) + " " + Files.size(f));
        Files.writeString(dir.resolve("a.txt"), "hello");
        try (Stream<Path> s = Files.walk(dir)) { System.out.println("walk " + s.map(x -> dir.relativize(x).toString()).sorted().toList()); }

        // crypto、zip、CRC32（aarch64 上都有指令内建）
        MessageDigest md = MessageDigest.getInstance("SHA-256");
        System.out.println("sha256 " + HexFormat.of().formatHex(md.digest("appine".getBytes(StandardCharsets.UTF_8))));
        Cipher c = Cipher.getInstance("AES/GCM/NoPadding");
        c.init(Cipher.ENCRYPT_MODE, new SecretKeySpec(new byte[16], "AES"), new GCMParameterSpec(128, new byte[12]));
        System.out.println("aes " + HexFormat.of().formatHex(c.doFinal("appine".getBytes(StandardCharsets.UTF_8))));
        KeyPairGenerator kpg = KeyPairGenerator.getInstance("EC");
        kpg.initialize(256);
        KeyPair kp = kpg.generateKeyPair();
        Signature sig = Signature.getInstance("SHA256withECDSA");
        sig.initSign(kp.getPrivate());
        sig.update(new byte[100]);
        byte[] signature = sig.sign();
        sig.initVerify(kp.getPublic());
        sig.update(new byte[100]);
        System.out.println("ecdsa " + sig.verify(signature));
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        try (var gz = new GZIPOutputStream(bos)) { gz.write("abc".repeat(10000).getBytes(StandardCharsets.UTF_8)); }
        byte[] unz = new GZIPInputStream(new ByteArrayInputStream(bos.toByteArray())).readAllBytes();
        CRC32 crc = new CRC32();
        crc.update(unz);
        System.out.println("zip " + bos.size() + " " + unz.length + " " + Long.toHexString(crc.getValue()));

        System.out.println(new BigInteger("3").pow(500).mod(new BigInteger("1000000007")) + " " + new BigDecimal("1").divide(new BigDecimal("7"), MathContext.DECIMAL128));
        System.out.println(Math.sqrt(2) + " " + Math.exp(1) + " " + Math.sin(1e10) + " " + StrictMath.log(10) + " " + 1e21 + " " + -0.0 + " " + Double.MIN_VALUE + " " + (float) Math.PI);

        // 信号：sun.misc.Signal 的处理函数在单独的线程里运行；最后 SIGTERM 让 JVM 跑关闭钩子后退出
        CountDownLatch hup = new CountDownLatch(1);
        sun.misc.Signal.handle(new sun.misc.Signal("HUP"), s -> { System.out.println("signal " + s.getName()); hup.countDown(); });
        sun.misc.Signal.raise(new sun.misc.Signal("HUP"));
        hup.await();
        Runtime.getRuntime().addShutdownHook(new Thread(() -> System.out.println("shutdown hook")));
        System.out.flush();
        sun.misc.Signal.raise(new sun.misc.Signal("TERM"));
        Thread.sleep(60_000);
        System.out.println("not reached");
    }
}
JAVA
# sun.misc.Signal 会招来"内部专有 API"的警告，编译成功时不显示
javac -d . Main.java 2>javac.log || { cat javac.log; exit 1; }
set +e
java -cp . Main
echo "exit $?"
java -XX:+UseZGC -cp . Main | md5sum
cat > Sleeper.java <<'JAVA'
public class Sleeper {
    public static void main(String[] a) throws Exception {
        System.out.println("sleeper up");
        Thread.sleep(600_000);
    }
}
JAVA
javac -d . Sleeper.java
java -cp . Sleeper > sleeper.out &
pid=$!
# 子进程可能还没创建 sleeper.out：grep -s 不报文件不存在
while ! grep -qs 'sleeper up' sleeper.out; do sleep 0.1; done
jcmd $pid VM.version | tail -n +2
kill $pid
wait $pid
echo "sleeper exit $?"
