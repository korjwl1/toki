# clitrace

Claude Code CLI의 JSONL 세션 로그를 파일 시스템 이벤트 기반으로 감시하여, 모델별 토큰 사용량을 실시간 추적하는 Rust 라이브러리 모듈.

## Quick Start

### 바이너리로 실행

```bash
cd module/clitrace
cargo run --release -- trace
```

기본 설정으로 `~/.claude/projects/` 스캔 후 watch mode 진입.
`trace` 모드는 동일 DB 경로 기준 단일 인스턴스만 허용한다.
전체 재스캔이 필요하면 `--full-rescan`을 사용한다.

```bash
# 시작 시 일별 그룹핑으로 요약 출력
cargo run --release -- trace --startup-group-by day

# 시작 시 시간별 그룹핑 (체크포인트 필수, --full-rescan 불가)
cargo run --release -- trace --startup-group-by hour
```

Trace 옵션:
- `--startup-group-by hour|day|week|month|year`: cold start 시 시간 단위 그룹핑 요약
  - `hour`는 기존 체크포인트가 있어야 사용 가능 (증분 데이터만 출력)
  - `hour`는 `--full-rescan`과 함께 사용 불가
- `--session-id <PREFIX>`: 세션 UUID 접두사로 필터링
- `--project <NAME>`: 프로젝트 디렉토리 이름으로 필터링 (서브스트링 매치)

### 글로벌 옵션

- `--output-format table|json`: print sink의 출력 형식 (기본: table)
- `--sink <SPEC>`: 출력 대상 (기본: print). 복수 지정 가능
  - `print` — 터미널 출력 (`--output-format`에 따라 table/json)
  - `uds://<path>` — Unix Domain Socket으로 NDJSON 전송
  - `http://<url>` — HTTP POST로 JSON 전송 (5초 timeout)
  - print 외 sink은 자동 JSON 출력
- `--timezone <IANA>` / `-z <IANA>`: 타임존 지정 (기본: UTC)
  - 예: `-z Asia/Seoul`, `-z US/Eastern`, `-z Europe/London`
  - 버킷팅(일별/시간별 등)과 `--since`/`--until` 해석에 적용
- `--no-cost`: 비용 계산 비활성화 (가격 fetch 스킵)

### Report 모드 (one-shot)

```bash
cargo run --release -- report
cargo run --release -- report --since 20260301
cargo run --release -- report --since 20260301 --until 20260331
cargo run --release -- report monthly
cargo run --release -- report monthly --since 20260301
cargo run --release -- report yearly
cargo run --release -- report daily --since 20260301
cargo run --release -- report daily --from-beginning
cargo run --release -- report weekly --since 20260301 --start-of-week tue
cargo run --release -- report hourly --since 20260301
cargo run --release -- report hourly --from-beginning

# 프로젝트별 필터링
cargo run --release -- report --project clitrace
cargo run --release -- report --project ddleague daily --since 20260301

# 세션별 그룹핑/필터링
cargo run --release -- report --group-by-session
cargo run --release -- report --session-id 4de9291e

# 타임존 지정 (KST 기준 일별 리포트)
cargo run --release -- -z Asia/Seoul report daily --since 20260301

# 비용 표시 없이 리포트
cargo run --release -- --no-cost report
```

Report 옵션:
- 서브커맨드 없이 실행하면 전체 총합 출력 (`--since`/`--until` 선택적)
- 서브커맨드: `daily | weekly | monthly | yearly | hourly`
  - `hourly`, `daily`, `weekly`는 `--since` 또는 `--from-beginning` 필수
  - `monthly`, `yearly`는 제한 없음
  - `--start-of-week`는 `weekly`에서만 사용 가능
- `--since` (inclusive, UTC, `>=`): `YYYYMMDD` 또는 `YYYYMMDDhhmmss`
  - `YYYYMMDD`는 해당 날짜의 `00:00:00` UTC로 해석
- `--until` (inclusive, UTC, `<=`): `YYYYMMDD` 또는 `YYYYMMDDhhmmss`
  - `YYYYMMDD`는 해당 날짜의 `23:59:59` UTC로 해석
- `--from-beginning`: `--since` 없이 전체 데이터 그룹핑 허용
- `--project <NAME>`: 프로젝트 디렉토리 이름으로 필터링 (서브스트링 매치)
  - 예: `--project clitrace`는 `clitrace`가 포함된 프로젝트만 조회
  - 예: `--project ddleague`는 `ddleague`, `ddleague-module`, `ddleague-module-clitrace` 등 모두 포함
