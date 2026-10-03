# Harness агентов

Факты об агентах в orbit session, по слоям Iapetus. Решения Iapetus — в
`CONTRACT.md` («Агент в коробке», «Перехват в orbit session», «FUSE», «Шлюз»).

Версии: Claude Code 2.1.287 (каталог инструментов, запись файлов, оболочка;
остальное снято на 2.1.285), Codex CLI 0.160.0, Antigravity CLI (`agy`) 1.2.14,
Gemini CLI 0.62.0 (сравнение). Пометки: (з) — снято запуском на поддельном API
без сети, (с) — статический разбор бинарника, (к) — по типам из пакета, (д) — по
документации или исходникам, (?) — не снято. Не проверены нигде: настоящие подписки и токены, настоящий
OpenRouter, приём изменённого запроса настоящим upstream. Основной consumer-ход
agy не снят. Сетевые отличия и телеметрия — `RISKS.md`.

## Агенты

| | Claude Code | Codex CLI | Antigravity CLI |
|---|---|---|---|
| Что | Node-бинарник Anthropic | Rust, Apache-2.0, `openai/codex` | Go, закрытый `agy`, внутри язык-сервер `jetski` (gRPC на localhost), один процесс |
| Протокол | Anthropic Messages SSE | Responses SSE | Gemini `streamGenerateContent` SSE |
| Без интерактива | `-p` | `codex exec` | `-p` |
| Самообновление | `DISABLE_AUTOUPDATER` — фон, `DISABLE_UPDATES` — всё | пакет | фон; в каталоге без записи пропускается (з) |

## Файлы

| | Claude Code | Codex CLI | agy |
|---|---|---|---|
| Запись | временный файл `<имя>.tmp.<pid>.<12 hex>` в каталоге цели (`O_CREAT\|O_EXCL\|O_NOFOLLOW`), один `write`, `fchmod`, `fsync`, `rename`; пути вида `/proc/self/fd/<N>/<имя>`, где N — `O_PATH`-дескриптор каталога (нужен `/proc`) | `apply_patch` в процессе: `O_RDONLY`, затем `O_WRONLY\|O_CREAT\|O_TRUNC` и один `write`; без tmp, `rename`, `fsync`; права сохраняются; `Move to` — запись цели и `unlink` источника | чтение, `O_TRUNC`, один `write`; без tmp и `rename`; права сохраняются (з) |
| «Сначала прочитай» | Sonnet 5.5 — нет; Opus 4.6, Haiku 4.5 и старше, notebook — да | нет | нет |
| Проверка устаревания | Write: «File has been modified since read…» при выросшем mtime и другом содержимом; Edit перечитывает и накладывает правку на текущее содержимое, при смене содержимого — заметка | нет; патч по контексту: «apply_patch verification failed: Failed to find expected lines»; `Add File` поверх существующего перезаписывает | нет |
| Старт сессии | `rg --files --hidden <cwd>`, `inotify` на cwd, проба fswatch в `/tmp/claude-<uid>/` | — | — |

Выводы для Iapetus:
- Два семейства записи: Claude Code — tmp + `rename`; Codex и agy — `O_TRUNC` +
  `write`. FUSE коммитит на `rename` у первого и на `flush` у второго.
- Внешняя запись в окне между чтением агента и его записью теряется молча у всех
  трёх, агент сообщает успех. Обнаруживает внешнюю правку только Claude Code, и
  только случившуюся до вызова инструмента.
- Временные файлы в проекте создаёт только Claude Code: `.tmp.<pid>.<12 hex>` у
  цели; при ошибке удаляется, после SIGKILL остаётся, цель цела. Настройки
  `.claude/settings*.json`, `.mcp.json`, `.claude/scheduled_tasks.json` — через
  `.claude/.cc-writes/<имя>.tmp.<pid>.<12 hex>`, если каталог есть (по бандлу
  2.1.286). SIGKILL посреди записи оставляет у Codex и agy файл в 0 байт.
- Защиты Claude Code построены на mtime от ФС: при TTL атрибутов 1 с Write
  молча стирает внешнее изменение, а Read после внешней смены отдаёт заглушку
  «file unchanged since your last Read»; при TTL 0 обе защиты работают.
- Песочница Codex `workspace-write` (bwrap) монтировала бы в проект пустые
  `.git`, `.agents`, `.codex`, `.aws`; в Docker bwrap не стартует, нужен
  `danger-full-access`.

### Claude Code на FUSE (fuser 0.18)

- Старт: `opendir`, `readdir`, `lookup` на `.git`, `.gitignore`, `.ignore`,
  `.rgignore` в каждом каталоге; отдельно `lookup` `.claude`, `.mcp.json`,
  `CLAUDE.md`, `CLAUDE.local.md`, `AGENTS.md`. Одни и те же отсутствующие пути
  проверяются до 6 раз.
- Read — `lookup`, `open`, `read`, `release`. Edit перед записью 3–5 раз целиком
  перечитывает файл, затем `create` tmp (`O_EXCL`), один `write`, `setattr mode`
  (у существующего), `fsync`, `rename`.
- `inotify` на FUSE видит только операции через точку монтирования.

| Ошибка ФС | Поведение Claude Code |
|---|---|
| `rename`: `ECANCELED`, `EAGAIN`, `EIO`, `ESTALE` | модели `<КОД>: <текст libuv>, rename '<tmp>' -> '<цель>'` с `is_error`; tmp удаляется |
| `rename`: `EPERM`, `EBUSY`, `EEXIST`, `EXDEV`; `create` существующего: `EACCES` | тихая перезапись: `O_WRONLY`, `setattr size=0`, запись на месте, `fsync`, «updated successfully» |
| `EINTR` на `rename`, `create`, `write`, `setattr`, `open` | повтор без конца |
| `fsync`, `setattr mode`: `EPERM`, `EINVAL`, `ENOTSUP` | игнорируется |
| задержки | `fsync` до 125 с и `rename` до 30 с не ломают; задержка `open` умножается на 3–6 открытий |

### Чтение

- Claude Code: Read больше 256 КБ целиком — ошибка (3,7 МБ с `offset`/`limit`
  вернулось 1,9 МБ); строки `N<TAB>текст`; повторное целое чтение без изменений
  — «Wasted call — file unchanged since your last Read…». Чтение одного файла
  через Bash (`cat`, `head`, `tail`, `sed -n`, `grep`) засчитывается как Read.
  `cat /dev/stdin` в TUI зависает.
- Codex: нерегламентированного чтения нет, `cat` в оболочке; `view_image` —
  только картинки.
- agy: `view_file` до 800 строк и 46 080 байт.

## Оболочка

| | Claude Code | Codex CLI | agy |
|---|---|---|---|
| Какая | `CLAUDE_CODE_SHELL` — путь к bash или zsh; скрипт или ссылка принимаются, если имя оканчивается на `bash`; `$SHELL` с нестандартным путём игнорируется | `pw_shell` из `/etc/passwd`, не `$SHELL` | `bash` из `PATH` процесса agy (argv0 `bash`); `$SHELL` игнорируется (з) |
| Вызов | см. ниже | `<shell> -lc <cmd>` при `features.shell_snapshot=false`; по умолчанию shell snapshot: `-lc` только создаёт снимок (`~/.codex/shell_snapshots`), команда идёт как `<shell> -c <обёртка>`, обёртка исполняет `<shell> -c <cmd>` (з); `-c` без `-l` и при `login: false`; запуск через `/proc/self/exe --codex-run-as-process-setup` (з) | `bash -c <команда>` в pty, `TERM=dumb`, `PAGER=cat`; `PATH` начинается с `~/.gemini/antigravity-cli/bin` |
| cwd | сохраняется в проекте, вне проекта сбрасывается | `workdir`, `cd` не переносится | `Cwd` обязателен и должен существовать; `cd` и переменные не переносятся, кроме `RunPersistent` |
| Подмена | `CLAUDE_CODE_SHELL` → ссылка `bash` на оболочку orbit | ссылка в `/etc/passwd` либо отключённый snapshot | ссылка `bash` в каталоге, стоящем в `PATH` процесса agy раньше `/usr/bin` (шим получил все 4 команды) (з) |
| Фон | `run_in_background`, итог — `<task-notification>` | `exec_command` отдаёт `session ID`, `write_stdin` | дольше `WaitMsBeforeAsync` — `task-N`, итог — `<SYSTEM_MESSAGE>` |

