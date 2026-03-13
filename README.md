# clitrace

AI CLI 도구(Claude Code 등)의 JSONL 세션 로그를 실시간 감시하여 모델별 토큰 사용량을 추적하는 Rust 도구.
내장 TSDB에 이벤트와 시간별 rollup을 저장하고, 데몬/클라이언트 구조로 실시간 trace와 one-shot report를 모두 지원한다.

## Architecture

Docker처럼 데몬/클라이언트 구조로 동작한다:

```
clitrace daemon start   # 항상 실행 (= dockerd)
clitrace trace          # 실시간 스트림 클라이언트 (= docker logs -f)
clitrace report         # TSDB 조회 클라이언트 (= docker ps)
```

- **daemon**: 파일 감시 + TSDB 저장. 항상 실행되어야 하며, trace 클라이언트가 없을 때는 Sink 오버헤드 0.
- **trace**: 데몬에 UDS로 연결하여 실시간 이벤트 스트림 수신. print/uds/http 모든 sink 지원.
- **report**: TSDB에서 직접 조회. 데몬과 독립적으로 동작.

## Quick Start

```bash
# 1. 데몬 시작 (foreground, Ctrl+C으로 종료)
cargo run --release -- daemon start

# 2. 다른 터미널에서 실시간 이벤트 스트림
cargo run --release -- trace

# 3. 리포트 조회 (TSDB에서 즉시 조회)
cargo run --release -- report
cargo run --release -- report daily --since 20260301
cargo run --release -- report monthly
```

## Daemon

`daemon`은 항상 실행되는 서버 프로세스다. 시작 시 전체 세션 파일을 스캔(cold start)하여 TSDB를 구축하고, 이후 FSEvents로 파일 변경을 감시하며 새 이벤트를 실시간 수집한다.

```bash
clitrace daemon start
clitrace daemon start --startup-group-by day
clitrace daemon start --session-id 4de9291e
clitrace daemon start --project clitrace
clitrace daemon start --full-rescan
clitrace daemon stop
clitrace daemon status
```

| 옵션 | 설명 |
|------|------|
| `--startup-group-by hour\|day\|week\|month\|year` | cold start 시 시간 단위 그룹핑 요약 |
| `--session-id <PREFIX>` | 세션 UUID 접두사 필터 |
| `--project <NAME>` | 프로젝트 디렉토리 서브스트링 필터 |
| `--full-rescan` | 체크포인트 초기화 후 전체 재스캔 |
| `--claude-root <PATH>` | Claude Code 루트 디렉토리 (기본: `~/.claude`) |
| `--db-path <PATH>` | DB 경로 (기본: `~/.config/clitrace/clitrace.fjall`) |
| `--sock <PATH>` | UDS 소켓 경로 (기본: `~/.config/clitrace/daemon.sock`) |

- PID 파일: `~/.config/clitrace/daemon.pid`
- 동일 DB 경로 기준 단일 인스턴스만 허용 (file lock)

## Trace (Client)

`trace`는 실행 중인 데몬에 UDS로 연결하여 실시간 이벤트를 스트림으로 수신하는 클라이언트 명령이다. 데몬에 trace 클라이언트가 연결되어 있지 않으면 Sink 처리가 완전히 비활성화되어 리소스 소모가 없다(zero overhead).

```bash
clitrace trace
clitrace trace --sock /custom/daemon.sock
```

- 데몬이 실행 중이어야 함 (`clitrace daemon start` 먼저)
- 복수 클라이언트 동시 연결 지원 (fan-out)
- 모든 sink 타입 지원: `--sink print`, `--sink uds://...`, `--sink http://...`

## Report (One-Shot)

`report`는 TSDB에 저장된 데이터를 즉시 조회한다. 데몬과 독립적으로 동작하며, TSDB가 비어있으면 JSONL 파일을 직접 스캔한다.

