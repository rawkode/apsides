FROM oven/bun:1.4.0 AS build
WORKDIR /app
COPY package.json bun.lock ./
RUN bun install --frozen-lockfile
COPY tsconfig.json ./
COPY src ./src
COPY examples ./examples
RUN bun build src/cli.ts --compile --outfile /controller

FROM debian:bookworm-slim
RUN apt-get update && apt-get install --no-install-recommends -y ca-certificates libstdc++6 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /controller /usr/local/bin/controller
ENTRYPOINT ["/usr/local/bin/controller"]