Claude Code:
- `CLAUDE_CODE_SHELL` получает три вызова: `-c env`; `-c -l <скрипт>`, который
  coreutils (`id`, `run-parts`, `head`, `cut`, `grep`, `awk`, `sed`, `cat`)
  собирает снимок окружения в `~/.claude/shell-snapshots/snapshot-bash-*.sh`;
  команда.
- Вызов команды: `-c "source <снимок> 2>/dev/null || true && shopt -u extglob
  2>/dev/null || true && { \builtin unalias -- 'unsetenv'; \builtin unset -f --
  'unsetenv'; } >/dev/null 2>&1 || true && eval <слово> < /dev/null && pwd -P
  >| /tmp/claude-XXXX-cwd"`. Простая команда — без кавычек (`eval pwd`), прочие
  в одинарных (`'"'"'` для кавычки внутри).
- cwd узнаётся по файлу после `pwd -P`; он пишется только при коде 0. `cd` в
  проекте сообщается system-сообщением `# Environment update - Primary working
  directory: X (was Y)`; вне проекта — сброс с припиской «Shell cwd was reset
  to …».
- stdout и stderr — в один файл задачи
  `/tmp/claude-<uid>/<проект>/<сессия>/tasks/<id>.output`; stdin — `/dev/null`.
- `CLAUDE_CODE_SHELL_PREFIX` получает только вызов команды, всей строкой в `$1`;
  оборачивает также хуки, статусную строку и запуск stdio-MCP.
- Снимок подменяет `find` и `grep` функциями на встроенные в бинарник `bfs` и
  `ugrep` (`CLAUDE_CODE_EXECPATH`), `rg` — на встроенный `rg`, если системного нет;
  без бинарника — системные. Обёртка `pkill` отказывает при совпадении с процессом
  CLI.
- Monitor (`command`) идёт через тот же `CLAUDE_CODE_SHELL` с той же обёрткой,
  каждая строка вывода — уведомление; `ws` — WebSocket-клиент в агенте, мимо оболочки.
- Дети видят `CLAUDECODE=1`, `CLAUDE_CODE_ENTRYPOINT`, `CLAUDE_CODE_EXECPATH`,
  `CLAUDE_CODE_SESSION_ID`, `CLAUDE_CODE_MESSAGING_SOCKET`,
  `CLAUDE_CODE_MESSAGING_TOKEN`, `ANTHROPIC_BASE_URL`; на runner это окружение
  переносить нельзя.
- Результат: код не 0 — `Exit code N\n<вывод>` с `is_error` (код 1 у `grep` без
  совпадений — успех); таймаут — `Exit code 143\nCommand timed out after Ns`;
  вывод больше ~30 000 символов — `<persisted-output>` с путём в
  `~/.claude/projects/<проект>/<сессия>/tool-results/` и первыми 2 КБ.

agy: окружение agy наследуется целиком плюс `ANTIGRAVITY_*` (в том числе
`ANTIGRAVITY_SOURCE_METADATA` с вызовом). Codex: дети видят `CODEX_CI=1`,
`CODEX_HOME`, `CODEX_SESSION_ID`, `CODEX_THREAD_ID`, `CODEX_VERSION` (з); exec-server
(`codex exec-server`, клиент `CODEX_EXEC_SERVER_URL=ws://…`, экспериментально)
выносит `process/*` и `fs/*` по JSON-RPC; клиент без проекта не делает ни
файловых операций, ни `execve`, `AGENTS.md` читается с удалённой стороны, на
клиенте нужен пустой каталог с тем же путём и `-C`.

## API и шлюз

### Claude Code

- Адреса: `HEAD /api/hello` на старте; `POST /v1/messages?beta=true` (SSE,
  каждый шаг); `POST /v1/messages/count_tokens?beta=true` (размер результата
  Read и MCP). Мимо `ANTHROPIC_BASE_URL`, на `api.anthropic.com`: проверка
  домена WebFetch, DesignSync, RemoteTrigger, флаги, профиль OAuth, проверка
  связи при первом запуске. `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`
  отключает часть этого трафика.
- С одним `ANTHROPIC_BASE_URL` вход по подписке остаётся действующим. Флаги
  серверов Anthropic, remote managed settings и organization policy не
  загружаются. В 2.1.286 custom base URL меняет форму запроса (нет `cch`,
  prompt id, части диагностики) и даёт в телеметрии
  `policy_ineligible_reason: custom_base_url`.
- Тело запроса:

| Поле | Значение |
|---|---|
| `model` | Sonnet 5.5 при токене подписки, Opus 5.5 при API-ключе |
| `max_tokens` | 128000 Sonnet и Opus, 64000 Fable, 32000 Haiku |
| `metadata.user_id` | JSON `{"device_id","account_uuid","session_id"}` |
| `thinking` | Sonnet, Opus, Fable — `adaptive` (в `-p` с `"display":"omitted"`); Haiku — `enabled`, `budget_tokens` 31999 |
| `context_management` | `clear_thinking_20251015`, `keep: all` |
| `output_config` | `{"effort":"medium"}` |
| `safeguards` | `dangerous_tool_use` с `classifier_context` (`live_cwd`, `git_state`, …) — контейнера, не runner; только в режимах, где возможен auto |

- `system`: `[0]` `x-anthropic-billing-header: cc_version=2.1.285.<3 hex>;
  cc_entrypoint=cli|sdk-cli;` без cache_control; `[1]` «You are Claude Code…»
  (TUI) или «You are a Claude agent, built on Anthropic's Claude Agent SDK.»
  (`-p`, субагент); `[2]` основной промпт ~5,9 КБ с «# Memory»
  (`~/.claude/projects/<проект>/memory/`), «# Environment». `--append-system-prompt`
  дописывается в `system[2]`; `--system-prompt` заменяет только `system[2]`.
- `messages[0]` user: блок 1 — `<system-reminder>` со всеми CLAUDE.md и rules;
  блок 2 — атрибуция коммитов; последний — текст пользователя. `messages[1]`
  system: `# Environment` (cwd, git, платформа, Shell, модель, deferred-инструменты,
  агенты, skills, `<total_tokens>`, дата). Всё не-пользовательское идёт
  system-сообщениями (бета `mid-conversation-system-2026-04-07`: Sonnet, Opus,
  Fable); у Haiku — `<system-reminder>` внутри user.
- Вставки по ходу: `<total_tokens>` в хвост каждого запроса; `# Environment
  update` при `cd`; вложенный CLAUDE.md и rule с `paths:` после Read подходящего
  файла; смена модели, даты, MCP; plan mode (~4,9 КБ); заметка «changed on disk».
  История остаётся неизменной, кроме `replace_all:false` в Edit и схлопывания
  `content` при уходе брейкпоинта.
- Заголовки: `Authorization: Bearer` (API-ключ — `x-api-key`),
  `anthropic-version: 2023-06-01`, `anthropic-beta`, `User-Agent:
  claude-cli/2.1.285 (external, cli|sdk-cli)`, `x-app: cli`,
  `X-Claude-Code-Session-Id`, `x-claude-code-agent-id` (только субагенты),
  `anthropic-dangerous-direct-browser-access: true`, `X-Stainless-*`.
