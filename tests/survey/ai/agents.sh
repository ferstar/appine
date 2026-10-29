# AI 编程 CLI（docs/rounds/103-ai-cli-survey.md）：Claude Code、Codex、Gemini CLI 不用账号，
# 下面的模拟服务冒充三家的模型 API，按固定的几步要 CLI 调用工具（执行命令、写读改文件、
# grep、glob），打印每一步工具的结果；再看两家用 bubblewrap 的沙箱，以及 tmux 里的交互界面
# （另有 opencode、GitHub Copilot CLI、Crush、Qwen Code，只看版本和界面）。Alpine、Debian
# 的仓库和原生的容器里都能跑，缺的软件包用 apk 或 apt 装；npm 要联网。每一项前面是 "== 名字"
export LANG=C.UTF-8 HOME=/root PATH=/usr/local/bin:$PATH SHELL=/bin/sh
need="nodejs npm python3 git ripgrep tmux bubblewrap socat curl bash"
if command -v apk >/dev/null; then
    apk add -q $need 2>&1 | tail -2
else
    apt-get update -q >/dev/null
    DEBIAN_FRONTEND=noninteractive apt-get install -y -q --no-install-recommends $need \
        ca-certificates procps 2>&1 | tail -1
fi
echo "node $(node --version), npm $(npm --version)"
W=/tmp/ai
cat > /tmp/mock_llm.py <<'PY'
"""模拟三家的模型 API，让 AI 编程 CLI 不用账号也能走完一轮带工具的对话：
Anthropic Messages（Claude Code）、OpenAI Responses（Codex）、Gemini（Gemini CLI）。

主对话（请求里有执行命令的工具）按 STEPS 依次要求调用工具，工具结果都回来之后回答
"all steps done"；别的请求（起标题、判断话题之类）回答 "ok"。每个工具结果追加到 LOG，
普查打印它，看各个工具在 appine 里的结果。每个请求的路径和工具名也记下来。

    python3 mock_llm.py 端口 日志 工作目录
"""
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT, LOG, WORK = int(sys.argv[1]), sys.argv[2], sys.argv[3]

# (动作, 参数)：shell 对三家都一样；文件类的动作按各家的工具名和参数名换
STEPS = [
    ("shell", f"echo shell-ok; expr 6 '*' 7; uname -sr; cd {WORK} && pwd"),
    ("write", (f"{WORK}/hello.py", "print('hi from python')\n")),
    ("read", f"{WORK}/hello.py"),
    ("edit", (f"{WORK}/hello.py", "hi from", "edited by")),
    ("shell", f"python3 {WORK}/hello.py"),
    ("grep", ("edited", WORK)),
    ("glob", ("**/*.py", WORK)),
    ("shell", "sleep 1 & wait; echo background-ok; git --version"),
]


def log(line):
    with open(LOG, "a") as f:
        f.write(line + "\n")


def anthropic_call(step):
    kind, a = step
    return {
        "shell": lambda: ("Bash", {"command": a, "description": "run"}),
        "write": lambda: ("Write", {"file_path": a[0], "content": a[1]}),
        "read": lambda: ("Read", {"file_path": a}),
        "edit": lambda: ("Edit", {"file_path": a[0], "old_string": a[1], "new_string": a[2]}),
        "grep": lambda: ("Grep", {"pattern": a[0], "path": a[1], "output_mode": "content"}),
        "glob": lambda: ("Glob", {"pattern": a[0], "path": a[1]}),
    }[kind]()


def codex_call(step, tools):
    """Codex 只给 shell 和 apply_patch：文件类的动作换成命令。"""
    kind, a = step
    cmd = {
        "shell": lambda: a,
        "write": lambda: f"cat > {a[0]} <<'EOF'\n{a[1]}EOF\necho wrote",
        "read": lambda: f"cat {a}",
        "edit": None,
        "grep": lambda: f"rg -n {a[0]} {a[1]}",
        "glob": lambda: f"rg --files -g '{a[0]}' {a[1]}",
    }[kind]
    if cmd is None:
        path, old, new = a
        if "apply_patch" in tools:
            patch = (f"*** Begin Patch\n*** Update File: {path}\n@@\n-print('{old} python')\n"
                     f"+print('{new} python')\n*** End Patch\n")
            if tools["apply_patch"].get("type") == "custom":
                return "apply_patch", patch, True
            return "apply_patch", json.dumps({"input": patch}), False
        cmd = lambda: f"sed -i 's/{old}/{new}/' {path} && echo edited"
    cmd = cmd()
    for name in ("exec_command", "shell", "local_shell", "shell_command"):
        if name in tools:
            params = tools[name].get("parameters", {}).get("properties", {})
            if "cmd" in params:
                args = {"cmd": cmd}
            elif params.get("command", {}).get("type") == "string":
                args = {"command": cmd}
            else:
                args = {"command": ["bash", "-lc", cmd]}
            if "workdir" in params:
                args["workdir"] = WORK
            return name, json.dumps(args), False
    raise KeyError(f"no shell tool in {list(tools)}")


