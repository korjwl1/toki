# Database Comparison: fjall vs tsink vs ugnos vs InfluxDB

toki의 현재 아키텍처와 요구사항을 기준으로, 각 DB 엔진으로 전환했을 때의 득실을 분석한다.

## toki의 DB 사용 패턴 요약

| 항목 | 현재 구현 |
|------|-----------|
| 데이터 모델 | 이벤트(32B StoredEvent) + 시간별 롤업(40B RollupValue) |
| 쓰기 패턴 | 배치 트랜잭션 (cold start: 1024개 청크, watch: 64개 + 1초 flush) |
| 읽기 패턴 | 롤업 범위 스캔 (fast path) / 이벤트 범위 스캔 + dict lookup (filter path) |
| 키 설계 | `[BE u64 timestamp][string suffix]` → 시간순 정렬 보장 |
| 보조 구조 | 딕셔너리 인코딩 (string→u32), 세션/프로젝트 인덱스, 체크포인트 |
| 데이터 규모 | 100MB~2GB (수십만~수백만 이벤트), 단일 머신 로컬 |
| 성능 목표 | idle 5MB, 리포트 7ms, cold start 2GB에 1.5초 |

---

## 1. fjall (현재) — 범용 embedded LSM KV store

### 장점 (toki에 맞는 이유)

- **완전한 제어권**: 7개 keyspace를 자유롭게 설계. 이벤트, 롤업, 딕셔너리, 인덱스, 체크포인트, 메타를 각각 독립된 keyspace로 분리해 최적화.
- **배치 트랜잭션**: `OwnedWriteBatch`로 이벤트 + 인덱스 + 롤업 + 딕셔너리를 단일 원자적 배치로 커밋. rollup-on-write 패턴이 자연스럽게 가능.
- **딕셔너리 인코딩 직접 구현**: string→u32 매핑을 직접 관리해서 StoredEvent를 32바이트로 압축. TSDB 엔진들은 이런 수동 최적화를 허용하지 않음.
- **초경량**: 의존성이 순수 Rust, 별도 런타임/서버 불필요. idle 5MB 달성의 핵심.
- **키 설계 자유도**: BE u64 timestamp prefix로 시간순 정렬을 직접 제어. 범위 스캔이 LSM iterator로 깔끔하게 동작.
- **스키마 버전 관리**: 메타 keyspace에 버전 저장 → 불일치 시 전체 리셋. 이 단순한 마이그레이션 전략은 범용 KV에서만 가능.
- **빌드 단순성**: `fjall = "3"` 한 줄. C 라이브러리 링킹 없음.

### 단점

- **압축 없음**: fjall 기본값 사용 중이라 Gorilla나 zstd 같은 시계열 특화 압축 없음. 하지만 toki 데이터는 소스 JSONL 대비 3% 수준이라 이미 충분.
- **수동 구현 부담**: 롤업, 딕셔너리, 인덱스, 리텐션 전부 직접 구현. 하지만 이건 toki 특화 최적화의 원천이기도 함.
- **시계열 전용 기능 없음**: 다운샘플링, 자동 파티셔닝, PromQL 같은 것은 없음. toki는 필요하지도 않음.

---

## 2. tsink — embedded time-series DB (Gorilla 압축)

> Rust 순수 구현, Gorilla XOR + delta/bitpack + zstd 적응형 압축, 64 shard 무잠금 쓰기, 3단계 tiered storage.

### toki에 쓸 경우의 이점

| 항목 | 평가 |
|------|------|
| Gorilla 압축 | 16B → ~0.4B (40x). **하지만** toki의 이벤트는 이미 32B이고, 딕셔너리 인코딩으로 소스 대비 97% 압축 달성. 추가 이득 미미. |
| 10M ops/sec 쓰기 | toki는 watch mode에서 초당 수십 이벤트. 오버스펙. |
| 64 shard 동시성 | toki는 단일 writer thread. 필요 없음. |
| Tiered storage | Hot/Warm/Cold 자동 관리. toki 데이터는 최대 2GB로, 전체가 hot에 머무름. |
| PromQL 내장 | toki는 이미 자체 PromQL-inspired 쿼리 파서 보유. 중복. |
| InfluxDB/Prometheus 프로토콜 | toki는 외부 프로토콜 노출 안 함. 불필요. |

### toki에 쓸 경우의 손실

