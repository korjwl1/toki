# toki 사용 가이드

## 📦 설치하기

Homebrew를 사용하는 것이 가장 빠르고 권장되는 방법입니다.

```bash
brew tap korjwl1/tap
brew install toki
```

*(소스에서 직접 빌드하려면 `cargo build --release`를 사용하세요. 바이너리는 `target/release/toki`에 생성됩니다.)*

---

## 🛠 주요 명령어

toki는 효율적인 동작을 위해 **데몬(Daemon) - 클라이언트** 구조로 설계되어 있습니다.

- **`daemon`**: 백그라운드에서 로그를 감시하고 인덱싱하는 엔진입니다.
- **`report`**: 수집된 데이터를 즉시 조회하는 리포트 도구입니다.
- **`trace`**: 실시간으로 발생하는 토큰 사용 이벤트를 스트리밍합니다.
- **`settings`**: 추적할 도구(provider)나 설정을 관리합니다.

---

## 🐇 데몬 (Daemon) 관리

### 시작하기
데몬을 실행하면 자동으로 로컬 로그 스캔 및 실시간 감시가 시작됩니다.

```bash
toki daemon start              # 백그라운드 실행 (권장)
toki daemon start --foreground # 디버그를 위한 포그라운드 실행
```

> **💡 Cold Start 안내**: 처음 실행하거나 대량의 기존 데이터를 처리할 때, toki는 모든 CPU 코어를 사용해서 최대한 빠르게 인덱싱합니다. CPU 점유율이 일시적으로 높아질 수 있지만, 이는 **과거 데이터를 단 몇 초 만에 처리하기 위한 의도된 동작**입니다. 인덱싱이 끝나면 CPU 점유율은 바로 0%에 가깝게 내려가고, 이후에는 변경된 로그만 증분 처리합니다.

### 중지 및 재시작

```bash
toki daemon stop      # 데몬 안전하게 종료
toki daemon restart   # 설정 변경 후 즉시 반영
toki daemon status    # 현재 데몬 상태 및 PID 확인
```

### 초기화

```bash
toki daemon reset     # 모든 DB 데이터 및 인덱스 초기화
```

---

## 🔍 추적 대상(Provider) 설정

toki는 Claude Code(`~/.claude`)와 Codex CLI(`~/.codex`)를 지원합니다. 처음 실행할 때 설치된 도구를 자동으로 감지해서 활성화하기 때문에, 대부분의 경우 별도 설정이 필요 없습니다.

직접 관리하고 싶다면 TUI 또는 CLI로 설정할 수 있습니다:

```bash
# TUI에서 provider 체크박스로 선택
toki settings

# 또는 CLI로 개별 추가/제거
toki settings set providers --add claude_code
toki settings set providers --add codex
toki settings set providers --remove codex

# 등록된 목록 확인
toki settings get providers
```

*(Provider 설정을 변경한 뒤에는 `toki daemon restart`가 필요합니다.)*

---

## 📡 Trace (실시간 스트리밍)

`trace`는 실행 중인 데몬에 UDS로 연결해서 실시간 토큰 사용 이벤트를 받아오는 클라이언트 명령입니다. 데몬에 `TRACE` 커맨드를 보내고, 돌아오는 JSONL 스트림을 sink로 출력합니다.

```bash
# 실시간 JSONL 스트림 (stdout)
toki trace

# UDS 또는 HTTP로 중계
toki trace --sink uds:///tmp/toki.sock
toki trace --sink http://localhost:8080/events

# 멀티 싱크 (터미널 + HTTP)
toki trace --sink print --sink http://localhost:8080/events

# 비용 필드 제외
toki trace --no-cost
```

- 항상 JSONL 형식으로 출력합니다 (`--output-format`은 report에만 적용)
- `--sink`로 UDS, HTTP 등 외부 대상으로 중계할 수 있습니다
- 기본적으로 `cost_usd` 필드가 포함됩니다 (데몬이 pricing 로드). `--no-cost`로 제외 가능
- 데몬이 먼저 실행 중이어야 합니다 (`toki daemon start`)
- 여러 클라이언트가 동시에 연결할 수 있습니다 (condvar 기반 fan-out, 클라이언트당 2 스레드)
- 클라이언트가 없으면 데몬의 Sink 처리는 완전히 비활성화됩니다 (zero overhead)
- Ctrl+C로 종료해도 데몬은 계속 실행됩니다
- `--sink uds://`나 `--sink http://` 사용 시, `toki trace`를 자식 프로세스로 실행하면 부모가 종료될 때 자동으로 함께 종료됩니다 (SIGPIPE)

