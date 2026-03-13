# toki Usage Guide

## Installation

```bash
cargo build --release
# 바이너리: target/release/toki
```

## Commands

toki는 데몬/클라이언트 구조로 동작한다:
- **`daemon start`**: 서버 프로세스. cold start 후 파일 감시 + TSDB 저장
- **`daemon stop/status`**: 데몬 관리
- **`trace`**: 데몬에 연결하여 실시간 이벤트 스트림 수신
- **`report`**: one-shot TSDB 조회. 데몬이 수집한 데이터를 조회

## daemon

### daemon start

```bash
toki daemon start
```

1. `~/.claude/projects/` 하위의 모든 세션 파일을 스캔 (cold start)
2. 파싱된 이벤트를 TSDB에 저장
3. 전체 토큰 사용량 요약 출력
4. FSEvents 감시 모드 진입
5. UDS 리스너 시작 (trace 클라이언트 수신 대기)

#### Cold Start 그룹핑

시작 시 요약을 시간 단위로 그룹핑하여 출력:

```bash
toki daemon start --startup-group-by day      # 일별
toki daemon start --startup-group-by week     # 주별
toki daemon start --startup-group-by month    # 월별
toki daemon start --startup-group-by year     # 연별
toki daemon start --startup-group-by hour     # 시간별 (기존 체크포인트 필요)
```

`hour`는 증분 데이터만 출력하므로 이전에 한 번 이상 데몬을 실행한 적이 있어야 한다.
`--full-rescan`과 함께 사용할 수 없다.

#### 필터링

```bash
# 세션 UUID 접두사로 필터 (해당 세션의 이벤트만 출력/저장)
toki daemon start --session-id 4de9291e

# 프로젝트 디렉토리 이름으로 필터 (서브스트링 매치)
toki daemon start --project toki
```

#### 전체 재스캔

```bash
toki daemon start --full-rescan
```

체크포인트를 초기화하고 모든 파일을 처음부터 다시 읽는다.
가격 캐시는 보존된다.

#### 커스텀 경로

```bash
# DB 경로 (CLI 인자로만 지정 가능)
toki daemon start --db-path /custom/toki.fjall

# Claude root, daemon socket 등은 toki settings에서 설정
toki settings
```

#### 단일 인스턴스 제한

동일 DB 경로에 대해 하나의 데몬만 실행 가능하다.
이미 실행 중이면 `Daemon already running (PID xxx)` 메시지와 함께 종료된다.

### daemon stop

```bash
toki daemon stop
toki daemon stop --sock /custom/daemon.sock
```

실행 중인 데몬에 SIGTERM을 전송하여 graceful shutdown한다.
PID 파일과 소켓 파일을 정리한다.

### daemon status

```bash
toki daemon status
```

데몬의 실행 여부와 PID를 표시한다.

## trace

trace는 실행 중인 데몬에 UDS로 연결하여 실시간 이벤트를 수신하는 클라이언트 명령이다.

```bash
# 기본: 터미널에 실시간 출력
toki trace

# 커스텀 소켓 경로
toki trace --sock /custom/daemon.sock

# JSON 형식으로 출력
toki --output-format json trace

# HTTP로 중계
toki trace --sink http://localhost:8080/events

# 복수 sink
toki trace --sink print --sink http://localhost:8080/events
```

- 데몬이 실행 중이어야 한다 (`toki daemon start` 먼저)
- 복수 클라이언트가 동시에 연결할 수 있다 (fan-out)
- 클라이언트가 연결되어 있지 않으면 데몬의 Sink 처리는 완전 비활성화 (zero overhead)
- Ctrl+C로 종료. 데몬은 계속 실행된다

### 지원 sink 타입

| Sink | 설명 |
|------|------|
| `print` (기본) | 터미널 출력 (`--output-format`에 따라 table/json) |
| `uds://<path>` | 다른 Unix Domain Socket으로 NDJSON 중계 |
| `http://<url>` | HTTP POST로 JSON 중계 (5초 timeout) |

## report

데몬이 최소 1회 이상 실행되어 TSDB에 데이터가 수집된 상태여야 한다.
데이터가 없으면 "No data in TSDB" 메시지와 함께 데몬 시작을 안내한다.

