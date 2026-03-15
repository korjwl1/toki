<!--
SYNC IMPACT REPORT
- Version: 1.3.1
- Ratified: 2026-03-13
- Added Principles: all 8 (new document)
- Removed Principles: none
- Changed: §2 removed gemini/codex stub references (stubs deleted); §4 4-thread daemon/client architecture; §5 7 keyspaces (TSDB schema)
- Template Updates:
  - .spec-mix/active-mission/templates/plan-template.md ✅ no changes needed
  - .spec-mix/active-mission/templates/spec-template.md ✅ no changes needed
  - .spec-mix/active-mission/templates/tasks-template.md ✅ no changes needed
- Deferred TODOs: none
-->

# Project Constitution: toki

## Scope & Current Target

toki는 AI CLI 도구들의 로컬 JSONL/JSON 로그를 감시하여
토큰 사용량과 비용을 추적하는 Rust 모듈이다.

**현재 구현 범위 (v0.1)**: **Claude Code CLI + macOS**만 대상으로 한다.
Gemini CLI, Codex CLI, Windows, Linux는 trait 인터페이스가 준비되어 있으며,
필요 시 provider/platform 모듈을 추가하여 확장한다.

**참조 문서** (`../../logic/` 폴더):
- `jsonl-file-watcher.md`: 초기 아키텍처 참조 — 현재 구현은 역순 라인 스캔 +
  xxHash3 기반 체크포인트, 즉시 DB 저장, bracket-depth JSON 완성도 검사로 개선됨
- `token-estimation-strategy.md`: Claude Code CLI는 서버가 정확한
  토큰 수를 제공하므로 토크나이저 추정 불필요
- `claude-code-web-interception.md`: Claude Code 메시지 구조 참고
  (message.usage 내 input_tokens, output_tokens, cache_read_input_tokens,
  cache_creation_input_tokens 필드)

---

## Core Principles

### 1. Module-First Design

- toki는 독립 실행보다 **모듈로 사용되는 것이 주 목적**이다.
  공개 API(pub 함수, trait, struct)는 반드시 임베딩 측에서 호출 가능하도록
  설계해야 하며, `main.rs`는 참조 구현(reference binary)에 불과하다.

- 호스트 애플리케이션의 런타임과 충돌하지 않도록
  외부 런타임(tokio 등)에 대한 의존을 금지한다.
  `start() → Handle`, `Handle::stop()` 패턴으로 진입/종료를 제공한다.

- **근거**: 향후 상위 애플리케이션에서 toki 엔진을 라이브러리로
  가져다 쓰는 것이 확정된 요구사항이다.
  호스트가 이미 tokio/rayon 등을 사용할 수 있으므로 런타임 중립이 필수.

### 2. Provider–Platform Separation

- **Provider(프로바이더)**: Claude Code, Gemini CLI, Codex 등 AI CLI 도구별
  로직은 `providers/<name>/` 아래에 독립 모듈로 분리한다.
  공통 인터페이스는 `LogParser` trait으로 강제한다.

- **Platform(플랫폼)**: macOS, Windows, Linux 등 OS별 파일 시스템 감시 및
  경로 유틸리티는 `platform/<os>/` 아래에 분리한다.
  빌드 시 `cfg(target_os)` 조건 컴파일로 해당 OS 모듈만 포함한다.

- **현재 v0.1 구현**: `providers/claude_code/` + `platform/macos/`만 구현.
  새 provider/platform 추가 시 해당 모듈 디렉토리를 생성한다.

- **근거**: 최종적으로 3 provider × 3 OS 조합으로 확장되어야 하므로
  축 분리가 필수. 하지만 지금 당장은 동작하는 Claude Code + macOS 하나를
  완성하는 것이 우선이다.

### 3. Minimal Resource Consumption

- 파일 읽기는 MUST 증분 읽기를 사용한다.
  역순 라인 스캔(find_resume_offset)으로 마지막 처리 줄을 찾아
  그 이후만 읽는다. 전체 파일을 매번 다시 읽는 것은 금지한다.

- 체크포인트(last_line_len + xxHash3-64 해시)를 DB에 영속화하여
  재시작 시 불필요한 재처리를 방지한다. 바이트 오프셋을 저장하지
  않으므로 compaction으로 파일 내용이 변해도 자동 복구된다.

