# Targets:
#   (default)  the server, `lht` and the demo MCP server, without the web app. The demo mounts the web app.
#   release    the same plus the built web app from `web-dist/` in `/usr/share/lighthouse/web`.
#              Build lighthouse-web first and copy its `dist` to `web-dist/`, then `docker build --target release .`
FROM rust:1-slim-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends cmake clang pkg-config make perl
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p lighthouse-server -p lighthouse-cli -p lighthouse-mcp

FROM debian:bookworm-slim AS base
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
COPY automations/examples /usr/share/lighthouse/automations-examples
COPY demo/agents /usr/share/lighthouse/demo/agents
COPY demo/automations /usr/share/lighthouse/demo/automations
USER lighthouse
WORKDIR /var/lib/lighthouse
ENV LIGHTHOUSE_CONFIG=/etc/lighthouse/lighthouse.toml
EXPOSE 8080
CMD ["lighthouse-server"]

FROM base AS release
COPY web-dist /usr/share/lighthouse/web
ENV LIGHTHOUSE_PATHS__WEB=/usr/share/lighthouse/web

# The last stage is the default target, so a plain `docker build` needs no `web-dist/`.
FROM base AS runtime
