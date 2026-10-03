# Авторизация агентов

> Решение: автоматическая выдача учётных данных отменена, агент входит вручную (`CONTRACT.md`, «Доступ и
> авторизация»). Ниже — разбор способов входа и хранения по агентам; части про хранение на server и передачу в контейнер
> не применяются.

Claude Code 2.1.285, Codex CLI 0.159.3, Antigravity CLI (agy) 1.2.14. Пометки: (з) — проверено запуском, (д) — документация или исходники, (?) — не проверено. Ротация refresh-токена на стендах — модель сервера, не настоящее поведение.

## Claude Code

| Способ | Хранение | Срок и обновление | Несколько копий | В контейнер |
|---|---|---|---|---|
| `/login` | `~/.claude/.credentials.json` `0600` (з) | refresh за 5 мин до истечения и после 401; `invalid_grant` обнуляет токены (з) | один файл — блокировка `.oauth_refresh.lock` (з); копии гасят друг друга (д, з) | копией нельзя |
| `setup-token` → `CLAUDE_CODE_OAUTH_TOKEN` | нигде | год (`expires_in` 31536000); refresh-токена нет (з) | гонок нет (д); много сессий (?) | да |
| `CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR` | fd | как `setup-token` | — | да; переменная удаляется из окружения (з); не документирована |
| `ANTHROPIC_API_KEY` / `apiKeyHelper` | окружение / команда | бессрочно; помощник — кэш 5 мин, повтор после 401 (д) | без ограничений | да (`…_API_KEY_FILE_DESCRIPTOR`, помощник читает файл на tmpfs) (з) |
| Bedrock, Vertex, Foundry | цепочки SDK (д) | — | — | (?) |

- Приоритет (д, з): облако, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`, `apiKeyHelper`, `CLAUDE_CODE_OAUTH_TOKEN`, `/login`.
- Подписка: `Bearer`, бета `oauth-2025-04-20`, лимиты 5 ч и неделя. API-ключ: `x-api-key`, оплата API, `rate_limits` в статусной строке нет.
- В окружении команд Bash учётных переменных нет (з).

## Codex CLI

| Способ | Хранение | Срок и обновление | Несколько копий | В контейнер |
|---|---|---|---|---|
| ChatGPT (PKCE или `--device-auth`) | `$CODEX_HOME/auth.json`, `0600` при создании, запись без rename (з) | refresh при остатке до `exp` < 5 мин, новый refresh-токен пишется в файл; `refresh_token_reused/expired/invalidated` окончательны (з) | нельзя: вторая копия получает `reused` (з, д); общий `CODEX_HOME` работает — файл перечитывается перед refresh (з) | да, файлом на tmpfs |
| `codex logout` | — | отзывает refresh-токен, гибнут все копии (з) | — | — |
| `codex login --with-api-key` | `auth.json` (з) | бессрочно | (?) | да |
| `CODEX_API_KEY` | окружение | — | — | TUI не читает (д) |
| `CODEX_ACCESS_TOKEN` | окружение | 7–90 дней | (?) | только Business/Enterprise (д) |
| `[model_providers.X.auth] command` | ничего (з) | команда раз в 300 с и после 401 (з) | — | да; refresh-токен у владельца команды |
| app-server `chatgptAuthTokens` | память | — | — | «FOR OPENAI INTERNAL USE ONLY» (д) |

- Подписка: `Bearer`, `ChatGPT-Account-ID`; лимиты 5 ч и неделя (д). Тело `/responses` сжато zstd.
- keyring без D-Bus не работает (з); `auto` ведёт себя как файл.
- Обычные режимы `Chatgpt`, `ChatgptAuthTokens`, `ApiKey` идут через `BearerAuthProvider`, без mTLS.
- [Документация](https://learn.chatgpt.com/docs/auth/ci-cd-auth.md): «Do not share the same file across concurrent jobs or multiple machines»; [auth](https://developers.openai.com/codex/auth.md) допускает перенос `auth.json` копированием, в том числе `docker cp`.

## Antigravity CLI

| Способ | Хранение | Срок и обновление | Несколько копий | В контейнер |
|---|---|---|---|---|
| Вход Google (consumer) | без D-Bus `~/.gemini/antigravity-cli/antigravity-oauth-token` `0600` (з) | access ~1 ч; refresh на `oauth2.googleapis.com/token`, файл переписывается на месте без блокировки (з) | клиент не мешает (з); при ротации у Google вторая копия получит `invalid_grant` (?) | да, файлом; только для чтения — refresh на каждом старте (з) |
| Повторный вход | — | новый refresh-токен каждый раз (`prompt=consent`) (з); у Google лимит 100 на аккаунт и client ID, лишние гасят старейший (д) | — | — |
| Gemini API key | `settings.json` + `GEMINI_API_KEY` (д, з) | бессрочно | без ограничений | окружением (?) |
| ADC (`AGY_ADC_AUTH=true`) | файл ADC, только чтение (з) | external_account — внешняя команда (з) | любое число (з) | да; Vertex, оплата Google Cloud (д) |

- Consumer: `Bearer`, `auth_method=consumer`; Free — неделя, Pro/Ultra — 5 ч и неделя (д). `invalid_grant`: файл не удаляется, выход 1 (з).
- Запросы `v1internal:*` идут на `CLOUD_CODE_URL`; OAuth, userinfo, Unleash и `/log` — на хосты Google.

## Пригодность для Iapetus

| Способ | Вывод |
|---|---|
| Claude `setup-token` | годится; риск хранения принят ([RISKS](RISKS.md)) |
| Claude копии `.credentials.json` | нет, гасят друг друга |
| Claude API-ключ (`apiKeyHelper`) | годится; лимитов подписки в TUI нет |
| Codex `auth.json` ChatGPT | с оговоркой: одна сессия на файл, server забирает обновлённый файл обратно |
| Codex API-ключ (`--with-api-key`) | годится; `CODEX_API_KEY` для TUI не подходит |
| Codex `chatgptAuthTokens`, `CODEX_ACCESS_TOKEN` | нет |
| agy consumer-токен | с оговоркой: риск принят, ротация у Google не проверена |
| agy ADC / Vertex, Gemini API key | с оговоркой: не подписка, другой API шлюза |

## Нужен настоящий аккаунт

- Claude: срок и отзыв `setup-token`, много сессий на одном токене, заголовки лимитов при `CLAUDE_CODE_OAUTH_TOKEN`.
- Codex: ротация refresh-токена и допуск на гонку, сроки, отзыв при `logout`, device code с личной подпиской.
- agy: ротация и параллельные копии, срок, RAPT для Workspace, отзыв, keyring с D-Bus.
