# toki 사용 가이드

이 문서는 task 중심으로 작성되었다. 각 H2 섹션은 세 가지 역할 중 하나로 태깅된다:

- **Quick reference** — 플래그, 출력 형식, 설정 키 조회용.
- **Common tasks** — 특정 목표를 달성하기 위한 복붙 가능한 예시.
- **How it works** — 의존 가능한 동작에 대한 짧은 설명.

## 토픽

- **데몬 실행 및 provider 처리** — `daemon`, `provider 관리`
- **데이터 조회** — `report`, `query`, `trace`, `windows`
- **toki 설정** — `settings`, `sync`, 클라이언트 옵션, 출력 형식
- **동작 이해** — retention, 디버그 로깅, JSONL 구조, 라이브러리 사용

## Quick reference: 명령어

toki는 데몬/클라이언트 구조로 동작한다:

- **`daemon start`**: 서버 프로세스. cold start 후 파일 감시 + TSDB 저장
- **`daemon stop/restart/status`**: 데몬 관리
- **`daemon enable/disable`**: 로그인 자동 시작 설치/제거
- **`daemon reset`**: 재구축 가능한 이벤트 DB 삭제; 설정과 window 이력은 보존
- **`settings set providers --add/--remove`**: provider 관리 (Claude Code, Codex CLI 등)
- **`trace`**: 데몬에 연결하여 실시간 이벤트 스트림 수신
- **`query`**: 최상위 PromQL instant/range 쿼리; 로컬·원격 실행 지원
- **`report`**: one-shot TSDB 조회. 데몬이 수집한 데이터를 조회
- **`windows`**: 현재 rate-limit gauge와 저장된 window 이력

## 소스에서 빌드

```bash
git clone https://github.com/korjwl1/toki.git toki
cd toki
cargo build --release
# 바이너리: target/release/toki
# PATH에 추가하거나 직접 실행
```

릴리즈 바이너리는 macOS 또는 Linux에서
`brew tap korjwl1/tap && brew install toki`로 설치할 수 있다.

## Common tasks: daemon

### daemon start

```bash
toki daemon start              # 백그라운드로 분리 (기본)
toki daemon start --foreground # 포그라운드 실행 (디버그용)
```

기본적으로 백그라운드로 분리된다. 디버그할 때는 `--foreground` 옵션으로 포그라운드에서 실행한다.

1. 설정된 provider의 세션 파일을 스캔 (cold start)
2. 파싱된 이벤트를 provider별 TSDB에 저장
3. 파일 감시기와 설정된 window/sync worker 시작
4. trace, report, query, window 클라이언트용 UDS listener 시작

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

실행 중인 데몬을 중지하고 다시 시작한다. provider root/선택, socket 경로,
`window_tracking`, `retention_days`, `window_retention_days`는 재시작이
필요하다. Sync와 `window_polling`은 hot reload되고 표시 설정은 새 CLI process가
매번 읽는다.

### daemon status

```bash
toki daemon status
```

데몬의 실행 여부와 PID를 표시한다.

### daemon reset

```bash
toki daemon reset
```

데몬이 실행 중이면 먼저 중지한 뒤 legacy 이벤트 DB와 provider별 재구축 가능한
`<provider>.fjall`을 삭제한다. 다음 시작 시 provider 로그에서 이벤트, 인덱스,
dictionary, checkpoint를 다시 만든다.

`settings.json`, 인증 정보, `<provider>.windows.fjall`은 의도적으로 보존한다.
Window peak는 항상 재구축 가능한 데이터가 아니기 때문이다.

### daemon enable / disable

```bash
toki daemon enable   # 로그인 자동 시작 설치
toki daemon disable  # 로그인 자동 시작 제거
```

macOS에서는 launchd, Linux에서는 사용자 systemd unit을 사용한다.

## Common tasks: provider 관리

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

각 provider는 이벤트 DB(`~/.config/toki/<provider>.fjall`)와 별도 window 이력
DB(`~/.config/toki/<provider>.windows.fjall`)를 가진다.
provider를 추가하거나 제거한 뒤 데몬이 실행 중이면 재시작이 필요하다.

## Common tasks: trace

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

