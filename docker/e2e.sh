#!/usr/bin/env bash
# Сквозной стенд: orbit server и контейнеры session во вложенном Docker (dind), runner и worker снаружи,
# вместо Anthropic — поддельный Messages API. Устройство — в docker-compose.e2e.yml.
#   docker/e2e.sh                 собрать образы, прогнать весь сценарий; dind и загруженные в него образы остаются
#   docker/e2e.sh clean           полная уборка: контейнеры, тома (в том числе образы dind) и сеть проекта
#   docker/e2e.sh files marker    только указанные шаги (ниже); нужные им предшествующие шаги идут сами
#   E2E_STEP=files docker/e2e.sh  то же через переменную; в шапке driver.py — шаги и их порядок
#   E2E_SKIP_BUILD=1 docker/e2e.sh не пересобирать образы (они должны быть собраны)
#   E2E_KEEP=1 docker/e2e.sh      не убирать runner, контейнеры и тома стенда после прогона: для разбора
#   E2E_PROJECT=имя docker/e2e.sh свой проект compose; по умолчанию — по каталогу рабочей копии, и параллельные
#                                 прогоны из разных рабочих копий не делят контейнеры, тома и образы
#   E2E_LOGS=каталог docker/e2e.sh журналы не в target/e2e-logs (для нескольких стендов одновременно)
#   E2E_CONTROL=каталог docker/e2e.sh каталог связи драйвера со стендом (target/e2e-control): заявки на остановку и
#                                 запуск runner и режим поддельного API; для нескольких стендов одновременно — свой
# Между прогонами остаются dind и его образы: каждый прогон начинается с чистого состояния server, session и
# runner. Журналы всех участников — в target/e2e-logs/, журналы сборки образов — там же (build-*.log).
# Код выхода — число неудавшихся шагов сценария; сборка, образ без нужного состава и сбой подготовки дают 1.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
project_dir=$(basename "$root" | tr -c 'a-z0-9\n' '-')
export E2E_PROJECT=${E2E_PROJECT:-iapetus-e2e-$project_dir}
logs=${E2E_LOGS:-$root/target/e2e-logs}
compose=(docker compose -p "$E2E_PROJECT" -f "$root/docker/docker-compose.e2e.yml")
profiles=(--profile runners --profile driver)
image_server=$E2E_PROJECT-server
image_session=$E2E_PROJECT-session
image_session_fake=$E2E_PROJECT-session-fake
image_runner=$E2E_PROJECT-runner
export E2E_RUNNER_IMAGE=$image_runner
# Каталог связи драйвера со стендом: заявки на остановку и запуск runner и режим поддельного API.
export E2E_CONTROL=${E2E_CONTROL:-$root/target/e2e-control}
dind() { "${compose[@]}" exec -T dind "$@"; }
phase_start=$SECONDS
phase() {
    echo "этап $1: $((SECONDS - phase_start)) с"
    phase_start=$SECONDS
}

if [ "${1:-}" = clean ]; then
    "${compose[@]}" "${profiles[@]}" down -v --remove-orphans
    exit 0
fi
export E2E_STEP=${E2E_STEP:-$*}

collect() {
    mkdir -p "$logs"
    "${compose[@]}" "${profiles[@]}" ps -a > "$logs/compose-ps.txt" 2>&1 || true
    for service in runner-a runner-b dind; do
        "${compose[@]}" logs --no-color "$service" > "$logs/$service.log" 2>&1 || true
    done
    dind docker ps -a > "$logs/inner-ps.txt" 2>&1 || true
    dind docker logs orbit-server > "$logs/orbit-server.log" 2>&1 || true
    dind docker logs fake-api > "$logs/fake-api.log" 2>&1 || true
    # Журналы контейнеров session, которые server сохранил перед удалением, лежат в каталоге состояния server.
    mkdir -p "$logs/saved-session-logs"
    dind docker cp orbit-server:/var/lib/orbit/session-logs - 2>/dev/null | tar -x -C "$logs/saved-session-logs" 2>/dev/null || true
    # Контейнер session исчезает вместе с сессией, поэтому его журнал снимает фоновый цикл `snapshot_session_logs`;
    # последний снимок — здесь, чтобы в журнал попало всё, что контейнер вывел после предыдущего.
    dind sh -c "mkdir -p /shared/session-logs; $snapshot_once" > /dev/null 2>&1 || true
    for log in $(dind sh -c 'ls /shared/session-logs/*.log' 2>/dev/null || true); do
        dind cat "$log" > "$logs/$(basename "$log")" 2>&1 || true
    done
    echo "журналы: $logs"
}

