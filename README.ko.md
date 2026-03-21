<p align="center">
  <img src="assets/logo.png" alt="toki logo" width="160" />
</p>

<h1 align="center">toki</h1>

<p align="center">
  <b>당신의 작업을 방해하지 않는, 가장 영리한 토큰 사용량 트래커</b><br>
  Rust 기반 | 데몬 아키텍처 | Idle 메모리 5MB | 리포트 7ms | 설정 제로
</p>

<p align="center">
  <sub><b>toki</b> = <b>to</b>ken <b>i</b>nspector — '토끼'처럼 빠르고 가벼운 도구를 지향합니다.</sub>
</p>

<p align="center">
  <a href="README.md">🇺🇸 English</a>
</p>

---

### "도구는 도구다워야 합니다."

AI CLI 도구를 쓰면서, 사용량 확인 때문에 터미널이 멈추거나 별도 서버 설정이 필요하다면 그건 도구의 역할을 벗어난 거죠. **toki**는 있는지 없는지 모를 만큼 가볍고 빠르게 돌아가면서, 필요할 때는 강력한 분석까지 해냅니다.

---

## ✨ 핵심 차별점

### 1. 설정 제로 (Zero Configuration)
OpenTelemetry처럼 복잡한 Collector나 환경 변수 설정이 필요 없습니다. 설치하고 실행하면 바로, Claude Code나 Codex CLI가 남긴 로그를 자동으로 찾아 분석합니다.

### 2. 과거 데이터까지 한 번에 (Retroactive Analysis)
대부분의 트래커는 설치 이후의 데이터만 기록하지만, toki는 다릅니다. 설치 전에 이미 쌓여있던 수개월 치 과거 로그도 즉시 인덱싱해서 전체 사용량을 보여줍니다.

### 3. 작업을 방해하지 않는 구조 (Non-blocking Architecture)
toki는 백그라운드 데몬으로 동작합니다. AI CLI 도구가 파일을 쓰는 동안 toki는 조용히 지켜보고 처리할 뿐, 작업 중인 프로세스에는 아무런 영향도 주지 않습니다.

### 4. 압도적인 속도 (Instant Reports)
기존 도구들이 매번 기가바이트 단위의 파일을 처음부터 다시 읽을 때, toki는 전용 시계열 DB(TSDB)를 사용해서 수 기가바이트의 데이터도 단 **7ms** 만에 요약합니다.

---

## 🚀 빠른 시작

### 설치하기 (macOS)
Homebrew를 통해 간편하게 설치할 수 있습니다.

```bash
brew tap korjwl1/tap
brew install toki
```

### 시작하기

toki는 설치된 AI CLI 도구를 자동으로 감지합니다. `~/.claude`(Claude Code)나 `~/.codex`(Codex CLI) 디렉토리가 있으면 별도 설정 없이 바로 추적을 시작합니다.

```bash
# 데몬 시작 (로그 스캔 및 감시 시작)
toki daemon start

# 사용량 리포트 확인
toki report

# 설정 변경이 필요하면 TUI로 간편하게
toki settings
```

`toki settings`를 실행하면 터미널에서 바로 설정 화면이 열립니다. 추적할 provider 선택, 경로 변경, 타임존 설정 등을 할 수 있습니다.

---

## 📊 성능 및 벤치마크

toki는 그냥 빠르기만 한 게 아니라, 하드웨어 자원을 효율적으로 쓰도록 설계했습니다.

### Cold Start (초기 전체 인덱싱)
처음 실행할 때 toki는 `rayon` 기반 멀티스레딩으로 모든 코어를 풀가동합니다. CPU 점유율이 일시적으로 높아 보일 수 있지만, 이는 **기존 데이터를 가장 빠르게 처리하기 위한 의도된 설계**입니다. 이 과정은 딱 한 번만 발생하고, 이후에는 변경된 부분만 증분 처리합니다.

- **ccusage 대비 14배 빠름**
- **메모리 효율성**: zzusage 대비 **93% 적은 메모리** 사용 (2GB 데이터 기준)

<p align="center">
  <img src="docs/bench_cold_start.png" alt="Cold Start 벤치마크" width="800" />
</p>

### 리포트 속도
인덱싱이 끝난 뒤의 조회 속도는 비교 대상이 없습니다. 2GB 데이터 기준, ccusage보다 **1,700배 이상** 빠릅니다.

| 데이터 크기 | toki (TSDB) | ccusage (Full Scan) | zzusage (Full Scan) |
|:---:|:---:|:---:|:---:|
| 100 MB | **0.007s** | 2.38s | 0.13s |
| 1 GB | **0.007s** | 10.88s | 0.76s |
| 2 GB | **0.007s** | 21.53s | 1.41s |

---

## 🛠 기술적 특징

- **Non-blocking I/O**: 메인 작업에 영향을 주지 않는 독립된 데몬 구조.
- **TSDB 기반**: `fjall` 임베디드 시계열 DB로 조회 성능을 극대화.
- **스마트 체크포인트**: `xxHash3` 기반 체크포인트로 중단된 지점부터 정확하게 재개.
- **저사양 친화적**: Idle 상태에서 단 5MB의 메모리만 사용.

---

## 🔍 기존 도구와 비교 (Why toki?)

### vs OpenTelemetry (OTEL)
OTEL은 좋은 표준이지만, 개인 개발자의 CLI 도구 추적에는 좀 과합니다.
- **OTEL**: Collector 서버 필요, 설정 복잡, 과거 데이터 분석 불가, 네트워크 오버헤드.
- **toki**: 서버 불필요, 설정 제로, 과거 데이터 분석 가능, 100% 로컬 동작.

### vs ccusage / zzusage
이 도구들은 리포트를 요청할 때마다 수백 개의 JSONL 파일을 처음부터 끝까지 다시 읽습니다.
- **ccusage/zzusage**: 조회 시마다 CPU/IO 부하 발생, 대용량 데이터에서 터미널 프리징.
- **toki**: 백그라운드에서 증분 인덱싱, 조회는 즉각적인 DB 쿼리로 처리.

---

## 📝 라이선스
이 프로젝트는 FSL-1.1-Apache-2.0 라이선스를 따릅니다.

---
<p align="center">
  Built with 🦀 by <a href="https://github.com/korjwl1">korjwl1</a>
</p>
