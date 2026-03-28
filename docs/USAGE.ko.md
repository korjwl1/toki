# toki Usage Guide

## Build from Source

```bash
cargo build --release
# 바이너리: target/release/toki
# PATH에 추가하거나 직접 실행
```

## Commands

toki는 데몬/클라이언트 구조로 동작한다:
- **`daemon start`**: 서버 프로세스. cold start 후 파일 감시 + TSDB 저장
- **`daemon stop/restart/status`**: 데몬 관리
- **`daemon reset`**: DB 전체 삭제 및 초기화
- **`settings set providers --add/--remove`**: provider 관리 (Claude Code, Codex CLI 등)
- **`trace`**: 데몬에 연결하여 실시간 이벤트 스트림 수신
- **`report`**: one-shot TSDB 조회. 데몬이 수집한 데이터를 조회

## daemon

### daemon start

```bash
toki daemon start              # 백그라운드로 분리 (기본)
toki daemon start --foreground # 포그라운드 실행 (디버그용)
```

기본적으로 백그라운드로 분리된다. 디버그할 때는 `--foreground` 옵션으로 포그라운드에서 실행한다.

1. 설정된 provider의 세션 파일을 스캔 (cold start)
2. 파싱된 이벤트를 provider별 TSDB에 저장
3. 전체 토큰 사용량 요약 출력
4. FSEvents 감시 모드 진입
5. UDS 리스너 시작 (trace 클라이언트 수신 대기)

데몬 설정(소켓 경로, Claude Code root 등)은 `toki settings`에서 관리한다.

동일 DB 경로에 대해 하나의 데몬만 실행 가능하다.
이미 실행 중이면 `Daemon already running (PID xxx)` 메시지와 함께 종료된다.

### daemon stop

```bash
toki daemon stop
```

실행 중인 데몬에 SIGTERM을 전송하여 graceful shutdown한다.
PID 파일과 소켓 파일을 정리한다.

### daemon restart

```bash
toki daemon restart
```

실행 중인 데몬을 중지하고 다시 시작한다.
설정(`toki settings`)을 변경한 뒤 데몬에 즉시 반영하려면 이 명령을 사용한다.

### daemon status

```bash
toki daemon status
```

데몬의 실행 여부와 PID를 표시한다.

### daemon reset

```bash
toki daemon reset
```

데몬이 실행 중이면 먼저 중지한 뒤, TSDB 데이터베이스를 전체 삭제한다.
모든 이벤트, rollup, 체크포인트, 설정이 초기화된다.
삭제 후 `toki daemon start`로 처음부터 다시 데이터를 수집할 수 있다.

## provider 관리

toki는 `~/.claude`(Claude Code)와 `~/.codex`(Codex CLI)를 자동으로 감지하여 활성화한다. 대부분의 경우 별도 설정 없이 바로 사용할 수 있다.
직접 관리하려면 TUI(`toki settings`) 또는 CLI로 설정한다.

```bash
# Claude Code 추적 활성화
toki settings set providers --add claude_code

# Codex CLI 추적 활성화
toki settings set providers --add codex

# Provider 비활성화
toki settings set providers --remove codex

# 전체 provider 목록 + 상태 확인
toki settings get providers
```

각 provider는 독립된 데이터베이스(`~/.config/toki/<provider>.fjall`)를 가진다.
provider를 추가하거나 제거한 뒤 데몬이 실행 중이면 재시작이 필요하다.

## trace

trace는 실행 중인 데몬에 UDS로 연결하여 실시간 이벤트를 수신하는 클라이언트 명령이다.
`TRACE` 커맨드를 전송한 뒤, daemon이 보내는 JSONL을 sink로 출력한다.

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

- 항상 JSONL 형식 (`--output-format`은 report에만 적용)
- `--sink`로 UDS, HTTP 등으로 중계 가능
- 기본적으로 `cost_usd` 필드 포함 (daemon이 pricing 로드); `--no-cost`로 제외
- 데몬이 실행 중이어야 한다 (`toki daemon start` 먼저)
- 복수 클라이언트가 동시에 연결할 수 있다 (fan-out via condvar, 클라이언트당 2 스레드)
- 클라이언트가 연결되어 있지 않으면 데몬의 Sink 처리는 완전 비활성화 (zero overhead)
- Ctrl+C로 종료. 데몬은 계속 실행된다
- `--sink uds://` 또는 `--sink http://` 사용 시 `toki trace`를 child process로 실행하면 부모 종료 시 자동 종료 (SIGPIPE)

## report