### 전체 요약

```bash
toki report
toki report --since 20260301
toki report --since 20260301 --until 20260331
```

전체 기간 또는 지정 범위의 모델별 토큰 사용량 합계를 출력한다.

### 시간별 그룹핑

```bash
toki report daily --since 20260301
toki report daily --from-beginning
toki report weekly --since 20260301
toki report weekly --since 20260301 --start-of-week tue
toki report monthly
toki report yearly
toki report hourly --since 20260301
toki report hourly --from-beginning
```

| 서브커맨드 | `--since` 필수 | `--from-beginning` 가능 | 비고 |
|-----------|----------------|------------------------|------|
| `hourly` | O | O | |
| `daily` | O | O | |
| `weekly` | O | O | `--start-of-week` 사용 가능 |
| `monthly` | X | O | |
| `yearly` | X | O | |

`hourly`, `daily`, `weekly`는 데이터 양이 많을 수 있으므로 `--since` 또는 `--from-beginning`을 필수로 요구한다.

### --since / --until 형식

| 형식 | 예시 | 해석 |
|------|------|------|
| `YYYYMMDD` | `20260301` | `--since`: 00:00:00, `--until`: 23:59:59 |
| `YYYYMMDDhhmmss` | `20260301143000` | 정확한 시각 |

- `--timezone`이 지정되면 입력값을 해당 타임존의 로컬 시간으로 해석하여 UTC로 변환
- `--timezone`이 없으면 UTC로 해석

```bash
# UTC 기준
toki report daily --since 20260301

# KST 기준 (2026-03-01 00:00:00 KST = 2026-02-28 15:00:00 UTC)
toki -z Asia/Seoul report daily --since 20260301
```

### 세션별 그룹핑

```bash
toki report --group-by-session
toki report --group-by-session --since 20260301
```

시간 기반 서브커맨드(`daily`, `weekly` 등)와 동시에 사용할 수 없다.

### 필터링

`--session-id`와 `--project`는 Report의 모든 모드에서 사용할 수 있다.

```bash
# 프로젝트 필터 (서브스트링 매치)
toki report --project toki
toki report daily --since 20260301 --project ddleague
toki report monthly --project myapp

# 세션 필터 (UUID 접두사)
toki report --session-id 4de9291e
toki report --session-id 4de9 --group-by-session

# 조합
toki report --session-id abc --project myapp
toki report daily --since 20260301 --session-id abc
```

필터가 지정되면 rollup 대신 이벤트 레벨 스캔을 사용한다 (rollup에는 세션/프로젝트 정보가 없으므로).

### PromQL 스타일 쿼리

`report query` 서브커맨드로 PromQL에서 영감을 받은 자유 쿼리를 실행할 수 있다.

#### 문법

```
metric{filters}[bucket] by (dimensions)
```

| 요소 | 필수 | 설명 |
|------|------|------|
| `metric` | O | `usage`, `sessions`, `projects` |
| `{filters}` | X | `key="value"` 쌍, `,`로 구분 |
| `[bucket]` | X | 시간 버킷: `s`, `m`, `h`, `d`, `w` |
| `by (dims)` | X | 그룹 기준: `model`, `session`, `project` |

필터 키: `model`, `session`, `project`, `since`, `until`

#### 예시

```bash
# 전체 사용량 요약
toki report query 'usage'

# 모델 필터
toki report query 'usage{model="claude-opus-4-6"}'

# 1시간 버킷 + 모델별 그룹핑
toki report query 'usage{since="20260301"}[1h] by (model)'

# 세션별 그룹핑 + 시간 범위
toki report query 'usage{since="20260301", until="20260331"} by (session)'

# 프로젝트별 그룹핑
toki report query 'usage{project="myapp"} by (project)'

# 복합 그룹핑
toki report query 'usage[1d] by (model, session)'

# 세션 리스팅
toki report query 'sessions'
toki report query 'sessions{project="myapp"}'
toki report query 'sessions{since="20260301"}'

# 프로젝트 리스팅
toki report query 'projects'
toki report query 'projects{project="myapp"}'
```

## settings

`toki settings`는 cursive TUI로 설정 페이지를 연다. 모든 설정은 DB에 저장된다.