# Снимок журналов живых контейнеров session в общий том: после завершения сессии контейнера нет, а замена
# контейнера — новый контейнер под тем же именем, поэтому файл называется по идентификатору.
snapshot_once='for c in $(docker ps --format "{{.ID}}" --filter name=iapetus-session-); do
    docker logs "$c" > "/shared/session-logs/$c.new" 2>&1 && mv "/shared/session-logs/$c.new" "/shared/session-logs/$c.log"
done'

# Снимает журналы раз в секунду, пока не появится файл остановки.
snapshot_session_logs() {
    dind sh -c "rm -f /tmp/snapshot-stop; mkdir -p /shared/session-logs; while [ ! -e /tmp/snapshot-stop ]; do $snapshot_once; sleep 1; done"
}

# Чистое состояние для прогона: dind и образы остаются, всё остальное, что оставил прошлый прогон, убирается.
# Тома runner не нужны dind и убираются целиком; общий том `shared` смонтирован в dind и чистится изнутри.
reset_stand() {
    "${compose[@]}" "${profiles[@]}" kill runner-a runner-b driver > /dev/null 2>&1 || true
    "${compose[@]}" "${profiles[@]}" rm -f runner-a runner-b driver > /dev/null 2>&1 || true
    docker volume rm -f "${E2E_PROJECT}_workspace-a" "${E2E_PROJECT}_workspace-b" "${E2E_PROJECT}_home-a" > /dev/null
    dind sh -c 'docker ps -aq | xargs -r docker rm -f > /dev/null
        docker volume prune -af > /dev/null
        docker network rm iapetus-sessions iapetus-nodes > /dev/null 2>&1
        rm -rf /shared/* /shared/.[!.]*
        true'
}

# Драйвер не видит Docker. Он кладёт в каталог связи заявку `req-<N>` (`kill|start|state runner-a|runner-b`), а этот цикл
# её выполняет и пишет вывод и код возврата в `ack-<N>`.
serve_control() {
    local req id command service
    while true; do
        for req in "$E2E_CONTROL"/req-*; do
            [ -e "$req" ] || continue
            id=${req##*/req-}
            read -r command service < "$req"
            rm -f "$req"
            case "$service" in
                runner-a | runner-b) ;;
                *) echo "заявка $command $service отклонена" > "$E2E_CONTROL/ack-$id.tmp"; mv "$E2E_CONTROL/ack-$id.tmp" "$E2E_CONTROL/ack-$id"; continue ;;
            esac
            case "$command" in
                kill) "${compose[@]}" "${profiles[@]}" kill "$service" > "$E2E_CONTROL/ack-$id.tmp" 2>&1 ;;
                start) "${compose[@]}" "${profiles[@]}" start "$service" > "$E2E_CONTROL/ack-$id.tmp" 2>&1 ;;
                state) docker inspect --format '{{.State.Status}}' "$("${compose[@]}" "${profiles[@]}" ps -aq "$service")" > "$E2E_CONTROL/ack-$id.tmp" 2>&1 ;;
                *) echo "заявка $command $service отклонена" > "$E2E_CONTROL/ack-$id.tmp" ;;
            esac
            mv "$E2E_CONTROL/ack-$id.tmp" "$E2E_CONTROL/ack-$id"
        done
        sleep 0.1
    done
}

control_pid=
snapshot_pid=
cleanup() {
    if [ -n "$control_pid" ]; then
        kill "$control_pid" 2>/dev/null || true
        wait "$control_pid" 2>/dev/null || true
    fi
    if [ -n "$snapshot_pid" ]; then
        dind touch /tmp/snapshot-stop || true
        wait "$snapshot_pid" 2>/dev/null || true
    fi
    collect
    if [ -z "${E2E_KEEP:-}" ]; then
        reset_stand || true
    fi
    phase "уборка"
}

# Сборка образа в фоне: вывод — в свой журнал, код выхода забирает `wait` по PID.
build_pids=()
build_names=()
build_in_background() {
    local name=$1
    shift
    "$@" > "$logs/build-$name.log" 2>&1 &
    build_pids+=("$!")
    build_names+=("$name")
}

wait_for_builds() {
    local failed=0 index
    for index in "${!build_pids[@]}"; do
        if ! wait "${build_pids[$index]}"; then
            echo "сборка ${build_names[$index]} не удалась, конец журнала $logs/build-${build_names[$index]}.log:" >&2
            tail -n 30 "$logs/build-${build_names[$index]}.log" >&2
            failed=1
        fi
    done
    build_pids=()
    build_names=()
    return "$failed"
}