| 항목 | 문제 |
|------|------|
| **데이터 모델 불일치** | tsink는 `(timestamp, f64)` 메트릭 모델. toki의 StoredEvent는 4개 dict ID + 4개 u64 토큰 카운터. tsink 모델에 맞추려면 이벤트 하나를 4~8개 독립 시리즈로 쪼개야 함. |
| **딕셔너리 인코딩 불가** | tsink는 float 시리즈 전용. string→u32 딕셔너리를 얹으려면 별도 사이드 스토어 필요 → 복잡성 증가, 원자적 배치 불가. |
| **보조 keyspace 없음** | 체크포인트, 메타, 세션/프로젝트 인덱스를 위한 범용 KV 기능 없음. 별도 DB (redb, sled 등) 필요. |
| **메모리 오버헤드** | 64 shard + tiered buffer + WAL. idle 5MB 불가능. tsink는 고처리량 모니터링 시스템 대상 설계라 baseline memory가 높음. |
| **의존성 무게** | Gorilla 코덱, zstd, mmap 세그먼트 관리 등 toki가 쓰지 않을 기능의 컴파일/런타임 비용. |
| **롤업 직접 불가** | tsink의 쿼리 레이어에서 시간 집계는 가능하지만, toki의 rollup-on-write (쓰기 시점에 시간별 집계 선계산) 패턴은 불가. 리포트 때마다 raw 데이터 스캔해야 해서 7ms 응답 불가능. |

### 판정: ❌ 부적합

tsink는 **고처리량 메트릭 수집 시스템** (Prometheus/Datadog급)용. toki는 저처리량 + 복합 데이터 모델 + 초경량이 목표. 데이터 모델 불일치가 치명적이고, toki의 핵심 최적화(딕셔너리 인코딩, rollup-on-write, 보조 인덱스)를 전부 포기해야 함.

---

## 3. ugnos — concurrent time-series DB core (columnar)

> Rust 구현, columnar in-memory storage, WAL + snapshot 영속화, tag 필터링, LZ4/zstd 압축.

### toki에 쓸 경우의 이점

| 항목 | 평가 |
|------|------|
| Tag 기반 필터링 | `model`, `session`, `project` 등을 tag로 모델링 가능. 현재 dict + index 조합을 대체 가능... 이론적으로. |
| Columnar storage | 같은 필드끼리 묶어서 압축 효율 증가. 토큰 카운터 같은 u64 배열은 delta 인코딩에 유리. |
| WAL + Snapshot | 크래시 세이프 영속화 기본 제공. |
| PromQL 내장 | Grafana 호환 쿼리 가능. |

### toki에 쓸 경우의 손실

| 항목 | 문제 |
|------|------|
| **In-memory 우선 설계** | ugnos는 columnar 데이터를 메모리에 유지하고 snapshot으로 영속화. toki의 2GB 데이터를 전부 메모리에 올리면 idle 5MB는 불가능. |
| **롤업 선계산 불가** | ugnos는 쿼리 시점에 집계. rollup-on-write 패턴 불가 → 리포트 응답 시간 7ms 달성 불가. |
| **체크포인트/메타 저장소 없음** | 범용 KV 기능 없음. 보조 저장소 필요. |
| **성숙도 우려** | 벤치마크 공개 데이터 부족. crates.io에서 비교적 신생 프로젝트. |
| **데이터 모델 변환 비용** | toki의 StoredEvent 구조를 ugnos의 columnar 스키마로 매핑하는 어댑터 레이어 필요. 딕셔너리 인코딩은 ugnos 내부에서 처리 가능하나, 현재처럼 수동 제어는 불가. |
| **메모리 사용량** | Columnar buffer + WAL + Snapshot 관리 → baseline이 높음. |

### 판정: ❌ 부적합

ugnos는 **대규모 메트릭 분석 시스템**의 코어 엔진으로 설계됨. in-memory columnar 모델은 toki의 "5MB idle, 디스크 우선" 철학과 정면 충돌. 그리고 toki가 가장 자랑하는 7ms 리포트 응답은 rollup-on-write에 의존하는데, 이를 구현할 방법이 없음.

---

## 4. InfluxDB — 외부 time-series DB 서버

> Go/Rust 구현 (v3는 Rust + Apache Arrow + DataFusion + Parquet). 독립 서버 프로세스.

### toki에 쓸 경우의 이점

| 항목 | 평가 |
|------|------|
| 강력한 쿼리 엔진 | SQL/Flux/InfluxQL. 복잡한 시계열 분석 가능. |
| 자동 다운샘플링 | Continuous query로 롤업 자동화 가능. |
| Parquet 기반 압축 | Columnar + 고압축. 저장 효율 높음. |
| 생태계 | Grafana 연동, Telegraf 수집 등. |

### toki에 쓸 경우의 손실

| 항목 | 문제 |
|------|------|
| **별도 서버 프로세스** | toki의 핵심 가치 = "invisible, zero-config". InfluxDB 서버를 별도로 띄워야 함. `toki daemon start` 하나로 끝나는 UX 파괴. |
| **메모리** | InfluxDB 최소 권장 256MB+. toki의 idle 5MB와 비교 불가. |
| **Cold start** | InfluxDB 서버 기동만 수 초. toki의 1.5초 cold start (2GB 파싱 포함)보다 느림. |
| **IPC 오버헤드** | embedded DB → 함수 호출. InfluxDB → HTTP/gRPC. 7ms 리포트 응답 불가. |
| **설치 부담** | 사용자가 InfluxDB를 별도 설치해야 함. `cargo install toki`로 끝나는 UX 파괴. |
| **딕셔너리 인코딩** | 불필요해지지만, InfluxDB의 내부 인덱싱이 대체. 다만 32B event → InfluxDB point 변환 시 오버헤드 발생. |
| **배포 복잡성** | 단일 바이너리 → 바이너리 + DB 서버. macOS/Linux 크로스 빌드 복잡성 증가. |
| **라이선스** | InfluxDB v3는 MIT/Apache 2.0이지만 클러스터 기능은 상용. |

