"""Сценарий стенда e2e: waldo worker в pty, экран читается эмулятором терминала (pyte).

Каждый шаг печатает `PASS шаг (время)` или `FAIL шаг: причина` и экран на момент отказа. Шаг, от которого зависят
следующие, останавливает сценарий. Код выхода — число неудавшихся шагов.

Сценарий разбит на этапы, порядок: connect, files, reattach, exit, runners, clone, cloneuse, plainclone, home, switch, marker,
lost, login, revoke, end. `E2E_STEP` — этапы через пробел или запятую: идут только они и этапы, без которых они не
работают (connect нужен всем; reattach, exit, runners, clone, home — files; cloneuse, plainclone — clone; marker —
switch, files; lost, login, revoke — marker). Пусто — весь сценарий.

Драйвер не видит Docker: остановку и запуск runner выполняет скрипт стенда по заявкам в `/control` (`host`), а режим
поддельного API задаёт файл `/control/api-mode`.

Ожидание идёт по экрану, а не по времени: `wait` возвращается, как только нужное появилось, и отказывает по
одному из двух пределов — экран не менялся `idle` секунд или вышел бюджет шага.
"""

import os
import subprocess
import sys
import time
from datetime import datetime, timezone
from collections import defaultdict
from pathlib import Path

import pexpect
import pyte
from pyte.screens import StaticDefaultDict

COLS, ROWS = 140, 40
SHARED = Path("/shared")
CONF = Path("/tmp/worker.conf")
WORKSPACE_A = Path("/ws-a")
WORKSPACE_B = Path("/ws-b/b")
HOME_A = Path("/home-a")
CONTROL = Path("/control")
DETACH = b"\x1d"
ENTER = b"\r"
DOWN = b"\x1b[B"
ESC = b"\x1b"

ALTERNATE_SCREEN = (47, 1047, 1049)
IDLE = 2.0
RAW_TAIL = 400

failures = []
started = time.time()
last_step = started


class AlternateScreen(pyte.Screen):
    """pyte с альтернативным экраном: без него worker TUI рисовал бы поверх остатков экрана агента.

    Вход в альтернативный экран даёт чистый буфер, выход возвращает прежний, как настоящий терминал.
    """

    main = None

    def set_mode(self, *modes, **kwargs):
        if kwargs.get("private") and any(mode in ALTERNATE_SCREEN for mode in modes) and self.main is None:
            self.main = self.buffer
            self.buffer = defaultdict(lambda: StaticDefaultDict(self.default_char))
            self.cursor_position()
            self.dirty.update(range(self.lines))
        super().set_mode(*modes, **kwargs)

    def reset_mode(self, *modes, **kwargs):
        if kwargs.get("private") and any(mode in ALTERNATE_SCREEN for mode in modes) and self.main is not None:
            self.buffer = self.main
            self.main = None
            self.dirty.update(range(self.lines))
        super().reset_mode(*modes, **kwargs)


