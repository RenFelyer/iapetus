# Iapetus

**Run Claude Code in a Docker sandbox while it works on your real machine.** The coding agent lives in a container on a
separate server; its environment is a developer machine (the *runner*): project files are served over FUSE, shell
commands and project MCP servers run on the runner over SSH. The agent sees the runner's project path, hostname,
`/etc/os-release` and `$HOME`, while its subscription, configuration and conversations stay in the container. One
agent can switch between machines and keep the conversation; several sessions are managed from one terminal UI.

The agent binary is unmodified: files are intercepted by FUSE, commands by a substitute shell, and API traffic passes
through a gateway that only observes and forwards it. Written in Rust: SSH with its own certificate authority, a
multiplexed protocol, a Docker API proxy that only creates containers from a fixed template.

**Status: experimental, not maintained.** Tested end to end in Docker against a fake Anthropic API and once with a
real Claude Code login. Interception relies on Claude Code internals and may break with new releases. Codex CLI and
Antigravity CLI profiles exist but are not tested on real accounts. You log in to the agent yourself; Iapetus never
stores or forwards credentials, and compliance with the providers' terms is your responsibility. Documentation below
and in [`docs/`](docs/) is in Russian.

Агент для программирования (Claude Code; задел — Codex CLI и Antigravity CLI) работает в контейнере Docker на
отдельном сервере, а его окружение — машина разработчика: файлы проекта, команды и MCP-серверы проекта исполняются
там. Агент видит путь проекта, hostname, `/etc/os-release` и `$HOME` этой машины, а его подписка, конфигурация и
разговоры остаются в контейнере. Одним агентом можно переключаться между машинами с продолжением разговора и
управлять несколькими сессиями из одного TUI.

Бинарник агента не меняется: файлы перехватывает FUSE, команды — подменённая оболочка, а запросы к API проходят через
шлюз, который только наблюдает и пересылает без изменений.

## Статус

Экспериментальный проект, не сопровождается. Работает на стенде, в повседневной работе не использовался.

- Сценарий `docker/e2e.sh` проходит на настоящих `orbit server`, `orbit session`, `waldo` и Claude Code 2.1.285 с
  поддельным Messages API: вход из TUI в агента, чтение, правка и команда на runner, повторный вход, смена runner с
  перемонтированием и продолжением разговора, клоны runner (worktree репозитория и обычный каталог), проект внутри
  `$HOME` runner (чтение и запись вне проекта, закрытый `~/.claude`, `CLAUDE.md` предка), потеря и возвращение
  runner, отзыв runner, ответ 401 и повторный вход, завершение сессии. MCP-серверы проекта и обновление агента проверены
  только тестами.
- С настоящим входом в Claude Code снят один короткий прогон: вход, ответы API, окна лимитов подписки, MCP, подагент,
  окружение runner. Ответ 429 при исчерпанном лимите, 401 и телеметрия на настоящем аккаунте не проверены.
- Перехват опирается на внутреннее поведение Claude Code (вид вызова оболочки, запись файлов через временный файл и
  `rename`, формат каталога разговоров, заголовки ответов API). Новая версия Claude Code может его сломать.
- Codex CLI и Antigravity CLI подключены профилями, но проверены только на поддельных API или статически.

## Условия использования

В агента пользователь входит сам, внутри контейнера session; Iapetus учётные данные не хранит и не передаёт. Условия
Claude Code разрешают входить своей подпиской в немодифицированный Claude Code, но частную установку вроде этой прямо
не разбирают; провайдер видит в телеметрии нестандартный адрес API и запуск в Docker. Условия Antigravity называют
доступ к сервису через стороннее ПО нарушением. Все риски условий провайдеров (Anthropic, Google, OpenAI) — на
пользователе. Чужие подписки использовать нельзя.

## Устройство

Две программы на Rust и общая библиотека:

| Роль | Где | Что делает |
|---|---|---|
| `orbit server` | один на систему, свой контейнер | Центр сертификатов SSH, реестр, регистрация runner, окружение и документы, MCP-сервер для агентов; поднимает контейнеры session через свой прокси Docker и обновляет агентов |
| `orbit session` | контейнер на хосте server | Одна сессия агента в pty: FUSE, подменённая оболочка, шлюз API; принимает вход worker в терминал агента |
| `waldo worker` | машина пользователя | TUI управления: сессии, узлы (отзыв, новый `orbit_key`), окружение; вход в терминал агента |
| `waldo runner` | каждая машина, где идёт работа | Исполняет файловые операции и команды сессии в каталоге проекта |

