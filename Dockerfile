FROM rust:1.85-alpine AS build

RUN apk add --no-cache musl-dev perl make

WORKDIR /build
COPY . .

RUN --mount=type=cache,target=/build/target/ \
    --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/usr/local/cargo/registry/ \
	cargo build --release && cp target/release/bambu-octoprint-emulator .

FROM scratch

COPY --from=build /build/bambu-octoprint-emulator /
CMD ["/bambu-octoprint-emulator"]
