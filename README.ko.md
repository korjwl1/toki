<p align="center">
  <img src="assets/logo.png" alt="toki logo" width="160" />
</p>

<h1 align="center">toki</h1>

<p align="center">
  <b>AI CLI 도구를 위한 초고속 토큰 사용량 트래커</b><br>
  Rust로 구축. 데몬 기반. TSDB에 색인. 리포트는 언제나 13ms.
</p>

<p align="center">
  <sub><b>toki</b> = <b>to</b>ken <b>i</b>nspector — 발음이 토끼(rabbit)와 비슷하고, 토끼의 빠른 이미지는 이 도구의 성능 철학과 잘 어울린다.</sub>
</p>

<p align="center">
  <a href="README.md">🇺🇸 English</a>
</p>

---

> **바이브 코딩이 아닙니다.** toki의 모든 모듈은 전문 시스템 엔지니어가 직접 설계하고 최적화했습니다 — TSDB 스키마와 rollup-on-write 전략부터 xxHash3 체크포인트 복구까지. AI가 생성한 아키텍처도, 복사-붙여넣기 추상화도 없습니다. 오직 의도적인 엔지니어링만 있습니다.

---

## 성능

toki가 존재하는 이유. [ccusage](https://github.com/ryoppippi/ccusage) (Node.js), [zzusage](https://github.com/nickarellano/zzusage) (Zig)와 동일 데이터셋으로 벤치마크, 매 실행 전 `sudo purge`로 디스크 캐시 초기화.

### 리포트 속도 (색인 조회 vs 전체 재스캔)

toki report는 데이터 크기와 무관하게 **~13ms 고정** (UDS 쿼리 → TSDB rollup 조회).
ccusage와 zzusage는 매번 모든 파일을 처음부터 다시 읽어야 한다.

| 데이터 | toki | ccusage | zzusage | vs ccusage | vs zzusage |
|--------|------|---------|---------|------------|------------|
| 100 MB | **0.013s** | 2.37s | 0.12s | **182x** 빠름 | **9x** 빠름 |
| 500 MB | **0.013s** | 6.05s | 0.35s | **465x** 빠름 | **27x** 빠름 |
| 1 GB | **0.013s** | 11.07s | 0.65s | **851x** 빠름 | **50x** 빠름 |
| 2 GB | **0.013s** | 21.73s | 1.22s | **1,672x** 빠름 | **94x** 빠름 |

### Cold Start (전체 파일 색인)

toki는 파싱과 **동시에** TSDB에 색인까지 하면서도, 파싱만 하는 도구보다 빠르다.

| 데이터 | toki | ccusage | zzusage | vs ccusage | vs zzusage |
|--------|------|---------|---------|------------|------------|
| 100 MB | 0.11s | 2.37s | 0.12s | **21x** | ~1.0x |
| 500 MB | 0.39s | 6.05s | 0.35s | **16x** | ~0.9x |
| 1 GB | 0.78s | 11.07s | 0.65s | **14x** | ~0.8x |
| 2 GB | 1.54s | 21.73s | 1.22s | **14x** | ~0.8x |

### 메모리 & CPU

| 데이터 | toki | ccusage | zzusage |
|--------|------|---------|---------|
| 500 MB | **83 MB** | 126 MB | 613 MB |
| 2 GB | **161 MB** | 126 MB | 2,311 MB |

- **toki** — 파일별 스트리밍 처리, mmap zero-copy. 메모리가 flat하게 유지됨.
- **ccusage** — Node.js 힙 ~126MB 고정, 순차 처리 후 GC.
- **zzusage** — 전체 이벤트를 메모리에 적재. 데이터 크기에 비례하여 폭증 (2GB 데이터 → 2.3GB RAM).

toki report: **~5 MB RSS, ~0% CPU**. 데이터 크기는 무관.

> 측정 환경: Apple M1 MacBook Air (8GB RAM), macOS, 절전 모드 off.
> 재현: `sudo -v && python3 benches/benchmark.py run --purge --tool all`

---

## 동작 방식

Docker처럼 데몬/클라이언트 구조로 동작한다:

```
toki daemon start     # 항상 실행되는 서버   (≈ dockerd)
toki trace            # 실시간 스트림        (≈ docker logs -f)
toki report           # 즉시 TSDB 조회      (≈ docker ps)
```

- **daemon** — Claude Code JSONL 세션 로그를 FSEvents로 감시, 이벤트를 파싱하여 내장 TSDB(fjall)에 기록. trace 클라이언트가 없을 때는 Sink 오버헤드 0.
- **trace** — UDS로 데몬에 연결, 실시간 이벤트 스트림 수신. `print`, `uds://`, `http://` 모든 sink 지원.
- **report** — UDS로 데몬에 쿼리 전송, TSDB 결과 수신. 언제나 빠르고, 언제나 색인됨.

---

## Quick Start

```bash
# 빌드
cargo build --release
# 바이너리: target/release/toki — PATH에 추가하거나 직접 실행

# 1. 데몬 시작 (foreground, Ctrl+C으로 종료)
toki daemon start

# 2. 다른 터미널에서 실시간 이벤트 스트림
toki trace

# 3. 리포트 조회 (TSDB에서 즉시 조회)
toki report
toki report daily --since 20260301
toki report monthly

# 4. PromQL 스타일 쿼리
toki report query 'usage{model="claude-opus-4-6"}[1h] by (model)'
toki report query 'sessions{project="myapp"}'
```

---

## 명령어

### Daemon

```bash
toki daemon start       # 데몬 시작 (foreground)
toki daemon stop        # 데몬 중지
toki daemon restart     # 중지 + 재시작 (설정 변경 반영)
toki daemon status      # 실행 상태 확인
toki daemon reset       # DB 전체 삭제 + 초기화
```

### Report

```bash
# 전체 요약
toki report
toki report --since 20260301 --until 20260331

# 시간별 그룹핑
toki report daily --since 20260301
toki report weekly --since 20260301 --start-of-week tue
toki report monthly
toki report yearly
toki report hourly --from-beginning

# 세션/프로젝트 필터
toki report --group-by-session
toki report --project toki
toki report --session-id 4de9291e

# PromQL 스타일 쿼리
toki report query 'usage{model="claude-opus-4-6"}[1h] by (model)'
toki report query 'usage{session="4de9", since="20260301"} by (session)'
toki report query 'sessions{project="myapp"}'
toki report query 'projects'

# 옵션
toki -z Asia/Seoul report daily --since 20260301   # 타임존 지정
toki --no-cost report                               # 비용 표시 없이
```

<details>
<summary>리포트 옵션 레퍼런스</summary>

| 옵션 | 설명 |
|------|------|
| *(서브커맨드 없음)* | 전체 총합 (`--since`/`--until` 선택적) |
| `daily\|weekly\|monthly\|yearly\|hourly` | 시간별 그룹핑 |
| `query '<PROMQL>'` | PromQL 스타일 자유 쿼리 |
| `--since YYYYMMDD[hhmmss]` | 시작 시점 (inclusive, `>=`) |
| `--until YYYYMMDD[hhmmss]` | 종료 시점 (inclusive, `<=`) |
| `--from-beginning` | `--since` 없이 전체 그룹핑 허용 |
| `--group-by-session` | 세션별 그룹핑 (시간 서브커맨드와 동시 사용 불가) |
| `--session-id <PREFIX>` | 세션 UUID 접두사 필터 |
| `--project <NAME>` | 프로젝트 디렉토리 서브스트링 필터 |
| `--start-of-week mon\|tue\|...\|sun` | `weekly`에서만 사용 |

</details>

<details>
<summary>PromQL 쿼리 문법</summary>

```
metric{filters}[bucket] by (dimensions)
```

| 요소 | 설명 | 예시 |
|------|------|------|
| metric | `usage`, `sessions`, `projects` | `usage` |
| filters | `key="value"` 쌍, `,`로 구분 | `{model="claude-opus-4-6", since="20260301"}` |
| bucket | 시간 버킷 (s/m/h/d/w) | `[1h]`, `[5m]`, `[1d]` |
| dimensions | 그룹 기준 (model/session/project) | `by (model, session)` |

필터 키: `model`, `session`, `project`, `since`, `until`

</details>

### Trace

```bash
toki trace                                              # 기본 (터미널 출력)
toki trace --sink print --sink http://localhost:8080     # 멀티 싱크
```

### Settings

```bash
toki settings                              # TUI 열기 (cursive)
toki settings set claude_code_root /path   # 개별 설정 변경
toki settings get timezone                 # 설정 조회
toki settings list                         # 전체 설정 출력
```

<details>
<summary>설정 레퍼런스</summary>

| 설정 항목 | 설명 | 기본값 |
|-----------|------|--------|
| Claude Code Root | Claude Code 루트 디렉토리 | `~/.claude` |
| Daemon Socket | 데몬 UDS 소켓 경로 | `~/.config/toki/daemon.sock` |
| Timezone | IANA 타임존 (빈값=UTC) | *(없음)* |
| Output Format | 기본 출력 형식 | `table` |
| Start of Week | 주간 리포트 시작 요일 | `mon` |
| No Cost | 비용 계산 비활성화 | `false` |
| Retention Days | 이벤트 보존 기간 (0=무제한) | `0` |
| Rollup Retention Days | Rollup 보존 기간 (0=무제한) | `0` |

우선순위: **CLI 인자 > settings.json > 기본값**

</details>

### 클라이언트 옵션 (trace / report)

| 옵션 | 설명 |
|------|------|
| `--output-format table\|json` | 출력 형식 오버라이드 |
| `--sink <SPEC>` | 출력 대상, 복수 지정 가능 |
| `--timezone <IANA>` / `-z` | 타임존 오버라이드 |
| `--no-cost` | 비용 계산 비활성화 오버라이드 |

---

## 문서

| 문서 | 설명 |
|------|------|
| **[아키텍처 & 설계](docs/DESIGN.md)** | 데몬 스레드, TSDB 스키마, rollup 전략, 체크포인트 복구, 데이터 흐름 |
| **[사용법 가이드](docs/USAGE.md)** | 상세 명령어 레퍼런스, 출력 형식, 라이브러리 API, 예제 |
| **[JSONL 형식 레퍼런스](docs/claude-code-jsonl-format.md)** | Claude Code JSONL 구조, 라인 타입, 파싱 최적화 |
| **[벤치마크 상세](benches/COMPARISON.md)** | 전체 비교 방법론, 아키텍처 분석, 스케일링 예측 |

---

## 비용 계산

모든 출력에 모델별 추정 비용(USD)이 표시된다. 가격 데이터는 [LiteLLM](https://github.com/BerriAI/litellm) 커뮤니티 가격표에서 가져온다.

- **최초 실행**: LiteLLM JSON 다운로드 → Claude 모델 추출 → 파일 캐시 (`~/.config/toki/pricing.json`)
- **이후 실행**: HTTP ETag 조건부 요청 → 변경 없으면 304 (바디 없이 ~50ms)
- **오프라인**: 캐시된 데이터로 동작, 캐시 없으면 Cost 컬럼 생략
- **`--no-cost`**: 가격 fetch 스킵

---

## 기술 스택

| 용도 | 선택 | 근거 |
|------|------|------|
| DB | fjall 3.x | Pure Rust LSM-tree, TSDB keyspace 구조에 적합 |
| 동시성 | std::thread + crossbeam-channel | 런타임 충돌 없음, 라이브러리 안전 |
| 병렬 스캔 | rayon | cold start 세션 파일 병렬 처리 |
| 파일 감시 | notify 6.x | macOS FSEvents 자동 사용 |
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
├── common/types.rs                 # 공통 타입, trait 정의
├── daemon/                         # 데몬 서버 컴포넌트
│   ├── broadcast.rs                # BroadcastSink (zero-overhead fan-out)
│   ├── listener.rs                 # UDS accept loop
│   └── pidfile.rs                  # PID 파일 관리
├── sink/                           # 출력 추상화 (Sink trait)
│   ├── print.rs                    # PrintSink (table/json → stdout)
│   ├── uds.rs                      # UdsSink (NDJSON → UDS)
│   └── http.rs                     # HttpSink (JSON POST)
├── providers/claude_code/parser.rs # JSONL 파싱 + 세션 디스커버리
└── platform/macos/mod.rs           # macOS FSEvents 감시
```

---

## 라이선스

[FSL-1.1-Apache-2.0](LICENSE)