class Screen:
    """pexpect + pyte: байты процесса идут в эмулятор, `text()` — текущий экран."""

    def __init__(self, child):
        self.child = child
        self.screen = AlternateScreen(COLS, ROWS)
        self.stream = pyte.ByteStream(self.screen)
        self.alive = True
        self.received = 0
        self.last_data = time.time()
        self.raw_tail = b""
        self.raw_all = bytearray()
        self.sent = []
        self.why = ""

    def pump_once(self, timeout):
        """Читает порцию вывода в эмулятор; возвращает, пришли ли байты."""
        try:
            data = self.child.read_nonblocking(65536, timeout=timeout)
        except pexpect.TIMEOUT:
            return False
        except pexpect.EOF:
            self.alive = False
            return False
        self.received += len(data)
        self.last_data = time.time()
        self.raw_tail = (self.raw_tail + data)[-RAW_TAIL:]
        self.raw_all += data
        self.stream.feed(data)
        return True

    def pump(self, seconds):
        end = time.time() + seconds
        while time.time() < end and self.alive:
            self.pump_once(0.1)

    def settle(self, quiet=0.15, cap=2.0):
        """Читает вывод, пока он не стихнет на `quiet` секунд, но не дольше `cap`."""
        end = time.time() + cap
        while time.time() < end and self.alive:
            if not self.pump_once(quiet):
                return

    def rows(self):
        """Строки экрана по ячейкам: номер символа равен номеру столбца, вторая ячейка широкого символа — пробел.

        `screen.display` на широких символах (значки runner) падает с IndexError.
        """
        return ["".join(self.screen.buffer[y][x].data or " " for x in range(COLS)) for y in range(ROWS)]

    def text(self):
        return "\n".join(line.rstrip() for line in self.rows()).rstrip()

    def wait_until(self, done, budget=60, idle=IDLE):
        """Ждёт, пока `done(текст экрана)` не станет истиной. Отказ — экран не менялся `idle` с или вышел `budget`."""
        start = time.time()
        quiet_since = start
        seen = self.text()
        while True:
            if done(seen):
                return True
            if not self.alive:
                self.why = "процесс worker завершился"
                return False
            now = time.time()
            if now - start >= budget:
                self.why = f"бюджет {budget} с вышел"
                return False
            if now - quiet_since >= idle:
                self.why = f"экран не менялся {idle:g} с"
                return False
            if self.pump_once(0.1):
                text = self.text()
                if text != seen:
                    seen = text
                    quiet_since = time.time()

    def wait(self, needle, budget=60, idle=IDLE):
        """Ждёт `needle` на экране. При отказе в причине сказано, дошла ли строка до драйвера вообще."""
        if self.wait_until(lambda text: needle in text, budget, idle):
            return True
        delivered = "дошла" if needle.encode() in self.raw_all else "не дошла"
        self.why += f", строка {delivered} в потоке байт"
        return False

    def poll(self, action, done, budget=60, interval=1.0):
        """Выполняет `action` раз в `interval`, пока `done(текст)` не станет истиной; предел один — бюджет."""
        end = time.time() + budget
        while time.time() < end:
            action()
            if self.wait_until(done, budget=interval, idle=interval + 1):
                return True
            self.why = "условие не наступило"
            if not self.alive:
                return False
        self.why = f"бюджет {budget} с вышел"
        return False

    def send(self, data):
        self.sent.append((stamp(), data))
        self.child.send(data)

    def type(self, text):
        """Печатает текст и ждёт, пока он появится на экране."""
        self.child.send(text.encode())
        return self.wait(text, budget=10, idle=3)


def stamp():
    """Время UTC в том же виде, что у журналов orbit и поддельного API."""
    return datetime.now(timezone.utc).strftime("%H:%M:%S.%f")


def step(name, ok, reason="", screen=None):
    global last_step
    now = time.time()
    if ok:
        print(f"PASS {name} ({now - last_step:.1f} с)", flush=True)
        last_step = now
        return True
    if screen is not None and screen.why:
        reason = f"{reason} [{screen.why}]"
    print(f"FAIL {name}: {reason} ({stamp()})", flush=True)
    if screen is not None:
        print(
            f"--- экран (получено {screen.received} байт, последние {now - screen.last_data:.1f} с назад) ---\n"
            + screen.text()
            + f"\n--- последние отправленные клавиши: {screen.sent[-6:]!r}"
            + f"\n--- хвост потока: {screen.raw_tail!r}\n-------------",
            flush=True,
        )
    failures.append(name)
    last_step = now
    return False


def host(command, timeout=60):
    """Просит скрипт стенда выполнить `kill|start|state <runner>`; возвращает его вывод или `None` по таймауту."""
    number = f"{int(time.time() * 1000)}"
    request = CONTROL / f"req-{number}"
    ack = CONTROL / f"ack-{number}"
    request.write_text(command + "\n")
    end = time.time() + timeout
    while time.time() < end:
        if ack.exists():
            return ack.read_text().strip()
        time.sleep(0.1)
    return None


def set_api(mode):
    """Режим поддельного API: `401` — отказ в ключе, пусто — обычные ответы."""
    temp = CONTROL / "api-mode.tmp"
    temp.write_text(mode)
    temp.replace(CONTROL / "api-mode")


def run_turn(screen, text, done, budget=120):
    """Печатает реплику агенту и ждёт его ответа `done`."""
    screen.type(text)
    screen.send(ENTER)
    return screen.wait(done, budget, idle=20)


def list_poll(screen, done, budget=60):
    """Обновляет список сессий (r), пока `done(экран)` не станет истиной."""
    return screen.poll(lambda: screen.send(b"r"), done, budget=budget, interval=1.0)


def enter_agent(screen):
    """Из списка сессий входит в агента, когда он запущен, и ждёт его приглашения."""
    if not screen.poll(lambda: screen.send(ENTER), in_agent, budget=120, interval=1.0):
        return False
    agent_ready(screen)
    if "❯" not in screen.text():
        screen.why = "приглашения агента на экране нет: агент вышел или не нарисовался"
        return False
    return True


def pick(screen, label):
    """Ставит выделение списка выбора на пункт с `label` (выделенная строка — reverse) и подтверждает выбор."""
    for _ in range(12):
        for row, line in enumerate(screen.rows()):
            column = line.find(label)
            if column >= 0:
                if screen.screen.buffer[row][column].reverse:
                    screen.send(ENTER)
                    screen.settle()
                    return True
                screen.send(DOWN)
                screen.settle()
                break
        else:
            return False
    return False


