# `claude -p`: опции и тонкости

> Справочно: свой TUI поверх headless-режима отменён, родной интерфейс агента работает в pty session (`CONTRACT.md`, «Вход через TUI worker»). Документ описывает неинтерактивный режим как справку.

Сверено с Claude Code 2.1.285 и `code.claude.com/docs/en/cli-reference.md`, `headless.md`. Основа второго этапа (свой TUI, headless).

## Поведение

- Промпт — аргументом, через stdin или обоими (склеиваются). Без обоих — ошибка, код 1. Stdin до 10 МБ.
- Код 0 — успех. Ошибка флагов — в stderr до старта; ошибка внутри прогона (нет авторизации) — результатом в stdout.
- Диалога доверия к каталогу нет: без `--bare` выполняются хуки и `.mcp.json` проекта без approve.
- Невалидные файлы настроек игнорируются молча (`claude doctor`).
- Битая запись в `--mcp-config` пропускается, код 0; ловить по непустому `mcp_server_errors` в `system/init`.
- Флаги со списком съедают промпт: `--allowedTools`, `--disallowedTools`, `--tools`, `--add-dir`, `--mcp-config`, `--betas`, `--file`, `--plugin-url`. Промпт ставить сразу после `-p`; со stdin промпт молча теряется.

## Ввод и вывод

| Флаг | Действие | Тонкость |
|---|---|---|
| `--output-format text\|json\|stream-json` | текст / один объект / NDJSON | `stream-json` требует `--verbose` |
| `--input-format stream-json` | сообщения по stdin, многоходовой диалог одним процессом | |
| `--include-partial-messages` | `stream_event` / `text_delta` по токенам | `-p` + `stream-json` |
| `--include-hook-events` | события хуков | `Notification`, `SessionEnd`, `PreCompact`, `PostCompact` не дают `hook_started` |
| `--forward-subagent-text` | текст и thinking сабагентов с `parent_tool_use_id` | без него только `tool_use`/`tool_result` |
| `--replay-user-messages` | эхо user-сообщений | `stream-json` на входе и выходе |
| `--json-schema` | ответ в `.structured_output` | `format` не проверяется |

- `json`: `result`, `session_id`, `subtype` (`success`, `error_max_turns`, …), `is_error`, `num_turns`, `total_cost_usd` (клиентская оценка, при `--resume` за весь разговор), `modelUsage`, `usage`, `permission_denials`, `stop_reason`, `terminal_reason`, `duration_ms`.
- `stream-json`: первое событие `system/init` (модель, инструменты, `mcp_servers`, `mcp_server_errors`, `plugins`, `capabilities`), последнее `result`; между ними `system/api_retry` и `permission_denied`.
- Медленному читателю `stream-json` процесс ждёт до 30 с.

## Лимиты и модель

- `--max-turns N`: при превышении `error_max_turns`, `result: null`, код 1, частичного ответа нет.
- `--max-budget-usd X`: сабагенты считаются, затраты до `--resume` нет; по исчерпании фоновые сабагенты останавливаются.
- `--model` перекрывает `ANTHROPIC_MODEL`; `--fallback-model a,b` при перегрузке; `--effort`; `--betas` только с API-ключом.

## Разрешения

Спросить некого: в режиме `default` всё, что спросило бы, отклоняется и попадает в `permission_denials`.

| Флаг | Действие |
|---|---|
| `--allowedTools "Bash(git diff *)"` | allow; пробел перед `*` важен (`git diff*` совпадёт с `git diff-index`) |
| `--disallowedTools` | голое имя (`Edit`, `mcp__*`) убирает инструмент из контекста; шаблон (`Bash(rm *)`) режет только совпавшие вызовы |
| `--tools "Bash,Read"` / `""` | какие встроенные инструменты существуют; на MCP не влияет |
| `--permission-mode` | `default`, `acceptEdits` (правки файлов и `mkdir`/`touch`/`mv`/`cp`), `plan`, `auto`, `dontAsk`, `bypassPermissions` |
| `--permission-prompt-tool mcp__srv__tool` | MCP-инструмент отвечает на запросы; ждёт подключения до `MCP_TIMEOUT` (30 с) |
| `--permission-prompts host\|none` | `none` — отказ без ожидания, убирает `AskUserQuestion`; при SDK-хосте или `--permission-prompt-tool` иначе прогон висит |

## Контекст и конфигурация

- `--bare` не грузит хуки, скиллы, плагины, MCP, автопамять, CLAUDE.md, LSP; авторизация только `ANTHROPIC_API_KEY` или `apiKeyHelper` через `--settings`, подписка не читается. Нужное — явно: `--append-system-prompt[-file]`, `--settings`, `--mcp-config`, `--agents`, `--plugin-dir`.
- Без `--bare` грузится всё, как в интерактиве, включая `~/.claude/CLAUDE.md`.
- `--settings <file|json>` поверх файлов, до 2 МиБ; `--setting-sources`; `--strict-mcp-config` — только заданные серверы; `--agents`, `--agent`; `--safe-mode` — без кастомизаций.
- `--system-prompt[-file]` заменяет только основной промпт `system[2]`; `system[0]`, `system[1]`, env-сообщение, CLAUDE.md и tools остаются. `--append-system-prompt[-file]` дописывает в конец.
- Промпт фиксируется на первом запросе (`--system-prompt-snapshot on`): при `--continue` новый `--append-system-prompt` действует только после компакта; при подборе — `off`.
- `__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__` отделяет кэшируемую часть заменяющего промпта; `--exclude-dynamic-system-prompt-sections` выносит cwd, git status, пути памяти в первое сообщение (общий кэш между машинами).

## Сессии

- `-c` с `-p` продолжает и `-p`/SDK-сессии; интерактивный `-c` их пропускает.
- `-r <id|имя|путь .jsonl>` ищет по всем проектам машины; `--session-id <uuid>` — свой ID; `--fork-session`; `-n` — имя; `--no-session-persistence` — без записи, продолжить нельзя.
- Продолжение: `sid=$(claude -p … --output-format json | jq -r .session_id)`, затем `claude -p … -r "$sid"`.

## Процесс

- Фоновый Bash убивается через ~5 с после результата.
- Фоновые сабагенты и workflow держат процесс; через 10 минут простоя их останавливают, результат теряется (`CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS`, `0` снимает).
- SIGTERM: код 143, ход бросается без результата, `SessionEnd`-хуки выполняются; чтобы закрыть ход, сначала SIGINT.
- Недоступно с `-p`: `--bg`, `--cloud "описание"`, `/login`. В промпте работают `/skill-name`, `/model`, `/effort`, `/config`, `/output-style`, `/mcp`.
- zsh не разбивает переменные: флаги передавать массивом.
