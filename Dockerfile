# Multi-stage build for moonlight-web-stream
FROM rust:latest as builder

# Build mode: debug or release (default: debug)
ARG BUILD_MODE=debug

# Install Rust nightly (required by moonlight-web-stream)
RUN rustup default nightly

# Install build dependencies (including nodejs/npm for frontend build)
RUN apt-get update && apt-get install -y \
    procps \
    curl \
    cmake \
    libssl-dev \
    pkg-config \
    clang \
    libclang-dev \
    nodejs \
    npm \
    && rm -rf /var/lib/apt/lists/*

# CRITICAL: Tell openssl-sys to use system OpenSSL instead of building from source
# This prevents slow perl-based OpenSSL compilation (no more perl processes!)
ENV OPENSSL_NO_VENDOR=1
ENV OPENSSL_DIR=/usr
ENV OPENSSL_LIB_DIR=/usr/lib/x86_64-linux-gnu
ENV OPENSSL_INCLUDE_DIR=/usr/include

WORKDIR /build

# Copy source code
COPY . .

# Build the binary with cargo cache (debug or release based on BUILD_MODE)
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    if [ "$BUILD_MODE" = "debug" ]; then \
        cargo build && \
        cp /build/target/debug/web-server /tmp/web-server && \
        cp /build/target/debug/streamer /tmp/streamer; \
    else \
        cargo build --release && \
        cp /build/target/release/web-server /tmp/web-server && \
        cp /build/target/release/streamer /tmp/streamer; \
    fi

# Build web frontend
WORKDIR /build/moonlight-web/web-server
RUN --mount=type=cache,target=/root/.npm \
    npm install
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

# Copy binaries from builder (copied to /tmp during build due to cache mount)
COPY --from=builder /tmp/web-server /app/web-server
COPY --from=builder /tmp/streamer /app/streamer

# Copy web assets (use 'dist' for debug builds, 'static' for release builds - see web.rs)
COPY --from=builder /build/moonlight-web/web-server/dist /app/dist

# Create config directory
RUN mkdir -p /server

# Enable trace logging for debugging UDP/streaming issues
ENV RUST_LOG=moonlight_common=trace,moonlight_web=trace

# Expose web server port
EXPOSE 8080

# Run web server
CMD ["/app/web-server"]
