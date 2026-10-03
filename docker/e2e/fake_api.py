"""Поддельный Anthropic Messages API для стенда e2e: сценарий по тексту запроса пользователя.

Сценарий выбирает последнее сообщение пользователя с текстом (не tool_result), шаг — число уже выполненных
вызовов инструментов после него. Запрос без инструментов (заголовок, проба квоты) получает короткий текст.
Каждый запрос пишется в журнал одной строкой JSON: стенд читает по нему, какие вызовы дошли до API.
"""

import json
import re
import sys
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer



def log(entry):
    """Строка журнала: время UTC, как у журналов orbit, чтобы сопоставлять запросы с событиями session."""
    print(datetime.now(timezone.utc).strftime("%H:%M:%S.%f"), entry, flush=True)


# Режим отвечает файл: драйвер пишет в него `401`, и модель отказывает в ключе, пока файл не станет пустым.
MODE_FILE = "/control/api-mode"

HELLO_OLD = "hello from runner"
HELLO_NEW = "edited by agent"

# Шаги сценариев: (имя инструмента, вход) либо строка — итоговый текст. Строка с `{…}` — шаблон: сводка по
# результатам инструментов и истории (`render`). Сценарий выбирается по самому длинному имени в тексте.
SCENARIOS = {
    "e2e-files": [
        ("Read", {"file_path": "/workspace/hello.txt"}),
        ("Edit", {"file_path": "/workspace/hello.txt", "old_string": HELLO_OLD, "new_string": HELLO_NEW}),
        ("Bash", {"command": "echo ran-on-$(hostname) > /workspace/bash-out.txt && cat /workspace/hello.txt", "description": "run"}),
        "e2e-files-done",
    ],
    "e2e-list": [
        ("mcp__iapetus__runner_list", {}),
        "e2e-list-done",
    ],
    # Смена runner пересоздаёт контейнер: Claude Code в нём продолжает разговор (`--continue`), но прерванный ход
    # не доигрывает, поэтому ход после смены — отдельный сценарий.
    "e2e-switch": [
        ("mcp__iapetus__runner_use", {"runner": "runner-b"}),
        "e2e-switch-done",
    ],
    "e2e-marker": [
        ("Bash", {"command": "echo seen-$(cat /workspace/b/marker.txt) > /workspace/b/seen.txt", "description": "marker"}),
        "e2e-marker-done {context}",
    ],
    "e2e-runners": [
        ("mcp__iapetus__runner_list", {}),
        "e2e-runners-done {runners}",
    ],
    "e2e-clone": [
        ("mcp__iapetus__runner_clone", {"dir": "/home/waldo/repo"}),
        "e2e-clone-done {clone}",
    ],
    "e2e-clone-plain": [
        ("mcp__iapetus__runner_clone", {"dir": "/home/waldo/plain"}),
        "e2e-clone-plain-done {clone}",
    ],
    "e2e-plain-use": [
        ("mcp__iapetus__runner_use", {"runner": "plain"}),
        "e2e-plain-use-done",
    ],
    "e2e-plain-work": [
        ("Bash", {"command": "echo from-plain > /home/waldo/plain/note.txt", "description": "plain"}),
        "e2e-plain-work-done {context}",
    ],
    "e2e-plain-back": [
        ("mcp__iapetus__runner_use", {"runner": "runner-a"}),
        "e2e-plain-back-done",
    ],
    # Проект внутри $HOME runner: чтение и запись вне проекта, закрытый ~/.claude, ссылка в проекте.
    "e2e-home-clone": [
        ("mcp__iapetus__runner_clone", {"dir": "/home/waldo/homeproj"}),
        "e2e-home-clone-done {clone}",
    ],
    "e2e-home-use": [
        ("mcp__iapetus__runner_use", {"runner": "homeproj"}),
        "e2e-home-use-done",
    ],
    "e2e-home": [
        ("Read", {"file_path": "/home/waldo/outside.txt"}),
        ("Write", {"file_path": "/home/waldo/written.txt", "content": "written by agent\n"}),
        ("Read", {"file_path": "/home/waldo/.claude/secret.txt"}),
        ("Read", {"file_path": "/home/waldo/homeproj/link.txt"}),
        "e2e-home-done {home}",
    ],
    "e2e-home-back": [
        ("mcp__iapetus__runner_use", {"runner": "runner-a"}),
        "e2e-home-back-done",
    ],
    "e2e-clone-use": [
        ("mcp__iapetus__runner_use", {"runner": "repo-clone-1"}),
        "e2e-clone-use-done",
    ],
    "e2e-clone-work": [
        ("Bash", {"command": "echo from-clone > /home/waldo/repo-clone-1/note.txt", "description": "clone"}),
        "e2e-clone-work-done {context}",
    ],
    "e2e-clone-back": [
        ("mcp__iapetus__runner_use", {"runner": "runner-a"}),
        "e2e-clone-back-done",
    ],
    # Команды при потерянном и вернувшемся runner: итог — признак ошибки инструмента.
    "e2e-down": [
        ("Bash", {"command": "echo down > /workspace/b/down.txt", "description": "down"}),
        "e2e-down-done error={error}",
    ],
    "e2e-up": [
        ("Bash", {"command": "echo up > /workspace/b/up.txt", "description": "up"}),
        "e2e-up-done error={error} {context}",
    ],
    "e2e-auth": ["e2e-auth-done"],
    "e2e-auth-again": ["e2e-auth-again-done"],
}


