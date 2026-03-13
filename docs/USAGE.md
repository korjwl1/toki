# clitrace Usage Guide

## Installation

```bash
cargo build --release
# 바이너리: target/release/clitrace
```

## Commands

clitrace는 데몬/클라이언트 구조로 동작한다:
- **`daemon start`**: 서버 프로세스. cold start 후 파일 감시 + TSDB 저장
- **`daemon stop/status`**: 데몬 관리
- **`trace`**: 데몬에 연결하여 실시간 이벤트 스트림 수신
- **`report`**: one-shot TSDB 조회. 데몬과 독립적으로 동작

## daemon

### daemon start

```bash
clitrace daemon start
```

1. `~/.claude/projects/` 하위의 모든 세션 파일을 스캔 (cold start)
2. 파싱된 이벤트를 TSDB에 저장
3. 전체 토큰 사용량 요약 출력
4. FSEvents 감시 모드 진입
5. UDS 리스너 시작 (trace 클라이언트 수신 대기)

#### Cold Start 그룹핑

시작 시 요약을 시간 단위로 그룹핑하여 출력:

```bash
clitrace daemon start --startup-group-by day      # 일별
clitrace daemon start --startup-group-by week     # 주별
clitrace daemon start --startup-group-by month    # 월별
clitrace daemon start --startup-group-by year     # 연별
clitrace daemon start --startup-group-by hour     # 시간별 (기존 체크포인트 필요)
```

`hour`는 증분 데이터만 출력하므로 이전에 한 번 이상 데몬을 실행한 적이 있어야 한다.
`--full-rescan`과 함께 사용할 수 없다.

#### 필터링

```bash
# 세션 UUID 접두사로 필터 (해당 세션의 이벤트만 출력/저장)
clitrace daemon start --session-id 4de9291e

# 프로젝트 디렉토리 이름으로 필터 (서브스트링 매치)
clitrace daemon start --project clitrace
```

#### 전체 재스캔

```bash
clitrace daemon start --full-rescan
```

체크포인트를 초기화하고 모든 파일을 처음부터 다시 읽는다.
가격 캐시는 보존된다.

#### 커스텀 경로

```bash
# DB 경로 (CLI 인자로만 지정 가능)
clitrace daemon start --db-path /custom/clitrace.fjall

# Claude root, daemon socket 등은 clitrace settings에서 설정
clitrace settings
```

#### 단일 인스턴스 제한

동일 DB 경로에 대해 하나의 데몬만 실행 가능하다.
이미 실행 중이면 `Daemon already running (PID xxx)` 메시지와 함께 종료된다.

### daemon stop

```bash
clitrace daemon stop
clitrace daemon stop --sock /custom/daemon.sock
```

실행 중인 데몬에 SIGTERM을 전송하여 graceful shutdown한다.
PID 파일과 소켓 파일을 정리한다.

### daemon status

```bash
clitrace daemon status
```

데몬의 실행 여부와 PID를 표시한다.

## trace

trace는 실행 중인 데몬에 UDS로 연결하여 실시간 이벤트를 수신하는 클라이언트 명령이다.

```bash
# 기본: 터미널에 실시간 출력
clitrace trace

# 커스텀 소켓 경로
clitrace trace --sock /custom/daemon.sock

# JSON 형식으로 출력
clitrace --output-format json trace

# HTTP로 중계
clitrace trace --sink http://localhost:8080/events

# 복수 sink
clitrace trace --sink print --sink http://localhost:8080/events
```

- 데몬이 실행 중이어야 한다 (`clitrace daemon start` 먼저)
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

### 전체 요약

```bash
clitrace report
clitrace report --since 20260301
clitrace report --since 20260301 --until 20260331
```

전체 기간 또는 지정 범위의 모델별 토큰 사용량 합계를 출력한다.

### 시간별 그룹핑

```bash
clitrace report daily --since 20260301
clitrace report daily --from-beginning
clitrace report weekly --since 20260301
clitrace report weekly --since 20260301 --start-of-week tue
clitrace report monthly
clitrace report yearly
clitrace report hourly --since 20260301
clitrace report hourly --from-beginning
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
clitrace report daily --since 20260301

# KST 기준 (2026-03-01 00:00:00 KST = 2026-02-28 15:00:00 UTC)
clitrace -z Asia/Seoul report daily --since 20260301
```

### 세션별 그룹핑

```bash
clitrace report --group-by-session
clitrace report --group-by-session --since 20260301
```

시간 기반 서브커맨드(`daily`, `weekly` 등)와 동시에 사용할 수 없다.

### 필터링

```bash
# 프로젝트 필터 (서브스트링 매치)
clitrace report --project clitrace
clitrace report --project ddleague daily --since 20260301

# 세션 필터 (UUID 접두사)
clitrace report --session-id 4de9291e

# 서브커맨드 내부에서도 지정 가능 (우선순위: 서브커맨드 > 부모)
clitrace report --session-id abc daily --since 20260301 --session-id def
```

