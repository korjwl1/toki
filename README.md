# webtrace

Claude Code CLI의 JSONL 세션 로그를 파일 시스템 이벤트 기반으로 감시하여, 모델별 토큰 사용량을 실시간 추적하는 Rust 라이브러리 모듈.

## Quick Start

### 바이너리로 실행

```bash
cd module/webtrace
cargo run --release
```

기본 설정으로 `~/.claude/projects/` 스캔 후 watch mode 진입.

### 환경변수 오버라이드

```bash
WEBTRACE_CLAUDE_ROOT=/path/to/custom/.claude cargo run --release
WEBTRACE_DB_PATH=/path/to/custom.db cargo run --release
WEBTRACE_DEBUG=1 cargo run --release   # 성능 타이밍 로그 출력
```

### 라이브러리로 사용

```toml
# Cargo.toml
[dependencies]
webtrace = { path = "../module/webtrace" }
```

```rust
use webtrace::{Config, start};

fn main() {
    let config = Config::new()
        .with_claude_root("/custom/path/.claude".to_string());

    let handle = start(config).expect("Failed to start webtrace");

    // ... 호스트 애플리케이션 로직 ...

    handle.stop(); // 또는 handle이 drop되면 자동 종료
}
```

## 출력 예시

### Cold Start (모델별 요약)

```
[webtrace] ═══════════════════════════════════════════
[webtrace] Token Usage Summary
[webtrace] ───────────────────────────────────────────
[webtrace] Model: claude-opus-4-6
[webtrace]   Input:        1,234 | Cache Create:       56,789
[webtrace]   Cache Read:  98,765 | Output:              4,321
[webtrace]   Events: 42
[webtrace] ───────────────────────────────────────────
[webtrace] Model: claude-haiku-4-5-20251001
[webtrace]   Input:          567 | Cache Create:       12,345
[webtrace]   Cache Read:  34,567 | Output:              2,100
[webtrace]   Events: 18
[webtrace] ═══════════════════════════════════════════
```

### Watch Mode (실시간 이벤트)

```
[webtrace] claude-opus-4-6 | session.jsonl | in:3 cc:5139 cr:9631 out:14
```

## Architecture

### Thread Model

```mermaid
flowchart TB
    subgraph Host["Host Application / main.rs"]
        start["webtrace::start(config)"]
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

    subgraph DB["redb (webtrace.db)"]
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

    scope --> merge["Merge → ModelUsageSummary"]
    merge --> print["print_summary()"]
    merge --> flush2["flush_checkpoints()"]
```

### 체크포인트 무결성

```mermaid
flowchart TD
    start2["process_file(path)"] --> has_cp{"checkpoint\nexists?"}

    has_cp -->|No| case1["offset = 0\n전체 읽기"]
    has_cp -->|Yes| find["find_resume_offset()\n역순 라인 스캔"]

    find -->|Found| case2["offset = matched + 1\n증분 읽기"]
    find -->|Not Found| case1

    case1 --> read["read_lines_from(offset)\n불완전 마지막 줄: bracket-depth 검사"]
    case2 --> read

    read --> parse["parse_line() × N"]
    parse --> update["db.upsert_checkpoint()\n즉시 저장"]
```

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
        st["settings\nkey → value"]
    end

    jsonl1 --> filter
    jsonl2 --> filter
    filter --> extract
    extract --> event["UsageEvent"]
    event --> summary["ModelUsageSummary"]
    event --> cp
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
| `db_path` | PathBuf | `~/.config/webtrace/webtrace.db` | DB 파일 경로 |
| `poll_interval_secs` | u64 | 30 | 백업 폴링 간격 |

설정 우선순위: **환경변수** > **DB settings 테이블** > **기본값**

## Project Structure

```
module/webtrace/
├── Cargo.toml
├── README.md
├── src/
│   ├── lib.rs                          # Public API: start(), Handle, Config
│   ├── main.rs                         # 참조 바이너리 (Ctrl+C 핸들링)
│   ├── config.rs                       # Config + 환경변수/DB 우선순위
│   ├── db.rs                           # redb 래퍼 (checkpoints + settings)
│   ├── engine.rs                       # TrackerEngine: cold_start + watch_loop
│   ├── checkpoint.rs                   # 역순 라인 스캔, xxHash3 매칭, JSON 완성도 검사
│   ├── common/
│   │   ├── mod.rs
│   │   └── types.rs                    # UsageEvent, FileCheckpoint, LogParser trait
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
└── tests/                             # 58+ unit tests (cargo test)
```

## Tech Stack

| 용도 | 선택 | 근거 |
|------|------|------|
| DB | redb 2.x | Pure Rust, C 의존성 없음, key-value에 적합 |
| 동시성 | std::thread + crossbeam-channel | 런타임 충돌 없음, 라이브러리 안전 |
| 파일 감시 | notify 6.x | macOS FSEvents 자동 사용 |
| 직렬화 | bincode 1.x (checkpoint), serde_json (JSONL) | 바이너리 최소 오버헤드 |
| 해시 | xxhash-rust 0.8 (xxh3) | 체크포인트 줄 식별 (30GB/s, 비암호화) |

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
