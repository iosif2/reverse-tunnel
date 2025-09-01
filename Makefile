PROJECT_NAME := my_rust_project

.PHONY: all build release test run clean

all: build

build:
	@cargo build

release:
	@cargo build --release

test:
	@cargo test

run:
	@cargo run

clean:
	@cargo clean

server:
	@cargo run --bin server -- --host 0.0.0.0 --port 8888

client:
	@cargo run --bin client -- -s 127.0.0.1:8888 -p 9999 -b 3000

http_server:
	@cargo run --bin http-server

help:
	@echo "Makefile for $(PROJECT_NAME) Rust project:"
	@echo "  all     - Builds the project (same as 'build')"
	@echo "  build   - Builds the project in debug mode"
	@echo "  release - Builds the project in release mode"
	@echo "  test    - Runs all tests"
	@echo "  run     - Runs the main binary in debug mode"
	@echo "  clean   - Removes the target directory"
	@echo "  help    - Displays this help message"