Report는 UDS로 daemon에 쿼리를 전송(`REPORT` 커맨드 후 JSON payload)하고, 결과를 받는다. DB를 직접 열지 않는다.

데몬이 실행 중이어야 한다. 데몬이 꺼져 있으면 "Cannot connect to toki daemon" 메시지와 함께 시작을 안내한다.
데몬이 실행 중이지만 아직 데이터가 없으면 (cold start 진행 중) "No data in TSDB" 메시지를 표시한다.

### 전체 요약

```bash
toki report
toki report --provider claude_code            # 단일 provider만 조회
toki report --since 20260301
toki report --since 20260301 --until 20260331
```

전체 기간 또는 지정 범위의 모델별 토큰 사용량 합계를 출력한다.
기본적으로 모든 활성 provider의 결과를 병합한다. `--provider`로 단일 provider만 필터링할 수 있다.

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

`hourly`, `daily`, `weekly`는 데이터 양이 많을 수 있으므로 `--since`를 필수로 요구한다.

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

`--session-id`, `--project`, `--provider`는 Report의 모든 모드에서 사용할 수 있다.

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

필터가 지정되면 rollup 대신 이벤트 레벨 스캔을 사용한다 (rollup에는 세션/프로젝트 정보가 없으므로).

### PromQL 스타일 쿼리

`report query` 서브커맨드로 PromQL에서 영감을 받은 자유 쿼리를 실행할 수 있다.

#### 문법

```
[집계함수(] metric{filters}[bucket] [offset duration] [)] [by (dimensions)]
```

| 요소 | 필수 | 설명 |
|------|------|------|
| `metric` | O | `usage`, `sessions`, `projects`, `events` |
| `{filters}` | X | `key="value"` 쌍, `,`로 구분 |
| `[bucket]` | X | 시간 버킷: `s`, `m`, `h`, `d`, `w` — 복합 가능: `2h30m` (usage 전용). 데이터가 있는 버킷만 반환하며, 빈 구간은 zero-fill하지 않음. |
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

집계 없이 사용하면 기존 동작대로 모델별 분리 출력.

#### Events 출력

`events` 메트릭은 개별 API 호출 레코드를 반환한다:

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

`toki settings`는 cursive TUI로 설정 페이지를 연다. 모든 설정은 `~/.config/toki/settings.json` 파일에 저장된다.

```bash
# TUI로 설정
toki settings

# 비대화형 CLI로 설정
toki settings set claude_code_root ~/.claude
toki settings set timezone Asia/Seoul
toki settings get timezone
toki settings list
```

데몬 영향 설정(`claude_code_root`, `codex_root`, `daemon_sock`, `retention_days`, `rollup_retention_days`) 변경 시
데몬이 실행 중이면 재시작 여부를 묻는다.

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

환경변수는 사용하지 않는다 (`TOKI_DEBUG` 제외).

## sync