```bash
toki settings
toki settings --db-path /custom/toki.fjall
```

| 설정 항목 | DB key | 기본값 |
|-----------|--------|--------|
| Claude Code Root | `claude_code_root` | `~/.claude` |
| Daemon Socket | `daemon_sock` | `~/.config/toki/daemon.sock` |
| Timezone | `timezone` | (빈값 = UTC) |
| Output Format | `output_format` | `table` |
| Start of Week | `start_of_week` | `mon` |
| No Cost | `no_cost` | `false` |
| Retention Days | `retention_days` | `0` (무제한) |
| Rollup Retention Days | `rollup_retention_days` | `0` (무제한) |

설정 우선순위: **CLI 인자 > DB settings > 기본값**

환경변수는 사용하지 않는다 (`TOKI_DEBUG` 제외).

## Global Options

모든 명령에 적용되는 옵션 (DB 설정을 오버라이드):

### --output-format

```bash
toki --output-format table report          # 기본값
toki --output-format json report
toki --output-format json trace
```

`print` sink에만 적용된다. UDS/HTTP sink은 항상 JSON이다.

### --sink

```bash
# 기본: 터미널 출력
toki trace

# UDS 전송
toki trace --sink uds:///tmp/toki.sock

# HTTP 전송 (5초 timeout)
toki trace --sink http://localhost:8080/v1/events

# 복수 sink (터미널 + HTTP)
toki trace --sink print --sink http://localhost:8080/events

# report에서도 사용 가능
toki report --sink http://localhost:8080/report
```

### --timezone / -z

```bash
toki -z Asia/Seoul report daily --since 20260301
toki -z US/Eastern report weekly --from-beginning
toki -z Europe/London daemon start --startup-group-by day
```

적용 범위:
- `--since`/`--until` 입력값 해석
- 시간 버킷팅 (일별/시간별 등의 날짜 경계)

### --no-cost

```bash
toki --no-cost report
toki --no-cost daemon start
```

가격 데이터 fetch를 스킵하고 Cost 컬럼을 표시하지 않는다.

## Output Formats

### Table (기본)

#### 전체 요약

```
[toki] Token Usage Summary
┌───────────────────────────┬─────────┬─────────┬────────────┬──────────────┬──────────────┬────────┬─────────┐
│ Model                     ┆ Input   ┆ Output  ┆ Cache      ┆ Cache        ┆ Total        ┆ Events ┆ Cost    │
│                           ┆         ┆         ┆ Create     ┆ Read         ┆ Tokens       ┆        ┆ (USD)   │
╞═══════════════════════════╪═════════╪═════════╪════════════╪══════════════╪══════════════╪════════╪═════════╡
│ claude-opus-4-6           ┆ 1,234   ┆ 4,321   ┆ 56,789     ┆ 98,765       ┆ 161,109      ┆ 42     ┆ $1.21   │
├╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌┤
│ claude-haiku-4-5-20251001 ┆ 567     ┆ 2,100   ┆ 12,345     ┆ 34,567       ┆ 49,579       ┆ 18     ┆ $0.023  │
├╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌┤
│ Total                     ┆ 1,801   ┆ 6,421   ┆ 69,134     ┆ 133,332      ┆ 210,688      ┆ 60     ┆ $1.23   │
└───────────────────────────┴─────────┴─────────┴────────────┴──────────────┴──────────────┴────────┴─────────┘
```

#### 그룹핑 (daily, weekly, ...)

```
[toki] Usage by daily
─── 2026-03-01 ───
┌───────────────────────────┬─────────┬─────────┬────────────┬──────────────┬──────────────┬────────┬─────────┐
│ Model                     ┆ Input   ┆ Output  ┆ ...        ┆ ...          ┆ ...          ┆ Events ┆ Cost    │
...
─── 2026-03-02 ───
...
```

#### 세션/프로젝트 리스팅

```
[toki] sessions (3)
┌──────────────────────────────────────┐
│ Session ID                           │
╞══════════════════════════════════════╡
│ 4de9291e-061e-414a-85cb-de615826aded │
├╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌┤
│ db7cd31e-fdb1-4767-a6a2-f2f3dc68a74b │
├╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌┤
│ f1273bff-d1d8-45ae-a85e-624658132804 │
└──────────────────────────────────────┘
```

