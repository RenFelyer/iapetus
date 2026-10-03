# Окружение агента как на runner

Цель (решение пользователя, принцип — в контракте, «Агент в коробке»). Вопрос: можно ли дать агенту в контейнере orbit session ощущение, что он работает на waldo runner — сеть, файлы, процессы, — а в контейнере оставить только авторизацию и конфигурацию самого агента.

Часть этого уже сделана, часть упирается в процесс агента, часть противоречит границе безопасности. Устройство слоёв — в `CONTRACT.md` («Перехват в session», «Агент в коробке»).

## Что агент получает с runner

| Что видит агент | Как достигается |
|---|---|
| Файлы проекта | FUSE: рабочий каталог — файловая система session, содержимое с runner |
| Команды | подменённая оболочка: `bash` исполняет на runner и ведёт cwd |
| Поиск | `find` и `grep` через Bash, то есть на runner |
| Инструкции и MCP проекта | файлами через FUSE; проектные MCP запускает runner |
| ОС и переменные runner | `AssignedRunner` (имя, ОС, имена переменных) попадает в правила агента |

Команда агента (`git`, `cargo`, `curl`) исполняется на runner с его сетью и инструментами.

## Что остаётся в контейнере session

- Сеть процесса агента: `WebFetch`, `WebSearch`, HTTP-MCP, проверка обновлений и телеметрия выходят из контейнера session, а не с runner (решено). Хосты, видимые только из сети runner, этим вызовам недоступны.
- Чтение и запись встроенными инструментами вне проекта (`/etc`, `~`, `/usr/include`) попадают в контейнер. Так задумано: runner отдаёт только рабочий каталог (ошибка `OutsideWorkdir`). Показать агенту `~/.ssh` и `/etc` runner значило бы раскрыть ему секреты.
- Свойства процесса: `process.platform`, `uname`, hostname, uid и домашний каталог — контейнерные. Через оболочку те же команды отдают данные runner.
- Всё, что агент делает в своём процессе мимо оболочки: встроенные `rg`, `find`, `grep` Claude Code, `run_workflow` у Antigravity (перехват не реализован).

## Решено

Собственные сетевые вызовы агента (`WebFetch`, `WebSearch`, авторизация, проверка обновлений, телеметрия, плагины, HTTP-MCP) выходят из контейнера session и не идут через runner: учётные данные агента остаются в контейнере. Любая подмена окружения не должна перехватывать эти вызовы; команды агента идут на runner, как и прежде.

## Что можно добавить дёшево

- Абсолютный путь проекта в session может совпадать с путём на runner: логи и сообщения об ошибках агента не расходятся с командами.

## Что не рекомендуется

- Перехват на уровне системных вызовов (seccomp-unotify, ptrace, `LD_PRELOAD`, подход gVisor): `claude` — упакованный бинарник, `agy` — статический Go, `LD_PRELOAD` на нём бесполезен; цена — медленно и хрупко.
- Запуск самого агента на runner: учётные данные агента оказались бы на машине пользователя, а смысл «агента в коробке» в том, чтобы их там не было.

## Заметки по соседней теме

Landlock (запись только в точку монтирования, домашний каталог агента и `/tmp`, без привилегий перед `exec` агента) и запрет `io_uring_setup` через seccomp не дают агенту писать мимо FUSE; на «ощущение» runner они не влияют. FUSE passthrough (ядро 6.9+, `fuser` 0.18: `open_backing`, `max_stack_depth ≥ 1`) ускоряет только чтение уже скачанных и сверенных по хешу файлов: после `open` процесс session операций по дескриптору не видит, запись остаётся через него. Это выводы чужого разбора, на работающей системе не проверены.

## Исследование: `$HOME` runner через FUSE

Проверено 2026-10-03 на Claude Code 2.1.288, Codex 0.159.3, agy 1.2.14.