- Беты: всегда — `claude-code-20250219`, `interleaved-thinking-2025-05-14`,
  `thinking-token-count-2026-05-13`, `context-management-2025-06-27`,
  `prompt-caching-scope-2026-01-05`; подписка — `oauth-2025-04-20`,
  `extended-cache-ttl-2025-04-11`; Sonnet, Opus, Fable — `mid-conversation-system`,
  `per-turn-control-2026-07-01`, `effort-2025-11-24`; TUI — `redact-thinking`;
  с `safeguards` — `dangerous-tool-use-2026-09-03`, `afk-mode-2026-01-31`.
- Служебные запросы:

| Запрос | Признаки |
|---|---|
| Проба квоты | только TUI, на старте; `max_tokens:1`, без system |
| Заголовок сессии | без `tools`; `output_config.format` json_schema `{"title"}`; параллельно первому запросу; после `/compact`; в `-p` нет |
| Компакт | те же system и tools; в последний user-блок дописано «CRITICAL: Respond with TEXT ONLY…» |
| Субагент | `x-claude-code-agent-id`, `cc_is_subagent=true`, cache_control без ttl |
| WebSearch | `tools` = `web_search_20250305`; system «You are an assistant for performing a web search tool use» |
| Классификатор auto mode | без `tools`; модель `claude-sonnet-5`; system ~132 КБ «You are a security monitor…»; две стадии (`max_tokens` 64 и 8192); ответ не `<block>` — действие блокируется |

- cache_control: `system[1]`, `system[2]` и последний блок последнего сообщения
  (на Sonnet и Opus — завершающее system `<total_tokens>`); в `tools` нет; занято 3
  из 4 брейкпоинтов. `ttl: 1h` только при токене подписки; при API-ключе,
  у субагентов и в `/compact` — 5 минут. Смена `tools` обнуляет кэш с начала.
- Повторы: 429 без `retry-after`, 500, 529 — до 10 повторов, паузы 0,5 с … 35 с,
  затем 30–40 с (~2,5 минуты); `retry-after` задаёт паузу точно; обрыв потока —
  повтор через ~0,6 с, после второго — без `stream`. Не повторяются 429 с
  `anthropic-ratelimit-unified-status: rejected` и 400 «prompt is too long».
  `CLAUDE_CODE_RETRY_WATCHDOG=1` поднимает предел до 300.
- Лимиты подписки: заголовки `anthropic-ratelimit-unified-5h-utilization`,
  `-5h-reset`, `-7d-utilization`, `-7d-reset` (и `-status`,
  `-surpassed-threshold`, `-representative-claim`). Статусная строка получает
  `rate_limits.five_hour` и `seven_day` (`used_percentage`, `resets_at`) — только
  при подписочном токене и после первого ответа; на stdin также `session_id`,
  `transcript_path`, `cwd`, `model`, `workspace`, `version`, `cost`,
  `context_window`, `prompt_cache`, `effort`, `thinking`, `output_style`,
  `prompt_id`, `fast_mode`, `scratchpad_dir`, `exceeds_200k_tokens`.

Для шлюза Claude Code:
- Проксировать `/v1/messages`, `/v1/messages/count_tokens`, `HEAD /api/hello`;
  из контейнера нужен выход на `api.anthropic.com` либо
  `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1`.
- Основной цикл — запрос с `tools` без типизированных серверных инструментов и без
  `x-claude-code-agent-id`. В компакт инструкции не подмешивать: попадут в сводку.
- Менять `system[2]`, описания `tools`, env-сообщение можно только
  детерминированно и одинаково на каждом запросе сессии (начало префикса; смена
  runner рвёт кэш, поэтому смену передавать дельтой в хвосте). Инструкции в хвост —
  system-сообщением после `<total_tokens>` (Sonnet, Opus, Fable) или user-блоком
  (Haiku); они не попадают в историю клиента и оплачиваются как некэшированный вход
  на каждом запросе.
- Нельзя: переименовывать инструменты и менять их схемы; менять историю assistant,
  thinking, `tool_use` id и входы; трогать `metadata`, `safeguards`, `thinking`,
  `context_management`, `anthropic-beta`, `usage` в SSE. Не установлено: `cch`,
  серверная проверка истории и thinking, приём изменённого запроса.
- `Authorization` и `x-api-key` в журнал не писать.

### Codex CLI

- `POST <base>/responses`, SSE, вся история на каждый вызов. Провайдер `openai`
  сначала 5–7 попыток WebSocket; свой провайдер: `supports_websockets = false`.
  Zstd-сжатие тела — только при подписке и провайдере с именем `OpenAI`.
- Поля: `model`, `instructions`, `input`, `tools`, `tool_choice: auto`,
  `parallel_tool_calls`, `reasoning`, `store: false`, `stream: true`,
  `include: ["reasoning.encrypted_content"]`, `prompt_cache_key`, `text`,
  `client_metadata`.
- Заголовки: `originator` (`codex_exec`, `codex-tui`), `user-agent`, `version`,
  `session-id`, `thread-id`, `x-codex-turn-metadata` (JSON: `request_kind` —
  `turn`, `compaction`, `prewarm`; `thread_source` — `user`, `subagent`,
  `thread_title`; `turn_trigger`; `subagent_kind`; `sandbox_mode`). Подписка:
  `chatgpt-account-id`; подагент: `x-codex-parent-thread-id`, `x-openai-subagent`.
- Модели:

| | gpt-5.5 | gpt-6.x, gpt-5.6.x |
|---|---|---|
| `instructions` | полный базовый промпт | пусто; промпт — первое `developer`-сообщение |
| `tools` | массив | нет; `input[0]` — `additional_tools` |
| Инструменты | `exec_command`, `write_stdin`, `apply_patch`… | `exec` (JavaScript) и `wait` |
| `parallel_tool_calls` | `true` | `false` |

  gpt-6: `exec` — custom-инструмент с сырым JS; внутри `tools.exec_command(...)`,
  `tools.apply_patch(...)`, `tools.mcp__сервер__имя(...)`. Шлюз видит только `exec`,
  вложенные вызовы скрыты, но файлы и команды всё равно идут через FUSE и оболочку.
  Нужен `codex-code-mode-host` рядом с `codex`. Модель по умолчанию без `model` —
  `gpt-6.1-sol`. Неизвестная модель (`vendor/unknown-model`): нет `apply_patch`;
  `apply_patch <<EOF` через `exec_command` перехватывается в процессе. Смена
  поколения каталога запускает сжатие.
- Конец хода — `response.completed` без вызовов инструментов; вызов — целиком в
  `output_item.done`. `incomplete` с `interrupted` — прерывание, иначе ошибка.
- Служебные запросы: заголовок треда (только TUI; `tools: []`, json_schema
  title); сжатие у своего провайдера — локально, `tools: []`, «You are performing
  a CONTEXT CHECKPOINT COMPACTION…»; у провайдера `openai` — `POST /responses` с
  `{"type":"compaction_trigger"}`, ждёт ровно один выходной элемент `compaction`;
  `/review` — отдельный тред с `x-openai-subagent: review`; `/goal` —
  автопродолжение `<codex_internal_context source="goal">`.
- Подписка: `/backend-api/*` (`codex/models`, `wham/accounts/check`,
  `plugins/featured`, …); без сети работает, пишет ошибки. Старт ходит на
  `git ls-remote https://github.com/openai/plugins.git`.
- Ошибки: 429 `usage_limit_reached` и прочие 429 — без повторов;
  `context_length_exceeded` — без повторов; `rate_limit_exceeded` — 6 запросов;
  500 — 30 за ~25 с; 401 и обрыв потока — по 6.
- Лимиты подписки — заголовки `x-codex-primary-*`, `x-codex-secondary-*`;
  внешней команды статусной строки нет.