```
waldo worker, runner × N ──управление: SSH──► orbit server ◄──управление── orbit session × N
waldo worker ──ssh -J через server──► orbit session (агент, FUSE, оболочка, шлюз)
waldo runner ──данные: SSH через jump server──► orbit session
```

- Внешний порт server слушает только адрес сети узлов (Tailscale, ZeroTier); сети сессий он не виден.
- Данные идут между session и runner по SSH через jump на server; server видит шифротекст.
- Контейнеры session создаются только по шаблону: прокси Docker сверяет тело запроса (образ по digest, набор
  capabilities, единственное устройство `/dev/fuse`, без bind и host-режимов, лимиты) и отвергает лишнее.
- Агент работает с правами пользователя runner: доступ к машине runner у него тот же, что у этого пользователя.
  Iapetus изолирует подписку и конфигурацию агента, а не машину runner от агента.

## Состав

- `nerve/` — общая библиотека: протокол, мультиплексор, правила сертификатов SSH, вход к server и session.
- `orbit/` — программа с режимами `server` и `session`.
  - `orbit server` — один процесс SSH с двумя слушателями: внешним для узлов (регистрация, продление сертификатов,
    канал управления, jump) и внутренним для session; он же MCP-сервер и прокси Docker. Аргументы (флаг —
    переменная окружения): `--data-dir` (`IAPETUS_DATA_DIR`) — каталог состояния; `--listen` (`IAPETUS_LISTEN`,
    обязателен, не `0.0.0.0` и не `::`) — внешний слушатель на адресе сети узлов; `--internal-listen`
    (`IAPETUS_INTERNAL_LISTEN`) — внутренний слушатель SSH; `--mcp-listen` (`IAPETUS_MCP_LISTEN`) — слушатель MCP;
    `--host` (`IAPETUS_HOST`, по умолчанию `orbit-server`) — имена server в его хост-сертификате, через запятую;
    `--author-name`, `--author-email` (`IAPETUS_AUTHOR_NAME`, `IAPETUS_AUTHOR_EMAIL`) — автор коммитов по умолчанию;
    `--update-delay` (`IAPETUS_UPDATE_DELAY`, секунды, по умолчанию 259200 — 3 суток; 0 — сразу) — сколько новая
    версия агента должна пробыть в канале выпусков, прежде чем server предложит её session.
    Контейнеры session server поднимает, когда задан `--session-image` (`IAPETUS_SESSION_IMAGE`, образ по digest) с
    `--session-orbit-url` и `--session-mcp-url`; сеть, AppArmor, лимиты, `--session-work-max` и `--docker-socket`
    (`IAPETUS_DOCKER_SOCKET`) — там же (`orbit server --help`). В `--data-dir` server держит реестр, ключи, CA и
    документы; первый `orbit_key` пишет в файл `orbit_key` и удаляет его после первого использования.
  - `orbit session` — контейнер одной сессии агента; его поднимает server. Монтирует FUSE по пути проекта на
    назначенном runner (и вторую FUSE по пути `$HOME` runner, если runner её отдаёт; проект внутри `$HOME` —
    поверх неё), ставит hostname, `os-release` и `$HOME` runner, запускает агента (`--agent`) в pty от
    пользователя `--shell-uid`, пускает worker в его терминал и поднимает шлюз на базовом адресе API агента
    (`--gateway-listen`, `--upstream`, `--gateway-api`: `messages` для Claude Code, `responses` для Codex CLI,
    `gemini` для Antigravity CLI). Все параметры ей передаёт server.