**Решение пользователя** (после исследования): FUSE `$HOME` по умолчанию, чтение и запись сразу, запретов нет, кроме `~/.claude`; предки проекта читаются. **Рекомендация исследования:** второй FUSE на `$HOME` runner тем же протоколом с выбором корня (`Root::{Project, Home}`): на runner второй `Workdir` с политикой запретных путей, в session второй `Vfs` с общими эпохой и связью. Проект внутри `$HOME` монтируется поверх каталога внутри FUSE `$HOME` (путь проекта и его предки из запретов исключены); `$HOME` внутри проекта или равный ему — без второго FUSE. Первый этап — только чтение (`MountOption::RO`, отказ на runner), запись — вторым. Отвергнуто: FUSE на выбранные каталоги (набор путей заранее неизвестен), overlay (запись в верхний слой терялась бы), bind записей изоляции поверх FUSE (переустановка при каждой смене runner).

**Находки:**
- Символические ссылки в FUSE проекта не работают: `readlink` — `ENOTSUP`, `cat`, `ls`, `stat -L` через ссылку падают (`node_modules/.bin`). Нужна операция `ReadLink` в протоколе; ядро само идёт по ссылке, запрет по пути срабатывает и через ссылки.
- Тихая потеря записи сейчас: встроенный `Write` в `~/new.txt` успешно пишет в пустой контейнерный `$HOME`, агент считает файл записанным на runner. На RO-монтировании Claude Code показывает понятное `EROFS: read-only file system`.
- Claude Code обходит предки `cwd` (`CLAUDE.md`, `CLAUDE.local.md`, `AGENTS.md`, `.claude/CLAUDE.md`, `.claude/rules`, `.mcp.json`, `.git`): с FUSE `$HOME` он прочтёт `~/CLAUDE.md`, `~/projects/CLAUDE.md`, `~/.claude/*` runner — сейчас предки пустые.
- В `$HOME` при `CLAUDE_CONFIG_DIR` вне него Claude Code пишет только `~/.local/share/applications/claude-code-url-handler.desktop` (обработчик `claude-cli://`, при первом запросе в TUI); `XDG_DATA_HOME` в каталог состояния уводит его из `$HOME`. Читает: пробы, `~/.config/anthropic`, `~/.claude/{state,ide,rules,CLAUDE.md}`, `~/.gitconfig`, `~/.config/git/*`, `~/.local/bin/claude`; секретов сам не открывает; `EACCES` на запретных каталогах переносит без сбоев.
- Codex в `$HOME` — только `stat` (`~/.agents/skills`, `.git`) и `inotify_add_watch` на `$HOME`; agy пишет только в `~/.gemini` (sqlite WAL/SHM): `.gemini` остаётся в контейнере. `grep_search`, `find_by_name`, `list_dir` agy читают дерево в процессе — единственный случай тяжёлого обхода `$HOME` через FUSE; `XDG_CACHE_HOME` уводит распакованный `rg` из `$HOME`.
- Claude Code с `$HOME` и проектом на FUSE (bindfs как `$HOME`, FUSE проекта поверх) проходит полный сценарий, в том числе tmp+`rename` и RO.

**Запретные пути:** разрешено по умолчанию, исключения списком на runner (флаги `--home-deny <glob>`, `--home off`); ответ — `EACCES`, в `ReadDir` запретные записи опускаются; проверка по тексту пути и повторно по каноническому пути открытого дескриптора. Перечень: ключи и учётные файлы (`.ssh`, `.gnupg`, `.pki`, `.password-store`, `.local/share/keyrings`, `.netrc`, `.authinfo*`, `.git-credentials`, `.config/git/credentials`, `.config/gh`, `.config/glab-cli`, `.npmrc`, `.yarnrc.yml`, `.pypirc`, `.cargo/credentials*`, `.gem/credentials`, `.m2/settings.xml`, `.gradle/gradle.properties`, `.docker/config.json`, `.kube`, `.config/helm`, `.terraform.d/credentials.tfrc.json`, `.vault-token`, `.config/op`), облака (`.aws`, `.azure`, `.config/gcloud`, `.config/doctl`, `.config/rclone`, `.s3cfg`, `.boto`), агенты (`.claude`, `.claude.json`, `.codex`, `.gemini`, `.config/anthropic`, `.config/openai`, `.config/github-copilot`, `.cursor`, `.config/opencode`, `.local/share/opencode`), профили приложений с сессиями (браузеры, менеджеры паролей, мессенджеры), история оболочек и REPL, Iapetus (`$XDG_STATE_HOME/iapetus`, `$XDG_CONFIG_HOME/iapetus`, точный `--identity-dir` runner и родителя клона). Для записи — дополнительно `.bashrc`, `.profile`, `.zshrc`, `.gitconfig`, `.config/autostart`, `.config/systemd`, `.local/bin`, `.local/share/applications` и подобные. Запрет — не граница безопасности (через Bash агент читает всё, что может пользователь runner), а ограждение встроенных инструментов от случайного чтения.