- 항상 JSONL 형식 (`--output-format`은 query/report에서 사용)
- `--sink`로 UDS, HTTP 등으로 중계 가능
- 기본적으로 `cost_usd` 필드 포함 (daemon이 pricing 로드); `--no-cost`로 제외
- 데몬이 실행 중이어야 한다 (`toki daemon start` 먼저)
- 복수 클라이언트가 동시에 연결할 수 있다 (fan-out via condvar, 클라이언트당 2 스레드)
- 클라이언트가 연결되어 있지 않으면 데몬의 Sink 처리는 완전 비활성화 (zero overhead)
- Ctrl+C로 종료. 데몬은 계속 실행된다
- `--sink uds://` 또는 `--sink http://` 사용 시 `toki trace`를 child process로 실행하면 부모 종료 시 자동 종료 (SIGPIPE)

## Common tasks: report

Report는 UDS로 daemon에 쿼리를 전송(`REPORT` 커맨드 후 JSON payload)하고, 결과를 받는다. DB를 직접 열지 않는다.

데몬이 실행 중이어야 한다. 데몬이 꺼져 있으면 "Cannot connect to toki daemon" 메시지와 함께 시작을 안내한다.
데몬이 실행 중이지만 아직 데이터가 없으면 (cold start 진행 중) "No data in TSDB" 메시지를 표시한다.

### 전체 요약

```bash
toki report
toki report --provider claude_code            # 단일 provider만 조회
toki report --start 20260301
toki report --start 20260301 --end 20260331
```

전체 기간 또는 지정 범위의 모델별 토큰 사용량 합계를 출력한다.
기본 응답은 활성 provider별 결과 set을 분리한다. 서로 다른 token column 의미를
provider 사이에서 합치지 않는다. `--provider`로 하나를 선택할 수 있다.

### 시간별 그룹핑

```bash
toki report daily --start 20260301
toki report weekly --start 20260301
toki report weekly --start 20260301 --start-of-week tue
toki report monthly
toki report yearly
toki report hourly --start 20260301
```

모든 grouping 서브커맨드는 선택적 범위를 받는다. 특히 `hourly`, `daily`,
`weekly`에서는 `--start`로 출력 범위를 제한하는 것이 좋다.

### --start / --end 형식

| 형식 | 예시 | 해석 |
|------|------|------|
| `YYYYMMDD` / `YYYY-MM-DD` | `20260301` / `2026-03-01` | `--start`: 00:00:00.000, `--end`: 23:59:59.999 |
| `YYYYMMDDhhmmss` | `20260301143000` | 정확한 시각 |
| Unix seconds | `1772323200` | 정확한 UTC 초 |
| Unix milliseconds | `1772323200123` | 정확한 UTC 밀리초 |
| RFC 3339 / ISO 8601 | `2026-03-01T14:30:00+09:00` | offset-aware; naive 값은 선택한 timezone 사용 |

- `--timezone`이 지정되면 입력값을 해당 타임존의 로컬 시간으로 해석하여 UTC로 변환
- `--timezone`이 없으면 timezone 없는 값을 UTC로 해석
- 날짜만 쓴 `--end`는 마지막 날 전체를 포함하며, 역전 범위는 거부

```bash
# UTC 기준
toki report daily --start 20260301

# KST 기준 (2026-03-01 00:00:00 KST = 2026-02-28 15:00:00 UTC)
toki report -z Asia/Seoul daily --start 20260301
```

### 세션별 그룹핑

```bash
toki report --group-by-session
toki report --group-by-session --start 20260301
```

시간 기반 서브커맨드(`daily`, `weekly` 등)와 동시에 사용할 수 없다.

### 필터링

`--session-id`, `--project`, `--provider`는 Report의 모든 모드에서 사용할 수 있다.

```bash
# 프로젝트 필터 (서브스트링 매치)
toki report --project toki
toki report daily --start 20260301 --project ddleague
toki report monthly --project myapp

# 세션 필터 (UUID 접두사)
toki report --session-id 4de9291e
toki report --session-id 4de9 --group-by-session

# Provider 필터
toki report --provider claude_code
toki report --provider codex daily --start 20260301

# 조합
toki report --session-id abc --project myapp
toki report daily --start 20260301 --session-id abc
```

요약과 grouping report는 모두 시간순 이벤트 keyspace를 스캔한다. 시간 필터가
없는 session/project 목록은 해당 인덱스를 사용할 수 있다.

### PromQL 스타일 쿼리

