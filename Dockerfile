# Use the Rust image from the official Docker Hub
FROM rust:1.73-bookworm as builder

WORKDIR /usr/src/app

# Copy only the necessary files for building the project
COPY ./Cargo.toml ./
COPY ./Cargo.lock ./
COPY ./server ./server
COPY ./client ./client

# Build the project
RUN cargo build --release --bin server

# Create a new stage to reduce the size of the final image
FROM debian:bookworm-slim

WORKDIR /usr/src/app

# Copy only the built binary from the previous stage
COPY --from=builder /usr/src/app/target/release/server .

# Expose the port your Rust application will run on
EXPOSE 6969