def in_list(text):
    return "Enter вход" in text


def in_agent(text):
    return bool(text) and "Сессии" not in text


def agent_ready(screen):
    """Ждёт приглашение агента и тишину после него: копия экрана приходит сразу, а запущенный заново агент ещё рисует."""
    screen.wait("❯", 30, idle=10)
    screen.settle(quiet=0.5, cap=5)


def connect(screen):
    if not step("worker показывает список сессий", screen.wait("Enter вход", 30, idle=30), "TUI не нарисован", screen):
        return False
    # Runner регистрируются параллельно: ждём, пока оба появятся в диалоге выбора runner (клавиша c; r обновляет список).
    shown = {"text": ""}

    def open_dialog():
        screen.send(b"c")
        screen.settle()
        shown["text"] = screen.text()
        screen.send(ESC)
        screen.settle()
        if "runner-a" not in shown["text"] or "runner-b" not in shown["text"]:
            screen.send(b"r")
            screen.settle(cap=0.5)

    runners_ok = screen.poll(open_dialog, lambda _: "runner-a" in shown["text"] and "runner-b" in shown["text"], budget=60, interval=0.3)
    if not step("оба runner зарегистрировались и видны в TUI", runners_ok, "в списке runner нет runner-a и runner-b", screen):
        return False

    # Новое подключение: агент, затем runner-a. Ключа агента нет: вход в агента выполняет пользователь, а стенд
    # кладёт в том состояния фиктивные учётные данные (см. `claude-login.sh`).
    screen.send(b"n")
    screen.wait("Claude Code", 10)
    pick(screen, "Claude Code")
    if not step("после агента мастер предлагает runner", screen.wait("Новое подключение: runner", 10), "нет выбора runner", screen):
        return False
    pick(screen, "runner-a")

    # Вход в агента — после `Ready`; экран становится экраном Claude Code.
    woke = screen.wait_until(lambda text: "Агент просыпается" in text or in_agent(text), budget=10, idle=5)
    step("server принял запрос на сессию", woke, "нет ответа server", screen)
    failure_marks = ("не поднята", "завершилась", "не удался")
    entered = screen.wait_until(lambda text: in_agent(text) or any(mark in text for mark in failure_marks), budget=120, idle=120)
    if not step("worker вошёл в TUI агента", entered and in_agent(screen.text()), "Ready не пришёл или вход не удался", screen):
        return False
    agent_ready(screen)
    print("--- экран агента после входа ---\n" + screen.text() + "\n---", flush=True)
    return True


def files(screen):
    screen.type("e2e-files")
    screen.send(ENTER)
    done = screen.wait("e2e-files-done", 120, idle=10)
    step("агент отработал сценарий чтения, правки и команды", done, "ответ e2e-files-done не появился", screen)
    hello = WORKSPACE_A / "hello.txt"
    step(
        "правка файла дошла до runner",
        hello.exists() and hello.read_text() == "edited by agent\n",
        f"hello.txt на runner: {hello.read_text() if hello.exists() else 'нет файла'!r}",
    )
    out = WORKSPACE_A / "bash-out.txt"
    step(
        "команда выполнена на runner",
        out.exists() and out.read_text().strip() == "ran-on-runner-a",
        f"bash-out.txt на runner: {out.read_text() if out.exists() else 'нет файла'!r}, каталог: {sorted(p.name for p in WORKSPACE_A.iterdir())}",
    )
    return True


def reattach(screen):
    # Выход из агента и повторный вход: зритель получает экран из копии агента.
    screen.send(DETACH)
    step("Ctrl-] возвращает список сессий", screen.wait("Enter вход", 10, idle=5), "список не вернулся", screen)
    screen.send(ENTER)
    step("повторный вход показывает экран агента", screen.wait("e2e-files-done", 20, idle=5), "экран агента не восстановлен", screen)
    return True


