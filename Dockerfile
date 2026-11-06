FROM rust:1-slim-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends cmake clang pkg-config make perl
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p lighthouse-server -p lighthouse-cli -p lighthouse-mcp

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN useradd --system --uid 10001 --home-dir /var/lib/lighthouse lighthouse \
    && mkdir -p /var/lib/lighthouse/data /etc/lighthouse \
    && chown -R lighthouse /var/lib/lighthouse
COPY --from=build /src/target/release/lighthouse-server /usr/local/bin/
COPY --from=build /src/target/release/lht /usr/local/bin/
COPY --from=build /src/target/release/lighthouse-demo-mcp /usr/local/bin/
COPY skills /usr/share/lighthouse/skills
COPY agents/examples /usr/share/lighthouse/agents-examples
USER lighthouse
WORKDIR /var/lib/lighthouse
ENV LIGHTHOUSE_CONFIG=/etc/lighthouse/lighthouse.toml
EXPOSE 8080
CMD ["lighthouse-server"]
