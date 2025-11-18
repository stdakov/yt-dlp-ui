###########################################################
# 1. Builder stage
###########################################################
FROM rust:1.75 AS builder

WORKDIR /app

# Pre-build dependencies using a dummy main.rs
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src \
    && echo "fn main() { println!(\"dummy\"); }" > src/main.rs \
    && cargo build --release \
    && rm -rf src

# Copy actual source and force rebuild of your real binary
COPY . .
RUN touch src/main.rs \
    && cargo build --release


###########################################################
# 2. Runtime stage
###########################################################
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
    ca-certificates \
    ffmpeg \
    python3 \
    python3-pip \
    curl \
    && curl -L https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp \
    -o /usr/local/bin/yt-dlp \
    && chmod +x /usr/local/bin/yt-dlp \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy app binary and static assets
COPY --from=builder /app/target/release/yt-dlp-ui /usr/local/bin/yt-dlp-ui
COPY --from=builder /app/static /app/static
COPY --from=builder /app/templates /app/templates

ENV DATA_DIR=/data
ENV DOWNLOADS_DIR=/data/downloads
ENV ARCHIVES_DIR=/data/archives
ENV BIND_ADDR=0.0.0.0:8090

VOLUME ["/data"]
EXPOSE 8090

CMD ["yt-dlp-ui"]