def exit_agent(screen):
    # `/exit` завершает процесс Claude Code. Session запускает его заново с `--continue`: канал worker закрывается,
    # TUI возвращается в список с сообщением, состояние не уходит в «сбой», и после входа на экране прежний разговор.
    screen.type("/exit")
    screen.send(ENTER)
    closed = screen.wait_until(in_list, 60, idle=30)
    if not step("после /exit worker вернулся в список сессий", closed, "список сессий не вернулся", screen):
        return False
    listed = screen.text()
    step("TUI сообщает, что агент перезапущен", "перезапущен" in listed, "в списке нет «перезапущен»", screen)
    step("сессия после /exit не в сбое", "сбой" not in listed, "состояние сессии — сбой", screen)

    entered = screen.poll(lambda: screen.send(ENTER), in_agent, budget=120, interval=1.0)
    if not step("вход в перезапущенного агента", entered, "агент не принял вход", screen):
        return False
    agent_ready(screen)
    step("перезапущенный агент показывает прежний разговор", "e2e-files-done" in screen.text(), "прежней реплики на экране нет", screen)

    # Claude Code после `--continue` сам к API не ходит: пока человек не напишет, сессия «ждёт входа» с причиной
    # перезапуска; первый ответ сервиса причину снимает.
    screen.send(DETACH)
    waiting = screen.wait_until(lambda text: in_list(text) and "агент перезапущен" in text and "сбой" not in text, 10, idle=5)
    step("до первого хода список показывает причину «агент перезапущен»", waiting, "причины нет", screen)
    screen.send(ENTER)
    agent_ready(screen)
    screen.type("e2e-list")
    screen.send(ENTER)
    step("перезапущенный агент отработал ход", screen.wait("e2e-list-done", 60, idle=10), "ответ e2e-list-done не появился", screen)
    screen.send(DETACH)
    working = screen.poll(lambda: screen.send(b"r"), lambda text: "работает" in text and "перезапущен" not in text, budget=30, interval=1.0)
    step("после хода сессия работает, причины перезапуска нет", working, "состояние не стало «работает»", screen)
    screen.send(ENTER)
    agent_ready(screen)
    return True


def line_with(screen, needle):
    """Строка экрана с `needle` (последняя) или пустая строка."""
    found = [line.strip() for line in screen.rows() if needle in line]
    return found[-1] if found else ""


def line_with_text(text, needle):
    found = [line for line in text.splitlines() if needle in line]
    return found[-1] if found else ""


def runner_dialog_shows(screen, names, budget=60):
    """Открывает диалог выбора runner (c), пока в нём не появятся все `names`; r обновляет список."""
    shown = {"text": ""}

    def open_dialog():
        screen.send(b"c")
        screen.settle()
        shown["text"] = screen.text()
        screen.send(ESC)
        screen.settle()
        if not all(name in shown["text"] for name in names):
            screen.send(b"r")
            screen.settle(cap=0.5)

    return screen.poll(open_dialog, lambda _: all(name in shown["text"] for name in names), budget=budget, interval=0.3)


def runners(screen):
    # Агент вызывает MCP-инструмент runner_list: поддельный API разбирает ответ и пишет сводку на экран.
    expected = "e2e-runners-done runner-a:Busy runner-b:Free"
    done = run_turn(screen, "e2e-runners", "e2e-runners-done")
    line = line_with(screen, "e2e-runners-done")
    step("runner_list вернул оба runner с верным состоянием", done and expected in line, f"сводка агента: {line!r}", screen)
    return True


def clone(screen):
    # Два клона на машине runner-a: каталог репозитория (worktree рядом и ветка iapetus/<имя>) и обычный каталог.
    done = run_turn(screen, "e2e-clone", "e2e-clone-done")
    line = line_with(screen, "e2e-clone-done")
    step("runner_clone создал клон репозитория", done and "e2e-clone-done repo-clone-1" in line, f"итог агента: {line!r}", screen)
    done = run_turn(screen, "e2e-clone-plain", "e2e-clone-plain-done")
    line = line_with(screen, "e2e-clone-plain-done")
    step("runner_clone создал клон обычного каталога", done and "e2e-clone-plain-done plain" in line, f"итог агента: {line!r}", screen)
    worktree = HOME_A / "repo-clone-1"
    branch = HOME_A / "repo/.git/refs/heads/iapetus/repo-clone-1"
    step(
        "клон репозитория — worktree на своей ветке",
        worktree.is_dir() and branch.exists(),
        f"worktree {worktree.is_dir()}, ветка {branch.exists()}, каталог: {sorted(p.name for p in HOME_A.iterdir())}",
    )

    screen.send(DETACH)
    screen.wait("Enter вход", 10, idle=5)
    step("клоны зарегистрированы и видны в TUI", runner_dialog_shows(screen, ["repo-clone-1", "plain"]), "в диалоге runner нет клонов", screen)
    screen.send(ENTER)
    agent_ready(screen)

    expected = "e2e-runners-done plain:Free+tmp repo-clone-1:Free+tmp runner-a:Busy runner-b:Free"
    run_turn(screen, "e2e-runners", expected)
    line = line_with(screen, expected)
    step("runner_list показывает клоны как свободные временные runner", expected in line, f"сводка агента: {line!r}", screen)
    return True