최상위 `query` 명령으로 PromQL에서 영감을 받은 자유 쿼리를 실행한다.

#### 문법

```text
[집계함수(] metric{filters}[bucket] [offset duration] [)] [by (dimensions)]
```

| 요소 | 필수 | 설명 |
|------|------|------|
| `metric` | 필수 | `toki_tokens_total` (`usage` alias), `cost`, `events`, `windows`, `sessions`, `projects` |
| `{filters}` | - | `key="value"` 쌍, `,`로 구분 |
| `[bucket]` | - | 시간 버킷: `s`, `m`, `h`, `d`, `w` — 복합 가능: `2h30m` (usage 전용). 데이터가 있는 버킷만 반환하며, 빈 구간은 zero-fill하지 않음. |
| `offset <dur>` | - | 시간 윈도우를 과거로 이동 (예: `offset 7d`) |
| `sum\|avg\|count()` | - | 집계: 모델 차원 collapse (usage 전용) |
| `by (dims)` | - | 그룹 기준: `model`, `session`, `project` (usage 전용) |

필터 키: `model`, `session`, `project`, `provider`, `type`. 시간 범위는 label
filter가 아니라 CLI flag로 지정한다.

#### 예시

```bash
# 전체 사용량 요약
toki query 'usage'

# 모델 필터
toki query 'usage{model="claude-opus-4-6"}'

# 1시간 버킷 + 모델별 그룹핑
toki query --start 20260301 'usage[1h] by (model)'

# Provider 필터 + 모델별 그룹핑
toki query 'usage{provider="codex"} by (model)'

# 세션별 그룹핑 + 시간 범위
toki query --start 20260301 --end 20260331 'usage by (session)'

# 프로젝트별 그룹핑
toki query 'usage{project="myapp"} by (project)'

# 복합 그룹핑
toki query 'usage[1d] by (model, session)'

# offset 수정자 — 이전 기간과 비교
toki query 'usage[1d] offset 7d'

# 집계 함수 — 모델 차원 collapse
toki query 'sum(usage[1d])'                                    # 일별 전체 합산
toki query 'avg(usage[1d])'                                    # 이벤트당 평균
toki query 'count(usage[1d])'                                  # 이벤트 수만
toki query --start 20260301 'sum(usage[1d]) by (project)'      # 프로젝트별 일별 합산

# raw 이벤트
toki query --start 20260320 'events'
toki query --start 20260301 'events{model="claude-opus-4-6"}'
toki query 'events{session="abc123"}'

# 세션 리스팅
toki query 'sessions'
toki query 'sessions{project="myapp"}'
toki query --start 20260301 'sessions'

# 프로젝트 리스팅
toki query 'projects'
toki query 'projects{project="myapp"}'
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

## Common tasks: query

`toki query`는 PromQL 진입점이다. 명시적 범위가 없으면 instant/rolling
쿼리로, `--start` 또는 `--end`가 있으면 해당 범위를 스캔한다. `[1h]`,
`[1d]` 같은 selector는 출력 bucket을 정하고, 원격 쿼리에서는 `--step`
미지정 시 기본 step도 제공한다.

### 플래그

| 플래그 | 설명 |
|--------|------|
| `-z <IANA>` | 시간 해석용 타임존 |
| `-w`, `--start-of-week <요일>` | `[1w]` bucket의 주 시작 경계 |
| `--remote` | 로컬 데몬 대신 toki-sync 서버로 쿼리 전송 |
| `--output-format table\|json` | 출력 형식 |
| `--start`, `--end` | 명시적 스캔 범위 |
| `--step <duration>` | 원격 range-query step; selector 유도값보다 우선 |
| `--no-cost` | 비용 계산 비활성화 |

### 예시

```bash
# Instant 쿼리 (순수 PromQL)
toki query "sum by (model)(toki_tokens_total[1h])"
toki query -z Asia/Seoul "sum by (model)(toki_tokens_total[1d])"

# type 필터와 =~ 정규식 연산자
toki query 'sum(toki_tokens_total{type=~"input|output"}[1h])'

# 원격 쿼리 (sync 서버 경유)
toki query --remote "sum by (model)(toki_tokens_total[1h])"

# 화요일 주 경계
toki query -w tue "sum by (model)(toki_tokens_total[1w])"

# 명시적 범위
toki query --start 2026-03-01 --end 2026-03-31 --step 1d \
  "sum by (model)(toki_tokens_total[1d])"