- `--session-id <PREFIX>`: 세션 UUID 접두사로 필터링
- `--group-by-session`: 세션별로 그룹핑 (시간 기반 서브커맨드와 함께 사용 불가)

### 비용 계산 (Cost)

기본적으로 모든 출력에 모델별 추정 비용(USD)이 표시된다.
가격 데이터는 [LiteLLM](https://github.com/BerriAI/litellm)의 커뮤니티 유지 가격표에서 가져온다.

- **최초 실행**: LiteLLM JSON 다운로드 → Claude 모델만 추출 → DB에 캐시
- **이후 실행**: HTTP ETag 조건부 요청 → 변경 없으면 304 응답 (바디 없이 ~50ms)
- **오프라인**: 캐시된 가격 데이터로 동작, 캐시 없으면 Cost 컬럼 생략
- **`--no-cost`**: 가격 fetch 자체를 스킵, Cost 컬럼 미표시
- **`--full-rescan`**: 체크포인트만 초기화, 가격 캐시는 보존

가격은 현재 시점 기준으로 전체 데이터에 일괄 적용된다 (시간대별 역사적 가격 추적 없음).
API key 사용자에게는 실제 청구 금액에 가깝고, Max Plan 구독자에게는 참고용 추정치이다.

### 출력 형식 (`--output-format`) & Sink (`--sink`)

```bash
# 기본값: 테이블 출력 (print sink)
cargo run --release -- report weekly --from-beginning

# JSON 출력 (print sink)
cargo run --release -- report --output-format json weekly --from-beginning

# trace에서도 사용 가능 (이벤트가 NDJSON으로 출력)
cargo run --release -- trace --output-format json

# UDS 전송 (자동 JSON)
cargo run --release -- trace --sink uds:///tmp/clitrace.sock

# HTTP 전송 (자동 JSON, 5초 timeout)
cargo run --release -- trace --sink http://localhost:8080/v1/events

# 터미널 + HTTP 동시 출력 (MultiSink)
cargo run --release -- trace --sink print --sink http://localhost:8080/events

# report에서도 sink 사용 가능
cargo run --release -- report --sink http://localhost:8080/report
```

`--output-format`은 print sink에만 적용되며, UDS/HTTP sink은 항상 JSON으로 전송한다.
`--sink`과 `--output-format`은 글로벌 옵션으로, `report`/`trace` 앞이나 뒤 어디에나 위치할 수 있다.

### 환경변수 오버라이드

```bash
CLITRACE_CLAUDE_ROOT=/path/to/custom/.claude cargo run --release -- trace
CLITRACE_DB_PATH=/path/to/custom.db cargo run --release -- trace
CLITRACE_DEBUG=1 cargo run --release -- trace   # 디버그 로그 (상태 전이, 이벤트, 타이밍)
CLITRACE_DEBUG=2 cargo run --release -- trace   # 레벨 1 + verbose (size unchanged, no new lines 스킵 로그)
```

### 라이브러리로 사용

```toml
# Cargo.toml
[dependencies]
clitrace = { path = "../module/clitrace" }
```

```rust
use clitrace::{Config, start};
use clitrace::sink::{PrintSink, OutputFormat};

fn main() {
    let config = Config::new()
        .with_claude_root("/custom/path/.claude".to_string());

    let sink = Box::new(PrintSink::new(OutputFormat::Table));
    let handle = start(config, None, sink, false).expect("Failed to start clitrace");

    // ... 호스트 애플리케이션 로직 ...

    handle.stop(); // 또는 handle이 drop되면 자동 종료
}
```

## 출력 예시

### Table (기본)

```
[clitrace] Token Usage Summary
┌───────────────────────────┬─────────┬─────────┬────────────┬──────────────┬──────────────┬────────┬─────────┐
│ Model                     ┆ Input   ┆ Output  ┆ Cache      ┆ Cache        ┆ Total        ┆ Events ┆ Cost    │
│                           ┆         ┆         ┆ Create     ┆ Read         ┆ Tokens       ┆        ┆ (USD)   │
╞═══════════════════════════╪═════════╪═════════╪════════════╪══════════════╪══════════════╪════════╪═════════╡
│ claude-opus-4-6           ┆ 1,234   ┆ 4,321   ┆ 56,789     ┆ 98,765       ┆ 161,109      ┆ 42     ┆ $1.21   │
├╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌┤
│ claude-haiku-4-5-20251001 ┆ 567     ┆ 2,100   ┆ 12,345     ┆ 34,567       ┆ 49,579       ┆ 18     ┆ $0.0234 │
├╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌┤
│ Total                     ┆ 1,801   ┆ 6,421   ┆ 69,134     ┆ 133,332      ┆ 210,688      ┆ 60     ┆ $1.23   │
└───────────────────────────┴─────────┴─────────┴────────────┴──────────────┴──────────────┴────────┴─────────┘
```

### JSON (`--output-format json`)

Summary:
```json
{
  "type": "summary",
  "data": [
    {
      "model": "claude-opus-4-6",
      "input_tokens": 1234,
      "output_tokens": 4321,
      "cache_creation_input_tokens": 56789,
      "cache_read_input_tokens": 98765,
      "total_tokens": 161109,
      "events": 42,
      "cost_usd": 1.2345
    }
  ]
}
```

Grouped (daily, weekly, ...):
```json
{
  "type": "daily",
  "data": [
    {
      "period": "2026-03-01",
      "usage_per_models": [
        {
          "model": "claude-opus-4-6",
          "input_tokens": 1234,
          "output_tokens": 4321,
          "cache_creation_input_tokens": 56789,
          "cache_read_input_tokens": 98765,
          "total_tokens": 161109,
          "events": 42,
          "cost_usd": 1.2345
        }
      ]
    }
  ]
}
```

### Watch Mode (실시간 이벤트)

Table:
```
[clitrace] claude-opus-4-6 | session.jsonl | in:3 cc:5139 cr:9631 out:14 | $0.0112
```

JSON (NDJSON, 한 줄씩):
```json
{"type":"event","data":{"model":"claude-opus-4-6","source":"4de9291e","input_tokens":3,"output_tokens":14,"cache_creation_input_tokens":5139,"cache_read_input_tokens":9631,"cost_usd":0.0112}}
```

## Architecture

### Thread Model

```mermaid
flowchart TB
    subgraph Host["Host Application / main.rs"]
        start["clitrace::start(config)"]
        handle["Handle"]
        stop["handle.stop()"]
        start --> handle
        handle -.-> stop
    end

    start -->|spawns| NT
    start -->|spawns| WT

    subgraph NT["notify thread (FSEvents)"]
        fsevents["macOS FSEvents"]
        callback["on_event callback"]
        fsevents --> callback
    end

    subgraph WT["worker thread"]
        loop["select! loop"]
        recv["event_rx"]
        process["process_file()"]
        poll["backup poll (glob)"]
        db_write["db.upsert_checkpoint()"]
        loop --> recv
        recv -->|"Ok(path)"| process
        process --> db_write
        loop -->|"default(30s)"| poll
    end

    callback -->|"tx.send(path)"| recv

    subgraph DB["redb (clitrace.db)"]
        checkpoints["checkpoints table"]
        settings["settings table"]
    end

    db_write --> checkpoints
    poll --> process

    stop -->|"stop_tx.send()"| loop
```

### Cold Start 병렬 처리

```mermaid
flowchart LR
    discover["discover_sessions()"] --> sessions["SessionGroup[]"]

    sessions --> scope["std::thread::scope"]

    subgraph scope["Scoped Threads (semaphore = num_cpus)"]
        direction TB
        s1["Session A"]
        s2["Session B"]
        s3["Session C"]

        subgraph s1["Session A"]
            p1["parent.jsonl"]
            sub1["agent-1.jsonl"]
            sub2["agent-2.jsonl"]
        end

        subgraph s2["Session B"]
            p2["parent.jsonl"]
        end

        subgraph s3["Session C"]
            p3["parent.jsonl"]
            sub3["agent-3.jsonl"]
        end
    end

    scope --> sink["sink.emit_summary() / sink.emit_grouped()"]
    scope --> flush2["flush_checkpoints()"]
```

### Active/Idle 파일 분류 & 체크포인트

macOS FSEvents가 디렉토리 내 파일 하나가 변해도 같은 디렉토리의 모든 파일에 이벤트를 발생시키므로, 파일별 active/idle 상태를 추적하여 불필요한 처리를 최소화한다.

```mermaid
flowchart TD
    start2["process_file(path)"] --> state{"FileActivity\n존재?"}

    state -->|No| new_active["새 파일 → Active"]
    state -->|Yes| idle_check{"now - last_active\n> 15s?"}

    idle_check -->|Yes| demote["Active → Idle 전환"]
    idle_check -->|No| keep["상태 유지"]

    new_active --> cooldown
    demote --> cooldown
    keep --> cooldown

    cooldown{"쿨다운 체크\nActive: 150ms\nIdle: 500ms"} -->|"too soon"| skip_cd["즉시 return"]
    cooldown -->|"passed"| size_check{"stat()\nsize 변화?"}

    size_check -->|No| skip["스킵 (stat only)"]
    size_check -->|Yes| find["find_resume_offset()\n+ process_lines_streaming()"]

    find --> new_lines{"새 줄\n있음?"}

    new_lines -->|No| skip2["상태 유지, return"]
    new_lines -->|Yes| parse["parse + checkpoint"]

    parse --> promote["→ Active 승격\n(Idle이었으면 promote 로그)"]
```

| 상수 | 값 | 역할 |
|------|----|------|
| `ACTIVE_COOLDOWN` | 150ms | Active 파일 재처리 최소 간격 |
| `IDLE_COOLDOWN` | 500ms | Idle 파일 stat() 최소 간격 |
| `IDLE_TRANSITION` | 15s | 새 줄 없이 경과 시 Idle로 전환 |

**파일 크기 기반 fast skip**:
- watch 이벤트 수신 시 `stat()`으로 파일 크기만 확인 (파일 open/read 없음)
- 크기 변화 없으면 즉시 스킵 (~1-5µs vs 기존 ~150-300µs)
- JSONL 특성상 새 줄 추가 = 크기 증가, compaction = 크기 감소이므로 false negative 없음

**역순 스캔 알고리즘**:
- 파일 끝에서 4KB 청크 단위로 역순 읽기
- 라인 길이 pre-filter (O(1) 정수 비교, ~85% 후보 제거)
- 길이 일치 시에만 xxHash3-64 비교 (30GB/s)
- 청크 경계를 넘는 라인은 fragment 누적으로 처리
- Compaction으로 바이트 위치가 변해도 라인 해시로 복구

### 데이터 흐름

```mermaid
flowchart LR
    subgraph Source["~/.claude/projects/"]
        jsonl1["UUID.jsonl"]
        jsonl2["UUID/subagents/agent-*.jsonl"]
    end

    subgraph Parser["ClaudeCodeParser"]
        filter["type == assistant"]
        extract["4종 토큰 추출"]
    end

    subgraph Storage["redb"]
        cp["checkpoints\nfile_path → bincode"]
        st["settings\nkey → value\n(pricing_data, pricing_etag)"]
    end

    subgraph Pricing["Pricing"]
        litellm["LiteLLM JSON\n(GitHub)"]
        ptable["PricingTable"]
    end

    jsonl1 --> filter
    jsonl2 --> filter
    filter --> extract
    extract --> event["UsageEvent"]
    event --> summary["ModelUsageSummary"]
    event --> cp
    litellm -->|"ETag caching"| st
    st --> ptable
    ptable -->|"cost_usd"| summary

    subgraph Sink["Sink (출력)"]
        print["PrintSink\n(table/json → stdout)"]
        uds["UdsSink\n(NDJSON → UDS)"]
        http["HttpSink\n(JSON POST)"]
    end

    summary --> print
    summary --> uds
    summary --> http
```

## Data Model

### UsageEvent

| 필드 | 타입 | 설명 |
|------|------|------|
| `event_key` | String | `{message.id}:{timestamp}` |
| `source_file` | String | 원본 JSONL 파일 경로 |
| `model` | String | `claude-opus-4-6` 등 |
| `input_tokens` | u64 | 캐시 미적용 입력 토큰 |
| `cache_creation_input_tokens` | u64 | 캐시 생성 입력 토큰 |
| `cache_read_input_tokens` | u64 | 캐시 읽기 입력 토큰 |
| `output_tokens` | u64 | 출력 토큰 |

### FileCheckpoint

| 필드 | 타입 | 설명 |
|------|------|------|
| `file_path` | String | JSONL 파일 절대 경로 (key) |
| `last_line_len` | u64 | 마지막 처리 줄의 바이트 길이 (pre-filter용) |
| `last_line_hash` | u64 | 마지막 처리 줄의 xxHash3-64 해시 |

### Config

| 필드 | 타입 | 기본값 | 설명 |
|------|------|--------|------|
| `claude_code_root` | String | `~/.claude` | 루트 디렉토리 |
| `db_path` | PathBuf | `~/.config/clitrace/clitrace.db` | DB 파일 경로 |
| `full_rescan` | bool | false | 시작 시 체크포인트 초기화 |
| `session_filter` | Option\<String\> | None | 세션 UUID 접두사 필터 |
| `project_filter` | Option\<String\> | None | 프로젝트 이름 서브스트링 필터 |
| `tz` | Option\<Tz\> | None | 타임존 (IANA 이름, 버킷팅/필터에 적용) |

설정 우선순위: **환경변수** > **DB settings 테이블** > **기본값**

## Project Structure

```
module/clitrace/
├── Cargo.toml
├── README.md
├── src/
│   ├── lib.rs                          # Public API: start(), Handle, Config
│   ├── main.rs                         # 참조 바이너리 (Ctrl+C 핸들링)
│   ├── config.rs                       # Config + 환경변수/DB 우선순위
│   ├── db.rs                           # redb 래퍼 (checkpoints + settings)
│   ├── engine.rs                       # TrackerEngine: cold_start + watch_loop
│   ├── pricing.rs                      # LiteLLM 가격 fetch, ETag 캐싱, 비용 계산
│   ├── checkpoint.rs                   # 역순 라인 스캔, xxHash3 매칭, JSON 완성도 검사
│   ├── common/
│   │   ├── mod.rs
│   │   └── types.rs                    # UsageEvent, FileCheckpoint, LogParser trait
│   ├── sink/                           # 출력 추상화 (Sink trait)
│   │   ├── mod.rs                      # Sink trait, MultiSink, create_sinks()
│   │   ├── json.rs                     # 공통 JSON 직렬화 (3개 sink 공유)
│   │   ├── print.rs                    # PrintSink: table/json → stdout
│   │   ├── uds.rs                      # UdsSink: NDJSON over Unix Domain Socket
│   │   └── http.rs                     # HttpSink: JSON POST (5s timeout)
│   ├── providers/
│   │   ├── mod.rs
│   │   ├── claude_code/
│   │   │   ├── mod.rs
│   │   │   └── parser.rs              # JSONL 파싱 + 세션 디스커버리
│   │   ├── gemini/
│   │   │   └── mod.rs                 # TODO
│   │   └── codex/
│   │       └── mod.rs                 # TODO
│   └── platform/
│       ├── mod.rs                     # create_watcher(), watch_directory()
│       ├── macos/mod.rs               # macOS 기본 경로
│       ├── windows/mod.rs             # TODO
│       └── linux/mod.rs               # TODO
└── tests/                             # 68+ unit tests (cargo test)
```

## Tech Stack

| 용도 | 선택 | 근거 |
|------|------|------|
| DB | redb 2.x | Pure Rust, C 의존성 없음, key-value에 적합 |
| 동시성 | std::thread + crossbeam-channel | 런타임 충돌 없음, 라이브러리 안전 |
| 파일 감시 | notify 6.x | macOS FSEvents 자동 사용 |
| 직렬화 | bincode 1.x (checkpoint), serde_json (JSONL) | 바이너리 최소 오버헤드 |
| 해시 | xxhash-rust 0.8 (xxh3) | 체크포인트 줄 식별 (30GB/s, 비암호화) |
| HTTP | ureq 2.x | 동기 HTTP client, ETag 조건부 요청 |
| 테이블 출력 | comfy-table 7.1 | Unicode 테이블 렌더링 |

## JSONL 구조 참고

Claude Code는 `~/.claude/projects/<encoded-path>/` 하위에 세션 로그를 저장한다.

```
~/.claude/projects/-Users-user-Documents-project/
├── 4de9291e-061e-414a-85cb-de615826aded.jsonl        # 부모 세션
├── 4de9291e-061e-414a-85cb-de615826aded/
│   └── subagents/
│       └── agent-aed1da92cc2e4e9e7.jsonl             # 서브에이전트
└── db7cd31e-fdb1-4767-a6a2-f2f3dc68a74b.jsonl        # 다른 세션
```

JSONL 줄 타입:
- `file-history-snapshot` — 무시
- `user` — 무시
- **`assistant`** — 파싱 대상 (`message.usage`에 4종 토큰)

서브에이전트 토큰은 부모에 포함되지 않으며 별도 파일에 기록된다 (전체의 ~16%).
