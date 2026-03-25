<p align="center">
  <img src="assets/logo.png" alt="toki logo" width="160" />
</p>

<h1 align="center">toki</h1>

<p align="center">
  <b>존재감 없는 토큰 사용량 트래커</b><br>
  Rust로 구축. 데몬 기반. idle 5MB. 리포트 7ms. 작업을 전혀 방해하지 않습니다.
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

---

## 목차

- [Quick Start](#quick-start)
- [누가 쓰면 좋을까?](#누가-쓰면-좋을까)
- [동작 방식](#동작-방식)
- [성능](#성능)
- [프라이버시 & 보안](#프라이버시--보안)
- [명령어](#명령어)
- [비용 계산](#비용-계산)
- [지원 Provider](#지원-provider)
- [예정된 기능](#예정된-기능)
- [후원](#후원)
- [라이선스](#라이선스)

---

## Quick Start

```bash
# 설치 (macOS)
brew tap korjwl1/tap
brew install toki

# toki는 ~/.claude, ~/.codex를 자동 감지합니다. 별도 설정 불필요.

# 1. 데몬 시작
toki daemon start

# 2. 실시간 이벤트 스트림 (다른 터미널에서)
toki trace

# 3. 리포트 조회
toki report daily --since 20260301
toki report --provider claude_code
toki report monthly

# 4. PromQL 스타일 쿼리
toki report query 'sum(usage{since="20260301"}[1d]) by (project)'
toki report query 'events{since="20260320"}'
```

---

## 누가 쓰면 좋을까?

- **토큰 리포트 볼 때마다 터미널이 멈추는 분.** toki는 cold start 14배, 리포트 1,700배 빠릅니다. 2GB 데이터도 7ms면 나옵니다.

- **"총 토큰" 이상의 분석이 필요한 분.** 모델별, 세션별, 프로젝트별, 날짜별 분석을 PromQL 스타일로 자유롭게. 시간 범위 필터, 다차원 그룹핑, 비용 추적까지 한 줄이면 됩니다.

- **OpenTelemetry 설정이 귀찮은 분.** Collector도, 환경변수도, 설정 파일 수정도 필요 없습니다. toki를 설치하고 실행하면 디스크에 있는 세션 로그를 바로 읽습니다. 설치 전에 쌓인 수개월치 데이터도 즉시 분석됩니다.

- **여러 AI CLI 도구를 쓰는 분.** Claude Code와 Codex CLI를 하나의 통합 뷰로 볼 수 있습니다. `--provider`로 도구별 필터링도 됩니다.

---

## 동작 방식

Docker처럼 데몬/클라이언트 구조입니다:

```
toki daemon start     # 항상 실행되는 서버   (≈ dockerd)
toki trace            # 실시간 스트림        (≈ docker logs -f)
toki report           # 즉시 TSDB 조회      (≈ docker ps)
```

- **daemon** — 설정된 provider(Claude Code, Codex CLI)의 세션 로그를 감시하고, 이벤트를 파싱해서 provider별 내장 TSDB(fjall)에 기록합니다. 기본 4스레드 + trace 클라이언트당 2스레드. trace 클라이언트가 없으면 Sink 오버헤드는 0입니다.
- **trace** — UDS로 데몬에 연결해서 실시간 JSONL 이벤트 스트림을 받습니다. `--sink` 옵션으로 UDS나 HTTP로 다른 서비스에 중계할 수도 있습니다.
- **report** — 데몬에 쿼리를 보내고, 모든 provider TSDB의 결과를 병합하여 받습니다. 언제나 빠르고, 언제나 색인된 상태. `--provider`로 단일 provider만 조회할 수도 있습니다.

---

## 성능

idle 5MB, CPU 0%, 리포트 7ms. 대부분의 대안은 실행할 때마다 전체 파일을 처음부터 다시 읽습니다. toki는 한 번 색인하고 그 뒤로는 사라집니다.

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
| 500 MB | 71 MB | 126 MB | 615 MB |
| 1 GB | 119 MB | 127 MB | 1,209 MB |
| 2 GB | 166 MB | 126 MB | **2,311 MB** |

> **zzusage와 속도가 비슷한데 의미가 있나?** toki는 라인마다 더 많은 일을 합니다 — TSDB 쓰기, rollup 집계, 체크포인트 저장, 스키마 검증. zzusage는 이걸 전부 생략하고 순수 파싱만 합니다. 그런데도 실행 시간은 거의 같습니다.

</details>

### 리포트 속도 (색인 TSDB 조회 vs 전체 재스캔)

데이터 크기와 무관하게 **~7ms**. 2GB 기준 ccusage보다 **1,742배** 빠릅니다.

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

## 프라이버시 & 보안

toki는 정책이 아닌 아키텍처로 프라이버시를 보장합니다.

- **프롬프트 접근 없음**: JSONL 파서는 `"assistant"` 라인에서 토큰 수와 모델명만 추출합니다. 프롬프트, 응답, 파일 내용, thinking 블록은 메모리에 로드되지 않습니다 — serde가 힙 할당 없이 건너뜁니다.
- **데이터 전송 없음**: 모든 처리는 로컬에서 이루어집니다. 유일한 외부 요청은 LiteLLM 가격표 fetch뿐입니다 (`--no-cost`로 비활성화).
- **대화 내용 로깅 없음**: TSDB에는 타임스탬프, 모델명, 세션 ID, 소스 파일 경로, 프로젝트명, 토큰 수 정수만 저장됩니다.
- **읽기 전용 접근**: toki는 세션 파일을 읽기만 합니다. CLI 도구의 데이터를 수정하거나 삭제하지 않습니다.

---

## 명령어

### Daemon

```bash
toki daemon start                # 데몬 시작 (백그라운드)
toki daemon start --foreground   # 포그라운드 실행 (디버그용)
toki daemon stop                 # 데몬 중지
toki daemon restart              # 중지 + 재시작 (설정 변경 반영)
toki daemon status               # 실행 상태 확인
toki daemon reset                # DB 전체 삭제 + 초기화
```

### Report

```bash
# 전체 요약
toki report
toki report --provider claude_code
toki report --since 20260301 --until 20260331

# 시간별 그룹핑
toki report daily --since 20260301
toki report weekly --start-of-week tue
toki report monthly

# 세션/프로젝트 필터
toki report --group-by-session
toki report --project toki

# PromQL 스타일 쿼리
toki report query 'sum(usage[1d]) by (project)'
toki report query 'events{since="20260320"}'
toki report query 'usage[1d] offset 7d'
```

전체 명령어 레퍼런스, 쿼리 문법, 설정 옵션은 **[사용법 가이드](docs/USAGE.ko.md)**를 참고하세요.

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

---

## 비용 계산

모든 출력에 모델별 추정 비용(USD)이 포함됩니다. 가격 데이터는 [LiteLLM](https://github.com/BerriAI/litellm) 커뮤니티 가격표에서 가져옵니다.

- **최초 실행**: LiteLLM JSON 다운로드 → `litellm_provider` 기준 필터 (Anthropic, OpenAI, Gemini) → 파일 캐시 (`~/.config/toki/pricing.json`)
- **이후 실행**: HTTP ETag 조건부 요청 → 변경 없으면 304 (바디 없이 ~50ms)
- **오프라인**: 캐시된 데이터로 동작. 캐시가 없으면 Cost 컬럼 생략
- **`--no-cost`**: 가격 fetch를 건너뜁니다

---

## 지원 Provider

| Provider | CLI 도구 | 데이터 형식 | 상태 |
|----------|---------|-------------|------|
| `claude_code` | [Claude Code](https://claude.ai/code) | JSONL (append-only) | 지원 |
| `codex` | [Codex CLI](https://github.com/openai/codex) | JSONL (append-only) | 지원 |
| *(gemini)* | [Gemini CLI](https://github.com/google-gemini/gemini-cli) | JSON (full rewrite) | 예정 |

각 provider는 독립된 데이터베이스(`~/.config/toki/<provider>.fjall`)를 가집니다. 리포트는 기본적으로 모든 활성 provider의 결과를 병합하며, `--provider`로 단일 provider만 필터링할 수 있습니다.

---

## 예정된 기능

| 기능 | 설명 | 상태 |
|------|------|------|
| Gemini CLI | Google Gemini CLI provider 지원 | 예정 |
| `toki-sync` | 멀티 디바이스 지원 — 여러 기기 간 사용량 데이터 동기화 | 예정 |

기능 요청이나 버그 리포트는 [이슈](https://github.com/korjwl1/toki/issues)에 남겨주세요.


## 문서

| 문서 | 설명 |
|------|------|
| **[아키텍처 & 설계](docs/DESIGN.ko.md)** | 데몬 스레드, TSDB 스키마, rollup 전략, 체크포인트 복구, 데이터 흐름 |
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
| 테이블 | comfy-table 7.1 | Unicode 테이블 렌더링 |
| IPC | Unix Domain Socket | 데몬-클라이언트 NDJSON 스트리밍 |

---

## 프로젝트 구조

```
src/
├── lib.rs                          # Public API: start(), Handle
├── main.rs                         # CLI 바이너리 (clap)
├── config.rs                       # Config + 파일 기반 설정
├── db.rs                           # fjall 래퍼 (7 keyspaces)
├── engine.rs                       # TrackerEngine: cold_start + watch_loop
├── writer.rs                       # DB writer thread (DbOp channel)
├── query.rs                        # TSDB 쿼리 엔진 (report용)
├── query_parser.rs                 # PromQL 스타일 쿼리 파서
├── retention.rs                    # 데이터 보존 정책
├── checkpoint.rs                   # 역순 라인 스캔, xxHash3 매칭
├── pricing.rs                      # LiteLLM 가격 fetch, ETag 캐싱
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
└── platform/mod.rs                 # FSEvents 감시 + provider별 폴링 전략
```

---

## 후원

<a href="https://github.com/sponsors/korjwl1">
  <img src="https://img.shields.io/badge/Sponsor-%E2%9D%A4-pink?style=for-the-badge&logo=github" alt="Sponsor" />
</a>

toki가 도움이 됐다면 후원으로 개발을 지원해주세요.

유료 제품에 toki를 사용하시려면 후원 또는 [연락](mailto:korjwl1@gmail.com)을 부탁드립니다.

---

## 라이선스

[FSL-1.1-Apache-2.0](LICENSE)
