# Риски условий использования

> Решение: автоматическая выдача учётных данных отменена, агент входит вручную (`CONTRACT.md`, «Доступ и
> авторизация»). Ниже — разбор способов входа и хранения по агентам; части про хранение на server и передачу в контейнер
> не применяются.

Проверено 2026-10-01: Claude Code 2.1.286, Antigravity CLI (`agy`) 1.2.14, Codex CLI 0.159.3 (CLI в Docker без сети, с фиктивными токенами и локальными заглушками вместо провайдеров). Проверка показывает, что клиент передаёт и что говорят условия; серверные правила обнаружения и вероятность блокировки неизвестны.

## Договорные риски

| Сервис | Риск | Граница | Снижение |
|---|---|---|---|
| Claude Code | [Условия](https://code.claude.com/docs/en/legal-and-compliance): разработчики не вправе «collect, store, or intermediate Claude.ai credentials or session tokens»; server учётные данные Claude не хранит и не передаёт: пользователь входит сам внутри контейнера | Тот же текст разрешает пользователю входить своей подпиской в немодифицированный Claude Code, в том числе на размещённой платформе; для частной установки, где Iapetus и подписка у одного лица, не разъяснено. Чужая подписка (например, брата) и передача учётных данных запрещены; код этому не мешает | Вход пользователя внутри контейнера (так и сделано) или API key Claude Console |
| Antigravity | [Условия](https://antigravity.google/terms), п. 6: доступ к сервису сторонним ПО — нарушение, возможна приостановка или закрытие аккаунта; Iapetus ставит между `agy` и Google свой шлюз и выполняет действия через FUSE и runner | Сочетание «оригинальный CLI + пассивный шлюз + своя машина» условия не разбирают: ни допустимость, ни неизбежность блокировки не установлены. Не действует при доступе через Gemini Enterprise / Google Cloud | Gemini Enterprise или Google Cloud по договору организации. Обычный API key разрешения consumer-сценария не даёт (Gemini API — «not for consumer use») |
| Codex CLI | Запрета не найдено: OpenAI документирует вход через ChatGPT, API key и app-server с OAuth access token | Рекомендация API key и интеграционный путь не разрешают любой способ хранения и переноса подписочных токенов | Не полагаться на поведение токенов: перенос, параллельное использование и ротация `auth.json` на настоящем аккаунте не проверены |

Решение пользователя: вход в агентов вручную внутри контейнера session, server учётные данные не хранит; Antigravity входит аккаунтом пользователя, один аккаунт ведёт несколько сессий (каждая session входит отдельно), пассивный шлюз остаётся в сетевом пути. Риски Claude и Antigravity приняты до разъяснения провайдеров; риск хранения токенов Claude на server снят сменой авторизации; чужие подписки пользователь вводит на свой риск. Релиз — частная установка: подписка и аккаунты принадлежат пользователю, риски условий провайдеров на нём (в `README.md`).

## Что провайдер видит

Видимость настроек не доказывает наказания: данных, что эти признаки нарушают условия или служат обнаружением Iapetus, нет. То же для параллельных сессий, одного IP, круглосуточной работы и `bypassPermissions`.

| Клиент | Передаёт |
|---|---|
| Claude Code 2.1.286 | В телеметрии `/api/event_logging/v2/batch`: `policy_ineligible_reason: "custom_base_url"` (сам URL не найден), `deployment_environment: "docker"`, ядро, дистрибутив, `entrypoint: "sdk-cli"`, `is_interactive: false`, device/session ID. В `/v1/messages?beta=true`: Bearer, `metadata.user_id`, `x-app: cli`, атрибуция в system-блоке |
| Codex 0.159.3 | В `/responses` (тело в zstd): Bearer, `ChatGPT-Account-ID`, installation/thread/turn ID, `sandbox_mode`; в `/analytics-events/events`: `approval_policy`, `sandbox_policy`, `sandbox_network_access`, версия CLI |
| agy 1.2.14 | В `/v1internal:streamGenerateContent`: Bearer, `User-Agent` с версией, ОС и `auth_method=consumer`, `userAgent: "antigravity"`; в Unleash: имя пользователя и hostname; бинарная телеметрия на `play.googleapis.com/log` не декодирована |

- Claude: в запросе на штатный `api.anthropic.com` есть `cch` (заполняется клиентом: `cch=00000` в коде, значение в сети), `cc_prompt_id`, `x-claude-code-prompt-id`, `diagnostics.previous_message_id`; при custom `ANTHROPIC_BASE_URL` их нет. Алгоритм `cch` и серверная проверка не установлены: нельзя считать ни подписью, ни безобидным полем.
- Подпись всего HTTP-запроса клиентским сертификатом не найдена ни у одного CLI (Claude, agy — по запросам; Codex — `with_no_client_auth()`). Режим Codex `AgentIdentity` подписывает только `agent_runtime_id:task_id:timestamp` и к обычному входу не относится.
- Свой CA принимают Claude (`NODE_EXTRA_CA_CERTS`) и Codex: трафик читается в управляемой среде; принятие изменённого запроса настоящим upstream не проверено.

## TLS ClientHello

| Клиент | Cipher suites | ALPN |
|---|---:|---|
| Claude Code, штатный адрес | 17 | `http/1.1` |
| Codex | 10 | `h2`, `http/1.1` |
| agy, путь Gemini API key | 13 | `h2`, `http/1.1` |
| reqwest-клиент шлюза orbit | 10 | `http/1.1` |

У Claude и agy шифры отличаются от шлюза; у Codex шифры совпали, ALPN отличается. Порядок расширений не считается (rustls меняет его). Применение ClientHello для обнаружения неизвестно; для agy consumer-ClientHello не снят.

## Открытые вопросы

- agy: основной consumer-запрос и каталог моделей (поддельный `fetchAvailableModels` даёт `plan model not specified`).
- Claude: алгоритм и проверка `cch`; приём изменённых thinking-блоков и истории.
- Реальная ротация, отзыв и перенос OAuth-токенов.
- Codex: WebSocket `/responses` (CLI откатывается на HTTP/SSE; шлюз фильтрует `upgrade`, туннелирования нет); полный цикл через orbit gateway.
- Полнота журналов как замены шлюзу; влияние server flags, managed policies и тарифов; серверные причины санкций.