def mode():
    try:
        with open(MODE_FILE) as file:
            return file.read().strip()
    except OSError:
        return ""


def block_text(block):
    if isinstance(block, str):
        return block
    if block.get("type") == "text":
        return block.get("text", "")
    return ""


def locate(messages):
    """Сценарий и число вызовов инструментов, выполненных после его запроса."""
    scenario, done = None, 0
    for message in messages:
        if message.get("role") != "user":
            continue
        content = message["content"]
        blocks = [content] if isinstance(content, str) else content
        results = sum(1 for b in blocks if isinstance(b, dict) and b.get("type") == "tool_result")
        text = " ".join(block_text(b) for b in blocks)
        if results:
            done += results
            continue
        named = [name for name in SCENARIOS if name in text]
        if named:
            scenario, done = max(named, key=len), 0
    return scenario, done


def last_result_block(messages):
    """Последний tool_result: по нему видно, чем кончился вызов инструмента."""
    for message in reversed(messages):
        content = message.get("content")
        if message.get("role") != "user" or isinstance(content, str):
            continue
        for block in content:
            if isinstance(block, dict) and block.get("type") == "tool_result":
                return block
    return None


def result_text(block):
    inner = block.get("content") if block else None
    if isinstance(inner, list):
        inner = " ".join(block_text(b) for b in inner)
    return "" if inner is None else str(inner)


def last_result(messages):
    block = last_result_block(messages)
    return None if block is None else result_text(block)[:300]


def scenario_results(messages):
    """Результаты инструментов `(ошибка, текст)` после запроса последнего сценария, по порядку вызовов."""
    results = []
    for message in messages:
        content = message.get("content")
        if message.get("role") != "user" or isinstance(content, str):
            continue
        blocks = [b for b in content if isinstance(b, dict)]
        found = [b for b in blocks if b.get("type") == "tool_result"]
        if found:
            results += [(bool(b.get("is_error")), result_text(b)) for b in found]
        elif any(name in " ".join(block_text(b) for b in blocks) for name in SCENARIOS):
            results = []
    return results


def home_summary(request):
    """Итоги проверок проекта внутри $HOME: чтение вне проекта, запись, закрытый ~/.claude, ссылка, CLAUDE.md предка."""
    results = scenario_results(request.get("messages", []))
    if len(results) < 4:
        return f"результатов {len(results)} из 4"
    (_, outside), (write_error, _), (secret_error, secret), (_, link) = results[:4]
    dump = json.dumps(request, ensure_ascii=False)
    seen = "claude-md-seen-by-agent" in dump
    cwd = re.findall(r"Primary working directory: ([^\\\s\"]+)", dump)
    return (
        f"read={'ok' if 'home outside' in outside else 'no'} write={'no' if write_error else 'ok'} "
        f"claudedir={'open' if 'runner-secret' in secret else 'closed' if secret_error else 'empty'} link={'ok' if 'linked' in link else 'no'} "
        f"claudemd={'seen' if seen else 'absent'} cwd={cwd[-1] if cwd else '?'}"
    )


def runners_summary(text):
    """`имя:состояние` каждого runner из ответа `runner_list`, по алфавиту; временный — с `+tmp`."""
    try:
        runners = json.loads(text)
        return " ".join(sorted(f"{r['name']}:{r['presence']}{'+tmp' if r['temporary'] else ''}" for r in runners))
    except (ValueError, KeyError, TypeError):
        return f"не разобрано: {text[:80]}"


def clone_summary(text):
    try:
        return json.loads(text)["name"]
    except (ValueError, KeyError, TypeError):
        return f"не разобрано: {text[:80]}"