```bash
# 전체 요약
clitrace report
clitrace report --since 20260301
clitrace report --since 20260301 --until 20260331

# 시간별 그룹핑
clitrace report daily --since 20260301
clitrace report weekly --since 20260301 --start-of-week tue
clitrace report monthly
clitrace report yearly
clitrace report hourly --from-beginning

# 세션별 그룹핑
clitrace report --group-by-session

# 프로젝트/세션 필터
clitrace report --project clitrace
clitrace report --session-id 4de9291e

# 타임존 지정
clitrace -z Asia/Seoul report daily --since 20260301

# 비용 표시 없이
clitrace --no-cost report
```

| 옵션 | 설명 |
|------|------|
| 서브커맨드 없음 | 전체 총합 (`--since`/`--until` 선택적) |
| `daily\|weekly\|monthly\|yearly\|hourly` | 시간별 그룹핑 |
| `--since YYYYMMDD[hhmmss]` | 시작 시점 (inclusive, `>=`) |
| `--until YYYYMMDD[hhmmss]` | 종료 시점 (inclusive, `<=`) |
| `--from-beginning` | `--since` 없이 전체 그룹핑 허용 |
| `--group-by-session` | 세션별 그룹핑 (시간 서브커맨드와 동시 사용 불가) |
| `--start-of-week mon\|tue\|...\|sun` | `weekly`에서만 사용 |

## Settings

`settings`는 cursive TUI로 설정 페이지를 연다. 설정은 DB에 저장되며, CLI 인자로도 개별 오버라이드 가능.

```bash
clitrace settings
clitrace settings --db-path /custom/clitrace.fjall
```

| 설정 항목 | 설명 | 기본값 |
|-----------|------|--------|
| Claude Code Root | Claude Code 루트 디렉토리 | `~/.claude` |
| Daemon Socket | 데몬 UDS 소켓 경로 | `~/.config/clitrace/daemon.sock` |
| Timezone | IANA 타임존 (빈값=UTC) | (없음) |
| Output Format | 기본 출력 형식 | `table` |
| Start of Week | 주간 리포트 시작 요일 | `mon` |
| No Cost | 비용 계산 비활성화 | `false` |
| Retention Days | 이벤트 보존 기간 (0=무제한) | `0` |
| Rollup Retention Days | Rollup 보존 기간 (0=무제한) | `0` |

설정 우선순위: **CLI 인자 > DB settings > 기본값** (환경변수 미사용)

## Global Options

| 옵션 | 설명 |
|------|------|
| `--output-format table\|json` | 출력 형식 오버라이드 (DB 설정보다 우선) |
| `--sink <SPEC>` | 출력 대상, 복수 지정 가능 |
| `--timezone <IANA>` / `-z` | 타임존 오버라이드 |
| `--no-cost` | 비용 계산 비활성화 오버라이드 |
| `--db-path <PATH>` | DB 경로 (기본: `~/.config/clitrace/clitrace.fjall`) |

### Sink 종류

| Sink | 설명 |
|------|------|
| `print` (기본) | 터미널 출력 (`--output-format`에 따라 table/json) |
| `uds://<path>` | Unix Domain Socket으로 NDJSON 전송 |
| `http://<url>` | HTTP POST로 JSON 전송 (5초 timeout) |

```bash
# 터미널 + HTTP 동시 출력
clitrace trace --sink print --sink http://localhost:8080/events
```

## Cost Calculation

