# clitrace Architecture & Design

## Overview

clitrace는 3개 스레드 + bounded channel 아키텍처로 동작한다.
데이터는 fjall 임베디드 DB의 7개 keyspace에 저장되며, writer thread가 DB를 단독 소유한다.

## Thread Model

```
┌─────────────────────────────────────────────────────────┐
│ Host Application / main.rs                              │
│  start(config) → Handle                                 │
│  handle.stop() → graceful shutdown                      │
└──────────┬──────────────────────────────────────────────┘
           │ spawns
           ├──────────────────────────────┐
           ▼                              ▼
┌─────────────────────┐    ┌─────────────────────────────┐
│ Worker Thread       │    │ Writer Thread               │
│                     │    │                             │
│ select! {           │    │ select! {                   │
│   event_rx → file   │    │   op_rx → handle_op()      │
│   stop_rx  → exit   │    │   tick(86400s) → retention  │
│   default(30s)→poll │    │ }                           │
│ }                   │    │                             │
│                     │    │ Owns: Database              │
│ db_tx.try_send()  ──┼───▶│ dict_cache: HashMap         │
│ db_tx.send()        │    │ pending_events: Vec (≤64)   │
└─────────────────────┘    └─────────────────────────────┘
        ▲
        │ event_tx.send(path)
┌───────┴─────────────┐
│ Notify Thread       │
│ macOS FSEvents      │
└─────────────────────┘
```

### Worker Thread

- `crossbeam_channel::select!`로 FSEvents 이벤트, stop 시그널, 30초 백업 폴링을 다중화
- 파일 변경 감지 → `process_file_with_ts()` → 파싱된 이벤트를 Sink에 출력 + `DbOp::WriteEvent`를 writer에 전송
- watch mode에서는 `try_send` 사용 (UI 스레드 blocking 방지)
- 5초 간격으로 dirty checkpoints를 writer에 batch flush

### Writer Thread

- `Database`를 단독 소유 — Send 이슈 없음, 단일 스레드 접근
- `DbOp` 수신 → pending에 축적 → 64개 도달 시 batch commit
- Dictionary cache를 메모리에 유지 (일반 HashMap, DashMap 불필요)
- 일 1회 retention tick으로 오래된 데이터 자동 삭제
- Shutdown 시 잔여 pending events flush 후 종료

### Startup Sequence

```
1. Database::open() + load_all_checkpoints()
2. (db_tx, db_rx) = bounded(1024)
3. Writer thread spawn (Database 소유권 이전)
4. TrackerEngine::new(db_tx, checkpoints)
5. Cold start: 전체 세션 파일 스캔 → TSDB에 이벤트 저장
6. Watcher + Worker thread spawn
```

### Shutdown Sequence

```
1. stop_tx.send() → Worker thread 종료 (잔여 checkpoints flush)
2. db_tx.send(Shutdown) → Writer thread 종료 (잔여 events flush)
3. Worker thread join → Writer thread join
```

## TSDB Schema

fjall의 7개 keyspace:

| Keyspace | Key | Value | 용도 |
|----------|-----|-------|------|
| `checkpoints` | file_path (string) | bincode(FileCheckpoint) | 증분 읽기 위치 |
| `meta` | key (string) | value (string) | 설정, 가격 캐시 |
| `events` | `[ts_ms BE:8][message_id]` | bincode(StoredEvent) | 개별 이벤트 |
| `rollups` | `[hour_ts BE:8][model_name]` | bincode(RollupValue) | 시간별 모델 집계 |
| `idx_sessions` | `{session_id}\0[ts:8][msg_id]` | empty | 세션 인덱스 |
| `idx_projects` | `{project}\0[ts:8][msg_id]` | empty | 프로젝트 인덱스 |
| `dict` | string | bincode(u32) | 문자열 → ID 딕셔너리 압축 |

- Big-endian timestamp → lexicographic = chronological 정렬
- Range scan으로 시간 범위 쿼리 가능
- Index keyspace는 value가 empty — 키 존재 여부만으로 lookup

### Dictionary Compression

반복되는 문자열(model, session_id, source_file)을 u32 ID로 압축하여 `events` keyspace의 value 크기를 줄인다.

- `dict` keyspace: `"claude-opus-4-6"` → `1`, `"session-abc"` → `2`
- Writer thread가 dict_cache를 메모리에 유지, 새 문자열은 자동 등록
- 역방향 조회(ID → string)는 `load_dict_reverse()`로 report 시에만 사용

### Rollup-on-Write

