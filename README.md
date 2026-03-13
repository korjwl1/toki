# clitrace

AI CLI 도구(Claude Code 등)의 JSONL 세션 로그를 실시간 감시하여 모델별 토큰 사용량을 추적하는 Rust 도구.
내장 TSDB에 이벤트와 시간별 rollup을 저장하고, 실시간 trace와 one-shot report를 모두 지원한다.

## Quick Start

```bash
# 데몬 실행: cold start → watch mode 진입
cargo run --release -- trace

# 리포트 조회 (TSDB에서 즉시 조회)
cargo run --release -- report
cargo run --release -- report daily --since 20260301
cargo run --release -- report monthly
```

## Trace (Watch Mode)

`trace`는 데몬 프로세스로 동작한다. 시작 시 전체 세션 파일을 스캔(cold start)하여 TSDB를 구축하고,
이후 파일 변경을 감시하며 새 이벤트를 실시간으로 수집한다.

```bash
cargo run --release -- trace
cargo run --release -- trace --startup-group-by day
cargo run --release -- trace --session-id 4de9291e
cargo run --release -- trace --project clitrace
cargo run --release -- trace --full-rescan
```

| 옵션 | 설명 |
|------|------|
| `--startup-group-by hour\|day\|week\|month\|year` | cold start 시 시간 단위 그룹핑 요약 |
| `--session-id <PREFIX>` | 세션 UUID 접두사 필터 |
| `--project <NAME>` | 프로젝트 디렉토리 서브스트링 필터 |
| `--full-rescan` | 체크포인트 초기화 후 전체 재스캔 |

- 동일 DB 경로 기준 단일 인스턴스만 허용 (file lock)
- `--startup-group-by hour`는 기존 체크포인트 필요, `--full-rescan`과 함께 사용 불가

## Report (One-Shot)

`report`는 TSDB에 저장된 데이터를 즉시 조회한다. TSDB가 비어있으면 JSONL 파일을 직접 스캔한다.

```bash
# 전체 요약
cargo run --release -- report
cargo run --release -- report --since 20260301
cargo run --release -- report --since 20260301 --until 20260331

# 시간별 그룹핑
cargo run --release -- report daily --since 20260301
cargo run --release -- report weekly --since 20260301 --start-of-week tue
cargo run --release -- report monthly
cargo run --release -- report yearly
cargo run --release -- report hourly --from-beginning

# 세션별 그룹핑
cargo run --release -- report --group-by-session
cargo run --release -- report --session-id 4de9291e

# 프로젝트 필터
cargo run --release -- report --project clitrace
cargo run --release -- report --project ddleague daily --since 20260301

# 타임존 지정
cargo run --release -- -z Asia/Seoul report daily --since 20260301

# 비용 표시 없이
cargo run --release -- --no-cost report
```

| 옵션 | 설명 |
|------|------|
| 서브커맨드 없음 | 전체 총합 (`--since`/`--until` 선택적) |
| `daily\|weekly\|monthly\|yearly\|hourly` | 시간별 그룹핑 |
| `--since YYYYMMDD[hhmmss]` | 시작 시점 (inclusive, `>=`) |
| `--until YYYYMMDD[hhmmss]` | 종료 시점 (inclusive, `<=`) |
| `--from-beginning` | `--since` 없이 전체 그룹핑 허용 |
| `--group-by-session` | 세션별 그룹핑 (시간 서브커맨드와 동시 사용 불가) |
| `--start-of-week mon\|tue\|...\|sun` | `weekly`에서만 사용 |

- `YYYYMMDD`는 `--since`일 때 `00:00:00`, `--until`일 때 `23:59:59`로 해석
- `hourly`, `daily`, `weekly`는 `--since` 또는 `--from-beginning` 필수

## Global Options

| 옵션 | 설명 |
|------|------|
| `--output-format table\|json` | print sink 출력 형식 (기본: table) |
| `--sink <SPEC>` | 출력 대상, 복수 지정 가능 |
| `--timezone <IANA>` / `-z` | 타임존 (기본: UTC) |
| `--no-cost` | 비용 계산 비활성화 |

### Sink 종류

| Sink | 설명 |
|------|------|
| `print` (기본) | 터미널 출력 (`--output-format`에 따라 table/json) |
| `uds://<path>` | Unix Domain Socket으로 NDJSON 전송 |
| `http://<url>` | HTTP POST로 JSON 전송 (5초 timeout) |

```bash
# 터미널 + HTTP 동시 출력
cargo run --release -- trace --sink print --sink http://localhost:8080/events
```

## Cost Calculation