Для шлюза Codex:
- Классифицировать запросы по `x-codex-turn-metadata` и заголовкам подагентов.
- Провайдер с именем `OpenAI` включает remote compaction v2 (шлюз обязан ответить
  `compaction`); свой провайдер называть иначе, `supports_websockets = false`.
- При подписке нужен HTTPS (`CODEX_CA_CERTIFICATE`) и ответ на
  `/backend-api/wham/accounts/check`; проще свой провайдер с
  `requires_openai_auth = true` — работает по http.
- Инструкции в хвост `input` (новое `developer`) или `additionalContext`;
  `instructions` и `tools` — кэш-префикс.
- Переименование вызова симметрично в потоке и в `input` возможно (Codex
  возвращает `name` и `namespace` как есть, неизвестное — `unsupported call`);
  на настоящем API не проверено.
- Перехват проще у gpt-5.5.

### agy

- Два входа: аккаунт Google — `POST
  https://daily-cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse`,
  `Authorization: Bearer`, адрес меняет `CLOUD_CODE_URL` (http допустим); ключ
  Gemini API — `POST /v1beta/models/<m>:streamGenerateContent?alt=sse`,
  `x-goog-api-key`, `GOOGLE_GEMINI_BASE_URL`.
- Аккаунт: на `CLOUD_CODE_URL` уходят все `v1internal:*` — `loadCodeAssist`,
  `onboardUser`, `fetchAvailableModels`, `retrieveUserQuotaSummary`,
  `fetchUserInfo`, `listExperiments`, `fetchAdminControls`,
  `writeTrajectoryAcls`; обмен токена и `userinfo` остаются на хостах Google.
  Обёртка: `{project, requestId, model, userAgent:"antigravity", requestType,
  request{contents, systemInstruction, generationConfig, sessionId}}`; у заголовка
  беседы `requestType:"checkpoint"`. Ответ ждёт `{response:{candidates,
  usageMetadata}, traceId}`. Без валидного `fetchAvailableModels` (`models`,
  `defaultAgentModelId`, `agentModelSorts`) сессия падает «plan model not
  specified»; схема не снята, основной запрос аккаунта и набор инструментов (?).
- Ключ API: тело без обёртки — `contents`, `systemInstruction`, `tools` (один
  `functionDeclarations`, `parametersJsonSchema`), `generationConfig`; нет
  `toolConfig`, `safetySettings`, `cachedContent`. `User-Agent:
  google-genai-sdk/1.71.0 gl-go/…`. У аккаунта: `antigravity/cli/1.2.14
  (aidev_client; …; auth_method=consumer)`.
- Модель на проводе: `gemini-3.1-pro-preview`, `gemini-3.8-flash`,
  `gemini-3.7-flash`; слаг `--model gemini-3.8-flash-medium` ≠ id на проводе.
- Конец хода — `finishReason: STOP` без `functionCall`. У `functionCall` нет `id`:
  agy ставит UUID и хранит в истории. `thoughtSignature` обязана быть base64; на
  `functionCall` сохраняется. Результаты инструментов идут с `role:"model"`
  (`functionResponse`), системные события — `user` `<SYSTEM_MESSAGE>`.
- Служебные запросы (ключ API, без `tools`): заголовок беседы
  (`gemini-3.1-flash-lite-preview`), `search_web`, `generate_image`; субагент —
  свой запрос с 7 инструментами. `read_url_content`, `search_web`,
  `generate_image` исполняются в контейнере agy, не на runner.
- Повторы (`-p`, ключ API; код выхода 3, stderr `AGY_ERROR: {…}`): 400, 401, 500 —
  без повтора; 503 и 429 без деталей — 9 запросов за 136–157 с; 429 с `RetryInfo`
  ≤ 30 с — ждёт и повторяет, > 30 с или дневная `QuotaFailure` — стоп; обрыв
  потока — 9 попыток за 157 с. Для аккаунта не снято (?).
- Лимиты: `retrieveUserQuotaSummary` (`buckets`: `bucketId`, `window`,
  `remainingFraction`, `resetTime`) → `/usage` и поле `quota` в JSON `statusLine`;
  окна 5 часов и неделя (д). При ключе API `quota`, `email`, `plan_tier` нет;
  живое значение у аккаунта (?).
- Исходящие хосты: `daily-cloudcode-pa.googleapis.com`,
  `generativelanguage.googleapis.com`, `oauth2.googleapis.com`,
  `www.googleapis.com`, `accounts.google.com`, `play.googleapis.com/log`
  (телеметрия), `antigravity-unleash.goog` (флаги), `storage.googleapis.com` и
  `…run.app` (обновления).

Для шлюза agy:
- Режим аккаунта: шлюз отвечает на `loadCodeAssist`, `onboardUser`,
  `fetchAvailableModels`, `retrieveUserQuotaSummary` либо проксирует их.
- Инструкции — файлами в FUSE; в `systemInstruction` шлюз дописывает один раз за
  сессию (сбрасывает неявный кэш); хвост `contents` не трогать.
- Переименование вызовов: имя проверяется по реестру; преобразование аргументов
  обратимо (agy валидирует схему).

## Инструменты

### Claude Code

- Всегда (20): Agent (в `init` stream-json — `Task`), Bash, CronCreate, CronDelete,
  CronList, Edit, EnterWorktree, ExitWorktree, ListAgents, NotebookEdit, Read,
  ReportFindings, ScheduleWakeup, SendMessage, Skill, TaskStop, WebFetch, WebSearch,
  Workflow, Write.
- Условно: Monitor, PushNotification, DesignSync — пока не заданы
  `DISABLE_TELEMETRY` и `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` (Monitor — по
  документации, остальные — наблюдение); Task* (TaskCreate, TaskGet, TaskList,
  TaskUpdate) на Haiku 4.5, Opus 4.6 или при `CLAUDE_CODE_ENABLE_TODO_TOOLS=1`, на
  Sonnet 5.5 и Opus 5.5 нет; Glob и Grep только через `--tools` или `--allowedTools`
  (`--allowedTools Glob` даёт оба; по документации ещё при убранном Bash);
  `ToolSearch` при `ENABLE_TOOL_SEARCH=true` (остаются Agent, Bash, Edit, ListAgents,
  Read, ReportFindings, ScheduleWakeup, Skill, Workflow, Write и
  `DeferredToolPlaceholder`); `CLAUDE_CODE_SIMPLE=1` — Bash, Edit, Read.
  `--tools A,B` — ровно эти. Субагент general-purpose: Agent, Bash, Edit,
  EnterWorktree, ExitWorktree, NotebookEdit, Read, SendMessage, Skill, TaskStop,
  WebFetch, WebSearch, Write.
- Только по документации (в запусках не появились, нужен аккаунт или TUI): LSP,
  RemoteTrigger (подписка), AskUserQuestion, EnterPlanMode, ExitPlanMode (TUI),
  Artifact, SendFeedback, SendUserFile, ShareOnboardingGuide, EndConversation,
  PowerShell (нужен `pwsh`), SubagentHandback (auto mode), TodoWrite, TaskOutput,
  ListMcpResourcesTool, ReadMcpResourceTool, WaitForMcpServers (ресурсы MCP — при
  объявлении). В боевом контейнере с сетью возможны.
- Мимо FUSE и оболочки: WebFetch и PushNotification идут из сети агента; WebSearch
  — обычный запрос к `ANTHROPIC_BASE_URL` с `web_search_20250305` (`max_uses` 8);
  ListAgents и SendMessage работают через сокеты `/tmp/cc-socks/<n>.sock` и видят
  все сессии Claude Code с общим `/tmp`.