- DB 쓰기는 파일 처리 완료 즉시 upsert하여 데이터 일관성을 보장한다.
  cold start 시에는 배치 flush(단일 트랜잭션)로 효율화한다.

- 불완전한 마지막 줄(쓰기 도중 읽힌 경우)은 bracket-depth 검사
  (is_complete_json_object)로 판별하여, 완전한 JSON이면 포함하고
  그렇지 않으면 버린 뒤 다음에 다시 읽는다.

- **근거**: `~/.claude/projects/`에 수백 개 JSONL 파일이 존재할 수 있으며,
  백그라운드 프로세스로서 CPU/IO 영향이 최소여야 한다.

### 4. OS Threads + Channel (동시성 모델)

- 동시성은 MUST `std::thread` + `std::sync::mpsc` (또는 `crossbeam::channel`)
  기반으로 구현한다. tokio 비동기 런타임은 사용하지 않는다.

- **아키텍처** (데몬/클라이언트, 4 threads + bounded channel):
  - **notify 스레드**: FSEvents 콜백 → 채널로 파일 경로 전송
  - **worker 스레드**: `crossbeam_channel::select!`로 이벤트 수신 + stop 시그널 +
    백업 폴링을 다중화. 파일 읽기, JSON 파싱, BroadcastSink 출력, DbOp 전송을 처리.
  - **writer 스레드**: Database를 단독 소유. bounded channel(1024)로 DbOp 수신,
    64개 배치 commit, 일 1회 retention 실행.
  - **listener 스레드**: UDS accept loop. trace 클라이언트 연결을 BroadcastSink에 등록.
    클라이언트 0개 시 Sink 오버헤드 0 (zero overhead).
  - **trace 클라이언트**: 데몬에 UDS로 연결, NDJSON 스트림 수신 → 로컬 sink 출력.
  - **report 클라이언트**: DB를 읽기 전용으로 열어 TSDB 직접 쿼리.

- DB 커넥션은 writer 스레드에 소유시켜 Send 이슈를 원천 차단한다.
  Worker → Writer 간 통신은 bounded channel의 DbOp enum으로 이루어진다.

- **비교 분석 근거**:
  - ~~tokio async~~: 우리 워크로드는 로컬 파일 I/O이지 네트워크가 아니다.
    `tokio::fs`도 내부적으로 `spawn_blocking`을 쓰므로 이점 없음.
    라이브러리 모듈에서 런타임 충돌 위험. 바이너리 +300-800KB.
  - ~~rayon~~: cold start 병렬화엔 좋지만 이벤트 루프/타이머 미지원.
    단독 해법 불가. 글로벌 스레드풀 공유 문제.
  - ~~multi-process~~: 이 규모에서 IPC 복잡도만 증가. 불필요.
  - **std::thread**: notify가 이미 자체 스레드 사용.
    `recv_timeout`으로 이벤트 + 폴링 + 타이머를 단일 루프에서 처리.
    바이너리 오버헤드 0. DB 커넥션 Send 문제 없음. 라이브러리 친화적.

### 5. fjall (경량 로컬 DB)

- 데이터 저장은 MUST fjall(pure Rust LSM-tree embedded DB)을 사용한다.
  외부 서비스(Redis, PostgreSQL 등) 의존은 금지한다.

- DB에 저장하는 데이터 (7 keyspaces):
  - `checkpoints`: file_path(key) → bincode(FileCheckpoint)
  - `meta`: key → value (pricing 캐시 등)
  - `events`: [ts_ms BE:8][message_id] → bincode(StoredEvent)
  - `rollups`: [hour_ts BE:8][model_name] → bincode(RollupValue)
  - `idx_sessions`: {session_id}\0[ts:8][msg_id] → empty
  - `idx_projects`: {project}\0[ts:8][msg_id] → empty
  - `dict`: string → bincode(u32) (문자열 딕셔너리 압축)

- Big-endian timestamp 키로 lexicographic = chronological 정렬.
  Range scan으로 시간 범위 쿼리. Rollup-on-write로 시간별 집계.

- **비교 분석 근거 (SQLite vs fjall)**:
  - 데이터 모델이 key-value + range scan이다. SQL 불필요.
  - fjall은 Pure Rust → C 툴체인 불필요, 크로스 컴파일 용이.
  - LSM-tree 기반으로 TSDB keyspace 구조에 자연스럽게 적합.
  - Keyspace 단위 데이터 분리, atomic batch write 지원.
  - ~~SQLite 장점~~: 25년 검증, 복잡 쿼리 가능 → 우리한텐 불필요.