---

## 📊 리포트 (Report)

toki의 핵심은 **압도적인 조회 속도**입니다. 이미 인덱싱된 데이터를 TSDB에서 조회하기 때문에, 수 기가바이트의 로그가 있어도 즉시 결과를 보여줍니다.

`report`는 UDS를 통해 데몬에 쿼리를 보내고(`REPORT` 커맨드 + JSON payload), 결과를 받아옵니다. DB를 직접 열지 않습니다.

데몬이 실행 중이어야 합니다. 데몬이 꺼져 있으면 "Cannot connect to toki daemon" 메시지와 함께 시작 방법을 안내합니다.
데몬이 실행 중이지만 아직 데이터가 없으면(cold start 진행 중) "No data in TSDB" 메시지를 보여줍니다.

### 전체 요약

```bash
toki report
toki report --provider claude_code            # 단일 provider만 조회
toki report --since 20260301
toki report --since 20260301 --until 20260331
```

전체 기간 또는 지정 범위의 모델별 토큰 사용량 합계를 보여줍니다.
기본적으로 모든 활성 provider의 결과를 합쳐서 보여주며, `--provider`로 하나만 골라볼 수도 있습니다.

### 시간별 그룹핑

```bash
toki report daily --since 20260301
toki report weekly --since 20260301
toki report weekly --since 20260301 --start-of-week tue
toki report monthly
toki report yearly
toki report hourly --since 20260301
```

| 서브커맨드 | `--since` 필수 | 비고 |
|-----------|----------------|------|
| `hourly` | O | |
| `daily` | O | |
| `weekly` | O | `--start-of-week` 사용 가능 |
| `monthly` | X | |
| `yearly` | X | |

`hourly`, `daily`, `weekly`는 데이터 양이 많을 수 있어서 `--since`가 필수입니다.

### --since / --until 형식

| 형식 | 예시 | 해석 |
|------|------|------|
| `YYYYMMDD` | `20260301` | `--since`: 00:00:00, `--until`: 23:59:59 |
| `YYYYMMDDhhmmss` | `20260301143000` | 정확한 시각 |

- `--timezone`이 지정되면 입력값을 해당 타임존의 로컬 시간으로 해석해서 UTC로 변환합니다
- `--timezone`이 없으면 UTC로 해석합니다

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

시간 기반 서브커맨드(`daily`, `weekly` 등)와는 동시에 사용할 수 없습니다.

### 필터링

`--session-id`, `--project`, `--provider`는 Report의 모든 모드에서 사용할 수 있습니다.

```bash
# 프로젝트 필터 (서브스트링 매치)
toki report --project toki
toki report daily --since 20260301 --project ddleague
toki report monthly --project myapp

# 세션 필터 (UUID 접두사)
toki report --session-id 4de9291e
toki report --session-id 4de9 --group-by-session

# Provider 필터
toki report --provider claude_code
toki report --provider codex daily --since 20260301

# 조합
toki report --session-id abc --project myapp
toki report daily --since 20260301 --session-id abc
```

필터가 지정되면 rollup 대신 이벤트 레벨 스캔을 사용합니다 (rollup에는 세션/프로젝트 정보가 없기 때문).

### PromQL 스타일 쿼리

`report query` 서브커맨드로 PromQL에서 영감을 받은 자유 쿼리를 실행할 수 있습니다.

#### 문법

```
[집계함수(] metric{filters}[bucket] [offset duration] [)] [by (dimensions)]
```

| 요소 | 필수 | 설명 |
|------|------|------|
| `metric` | O | `usage`, `sessions`, `projects`, `events` |
| `{filters}` | X | `key="value"` 쌍, `,`로 구분 |
| `[bucket]` | X | 시간 버킷: `s`, `m`, `h`, `d`, `w` (usage 전용) |
| `offset <dur>` | X | 시간 윈도우를 과거로 이동 (예: `offset 7d`) |
| `sum\|avg\|count()` | X | 집계: 모델 차원 collapse (usage 전용) |
| `by (dims)` | X | 그룹 기준: `model`, `session`, `project` (usage 전용) |