def gemini_call(step, decls):
    """按声明里的函数名和参数名填：各版本的参数名不同（absolute_path、file_path……）。"""
    kind, a = step
    names = {
        "shell": ["run_shell_command"],
        "write": ["write_file"],
        "read": ["read_file"],
        "edit": ["replace", "edit"],
        "grep": ["grep_search", "search_file_content", "grep"],
        "glob": ["glob"],
    }[kind]
    name = next((n for n in names if n in decls), None)
    if name is None:
        name, kind, a = "run_shell_command", "shell", {"read": f"cat {a}"}.get(kind, str(a))
    props = decls[name]
    pick = lambda *cands: next((c for c in cands if c in props), cands[0])
    if kind == "shell":
        args = {pick("command"): a}
    elif kind == "write":
        args = {pick("file_path", "absolute_path", "path"): a[0], pick("content"): a[1]}
    elif kind == "read":
        args = {pick("absolute_path", "file_path", "path"): a}
    elif kind == "edit":
        args = {pick("file_path", "absolute_path"): a[0], pick("old_string"): a[1],
                pick("new_string"): a[2]}
        if "instruction" in props:
            args["instruction"] = "edit the greeting"
    else:
        args = {pick("pattern"): a[0], pick("path", "dir_path"): a[1]}
    return name, args


def fake_json(schema):
    """按 JSON schema（Gemini 的类型名是大写的）造一个合法的值：别的请求（模型路由的分类）要它。"""
    ty = str(schema.get("type", "object")).lower()
    if schema.get("enum"):
        return schema["enum"][0]
    if ty == "object":
        return {k: fake_json(v) for k, v in schema.get("properties", {}).items()}
    if ty == "array":
        return [fake_json(schema.get("items", {}))]
    if ty in ("integer", "number"):
        return schema.get("minimum", 1)
    if ty == "boolean":
        return False
    return "ok"