#### Watch Mode (실시간 이벤트, trace 클라이언트)

```
[toki] claude-opus-4-6 | session.jsonl | in:3 cc:5139 cr:9631 out:14 | $0.0112
```

### JSON (`--output-format json`)

#### Summary

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

#### Grouped

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

#### List (sessions/projects)

```json
{
  "type": "sessions",
  "items": [
    "4de9291e-061e-414a-85cb-de615826aded",
    "db7cd31e-fdb1-4767-a6a2-f2f3dc68a74b"
  ]
}
```

#### Watch Event (NDJSON, 한 줄씩)

```json
{"type":"event","data":{"model":"claude-opus-4-6","source":"4de9291e","input_tokens":3,"output_tokens":14,"cache_creation_input_tokens":5139,"cache_read_input_tokens":9631,"cost_usd":0.0112}}
```

### UDS/HTTP Sink

UDS와 HTTP sink은 JSON과 동일한 구조를 사용한다. `--output-format`과 무관하게 항상 JSON이다.

- **UDS**: NDJSON (줄 단위) 전송. 소켓이 없으면 에러 로그 후 continue
- **HTTP**: JSON POST (5초 timeout). 실패 시 에러 로그 후 continue

## Retention (데이터 보존)

기본적으로 비활성화되어 있다. `toki settings`에서 보존 기간을 설정하면 활성화된다.

| 대상 | 기본 보존 | 설정 키 |
|------|----------|---------|
| events (개별 이벤트) | 0 (무제한) | `retention_days` |
| rollups (시간별 집계) | 0 (무제한) | `rollup_retention_days` |

- 0 = 비활성화 (데이터를 삭제하지 않음)
- 활성화 시: daemon start 시 1회 실행 + 이후 24시간 간격
- rollup은 events보다 오래 보존하는 것을 권장: events 삭제 후에도 report 가능

## Debug Logging

```bash
# 레벨 1: 상태 전이, 이벤트, 타이밍, writer flush
TOKI_DEBUG=1 toki daemon start

# 레벨 2: 레벨 1 + size unchanged, no new lines 스킵 로그
TOKI_DEBUG=2 toki daemon start
```

출력 예시:
```
[toki:debug] process_file /path/to/session.jsonl — 3 lines, 1024 bytes, 2 events, Active | find_resume: 50µs, read: 120µs, total: 180µs
[toki:debug] flush_dirty — 5 checkpoints sent to writer
[toki:writer] flushed 64 events, 3 rollups in 450µs
[toki:writer] retention cleanup: 150 events, 12 rollups deleted (35ms)
```

## Library Usage

```toml
[dependencies]
toki = { path = "." }
```

```rust
use toki::{Config, start};
use toki::daemon::BroadcastSink;
use std::sync::Arc;

fn main() {
    let config = Config::new(); // loads defaults, then DB settings

    let broadcast = Arc::new(BroadcastSink::new());
    let handle = start(config, None, Box::new(broadcast.clone()))
        .expect("Failed to start");

    // ... application logic ...
    // broadcast.add_client(stream) to add trace clients

    handle.stop(); // 또는 drop 시 자동 종료
}
```

## Claude Code JSONL Structure

Claude Code는 `~/.claude/projects/<encoded-path>/` 하위에 세션 로그를 저장한다.

```
~/.claude/projects/-Users-user-Documents-project/
├── 4de9291e-061e-414a-85cb-de615826aded.jsonl        # 부모 세션
├── 4de9291e-061e-414a-85cb-de615826aded/
│   └── subagents/
│       └── agent-aed1da92cc2e4e9e7.jsonl             # 서브에이전트
└── db7cd31e-fdb1-4767-a6a2-f2f3dc68a74b.jsonl        # 다른 세션
```

파싱 대상 줄 타입:
- `type: "assistant"` — `message.usage`에서 4종 토큰 추출
- `type: "user"`, `type: "file-history-snapshot"` — 무시

서브에이전트 토큰은 부모에 포함되지 않으며 별도 파일에 기록된다.
상세한 JSONL 형식은 `docs/claude-code-jsonl-format.md` 참고.