# server не передаёт сессии адрес API (`IAPETUS_UPSTREAM`): на стенде вместо api.anthropic.com поддельный API,
# адрес которого кладётся в образ. Входа в агента нет: оболочка `claude` кладёт в том состояния фиктивные
# учётные данные подписки, как после входа пользователя.
build_session_fake() {
    docker build -t "$image_session_fake" -f - "$root/docker/e2e" <<DOCKERFILE
FROM $image_session
ENV IAPETUS_UPSTREAM=http://fake-api:8080
COPY --chmod=755 claude-login.sh /usr/local/bin/claude
DOCKERFILE
}

build_session() {
    docker build -f "$root/orbit/Dockerfile" --target session -t "$image_session" "$root"
    build_session_fake
}

build_runner_and_driver() {
    docker build -f "$root/waldo/Dockerfile" -t "$image_runner" "$root"
    "${compose[@]}" --profile driver build driver
}

# Агентам нужен не `python3`, не `git` и не `perl`: с ними `run_workflow` agy исполнялся бы в контейнере, а подагенты agy
# писали бы ветки и worktree в репозиторий runner, а `perl` — готовый сетевой клиент без `curl` и `nc`.
check_session_image() {
    local found
    found=$(docker run --rm --entrypoint sh "$image_session_fake" -c 'command -v python3; command -v git; command -v perl; true')
    if [ -n "$found" ]; then
        echo "в образе session есть лишнее: $found" >&2
        return 1
    fi
    echo "образ session без python3, git и perl"
}

# Каталог состояния каждого агента создан в образе и принадлежит agent: иначе именованный том, смонтированный
# в него, получил бы владельца root, и агент не смог бы писать в своё состояние.
check_session_state_dirs() {
    local dir owner
    for dir in .claude .codex .gemini; do
        owner=$(docker run --rm --entrypoint stat "$image_session_fake" -c %U "/home/agent/$dir" 2>&1 || true)
        if [ "$owner" != agent ]; then
            echo "каталог /home/agent/$dir в образе session: владелец '$owner', нужен agent" >&2
            return 1
        fi
    done
    echo "каталоги состояния агентов принадлежат agent"
}

# Из сети сессий (проба идёт из fake-api) порт 2200 закрыт на всех путях: по имени server, по адресу в сети узлов и
# через шлюз сети сессий, где docker публикует порт; порты 2201 и 2202, которые нужны session, открыты.
check_node_port_closed_to_sessions() {
    local probe='import socket, sys
host, port = sys.argv[1], int(sys.argv[2])
try:
    socket.create_connection((host, port), timeout=3).close()
except OSError:
    sys.exit(1)'
    local target
    for _ in $(seq 50); do
        dind docker exec fake-api python3 -c "$probe" "$sessions_ip" 2201 && break
        sleep 0.2
    done
    for target in "$sessions_ip 2201" "$sessions_ip 2202"; do
        # shellcheck disable=SC2086
        dind docker exec fake-api python3 -c "$probe" $target || { echo "session не достаёт до ${target// /:}, а должна" >&2; return 1; }
    done
    for target in "orbit-server 2200" "$nodes_ip 2200" "$sessions_ip 2200" "172.31.0.1 2200"; do
        # shellcheck disable=SC2086
        if dind docker exec fake-api python3 -c "$probe" $target; then
            echo "из сети сессий открыт порт ${target// /:}" >&2
            return 1
        fi
    done
    echo "порт 2200 закрыт для сети сессий, 2201 и 2202 открыты"
}

# Образ грузится в dind, только если там нет образа с тем же идентификатором.
load_into_dind() {
    local image=$1 local_id inner_id
    local_id=$(docker image inspect --format '{{.Id}}' "$image")
    inner_id=$(dind docker image inspect --format '{{.Id}}' "$image" 2>/dev/null || true)
    if [ "$local_id" = "$inner_id" ]; then
        return 0
    fi
    docker save "$image" | dind docker load > /dev/null
}

rm -rf "$logs"
mkdir -p "$logs" "$E2E_CONTROL"
# Каталог связи не пересоздаётся: dind остаётся между прогонами, и его монтирование держит прежний каталог.
find "$E2E_CONTROL" -mindepth 1 -delete
chmod 777 "$E2E_CONTROL"
trap cleanup EXIT

"${compose[@]}" up -d --wait dind > "$logs/dind-up.log" 2>&1 &
dind_pid=$!

if [ -z "${E2E_SKIP_BUILD:-}" ]; then
    build_in_background server docker build -f "$root/orbit/Dockerfile" --target server -t "$image_server" "$root"
    build_in_background session build_session
    build_in_background runner build_runner_and_driver
    wait_for_builds || exit 1
    phase "сборка образов"
fi

wait "$dind_pid" || { echo "dind не поднялся:" >&2; cat "$logs/dind-up.log" >&2; exit 1; }
check_session_image
check_session_state_dirs
phase "ожидание dind и проверка образа session"

