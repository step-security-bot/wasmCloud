# First stage: Build the binary
FROM rust:1-bullseye@sha256:0deb626fc05b074fc9b1453b007f92f0bec72c219e42a4a89b9b0d17201acb76 AS builder

WORKDIR /app

# Copy the necessary parts of the workspace since .dockerignore isn't being respected
COPY Cargo.toml Cargo.toml
COPY Cargo.lock Cargo.lock
COPY ./src ./src
COPY ./crates ./crates
COPY ./tests ./tests

# Build the binary
WORKDIR crates/secrets-nats-kv
RUN cargo build --release

# Second stage: Create a minimal container with the binary
FROM debian:bullseye-slim@sha256:3f9e53602537cc817d96f0ebb131a39bdb16fa8b422137659a9a597e7e3853c1

WORKDIR /app

# Copy the binary from the first stage
COPY --from=builder /app/target/release/secrets-nats-kv .

# Set the entrypoint
ENTRYPOINT ["./secrets-nats-kv"]
CMD ["run"]
