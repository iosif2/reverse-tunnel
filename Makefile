#
# Copyright 2025 iosif.dev 
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
# http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
#
PROJECT_NAME := reverse_tunnel

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
	@cargo run --bin http-server -- -p 8080

help:
	@echo "Makefile for $(PROJECT_NAME) Reverse Tunnel:"
	@echo "  all     - Builds the project (same as 'build')"
	@echo "  build   - Builds the project in debug mode"
	@echo "  release - Builds the project in release mode"
	@echo "  test    - Runs all tests"
	@echo "  run     - Runs the main binary in debug mode"
	@echo "  clean   - Removes the target directory"
	@echo "  help    - Displays this help message"