- Glob и Grep — в процессе агента, `rg` из бинарника. EnterWorktree создаёт
  `.claude/worktrees/<имя>` (ветка `worktree-<имя>`), требует git и коммит; кто
  запускает git — агент или оболочка, не снято (?). NotebookEdit требует Read
  («File has not been read yet»), способ записи не снят (?). Cron-задачи — в
  памяти, `durable` — `.claude/scheduled_tasks.json` (д).
- Встроенные Read, Edit, Write, Grep, Glob работают в процессе; замены нет.
  Порядок в `tools`: встроенные по алфавиту, затем `mcp__*`.
- Ошибки: «File does not exist…», `<tool_use_error>String to replace not found in
  file.`, «Found N matches … replace_all is false», неизвестное имя — `<tool_use_error>Error: No such tool
  available: X`. Параллельные вызовы — одно user-сообщение с несколькими
  tool_result.
- Права в `-p`: default — «Claude requested permissions to write to … but you
  haven't granted it yet.»; dontAsk — отказ «…running in don't ask mode»;
  acceptEdits разрешает Edit и Write. Правила permissions проверяются на входе,
  вернувшемся из хука; HTTP-хук при недоступности не блокирует вызов.
- WebFetch отказывает `localhost` и именам без точки; без сети — «Unable to
  verify if domain … is safe to fetch».
- MCP-результат больше 25 000 токенов — `<persisted-output>` в
  `tool-results/<toolu_id>.json`.

### Codex CLI (gpt-5.5, по умолчанию)

| Инструмент | Результат |
|---|---|
| `exec_command`, `write_stdin` | `Chunk ID…\nWall time…\nProcess exited with code N\nOutput:\n…`; фоновый — `Process running with session ID N` |
| `apply_patch` (custom, grammar lark) | `Exit code: 0\nWall time: 0 seconds\nOutput:\nSuccess. Updated the following files:\nA\|M\|D путь`; ошибки `apply_patch verification failed: …` (`Failed to read file to update …`; `*** Move to:` — после `@@`, не сразу за `Update File`) |
| `view_image` | `input_image` |
| `request_user_input` | вне Plan: «unavailable in Default mode» |
| `get_goal`, `create_goal`, `update_goal` | JSON |
| `tool_search` | BM25 по отложенным: `multi_agent_v1` (з), MCP (д); `defer_loading: true` |
| `web_search` | на стороне API |
| MCP | namespace `mcp__<сервер>`, всегда за `tool_search`; вызов по `namespace` + `name` работает и без поиска |
| `spawn_agent`, `close_agent`, `resume_agent`, `wait_agent`, `send_input` | namespace `multi_agent_v1`, за `tool_search`; поток в том же процессе, запуск не снят (?) |

- MCP-результат режется примерно до 48 КБ; `isError` в результат не попадает.
- Ошибки диспетчера: `unsupported call: <имя>`; `failed to parse function
  arguments`; `approval policy is Never; reject command…`. Легаси-инструмента
  `shell` нет.
- Отключение: `features.shell_tool|view_image|multi_agent = false`,
  `web_search = "disabled"`; остаются `request_user_input`, `apply_patch`, `*_goal`.
  `features.multi_agent_v2=true` — вместо `multi_agent_v1` namespace `collaboration`
  без отложения (`followup_task`, `interrupt_agent`, `list_agents`, `send_message`,
  `spawn_agent`, `wait_agent`); `features.goals=false` убирает `*_goal`.
- gpt-6.x: `exec` (custom, JS), `wait`, `request_user_input`,
  `request_user_input_async` (namespace `functions`), `sleep` (`clock`),
  `collaboration`; внутри `exec` — `apply_patch`, `exec_command`, `write_stdin`,
  `view_image`, `*_goal`, `clock__curr_time`. Вызов `exec` не снят (?).
- Запуск команд идёт через `/proc/self/exe --codex-run-as-process-setup`; MCP,
  `web_search` как вызов, песочницы bwrap и FUSE-точка не снимались (?).
- `apply_patch` в `workspace-write` и `read-only` идёт подпроцессом через bwrap.

### agy

- В запросе объявлено 14 инструментов при ключе API (в `-p` и TUI, при любой
  модели): `view_file`, `run_command`, `manage_task`, `send_message`, `schedule`,
  `invoke_subagent`, `define_subagent`, `manage_subagents`, `write_to_file`,
  `replace_file_content`, `generate_image`, `read_url_content`, `search_web`,
  `ask_question`; набор при входе аккаунтом (?). Реестр в событии `init` — 58 имён
  (ещё `grep_search`, `find_by_name`, `list_dir`, `notebook_edit`, `run_workflow`,
  `multi_replace_file_content`, `sed_file`, 22 `browser_*`, MCP-мета и служебные);
  не объявленные дают `unknown tool`. Агент `.agents/agents/<имя>.md` с
  `tools: [...]` объявляет и `grep_search`, `find_by_name`, `list_dir`,
  `notebook_edit`, `run_workflow`; имён вне его реестра (`browser_*`, `sed_file`,
  `call_mcp_tool`, …) агент не принимает.

| Инструмент | Результат |
|---|---|
| `view_file` (`AbsolutePath`, `StartLine`, `EndLine`, `ContentOffset`) | строки `N: …`, до 800 строк и 46 080 байт; также изображения, pdf, видео, аудио; неизвестный mime — ошибка |
| `write_to_file` (`TargetFile`, `CodeContent`, `Overwrite`, `Append`) | «Created file file://…»; существующий без `Overwrite` — ошибка |
| `replace_file_content` (`TargetFile`, `StartLine`, `EndLine`, `TargetContent`, `ReplacementContent`, `AllowMultiple`) | `[diff_block_start]…`; не найдено — «target content not found» |
| `run_command` (`CommandLine`, `Cwd`, `WaitMsBeforeAsync`, `RunPersistent`) | `The command exited with code N.\nOutput:\n…` |
| `manage_task`, `schedule` | список и kill фоновых команд; таймеры и cron в процессе agy (в `-p` блокирует) |
| `invoke_subagent`, `define_subagent`, `manage_subagents`, `send_message` | подагенты в том же процессе; `Workspace: share\|branch` — `git worktree` в каталоге данных agy, без `git` в `PATH` возврат к исходному workspace |
| `read_url_content`, `search_web`, `generate_image` | HTTP GET и запросы к модели из процесса agy, мимо runner; чтение URL по умолчанию спрашивает разрешение |
| `ask_question`, `ask_permission` | окно TUI |
| `grep_search`, `find_by_name`, `list_dir`, `notebook_edit` | в процессе, дерево читается через FUSE; `grep_search` распаковывает `rg` в `~/.cache/antigravity/bin/` (запись вне проекта), без него `grep -r` (з) |
| `run_workflow` (`ScriptPath`) | `python3 -I` из `PATH` исполняет процесс agy напрямую, мимо оболочки |
| `call_mcp_tool`, `list_resources`, `read_resource` | при настроенном MCP |

- Не снято (?): `multi_replace_file_content`, `sed_file`, `notebook_execution`,
  `run_script` (Python, в исходниках; в реестр не попал), `manage_inbox`,
  `delete_knowledge`, `list_permissions`, `browser_*` (нужен Chrome), запись под
  FUSE.
- Ошибки — текст в `output`; вызовы несут обязательные `toolSummary` и
  `toolAction`. Неизвестное имя (`bash`) — «Encountered error in tool validation:
  unknown tool»; имя остаётся в истории. В `-p` без
  `--dangerously-skip-permissions` отказ попадает в `denied_actions`.

## Инструкции