**Цена** (одна машина, SSH через jump, без сетевой задержки): `lookup` в нечитанном каталоге 0,85–1,1 мс, из кэша эпохи 0,33 мс; `readdir` 202 / 2002 записей 7,8 / 23 мс; чтение 4 КиБ 3,0 мс (повторно 0,58), 1 МиБ 97 мс, 128 МиБ 3,2 с (≈40 МБ/с). Старт `claude -p` в FUSE на ≈60 мс дольше (554 → 616 мс), из них на `$HOME` ≈40 мс. Claude Code трогает в `$HOME` ≈40 путей при старте и ≈13 за ход; при сетевой задержке d это до `N·d` (расчёт: d = 30 мс — 0,3–1,2 с на старт, 0,1–0,4 с на ход); чтение каталога целиком при первом `lookup` сводит старт к 5–6 `ReadDir`. Watch — свой `Watcher` для `$HOME`, фильтр запретных путей до отправки.

**Не снято:** настоящая сетевая задержка (нет `tc` в образе runner); FUSE Iapetus как `$HOME` вместе с FUSE проекта поверх; Codex и agy поверх FUSE `$HOME`; agy на аккаунте Google.

## Связанный вопрос: MCP

Проектные MCP-серверы runner запускает (stdio) или подключает (`http` streamable и `sse`, `${VAR}` в `url` и `headers`, секреты маскируются) у себя. Для полной поддержки:

| Возможность | Что нужно | Сложность |
|---|---|---|
| Изменение набора инструментов на лету (`list_changed`) | Runner заново шлёт сводку, session оповещает агента; поведение каждого агента при смене списка не проверено | средняя |
| Запросы сервера к клиенту (`roots`, `sampling`, `elicitation`) | Обратное направление в канале данных; `roots` runner отвечает сам, остальное — до агента | высокая |
| Подписки на ресурсы, прогресс, отмена вызова | Отдельные сообщения в протоколе | средняя |
| OAuth для удалённых серверов | Браузер и обратный адрес, которых нет на runner и в контейнере | высокая, лучше токены в окружении |

Пробы Claude Code 2.1.288 на заглушке HTTP MCP (2026-10-03, `claude -p`, без сети): `initialize` 2025-11-25 после `server/discover`, `capabilities {roots:{listChanged:true}, elicitation:{form,url}}`, `sampling` не объявлен; после `initialized` — GET-поток `text/event-stream`. `notifications/tools/list_changed` по GET-потоку: добавленный инструмент появляется со следующего запроса, удалённый остаётся в списке (замена `t1` на `t3` даёт оба); в `-p` обновляются только инструменты. `roots/list` — каталог запуска; `sampling/createMessage` — `-32601`; `elicitation/create` в `-p` — отмена, в TUI — диалог. Каждый `tools/call` несёт `_meta.progressToken` и `claudecode/toolUseId`; прерывание шлёт `notifications/cancelled` (`AbortError: remote-cancel`); `resources/subscribe` не приходит. Session MCP сейчас принимает только POST; вызов на runner не отменяется до `CALL_TIMEOUT` (120 с).

## Исследование: пробы в контейнере

Claude Code 2.1.287, Codex 0.159.3, agy 1.2.14. Пометки: (з) — снято запуском в контейнере с профилем прав session без сети (своя ловушка DNS и HTTPS, поддельный ключ, `strace -f`), (с) — разбор бинарника, (?) — не снято.

### Контекст проекта и конфигурация