def vcs_show(path):
    result = subprocess.run(
        ["git", "-c", "safe.directory=*", "-C", str(HOME_A / "repo"), "show", path],
        capture_output=True, text=True, check=False,
    )
    return result.stdout if result.returncode == 0 else f"git: {result.stderr.strip()}"


def go_to_runner(screen, scenario, name, what):
    """Агент сам переключает сессию на runner `name`: канал агента закрывается, имя появляется в списке сессий."""
    screen.type(scenario)
    screen.send(ENTER)
    step(f"канал агента закрыт {what}", screen.wait_until(in_list, 120, idle=20), "список сессий не вернулся", screen)
    moved = list_poll(screen, lambda text: name in text, budget=120)
    return step(f"список сессий показывает {name}", moved, f"в строке сессии нет {name}", screen)


def cloneuse(screen):
    # Переход на клон-worktree: агент в новом контейнере продолжает разговор под путём клона.
    if not go_to_runner(screen, "e2e-clone-use", "repo-clone-1", "переходом на клон репозитория"):
        return False
    if not step("вход в агента на клоне репозитория", enter_agent(screen), "агент не принял вход", screen):
        return False
    # В worktree Claude Code не принимает доверие по `/`; ответ по умолчанию «No, exit» завершил бы агента.
    asked = "Quick safety check" in screen.text()
    step("агент на клоне-worktree не спрашивает доверие каталогу", not asked, "Claude Code показал вопрос о доверии каталогу", screen)

    screen.type("e2e-clone-work")
    screen.send(ENTER)
    worked = screen.wait("e2e-clone-work-done", 120, idle=10)
    line = line_with(screen, "e2e-clone-work-done")
    step(
        "на клоне репозитория разговор продолжен под путём клона",
        worked and "history=kept cwd=/home/waldo/repo-clone-1" in line,
        f"итог поддельного API: {line!r}",
        screen,
    )
    note = HOME_A / "repo-clone-1/note.txt"
    step("команда выполнена в worktree клона", note.exists() and note.read_text() == "from-clone\n", f"note.txt: {note.read_text() if note.exists() else 'нет файла'!r}")

    # Возврат на runner-a: сессия ушла с клона, и он убирается — незакоммиченное в ветку, worktree удаляется.
    screen.type("e2e-clone-back")
    screen.send(ENTER)
    step("канал агента закрыт возвратом на runner-a", screen.wait_until(in_list, 120, idle=20), "список сессий не вернулся", screen)
    back = list_poll(screen, lambda text: "runner-a" in text and "repo-clone-1" not in text, budget=120)
    step("список сессий показывает runner-a", back, "в строке сессии нет runner-a", screen)
    end = time.time() + 60
    while (HOME_A / "repo-clone-1").exists() and time.time() < end:
        time.sleep(0.5)
    step("worktree клона удалён после ухода сессии", not (HOME_A / "repo-clone-1").exists(), "каталог repo-clone-1 остался")
    shown = vcs_show("iapetus/repo-clone-1:note.txt")
    step("ветка клона осталась с автокоммитом", shown == "from-clone\n", f"show iapetus/repo-clone-1:note.txt: {shown!r}")
    return enter_agent(screen)


def plainclone(screen):
    # Клон обычного каталога: просто точка входа, при уходе сессии каталог остаётся как есть.
    if not go_to_runner(screen, "e2e-plain-use", "plain", "переходом на клон каталога"):
        return False
    if not step("вход в агента на клоне каталога", enter_agent(screen), "агент не принял вход", screen):
        return False
    screen.type("e2e-plain-work")
    screen.send(ENTER)
    worked = screen.wait("e2e-plain-work-done", 120, idle=10)
    line = line_with(screen, "e2e-plain-work-done")
    step(
        "на клоне каталога разговор продолжен под путём клона",
        worked and "history=kept cwd=/home/waldo/plain" in line,
        f"итог поддельного API: {line!r}",
        screen,
    )
    screen.type("e2e-plain-back")
    screen.send(ENTER)
    step("канал агента закрыт возвратом на runner-a", screen.wait_until(in_list, 120, idle=20), "список сессий не вернулся", screen)
    back = list_poll(screen, lambda text: "runner-a" in text and "plain" not in text, budget=120)
    step("список сессий показывает runner-a после клона каталога", back, "в строке сессии нет runner-a", screen)
    time.sleep(3)
    note = HOME_A / "plain/note.txt"
    step(
        "каталог клона без репозитория остался нетронутым",
        note.read_text() == "from-plain\n" and not (HOME_A / "plain/.git").exists() if note.exists() else False,
        f"plain: {sorted(p.name for p in (HOME_A / 'plain').iterdir())}",
    )
    return enter_agent(screen)


