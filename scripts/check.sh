#!/usr/bin/env bash
# Полный прогон проверок: формат, clippy, зависимости, тесты в изолированных контейнерах.
# Имя проекта compose берётся из каталога рабочей копии: параллельные копии не делят контейнеры и тома.
#   scripts/check.sh            — всё
#   scripts/check.sh --no-docker — без контейнерных тестов
set -euo pipefail

cd "$(dirname "$0")/.."
docker=1
[[ ${1:-} == --no-docker ]] && docker=0

cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo deny --locked check
((docker)) || exit 0

project="iapetus-test-$(basename "$PWD" | tr -c 'a-z0-9\n' '-')"
compose=(docker compose -p "$project" -f docker/docker-compose.test.yml)
trap '"${compose[@]}" --profile privileged down -v' EXIT
"${compose[@]}" up --build --exit-code-from check
"${compose[@]}" --profile privileged run --rm --build check-privileges
