# syntax=docker/dockerfile:1
#
# Образ waldo runner. Контекст сборки — корень репозитория:
#   docker build -f waldo/Dockerfile .

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
COPY waldo/Cargo.toml waldo/
COPY nerve/src nerve/src
COPY waldo/src waldo/src
RUN cargo build --release --locked --workspace \
    && install -m 0755 target/release/waldo /usr/local/bin/waldo

FROM debian:trixie-slim AS runner
# git нужен для клонов через git worktree.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git \
    && rm -rf /var/lib/apt/lists/*
RUN useradd --uid 10001 --create-home --shell /usr/sbin/nologin waldo \
    && install -d -o waldo -g waldo /workspace /home/waldo/.local/state/iapetus
COPY --from=build /usr/local/bin/waldo /usr/local/bin/waldo
WORKDIR /workspace
USER waldo
ENTRYPOINT ["waldo"]
CMD ["runner"]