모든 출력에 모델별 추정 비용(USD)이 표시된다.
가격 데이터는 [LiteLLM](https://github.com/BerriAI/litellm) 커뮤니티 가격표에서 가져온다.

- **최초 실행**: LiteLLM JSON 다운로드 → Claude 모델 추출 → DB 캐시
- **이후 실행**: HTTP ETag 조건부 요청 → 변경 없으면 304 (바디 없이 ~50ms)
- **오프라인**: 캐시된 데이터로 동작, 캐시 없으면 Cost 컬럼 생략
- **`--no-cost`**: 가격 fetch 스킵
- 현재 시점 가격을 전체 데이터에 일괄 적용 (역사적 가격 추적 없음)

## Environment Variables

| 변수 | 설명 | 기본값 |
|------|------|--------|
| `CLITRACE_CLAUDE_ROOT` | Claude Code 루트 디렉토리 | `~/.claude` |
| `CLITRACE_DB_PATH` | DB 디렉토리 경로 | `~/.config/clitrace/clitrace.fjall` |
| `CLITRACE_DEBUG` | 디버그 로그 (1=normal, 2=verbose) | 0 |
| `CLITRACE_RETENTION_DAYS` | 이벤트 보존 기간 (일) | 90 |
| `CLITRACE_ROLLUP_RETENTION_DAYS` | Rollup 보존 기간 (일) | 365 |

## Library Usage

```toml
[dependencies]
clitrace = { path = "." }
```

```rust
use clitrace::{Config, start};
use clitrace::sink::{PrintSink, OutputFormat};

fn main() {
    let config = Config::new()
        .with_claude_root("/custom/.claude".to_string());

    let sink = Box::new(PrintSink::new(OutputFormat::Table));
    let handle = start(config, None, sink, false).expect("Failed to start");

    // ... application logic ...

    handle.stop(); // 또는 drop 시 자동 종료
}
```

## Output Examples

### Table (기본)

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

### JSON (`--output-format json`)

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

### Watch Mode (실시간)

```
[clitrace] claude-opus-4-6 | session.jsonl | in:3 cc:5139 cr:9631 out:14 | $0.0112
```

## Project Structure

```
├── Cargo.toml
├── README.md                              # 이 파일
├── docs/
│   ├── DESIGN.md                          # 아키텍처 및 내부 설계
│   ├── USAGE.md                           # 상세 사용법 및 출력 형식
│   └── claude-code-jsonl-format.md        # Claude Code JSONL 구조 참고
├── specs/
│   └── constitution.md                    # 프로젝트 원칙 (Constitution)
├── Comparison.md                          # ccusage 성능 비교
└── src/
    ├── lib.rs                             # Public API: start(), Handle
    ├── main.rs                            # CLI 바이너리 (clap)
    ├── config.rs                          # Config + 환경변수/DB 우선순위
    ├── db.rs                              # fjall 래퍼 (7 keyspaces)
    ├── engine.rs                          # TrackerEngine: cold_start + watch_loop
    ├── writer.rs                          # DB writer thread (DbOp channel)
    ├── query.rs                           # TSDB 쿼리 (report용)
    ├── retention.rs                       # 데이터 보존 정책 (자동 삭제)
    ├── checkpoint.rs                      # 역순 라인 스캔, xxHash3 매칭
    ├── pricing.rs                         # LiteLLM 가격 fetch, ETag 캐싱
    ├── common/
    │   └── types.rs                       # 공통 타입, trait 정의
    ├── sink/                              # 출력 추상화 (Sink trait)
    │   ├── mod.rs                         # Sink trait, MultiSink
    │   ├── json.rs                        # JSON 직렬화 (공용)
    │   ├── print.rs                       # PrintSink (table/json → stdout)
    │   ├── uds.rs                         # UdsSink (NDJSON → UDS)
    │   └── http.rs                        # HttpSink (JSON POST)
    ├── providers/
    │   └── claude_code/parser.rs          # JSONL 파싱 + 세션 디스커버리
    └── platform/
        └── macos/mod.rs                   # macOS FSEvents 감시
```

## Tech Stack

| 용도 | 선택 | 근거 |
|------|------|------|
| DB | fjall 3.x | Pure Rust LSM-tree, TSDB keyspace 구조에 적합 |
| 동시성 | std::thread + crossbeam-channel | 런타임 충돌 없음, 라이브러리 안전 |
| 병렬 스캔 | rayon | cold start 세션 파일 병렬 처리 |
| 파일 감시 | notify 6.x | macOS FSEvents 자동 사용 |
| 직렬화 | bincode 1.x (DB), serde_json (JSONL) | 바이너리 최소 오버헤드 |
| 해시 | xxhash-rust 0.8 (xxh3) | 체크포인트 줄 식별 (30GB/s) |
| HTTP | ureq 2.x | 동기 HTTP, ETag 조건부 요청 |
| CLI | clap 4.x | 서브커맨드, 글로벌 옵션 지원 |
| 테이블 | comfy-table 7.1 | Unicode 테이블 렌더링 |