| | Claude Code | Codex CLI | agy |
|---|---|---|---|
| Файлы | `CLAUDE.md`, `.claude/CLAUDE.md`, `.claude/rules/*.md`, `CLAUDE.local.md`; `AGENTS.md` — если нет `CLAUDE.md`, оба — при `pluginConfigs."agents-md@builtin".options.instructionFiles = "claude-md-and-agents-md"` (з) | `AGENTS.md` от корня проекта (каталог с `.git`) до cwd, без `.git` — только cwd; `AGENTS.override.md` заменяет в своём каталоге | `AGENTS.md`, `GEMINI.md` корня (`CLAUDE.md` не читается); `.agents/rules/*.md` с `trigger: always_on` |
| Глобальные | `/etc/claude-code/CLAUDE.md` и `.claude/rules/*.md` (managed), затем `$CLAUDE_CONFIG_DIR/CLAUDE.md` и `rules/*.md` (`/etc/claude-code/rules/` не читается) | `$CODEX_HOME/AGENTS.override.md`, иначе `AGENTS.md`; `additional_developer_instructions` в `/etc/codex/requirements.toml` | `~/.gemini/AGENTS.md`, `GEMINI.md`, `config/rules/*.md` |
| Куда | `messages[0]`, блок 1, секции `Contents of <путь>`; затем индекс `MEMORY.md` | `user`-сообщение `# AGENTS.md instructions for <cwd>` перед `<environment_context>`; `developer_instructions` — первое `developer` | `systemInstruction`, `<user_rules>`, теги `<RULE[путь]>` |
| Перечитывание | раз за процесс; правка загруженного файла — заметка «changed on disk» (только TUI, один раз на файл); новые файлы посреди сессии не подхватываются | TUI: раз за сессию; глобальный файл — на каждом запросе, правка даёт `user`-сообщение «These AGENTS.md instructions replace all previously provided…»; `codex exec resume` читает заново | в начале каждого пользовательского хода; внутри хода хеш неизменен; модели не сообщается |
| Вложенные | `CLAUDE.md`, rule с `paths:` — system-сообщением после Read подходящего файла; Write и Edit без Read не подгружают | нет | `sub/AGENTS.md` не подхвачены (з) |
| Пределы | — | `project_doc_max_bytes` режет молча | 24 КБ на файл, 20 000 токенов на все правила |

Claude Code:
- Порядок в `messages[0]`: managed, глобальный пользовательский (импорты `@путь`
  раскрываются), rules без `paths:`, проектные, `CLAUDE.local.md`, `AGENTS.md`.
- `--resume` и `--continue` оставляют `messages[0]` прежним и присылают
  изменённые и новые файлы дельтой — первым блоком первого user-сообщения хода
  («Instruction files were re-read when this session started…»); `/clear`
  пересобирает блок целиком. `--bare` — без CLAUDE.md и памяти.
- Skills — список в env-сообщении (пользовательские, проектные, встроенные,
  `.claude/commands/*.md`, плагинные `plugin:skill`), текст — при вызове; новый
  skill подхватывается, только если `.claude/skills` существовал на старте. Свои
  агенты `.claude/agents/*.md` — строка в «Available agent types». Субагенты
  general-purpose и свои получают блок CLAUDE.md, Explore — нет.

Codex:
- Базовый промпт заменяют целиком `instructions` в config.toml,
  `model_instructions_file`, `baseInstructions`, `model_catalog_json`;
  `personality = "none"` убирает `# Personality`.
- Переключатели `include_environment_context`, `include_permissions_instructions`,
  `include_apps_instructions`, `include_collaboration_mode_instructions` убирают
  блоки. Профили: `$CODEX_HOME/<имя>.config.toml`, `--profile`.
- `input` только растёт, префикс стабилен. Кэш ломают смена модели, смена
  песочницы и смена набора MCP.
- Скиллы: список в `<skills_instructions>`, тело — `<skill>` при выборе через
  app-server.

agy:
- `model_decision` — путь и описание в промпте, тело читает сам; `glob` —
  дописывается к `functionResponse` вызова на подходящий файл; `manual` — по `@`.
- Навыки: описание в `<skills>`, тело читает `view_file`. Инструкции MCP-сервера
  — файл `~/.gemini/antigravity-cli/mcp/<сервер>/instructions.md`.
- `systemInstruction` ≈ 16 000 символов (`<identity>`, `<user_information>`,
  `<user_rules>`, `<skills>`, `<mcp_servers>`, …), одинаков на всех запросах хода
  и при resume.

Вывод для Iapetus: инструкции runner доставляются файлами через FUSE, не шлюзом
(тело запроса остаётся сформированным агентом). Глобальные и managed-файлы у
Claude Code и Codex перечитываются по-разному (см. строку «Перечитывание»).

### Правила при продолжении разговора

Claude Code 2.1.285, Codex 0.159.3, agy 1.2.14; запуск на поддельных API без токенов (з); реакция настоящей модели не снята (?). Файл правил Iapetus у каждого агента один: Claude Code — `/etc/claude-code/.claude/rules/iapetus.md`, Codex — `~/.codex/AGENTS.md`, agy — `~/.gemini/config/rules/iapetus.md` с `trigger: always_on`.

| | Claude Code `--continue` | Codex `resume --last` | agy `-c` |
|---|---|---|---|
| Новый запуск | все файлы правил одним блоком `<system-reminder>` в `messages[0]`, в `system` их нет | глобальный и проектный `AGENTS.md` — одним `user`-сообщением, управляемые требования — `developer`-сообщением | `<user_rules>` в `systemInstruction`, пересобирается на каждом запросе |
| Продолжение после правки файла | в `messages[0]` остаются прежние версии, изменённые файлы приходят в первом `user`-сообщении нового хода блоком «each replaces its earlier copy» (сравнение по содержимому, а не по mtime) | история хранит все версии, новая приходит сообщением «replace all previously provided» | только актуальные правила, истории версий нет |
| Удаление и новый файл | «Instructions no longer present: <путь>»; новый файл приходит таким же блоком | — | удалённый файл исчезает со следующего `-c`, новый подхватывается |
| Без правок | блока нет | блока нет | — |
| Правка в живом процессе | заметка «changed on disk since you last read it» с полным текстом, один раз на файл за процесс; замена через `rename` сработала один раз; правка с прежним mtime не замечена; новый файл не подхватывается | глобальный `~/.codex/AGENTS.md` — на следующем запросе; проектный `AGENTS.md` и `requirements.toml` — раз за сессию | со следующего хода |

- Инструкции MCP-сервера Codex кладёт в описание инструмента (`tools`), в историю они не пишутся: после `resume` всегда актуальны, дублей нет.
- Каждая правка у Claude Code и Codex добавляет полный текст в историю навсегда; у agy правка правил сбрасывает кэш всего префикса (с, не измерялось).
- Не снято (?): проектный `CLAUDE.md` через FUSE (важен настоящий mtime от FUSE), `AGENTS.md` Claude Code, правка agy посреди хода, реакция настоящей модели на «replaces».

## MCP

Спецификация MCP 2026-07-28: без сессий и `initialize`, `server/discover`,
`list_changed` — по `subscriptions/listen`. Версии клиентов: Claude Code 2.1.285,
Codex 0.159.3, Gemini 0.62.0, agy 1.2.14.