def context(request):
    """Что агент знает после смены runner: прежняя реплика в истории и рабочий каталог из системного запроса."""
    remembered = any(
        message.get("role") == "assistant" and "e2e-files-done" in " ".join(block_text(b) for b in message["content"] if isinstance(b, dict))
        for message in request.get("messages", [])
        if not isinstance(message.get("content"), str)
    )
    dump = json.dumps(request, ensure_ascii=False)
    # Путь проекта называет сам Claude Code (окружение в описании инструмента Agent) и документ окружения Iapetus.
    claude = re.findall(r"Primary working directory: ([^\\\s\"]+)", dump)
    iapetus = re.findall(r"Проект лежит в `([^`]+)`", dump)
    return (
        f"history={'kept' if remembered else 'lost'} cwd={claude[-1] if claude else '?'} "
        f"doc={iapetus[-1] if iapetus else '?'}"
    )


def render(template, request):
    block = last_result_block(request.get("messages", []))
    text = result_text(block)
    return template.format(
        context=context(request),
        runners=runners_summary(text),
        clone=clone_summary(text),
        home=home_summary(request),
        error=bool(block and block.get("is_error")),
    )


def sse(event, data):
    return f"event: {event}\ndata: {json.dumps(data)}\n\n".encode()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def reply(self, code, body, content_type="application/json"):
        self.send_response(code)
        self.send_header("content-type", content_type)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_HEAD(self):
        log(json.dumps({"head": self.path}))
        self.send_response(200)
        self.send_header("content-length", "0")
        self.end_headers()

    def do_GET(self):
        log(json.dumps({"get": self.path}))
        self.reply(200, b"{}")

    def do_POST(self):
        length = int(self.headers.get("content-length", 0))
        request = json.loads(self.rfile.read(length) or b"{}")
        path = self.path.split("?")[0]
        if path != "/v1/messages":
            log(json.dumps({"post": self.path}))
            self.reply(200, b"{}")
            return
        if mode() == "401":
            log(json.dumps({"messages": "401", "stream": request.get("stream", False)}))
            body = {"type": "error", "error": {"type": "authentication_error", "message": "invalid x-api-key"}}
            self.reply(401, json.dumps(body).encode())
            return
        tools = [tool["name"] for tool in request.get("tools", [])]
        scenario, done = locate(request.get("messages", []))
        step = None
        if tools and scenario is not None:
            step = SCENARIOS[scenario][min(done, len(SCENARIOS[scenario]) - 1)]
        if step is not None and not isinstance(step, str) and step[0] not in tools:
            step = f"инструмента {step[0]} нет среди {tools}"
        if isinstance(step, str) and "{" in step:
            step = render(step, request)
        log(json.dumps({"messages": request.get("model"), "tools": len(tools), "scenario": scenario, "done": done,
                        "step": step, "result": last_result(request.get("messages", [])), "stream": request.get("stream", False)}, ensure_ascii=False))
        if step is None or isinstance(step, str):
            content = [{"type": "text", "text": step or "ok"}]
            stop = "end_turn"
        else:
            content = [{"type": "tool_use", "id": f"toolu_{scenario}_{done}_{len(request.get('messages', []))}", "name": step[0], "input": step[1]}]
            stop = "tool_use"
        usage = {"input_tokens": 10, "output_tokens": 5}
        message = {"id": f"msg_{done}", "type": "message", "role": "assistant", "model": request.get("model", "fake"),
                   "content": content, "stop_reason": stop, "stop_sequence": None, "usage": usage}
        if not request.get("stream"):
            self.reply(200, json.dumps(message).encode())
            return
        out = [sse("message_start", {"type": "message_start", "message": {**message, "content": [], "stop_reason": None}})]
        for index, block in enumerate(content):
            if block["type"] == "text":
                out.append(sse("content_block_start", {"type": "content_block_start", "index": index, "content_block": {"type": "text", "text": ""}}))
                out.append(sse("content_block_delta", {"type": "content_block_delta", "index": index, "delta": {"type": "text_delta", "text": block["text"]}}))
            else:
                start = {**block, "input": {}}
                out.append(sse("content_block_start", {"type": "content_block_start", "index": index, "content_block": start}))
                out.append(sse("content_block_delta", {"type": "content_block_delta", "index": index, "delta": {"type": "input_json_delta", "partial_json": json.dumps(block["input"])}}))
            out.append(sse("content_block_stop", {"type": "content_block_stop", "index": index}))
        out.append(sse("message_delta", {"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": None}, "usage": {"output_tokens": 5}}))
        out.append(sse("message_stop", {"type": "message_stop"}))
        self.reply(200, b"".join(out), "text/event-stream")


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8080
    ThreadingHTTPServer(("0.0.0.0", port), Handler).serve_forever()