## settings

`clitrace settings`는 cursive TUI로 설정 페이지를 연다. 모든 설정은 DB에 저장된다.

```bash
clitrace settings
clitrace settings --db-path /custom/clitrace.fjall
```

| 설정 항목 | DB key | 기본값 |
|-----------|--------|--------|
| Claude Code Root | `claude_code_root` | `~/.claude` |
| Daemon Socket | `daemon_sock` | `~/.config/clitrace/daemon.sock` |
| Timezone | `timezone` | (빈값 = UTC) |
| Output Format | `output_format` | `table` |
| Start of Week | `start_of_week` | `mon` |
| No Cost | `no_cost` | `false` |
| Retention Days | `retention_days` | `0` (무제한) |
| Rollup Retention Days | `rollup_retention_days` | `0` (무제한) |

설정 우선순위: **CLI 인자 > DB settings > 기본값**

환경변수는 사용하지 않는다 (`CLITRACE_DEBUG` 제외).

## Global Options

모든 명령에 적용되는 옵션 (DB 설정을 오버라이드):

### --output-format

```bash
clitrace --output-format table report          # 기본값
clitrace --output-format json report
clitrace --output-format json trace
```

`print` sink에만 적용된다. UDS/HTTP sink은 항상 JSON이다.

### --sink

```bash
# 기본: 터미널 출력
clitrace trace

# UDS 전송
clitrace trace --sink uds:///tmp/clitrace.sock

# HTTP 전송 (5초 timeout)
clitrace trace --sink http://localhost:8080/v1/events

# 복수 sink (터미널 + HTTP)
clitrace trace --sink print --sink http://localhost:8080/events

# report에서도 사용 가능
clitrace report --sink http://localhost:8080/report
```

### --timezone / -z

```bash
clitrace -z Asia/Seoul report daily --since 20260301
clitrace -z US/Eastern report weekly --from-beginning
clitrace -z Europe/London daemon start --startup-group-by day
```

적용 범위:
- `--since`/`--until` 입력값 해석
- 시간 버킷팅 (일별/시간별 등의 날짜 경계)

### --no-cost

```bash
clitrace --no-cost report
clitrace --no-cost daemon start
```

가격 데이터 fetch를 스킵하고 Cost 컬럼을 표시하지 않는다.

## Output Formats

### Table (기본)

#### 전체 요약

```
[clitrace] Token Usage Summary
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
[clitrace] Usage by daily
─── 2026-03-01 ───
┌───────────────────────────┬─────────┬─────────┬────────────┬──────────────┬──────────────┬────────┬─────────┐
│ Model                     ┆ Input   ┆ Output  ┆ ...        ┆ ...          ┆ ...          ┆ Events ┆ Cost    │
...
─── 2026-03-02 ───
...
```

#### Watch Mode (실시간 이벤트, trace 클라이언트)

```
[clitrace] claude-opus-4-6 | session.jsonl | in:3 cc:5139 cr:9631 out:14 | $0.0112
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

#### Watch Event (NDJSON, 한 줄씩)

```json
{"type":"event","data":{"model":"claude-opus-4-6","source":"4de9291e","input_tokens":3,"output_tokens":14,"cache_creation_input_tokens":5139,"cache_read_input_tokens":9631,"cost_usd":0.0112}}
```

### UDS/HTTP Sink

UDS와 HTTP sink은 JSON과 동일한 구조를 사용한다. `--output-format`과 무관하게 항상 JSON이다.

- **UDS**: NDJSON (줄 단위) 전송. 소켓이 없으면 에러 로그 후 continue
- **HTTP**: JSON POST (5초 timeout). 실패 시 에러 로그 후 continue

## Retention (데이터 보존)

기본적으로 비활성화되어 있다. `clitrace settings`에서 보존 기간을 설정하면 활성화된다.

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
CLITRACE_DEBUG=1 clitrace daemon start

# 레벨 2: 레벨 1 + size unchanged, no new lines 스킵 로그
CLITRACE_DEBUG=2 clitrace daemon start
```

출력 예시:
```
[clitrace:debug] process_file /path/to/session.jsonl — 3 lines, 1024 bytes, 2 events, Active | find_resume: 50µs, read: 120µs, total: 180µs
[clitrace:debug] flush_dirty — 5 checkpoints sent to writer
[clitrace:writer] flushed 64 events, 3 rollups in 450µs
[clitrace:writer] retention cleanup: 150 events, 12 rollups deleted (35ms)
```

## Library Usage

```toml
[dependencies]
clitrace = { path = "." }
```

```rust
use clitrace::{Config, start};
use clitrace::daemon::BroadcastSink;
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