- `waldo/` — программа с режимами `worker` и `runner`.
  - `waldo worker [--settings ПУТЬ]` — TUI со списком сессий, лимитами подписки, узлами (отзыв runner, новый `orbit_key`) и окружением; вход в
    агента — Enter, выход — Ctrl-]. Настройки — только файл (`--settings`, `IAPETUS_WORKER_SETTINGS`, по умолчанию
    `$XDG_CONFIG_HOME/iapetus/worker.conf`, права `0600`): строки `ключ = значение` — `orbit_url` (адрес SSH server,
    `хост:порт`), `orbit_ca` (открытый ключ CA server), `orbit_key` (ключ доступа), `name` (имя входа, по умолчанию
    имя локального пользователя). Журнал — `worker.log` рядом с файлом настроек.
  - `waldo runner [КАТАЛОГ]` — работает в каталоге проекта (по умолчанию — каталог запуска) и выполняет файловые
    операции и команды назначенной сессии. Флаги и переменные: `--orbit-url` (`IAPETUS_ORBIT_URL`) — адрес SSH
    server, `--orbit-ca` (`IAPETUS_ORBIT_CA`) — файл с открытым ключом CA server, `--orbit-key`
    (`IAPETUS_ORBIT_KEY`) — ключ доступа, нужен при первом запуске (дальше берётся из каталога личности), `--name`
    (`IAPETUS_NAME`) — имя узла, по умолчанию имя локального пользователя, `--env-file` (`IAPETUS_ENV_FILE`, по
    умолчанию `.env`) — файл окружения команд, `--home on|off` (`IAPETUS_HOME`, по умолчанию `on`) — отдавать ли
    сессии `$HOME` runner на чтение и запись (кроме `~/.claude`; конфигурация агента остаётся в контейнере session).
    Личность (fingerprint, ключ, сертификат) лежит в `--identity-dir` (`IAPETUS_IDENTITY_DIR`), по умолчанию
    `iapetus/runner/<каталог>-<хеш>` в `$XDG_STATE_HOME`. Runner принимает запросы только от server.
- `docker/` — compose-файлы для разработки, стенд e2e и изолированный прогон тестов; профиль AppArmor.

## Запуск

Образы (контекст сборки — корень репозитория):

```sh
docker build -f orbit/Dockerfile --target server -t iapetus-server .
docker build -f orbit/Dockerfile --target session -t iapetus-session .
docker build -f waldo/Dockerfile -t iapetus-runner .
```

Узлы (адреса — примеры; `100.64.0.1` — адрес server в сети узлов):

```sh
orbit server --data-dir /var/lib/orbit --listen 100.64.0.1:2200 \
  --internal-listen 172.31.0.10:2201 --mcp-listen 172.31.0.10:2202
ssh-keygen -y -f /var/lib/orbit/ssh-ca > ca.pub
IAPETUS_ORBIT_KEY="$(cat /var/lib/orbit/orbit_key)" waldo runner ~/project --orbit-url 100.64.0.1:2200 --orbit-ca ca.pub
waldo worker --settings ~/.config/iapetus/worker.conf
```

Систему стоит запускать в отдельном rootless Docker под непривилегированным пользователем: контейнеру session нужен
`CAP_SYS_ADMIN` для FUSE. Профиль AppArmor для хоста, где `docker-default` запрещает `mount`, —
`docker/apparmor/iapetus-session`.

Среда разработки:

```sh
docker compose -f docker/docker-compose.dev.yml up --build
docker compose -f docker/docker-compose.dev.yml --profile session up --build orbit-session
docker compose -f docker/docker-compose.dev.yml down -v
```

## Проверки

```sh
cargo build
scripts/check.sh              # формат, clippy, cargo deny и тесты в изолированной сети
scripts/check.sh --no-docker  # без контейнерных тестов
docker/e2e.sh                 # сквозной сценарий в Docker-in-Docker; docker/e2e.sh clean — уборка
```

Голый `cargo test` пропускает медленные тесты (их включает `RUN_SLOW_TESTS=1`, как в `scripts/check.sh`). Имя проекта
compose `scripts/check.sh` берёт из каталога рабочей копии, чтобы прогоны из разных копий не мешали друг другу.

Формат при коммите проверяет хук `.githooks/pre-commit`; он подключается в рабочей копии один раз:

```sh
git config core.hooksPath .githooks
```

Кэш сборки Docker растёт со слоями образа `check`; старые слои убирает

```sh
docker buildx prune -f --filter 'description~="cargo test --workspace --locked --no-run"'
```

## Лицензия

[0BSD](LICENSE): код можно использовать, менять и распространять как угодно, без указания автора; автор не даёт
гарантий и не отвечает за последствия использования.