필터 키: `model`, `session`, `project`, `provider`, `since`, `until`

#### 예시

```bash
# 전체 사용량 요약
toki report query 'usage'

# 모델 필터
toki report query 'usage{model="claude-opus-4-6"}'

# 1시간 버킷 + 모델별 그룹핑
toki report query 'usage{since="20260301"}[1h] by (model)'

# Provider 필터 + 모델별 그룹핑
toki report query 'usage{provider="codex"} by (model)'

# 세션별 그룹핑 + 시간 범위
toki report query 'usage{since="20260301", until="20260331"} by (session)'

# 프로젝트별 그룹핑
toki report query 'usage{project="myapp"} by (project)'

# 복합 그룹핑
toki report query 'usage[1d] by (model, session)'

# offset 수정자 — 이전 기간과 비교
toki report query 'usage[1d] offset 7d'

# 집계 함수 — 모델 차원 collapse
toki report query 'sum(usage[1d])'                                    # 일별 전체 합산
toki report query 'avg(usage[1d])'                                    # 이벤트당 평균
toki report query 'count(usage[1d])'                                  # 이벤트 수만
toki report query 'sum(usage{since="20260301"}[1d]) by (project)'     # 프로젝트별 일별 합산

# raw 이벤트
toki report query 'events{since="20260320"}'
toki report query 'events{model="claude-opus-4-6", since="20260301"}'
toki report query 'events{session="abc123"}'

# 세션 리스팅
toki report query 'sessions'
toki report query 'sessions{project="myapp"}'
toki report query 'sessions{since="20260301"}'

# 프로젝트 리스팅
toki report query 'projects'
toki report query 'projects{project="myapp"}'
```

#### 집계 함수 의미

| 함수 | 토큰 필드 | 이벤트 수 | 비용 | 모델명 |
|------|----------|----------|------|--------|
| `sum()` | 전체 모델 합산 | 합산 | 합산 | `(total)` |
| `avg()` | 합산 / event_count | 1 | 합산/count | `(avg/event)` |
| `count()` | 0 | 합산 | 0 | `(count)` |

집계 없이 사용하면 기존처럼 모델별로 나눠서 보여줍니다.

#### Events 출력

`events` 메트릭은 개별 API 호출 레코드를 반환합니다:

```json
{
  "type": "events",
  "data": [
    {
      "timestamp": "2026-03-20T10:30:00",
      "model": "claude-opus-4-6",
      "session": "4de9291e-...",
      "project": "myapp",
      "input_tokens": 100,
      "output_tokens": 50,
      "cache_creation_input_tokens": 0,
      "cache_read_input_tokens": 0,
      "cost_usd": 0.003
    }
  ]
}
```

## settings

`toki settings`를 실행하면 TUI 설정 화면이 열립니다. 모든 설정은 `~/.config/toki/settings.json`에 저장됩니다.

```bash
# TUI로 설정
toki settings

# 비대화형 CLI로 설정
toki settings set claude_code_root ~/.claude
toki settings set codex_root ~/.codex
toki settings set timezone Asia/Seoul
toki settings get timezone
toki settings list
```

데몬에 영향을 주는 설정(`claude_code_root`, `codex_root`, `daemon_sock`, `retention_days`, `rollup_retention_days`)을 변경하면,
데몬이 실행 중일 경우 재시작 여부를 물어봅니다.

| 설정 항목 | key | 기본값 | 데몬 영향 |
|-----------|-----|--------|-----------|
| Providers | `providers` | `[]` | O |
| Claude Code Root | `claude_code_root` | `~/.claude` | O |
| Codex CLI Root | `codex_root` | `~/.codex` | O |
| Daemon Socket | `daemon_sock` | `~/.config/toki/daemon.sock` | O |
| Timezone | `timezone` | (빈값 = UTC) | X |
| Output Format | `output_format` | `table` | X |
| Start of Week | `start_of_week` | `mon` | X |
| No Cost | `no_cost` | `false` | X |
| Retention Days | `retention_days` | `0` (무제한) | O |
| Rollup Retention Days | `rollup_retention_days` | `0` (무제한) | O |

설정 우선순위: **CLI 인자 > 설정 파일 (settings.json) > 기본값**

환경변수는 사용하지 않습니다 (`TOKI_DEBUG` 제외).