# JSON 출력
toki query --output-format json "toki_tokens_total[1h]"

# 비용 제외
toki query --no-cost "toki_tokens_total[1h]"
```

`toki report query`는 제거되었다. 표현식을 `toki query`로 옮기고 범위
flag는 그대로 사용한다:

```bash
toki query --start 20260301 --end 20260331 \
  "sum by (model)(toki_tokens_total[1d])"
```

현재 원격 제한: sync server는 로컬 parser가 지원하는 모든 RFC 3339 bound를
받지 않으며, server query에는 pagination 없는 상한이 있다. 매우 큰 원격
범위는 CLI truncation 경고 없이 부분 결과가 될 수 있으므로 bounded query를 권장한다.

## Common tasks: rate-limit windows

Window tracking은 기본 활성화된다. Codex limit은 rollout JSONL에서 수동적으로
추출하고 Claude limit은 활동 기반 usage/profile poller로 가져온다. Window row는
provider별 별도 DB에 저장되며 sync 활성화 시 동기화된다.

```bash
toki windows                         # `windows status`와 동일
toki windows status --fresh          # 제한 시간 내 live 재검증
toki windows status --json
toki windows list                    # 기본 -28d부터 +8d anchor까지
toki windows list --start 20260801 --end 20260831
toki windows list --start 2026-08-01 --end 2026-08-31 --json
toki query windows                   # 저장된 로컬 이력
toki query --remote windows          # 병합된 server 이력
```

`window_tracking=false`는 수집을 끄며 재시작이 필요하다.
`window_polling=false`는 Claude network polling만 끄고 hot reload된다.
이력은 기본 730일인 `window_retention_days`를 따른다.

## Common tasks: settings

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

Boolean 설정은 `true/false`, `on/off`, `yes/no`, `1/0`을 받는다. 잘못된
IANA timezone은 UTC로 조용히 바뀌지 않고 거부된다.

| 설정 항목 | key | 기본값 | 데몬 영향 |
|-----------|-----|--------|-----------|
| Providers | `providers` | 미설정 시 자동 감지 | 재시작 |
| Claude Code Root | `claude_code_root` | `~/.claude` | 재시작 |
| Codex CLI Root | `codex_root` | `~/.codex` | 재시작 |
| Daemon Socket | `daemon_sock` | `~/.config/toki/daemon.sock` | 재시작 |
| Timezone | `timezone` | 빈값(UTC) | hot reload/client |
| Output Format | `output_format` | `table` | hot reload/client |
| Start of Week | `start_of_week` | `mon` | hot reload/client |
| No Cost | `no_cost` | `false` | client 설정; daemon startup 가격 fetch에는 현재 미적용 |
| Event retention | `retention_days` | `0` (무제한) | 재시작 |
| Window tracking | `window_tracking` | `true` | 재시작 |
| Claude window polling | `window_polling` | `true` | hot reload |
| Window retention | `window_retention_days` | `730` | 재시작 |
| 로그인 자동 시작 | `daemon_autostart` | platform 상태 | CLI 관리 |

설정 우선순위: **CLI 인자 > 설정 파일 (settings.json) > 기본값**

알려진 CLI 불일치: `settings set retention_days ...`는 현재 hot reload된다고
출력하지만 writer는 daemon 시작 때 retention policy를 capture한다. 이 key를
바꾼 뒤 daemon을 재시작해야 한다.

`TOKI_HOME`은 격리 실행에서 provider root와 toki 상태가 사용할 home을
오버라이드한다. `TOKI_DEBUG`는 진단 로그를 활성화한다.

## Common tasks: sync

여러 디바이스의 토큰 사용량을 중앙 [toki-sync](https://github.com/korjwl1/toki-sync) 서버로 동기화한다. Sync 관리는 `toki settings sync` 아래에 있다.

### sync enable

```bash
toki settings sync enable --server <host>
toki settings sync enable --server sync.example.com
toki settings sync enable --server 1.2.3.4 --insecure
```

데몬을 toki-sync 서버에 연결한다. 브라우저를 열어 device code flow로 인증한다. CLI에 인증 정보를 직접 전달하지 않는다. 핫리로드로 즉시 반영 — 데몬 재시작 불필요.

| 플래그 | 필수 | 설명 |
|--------|------|------|
| `--server <host>` | 필수 | 동기화 서버 호스트명 또는 IP (포트 제외) |
| `--sync-port <port>` | - | TCP 동기화 포트 (기본: 9090) |
| `--http-port <port>` | - | HTTP API 포트 (기본: TLS 시 443 / TLS 미사용 시 9091) |
| `--insecure` | - | 자체 서명 TLS 인증서 허용 (IP 전용 서버용) |
| `--no-tls` | - | TLS 비활성화 (개발 전용) |
| `--headless` | - | 비대화형 모드 (URL과 코드를 출력하여 수동 입력) |
| `--device-name <이름>` | - | 디바이스 이름 지정 (기본값: 호스트명) |

인증 정보는 macOS Keychain(macOS) 또는 `~/.config/toki/sync.json`(Linux)에 저장된다.

### sync disable

동기화를 비활성화하고 로컬 인증 정보를 삭제한다.

```bash
toki settings sync disable              # 대화형: 원격 데이터 삭제 여부를 묻는다
toki settings sync disable --delete     # 서버에서 이 디바이스 + VM 데이터를 삭제한다
toki settings sync disable --keep       # 원격 데이터를 유지한다 (디바이스 기록 보존)
```

| 플래그 | 동작 |
|--------|------|
| (없음) | 프롬프트: "서버에서 이 디바이스의 데이터를 삭제하시겠습니까? [y/N]" |
| `--delete` | 서버에서 디바이스와 시계열 데이터를 즉시 삭제한다 |
| `--keep` | 서버 데이터를 보존한다 — 디바이스 이전이나 일시적 비활성화에 유용하다 |

모든 경우에 로컬 인증 정보(Keychain/sync.json)와 설정이 삭제된다. 핫리로드로 즉시 반영.

### sync status

```bash
toki settings sync status
```

현재 동기화 설정을 표시한다: 서버 주소, 디바이스 이름, 연결 상태, TLS 모드.

### sync rename

```bash
toki settings sync rename <new-name>
```

동기화 서버에서 현재 디바이스의 이름을 변경한다.

### sync devices

```bash
toki settings sync devices
```

동기화 서버에 등록된 모든 디바이스 목록을 표시한다.

### sync remove

```bash
toki settings sync remove <device-id>
```

선택한 디바이스를 서버에서 제거한다. 제거된 디바이스는 다음 연결에서
거부된다. ID는 먼저 `devices`로 확인한다.

### sync 명령 요약

전체 sync 명령은 다음과 같다:

```bash
toki settings sync enable --server <host>
toki settings sync disable              # 대화형 프롬프트
toki settings sync disable --delete     # 원격 데이터 삭제
toki settings sync disable --keep       # 원격 데이터 유지
toki settings sync status
toki settings sync devices
toki settings sync rename <new-name>
toki settings sync remove <device-id>
```

### query --remote

CLI에서 서버 집계 데이터를 직접 조회할 수 있다:

```bash
toki query --remote 'sum by (model)(toki_tokens_total)'
toki query --remote 'toki_tokens_total{device="macbook-pro"}'
```

`--remote` 플래그는 PromQL 쿼리를 로컬 데몬 대신 toki-sync 서버로 전송한다. sync가 활성화되어 있어야 한다.

## Quick reference: 클라이언트 옵션

| 옵션 | 적용 대상 | 설명 |
|------|----------|------|
| `--output-format table\|json` | query, report | 출력 형식 오버라이드 |
| `--sink <SPEC>` | trace | 출력 대상: `print`, `uds://<path>`, `http://<url>` (복수 지정 가능) |
| `--timezone <IANA>` / `-z` | query, report | 타임존 오버라이드 |
| `-w`, `--start-of-week <요일>` | query | `[1w]` bucket의 주 시작 경계 |
| `--start`, `--end` | query, report, windows list | 시간 범위 |
| `--step <duration>` | query | 원격 range-query step |
| `--remote` | query | toki-sync 서버로 쿼리 전송 |
| `--no-cost` | trace, query, report | 비용 계산 비활성화 |

