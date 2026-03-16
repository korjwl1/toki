# toki vs ccusage 성능 비교

## 기본 프로필

| 항목 | toki | ccusage |
|------|------|---------|
| 언어 | Rust (Edition 2021) | TypeScript (Node.js/Bun) |
| 실행 모델 | 데몬/클라이언트 (상주) | Batch CLI (실행→집계→종료) |
| DB | fjall (embedded LSM-tree TSDB) | 없음 (stateless) |
| 증분 처리 | 체크포인트 기반 resume | 없음 (매번 전체 재스캔) |
| 중복 제거 | xxHash3-64 line hash | messageId:requestId Set |

---

## 벤치마크 실행

```bash
# 1. 테스트 데이터 생성 (실제 ~/.claude 데이터 기반)
python3 benches/benchmark.py generate --sizes 100,200,500,1000,2000

# 2. 벤치마크 실행
python3 benches/benchmark.py run                       # 둘 다
python3 benches/benchmark.py run --tool toki            # toki만
python3 benches/benchmark.py run --tool ccusage          # ccusage만
python3 benches/benchmark.py run --sizes 100,200         # 특정 크기만

# 3. 차트 생성
python3 benches/benchmark.py plot

# 전체 (생성 + 실행 + 차트)
python3 benches/benchmark.py all
```

- 프로세스 모니터링: 50ms 간격 CPU%/RSS 샘플링
- N회 반복 (기본 3) 후 평균
- 결과: `benches/results/` (CSV + JSON + PNG/SVG 차트)

---

## Phase 1: Cold Start (전체 파일 색인)

> **공정한 비교 지점**: 둘 다 모든 JSONL 파일을 처음부터 읽어서 집계하는 단계.

| 도구 | 동작 |
|------|------|
| toki | `daemon reset` → `daemon start` (cold start 완료까지만 측정) |
| ccusage | `ccusage` (전체 파일 읽기 → 집계 → 출력) |

### 왜 차이가 나는가

1. **병렬 파일 처리**: toki는 rayon으로 모든 세션 파일을 CPU 코어 수만큼 병렬 처리.
   ccusage는 순차 stream readline.
2. **파싱 성능 (3-5x)**: Rust serde_json은 Node.js JSON.parse + Valibot 검증 대비
   라인당 2-5x 빠름.
3. **메모리**: toki는 이벤트를 즉시 accumulate (O(M) 모델 수만큼).
   ccusage는 모든 entry를 배열에 수집 후 groupBy → O(N) 메모리.
4. **추가 오버헤드**: toki는 TSDB에 이벤트/rollup 기록 + 체크포인트 저장.
   ccusage는 출력만 하고 종료. 이 오버헤드에도 불구하고 toki가 빠름.

---

## Phase 2: Report (사전 색인 vs 매번 전체 읽기)

> **핵심 구조적 차이**: toki는 Phase 1에서 이미 색인된 TSDB 데이터를 조회.
> ccusage는 매 실행마다 모든 파일을 다시 읽음.

| 시나리오 | toki 동작 | ccusage 동작 |
|----------|-----------|-------------|
| 전체 요약 | TSDB rollup 조회 | 전체 파일 재스캔 |
| daily/weekly/monthly | TSDB 시간 범위 쿼리 | 전체 파일 재스캔 + 그룹핑 |
| 세션/프로젝트 필터 | TSDB 인덱스 lookup | 전체 파일 재스캔 + 필터 |
| PromQL 쿼리 | TSDB 쿼리 엔진 | 지원 안 함 |

```
toki report:   O(R)     R = rollup 수 (시간 버킷 × 모델 수, 보통 수백 개)
ccusage:       O(N)     N = 전체 라인 수 (수만~수십만)
```

---

## 데이터 규모별 예측

| 규모 | 라인 수 | toki cold start | ccusage | toki report | ccusage report |
|------|---------|-----------------|---------|-------------|----------------|
| 소규모 | 1K | ~100ms | ~300ms | ~5ms | ~300ms |
| 중규모 | 50K | ~3s | ~10s | ~5ms | ~10s |
| 대규모 | 500K | ~20s | ~2min+ | ~5ms | ~2min+ |
| 초대규모 | 5M | ~3min | OOM 위험 | ~5ms | OOM |

> toki report는 TSDB rollup 조회이므로 **원본 데이터 규모와 무관하게 ~5ms**.

---

## 실시간 수집 (Watch Mode)

toki만의 고유 기능. ccusage에는 대응하는 기능이 없다.

| 측면 | toki | ccusage |
|------|------|---------|
| 변화 감지 | FSEvents → checkpoint resume | 매번 전체 재스캔 |
| 지연 시간 | ~1-2ms (이벤트 발생 즉시) | ~5-15초 (배치 재실행) |
| CPU (idle) | ~0% | N/A (실행 안 함) |
| 새 라인 10개 처리 | ~500µs | ~5-15초 (전체 재계산) |
| 서버 전송 | 이벤트 드리븐 push | polling 필요 |
| Time Complexity | O(ΔL) 새 라인만 | O(N) 전체 재처리 |

---

## 아키텍처 비교

```
toki:     O(N) 초기 1회 + O(R) 이후 report + O(ΔL) 실시간
ccusage:  O(N) × 실행 횟수
```

| 항목 | toki | ccusage |
|------|------|---------|
| 증분 처리 | checkpoint + reverse-scan resume | 없음 |
| 파일 compaction 대응 | hash 기반 자동 복구 | 상관없음 (매번 전체 읽기) |
| 메모리 효율 | O(F) 체크포인트 유지 | O(N) 전체 entry 메모리 적재 |
| 바이너리 크기 | ~2-5MB (네이티브) | ~수백 KB + Node.js 런타임 |
| 배포 용이성 | 단일 바이너리 | npm/bun install |
| 가격 계산 | LiteLLM (ETag 캐싱, client-side) | LiteLLM (online/offline) |
| 리포트 다양성 | summary/daily/weekly/monthly/yearly/hourly/session/project/PromQL | daily/weekly/monthly/session |

---

## 결론

| 용도 | 추천 |
|------|------|
| 반복적 리포트 (daily check 등) | **toki** — 색인된 TSDB에서 ~5ms, ccusage는 매번 전체 재스캔 |
| 실시간/준실시간 서버 수집 | **toki** — O(ΔL) 증분처리, ms 지연 |
| 대규모 (500K+ 라인) | **toki** — ccusage는 OOM 위험, toki report는 데이터 규모 무관 |
| 일회성 빠른 확인 | **ccusage** — npm install 한 줄, 데몬 불필요 |
