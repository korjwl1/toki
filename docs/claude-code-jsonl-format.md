# Claude Code JSONL Format Reference

Claude Code CLI는 세션 로그를 `~/.claude/projects/<encoded-path>/` 하위에 JSONL 파일로 기록한다.

## 파일 구조

```
~/.claude/projects/-Users-user-Documents-project/
├── 4de9291e-061e-414a-85cb-de615826aded.jsonl          # 부모 세션
├── 4de9291e-061e-414a-85cb-de615826aded/
│   └── subagents/
│       └── agent-aed1da92cc2e4e9e7.jsonl               # 서브에이전트
└── db7cd31e-fdb1-4767-a6a2-f2f3dc68a74b.jsonl          # 다른 세션
```

- 부모 세션: UUID 형식 (`8-4-4-4-12` hex) 파일명
- 서브에이전트: `<UUID>/subagents/agent-*.jsonl`
- 서브에이전트 토큰은 부모에 포함되지 않으며 별도 파일에 기록됨

## 줄 타입 (type 필드)

JSONL의 각 줄은 `"type"` 필드로 구분된다. 총 7종 확인됨:

| type | 용도 | 토큰 정보 | 크기 특성 |
|------|------|----------|----------|
| `assistant` | AI 응답 (텍스트, tool use 포함) | **있음** (`message.usage`) | 평균 ~1.5KB |
| `user` | 사용자 입력 | 없음 | 평균 ~8.3KB (파일 내용 포함 시 큼) |
| `progress` | 스트리밍 진행 상태 | 없음 (내부에 assistant 중첩됨) | 평균 ~1.1KB |
| `file-history-snapshot` | 파일 스냅샷 | 없음 | 평균 ~0.6KB |
| `system` | 시스템 이벤트 (hook, stop 등) | 없음 | 평균 ~0.6KB |
| `queue-operation` | 큐 작업 | 없음 | 평균 ~0.2KB |
| `pr-link` | PR 링크 | 없음 | ~0.2KB |

**토큰 추적에 필요한 타입은 `assistant`만 해당.**

## assistant 줄 상세 구조

```json
{
  "parentUuid": "...",
  "isSidechain": false,
  "userType": "external",
  "cwd": "/path/to/project",
  "sessionId": "uuid",
  "version": "2.1.63",
  "gitBranch": "main",
  "message": {
    "model": "claude-opus-4-6",
    "id": "msg_01...",
    "type": "message",
    "role": "assistant",
    "content": [ ... ],
    "stop_reason": "end_turn",
    "stop_sequence": null,
    "usage": {
      "input_tokens": 3,
      "cache_creation_input_tokens": 5139,
      "cache_read_input_tokens": 9631,
      "output_tokens": 14,
      "server_tool_use": { ... },
      "service_tier": "...",
      "cache_creation": { ... },
      "inference_geo": "...",
      "iterations": 0,
      "speed": 0.0
    }
  },
  "requestId": "...",
  "type": "assistant",
  "uuid": "...",
  "timestamp": "2026-03-08T12:00:00Z"
}
```

### clitrace가 추출하는 필드

| 필드 경로 | 용도 |
|-----------|------|
| `type` | `"assistant"` 여부 판별 |
| `message.model` | 모델명 (집계 키) |
| `message.id` | 이벤트 식별자 |
| `message.usage.input_tokens` | 캐시 미적용 입력 토큰 |
| `message.usage.cache_creation_input_tokens` | 캐시 생성 입력 토큰 |
| `message.usage.cache_read_input_tokens` | 캐시 읽기 입력 토큰 |
| `message.usage.output_tokens` | 출력 토큰 |
| `timestamp` | 이벤트 시점 |

### clitrace가 무시하는 필드

| 필드 경로 | 무시 이유 |
|-----------|----------|
| `message.content[]` | 텍스트/thinking/tool_use 내용 — 토큰 추적에 불필요, 줄의 대부분을 차지 |
| `message.usage.server_tool_use` | 서버 측 도구 사용 메타데이터 |
| `message.usage.service_tier` | 서비스 티어 |
| `message.usage.cache_creation` | 캐시 생성 상세 |
| `message.usage.inference_geo` | 추론 지역 |
| `message.usage.iterations` | 반복 횟수 |
| `message.usage.speed` | 속도 메트릭 |
| `parentUuid`, `sessionId`, `cwd`, ... | 세션 메타데이터 — 현재 미사용 |

### content 블록 타입

`message.content[]` 배열 내 블록은 3종:

| content[].type | 설명 |
|----------------|------|
| `text` | 텍스트 응답 |
| `thinking` | 사고 과정 (extended thinking) |
| `tool_use` | 도구 호출 (파일 읽기, bash, 검색 등) |

## 파싱 최적화

### 프리필터

줄에 `"assistant"` 문자열이 포함되지 않으면 JSON 파싱 없이 즉시 스킵한다.

- `user`, `file-history-snapshot`, `system`, `queue-operation`, `pr-link` → 100% 스킵
- `progress` → `data.message` 안에 `"assistant"`가 중첩되어 있어 프리필터 통과 (false positive), serde 단계에서 `type != "assistant"`로 탈락

실측 기준 (5,162줄, 13.2MB):
- **67% 데이터량을 JSON 파싱 없이 스킵**
- false negative 0 (누락 없음)

### 타겟 struct 역직렬화

`serde_json::Value` 대신 필요한 필드만 정의한 struct로 역직렬화한다.
`content` 배열 등 불필요한 필드는 serde가 스캔은 하지만 힙에 할당하지 않는다.
문자열은 `&str` 차용으로 복사를 최소화한다.

## 주의사항

- JSON 키 순서는 보장되지 않음 — 필드 위치에 의존하는 최적화는 위험
- `"type"` 필드가 줄 시작이 아닌 중간(~280-388바이트)에 위치함
- 서버가 minified JSON을 출력하지만 향후 변경 가능 — 공백 유무에 의존하지 않음
- `assistant` 타입은 현재까지 100% `message.usage`를 동반 (없는 케이스 0건)