| | Claude Code | Codex CLI | agy | Gemini CLI |
|---|---|---|---|---|
| Версия по HTTP | `server/discover` 2026-07-28, откат на 2025-11-25; `initialize` 2025-11-25 | 2025-06-18 | `server/discover`, затем `initialize` 2025-11-25 | 2025-06-18 |
| `list_changed` посреди сессии | добавленные инструменты в `tools` со следующего запроса, удаление не отражается; замена набора — перезапуском; смена `tools` обнуляет кэш | не подхватывается (только журнал); новый набор — `config/mcpServer/reload` (app-server) при изменении конфигурации сервера | список перечитывается, `systemInstruction` и `tools` не меняются; новый сервер не подхватывается | подхватывается |
| Имя | `mcp__<сервер>__<инструмент>`; плагин `mcp__plugin_<плагин>_<сервер>__…` | namespace `mcp__<сервер>` | ленивые: имена в `<mcp_servers>`, схема — файл, вызов `call_mcp_tool` | `mcp_<сервер>_<инструмент>` |
| В запросе | все сразу | за `tool_search` | см. имя | все сразу |
| Предел результата | 25 000 токенов, остальное в файл | ~48 КБ | 60 000 символов → файл (порог не снят) | 8 000 + 32 000 символов, остальное в файл |
| Таймаут | нет, простой HTTP 5 мин | `tool_timeout_sec` 60 с | — | 600 с |
| Проект | `.mcp.json` | `.codex/config.toml` доверенного проекта | `.agents/mcp_config.json` (глобальный `~/.gemini/config/mcp_config.json`; ключ `serverUrl`, не `url`) | `.gemini/settings.json` |
| Запрет проектных | `/etc/claude-code/managed-mcp.json` (исключительный; с `--mcp-config` — выход 1) | `/etc/codex/requirements.toml` | системного файла нет | `/etc/gemini-cli/settings.json` `mcp.allowed` (файл root, каталог без записи группе) |

- Claude Code: `bypassPermissions` запускает stdio-серверы из `.mcp.json` без
  вопроса; `enableAllProjectMcpServers: false` и `enabledMcpjsonServers: []` не
  блокируют, блокируют managed-mcp.json, `--strict-mcp-config`,
  `disabledMcpjsonServers` с именами. `managedMcpServers` принимает только
  `https://`; server-managed settings при своём base URL не загружаются.
- Claude Code: HTTP-сервер при подключении получает `server/discover` (достаточно
  вернуть ошибку метода), `initialize`, `initialized`, GET (SSE), `tools/list`.
  Отключение сервера — system «…have disconnected», `tools` не меняется, появляется
  `WaitForMcpServers`; HTTP переподключается сам, stdio — только `/mcp` →
  Reconnect. Посреди сессии не видны `claude mcp add`, правка `.mcp.json`, плагин;
  `/reload-plugins` без `--force` отказывает (перечитывает весь разговор без кэша).
  `# MCP Server Instructions` приходит после подключения.
