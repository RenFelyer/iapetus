# syntax=docker/dockerfile:1
#
# Образы orbit. Контекст сборки — корень репозитория:
#   docker build -f orbit/Dockerfile --target server .
#   docker build -f orbit/Dockerfile --target session .

# cargo-chef точной версии: рецепт сборки зависимостей из Cargo.toml и Cargo.lock без исходников. Сборка падает,
# если sha256 архива не совпал с записанным: хеши взяты из релиза v${CARGO_CHEF_VERSION} на GitHub. Этапы
# cargo-chef, chef и planner одинаковы в orbit/Dockerfile, waldo/Dockerfile и
# образе check в docker/docker-compose.test.yml: одинаковые инструкции дают общие слои кэша на хосте.
FROM debian:trixie-slim AS cargo-chef
ARG CARGO_CHEF_VERSION=0.1.78
ARG CARGO_CHEF_SHA256_AMD64=aca691abfbfbbe00d482e0ed2249eec3091b65e96a0ce92947fb5b254d48b16d
ARG CARGO_CHEF_SHA256_ARM64=cd59b90fce5fa84c4189648fec33404ec5491791eebdb812df5e7fca8336408d
ARG TARGETARCH
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl xz-utils \
    && rm -rf /var/lib/apt/lists/*
RUN case "$TARGETARCH" in \
        amd64) triple=x86_64; sha256=$CARGO_CHEF_SHA256_AMD64 ;; \
        arm64) triple=aarch64; sha256=$CARGO_CHEF_SHA256_ARM64 ;; \
        *) echo "архитектура $TARGETARCH не поддержана" >&2; exit 1 ;; \
    esac \
    && curl -fsSL -o /cargo-chef.tar.xz \
        "https://github.com/LukeMathWalker/cargo-chef/releases/download/v${CARGO_CHEF_VERSION}/cargo-chef-${triple}-unknown-linux-musl.tar.xz" \
    && echo "$sha256  /cargo-chef.tar.xz" | sha256sum -c - \
    && tar -xJf /cargo-chef.tar.xz -C / --strip-components=1 "cargo-chef-${triple}-unknown-linux-musl/cargo-chef"

FROM rust:1.98-slim-trixie AS chef
COPY --from=cargo-chef /cargo-chef /usr/local/cargo/bin/cargo-chef
WORKDIR /src

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path /recipe.json

# Зависимости — отдельный слой с ключом по рецепту: правка исходников их не пересобирает, а тот же рецепт из
# любой рабочей копии на хосте берёт готовый слой. target и реестр не в cache mount: общий target отдал бы
# cargo бинарник другой рабочей копии, а слои неизменяемы, и параллельные сборки их не портят.
FROM chef AS build
COPY --from=planner /recipe.json /recipe.json
# Зависимости всего workspace одним набором features: сборка отдельного пакета выбирает другие features и
# компилирует общие зависимости заново.
RUN cargo chef cook --release --locked --recipe-path /recipe.json
# Копируются манифесты и src своей программы и nerve: правка tests/ и чужой программы образ не пересобирает.
# Исходники чужого крейта остаются заглушками от cook, их сборка мгновенна.
COPY Cargo.toml Cargo.lock ./
COPY nerve/Cargo.toml nerve/
COPY orbit/Cargo.toml orbit/
COPY nerve/src nerve/src
COPY orbit/src orbit/src
RUN cargo build --release --locked --workspace \
    && install -m 0755 target/release/orbit /usr/local/bin/orbit

FROM debian:trixie-slim AS server
# Server читает релизы агентов по HTTPS и проверяет сертификаты по системному хранилищу.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
# Домашний каталог принадлежит orbit: именованный том, смонтированный
# сюда в первый раз, наследует владельца.
RUN useradd --system --uid 10001 --home-dir /var/lib/orbit --create-home \
        --shell /usr/sbin/nologin orbit
COPY --from=build /usr/local/bin/orbit /usr/local/bin/orbit
ENV IAPETUS_DATA_DIR=/var/lib/orbit
USER orbit
ENTRYPOINT ["orbit", "server"]

# Ключ подписи репозитория apt Claude Code. Сборка падает, если файл содержит
# не один ключ или отпечаток первичного ключа отличается от ожидаемого.
FROM debian:trixie-slim AS claude-code-key
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl gnupg \
    && rm -rf /var/lib/apt/lists/*
RUN curl -fsSL https://downloads.claude.ai/keys/claude-code.asc -o /claude-code.asc \
    && test "$(gpg --show-keys --with-colons /claude-code.asc | grep -c '^pub:')" = 1 \
    && test "$(gpg --show-keys --with-colons /claude-code.asc | awk -F: '$1 == "fpr" { print $10; exit }')" \
        = 31DDDE24DDFAB679F42D7BD2BAA929FF1A7ECACE

# Пакет Codex CLI (codex, codex-code-mode-host, rg, bwrap) точной версии. Сборка падает, если sha256 архива
# не совпал с записанным: хеши взяты из релиза rust-v${CODEX_VERSION} на GitHub.
FROM debian:trixie-slim AS codex-package
ARG CODEX_VERSION=0.159.3
ARG CODEX_SHA256_AMD64=3930f31ac5fca861ea3e444e2683f261190d96b63fba58e0a40a879174369cdf
ARG CODEX_SHA256_ARM64=20b7d673cf2b64b6c6208fa4fda06feb9dac0572da9987294b48cde1dab9d001
ARG TARGETARCH
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
RUN case "$TARGETARCH" in \
        amd64) triple=x86_64; sha256=$CODEX_SHA256_AMD64 ;; \
        arm64) triple=aarch64; sha256=$CODEX_SHA256_ARM64 ;; \
        *) echo "архитектура $TARGETARCH не поддержана" >&2; exit 1 ;; \
    esac \
    && curl -fsSL -o /codex.tar.gz \
        "https://github.com/openai/codex/releases/download/rust-v${CODEX_VERSION}/codex-package-${triple}-unknown-linux-musl.tar.gz" \
    && echo "$sha256  /codex.tar.gz" | sha256sum -c - \
    && mkdir /opt/codex-package \
    && tar -xzf /codex.tar.gz -C /opt/codex-package \
    && rm -rf /opt/codex-package/codex-resources/voice

# Antigravity CLI (agy) точной версии. Сборка падает, если sha512 архива не совпал с записанным: хеши — из манифеста
# обновлений agy (`…/manifests/linux_<arch>.json`), откуда их берёт и официальный установщик.
FROM debian:trixie-slim AS agy-binary
ARG AGY_VERSION=1.2.14
ARG AGY_BUILD=4571742832820224
ARG AGY_SHA512_AMD64=fd771dfc74ddd07b61c8b0a6fd7a238f53a3a098a51052583a01d97ab84ee60db741ce5f041e87a9da3c1a9aabe95b053113374437df1e231773552781edaf09
ARG AGY_SHA512_ARM64=d96a67d6952a8ec1da16c8b6515f1393ed0260658cf104d9b57b8d13c92c591322ed5e08a62894842be81888116da0f5f80def7971c913d1f462300103426954
ARG TARGETARCH
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
RUN case "$TARGETARCH" in \
        amd64) platform=linux-x64/cli_linux_x64; sha512=$AGY_SHA512_AMD64 ;; \
        arm64) platform=linux-arm/cli_linux_arm64; sha512=$AGY_SHA512_ARM64 ;; \
        *) echo "архитектура $TARGETARCH не поддержана" >&2; exit 1 ;; \
    esac \
    && curl -fsSL -o /agy.tar.gz \
        "https://storage.googleapis.com/antigravity-public/antigravity-cli/${AGY_VERSION}-${AGY_BUILD}/${platform}.tar.gz" \
    && echo "$sha512  /agy.tar.gz" | sha512sum -c - \
    && mkdir /agy \
    && tar -xzf /agy.tar.gz -C /agy antigravity \
    && mv /agy/antigravity /agy/agy

FROM debian:trixie-slim AS session
ARG CLAUDE_CODE_VERSION=2.1.285
# ca-certificates нужен, чтобы прочитать https-репозиторий Claude Code; procps —
# Codex CLI: его TUI запускает демон app-server и проверяет процесс через `ps`.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates procps \
    && rm -rf /var/lib/apt/lists/*
COPY --from=claude-code-key /claude-code.asc /etc/apt/keyrings/claude-code.asc
RUN echo "deb [signed-by=/etc/apt/keyrings/claude-code.asc] https://downloads.claude.ai/claude-code/apt/stable stable main" \
        > /etc/apt/sources.list.d/claude-code.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends "claude-code=${CLAUDE_CODE_VERSION}-1" \
    && rm -rf /var/lib/apt/lists/*
# Codex CLI и Antigravity CLI лежат в каталогах root: агент не может их заменить, а agy
# пропускает самообновление, потому что каталог для него только для чтения.
COPY --from=codex-package /opt/codex-package /opt/codex-package
COPY --from=agy-binary --chmod=0755 /agy/agy /opt/agy/agy
RUN ln -s /opt/codex-package/bin/codex /usr/local/bin/codex \
    && ln -s /opt/agy/agy /usr/local/bin/agy
COPY --from=build /usr/local/bin/orbit /usr/local/bin/orbit
# /opt/iapetus/bash — оболочка агента: orbit под именем bash принимает команды и
# отдаёт их агенту. Агент получает её либо через свою переменную, либо как
# оболочку пользователя из /etc/passwd, либо как `bash` из PATH: каталог
# /opt/iapetus/path стоит в PATH раньше /usr/bin.
RUN install -d /opt/iapetus /opt/iapetus/path \
    && ln -s /usr/local/bin/orbit /opt/iapetus/bash \
    && ln -s /usr/local/bin/orbit /opt/iapetus/path/bash
# Пользователь агента и каталоги состояния всех поддержанных агентов: том, смонтированный в каталог состояния
# в первый раз, наследует владельца, и без этого том агента получил бы root.
RUN useradd --uid 1000 --create-home --home-dir /home/agent --shell /opt/iapetus/bash agent \
    && install -d -o agent -g agent /home/agent/.claude /home/agent/.codex /home/agent/.gemini
# Каталог сокета оболочки принадлежит agent: сессия без CAP_CHOWN создаёт сокет от его имени (euid потока).
RUN install -d -o agent -g agent -m 0755 /run/iapetus
# FUSE монтирует сам session через mount(2) крейтом fuser: fusermount3, пакет fuse3 и /etc/fuse.conf не нужны.
# Контейнеру нужны `--device /dev/fuse`, `--cap-drop ALL` с четырьмя `--cap-add` (набор — в шаблоне orbit server)
# и `--security-opt no-new-privileges`; на хосте с AppArmor — профиль `docker/apparmor/iapetus-session`.
# /work — кэш содержимого: в работе на него монтируется tmpfs, без монтирования каталог остаётся в слое контейнера.
RUN install -d -m 0700 /work
# `perl` — готовый сетевой клиент без `curl` и `nc`, а Claude Code и скрипты образа его не используют. Пакет
# perl-base обязателен (Essential) и нужен dpkg и debconf только при установке пакетов, поэтому бинарник удаляется
# последним шагом, после всех `apt-get install`; в запущенном контейнере пакеты не ставятся.
RUN rm -f /usr/bin/perl /usr/bin/perl5*
# Процесс стартует от root: session монтирует FUSE, запускает агента от
# пользователя agent без CAP_SYS_ADMIN и размонтирует FUSE при остановке.
# Агента обновляет только session по указанию server.
ENTRYPOINT ["orbit", "session"]