def sse(events, named=True):
    out = ""
    for e in events:
        if named:
            out += f"event: {e['type']}\n"
        out += f"data: {json.dumps(e)}\n\n"
    return out.encode()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def reply(self, code, body, ctype="application/json"):
        self.send_response(code)
        self.send_header("content-type", ctype)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        log(f"GET {self.path}")
        if self.path.split("?")[0].endswith("/models"):
            return self.reply(200, b'{"data": [], "models": [], "object": "list"}')
        self.reply(404, b'{"error": {"type": "not_found_error", "message": "mock"}}')

    def do_HEAD(self):
        self.reply(200, b"")

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("content-length", 0)))
        try:
            body = json.loads(raw or b"{}")
        except ValueError:
            body = {}
        path = self.path.split("?")[0]
        if path.endswith("/v1/messages"):
            return self.anthropic(body)
        if path.endswith("/responses"):
            return self.openai_responses(body)
        if ":streamGenerateContent" in path or ":generateContent" in path:
            return self.gemini(body, ":stream" in path)
        if path.endswith("count_tokens") or path.endswith(":countTokens"):
            return self.reply(200, b'{"input_tokens": 10, "totalTokens": 10}')
        log(f"POST {self.path} (unknown)")
        self.reply(404, b'{"error": {"type": "not_found_error", "message": "mock"}}')

    def step(self, api, done, results, tools):
        if done and results:
            log(f"{api} result {done}: {results[-1][:400]!r}")
        if done < len(STEPS):
            log(f"{api} step {done + 1}: {STEPS[done][0]}")
            return STEPS[done]
        return None

    # ---- Anthropic Messages ----
    def anthropic(self, body):
        tools = [t.get("name") for t in body.get("tools", [])]
        blocks = [b for m in body.get("messages", []) if isinstance(m.get("content"), list)
                  for b in m["content"] if b.get("type") == "tool_result"]

        def text(b):
            c = b.get("content")
            s = " ".join(x.get("text", "") for x in c if isinstance(x, dict)) if isinstance(c, list) else str(c)
            return ("ERROR " if b.get("is_error") else "") + s

        main = "Bash" in tools
        step = self.step("anthropic", len(blocks), [text(b) for b in blocks], tools) if main else None
        if main and not blocks:
            log(f"anthropic tools: {tools}")
        model = body.get("model", "claude")
        if step:
            name, args = anthropic_call(step)
            block = {"type": "tool_use", "id": f"toolu_{len(blocks) + 1:04d}", "name": name, "input": {}}
            delta = {"type": "input_json_delta", "partial_json": json.dumps(args)}
            stop, full = "tool_use", dict(block, input=args)
        else:
            t = "all steps done" if main else "ok"
            block, delta = {"type": "text", "text": ""}, {"type": "text_delta", "text": t}
            stop, full = "end_turn", {"type": "text", "text": t}
        usage = {"input_tokens": 10, "output_tokens": 5}
        msg = {"id": "msg_mock", "type": "message", "role": "assistant", "model": model, "content": [],
               "stop_reason": None, "stop_sequence": None, "usage": usage}
        if not body.get("stream"):
            return self.reply(200, json.dumps(dict(msg, content=[full], stop_reason=stop)).encode())
        self.reply(200, sse([
            {"type": "message_start", "message": msg},
            {"type": "content_block_start", "index": 0, "content_block": block},
            {"type": "content_block_delta", "index": 0, "delta": delta},
            {"type": "content_block_stop", "index": 0},
            {"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": None},
             "usage": {"output_tokens": 5}},
            {"type": "message_stop"},
        ]), "text/event-stream")

    # ---- OpenAI Responses ----
    def openai_responses(self, body):
        tools = {t.get("name") or t.get("type"): t for t in body.get("tools", [])}
        outputs = [i for i in body.get("input", []) if isinstance(i, dict)
                   and i.get("type") in ("function_call_output", "custom_tool_call_output")]
        texts = [str(o.get("output"))[:400] for o in outputs]
        main = any(n in tools for n in ("shell", "exec_command", "local_shell", "shell_command"))
        if main and not outputs:
            log(f"codex tools: {list(tools)}")
        step = self.step("codex", len(outputs), texts, tools) if main else None
        n = len(outputs) + 1
        if step:
            name, args, custom = codex_call(step, tools)
            if custom:
                item = {"type": "custom_tool_call", "id": f"ctc_{n}", "call_id": f"call_{n}",
                        "name": name, "input": args, "status": "completed"}
            else:
                item = {"type": "function_call", "id": f"fc_{n}", "call_id": f"call_{n}",
                        "name": name, "arguments": args, "status": "completed"}
        else:
            t = "all steps done" if main else "ok"
            item = {"type": "message", "id": f"msg_{n}", "role": "assistant", "status": "completed",
                    "content": [{"type": "output_text", "text": t, "annotations": []}]}
        resp = {"id": f"resp_{n}", "object": "response", "status": "completed", "output": [item],
                "usage": {"input_tokens": 10, "input_tokens_details": {"cached_tokens": 0},
                          "output_tokens": 5, "output_tokens_details": {"reasoning_tokens": 0},
                          "total_tokens": 15}}
        if not body.get("stream"):
            return self.reply(200, json.dumps(resp).encode())
        self.reply(200, sse([
            {"type": "response.created", "response": dict(resp, status="in_progress", output=[])},
            {"type": "response.output_item.added", "output_index": 0, "item": item},
            {"type": "response.output_item.done", "output_index": 0, "item": item},
            {"type": "response.completed", "response": resp},
        ]), "text/event-stream")

    # ---- Gemini ----
    def gemini(self, body, stream):
        decls = {}
        for t in body.get("tools", []):
            for d in t.get("functionDeclarations", []):
                schema = d.get("parameters") or d.get("parametersJsonSchema") or {}
                decls[d["name"]] = schema.get("properties", {})
        parts = [p for c in body.get("contents", []) for p in c.get("parts", [])
                 if "functionResponse" in p]
        texts = [json.dumps(p["functionResponse"].get("response"))[:400] for p in parts]
        main = "run_shell_command" in decls
        if main and not parts:
            log(f"gemini tools: {sorted(decls)}")
        step = self.step("gemini", len(parts), texts, decls) if main else None
        config = body.get("generationConfig", {})
        schema = config.get("responseSchema") or config.get("responseJsonSchema")
        if step:
            name, args = gemini_call(step, decls)
            part = {"functionCall": {"name": name, "args": args}}
        elif not main and schema:
            part = {"text": json.dumps(fake_json(schema))}
        else:
            part = {"text": "all steps done" if main else "ok"}
        resp = {"candidates": [{"content": {"role": "model", "parts": [part]}, "finishReason": "STOP",
                                "index": 0}],
                "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5, "totalTokenCount": 15},
                "modelVersion": "gemini-mock"}
        if not stream:
            return self.reply(200, json.dumps(resp).encode())
        self.reply(200, sse([resp], named=False), "text/event-stream")


ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
PY
python3 /tmp/mock_llm.py 18080 /tmp/mock.log $W &
mock=$!
M=http://127.0.0.1:18080

