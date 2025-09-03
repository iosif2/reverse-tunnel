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