reset_stand
phase "сброс состояния стенда"
for image in "$image_server" "$image_session_fake" python:3.13-slim-bookworm; do
    load_into_dind "$image" &
    build_pids+=("$!")
    build_names+=("load-$image")
done
for index in "${!build_pids[@]}"; do
    wait "${build_pids[$index]}" || { echo "загрузка в dind не удалась: ${build_names[$index]}" >&2; exit 1; }
done

phase "загрузка образов в dind"

# Шаблон принимает образ только по digest. Во вложенном Docker образ без реестра, и его digest — идентификатор образа.
image_id=$(dind docker image inspect --format '{{.Id}}' "$image_session_fake")
session_image="$image_session_fake@$image_id"
echo "образ session: $session_image"

snapshot_session_logs &
snapshot_pid=$!
# Две сети с фиксированными адресами: узлам (runner, worker) server виден в iapetus-nodes, session — в
# iapetus-sessions. Внешний слушатель занимает только адрес сети узлов, внутренний и MCP — только адрес сети
# сессий; порт 2200 опубликован только на адресе dind в сети стенда (как на хосте — на адресе сети узлов, а не на
# всех интерфейсах: иначе его достал бы шлюз сети сессий). Контейнер session из своей сети не достаёт до порта 2200.
publish_ip=$(dind hostname -i | cut -d" " -f1)
nodes_ip=172.30.0.10
sessions_ip=172.31.0.10
dind docker network create --subnet 172.30.0.0/24 iapetus-nodes > /dev/null
dind docker network create --subnet 172.31.0.0/24 iapetus-sessions > /dev/null
sock_gid=$(dind stat -c %g /var/run/docker.sock)
dind docker run -d --name fake-api --network iapetus-sessions -v /e2e:/e2e:ro -v /control:/control:ro \
    python:3.13-slim-bookworm python3 -u /e2e/fake_api.py 8080 > /dev/null
# Вторая сеть подключается до старта: слушатели привязаны к адресам обеих сетей.
dind docker create --name orbit-server --network iapetus-nodes --ip "$nodes_ip" -p "$publish_ip:2200:2200" \
    --user 10001 --group-add "$sock_gid" \
    -v /var/run/docker.sock:/var/run/docker.sock -v orbit-data:/var/lib/orbit \
    -e IAPETUS_LISTEN="$nodes_ip:2200" \
    -e IAPETUS_INTERNAL_LISTEN="$sessions_ip:2201" -e IAPETUS_MCP_LISTEN="$sessions_ip:2202" \
    -e IAPETUS_SESSION_IMAGE="$session_image" \
    -e IAPETUS_SESSION_ORBIT_URL=orbit-server:2201 \
    -e IAPETUS_SESSION_MCP_URL=http://orbit-server:2202/mcp \
    -e IAPETUS_SESSION_NETWORK=iapetus-sessions -e RUST_LOG=debug \
    "$image_server" > /dev/null
dind docker network connect --ip "$sessions_ip" iapetus-sessions orbit-server
dind docker start orbit-server > /dev/null
check_node_port_closed_to_sessions

# Первый orbit_key и открытый ключ CA — узлам: файл orbit_key server удаляет после первого входа.
for _ in $(seq 300); do
    if dind docker exec orbit-server test -s /var/lib/orbit/orbit_key && dind docker exec orbit-server test -s /var/lib/orbit/ssh-ca.pub; then
        break
    fi
    sleep 0.2
done
dind sh -c 'docker exec orbit-server cat /var/lib/orbit/orbit_key > /shared/orbit_key && docker exec orbit-server cat /var/lib/orbit/ssh-ca.pub > /shared/ssh-ca.pub && chmod 644 /shared/orbit_key /shared/ssh-ca.pub'

phase "запуск server и fake-api"
"${compose[@]}" --profile runners up -d runner-a runner-b
phase "запуск runner"
status=0
serve_control &
control_pid=$!
"${compose[@]}" --profile driver run --rm driver || status=$?
phase "сценарий"
# Завершённая сессия оставляет журнал своего контейнера у server: после удаления контейнера других следов нет.
if [ -z "$E2E_STEP" ] || [[ " ${E2E_STEP//,/ } " == *" end "* ]]; then
    if dind docker exec orbit-server sh -c 'grep -l "orbit session запущен" /var/lib/orbit/session-logs/*.log' > /dev/null 2>&1; then
        echo "журнал завершённой сессии сохранён server"
    else
        echo "журнал завершённой сессии не сохранён в session-logs server" >&2
        status=$((status + 1))
    fi
fi
echo "результат сценария: неудачных шагов $status"
exit "$status"