### --output-format

```bash
toki report --output-format table          # 기본값
toki report --output-format json
```

report와 query에 적용된다. Window 명령은 별도 `--json` flag를 사용한다.

### --timezone / -z

```bash
toki report -z Asia/Seoul daily --start 20260301
toki report -z US/Eastern weekly --start 20260101
```

적용 범위:
- `--start`/`--end` 입력값 해석
- 시간 버킷팅 (일별/시간별 등의 날짜 경계)

### --no-cost

```bash
toki report --no-cost
toki trace --no-cost
```

report: 가격 데이터 fetch를 스킵하고 Cost 컬럼을 표시하지 않는다.
trace: JSONL 출력에서 `cost_usd` 필드를 제거한다.

가격은 `~/.config/toki/pricing.json`의 LiteLLM cache를 사용한다. 정확한 모델
매치가 우선이며, 정확한 Claude `-fast` 행이 없으면 알려진 Opus fast variant에
provider 2배 배수를 적용한다. 공개 cache-read 가격이 없으면 cached input을
무료로 가정하지 않고 일반 input 가격을 보수적으로 쓴다. 사용할 온라인/캐시
가격이 없으면 값을 추정하지 않고 cost를 생략한다.

## Quick reference: 출력 형식

### Table (기본)

#### 전체 요약