def home(screen):
    # Проект внутри $HOME runner (0700): session монтирует FUSE $HOME runner, проект лежит поверх неё.
    done = run_turn(screen, "e2e-home-clone", "e2e-home-clone-done")
    line = line_with(screen, "e2e-home-clone-done")
    if not step("runner_clone создал клон внутри $HOME", done and "e2e-home-clone-done homeproj" in line, f"итог агента: {line!r}", screen):
        return False
    # Клон регистрируется как runner не сразу: переход раньше этого получил бы «runner не найден».
    screen.send(DETACH)
    screen.wait("Enter вход", 10, idle=5)
    if not step("клон внутри $HOME виден в TUI", runner_dialog_shows(screen, ["homeproj"]), "в диалоге runner нет клона", screen):
        return False
    screen.send(ENTER)
    agent_ready(screen)
    if not go_to_runner(screen, "e2e-home-use", "homeproj", "переходом на проект внутри $HOME"):
        return False
    if not step("вход в агента на проекте внутри $HOME", enter_agent(screen), "агент не принял вход", screen):
        return False
    screen.type("e2e-home")
    screen.send(ENTER)
    worked = screen.wait("e2e-home-done", 120, idle=20)
    line = line_with(screen, "e2e-home-done")
    step("агент отработал сценарий проекта внутри $HOME", worked, "ответ e2e-home-done не появился", screen)
    checks = [
        ("read=ok", "встроенный Read файла в $HOME вне проекта отдаёт содержимое runner"),
        ("write=ok", "Write в $HOME вне проекта принят"),
        ("claudedir=closed", "~/.claude runner недоступен агенту"),
        ("link=ok", "символическая ссылка в проекте читается"),
        ("claudemd=seen", "CLAUDE.md в предке проекта попал в запрос агента"),
        ("cwd=/home/waldo/homeproj", "агент работает под путём проекта внутри $HOME"),
    ]
    for mark, name in checks:
        step(name, mark in line, f"в итоге поддельного API нет {mark}: {line!r}", screen)
    written = HOME_A / "written.txt"
    step(
        "Write в $HOME вне проекта дошёл до runner",
        written.exists() and written.read_text() == "written by agent\n",
        f"written.txt на runner: {written.read_text() if written.exists() else 'нет файла'!r}",
    )
    screen.type("e2e-home-back")
    screen.send(ENTER)
    step("канал агента закрыт возвратом на runner-a", screen.wait_until(in_list, 120, idle=20), "список сессий не вернулся", screen)
    back = list_poll(screen, lambda text: "runner-a" in text and "homeproj" not in text, budget=120)
    step("список сессий показывает runner-a после проекта внутри $HOME", back, "в строке сессии нет runner-a", screen)
    return enter_agent(screen)


def switch(screen):
    # Смена runner: агент сам вызывает runner_use. Контейнер пересоздаётся, канал агента закрывается, и Claude Code
    # в новом контейнере продолжает разговор, но прерванный ход не доигрывает.
    screen.type("e2e-switch")
    screen.send(ENTER)
    step("канал агента закрыт сменой runner", screen.wait_until(in_list, 120, idle=20), "список сессий не вернулся", screen)
    moved = screen.poll(lambda: screen.send(b"r"), lambda text: "runner-b" in text, budget=120, interval=1.0)
    step("список сессий показывает runner-b", moved, "в строке сессии нет runner-b", screen)

    # Вход возможен, когда агент нового контейнера запущен: до этого Enter отказывает.
    entered = screen.poll(lambda: screen.send(ENTER), in_agent, budget=120, interval=1.0)
    if not step("повторный вход в агента после смены runner", entered, "агент нового контейнера не принял вход", screen):
        return False
    agent_ready(screen)
    return True


def marker(screen):
    screen.type("e2e-marker")
    screen.send(ENTER)
    marked = screen.wait("e2e-marker-done", 120, idle=10)
    step("агент отработал ход после смены runner", marked, "ответ e2e-marker-done не появился", screen)
    # Поддельный API смотрит на запрос: прежняя реплика агента в истории и новый путь проекта в системном запросе.
    line = line_with(screen, "e2e-marker-done")
    step(
        "после смены runner разговор продолжен под новым путём проекта",
        "history=kept cwd=/workspace/b" in line,
        f"итог поддельного API: {line!r}",
        screen,
    )
    seen = WORKSPACE_B / "seen.txt"
    step(
        "команда после смены runner выполнена на runner-b",
        seen.exists() and seen.read_text().strip() == "seen-marker from runner-b",
        f"seen.txt на runner-b: {seen.read_text() if seen.exists() else 'нет файла'!r}",
    )
    return True


