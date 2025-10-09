# Multi-stage build for moonlight-web-stream
FROM rust:latest as builder

# Install Rust nightly (required by moonlight-web-stream)
RUN rustup default nightly

# Install build dependencies
RUN apt-get update && apt-get install -y \
    cmake \
    libssl-dev \
    pkg-config \
    clang \
    libclang-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy source code
COPY . .

# Build the release binary
RUN cargo build --release

# Build web frontend
WORKDIR /build/moonlight-web/web-server
RUN apt-get update && apt-get install -y nodejs npm && rm -rf /var/lib/apt/lists/*
RUN npm install
RUN npm run build

# Runtime stage - use same Debian version as builder for GLIBC compatibility
FROM debian:sid-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    libssl3 \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Create app directory
WORKDIR /app

# Copy binaries from builder
COPY --from=builder /build/target/release/web-server /app/web-server
COPY --from=builder /build/target/release/streamer /app/streamer

# Copy web assets
COPY --from=builder /build/moonlight-web/web-server/dist /app/static

# Create config directory
RUN mkdir -p /server

# Expose web server port
EXPOSE 8080

# Run web server
CMD ["/app/web-server"]