- Claude Code ищет контекст от `cwd` вверх до `/` (з): в каждом каталоге `CLAUDE.md`, `CLAUDE.local.md`, `AGENTS.md`, `.claude/CLAUDE.md`, `.mcp.json`, `.claude/{commands,workflows,agents}`; в `cwd` и предках — `.claude/{settings*.json,skills,rules,agents,commands,output-styles}`.
- Конфигурация — `$HOME/.claude/*` и `$HOME/.claude.json`; с `CLAUDE_CONFIG_DIR` всё (вместе с `.claude.json`) переезжает в него, `~/.claude` не читается. Дальше `/etc/claude-code/{managed-settings.json,managed-settings.d,managed-mcp.json,CLAUDE.md,.claude/*}`, `~/.config/anthropic`, `~/.gitconfig`.
- `--add-dir D` читает `D/.claude/*`; `D/CLAUDE.md` — только при `CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD=1`. `--plugin-dir` даёт навыки вида `плагин:навык`.
- `@`-импорты в `CLAUDE.md` раскрываются (`@local.md`, `@../outside.md`, `@~/home.md`, `@/abs/path.md`) только при `projects["<абсолютный путь проекта>"].hasClaudeMdExternalIncludesApproved: true` в `.claude.json`; без записи внешние импорты (`../`, `~/`, абсолютный путь) не читаются, а импорт файла внутри проекта читается и без неё. `hasTrustDialogAccepted` значения не имеет, ключ `"/"` не заменяет ключ точного пути проекта (снято на стенде). Контейнер без записи теряет внешние импорты молча. Зависимость от удалённых флагов не исключена (?).
- Сессии и автопамять — `CLAUDE_CONFIG_DIR/projects/<слаг cwd>`: привязаны к пути `cwd` и лежат в контейнере.
- Codex (`AGENTS.md`, `~/.codex`) и agy (`GEMINI.md`, `.agents/`): только строки бинарников (с); обход предков и импорты не снимались (?).

**Принцип разделения.** Что harness вычисляет от `cwd` по имени — контекст проекта, берётся с FUSE; что от `$HOME`, `$CLAUDE_CONFIG_DIR`, `/etc/<harness>` — конфигурация и учётные данные, остаётся в контейнере. Граница определяется тем, от чего строится путь, а не списком каталогов: новый `<cwd>/.claude/prompts/` окажется на FUSE, новый `~/.claude/identities/` — в контейнере. Не покрыт новый абсолютный путь вида `/etc/claude-code/prompts/`: он в контейнере. Требуется: `cwd` внутри FUSE; `HOME` и `CLAUDE_CONFIG_DIR` вне дерева `cwd` и не предки `cwd`; предки точки монтирования пустые и синтетические; session пишет в `.claude.json` одобрение импортов для пути проекта при запуске; абсолютные пути вне проекта читаются из контейнера (согласуется с `OutsideWorkdir`).

Варианты: A — точка монтирования FUSE, пустые предки, `HOME` и `CLAUDE_CONFIG_DIR` вне дерева, запись одобрений; B — overlay или bind конфигурации с FUSE (работает, но конфигурация и проект перемешиваются); C — путь монтирования равен пути runner (дёшево, дополнение к A); D — прокладка FUSE отдаёт и конфигурацию (нарушает «учётные данные не на runner»). Рекомендация: A вместе с C.

### Сетевые вызовы Claude Code (з)

