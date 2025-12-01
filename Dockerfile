FROM rust:alpine AS builder

RUN apk add \
    musl-dev \
    make \
    cmake \
    g++

WORKDIR /app

COPY . /app

RUN cargo build --release

FROM alpine

# Run as non-root user
RUN adduser -D -u 1000 httpgate
USER httpgate

WORKDIR /app

# Copy the binary from builder
COPY --from=builder /app/target/release/httpgate /app/httpgate

EXPOSE 8080

ENTRYPOINT ["/app/httpgate"]
