# 왜 toki는 OpenTelemetry를 받지 않고 로컬 파일을 직접 파싱하는가

## 배경

Claude Code, Codex CLI, Gemini CLI 모두 OpenTelemetry (OTEL) export를 지원한다. 자연스러운 질문이 따른다: 왜 toki에 OTLP receiver를 내장해서 CLI 도구가 push하게 하지 않고, 로컬 세션 파일을 직접 파싱하는가?

이 문서는 toki의 file 기반 아키텍처가 어떤 근거로 선택되었는지 정리한다.

## 대안: 임베디드 OTLP receiver

```text
CLI tool → OTLP export → toki (localhost OTLP server) → DB
```

대신 현재 구조는:

```text
CLI tool → local session files → toki (file watcher) → DB
```

## 왜 file parsing이 이기는가

### 1. Cold-start가 file parser를 필수로 만든다

toki의 cold-start는 모든 과거 세션 파일을 스캔해서 TSDB에 색인한다. 이는 retroactive 분석을 위해 필수다 — toki 설치 이전의 토큰 사용량, 또는 데몬이 멈춰 있던 기간의 데이터를 보려면 cold-start가 필요하다.

cold-start가 파일을 읽어야 하므로 **file parser는 어떤 provider에 대해서도 반드시 존재해야 한다**. watch mode를 위해 OTLP 수신을 추가한다면, 두 개의 데이터 수집 경로를 유지해야 한다:

- File parser (전 provider, cold-start용)
- OTLP event mapper (OTLP를 지원하는 provider, watch mode용)

File-only 방식은 한 경로만 유지한다:

- File parser (전 provider, cold-start와 watch mode 양쪽 모두)

동일한 코드가 두 모드를 다 처리한다. 테스트도 하나, 버그도 하나, 유지보수 대상도 하나다.

### 2. 설정 제로

toki는 이미 존재하는 디렉토리(`~/.claude`, `~/.codex`)를 가리키기만 하면 동작한다. 어떤 CLI 도구의 설정도 바꿀 필요가 없다.

OTLP 수신은 각 CLI 도구를 toki의 endpoint로 export하도록 설정해야 한다:

- Claude Code: `~/.claude/settings.json`의 환경변수
- Codex: `~/.codex/config.toml`의 `[otel]` 섹션
- Gemini: `~/.gemini/settings.json`의 `telemetry` 객체

toki가 이 설정을 자동 주입한다고 해도, provider별 설정 주입 로직을 유지해야 한다 — 그리고 사용자의 기존 OTEL 설정과 충돌할 위험이 생긴다.

### 3. Retroactive 분석

toki는 설치 직후에 수개월치 과거 데이터를 분석할 수 있다. OTLP 수신은 실행 시점 이후의 데이터만 캡처한다. cold-start의 파일 스캔이 retroactive 분석을 가능하게 만들고, 이 스캔은 watch mode와 정확히 같은 parser를 쓴다.

### 4. 추가 의존성 없음

파일 감시는 OS 기본 메커니즘(FSEvents, inotify)을 `notify` crate로 사용한다 — 이미 의존성에 포함되어 있다. OTLP 수신은 gRPC 또는 HTTP 서버 스택(tonic/axum + protobuf용 prost)을 임베드해야 하며, 바이너리 크기와 의존성 표면이 크게 증가한다.

### 5. OTLP 데이터도 결국 provider별 해석이 필요하다

각 CLI 도구는 서로 다른 OTLP 시그널을 다른 메트릭명과 구조로 내보낸다:

| CLI | 토큰 메트릭명 | 시그널 타입 |
|-----|-------------|-------------|
| Claude Code | `claude_code.token.usage` | Log Record |
| Gemini CLI | `gemini_cli.token.usage` | Counter |
| Codex | custom metrics | Log Record |

OTLP를 받는다고 provider별 로직이 사라지는 게 아니다 — "파일 스키마 파싱"에서 "OTLP 이벤트 매핑"으로 이동할 뿐이다. 복잡도는 없어지지 않고 형태만 바뀐다.

### 6. 데이터 완결성

toki 데몬이 재시작될 때 file 기반 복구는 자동이다 — checkpoint가 마지막 처리 라인부터 재개하고, toki가 멈춘 사이에 기록된 데이터를 따라잡는다.

OTLP 수신에서는 toki가 멈춰 있는 동안 보내진 이벤트가 손실된다. 표준 OTEL SDK의 `BatchLogRecordProcessor`는 메모리에만 버퍼링하며, 제3자 endpoint에 대한 디스크 기반 retry가 없다. cold-start는 결국 파일에서 데이터를 복구하므로, OTLP 경로는 신뢰성 면에서 추가 가치가 없다 — file 경로와 완전히 중복된다.

## OTLP가 의미 있어지는 조건

OTLP 수신이 가치 있어지려면 다음 조건이 필요하다:

- toki가 cold-start의 파일 파싱을 완전히 포기 (retroactive 분석 손실)
- 대상 CLI 도구 전부가 OTLP를 지원 (현재는 아님 — pi-agent, OpenCode 등은 미지원 또는 제한적)
- toki의 목표가 토큰 추적에 집중하는 게 아니라 general-purpose observability

어느 것도 toki의 설계 목표에 해당하지 않는다.

## 요약

| 측면 | File parsing | OTLP reception |
|------|-------------|----------------|
| Parser가 필요한 provider | 전부 | 전부 (cold-start가 여전히 필요) |
| Watch mode 코드 경로 | 1 (file) | 2 (file + OTLP) |
| 사용자 설정 필요 | 없음 | CLI별 OTEL 설정 |
| 과거 데이터 | 즉시 | 활성화 시점 이후만 |
| 바이너리 의존성 | notify (기존) | + gRPC/HTTP 스택 |
| 데몬 다운 시 복구 | 자동 (checkpoint) | cold-start 전까지 데이터 손실 |

File parsing은 toki의 목표 — **여러 AI CLI 도구의 토큰 사용량을 설정 없이 추적** — 에 대해 더 단순하고, 더 완결적이며, 더 유지보수하기 쉬운 접근이다.
