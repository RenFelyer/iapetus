#!/bin/sh
# Claude Code стенда: вместо входа в браузере кладёт в том состояния фиктивные учётные данные подписки и запускает
# настоящий Claude Code. Файл пишется один раз: что Claude Code записал в том потом, остаётся.
dir=/home/agent/.claude
if [ ! -s "$dir/.credentials.json" ]; then
    umask 077
    mkdir -p "$dir"
    printf '%s' '{"claudeAiOauth":{"accessToken":"sk-ant-oat01-e2e-fake-access","refreshToken":"sk-ant-ort01-e2e-fake-refresh","expiresAt":4102444800000,"scopes":["user:inference","user:profile"],"subscriptionType":"max"}}' > "$dir/.credentials.json"
fi
exec /usr/bin/claude "$@"