모든 출력에 모델별 추정 비용(USD)이 표시된다.
가격 데이터는 [LiteLLM](https://github.com/BerriAI/litellm) 커뮤니티 가격표에서 가져온다.

- **최초 실행**: LiteLLM JSON 다운로드 → Claude 모델 추출 → DB 캐시
- **이후 실행**: HTTP ETag 조건부 요청 → 변경 없으면 304 (바디 없이 ~50ms)
- **오프라인**: 캐시된 데이터로 동작, 캐시 없으면 Cost 컬럼 생략
- **`--no-cost`**: 가격 fetch 스킵

## Environment Variables

| 변수 | 설명 | 기본값 |
|------|------|--------|
| `CLITRACE_DEBUG` | 디버그 로그 (1=normal, 2=verbose) | 0 |

> 모든 설정은 `clitrace settings` TUI 또는 CLI `--db-path` 인자로 관리한다. 환경변수는 디버그 로그에만 사용.

## Project Structure

```
├── Cargo.toml
├── README.md                              # 이 파일
├── docs/
│   ├── DESIGN.md                          # 아키텍처 및 내부 설계
│   ├── USAGE.md                           # 상세 사용법 및 출력 형식
│   └── claude-code-jsonl-format.md        # Claude Code JSONL 구조 참고
├── specs/
│   └── constitution.md                    # 프로젝트 원칙 (Constitution)
├── Comparison.md                          # ccusage 성능 비교
└── src/
    ├── lib.rs                             # Public API: start(), Handle
    ├── main.rs                            # CLI 바이너리 (clap)
    ├── config.rs                          # Config + 환경변수/DB 우선순위
    ├── db.rs                              # fjall 래퍼 (7 keyspaces)
    ├── engine.rs                          # TrackerEngine: cold_start + watch_loop
    ├── writer.rs                          # DB writer thread (DbOp channel)
    ├── query.rs                           # TSDB 쿼리 (report용)
    ├── retention.rs                       # 데이터 보존 정책 (자동 삭제)
    ├── checkpoint.rs                      # 역순 라인 스캔, xxHash3 매칭
    ├── pricing.rs                         # LiteLLM 가격 fetch, ETag 캐싱
    ├── settings.rs                        # cursive TUI 설정 페이지
    ├── common/
    │   └── types.rs                       # 공통 타입, trait 정의
    ├── daemon/                            # 데몬 서버 컴포넌트
    │   ├── mod.rs                         # default_sock_path, stop/status
    │   ├── broadcast.rs                   # BroadcastSink (zero overhead fan-out)
    │   ├── listener.rs                    # UDS accept loop
    │   └── pidfile.rs                     # PID 파일 관리
    ├── sink/                              # 출력 추상화 (Sink trait)
    │   ├── mod.rs                         # Sink trait, MultiSink
    │   ├── json.rs                        # JSON 직렬화 (공용)
    │   ├── print.rs                       # PrintSink (table/json → stdout)
    │   ├── uds.rs                         # UdsSink (NDJSON → UDS)
    │   └── http.rs                        # HttpSink (JSON POST)
    ├── providers/
    │   └── claude_code/parser.rs          # JSONL 파싱 + 세션 디스커버리
    └── platform/
        └── macos/mod.rs                   # macOS FSEvents 감시
```

## Tech Stack

| 용도 | 선택 | 근거 |
|------|------|------|
| DB | fjall 3.x | Pure Rust LSM-tree, TSDB keyspace 구조에 적합 |
| 동시성 | std::thread + crossbeam-channel | 런타임 충돌 없음, 라이브러리 안전 |
| 병렬 스캔 | rayon | cold start 세션 파일 병렬 처리 |
| 파일 감시 | notify 6.x | macOS FSEvents 자동 사용 |
| 직렬화 | bincode 1.x (DB), serde_json (JSONL) | 바이너리 최소 오버헤드 |
| 해시 | xxhash-rust 0.8 (xxh3) | 체크포인트 줄 식별 (30GB/s) |
| HTTP | ureq 2.x | 동기 HTTP, ETag 조건부 요청 |
| CLI | clap 4.x | 서브커맨드, 글로벌 옵션 지원 |
| 테이블 | comfy-table 7.1 | Unicode 테이블 렌더링 |
| IPC | Unix Domain Socket | 데몬-클라이언트 NDJSON 스트리밍 |