### 6. Incremental-Only Processing

- 최초 실행 시에는 대상 디렉토리 내 전체 세션 파일을 한 번 읽어
  초기 집계를 수행한다 (cold start).
  (`jsonl-file-watcher.md` §1 "앱 시작 → SQLite에서 file_checkpoints 로드" 참조)

- 이후 동작은 MUST FSEvents 이벤트 + 30초 백업 폴링으로 감지된
  **변경분만** 처리한다.
  (`jsonl-file-watcher.md` §5.1 FSEvents + §5.2 백업 폴링 참조)

- 두 가지 케이스를 통합 처리한다:
  - CASE 1: checkpoint 없음 (최초 발견) → offset 0, 전체 읽기
  - CASE 2: checkpoint 있음 → find_resume_offset()로 역순 라인 스캔,
    찾으면 그 다음부터 증분 읽기, 못 찾으면 전체 재처리 (at-least-once)

- **근거**: append-only JSONL 특성 활용. 최소 I/O와 자동 복구 보장.

### 7. Claude Code Parser Specifics (v0.1)

- Claude Code JSONL 파싱 규칙
  (`jsonl-file-watcher.md` §6.1 parse_claude 참조):
  - 파일 위치: `~/.claude/projects/**/*.jsonl` (기본값, 설정으로 변경 가능)
  - 각 줄: `{"type":"assistant","message":{"id":"...","model":"...","usage":{...}},...}`
  - 추출 필드: `message.usage.input_tokens`, `output_tokens`,
    `cache_read_input_tokens`, `cache_creation_input_tokens`
  - event_key: `{message.id}:{timestamp}` (중복 방지용)

- Claude Code는 서버가 정확한 토큰 수를 제공하므로
  토크나이저 추정이 불필요하다.
  (`token-estimation-strategy.md` §1 표 참조)

- 비용 계산: 모델별 가격표(내장) × 토큰 수.
  (`jsonl-file-watcher.md` §7 비용 계산 참조)

- 설정: 기본 `${HOME}/.claude`를 루트 폴더로 사용하되,
  DB settings 또는 환경변수로 변경 가능해야 한다.

### 8. Output as Print (Interim Contract)

- 현 단계에서 파싱 결과는 MUST stdout `println!`으로 출력한다.
  구조화된 이벤트 전달(callback, channel, gRPC 등)은 추후 모듈 통합 시 구현한다.

- 초기 스캔 완료 시: 파일별 × 모델별 합산 요약을 출력한다.
- 실시간 이벤트 감지 시: 개별 이벤트 정보를 즉시 출력한다.

- **근거**: 후속 로직이 아직 결정되지 않았으므로, 최소한의 출력 인터페이스만
  제공하여 검증 가능하게 한다.

---

## Development Workflow

1. **Constitution**: 프로젝트 원칙 정의 (이 문서)
2. **Specification**: 기능 요구사항 및 시나리오 작성
3. **Planning**: 기술 구현 계획 수립
4. **Implementation**: 코드 작성 + 테스트
5. **Review**: 코드 리뷰 및 원칙 준수 확인
6. **Integration**: main 브랜치 병합

## Quality Gates

Before merging to main:
- [ ] `cargo build` 성공 (경고 없음)
- [ ] `cargo test` 전체 통과
- [ ] `cargo clippy` 경고 없음
- [ ] 새 provider/platform 추가 시 기존 trait 인터페이스 준수 확인
- [ ] 증분 읽기 로직 변경 시 체크포인트 복구 시나리오 테스트
- [ ] 성능 회귀 없음 확인

## Decision-Making Framework

기술 선택 시 우선순위:

1. **리소스 효율성**: CPU/IO 영향 최소화가 최우선
2. **확장성**: provider/platform 추가 시 기존 코드 변경 최소화
3. **안정성**: 파일 누락/데이터 손실 방지
4. **단순성**: 과도한 추상화보다 명확한 구현 우선

## Governance

- **Version**: 1.3.1
- **Ratified**: 2026-03-13
- **Amendment Procedure**: 원칙 변경 시 constitution 버전을 시맨틱 버저닝으로 증가.
  MAJOR: 원칙 제거/재정의, MINOR: 원칙 추가/확장, PATCH: 표현 수정.