# 一个一个装：一次装七个、都命中缓存时 npm 同时解开几个大包，峰值超过开发虚拟机上 App 的
# 内存上限（2 GB）
for p in @anthropic-ai/claude-code @openai/codex @google/gemini-cli opencode-ai @github/copilot \
    @charmland/crush @qwen-code/qwen-code; do
    s=$(date +%s)
    npm install -g --no-audit --no-fund --loglevel=error $p >/tmp/npm.out 2>&1
    echo "npm install $p: $? ($(($(date +%s) - s))s)"
    tail -n 2 /tmp/npm.out
done
echo "memory peak $(($(cat /sys/fs/cgroup/memory.peak) >> 20)) MiB"
for c in claude codex gemini opencode copilot crush qwen; do
    echo "$c: $($c --version 2>&1 | head -n 1)"
done

# 跑一项：工作目录、模拟服务的日志都清空；打印退出码、用时、输出的最后几行和日志
case_() {
    name=$1
    shift
    rm -rf $W && mkdir -p $W && cd $W && : >/tmp/mock.log
    echo "== $name"
    s=$(date +%s)
    timeout 300 "$@" </dev/null >/tmp/case.out 2>&1
    echo "exit $? ($(($(date +%s) - s))s)"
    tail -n 4 /tmp/case.out
    cut -c1-300 /tmp/mock.log
    cd /
}

export ANTHROPIC_BASE_URL=$M ANTHROPIC_API_KEY=sk-ant-mock CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
case_ claude claude -p "do the steps" --allowedTools "Bash Read Write Edit Grep Glob"

mkdir -p $HOME/.codex
cat >$HOME/.codex/config.toml <<EOF
model = "gpt-mock"
model_provider = "mock"
[model_providers.mock]
name = "mock"
base_url = "$M/v1"
wire_api = "responses"
env_key = "MOCK_API_KEY"
EOF
export MOCK_API_KEY=mock
case_ codex codex exec --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox "do the steps"

# 认证方式要先选好；不信任当前目录时 --yolo 不起作用
mkdir -p $HOME/.gemini
echo '{"security": {"auth": {"selectedType": "gemini-api-key"}, "folderTrust": {"enabled": false}}}' \
    >$HOME/.gemini/settings.json
export GOOGLE_GEMINI_BASE_URL=$M GEMINI_API_KEY=mock
case_ gemini gemini -p "do the steps" --yolo

# 沙箱：两家都用 bubblewrap，要用户命名空间（docker 容器里也没有）
case_ codex-sandbox codex exec --skip-git-repo-check -s workspace-write "do the steps"
echo "== codex sandbox"
codex sandbox -- sh -c 'echo inside' 2>&1 | tail -n 3
echo "== bwrap"
bwrap --ro-bind / / --dev /dev --proc /proc true 2>&1 | tail -n 2
mkdir -p $HOME/.claude
echo '{"sandbox": {"enabled": true}}' >$HOME/.claude/settings.json
case_ claude-sandbox claude -p "do the steps" --allowedTools "Bash Read Write Edit Grep Glob"
rm $HOME/.claude/settings.json

# 交互界面：一次只开一个（几个一起开时 appine 里超过 App 的内存上限），在 tmux 里等一会儿
# 截屏，再看所有进程合计用了多少内存（cgroup 的 memory.current：appine 里是 App 的用量，原生
# 是容器的）。Codex 的后台服务在 Alpine 上读不到进程的启动时间（原生也一样），用 --no-daemon
T="tmux -S /tmp/tmux-ai"
mkdir -p $W
for c in claude "codex --no-daemon" gemini opencode copilot crush qwen; do
    $T kill-server 2>/dev/null
    sleep 2
    base=$(($(cat /sys/fs/cgroup/memory.current) >> 20))
    $T new-session -d -s s -x 110 -y 32 -c $W -e TERM=xterm-256color "$c"
    $T set -g remain-on-exit on
    sleep 20
    used=$(($(cat /sys/fs/cgroup/memory.current) >> 20))
    echo "== tui ${c%% *}: $($T display -p -t s:0 '#{pane_current_command} dead=#{pane_dead}'), memory $base -> $used MiB"
    $T capture-pane -p -t s:0 | grep -v '^ *$' | head -n 10 | cut -c1-110
done
$T kill-server
kill $mock