- Codex: список обычного сервера фиксирован на соединение; кэш каталогов 30 минут
  (отключает сервер: `capabilities.experimental["codex/tool-catalog-cache"] =
  {"cacheable": false}`). `config/mcpServer/reload` переподключает только сервер с
  изменённой конфигурацией. Правка upstream для `list_changed` (PR #12449) закрыта
  без слияния, внешние PR принимаются только по приглашению. Способы по
  надёжности: форк с патчем; демон app-server + reload (nonce в URL); перезапуск
  на границе хода через `resume`.
- agy: проектный MCP в TUI стартует только в каталоге из `trustedWorkspaces`, в
  `-p` всегда; `tools.eager` ломает загрузку сервера.
- Все пишут `progressToken` с каждым вызовом (у agy не проверено), sampling не
  объявляет никто; `Authorization: Bearer ${VAR}` подставляется из окружения.
- Надёжнее всего стабильный набор инструментов с динамикой внутри; затем
  `list_changed` на добавление; удаление и замена — перезапуском.
- Готового шлюза «MCP поверх NATS» нет; Rust SDK `rmcp` 3.5 (Apache-2.0)
  поддерживает 2026-07-28 и 2025-11-25.

## Headless, ACP, TUI

Справочно: свой TUI поверх headless-режима отменён, родной интерфейс агента работает в pty session.

| | Claude Code | Codex CLI | agy |
|---|---|---|---|
| Многоходовой headless | `-p --input-format stream-json --output-format stream-json`, один процесс | `codex app-server` (JSON-RPC по stdio или WebSocket); `exec --json` — один ход | `-p --input-format stream-json`, один процесс; слэш-команды и `control_request` завершают сессию с кодом 2 |
| События | `system/init`, `stream_event`, `rate_limit_event`, `system/task_*`, `result` | `item/*`, `turn/completed`, `account/rateLimits/updated` | `init`, `step_update`, `result` |
| Прерывание, сообщение посреди хода | control `interrupt`; сообщение в очередь | `turn/interrupt`, `turn/steer` | нет |
| Разрешения | control `can_use_tool` | `item/*/requestApproval` | нет; `--dangerously-skip-permissions` |
| Resume | `--resume`, `--continue` | `codex exec resume` | `--conversation <id>`, `-c`; несуществующий id — новая беседа, код 0 |
| ACP | адаптер на Agent SDK | адаптер поверх app-server | нет (есть `remote-control` и Python SDK) |

- ACP не заменяет FUSE и подменённую оболочку: агент читает, пишет и запускает
  сам; черновик ACP v2 убирает `fs/*` и `terminal/*`.
- Перевод в родной TUI: остановить headless и запустить TUI с `--resume <id>`
  (оба одновременно нельзя). Codex: `codex --remote ws://…` — вместе с другими
  клиентами (экспериментально). Codex app-server умеет `chatgptAuthTokens` с
  обновлением через `account/chatgptAuthTokens/refresh` (не проверено).
- `-p` и TUI Claude Code отличаются: `User-Agent`, `cc_entrypoint` (`sdk-cli` /
  `cli`), system[1], `thinking.display`, бета `redact-thinking` (TUI), набор
  инструментов (в `-p` нет AskUserQuestion, EnterPlanMode, ExitPlanMode), проба
  квоты и заголовок сессии (только TUI). `codex exec` всегда
  `approval_policy = never`; `<collaboration_mode>` и заголовок треда — только TUI.
- TUI Claude Code в pty без tmux (2.1.285): перерисовывает видимую часть
  (`CSI H`, `CSI 2K`), историю выше экрана не трогает; смена размера — полная
  перерисовка; не использует альтернативный экран, мышь, truecolor, `?2026h`;
  запрашивает DA1, XTVERSION, kitty keyboard, без ответов работает; `OSC 0`
  меняет заголовок каждый ход.
- Slash-команды Claude Code: локальные (`/model`, `/effort`, `/mcp`) запроса не
  шлют, а добавляют блоки `<command-name>` и `<local-command-stdout>` в следующее
  user-сообщение; `/clear` — пустая история и новый id сессии; `/compact` — запрос
  компакта, после него `messages[0]` — «This session is being continued…».

## Первый запуск

| | Убирают экраны |
|---|---|
| Claude Code | `hasCompletedOnboarding`, `hasSeenAutoDefaultNudge`, `hasSeenAutoDefaultNotice`, `projects["/"].hasTrustDialogAccepted` (вне git-репозитория; в репозитории и worktree — по пути проекта или корня репозитория) в `~/.claude.json` (при `CLAUDE_CONFIG_DIR` — `$CLAUDE_CONFIG_DIR/.claude.json`; Claude Code сам дописывает файл); ключ API — `customApiKeyResponses.approved`; bypass — `skipDangerousModePermissionPrompt` в `settings.json`. Токен в окружении экраны не убирает |
| Codex | `[projects."<cwd>"] trust_level = "trusted"`, `[notice.model_migrations]` (привязано к версии каталога), `approval_policy = never` + `sandbox_mode = danger-full-access` |
| agy | `~/.gemini/antigravity-cli/cache/onboarding.json` (`consumerOnboardingComplete`, `enterpriseOnboardingComplete`, `onboardingComplete` = `true`); `settings.json`: `trustedWorkspaces`, `colorScheme`, `showTips:false`, `showFeedbackSurvey:false`; без подтверждений — `toolPermission` и `artifactReviewPolicy` = `always-proceed`, для `-p` `--dangerously-skip-permissions` |

Песочница терминала agy выключена по умолчанию (`enableTerminalSandbox:false`);
включённая (`--sandbox`) отдаёт команду помощнику — самому бинарнику (urpc по
абстрактному unix-сокету, seccomp), в Docker не стартует; использует ли он `bash`
из `PATH`, неизвестно (?).

## Авторизация, лимиты, условия

Способы входа целиком — `AUTH.md`; риски — `RISKS.md`.

| | Claude Code | Codex CLI | agy |
|---|---|---|---|
| Вход | подписка claude.ai, `claude setup-token` (год), ключ API | подписка ChatGPT (браузер или `codex login --device-auth`), `CODEX_ACCESS_TOKEN` (Business/Enterprise), ключ API (`CODEX_API_KEY`, `codex login --with-api-key`) | аккаунт Google (PKCE, вставка кода в TUI, по ssh работает), ключ Gemini API (`settings.json` `"modelProvider":"gemini"` + `GEMINI_API_KEY`), ADC (`AGY_ADC_AUTH=true`) |
| Хранение | `.claude.json` / `CLAUDE_CONFIG_DIR` | `~/.codex/auth.json` | `~/.gemini/antigravity-cli/antigravity-oauth-token` (JSON с `refresh_token`; без D-Bus keyring пишет файл сам) |
| Не проверено | — | перенос между контейнерами, одновременное использование, ротация refresh token | срок refresh_token, ротация, копирование между контейнерами |

- Codex: TUI не читает `CODEX_API_KEY` (д); `OPENAI_API_KEY` не читается; постоянного
  токена для Plus/Pro нет.
- agy: `-p` без входа — «authentication required»; тарифы: без Pro/Ultra квота раз в
  неделю, Pro — окно 5 часов до недельного лимита, Ultra — максимум; возраст 18+.

Условия:
- **Claude** (code.claude.com/docs/en/legal-and-compliance): OAuth — для обычного
  использования Claude Code и родных приложений; продуктам, в том числе на Agent SDK,
  — ключи API. Запрещено направлять запросы через учётные данные Free, Pro, Max
  от имени своих пользователей и собирать, хранить или посредничать с учётными
  данными claude.ai. Разрешено: пользователь входит своей подпиской в
  немодифицированный бинарник, в том числе размещённый платформой; ключи API
  владелец может давать своим людям, если оплата идёт ему. Бинарник нельзя
  модифицировать и нельзя отключать его способы входа.
- **agy** (`antigravity.google/terms`): «Using third party software, tools, or
  services to access the Service (e.g. using OpenClaw with Antigravity OAuth) is a
  breach of this Agreement» — возможна блокировка аккаунта; «You must not abuse,
  harm, interfere with, or disrupt the Service. This includes … using the Service
  in connection with products not provided by us». FAQ стороннему агенту
  рекомендует ключ Gemini Enterprise или Google AI Studio. `docs/plans`: «no
  support for bring-your-own-key or bring-your-own-endpoint», хотя
  `GOOGLE_GEMINI_BASE_URL` и `CLOUD_CODE_URL` в бинарнике есть. Шлюз между agy и
  Code Assist при личном входе близок к «products not provided by us» (не
  юридический вывод). Для Enterprise и Cloud действуют условия администратора.
  Телеметрия и данные взаимодействий собираются, отключение — в настройках.
- **Codex**: для программных сценариев документация советует ключ API; запрета
  сторонних клиентов app-server не найдено.
- **Gemini CLI** (по пересказам, не перепроверено): с 2026-06-18 бесплатный тариф и
  Google AI Pro/Ultra не обслуживаются; CLI переходит в Antigravity CLI.

## OpenRouter

- Эндпоинты `/chat/completions`, `/messages` (Anthropic), `/responses`;
  нативного API Google нет. Оплата за токены; остаток по ключу — `GET /api/v1/key`;
  успешные ответы без `X-RateLimit-*`.
- **Claude Code** — официально: `ANTHROPIC_BASE_URL=https://openrouter.ai/api`,
  `ANTHROPIC_AUTH_TOKEN` — ключ OpenRouter, `ANTHROPIC_API_KEY=""`; гарантия только
  с провайдером Anthropic first-party. Заголовков `anthropic-ratelimit-unified-*`
  нет (лимитов подписки в статусной строке не будет); `cache_control` проходит.
- **Codex** — Responses только stateless (`store:true` и `previous_response_id` —
  400); у моделей `vendor/model` нет метаданных: `apply_patch` как инструмента нет,
  `apply_patch <<EOF` через оболочку перехватывается в процессе.
- **Gemini CLI** — только через переводчик (LiteLLM, claude-code-router).

## Gemini CLI 0.62.0 (сравнение)

В коробке заменён Antigravity CLI. Google, TypeScript, npm `@google/gemini-cli`.

- Вход: Google (Code Assist), ключ Gemini API, Vertex AI; Code Assist —
  `v1internal:streamGenerateContent` (`CODE_ASSIST_ENDPOINT`), ключ —
  `/v1beta/models/<m>:streamGenerateContent` (`GOOGLE_GEMINI_BASE_URL`, http только
  для localhost). Токен — `oauth_creds.json`.
- Файлы: на месте, `O_TRUNC` и один `write`; без «сначала прочитай». Команды:
  `bash -c` из `PATH`; cwd не переносится.
- Модель на проводе не равна выбранной: CLI переписывает id, при `auto` роутер
  (`flash-lite`) выбирает модель каждый ход; явная `model.name` убирает роутер.
- Инструкции: базовый промпт и `~/.gemini/GEMINI.md` — в `systemInstruction`;
  проектные `GEMINI.md` — в `contents[0]`; `AGENTS.md` только при
  `context.fileName`; полная замена промпта — `GEMINI_SYSTEM_MD`.
- Повторы: 429 с `QuotaFailure` — фатально; 429 без деталей и 5xx — 10 запросов
  за ~210–250 с; 400, 401, обрыв — без повторов.
- `list_changed` подхватывается; системные настройки читаются, только если файл
  принадлежит root, а каталог не записываем для группы.
- Отличия agy: Go, закрытый; нет роутера моделей, `GEMINI_SYSTEM_MD`, `mcp.allowed`,
  ACP; `AGENTS.md` без настройки; инструкции в `systemInstruction`; `thoughtSignature`
  base64; результаты инструментов с `role:"model"`; есть resume по id.

## Сводка по слоям Iapetus

По профилям «Агент в коробке» в `CONTRACT.md`.

| Слой | Claude Code | Codex CLI | agy |
|---|---|---|---|
| FUSE | tmp → коммит на `rename` (з) | `O_TRUNC` + `write` → коммит на `flush` (з) | то же (з) |
| Оболочка | `CLAUDE_CODE_SHELL`, ссылка `bash` (з) | `pw_shell`, `features.shell_snapshot=false` (з) | `bash` в `PATH` раньше `/usr/bin` (з) |
| Шлюз | Messages SSE, `ANTHROPIC_BASE_URL` (з) | Responses SSE, свой провайдер, `supports_websockets=false` (з) | `v1internal`, `CLOUD_CODE_URL` (з для адресации; основной ход аккаунта ?) |
| Инструкции | `CLAUDE.md`, rules, `AGENTS.md` → `messages[0]` (з) | `AGENTS.md` → `user`-сообщение (з) | `AGENTS.md`, `GEMINI.md`, `.agents/rules` → `systemInstruction`, каждый ход (з) |
| Лимиты в TUI | `anthropic-ratelimit-unified-*` (з) | `x-codex-primary-*`, `x-codex-secondary-*` (з) | `retrieveUserQuotaSummary` (д); живое значение (?) |
| Учётные данные | `claude setup-token`, год (д) | `auth.json`; перенос и ротация не проверены | `antigravity-oauth-token`; срок и копирование (?) |

Claude Code через OpenRouter: FUSE, оболочка, инструкции — как у Claude Code;
шлюз — другой upstream; лимитов подписки нет, только остаток ключа; учётные данные
— ключ OpenRouter (д).
