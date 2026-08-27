<p align="center">
  <img src="assets/logo.png" alt="toki logo" width="160" />
</p>

<h1 align="center">toki</h1>

<p align="center">
  <b>Claude Code와 Codex CLI를 위한 토큰 사용량 트래커</b><br>
  Rust로 구축. 데몬 기반. 증분 처리 중심. 작업 흐름을 가볍게 유지합니다.
</p>

<p align="center">
  <sub><b>toki</b> = <b>to</b>ken <b>i</b>nspector — 발음이 토끼(rabbit)와 비슷합니다. 토끼처럼 빠르고, 토끼처럼 가볍습니다.</sub>
</p>

<p align="center">
  <a href="README.md">🇺🇸 English</a>
</p>

<p align="center">
  <em>단순한 바이브 코딩이 아닌, 전문 개발자가 세심하게 설계한 프로그램입니다.</em>
</p>

<p align="center">
  <img src="assets/demo.gif" alt="toki 데모" width="900" />
</p>

> **GUI가 필요하신가요?** [Toki Monitor](https://github.com/korjwl1/toki-monitor)는 실시간 대시보드, 토큰 속도에 반응하는 토끼 애니메이션, 이상 감지 알림을 제공하는 macOS 메뉴바 앱입니다. toki 데몬 위에서 동작합니다.

> **멀티 디바이스 동기화?** [Toki Sync Server](https://github.com/korjwl1/toki-sync)로 여러 기기의 토큰 사용량을 통합 관리할 수 있습니다. 셀프 호스팅, `toki settings sync` 명령으로 연동합니다.

---

## 목차

- [Quick Start](#quick-start)
- [누가 쓰면 좋을까?](#누가-쓰면-좋을까)
- [동작 방식](#동작-방식)
- [성능](#성능)
- [프라이버시와 보안](#프라이버시와-보안)
- [명령어](#명령어)
- [멀티 디바이스 동기화](#멀티-디바이스-동기화)
- [비용 계산](#비용-계산)
- [지원 Provider](#지원-provider)
- [예정된 기능](#예정된-기능)
- [후원](#후원)
- [라이선스](#라이선스)

---

## Quick Start

설치부터 첫 리포트까지 30초 안에:

```bash
# 1. 설치 (macOS 또는 Homebrew를 사용하는 Linux)
brew tap korjwl1/tap
brew install toki

# 2. 데몬 시작 (~/.claude, ~/.codex 자동 감지)
toki daemon start

# 3. 사용량 확인
toki report
```

trace, PromQL 쿼리, 시간별 그룹핑, 원격 sync 등 추가 명령은 아래 [명령어](#명령어) 또는 [사용법 가이드](docs/USAGE.ko.md)를 참고하세요.

---

## 누가 쓰면 좋을까?

toki는 다음 네 가지 상황에 적합합니다:

- 토큰 리포트 볼 때마다 터미널이 멈추는 분. toki는 cold start 14배, 리포트 1,700배 빠릅니다. 2GB 데이터도 7ms면 나옵니다.
- "총 토큰" 이상의 분석이 필요한 분. 모델별, 세션별, 프로젝트별, 날짜별 분석을 PromQL 스타일로 자유롭게. 시간 범위 필터, 다차원 그룹핑, 비용 추적까지 한 줄이면 됩니다.
- OpenTelemetry 설정이 귀찮은 분. Collector도, 환경변수도, 설정 파일 수정도 필요 없습니다. toki를 설치하고 실행하면 디스크에 있는 세션 로그를 바로 읽습니다. 설치 전에 쌓인 수개월치 데이터도 즉시 분석됩니다.
- 여러 AI CLI 도구를 쓰는 분. Claude Code와 Codex CLI를 하나의 통합 뷰로 볼 수 있습니다. `--provider`로 도구별 필터링도 됩니다.

---

## 동작 방식

Docker처럼 데몬/클라이언트 구조입니다:

```text
toki daemon start     # 항상 실행되는 서버   (≈ dockerd)
toki trace            # 실시간 스트림        (≈ docker logs -f)
toki report           # 즉시 TSDB 조회      (≈ docker ps)
```

- **daemon** — 설정된 provider(Claude Code, Codex CLI)의 세션 로그를 감시하고 이벤트를 provider별 내장 TSDB(fjall)에 기록하며, rate-limit window 추적과 선택적 동기화를 수행합니다. writer/sync worker는 활성 provider별로 생성되고, 선택 기능의 poller/backfill worker는 필요할 때만 동작합니다.
- **trace** — UDS로 데몬에 연결해서 실시간 JSONL 이벤트 스트림을 받습니다. `--sink` 옵션으로 UDS나 HTTP로 다른 서비스에 중계할 수도 있습니다.
- **report** — 데몬에 쿼리를 보내 provider DB별 결과를 받습니다. Provider마다 다른 토큰 column은 분리해서 유지하며 `--provider`로 하나만 조회할 수 있습니다.

---

## 성능

아래 벤치마크 스냅샷에서는 idle 5MB, CPU 약 0%, 측정한 리포트 약 7ms를 기록했습니다. 대부분의 대안은 실행할 때마다 원본 JSONL을 처음부터 다시 읽지만 toki는 색인된 이벤트 DB를 조회합니다.

[ccusage](https://github.com/ryoppippi/ccusage) (Node.js), [zzusage](https://github.com/joelreymont/zzusage) (Zig)와 동일 데이터셋, `sudo purge` 후 측정.

### Cold Start (전체 파일 색인)

ccusage보다 **14배 빠르고**, zzusage와 비슷한 속도에 **메모리는 93% 적습니다**.

> 실제 사용에서는 체크포인트부터 이어서 처리하므로, 새로 쌓인 데이터만 색인합니다.

<p align="center">
  <img src="docs/bench_cold_start.png" alt="Cold Start 벤치마크" width="900" />
</p>

<details>
<summary>Cold Start 상세 데이터</summary>

#### 실행 시간

| 데이터 | toki | ccusage | zzusage | toki vs ccusage |
|--------|------|---------|---------|-----------------|
| 100 MB | **0.11s** | 2.38s | 0.13s | **21x** 빠름 |
| 200 MB | **0.16s** | 3.09s | 0.18s | **19x** 빠름 |
| 300 MB | **0.27s** | 4.47s | 0.27s | **16x** 빠름 |
| 400 MB | **0.31s** | 5.07s | 0.32s | **16x** 빠름 |
| 500 MB | **0.39s** | 6.06s | 0.40s | **15x** 빠름 |
| 1 GB | **0.78s** | 10.88s | 0.76s | **14x** 빠름 |
| 2 GB | **1.54s** | 21.53s | 1.41s | **14x** 빠름 |

#### 피크 메모리

| 데이터 | toki | ccusage | zzusage |
|--------|------|---------|---------|
| 100 MB | 37 MB | 126 MB | 165 MB |
| 200 MB | 38 MB | 127 MB | 246 MB |
| 300 MB | 67 MB | 127 MB | 421 MB |
| 400 MB | 69 MB | 127 MB | 492 MB |
| 500 MB | 71 MB | 126 MB | 615 MB |
| 1 GB | 119 MB | 127 MB | 1,209 MB |
| 2 GB | 166 MB | 126 MB | **2,311 MB** |

> **zzusage와 속도가 비슷한데 의미가 있나?** toki는 라인마다 이벤트/인덱스 쓰기, 체크포인트 저장, 중복 제거, 스키마 검증을 추가로 수행합니다. zzusage는 이를 생략하지만 이 벤치마크에서는 실행 시간이 거의 같습니다.

</details>

### 저장된 벤치마크의 리포트 속도 (색인 이벤트 DB vs 원본 재스캔)

이 데이터셋에서는 **약 7ms**, 2GB 기준 ccusage보다 **1,742배** 빨랐습니다.
현재 report는 matching usage event를 스캔하므로 이 표는 측정 스냅샷이지
constant-time 보장은 아닙니다.

<p align="center">
  <img src="docs/bench_report.png" alt="리포트 벤치마크" width="900" />
</p>

<details>
<summary>리포트 상세 데이터</summary>

#### 실행 시간

| 데이터 | toki (warm) | toki (cold disk) | ccusage | zzusage | warm vs ccusage | warm vs zzusage |
|--------|-------------|-----------------|---------|---------|-----------------|-----------------|
| 100 MB | **0.007s** | 0.16s | 2.38s | 0.13s | **358x** | **20x** |
| 200 MB | **0.007s** | 0.15s | 3.09s | 0.18s | **435x** | **25x** |
| 300 MB | **0.007s** | 0.15s | 4.47s | 0.27s | **602x** | **37x** |
| 400 MB | **0.008s** | 0.14s | 5.07s | 0.32s | **658x** | **41x** |
| 500 MB | **0.008s** | 0.16s | 6.06s | 0.40s | **785x** | **51x** |
| 1 GB | **0.009s** | 0.15s | 10.88s | 0.76s | **1,153x** | **81x** |
| 2 GB | **0.012s** | 0.17s | 21.53s | 1.41s | **1,742x** | **114x** |

#### 피크 메모리

| 데이터 | toki (warm) | toki (cold disk) | ccusage | zzusage |
|--------|-------------|-----------------|---------|---------|
| 100 MB | 5 MB | 8 MB | 126 MB | 165 MB |
| 500 MB | 5 MB | 8 MB | 126 MB | 615 MB |
| 1 GB | 5 MB | 8 MB | 127 MB | 1,209 MB |
| 2 GB | **10 MB** | 10 MB | 126 MB | **2,311 MB** |

#### 피크 CPU

| 데이터 | toki (warm) | toki (cold disk) | ccusage | zzusage |
|--------|-------------|-----------------|---------|---------|
| 100 MB | 0% | 14% | 101% | 20% |
| 500 MB | 0% | 18% | 100% | 76% |
| 1 GB | 1% | 18% | 100% | 102% |
| 2 GB | 0% | 12% | 101% | 122% |

</details>

### Idle 상태

cold start가 끝나면 toki는 시스템에서 사라집니다.

| CPU | 메모리 | DB 크기 |
|-----|--------|---------|
| **~0%** | **5 MB** | **세션 데이터의 ~3%** (2GB 세션 → 64MB TSDB) |

idle 상태가 있는 건 toki뿐입니다. 나머지는 실행할 때마다 전체 비용을 지불합니다.

> 측정 환경: Apple M1 MacBook Air (8GB RAM), macOS, 절전 모드 off.
> 재현: `sudo -v && python3 benches/benchmark.py run --purge --tool all`

---

## 프라이버시와 보안

toki는 정책이 아닌 아키텍처로 프라이버시를 보장합니다.

- **프롬프트/body 저장 없음**: provider parser는 필요한 usage와 routing 메타데이터(예: Claude `assistant` usage, Codex `token_count`/turn metadata)만 역직렬화합니다. Prompt text, response body, 편집 파일, thinking block은 무시하며 toki DB에 쓰지 않습니다.
- **기본은 로컬 처리**: 파싱과 로컬 리포트는 기기 안에서 처리합니다. 데몬은 GitHub 릴리즈 확인과 LiteLLM 가격표 요청을 할 수 있습니다. Report/query `--no-cost`는 해당 가격 요청을 건너뛰지만 trace `--no-cost`는 field만 제거하며 daemon은 시작할 때 가격을 가져옵니다. opt-in sync를 켜면 토큰 이벤트와 window 메타데이터를 설정한 toki-sync 서버에 보냅니다. 기본 활성화된 Claude window polling은 로컬 Claude 활동 뒤에만 Claude usage/profile endpoint를 호출하며 `window_polling=false`로 끌 수 있습니다.
- **대화 내용 로깅 없음**: TSDB에는 타임스탬프, 모델명, 세션 ID, 소스 파일 경로, 프로젝트명, 토큰 수 정수만 저장됩니다.
- **읽기 전용 접근**: toki는 세션 파일을 읽기만 합니다. CLI 도구의 데이터를 수정하거나 삭제하지 않습니다.

---

## 명령어

가장 많이 쓰는 흐름:

```bash
toki daemon start            # 백그라운드 데몬 시작
toki report                  # 사용량 요약 보기
toki trace                   # 실시간 이벤트 스트림
toki query 'sum by (model)(toki_tokens_total[1h])'   # PromQL 스타일 쿼리
```

전체 명령어 레퍼런스, 쿼리 문법, 설정 옵션, sync 명령은 **[사용법 가이드](docs/USAGE.ko.md)** 참고.

<details>
<summary>전체 명령어 (daemon, report, query, trace, settings, sync)</summary>

### Daemon

```bash
toki daemon start                # 데몬 시작 (백그라운드)
toki daemon start --foreground   # 포그라운드 실행 (디버그용)
toki daemon stop                 # 데몬 중지
toki daemon restart              # 중지 + 재시작 (설정 변경 반영)
toki daemon status               # 실행 상태 확인
toki daemon reset                # 이벤트 DB 재구축; 설정/window 이력 보존
toki daemon enable               # 로그인 시 자동 시작 활성화
toki daemon disable              # 로그인 자동 시작 비활성화
```

### Report

```bash
# 전체 요약
toki report
toki report --provider claude_code
toki report --start 20260301 --end 20260331

# 시간별 그룹핑
toki report daily --start 20260301
toki report weekly --start-of-week tue
toki report monthly

# 세션/프로젝트 필터
toki report --group-by-session
toki report --project toki

# PromQL 쿼리는 최상위 `query` 명령 사용
toki query --start 20260301 --end 20260331 'sum(usage[1d]) by (project)'
toki query --start 20260320 'events'
toki query 'usage[1d] offset 7d'
```

### Query

```bash
# Instant PromQL 쿼리
toki query 'sum by (model)(toki_tokens_total[1h])'
toki query -z Asia/Seoul 'sum by (model)(toki_tokens_total[1d])'

# 명시적 범위; --step은 원격 range-query 버킷을 제어
toki query --start 2026-03-01 --end 2026-03-31 --step 1d 'sum by (model)(toki_tokens_total[1d])'

# type 필터와 =~ 정규식 연산자
toki query 'sum(toki_tokens_total{type=~"input|output"}[1h])'

# 원격 쿼리 (sync 서버 경유)
toki query --remote 'sum by (model)(toki_tokens_total[1h])'

# 출력 형식 및 옵션
toki query -w tue 'sum by (model)(toki_tokens_total[1w])' # 화요일을 주 시작으로 사용
toki query --output-format json 'toki_tokens_total[1h]'
toki query --no-cost 'toki_tokens_total[1h]'
```

> `toki report query`는 제거되었습니다. `--start`와 `--end`를 직접 지원하는 최상위 `toki query`를 사용하세요.

### Rate-limit windows

```bash
toki windows                         # `windows status`와 동일
toki windows status --fresh          # 제한 시간 내 live refresh 요청
toki windows status --json
toki windows list                    # 기본 최근 28일 이력
toki windows list --start 2026-08-01 --end 2026-08-31 --json
toki query windows                   # query 경로로 동일 저장 이력 조회
```

### Trace

```bash
toki trace                                          # JSONL 스트림 (stdout)
toki trace --sink uds:///tmp/toki.sock              # UDS로 중계
toki trace --sink http://localhost:8080/events       # HTTP로 중계
```

### Settings

```bash
toki settings                                  # TUI 열기
toki settings set providers --add codex        # Provider 추가
toki settings list                             # 전체 설정 출력
```

### Sync

```bash
toki settings sync enable --server <host>       # 브라우저를 열어 인증합니다 (device code flow)
toki settings sync disable              # 원격 데이터 삭제 여부를 묻습니다
toki settings sync disable --delete     # 서버에서 이 디바이스의 데이터를 삭제합니다
toki settings sync disable --keep       # 원격 데이터를 유지하고 로컬에서만 비활성화합니다
toki settings sync status                                          # 연결 정보 확인
toki settings sync devices                                         # 등록된 디바이스 목록
toki settings sync rename <new-name>                               # 이 디바이스의 이름 변경
toki settings sync remove <device-id>                              # 다른 디바이스 제거
```

</details>

---

## 멀티 디바이스 동기화

여러 기기의 토큰 사용량을 중앙 [toki-sync](https://github.com/korjwl1/toki-sync) 서버로 동기화합니다. 모든 디바이스의 데이터를 한 곳에서 조회할 수 있습니다 — PromQL 쿼리, 웹 대시보드, [Toki Monitor](https://github.com/korjwl1/toki-monitor) 모두 지원합니다.

### 설정

```bash
# 동기화 서버에 연결 (브라우저를 열어 인증)
toki settings sync enable --server sync.example.com

# 자체 서명 TLS (IP 전용 서버)
toki settings sync enable --server 1.2.3.4 --insecure

# 상태 확인
toki settings sync status

# 등록된 디바이스 목록
toki settings sync devices

# ID로 등록 디바이스 제거
toki settings sync remove <device-id>

# CLI에서 서버 데이터 쿼리
toki query --remote 'sum by (model)(toki_tokens_total)'

# 동기화 비활성화
toki settings sync disable              # 원격 데이터 삭제 여부를 묻습니다
toki settings sync disable --delete     # 서버에서 이 디바이스의 데이터를 삭제합니다
toki settings sync disable --keep       # 원격 데이터를 유지하고 로컬에서만 비활성화합니다
```

### 동작 방식

- 데몬의 sync 스레드가 toki-sync 서버에 TLS TCP로 연결 (persistent connection)
- 이벤트를 배치로 모아서 (1,000/배치) zstd 압축(100개 이상 시) 후 ACK 기반 흐름 제어로 전송
- 연결이 끊기면: 이벤트가 로컬 fjall DB에 누적되고, 재연결 시 delta-sync
- JWT 자동 갱신, 지수 백오프 (2s→300s 상한), wake 감지
- 설정 핫리로드: `toki settings sync enable` 실행 시 데몬 재시작 없이 즉시 반영

### 프라이버시

동기화는 opt-in이며 기본적으로 꺼져 있습니다. 활성화하면 토큰 수와 routing/window 메타데이터(모델, provider, session/project/message identity, timestamp, device, limit/account/plan field)만 전송하며 프롬프트나 응답은 전송하지 않습니다. 명시적으로 insecure한 개발용 `--no-tls`를 선택하지 않는 한 TLS로 암호화합니다. 서버에서 사용자별 데이터는 label injection으로 격리됩니다.

### 현재 원격 쿼리 제한

Sync API에는 아직 cursor pagination이 없어 server가 큰 event scan을 제한하며,
매우 큰 범위는 CLI truncation 경고 없이 부분 결과가 될 수 있습니다. Server는 로컬 query가 지원하는 모든
RFC 3339 bound도 아직 받지 않습니다. 원격 query에는 명시적인 bounded 숫자/date
범위를 권장합니다.

---

## 비용 계산

사용 가능한 가격이 있을 때 usage/event 출력에 모델별 추정 비용(USD)을 포함합니다. 가격 데이터는 [LiteLLM](https://github.com/BerriAI/litellm) 커뮤니티 가격표에서 가져옵니다.

- **최초 실행**: LiteLLM JSON 다운로드 → `litellm_provider` 기준 필터 (Anthropic, OpenAI, Gemini) → 파일 캐시 (`~/.config/toki/pricing.json`)
- **이후 실행**: HTTP ETag 조건부 요청 → 변경 없으면 304 (바디 없이 ~50ms)
- **오프라인**: 캐시된 데이터로 동작. 캐시가 없으면 Cost 컬럼 생략
- **`--no-cost`**: report/query는 해당 가격 fetch를 건너뛰고, trace는 가격을 daemon이 소유하므로 출력의 `cost_usd`만 제거
- **cache-read 가격 누락**: cached input을 무료로 보지 않고 일반 input 가격을 보수적으로 적용
- **Claude fast mode**: LiteLLM에 정확한 `-fast` 행이 없으면 알려진 Opus fast 배수를 적용하며, 정확한 공개 행이 있으면 그 값이 우선

---

## 지원 Provider

| Provider | CLI 도구 | 데이터 형식 | 상태 |
|----------|---------|-------------|------|
| `claude_code` | [Claude Code](https://claude.ai/code) | JSONL (append-only) | 지원 |
| `codex` | [Codex CLI](https://github.com/openai/codex) | JSONL (append-only) | 지원 |
| *(gemini)* | [Gemini CLI](https://github.com/google-gemini/gemini-cli) | JSON (full rewrite) | 예정 |

각 provider는 이벤트 DB(`~/.config/toki/<provider>.fjall`)와 별도의 비재구축형 window 이력 DB(`~/.config/toki/<provider>.windows.fjall`)를 가집니다. 리포트는 기본적으로 모든 활성 provider를 조회하며, `--provider`로 하나만 필터링할 수 있습니다.

---

## 예정된 기능

| 기능 | 설명 | 상태 |
|------|------|------|
| Gemini CLI | Google Gemini CLI provider 지원 | 예정 |
| `toki-sync` | 멀티 디바이스 지원 — 여러 기기 간 사용량 데이터 동기화 | 지원 |

기능 요청이나 버그 리포트는 [이슈](https://github.com/korjwl1/toki/issues)에 남겨주세요.


## 문서

| 문서 | 설명 |
|------|------|
| **[아키텍처 & 설계](docs/DESIGN.ko.md)** | 데몬 worker, 이벤트/window 저장소, 체크포인트 복구, 데이터 흐름 |
| **[사용법 가이드](docs/USAGE.ko.md)** | 상세 명령어 레퍼런스, 출력 형식, 라이브러리 API, 예제 |
| **[JSONL 형식 레퍼런스](docs/claude-code-jsonl-format.ko.md)** | Claude Code JSONL 구조, 라인 타입, 파싱 최적화 |
| **[벤치마크 상세](benches/COMPARISON.ko.md)** | 전체 비교 방법론, 아키텍처 분석, 스케일링 예측 |
| **[Codex CLI 분석](docs/codex-cli-analysis.md)** | Codex CLI 로컬 데이터 형식, 토큰 구조, 파싱 전략 |
| **[Gemini CLI 분석](docs/gemini-cli-analysis.md)** | Gemini CLI 로컬 데이터 형식 분석 (향후 provider) |
| **[왜 OpenTelemetry가 아닌가?](docs/why-not-otel.md)** | toki가 OTEL 데이터 대신 로컬 파일을 파싱하는 이유 |
| **[OTEL 비교](docs/otel-comparison.md)** | OpenTelemetry 구현 상세: Claude Code vs Gemini CLI vs toki |

---

## 기술 스택

| 용도 | 선택 | 근거 |
|------|------|------|
| DB | fjall 3.x | Pure Rust LSM-tree, TSDB keyspace 구조에 적합 |
| 동시성 | std::thread + crossbeam-channel | 런타임 충돌 없음, 라이브러리 안전 |
| 병렬 스캔 | rayon | cold start 세션 파일 병렬 처리 |
| 파일 감시 | notify 6.x | FSEvents (macOS), inotify (Linux), provider별 폴링 전략 |
| 직렬화 | bincode (DB), serde_json (JSONL) | 바이너리 최소 오버헤드 |
| 해시 | xxhash-rust 0.8 (xxh3) | 체크포인트 줄 식별 (30GB/s) |
| HTTP | ureq 2.x | 동기 HTTP, ETag 조건부 요청 |
| CLI | clap 4.x | 서브커맨드, 글로벌 옵션 지원 |
| 테이블 | comfy-table 7.x | Unicode 테이블 렌더링 |
| Sync 프로토콜 | toki-sync-protocol (공유 crate) | Wire-compatible 타입, bincode 직렬화 |
| TLS | native-tls 0.2 | 플랫폼 TLS (sync 연결용) |
| IPC | Unix Domain Socket | 데몬-클라이언트 NDJSON 스트리밍 |

---

## 프로젝트 구조

```text
src/
├── lib.rs                          # Public API: start(), Handle
├── main.rs                         # CLI 바이너리 (clap)
├── config.rs                       # Config + 파일 기반 설정
├── db.rs                           # 이벤트 DB + 별도 window 이력 DB
├── engine.rs                       # TrackerEngine: cold_start + watch_loop
├── writer.rs                       # DB writer thread (DbOp channel)
├── query.rs                        # TSDB 쿼리 엔진 (report용)
├── query_parser.rs                 # PromQL 스타일 쿼리 파서
├── retention.rs                    # 데이터 보존 정책
├── checkpoint.rs                   # 역순 라인 스캔, xxHash3 매칭
├── pricing.rs                      # LiteLLM 가격 fetch, ETag 캐싱
├── windows.rs                      # 버전된 rate-limit window 추적/저장 형식
├── claude_poll.rs                  # 활동 기반 Claude usage/profile polling
├── update.rs                       # 비차단 릴리즈 업데이트 확인/캐시
├── settings.rs                     # Cursive TUI 설정 페이지
├── common/
│   ├── types.rs                    # 공통 타입, trait 정의
│   └── time.rs                     # 고속 타임스탬프 파서 (0.1µs)
├── daemon/                         # 데몬 서버 컴포넌트
│   ├── broadcast.rs                # BroadcastSink (zero-overhead fan-out)
│   ├── listener.rs                 # UDS accept loop + multi-DB 쿼리 병합
│   └── pidfile.rs                  # PID 파일 관리
├── sink/                           # 출력 추상화 (Sink trait)
│   ├── print.rs                    # PrintSink (table/json → stdout)
│   ├── uds.rs                      # UdsSink (NDJSON → UDS)
│   └── http.rs                     # HttpSink (JSON POST)
├── providers/                      # provider별 파서 (Provider trait)
│   ├── mod.rs                      # Provider trait, FileParser trait, registry
│   ├── claude_code/                # Claude Code JSONL 파서
│   │   ├── mod.rs                  # ClaudeCodeProvider impl
│   │   └── parser.rs              # 세션 디스커버리 + 라인 파싱
│   └── codex/                      # Codex CLI JSONL 파서
│       ├── mod.rs                  # CodexProvider impl
│       └── parser.rs              # Stateful 파서 (model tracking)
├── sync/                           # 멀티 디바이스 동기화
│   ├── thread.rs                   # Sync 루프, SyncToggle, wake 감지
│   ├── client.rs                   # TCP+TLS 클라이언트, 인증, 배치 전송
│   ├── protocol.rs                 # toki-sync-protocol 재수출
│   ├── backoff.rs                  # 지수 백오프 (2s→300s)
│   └── credentials.rs             # Keychain (macOS) / sync.json (Linux)
└── platform/mod.rs                 # FSEvents 감시 + provider별 폴링 전략
```

---

## 후원

<a href="https://github.com/sponsors/korjwl1">
  <img src="https://img.shields.io/badge/Sponsor-%E2%9D%A4-pink?style=for-the-badge&logo=github" alt="Sponsor" />
</a>

toki가 도움이 됐다면 후원으로 개발을 지원해주세요.

MIT 라이선스는 상업적 사용을 허용합니다. 후원은 선택 사항이며 지속적인
유지보수에 도움이 됩니다.

---

## 라이선스

[MIT](LICENSE)