### 판정: ❌ 부적합

InfluxDB는 **인프라 모니터링 플랫폼** 수준의 시스템. toki는 개인 개발자의 로컬 CLI 도구. 아키텍처 철학이 정반대. "invisible tracker"가 300MB짜리 외부 서버를 요구하는 순간 제품 가치가 소멸.

---

## 종합 비교표

| 기준 | fjall (현재) | tsink | ugnos | InfluxDB |
|------|:-----------:|:-----:|:-----:|:--------:|
| 데이터 모델 호환 | ✅ 완벽 | ❌ float 시리즈만 | ⚠️ 변환 필요 | ⚠️ 변환 필요 |
| Idle 메모리 5MB | ✅ 달성 | ❌ shard+buffer | ❌ in-memory | ❌ 256MB+ |
| 7ms 리포트 | ✅ rollup scan | ❌ raw 스캔 필수 | ❌ raw 스캔 필수 | ❌ IPC 오버헤드 |
| 딕셔너리 인코딩 | ✅ 직접 제어 | ❌ 불가 | ⚠️ 내부 처리 | ⚠️ 내부 처리 |
| 배치 트랜잭션 | ✅ 원자적 | ⚠️ 시리즈별 | ⚠️ WAL 기반 | ⚠️ 서버 측 |
| 보조 저장소 | ✅ keyspace | ❌ 별도 DB 필요 | ❌ 별도 DB 필요 | ⚠️ 가능 |
| 빌드/배포 | ✅ 순수 Rust | ✅ 순수 Rust | ✅ 순수 Rust | ❌ 외부 서버 |
| 시계열 압축 | ⚠️ 없음 | ✅ Gorilla 40x | ✅ LZ4/zstd | ✅ Parquet |
| Cold start 속도 | ✅ 1.5초 | ⚠️ 변환 오버헤드 | ⚠️ 메모리 로드 | ❌ 서버 기동 |
| 코드 복잡도 | ⚠️ 수동 구현 | ✅ 내장 기능 | ✅ 내장 기능 | ✅ 내장 기능 |

---

## 결론

**fjall이 toki에 최적의 선택이다.**

### 핵심 이유

1. **toki는 TSDB가 아니라 "시계열 데이터를 저장하는 KV store"가 필요하다.**
   이벤트 + 롤업 + 딕셔너리 + 인덱스 + 체크포인트 + 메타를 하나의 원자적 트랜잭션으로 관리하는 복합 데이터 모델이 핵심. TSDB 엔진들은 `(timestamp, float)` 메트릭 모델에 최적화되어 있어 이 복합성을 수용할 수 없다.

2. **rollup-on-write가 7ms 응답의 비밀이다.**
   쓰기 시점에 시간별 집계를 선계산하고, 리포트 시 롤업만 스캔. 이건 범용 KV의 read-modify-write가 가능해야만 구현 가능하다. TSDB 엔진들은 append-only 쓰기 모델이라 기존 롤업을 갱신할 수 없다.

3. **5MB idle는 embedded KV에서만 가능하다.**
   tsink의 64 shard, ugnos의 columnar buffer, InfluxDB의 서버 프로세스 — 전부 baseline 메모리가 toki의 전체 메모리 예산보다 크다.

4. **딕셔너리 인코딩은 수동 제어가 최적이다.**
   toki는 ~수천 개의 고유 문자열(모델명, 세션ID, 파일경로, 프로젝트명)을 u32로 압축한다. 이 압축은 쓰기 시점에 한 번만 발생하고, 읽기 시점에 역매핑으로 복원한다. TSDB의 내부 인덱싱은 이 수준의 제어를 제공하지 않는다.

5. **단일 바이너리 배포가 제품 가치다.**
   `cargo install toki` 한 줄로 끝나야 한다. 외부 서버나 무거운 런타임 의존성은 "invisible tracker" 철학과 양립 불가.

### 유일하게 tsink/ugnos가 이길 수 있는 시나리오

만약 toki가 다음 방향으로 진화한다면 TSDB 엔진이 유리해질 수 있다:

- 데이터 규모가 100GB+로 증가 (현재 최대 2GB)
- 복잡한 시계열 분석 쿼리 필요 (rate, histogram, percentile 등)
- 다중 사용자/팀 대시보드 (Grafana 연동)
- 실시간 알림/이상 탐지

하지만 이건 toki의 설계 철학 자체가 바뀌는 경우이므로, 현재 제품 방향에서는 해당 없음.
