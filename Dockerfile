FROM rust:1.91-bookworm AS build

WORKDIR /build
COPY . .

RUN --mount=type=cache,target=/build/target/ \
    --mount=type=cache,target=/usr/local/cargo/git/db \
    --mount=type=cache,target=/usr/local/cargo/registry/ \
	cargo build --release && cp target/release/bambu-octoprint-emulator .

FROM gcr.io/distroless/cc-debian12

COPY --from=build /build/bambu-octoprint-emulator /
CMD ["/bambu-octoprint-emulator"]