## 클라이언트 옵션

| 옵션 | 적용 대상 | 설명 |
|------|----------|------|
| `--output-format table\|json` | report | 출력 형식 오버라이드 |
| `--sink <SPEC>` | trace | 출력 대상: `print`, `uds://<path>`, `http://<url>` (복수 지정 가능) |
| `--timezone <IANA>` / `-z` | report | 타임존 오버라이드 |
| `--no-cost` | trace, report | 비용 계산 비활성화 |

### --output-format (report 전용)

```bash
toki report --output-format table          # 기본값
toki report --output-format json
```

report의 `print` 출력에만 적용됩니다.

### --timezone / -z

```bash
toki report -z Asia/Seoul daily --since 20260301
toki report -z US/Eastern weekly --since 20260101
```

적용 범위:
- `--since`/`--until` 입력값 해석
- 시간 버킷팅 (일별/시간별 등의 날짜 경계)

### --no-cost

```bash
toki report --no-cost
toki trace --no-cost
```

report: 가격 데이터 fetch를 건너뛰고 Cost 컬럼을 숨깁니다.
trace: JSONL 출력에서 `cost_usd` 필드를 제거합니다.

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
{"type":"event","data":{"model":"claude-opus-4-6","source":"4de9291e","provider":"Claude Code","timestamp":"2026-03-19T10:30:00.123Z","input_tokens":3,"output_tokens":14,"cache_creation_input_tokens":5139,"cache_read_input_tokens":9631,"cost_usd":0.0112}}
```

> Trace는 항상 JSONL을 출력합니다. `--no-cost`로 `cost_usd` 필드를 제외할 수 있습니다.

### Provider별 컬럼

provider마다 고유한 토큰 컬럼 스키마를 사용합니다. 테이블 헤더와 JSON 키가 provider별로 다릅니다:

| Provider | 컬럼 | JSON 키 |
|----------|------|---------|
| Claude Code | Input, Output, Cache Create, Cache Read | `input_tokens`, `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens` |
| Codex CLI | Input, Output, Cached Input, Reasoning Output | `input_tokens`, `output_tokens`, `cached_input_tokens`, `reasoning_output_tokens` |

리포트는 provider별로 독립된 테이블을 출력합니다. 컬럼의 의미가 다르기 때문에 여러 provider의 결과를 하나의 테이블로 합치지 않습니다.

### UDS/HTTP Sink

UDS와 HTTP sink은 JSON과 동일한 구조를 사용합니다. `--output-format` 설정과 무관하게 항상 JSON입니다.

- **UDS**: NDJSON (줄 단위) 전송. 소켓이 없으면 에러 로그를 남기고 계속 진행
- **HTTP**: JSON POST (5초 타임아웃). 실패하면 에러 로그를 남기고 계속 진행

## Retention (데이터 보존)

기본적으로 비활성화되어 있습니다. `toki settings`에서 보존 기간을 설정하면 활성화됩니다.

| 대상 | 기본 보존 | 설정 키 |
|------|----------|---------|
| events (개별 이벤트) | 0 (무제한) | `retention_days` |
| rollups (시간별 집계) | 0 (무제한) | `rollup_retention_days` |

- 0 = 비활성화 (데이터를 삭제하지 않음)
- 활성화하면: 데몬 시작 시 1회 실행, 이후 24시간마다 반복
- rollup은 events보다 오래 보존하는 걸 권장합니다. events를 지운 뒤에도 리포트를 볼 수 있습니다

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
    let handle = start(config, Box::new(broadcast.clone()))
        .expect("Failed to start");

    // ... application logic ...
    // broadcast.add_client(stream) to add trace clients

    handle.stop(); // 또는 drop 시 자동 종료
}
```

## Claude Code JSONL Structure

Claude Code는 `~/.claude/projects/<encoded-path>/` 아래에 세션 로그를 저장합니다.

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

서브에이전트 토큰은 부모에 포함되지 않고 별도 파일에 기록됩니다.
자세한 JSONL 형식은 `docs/claude-code-jsonl-format.md`를 참고하세요.

> **참고:** Codex CLI도 유사한 JSONL 형식을 사용하지만 별도의 파서로 처리됩니다. 자세한 Codex 데이터 형식은 `docs/codex-cli-analysis.md`를 참고하세요.