def lost(screen):
    # Runner пропал посреди сессии: агент жив, но команды не идут; вернулся — сессия снова работает.
    step("runner-b остановлен", host("kill runner-b") is not None, "стенд не выполнил заявку")
    screen.send(DETACH)
    screen.wait("Enter вход", 10, idle=5)
    warned = list_poll(screen, lambda text: "предупреждение" in text and "runner не подключён" in text, budget=60)
    step("сессия показывает предупреждение «runner не подключён»", warned, "нет предупреждения", screen)
    if not step("агент жив и принимает вход без runner", enter_agent(screen), "агент не принял вход", screen):
        return False
    done = run_turn(screen, "e2e-down", "e2e-down-done")
    line = line_with(screen, "e2e-down-done")
    step("команда без runner завершилась ошибкой", done and "e2e-down-done error=True" in line, f"итог поддельного API: {line!r}", screen)
    down = WORKSPACE_B / "down.txt"
    step("команда без runner не выполнена", not down.exists(), "down.txt появился при остановленном runner")

    screen.send(DETACH)
    screen.wait("Enter вход", 10, idle=5)
    step("runner-b запущен снова", host("start runner-b") is not None, "стенд не выполнил заявку")
    back = list_poll(screen, lambda text: "работает" in text and "предупреждение" not in text, budget=120)
    step("вернувшийся runner снимает предупреждение", back, "состояние не вернулось в «работает»", screen)
    if not step("вход в агента после возврата runner", enter_agent(screen), "агент не принял вход", screen):
        return False
    done = run_turn(screen, "e2e-up", "e2e-up-done")
    line = line_with(screen, "e2e-up-done")
    step("команда после возврата runner выполнена", done and "e2e-up-done error=False history=kept" in line, f"итог поддельного API: {line!r}", screen)
    up = WORKSPACE_B / "up.txt"
    step("команда после возврата runner дошла до runner-b", up.exists() and up.read_text() == "up\n", f"up.txt: {up.read_text() if up.exists() else 'нет файла'!r}")
    return True


def login(screen):
    # Сервис отказал в ключе (401): сессия ждёт входа. Следующий ответ 2xx снимает причину, агент не перезапускается.
    set_api("401")
    screen.type("e2e-auth")
    screen.send(ENTER)
    screen.settle(cap=3)
    screen.send(DETACH)
    screen.wait("Enter вход", 10, idle=5)
    waiting = list_poll(screen, lambda text: "ждёт входа" in text and "нужен вход" in text and "перезапуск" not in text, budget=60)
    step("после 401 сессия ждёт входа с причиной «нужен вход»", waiting, "нет состояния «нужен вход»", screen)

    set_api("")
    if not step("агент после 401 принимает вход", enter_agent(screen), "агент не принял вход", screen):
        return False
    done = run_turn(screen, "e2e-auth-again", "e2e-auth-again-done")
    step("агент отработал ход после снятия отказа", done, "ответ e2e-auth-again-done не появился", screen)
    step("агент после 401 не перезапускался: прежний разговор на экране", ("e2e-up-done" in screen.text() or "e2e-marker-done" in screen.text()), "прежней реплики на экране нет", screen)
    screen.send(DETACH)
    screen.wait("Enter вход", 10, idle=5)
    ok = list_poll(screen, lambda text: "работает" in text and "нужен вход" not in text and "перезапуск" not in text, budget=60)
    step("после 2xx сессия работает без причины", ok, "состояние не стало «работает»", screen)
    return True


def select_row(screen, label):
    """Ставит выделение списка (reverse) на строку с `label`, не подтверждая выбор."""
    for _ in range(12):
        for row, line in enumerate(screen.rows()):
            column = line.find(label)
            if column >= 0:
                if screen.screen.buffer[row][column].reverse:
                    return True
                screen.send(DOWN)
                screen.settle()
                break
        else:
            return False
    return False


def poll_host(command, expected, budget):
    end = time.time() + budget
    while time.time() < end:
        if host(command) == expected:
            return True
        time.sleep(1)
    return False