이벤트 저장 시 시간별 rollup도 동시에 갱신한다 (read-modify-write):

```
hour_ts = ts_ms - (ts_ms % 3_600_000)  // 시간 단위 절삭
key = (hour_ts, model_name)
rollup = db.get_rollup(key) or default
rollup += event tokens
batch.upsert_rollup(key, rollup)
```

Report에서 일별/월별 등 시간 그룹핑은 rollup keyspace만 스캔하면 되므로
전체 이벤트를 읽을 필요가 없다.

### Batch Transaction

Writer thread는 64개 이벤트를 모아서 단일 `OwnedWriteBatch`로 commit한다:

```
1. Drain pending_events
2. Rollup read-modify-write (hour별 기존값 읽기 → 누적)
3. Dict ID 해석 (cache hit → 0 alloc, miss → dict keyspace 추가)
4. events, idx_sessions, idx_projects, rollups 일괄 insert
5. batch.commit()
```

## Data Flow

### Cold Start (trace 시작)

```
discover_sessions()
    → SessionGroup[] (parent.jsonl + subagent/*.jsonl)
    → rayon parallel_scan (CPU 코어 수 제한)
        → process_lines_streaming() per file
        → parse_line_with_ts() → UsageEventWithTs
        → db_tx.send(WriteEvent)     ← blocking send (데이터 무손실)
        → accumulate to local HashMap
    → merge summaries
    → sink.emit_summary() / sink.emit_grouped()
    → db_tx.send(FlushCheckpoints)
```

Cold start에서는 blocking `send`를 사용한다.
rayon 스레드가 bounded channel(1024)을 채우면 writer가 소화할 때까지 대기하여
데이터 무손실을 보장한다.

### Watch Mode (실시간)

```
FSEvents → event_tx → Worker thread
    → stat() 크기 비교 (1-5µs fast skip)
    → find_resume_offset() 역순 스캔
    → process_lines_streaming() 증분 읽기
    → parse_line_with_ts()
    → sink.emit_event()              ← 실시간 출력
    → db_tx.try_send(WriteEvent)     ← non-blocking (채널 풀 시 drop)
```

Watch mode에서는 `try_send`를 사용한다.
Worker thread가 block되면 UI 출력이 지연되므로, 채널이 가득 차면 드롭한다.
(실사용 시 초당 이벤트가 수 개 수준이므로 채널이 차는 경우는 거의 없다.)

### Report (one-shot 조회)

```
DB open → has_any_rollups()? (O(1) 확인)
    → Yes: TSDB 쿼리 (for_each_rollup 스트리밍)
    → No:  JSONL 파일 직접 스캔 (fallback)
```

Report는 writer thread 없이 DB를 직접 읽기 전용으로 연다.
스트리밍 콜백 패턴으로 중간 Vec 할당 없이 집계한다.

## File Processing Pipeline

### Active/Idle 분류

macOS FSEvents는 디렉토리 내 파일 하나가 변해도 같은 디렉토리의 모든 파일에
이벤트를 발생시킨다. 파일별 상태를 추적하여 불필요한 처리를 최소화한다.

| 상수 | 값 | 역할 |
|------|----|------|
| `ACTIVE_COOLDOWN` | 150ms | Active 파일 재처리 최소 간격 |
| `IDLE_COOLDOWN` | 500ms | Idle 파일 stat() 최소 간격 |
| `IDLE_TRANSITION` | 15s | 새 줄 없이 경과 시 Idle 전환 |

```
process_file_with_ts(path)
    → FileActivity 존재?
        No  → Active (새 파일)
        Yes → 15s 경과? → Idle 전환
    → 쿨다운 체크 (Active: 150ms, Idle: 500ms)
    → stat() 크기 비교 (크기 변화 없으면 즉시 skip)
    → find_resume_offset() + process_lines_streaming()
    → 새 줄 있으면 파싱 + checkpoint 갱신 + Active 승격
```

### Fast Skip (크기 기반)

- watch 이벤트 수신 시 `stat()`으로 파일 크기만 확인 (파일 open/read 없음)
- 크기 변화 없으면 즉시 스킵 (~1-5µs vs 기존 ~150-300µs)
- JSONL 특성상 새 줄 추가 = 크기 증가이므로 false negative 없음

### 역순 스캔 (Checkpoint Recovery)

- 파일 끝에서 4KB 청크 단위로 역순 읽기
- 라인 길이 pre-filter (O(1) 정수 비교, ~85% 후보 제거)
- 길이 일치 시에만 xxHash3-64 비교 (30GB/s)
- Compaction으로 바이트 위치가 변해도 라인 해시로 복구