- `ANTHROPIC_BASE_URL` перенаправляет только `HEAD /api/hello` и `POST /v1/messages`. Остальное идёт на жёстко заданный `api.anthropic.com` с ключом: `/api/claude_code/{settings,policy_limits,organizations/metrics_enabled}`, `/api/eval/sdk-…`, `/api/claude_cli/bootstrap`, `/mcp-registry/v0/servers`, `/api/event_logging/v2/batch`.
- `WebSearch` — серверный инструмент внутри `/v1/messages`, своих хостов нет. `WebFetch` — три шага: `GET api.anthropic.com/api/web/domain_info?domain=…` без авторизации (без `can_fetch:true` страница не запрашивается), прямое DNS/TLS/GET к целевому хосту из процесса, `POST /v1/messages` с haiku на шлюз.
- HTTP и SSE MCP подключаются при старте напрямую к URL из конфигурации; проектный HTTP-MCP из `.mcp.json` в `-p` без DNS получает `failed`.
- `claude update` — `downloads.claude.ai/claude-code-releases/latest`; `claude auth login` печатает `claude.com/cai/oauth/authorize?…`, обмен кода — на `platform.claude.com`; `claude plugin marketplace add` запускает дочерний `git clone` в обход оболочки агента (в образе нет `ssh`).
- `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` оставляет только `/api/hello`, `/v1/messages` и WebFetch; флаги, bootstrap, mcp-registry и телеметрия исчезают.
- `HTTPS_PROXY` (CONNECT) соблюдают все вызовы всех трёх агентов; `ALL_PROXY=socks5h` Claude Code игнорирует, и WebFetch ломается.
- При старте Claude Code запускает `git … ls-files` и `sh -c "uname -s; uname -r; uname -m; command -v npm yarn pnpm bun deno node"`: отпечаток окружения уходит в телеметрию.
- Codex при старте — `chatgpt.com/backend-api/plugins/*`, `github.com`, `api.github.com/repos/openai/plugins`, `api.openai.com/v1/responses`; agy — `antigravity-unleash.goog`, `play.googleapis.com/log`, вход `accounts.google.com`.

Возможности контейнера с профилем session (з): `iptables` и `nft` недоступны; `/dev/net/tun` нет, `pasta` и `slirp4netns` без нового устройства не запустятся; `unshare -Urn` и `unshare -Um` работают от uid 1000 и после снятия `SYS_ADMIN` из bounding set: агент сам создаёт userns и netns, поэтому netns как ограничитель сети агента непригоден.

**Как не дать подмене окружения перехватить вызовы агента:** не класть в окружение агента `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`, `NO_PROXY`, `SSL_CERT_FILE`, `NODE_EXTRA_CA_CERTS`; сеть процесса агента ничем не подменять; поставить `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`; набор хостов, которые видит агент, контролировать стендом `probe.sh` + `mitm.py` (падение на новом хосте) и в эксплуатации — журналом DNS или egress-политикой сети контейнера на хосте Docker. Хосты, видимые только из сети runner, `WebFetch` недоступны: для них остаётся `curl` через оболочку.

### Свойства системы

| Что видит агент | Откуда | Цена подмены |
|---|---|---|
| Файлы, команды, инструкции, `.mcp.json` проекта | runner | сделано |
| Конфигурация и учётные данные harness; сеть harness | контейнер | граница |
| Путь `cwd` | точка монтирования | дёшево: равен пути runner, предки пустые |
| hostname, `/etc/os-release` | контейнер | дёшево: `sethostname` до понижения прав, запись из `AssignedRunner` (с, не снималось) |
| `$HOME` | контейнер | дёшево |
| `uname -r`, `process.platform` | ядро и Bun контейнера | только перехват системных вызовов |
| `/proc`, `/etc` встроенными Read и Write | контейнер | дорого и нельзя: раскрыл бы секреты runner |
| Встроенные `rg`, `find`, `grep`, `run_workflow` | контейнер, пути на FUSE | видят проект, но не окружение runner |

Через оболочку агент видит данные runner, встроенными инструментами — контейнер. Вывод: архитектуру не менять; дёшево приблизить «ощущение runner» (путь монтирования, hostname и `os-release`, `HOME` и `CLAUDE_CONFIG_DIR` вне дерева, одобрения импортов, сеть не трогать); дальше цена растёт нелинейно ради `uname -r`, `process.platform`, `/proc`.

### Не снято

Claude Code: обмен и обновление OAuth-токена, `setup-token` (нужен pty и аккаунт), коннекторы claude.ai и `mcp-proxy.anthropic.com`, Datadog и GrowthBook, `NO_PROXY` и `DISABLE_TELEMETRY` по отдельности, настоящий ответ `domain_info`. Codex: вход через `auth.openai.com`, обновление токена, настоящий `responses`, MCP. agy: вход, запросы к модели, `run_workflow`, MCP. Оболочка Iapetus в пробах не участвовала. Нужны тестовые аккаунты и контейнер с разрешающим egress и логом DNS.