여러 디바이스의 토큰 사용량을 중앙 [toki-sync](https://github.com/korjwl1/toki-sync) 서버로 동기화한다. 모든 서브커맨드는 `toki settings sync`로도 사용할 수 있다.

### sync enable

```bash
toki sync enable --server <host:port> --username <user>
toki sync enable --server sync.example.com:9090 --username admin
toki sync enable --server 1.2.3.4:9090 --insecure --username admin
```

데몬을 toki-sync 서버에 연결한다. 비밀번호는 대화형으로 입력받는다 (스크립트에서는 `--password` 사용). 핫리로드로 즉시 반영 — 데몬 재시작 불필요.

| 플래그 | 필수 | 설명 |
|--------|------|------|
| `--server <host:port>` | O | 동기화 서버 주소 (호스트명 또는 IP + 포트) |
| `--username <user>` | O | 계정 사용자명 |
| `--password <pass>` | X | 비밀번호 (생략 시 대화형 입력) |
| `--insecure` | X | 자체 서명 TLS 인증서 허용 (IP 전용 서버용) |
| `--no-tls` | X | TLS 비활성화 (개발 전용) |
| `--headless` | X | 비대화형 모드 (`--password` 필수) |

인증 정보는 macOS Keychain(macOS) 또는 `~/.config/toki/sync.json`(Linux)에 저장된다.

### sync disable

```bash
toki sync disable
```

동기화 서버와의 연결을 해제한다. 핫리로드로 즉시 반영.

### sync status

```bash
toki sync status
```

현재 동기화 설정을 표시한다: 서버 주소, 사용자명, 연결 상태, TLS 모드.

### sync devices

```bash
toki sync devices
```

동기화 서버에 등록된 모든 디바이스 목록을 표시한다.

### settings sync

모든 sync 명령은 `toki settings sync` 서브커맨드로도 사용할 수 있다:

```bash
toki settings sync enable --server <host:port> --username <user>
toki settings sync disable
toki settings sync status
toki settings sync devices
```

### report query --remote

CLI에서 서버 집계 데이터를 직접 조회할 수 있다:

```bash
toki report query --remote 'sum by (model)(toki_tokens_total)'
toki report query --remote 'toki_tokens_total{device="macbook-pro"}'
```

`--remote` 플래그는 PromQL 쿼리를 로컬 데몬 대신 toki-sync 서버로 전송한다. sync가 활성화되어 있어야 한다.

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

report의 `print` 출력에만 적용된다.

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

report: 가격 데이터 fetch를 스킵하고 Cost 컬럼을 표시하지 않는다.
trace: JSONL 출력에서 `cost_usd` 필드를 제거한다.

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

모든 JSON 리포트 출력은 `information`(쿼리 메타데이터)과 `providers`(provider 키로 접근하는 데이터)로 구성된다.

| 필드 | 설명 |
|------|------|
| `since` / `until` | TSDB에 있는 실제 데이터 범위 (rollup 첫/마지막 타임스탬프, O(1)) |
| `query_since` / `query_until` | 사용자가 `--since`/`--until`로 지정한 필터 (미지정 시 null) |
| `timezone` | 시간 해석에 사용된 타임존 (null = UTC) |
| `start_of_week` | 주간 그룹핑 시 주의 시작 요일 |
| `generated_at` | 리포트 생성 시각 |

#### Summary

```json
{
  "information": {
    "type": "summary",
    "since": "2026-01-15T00:00:00Z",
    "until": "2026-03-21T14:00:00Z",
    "query_since": null,
    "query_until": null,
    "timezone": null,
    "start_of_week": "mon",
    "generated_at": "2026-03-21T15:30:00Z"
  },
  "providers": {
    "claude_code": [
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
}
```

#### Grouped

```json
{
  "information": {
    "type": "daily",
    "since": "2026-01-15T00:00:00Z",
    "until": "2026-03-21T14:00:00Z",
    "query_since": "20260301",
    "query_until": null,
    "timezone": "Asia/Seoul",
    "start_of_week": "mon",
    "generated_at": "2026-03-21T15:30:00Z"
  },
  "providers": {
    "claude_code": [
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
}
```

#### List (sessions/projects)

```json
{
  "information": {
    "type": "sessions",
    "since": "2026-01-15T00:00:00Z",
    "until": "2026-03-21T14:00:00Z",
    "query_since": null,
    "query_until": null,
    "timezone": null,
    "start_of_week": "mon",
    "generated_at": "2026-03-21T15:30:00Z"
  },
  "providers": {
    "claude_code": [
      "4de9291e-061e-414a-85cb-de615826aded",
      "db7cd31e-fdb1-4767-a6a2-f2f3dc68a74b"
    ]
  }
}
```

#### Watch Event (NDJSON, 한 줄씩)

```json
{"type":"event","data":{"model":"claude-opus-4-6","source":"4de9291e","provider":"Claude Code","timestamp":"2026-03-19T10:30:00.123Z","input_tokens":3,"output_tokens":14,"cache_creation_input_tokens":5139,"cache_read_input_tokens":9631,"cost_usd":0.0112}}
```

> Trace는 항상 JSONL을 출력한다. `--no-cost`로 `cost_usd` 필드를 제외할 수 있다.

### Provider별 컬럼

provider마다 고유한 토큰 컬럼 스키마를 사용한다. 테이블 헤더와 JSON 키가 provider별로 다르다:

| Provider | 컬럼 | JSON 키 |
|----------|------|---------|
| Claude Code | Input, Output, Cache Create, Cache Read | `input_tokens`, `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens` |
| Codex CLI | Input, Output, Cached Input, Reasoning Output | `input_tokens`, `output_tokens`, `cached_input_tokens`, `reasoning_output_tokens` |

리포트는 provider별로 독립된 테이블을 출력한다. 컬럼 의미가 다르므로 여러 provider의 결과를 하나의 테이블로 합치지 않는다.

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
    let handle = start(config, Box::new(broadcast.clone()))
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

> **참고:** Codex CLI도 유사한 JSONL 형식을 사용하지만 별도의 파서로 처리된다. 상세한 Codex 데이터 형식은 `docs/codex-cli-analysis.md` 참고.