## Retention Policy

Writer thread가 데이터 보존 정책을 자동 실행한다:

| 대상 | 기본 보존 기간 | 환경변수 |
|------|----------------|----------|
| events | 90일 | `CLITRACE_RETENTION_DAYS` |
| rollups | 365일 | `CLITRACE_ROLLUP_RETENTION_DAYS` |

- 시작 시 1회 + 이후 24시간 간격으로 실행
- 1000개 키 단위로 batch 삭제 (대량 삭제 시 write stall 방지)
- 인덱스(idx_sessions, idx_projects)는 삭제 생략
  - 키가 `{prefix}\0{ts}{msg_id}` 구조라 시간 순 정렬이 아님 → O(n) full scan 필요
  - 고아 인덱스 엔트리는 value가 empty이므로 크기 무시 가능

## Data Types

### StoredEvent (events keyspace value)

```rust
pub struct StoredEvent {
    pub model_id: u32,                    // dict compressed
    pub session_id: u32,                  // dict compressed
    pub source_file_id: u32,             // dict compressed
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}
```

### RollupValue (rollups keyspace value)

```rust
pub struct RollupValue {
    pub input: u64,
    pub output: u64,
    pub cache_create: u64,
    pub cache_read: u64,
    pub count: u64,
}
```

### FileCheckpoint (checkpoints keyspace value)

```rust
pub struct FileCheckpoint {
    pub file_path: String,
    pub last_line_len: u64,      // 라인 길이 pre-filter용
    pub last_line_hash: u64,     // xxHash3-64
}
```

### DbOp (Writer thread channel message)

```rust
pub enum DbOp {
    WriteEvent { ts_ms, message_id, model, session_id, source_file, tokens },
    WriteCheckpoint(FileCheckpoint),
    FlushCheckpoints(Vec<FileCheckpoint>),
    Shutdown,
}
```

## Query Architecture

Report 명령은 DB를 읽기 전용으로 열어 직접 쿼리한다 (writer thread 불필요).

| 함수 | 데이터 소스 | 용도 |
|------|-------------|------|
| `report_summary_from_db` | rollups | 전체 요약 |
| `report_grouped_from_db` | rollups | 시간별 그룹핑 (daily/weekly/...) |
| `report_by_session_from_db` | events + dict | 세션별 그룹핑 |
| `has_tsdb_data` | rollups (O(1)) | TSDB 데이터 존재 여부 확인 |

- 스트리밍 콜백 패턴: `for_each_rollup(since, until, |ts, model, rollup| { ... })`
- 중간 Vec 할당 없이 HashMap에 직접 accumulate
- `has_tsdb_data`는 `first_key_value().is_some()`으로 O(1)

### TSDB vs JSONL Fallback

```
Report 명령 실행
  → DB open 시도
  → has_tsdb_data()?
      Yes → TSDB 쿼리 (~ms)
      No  → JSONL 파일 직접 스캔 (~seconds, cold_start_report 계열 함수)
```

TSDB에 데이터가 있으면 TSDB를 사용하고, 없으면(trace를 한 번도 실행하지 않은 경우)
JSONL 파일을 직접 스캔하는 fallback 경로가 있다.

## Config Priority

설정 값은 다음 우선순위로 결정된다:

```
CLI 인자 > 환경변수 > DB settings > 기본값
```

| 설정 | CLI | 환경변수 | DB key | 기본값 |
|------|-----|----------|--------|--------|
| Claude root | `--claude-root` | `CLITRACE_CLAUDE_ROOT` | `claude_code_root` | `~/.claude` |
| DB path | `--db-path` | `CLITRACE_DB_PATH` | - | `~/.config/clitrace/clitrace.fjall` |
| Retention | - | `CLITRACE_RETENTION_DAYS` | - | 90 |
| Rollup retention | - | `CLITRACE_ROLLUP_RETENTION_DAYS` | - | 365 |

## Backpressure

Engine → Writer 간 bounded channel(1024):

| 상황 | 동작 |
|------|------|
| Cold start (rayon parallel scan) | `send()` — blocking. 데이터 무손실 보장 |
| Watch mode (실시간 이벤트) | `try_send()` — non-blocking. 채널 풀 시 drop + debug log |
| Checkpoints flush | `send()` — blocking. 체크포인트 손실 방지 |

Watch mode에서 drop이 발생해도 다음 번 파일 변경 시 증분 읽기로 복구된다.