```text
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

```text
[toki] Usage by daily
─── 2026-03-01 ───
┌───────────────────────────┬─────────┬─────────┬────────────┬──────────────┬──────────────┬────────┬─────────┐
│ Model                     ┆ Input   ┆ Output  ┆ ...        ┆ ...          ┆ ...          ┆ Events ┆ Cost    │
...
─── 2026-03-02 ───
...
```

#### 세션/프로젝트 리스팅

```text
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

#### Watch mode (실시간 이벤트, trace 클라이언트)

```text
[toki] claude-opus-4-6 | session.jsonl | in:3 cc:5139 cr:9631 out:14 | $0.0112
```

### JSON (`--output-format json`)

모든 JSON 리포트 출력은 `information`(쿼리 메타데이터)과 `providers`(provider 키로 접근하는 데이터)로 구성된다.

| 필드 | 설명 |
|------|------|
| `since` / `until` | 이벤트 DB가 보고한 실제 데이터 범위 |
| `query_since` / `query_until` | 사용자가 `--start`/`--end`로 지정한 필터 (미지정 시 null) |
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

## How it works: retention (데이터 보존)

이벤트 retention은 기본 비활성화다. Window 이력은 provider 로그에서 항상
재구축할 수 없으므로 독립된 기본 730일을 사용한다.

| 대상 | 기본 보존 | 설정 키 |
|------|----------|---------|
| events (개별 이벤트) | 0 (무제한) | `retention_days` |
| rate-limit windows | 730일 | `window_retention_days` |

- 0 = 비활성화 (데이터를 삭제하지 않음)
- 활성화 시: daemon start 시 1회 실행 + 이후 24시간 간격
- 이벤트 정리는 시간 인덱스와 미사용 dictionary entry도 회수
- window 정리는 event retention이 꺼져도 독립 실행

## How it works: debug logging

```bash
# 레벨 1: 상태 전이, 이벤트, 타이밍, writer flush
TOKI_DEBUG=1 toki daemon start

# 레벨 2: 레벨 1 + size unchanged, no new lines 스킵 로그
TOKI_DEBUG=2 toki daemon start
```

출력 예시:

```text
[toki:debug] process_file /path/to/session.jsonl — 3 lines, 1024 bytes, 2 events, Active | find_resume: 50µs, read: 120µs, total: 180µs
[toki:debug] flush_dirty — 5 checkpoints sent to writer
[toki:writer] flushed 64 events in 450µs
[toki:writer] daily retention: 150 events, 24 index, 2 windows, 12 dict entries removed (35ms)
```

## Common tasks: 라이브러리 사용

```toml
[dependencies]
toki = { path = "." }
```

```rust
use toki::{Config, start};
use toki::daemon::BroadcastSink;
use std::sync::Arc;

fn main() {
    let config = Config::new(); // 기본값 다음 settings.json 로드

    let broadcast = Arc::new(BroadcastSink::new());
    let handle = start(config, Box::new(broadcast.clone()))
        .expect("Failed to start");

    // ... application logic ...
    // broadcast.add_client(stream) to add trace clients

    handle.stop(); // 또는 drop 시 자동 종료
}
```

## How it works: Claude Code JSONL 구조

Claude Code는 `~/.claude/projects/<encoded-path>/` 하위에 세션 로그를 저장한다.

```text
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
