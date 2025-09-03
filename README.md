# Reverse Tunnel

Rust로 구현된 고성능 리버스 터널(Reverse Tunnel) / 리버스 포트 포워딩 서비스입니다. 방화벽·NAT 내부의 클라이언트가 외부 공개 서버로 먼저 연결(콜백)하여 터널을 열고, 서버는 공개 포트로 들어오는 요청을 해당 터널을 통해 내부 서비스로 역방향 프록시합니다. SSH의 `-R`(Remote Port Forwarding)과 유사한 동작을 TCP/HTTP/WebSocket 수준에서 제공합니다.

## 구성요소
- 서버(`bin/server.rs`):
  - 공개 포트 리스닝 → 최초 바이트로 프로토콜 특성 판단 → 클라이언트 콜백 세션과 매칭 → 프록시 처리
- 클라이언트(`bin/client.rs`):
  - 외부 서버로 제어 채널 연결 및 등록 → 서버 지시에 따라 콜백 연결 생성 → 내부 백엔드와 터널 연결

## 아키텍처 개요
1) Client → Server: 제어 채널 등록(backend_port, public_port)
2) Public → Server: 외부 사용자가 서버의 public_port로 접속
3) Server → Client: 콜백 ID 전송, 클라이언트가 역방향 연결 생성
4) Server: Public 연결과 Client 콜백 연결을 매칭하여 데이터 중계

## 빠른 시작
빌드
```
cargo build --release
```

서버 실행 예시
```
cargo run --bin server -- \
  --host 0.0.0.0 \
  --port 7000 \
  --connection-timeout 5 \
  --idle-timeout 0 \
  --data-logging off \
  --buffer-size 32
```

클라이언트 실행 예시(예시 인자, 실제 값은 환경에 맞게 조정)
```
cargo run --bin client -- ...
```

## 주요 CLI 옵션(서버)
- --host, -H: 서버 바인드 호스트(기본 0.0.0.0)
- --port, -P: 서버 바인드 포트(기본 7000)
- --connection-timeout, -t: 콜백/초기 데이터 대기 타임아웃(초)
- --idle-timeout, -i: 유휴 타임아웃(초, 0은 무제한)
- --data-logging, -d: 데이터 로깅 레벨(off|minimal|verbose)
- --buffer-size, -b: 내부 버퍼 크기(KB)

## 라이선스
본 프로젝트는 LICENSE 파일을 참고하세요.


## 테스트
HTTP 서버를 테스트할 수 있는 curl 명령어들을 만들어드릴게요:

### 기본 GET 요청
```bash
curl -v http://localhost:3000/
```

### POST 요청 (JSON body)
```bash
curl -v -X POST http://localhost:3000/ \
  -H "Content-Type: application/json" \
  -H "User-Agent: TestClient/1.0" \
  -H "Authorization: Bearer test-token-123" \
  -d '{"name": "test", "value": 42, "active": true}'
```

### 복잡한 헤더와 body
```bash
curl -v -X PUT http://localhost:3000/ \
  -H "Content-Type: application/json" \
  -H "Accept: application/json" \
  -H "X-API-Key: abc123def456" \
  -H "X-Request-ID: req-$(date +%s)" \
  -H "User-Agent: MyApp/2.1.0 (Linux x86_64)" \
  -H "Accept-Language: ko-KR,ko;q=0.9,en;q=0.8" \
  -H "Cache-Control: no-cache" \
  -d '{
    "user": {
      "id": 123,
      "name": "홍길동",
      "email": "hong@example.com",
      "preferences": {
        "theme": "dark",
        "notifications": true
      }
    },
    "metadata": {
      "source": "mobile",
      "version": "2.1.0"
    }
  }'
```

### WebSocket 업그레이드 테스트
```bash
curl -v -X GET http://localhost:3000/ \
  -H "Upgrade: websocket" \
  -H "Connection: Upgrade" \
  -H "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==" \
  -H "Sec-WebSocket-Version: 13"
```

### 파일 업로드 시뮬레이션
```bash
curl -v -X POST http://localhost:8080/upload \
  -H "Content-Type: multipart/form-data" \
  -H "X-Upload-Session: session-$(uuidgen)" \
  -F "file=@/dev/null" \
  -F "description=테스트 파일"
```

### 랜덤 헤더와 body (한 줄로)
```bash
curl -v -X POST http://localhost:8080/random \
  -H "X-Random-Header: $(openssl rand -hex 8)" \
  -H "X-Timestamp: $(date +%s)" \
  -H "Content-Type: application/json" \
  -d "{\"random\": \"$(openssl rand -base64 32)\", \"timestamp\": $(date +%s)}"
```

### 여러 요청을 연속으로 보내기
```bash
for i in {1..5}; do
  curl -v -X POST http://localhost:8080/batch \
    -H "X-Request-Number: $i" \
    -H "Content-Type: application/json" \
    -d "{\"id\": $i, \"data\": \"request-$i\"}"
  echo "--- Request $i completed ---"
done
```

가장 유용한 건 **복잡한 헤더와 body** 명령어입니다. 이걸로 서버가 헤더와 본문을 제대로 파싱하는지 확인할 수 있어요!