def revoke(screen):
    # Отзыв runner-b из раздела «Узлы»: канал закрыт, войти он больше не может, сессия на нём ждёт назначения.
    if not in_list(screen.text()):
        screen.send(DETACH)
        screen.wait("Enter вход", 10, idle=5)
    screen.send(b"N")
    step("раздел «Узлы» открыт", screen.wait("Узлы", 10, idle=5), "раздел не нарисован", screen)
    step("в «Узлах» выбран runner-b", select_row(screen, "runner-b"), "строка runner-b не найдена", screen)
    screen.send(b"v")
    screen.wait("Отозвать runner", 10, idle=5)
    pick(screen, "Да, отозвать runner-b")
    step("TUI сообщает об отзыве", screen.wait("runner отозван", 10, idle=5), "нет «runner отозван»", screen)
    marked = screen.poll(lambda: screen.send(b"r"), lambda text: "отозван" in line_with_text(text, "runner-b"), budget=30, interval=1.0)
    step("«Узлы» помечают runner-b отозванным", marked, "в строке runner-b нет «отозван»", screen)
    screen.send(ESC)
    screen.wait("Enter вход", 10, idle=5)

    waiting = list_poll(screen, lambda text: "ждёт входа" in text and "нужно назначить runner" in text, budget=60)
    step("сессия на отозванном runner ждёт назначения runner", waiting, "нет «нужно назначить runner»", screen)
    screen.send(ENTER)
    screen.settle()
    step("вход в сессию без runner отклонён с подсказкой", "агент не запущен" in screen.text(), "нет «агент не запущен»", screen)

    # Отозванный runner получает отказ и завершается; повторный запуск с теми же личностью и ключом отклоняется так же.
    exited = poll_host("state runner-b", "exited", 60)
    step("отозванный runner завершился", exited, f"состояние контейнера runner-b: {host('state runner-b')!r}")
    host("start runner-b")
    time.sleep(3)
    again = poll_host("state runner-b", "exited", 60)
    step("отозванный runner не может войти снова", again, f"состояние контейнера runner-b: {host('state runner-b')!r}")
    screen.send(b"N")
    screen.wait("Узлы", 10, idle=5)
    line = line_with(screen, "runner-b")
    step("после повторного запуска runner-b остаётся отозванным и не в сети", "отозван" in line and "не в сети" in line, f"строка узла: {line!r}", screen)
    screen.send(ESC)
    screen.wait("Enter вход", 10, idle=5)
    return True


def end(screen):
    if not in_list(screen.text()):
        screen.send(DETACH)
        screen.wait("Enter вход", 10, idle=5)
    screen.send(b"e")
    screen.wait("Да, завершить сессию", 10, idle=5)
    pick(screen, "Да, завершить сессию")
    ended = screen.wait("сессия завершена", 60, idle=10)
    step("сессия завершена из TUI", ended, "нет «сессия завершена»", screen)
    screen.send(b"q")
    screen.pump(0.3)
    return True


STAGES = {
    "connect": connect,
    "files": files,
    "reattach": reattach,
    "exit": exit_agent,
    "runners": runners,
    "clone": clone,
    "cloneuse": cloneuse,
    "plainclone": plainclone,
    "home": home,
    "switch": switch,
    "marker": marker,
    "lost": lost,
    "login": login,
    "revoke": revoke,
    "end": end,
}
NEEDS = {
    "reattach": ["files"],
    "exit": ["files"],
    "runners": ["files"],
    "clone": ["files"],
    "home": ["files"],
    "cloneuse": ["clone"],
    "plainclone": ["clone"],
    "marker": ["switch", "files"],
    "lost": ["marker"],
    "login": ["marker"],
    "revoke": ["marker"],
}


def chosen_stages():
    requested = os.environ.get("E2E_STEP", "").replace(",", " ").split()
    unknown = [name for name in requested if name not in STAGES]
    if unknown:
        print(f"неизвестные этапы {unknown}, есть: {list(STAGES)}", flush=True)
        sys.exit(2)
    if not requested:
        return list(STAGES)
    wanted = {"connect"}
    pending = list(requested)
    while pending:
        name = pending.pop()
        if name not in wanted:
            wanted.add(name)
            pending.extend(NEEDS.get(name, []))
    return [name for name in STAGES if name in wanted]


def main():
    stages = chosen_stages()
    orbit_ca = (SHARED / "ssh-ca.pub").read_text().strip()
    orbit_key = (SHARED / "orbit_key").read_text().strip()
    CONF.write_text(f"orbit_url = orbit-server:2200\norbit_ca = {orbit_ca}\norbit_key = {orbit_key}\nname = e2e\n")
    CONF.chmod(0o600)
    env = {**os.environ, "TERM": "xterm-256color"}
    child = pexpect.spawn("waldo", ["worker", "--settings", str(CONF)], env=env, dimensions=(ROWS, COLS), encoding=None)
    screen = Screen(child)
    for name in stages:
        if not STAGES[name](screen):
            return


try:
    main()
except Exception as error:  # noqa: BLE001
    print(f"FAIL сценарий упал: {error!r}", flush=True)
    failures.append("сценарий упал")
    raise
finally:
    print(f"неудачных шагов: {len(failures)}, всего {time.time() - started:.1f} с", flush=True)
sys.exit(len(failures))
