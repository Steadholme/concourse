#!/bin/sh
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
container_name="concourse-murmur-pg-gate-$$"
postgres_image="postgres@sha256:1b1689b20d16a014a3d195653381cf2caa75a41a92d93b255a9d6ea29fd353aa"

cleanup() {
    docker stop "$container_name" >/dev/null 2>&1 || true
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

docker run --rm -d \
    --name "$container_name" \
    -e POSTGRES_PASSWORD=pw \
    -e POSTGRES_DB=murmur \
    -p 127.0.0.1::5432 \
    --health-cmd='pg_isready -U postgres -d murmur' \
    --health-interval=1s \
    --health-timeout=3s \
    --health-retries=30 \
    "$postgres_image" >/dev/null

attempt=1
while [ "$attempt" -le 30 ]; do
    status=$(docker inspect --format '{{.State.Health.Status}}' "$container_name")
    if [ "$status" = "healthy" ]; then
        break
    fi
    if [ "$status" = "unhealthy" ]; then
        docker logs "$container_name"
        exit 1
    fi
    attempt=$((attempt + 1))
    sleep 1
done
if [ "$(docker inspect --format '{{.State.Health.Status}}' "$container_name")" != "healthy" ]; then
    docker logs "$container_name"
    exit 1
fi

port_mapping=$(docker port "$container_name" 5432/tcp)
postgres_port=${port_mapping##*:}
export MURMUR_TEST_DATABASE_URL="postgres://postgres:pw@127.0.0.1:${postgres_port}/murmur"

cd "$repo_dir"
cargo test --manifest-path crates/atrium/Cargo.toml \
    --test murmur_postgres -- --ignored --nocapture
cargo test --manifest-path crates/murmur/Cargo.toml \
    --test postgres_security -- --ignored --nocapture
TEST_DATABASE_URL="$MURMUR_TEST_DATABASE_URL" \
    cargo test --manifest-path crates/almanac/Cargo.toml \
    --test pg_store -- --ignored --nocapture
