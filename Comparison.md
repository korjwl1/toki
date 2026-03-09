  # ddleague-clitrace vs ccusage 성능 비교

  ## 기본 프로필

  | 항목 | ddleague-clitrace | ccusage |
  |------|-------------------|---------|
  | 언어 | Rust (Edition 2021) | TypeScript (Node.js/Bun) |
  | 실행 모델 | Watch mode (상주 프로세스) | Batch CLI (실행→집계→종료) |
  | DB/상태 저장 | redb (embedded ACID DB) | 없음 (stateless) |
  | 증분 처리 | 체크포인트 기반 resume | 없음 (매번 전체 재스캔) |
  | 중복 제거 | xxHash3-64 line hash | messageId:requestId Set |

  ---

  ## 시나리오 1: 최초 실행 (Cold Start)

  > 가정: 500개 세션 파일, 총 ~50,000 라인, 파일당 평균 100라인 (~5MB 총합)

  | 단계 | clitrace | ccusage |
  |------|----------|---------|
  | 파일 탐색 | glob O(F) ~50ms | glob O(F) ~50ms |
  | 파싱 | 병렬 scoped threads (CPU수 제한) | 순차 stream readline |
  | JSON 파싱 | serde_json (~2-5µs/line) | JSON.parse + Valibot validation (~10-30µs/line) |
  | 집계 | HashMap accumulate O(1)/event | groupBy → aggregateByModel O(N) |
  | 체크포인트 저장 | redb batch write ~200ms | 없음 |
  | **예상 총 소요** | **~2-6초** | **~5-15초** |

  ### 왜 차이가 나는가

  1. **언어 성능 차이 (3-5x):** Rust의 serde_json은 Node.js JSON.parse 대비 2-3x 빠르고, Valibot 스키마
  검증 오버헤드까지 합치면 라인당 처리가 5-10x 차이
  2. **병렬 처리:** clitrace는 CPU 코어 수만큼 scoped thread로 파일을 병렬 처리. ccusage는 순차 stream
  3. **메모리:** clitrace는 이벤트를 즉시 accumulate (O(M) 모델 수만큼). ccusage는 모든 entry를 배열에
  수집 후 groupBy → O(N) 메모리

  > **결론:** 최초 실행은 clitrace가 약 3-5x 빠름. 다만 50,000라인 수준에서는 둘 다 수십 초 이내로
  실용적 차이는 크지 않음.

  ---

  ## 시나리오 2: 토큰 사용량 변화량(Delta) 수집 → 서버 전송

  > 이게 핵심 차이입니다.

  ### clitrace의 접근

  파일 변경 감지 (FSEvents)
    → stat() 크기 비교 (1-5µs)
    → 체크포인트에서 reverse-scan resume (~400µs)
    → 새 라인만 read (~200µs)
    → 파싱 (~10-50µs for 10 lines)
    → Delta 즉시 산출

  10개 새 라인 처리: ~500µs - 2ms

  ### ccusage의 접근

  CLI 재실행
    → 전체 파일 glob (~50ms)
    → 전체 파일 재스캔 (~5-15초, 50K lines)
    → 전체 재집계
    → 이전 결과와 diff 해야 delta 산출 (구현 안 됨)

  매번 전체 재계산: ~5-15초

  ### 변화량 감지 성능 비교

  | 측면 | clitrace | ccusage |
  |------|----------|---------|
  | Time Complexity | O(L) 새 라인 수만큼 | O(N) 전체 데이터 재처리 |
  | I/O | seek + 새 부분만 read | 모든 파일 전체 read |
  | 지연 시간 | ~1-2ms (실시간) | ~5-15초 (배치) |
  | CPU 사용량 | 거의 0 (idle시) | 매 실행마다 풀 스캔 |
  | 서버 전송 적합성 | 이벤트 드리븐, 즉시 push 가능 | polling 기반, cron으로 주기 실행 |

  ---

  ## 실질적 시나리오별 성능 예측

  ### 데이터 규모별 (1회 실행 기준)

  | 규모 | 라인 수 | clitrace 초기 | ccusage 초기 | clitrace 증분 | ccusage 증분 |
  |------|---------|---------------|--------------|---------------|--------------|
  | 소규모 | 1K | ~100ms | ~300ms | ~1ms | ~300ms |
  | 중규모 | 50K | ~3s | ~10s | ~1ms | ~10s |
  | 대규모 | 500K | ~20s | ~2min+ | ~1ms | ~2min+ |
  | 초대규모 | 5M | ~3min | OOM 위험 | ~1ms | OOM |

  ### 서버 수집 시나리오 (100명 사용자, 5분 간격 delta 수집)

  | 항목 | clitrace 기반 | ccusage 기반 |
  |------|---------------|--------------|
  | 수집 방식 | WebSocket/gRPC push | cron + REST poll |
  | 수집 주기 | 실시간 (이벤트 발생 즉시) | 5분 polling |
  | 클라이언트 CPU | ~0% (idle) | 5분마다 풀스캔 스파이크 |
  | 네트워크 payload | delta만 (~100B/event) | 전체 or diff 계산 필요 |
  | 서버 부하 | 이벤트 단위 ingestion | batch 집계 결과 수신 |
  | 데이터 신선도 | ms 단위 | 분 단위 |
  | 누락 위험 | 낮음 (체크포인트 ACID) | 높음 (crash시 전체 재계산) |

  ---

  ## 핵심 아키텍처 차이 요약

  clitrace:  O(N) 초기 + O(ΔL) 이후   ← 누적 비용 거의 없음
  ccusage:   O(N) × 실행 횟수          ← 데이터 커질수록 선형 증가

  | 강점 | clitrace | ccusage |
  |------|----------|---------|
  | 증분 처리 | 체크포인트 + reverse-scan resume | 없음 |
  | 파일 compaction 대응 | hash 기반으로 자동 복구 | 상관없음 (매번 전체 읽기) |
  | 메모리 효율 | O(F) 체크포인트만 유지 | O(N) 전체 entry 메모리 적재 |
  | 바이너리 크기 | ~2-5MB (네이티브) | ~수백 KB + Node.js 런타임 |
  | 배포 용이성 | 단일 바이너리 | npm/bun install |
  | 기능 풍부성 | 실시간 모니터링 특화 | daily/weekly/monthly/session/blocks 다양한 뷰 |
  | 가격 계산 | 없음 | LiteLLM 연동 (online/offline) |
  | 확장성 | 다른 provider 인터페이스 (Gemini, Codex) | 별도 앱으로 분리 (codex, opencode 등) |

  ---

  ## 결론

  | 용도 | 추천 |
  |------|------|
  | 서버에 delta 수집 (실시간/준실시간) | **clitrace 압도적 우위** — O(ΔL) 증분처리, 이벤트 드리븐, ms
  지연 |
  | 개인 사용, 일일/월간 리포트 | **ccusage** — 다양한 집계 뷰, 가격 계산, 설치 간편 |
  | 대규모 (500K+ 라인) | **clitrace** — ccusage는 OOM 위험, clitrace는 증분이라 무관 |
  | 초기 셋업 편의성 | **ccusage** — npm install 한 줄, 상태 관리 불필요 |

  > 서버에 토큰 사용량 변화량을 모아볼 목적이라면, clitrace의 체크포인트 기반 증분처리 + watch 모드가
  설계 의도 자체가 그 용도에 맞춰져 있어서 성능 차이가 수천 배 수준(ms vs 초)입니다.
