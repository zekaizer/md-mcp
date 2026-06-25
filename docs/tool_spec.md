# md-mcp 도구 명세

MCP로 md note 파일을 관리하는 tool 명세를 제안한다. 먼저 범위에 대한 가정을 명시한다.

**가정**
- 단일 vault(root 디렉토리) 하위의 `.md` 파일 관리
- 순수 markdown + YAML frontmatter 기준. Obsidian 전용 문법(wikilink, `^block-id`, backlink)은 **범위 밖**
- 모든 path는 vault-relative (절대경로·`..` 차단 전제)
- **path 접미사 규약**: 노트는 `.md`, 디렉토리는 `/`로 끝난다 — 접미사만으로 노트/디렉토리를 구문 판별(전 도구 공통)

**용어 정의** (markdown 표준엔 'section' 개념이 없다 — CommonMark는 heading 블록만 규정하므로 이 spec이 정의한다)
- **heading(제목)**: ATX(`#`~`######`) 제목 라인만 인정 — Setext(`===`/`---`)는 **범위 밖**(`---`의 frontmatter/thematic-break 모호성·h1~h2 깊이 제약 회피). level은 `#` 개수. fenced(```·~~~)·indented 코드블록 내부의 `#`는 heading이 **아니다**(코드펜스가 우선) — 섹션 경계 오인 방지.
- **section(섹션)**: heading 라인 + 다음 **동급 또는 상위** heading(level 숫자가 같거나 작은 것) 직전까지의 범위 = 그 heading의 lead 본문 + 모든 하위 subsection. markdown 비표준, 이 spec의 정의.
- **lead 본문**: heading 라인 직후 ~ **첫 하위(더 깊은) heading 직전**. subsection 제외.
- **subsection(하위 섹션)**: 대상보다 깊은 level로 그 아래 중첩된 section.
- **heading_path**: 최상위 조상 heading부터 대상까지의 텍스트 배열(중첩 경로). 빈 배열 `[]` = **노트 전체 body**(frontmatter 제외)를 한 섹션으로 취급(root). root에선 `scope=section`=전체 body, `scope=body`=첫 heading 직전 preamble.
- **scope**: `body`=lead 본문만, `section`=lead 본문 + 하위 subsection 전체. read_sections·edit_sections가 공유하는 범위 선택자.
- **frontmatter**: 파일 선두 YAML `---`…`---` 블록. section이 아니며 `edit_properties`로만 편집. 닫는 `---`이 없는 선두 `---`는 frontmatter가 아니라 본문으로 본다.
- **content_hash(섹션 해시)**: 한 섹션의 content를 주어진 `scope`로 해석한 바이트(대상 heading 라인 제외)의 해시. 노트 단위가 아닌 섹션 단위(§4).
- **vault**: 관리 대상 root 디렉토리. 모든 path는 vault-relative(접미사 규약은 가정 참조).

**설계 우선순위 (이 spec의 축)**
1. **콜 수 최소화**: MCP를 쓰는 에이전트가 적은 tool call로 작업을 끝내야 한다. → 가능한 모든 도구를 batch화.
2. **섹션 단위 편집 통일**: 부분 편집은 heading path로 지정하는 섹션 편집 하나로 모은다. 통째 교체·text-match·block 편집을 따로 두지 않는다.
3. **왕복(round-trip) 제거**: read→write, write→재read 왕복을 줄이도록 outline/section 읽기와 충분한 write 출력을 제공한다.

---

## 1. 도구 세트 개요

| Tool | 역할 | 배치 | 실패 의미론 |
|---|---|---|---|
| `read_notes` | 1개 이상 노트 전체 읽기 | ✓ | 부분 (per-item `exists`) |
| `read_outlines` | 노트들의 heading TOC | ✓ | 부분 |
| `read_sections` | heading path로 특정 섹션만 읽기 | ✓ | 부분 |
| `list_notes` | 디렉토리/glob 기준 목록 | — | 단일 |
| `search_notes` | 내용·파일명·frontmatter 검색 | — | 단일 |
| `create_notes` | 신규 노트 생성 (덮어쓰기 거부) | ✓ | **부분 성공** |
| `append_notes` | 노트 말미 append | ✓ | **부분 성공** |
| `edit_sections` | heading path 기준 섹션 편집 (replace/append/delete/insert/rename/move) | ✓ | **all-or-nothing** |
| `edit_properties` | frontmatter key:value 단위 set/remove | ✓ | **all-or-nothing** |
| `rename_notes` | 제자리 이름 변경 (1:1) | ✓ | **all-or-nothing** |
| `relocate_notes` | 디렉토리로 이동 (N:1-dir) | ✓ | **all-or-nothing** |
| `delete_notes` | 노트 삭제 | ✓ | **all-or-nothing** |

**실패 의미론 원칙**: 비파괴 도구(create/append)는 부분 성공 — 한 item의 실패가 형제 item을 가라앉히지 않는다. 파괴적 도구(edit/properties/rename/relocate/delete)는 all-or-nothing — batch 내 한 item이라도 거부되면 전체를 쓰지 않는다. 데이터 손실 위험이 있는 작업에서 "절반만 적용된 모호한 상태"를 만들지 않기 위함이다. **다중 파일에 걸친 batch도 서버가 원자성을 보장한다** — 원본 백업/저널 후 일괄 적용, 어느 파일이라도 write 실패하면 전부 롤백해 무적용으로 되돌린다(§4).

**출력 envelope**: 거부 시 `{ ok:false, errors:[{ index, item, operation, code, message }] }` — **어느 item(`index`·`item`)이, 어떤 명령(`operation`)에서, 왜(`code`·`message`) 막혔는지** 검출된 위반을 **전부** 보고하고 적용 결과는 비운다(아무것도 안 쓰임). 일부만 보고하면 에이전트가 고쳐 재시도→또 실패하는 왕복이 생기므로 전수 보고. `code`는 기계 판독용 사유(`NOT_FOUND`/`CONFLICT`/`HASH_MISMATCH`/`AMBIGUOUS`/`OVERLAP`/`HEADING_LEVEL`/`SUFFIX`/`DEST_NOT_DIR`/`BATCH_COLLISION`/`MISSING_CONTENT`/`TRAVERSAL`/`FRONTMATTER_PARSE` 등). `required`·`enum`·타입 같은 schema 위반은 MCP 프레임워크가 envelope 이전 단계에서 거른다(서버 로직 사유와 분리). 단 `maxItems`(배치 상한)는 inputSchema에 노출되더라도 프레임워크가 검증하지 않으므로, 서버가 `batch_limit`으로 같은 단계에서 `invalid_params`로 거른다. 성공 시 각 도구가 per-item 적용 결과를 반환(도구별 출력 참조). 비파괴 도구의 부분 성공도 실패 item은 동일한 `{ index, item, code, message }` 형태로 사유를 단다(성공 item과 나란히). 검증은 통과했으나 write 도중 I/O 실패하면 서버가 롤백하므로 결과는 무적용 — 다른 거부와 똑같이 `errors`로 보고(부분 적용 상태는 남지 않는다).

**표기 규약**: 아래 스키마는 지면을 위해 root `"type": "object"`를 생략한다(모든 inputSchema는 object). 객체 배열 파라미터는 도구 의미명(`notes`/`appends`/`edits`/`renames`/`moves`/`targets`), 문자열 path 배열은 `paths`로 통일. 모든 batch 배열(읽기·쓰기)은 `maxItems: 100`(서버 강제) — 과대 batch의 토큰·메모리 통제.

---

## 2. 읽기·검색 도구

### read_notes
```json
{
  "properties": {
    "paths": { "type": "array", "items": { "type": "string" },
               "description": "Vault-relative paths" },
    "include_body": { "type": "boolean", "default": true,
      "description": "본문(frontmatter 블록 제외) 포함 여부" },
    "include_frontmatter": { "type": "boolean", "default": true,
      "description": "파싱된 frontmatter 객체 포함 여부" }
  },
  "required": ["paths"]
}
```
출력: 각 path별 `{ path, content?, frontmatter?, exists }`. read_notes는 **소비(열람)용**으로 편집 워크플로에 참여하지 않으므로 `content_hash`를 주지 않는다(편집용 hash는 read_sections에서 취득).
- `content`는 **body만**(선행 `---` frontmatter 블록 제외), `include_body:true`일 때만 포함.
- `frontmatter`는 **파싱된 객체**(원문 YAML 아님), `include_frontmatter:true`일 때만 포함.
- 둘 다 false면 `exists`만 — 존재 확인용 경량 호출.
- 존재하지 않는 path는 전체를 실패시키지 말고 `exists:false`로 개별 보고.

### read_outlines
```json
{
  "properties": {
    "paths": { "type": "array", "items": { "type": "string" } }
  },
  "required": ["paths"]
}
```
출력: 각 노트별 `{ path, exists, headings }`. `headings`는 `[{ heading_path, level, line, occurrence, ambiguous }]` 평면 리스트(문서 순서)이며, 트리는 `heading_path` 길이와 `level`로 복원한다. 존재하지 않는 path는 `exists:false`로 개별 보고. **순수 구조 스캔** — 편집 전제가 아니므로 `content_hash`는 주지 않는다. 큰 노트를 전체 read하지 않고 구조만 파악 → 편집할 섹션의 `heading_path`·`occurrence` 확정용. 콜·토큰 절감의 핵심. `ambiguous:true`(동일 heading_path 2개 이상)인 heading만 `occurrence`(문서 순서 1-based)를 신경 쓰면 된다 — 나머지는 생략 가능. 같은 정규화 기준을 `read_sections`·`edit_sections`와 공유한다(§4).

### read_sections
```json
{
  "properties": {
    "targets": {
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "path": { "type": "string" },
          "heading_path": { "type": "array", "items": { "type": "string" },
            "description": "e.g. [\"Design\",\"Schema\"]. 빈 배열이면 root(노트 전체 body)" },
          "occurrence": { "type": "integer",
            "description": "heading_path 다수 매칭 시 1-based 선택. 생략 시 다수 매칭이면 error" },
          "scope": { "type": "string", "enum": ["body","section"], "default": "section",
            "description": "body=lead 본문만, section=lead 본문+하위 subsection 전체. edit_sections와 동일 의미 — 편집할 scope로 읽어야 content_hash가 호환" }
        },
        "required": ["path","heading_path"]
      }
    }
  },
  "required": ["targets"]
}
```
출력: 각 target별 `{ path, heading_path, occurrence, scope, content?, content_hash?, note_exists, found }`. `note_exists`(노트 자체 존재)와 `found`(heading_path 매칭 성공)를 **분리** — 뭉개면 에이전트가 "노트 없음"과 "섹션 없음"을 구분 못 한다. `content`·`content_hash`는 요청한 `scope` 범위로 산출(읽은 **그 섹션의 해시**)되며 `found:true`일 때만 포함. 입력 `occurrence`·`scope`를 그대로 echo — 같은 heading_path를 여러 occurrence로 읽어도 응답 item이 구분되고, 같은 occurrence·scope로 `edit_sections`에 넘겨야 대상 섹션과 `expected_hash`가 맞는다. 편집 직전 "그 섹션만" 읽어 토큰을 아낀다.

### list_notes
```json
{
  "properties": {
    "directory": { "type": "string", "default": "",
      "description": "탐색 시작 디렉토리(/로 끝남, e.g. daily/). \"\"=root" },
    "recursive": { "type": "boolean", "default": true },
    "glob": { "type": "string",
      "description": "노트 경로 필터(확장자 명시). e.g. daily/**/*.md, projects/2024-*.md" },
    "include_dirs": { "type": "boolean", "default": false,
      "description": "디렉토리도 결과에 포함. 디렉토리 항목은 path가 `/`로 끝남" },
    "limit": { "type": "integer", "default": 200, "maximum": 1000 },
    "cursor": { "type": "string",
      "description": "이전 응답의 next_cursor. 다음 페이지 요청 시 전달" }
  }
}
```
출력: `{ items: [{ path, size_bytes, modified_time }], next_cursor }`. `modified_time`은 ISO 8601(UTC). 디렉토리 항목(`include_dirs:true`)은 `path`가 `/`로 끝나고 `size_bytes`는 null — 이 종결 `/`가 곧 `relocate_notes.dest_dir`에 그대로 쓰는 형태다. 결과가 `limit`를 넘으면 `next_cursor`(opaque 문자열)를 반환, 더 없으면 생략·null. 토큰 폭주 방지를 위해 본문 미포함. 정렬은 안정적 기준(path 사전순) 고정 — 그래야 cursor paging이 중복·누락 없이 이어진다. dot-디렉토리·내부 상태 디렉토리(저널/백업/trash)는 기본 제외(§4 내부 상태 격리).

`directory`/`glob`/`recursive` 상호작용: `directory`는 탐색 시작점(default=root), `recursive`는 하위 디렉토리 재귀 여부, `glob`은 그 결과 경로에 대한 추가 필터(패턴은 `directory` 기준 상대). `glob`에 `**`가 있으면 재귀를 함의하므로 충돌 시 `glob`이 `recursive`보다 우선. `glob`은 노트(.md) 결과에만 적용 — 디렉토리(`include_dirs`)는 glob 필터를 거치지 않고 `directory`/`recursive` 범위로만 정해진다. 존재하지 않는 `directory`는 error가 아니라 빈 `items`.

### search_notes
```json
{
  "properties": {
    "query": { "type": "string",
      "description": "텍스트 검색어. frontmatter 필터만 쓸 거면 생략 가능" },
    "mode": { "type": "string", "enum": ["content","filename","both"], "default": "both",
      "description": "query 적용 대상. content=본문 전문, filename=path/basename 대소문자 무시 substring, both=합집합. query 없으면 무시" },
    "frontmatter": { "type": "object",
      "description": "frontmatter 필드 필터(top-level key들 AND). scalar=정확 일치, list=value 포함(contains). 값은 YAML scalar 타입으로 비교(string/number/bool, date는 ISO-8601 정규화). e.g. {\"status\":\"draft\",\"tags\":\"project\"}" },
    "frontmatter_exists": { "type": "object",
      "description": "key 존재/부재 필터(AND). {key:true}=그 key 있는 노트, {key:false}=없는 노트. e.g. {\"reviewed\":false}" },
    "limit": { "type": "integer", "default": 20, "maximum": 100 },
    "context_lines": { "type": "integer", "default": 2 },
    "cursor": { "type": "string",
      "description": "이전 응답의 next_cursor. 다음 페이지 요청 시 전달" }
  }
}
```
출력: `{ items: [{ path, snippet?, frontmatter? }], next_cursor }`. `query`·`frontmatter`·`frontmatter_exists`를 함께 주면 모두 **AND**(텍스트 매칭 ∧ 필드 일치 ∧ 존재/부재). `snippet`은 텍스트 매칭 주변 context(`query` 있을 때), `frontmatter`는 필터에 쓴 key들의 실제 값 echo(매칭 노트를 재read 없이 확인 — 왕복 절감). 결과가 `limit`를 넘으면 `next_cursor` 반환, 더 없으면 생략·null. snippet이 빈약하면 에이전트가 매칭 파일을 전부 다시 read한다 — snippet 품질이 곧 콜 수다. 정확도가 필요하면 BM25 ranking 적용(ranking을 쓰면 paging 정렬은 score 기준 고정).

---

## 3. 쓰기 도구

### create_notes (비파괴 · 부분 성공)
```json
{
  "properties": {
    "notes": {
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "path": { "type": "string" },
          "content": { "type": "string" },
          "frontmatter": { "type": "object" }
        },
        "required": ["path","content"]
      }
    },
    "overwrite": { "type": "boolean", "default": false }
  },
  "required": ["notes"]
}
```
`content`는 **body만**, frontmatter는 `frontmatter` 객체로 분리 전달한다 — `content`에 선행 `---` 블록을 넣으면 error(이중 frontmatter 방지). `overwrite:false`일 때 기존 노트 존재 시 해당 item만 error(의도치 않은 덮어쓰기 방지). 없는 parent 디렉토리는 자동 생성한다. 출력: per-item `{ path, created, error? }`.

### append_notes (비파괴 · 부분 성공)
```json
{
  "properties": {
    "appends": {
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "path": { "type": "string" },
          "content": { "type": "string" },
          "create_if_missing": { "type": "boolean", "default": false }
        },
        "required": ["path","content"]
      }
    }
  },
  "required": ["appends"]
}
```
노트 말미에 raw `content` append(EOF에 그대로 덧붙임). 데이터를 덮지 않으므로 비파괴 → 부분 성공. 섹션 말미가 아니라 노트 말미 전용(흔한 logging/journaling 경로). 섹션 내부 append는 `edit_sections`의 `append`를 쓴다. append 시 구분 개행을 자동 삽입하지 않으니(예측가능) 필요한 줄바꿈은 `content`에 호출자가 직접 포함한다(저장 시 LF 정규화와는 별개). 같은 path가 batch에 여러 번이면 배열 순서대로 누적 append. `create_if_missing` 생성 시 없는 parent 디렉토리도 자동 생성(create_notes와 동일).

### edit_sections (파괴적 · all-or-nothing)
```json
{
  "properties": {
    "edits": {
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "path": { "type": "string" },
          "heading_path": { "type": "array", "items": { "type": "string" },
            "description": "대상 섹션. 빈 배열이면 root(노트 전체 body)" },
          "occurrence": { "type": "integer",
            "description": "heading_path 다수 매칭 시 1-based 선택. 생략 시 다수 매칭이면 error" },
          "operation": { "type": "string",
            "enum": ["replace","append","delete","insert_before","insert_after","rename","move"] },
          "scope": { "type": "string", "enum": ["body","section"], "default": "section",
            "description": "replace/append/delete 적용 범위. body=lead 본문만(subsection 보존), section=lead 본문+하위 subsection 전체. 그 외 operation에서는 무시" },
          "content": { "type": "string",
            "description": "replace/append/insert_* 본문. delete/rename/move면 불필요. insert_*의 content는 새 heading 포함 가능" },
          "new_heading": { "type": "string",
            "description": "operation=rename 전용. 대상 heading의 새 텍스트(`#` 제외, level 유지)" },
          "destination": { "type": "object",
            "description": "operation=move 전용. 대상 섹션(subtree)을 옮길 위치",
            "properties": {
              "heading_path": { "type": "array", "items": { "type": "string" } },
              "occurrence": { "type": "integer" },
              "position": { "type": "string", "enum": ["before","after"] }
            } },
          "expected_hash": { "type": "string",
            "description": "Optional. 대상 섹션을 이 edit의 scope로 해석한 content_hash(섹션 해시). 같은 scope의 read_sections 값과 비교 — 불일치 시 batch 전체 거부. insert_*는 scope가 없으니 anchor 섹션을 scope=section으로 해석해 비교" }
        },
        "required": ["path","heading_path","operation"]
      }
    }
  },
  "required": ["edits"]
}
```
부분 편집의 단일 창구. 의미:
- 용어(section/lead 본문/subsection/scope)는 상단 **용어 정의** 참조.
- `replace`
  - `scope=body`: lead 본문만 `content`로 교체. **하위 subsection 보존.**
  - `scope=section`(기본): lead 본문 + 하위 subsection 전체를 `content`로 교체.
- `append`
  - `scope=body`: lead 본문 끝(첫 하위 heading 직전)에 추가.
  - `scope=section`(기본): 섹션 전체 끝(마지막 subsection 뒤, 다음 sibling heading 직전)에 추가.
- `delete`
  - `scope=body`: lead 본문만 비움. heading·subsection 보존.
  - `scope=section`(기본): heading + 본문 + subsection 전체 제거.
- `insert_before`/`insert_after`: 대상 섹션 앞/뒤에 `content` 삽입. `content`에 새 heading을 넣어 신규 섹션 생성. `scope` 무시. 빈 `heading_path`(root)면 `insert_before`=노트 맨 앞(frontmatter 직후) prepend, `insert_after`=노트 맨 끝 append(텍스트 결과는 `append_notes`와 같으나 이쪽은 파괴적 all-or-nothing). (`content`에 heading이 있으면 아래 **content heading 계층 검증** 공통 규칙 적용.)
- `rename`: 대상 heading 텍스트만 `new_heading`으로 교체(level·body·subsection 유지). `content`/`scope` 불필요 — 섹션 식별자 변경용(`edit_properties`의 key rename에 대응).
- `move`: 대상 섹션을 subtree째 `destination`(heading_path[+occurrence])의 `position`(before/after)으로 이동. `content`/`scope` 불필요. **destination이 대상 자신의 하위면 거부**(self-move), 이동 후 위치에서 heading 계층 검증 적용.
- `heading_path` 다수 매칭 시 `occurrence`(1-based)로 선택. **`occurrence` 생략 + 다수 매칭이면 error** (모호한 silent 편집 금지).
- `heading_path` **미발견 시 error**. silent no-op이 아니라 명시적 실패로 보고해야 LLM이 잘못된 성공으로 오판하지 않는다.
- 파괴적이므로 한 edit이라도 위 사유로 거부되면 batch 전체 미적용.
- **같은 노트 다중 edit**: 한 batch에 동일 노트의 edit이 여럿이면 모든 `heading_path`·`occurrence`·line을 **원본 스냅샷 기준**으로 해석한 뒤 일괄 적용한다 — 앞 edit이 뒤 edit의 인덱스를 흔들지 않는다. **단, 같은 batch에서 대상 섹션이 겹치면(동일·포함관계) 거부**(§4 대상 겹침 금지 — 겹침 편집은 content로 합쳐 1 edit).
- **content heading 계층 검증 (replace/append/insert/move 공통)**: `content`에 heading이 들어가는 operation은 그 content가 놓이는 위치에서 주변(anchor/인접) 섹션보다 얕은 level이거나 level을 건너뛰거나(h2→h4) 블록 내부 nesting이 어긋나면 거부(`HEADING_LEVEL`). `move`도 이동된 섹션 top heading level이 destination 위치에 안 맞으면 거부. create/`append_notes`는 노트 구조를 정의·확장하므로 제외(기존 구조에 끼워넣는 edit_sections operation만 대상).

출력: 편집된 **섹션별** 적용 결과 + 새 `content_hash`(편집 후 그 섹션을 같은 `scope`로 해석한 해시; read_sections와 함께 hash의 두 생산처). 이 hash를 다음 편집의 `expected_hash`로 재사용해 write 후 재read 왕복을 없앤다. `rename`/`move`는 대상의 heading_path가 바뀌므로 `new_heading_path`도 echo(이후 그 섹션 작업에 사용).

### edit_properties (파괴적 · all-or-nothing)
```json
{
  "properties": {
    "edits": {
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "path": { "type": "string" },
          "key": { "type": "string", "description": "frontmatter 최상위 key" },
          "value": {
            "description": "설정할 값. JSON 타입을 그대로 YAML로 직렬화 — 문자열 \"123\"과 숫자 123 구분 보존(리스트/맵/null 가능). 필드 자체를 생략하면 해당 key 제거" }
        },
        "required": ["path","key"]
      }
    }
  },
  "required": ["edits"]
}
```
frontmatter 편집기 — **item 하나 = (노트, key) 하나**의 atomic 단위. `value` 있으면 set(덮어쓰기), `value` **필드를 생략하면 그 key 제거**. `value:null`은 "null로 설정"이지 제거가 아니다(둘을 구분). 한 노트의 여러 key를 바꾸려면 item 여러 개를 준다. `edit_sections`는 frontmatter(YAML)를 건드리지 않는다(직렬화 일관성 보존). 안정적 직렬화기를 고정해 매 write마다 key 순서·따옴표가 바뀌어 sync diff가 오염되는 것을 막는다. set 대상 노트에 frontmatter 블록이 없으면 새로 생성한다. 파괴적이라 batch 전체 all-or-nothing. frontmatter는 section이 아니라 section-hash 대상이 아니므로 `expected_hash`(optimistic concurrency)를 두지 않는다 — frontmatter는 last-write-wins.

### rename_notes (파괴적 · all-or-nothing)
```json
{
  "properties": {
    "renames": {
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "path": { "type": "string", "description": "대상 파일(.md) 또는 디렉토리(/로 끝남), vault-relative" },
          "new_name": { "type": "string",
            "description": "새 basename(확장자 포함). 슬래시 금지 — 이동은 relocate_notes" }
        },
        "required": ["path","new_name"]
      }
    },
    "overwrite": { "type": "boolean", "default": false }
  },
  "required": ["renames"]
}
```
제자리 이름 변경. 각 item **1:1**, 같은 디렉토리 내에서 basename만 교체. `new_name`에 `/` 포함 시 error(상대 이동을 막아 blast radius 분리). `path`가 노트(.md)면 `new_name`도 `.md`로 끝나야 하고(아니면 거부), 디렉토리(`/`)면 `new_name`은 `.md`·`/` 없는 순수 이름이다. 충돌(같은 이름 존재) + overwrite:false → batch 전체 거부. 출력(성공): `{ renamed: [{ from, to }] }` — `to`=같은 디렉토리 + `new_name`(디렉토리 대상이면 `from`/`to`는 `/`로 끝남).

### relocate_notes (파괴적 · all-or-nothing)
```json
{
  "properties": {
    "moves": {
      "type": "array",
      "items": {
        "type": "object",
        "properties": {
          "source": { "type": "string", "description": "파일(.md) 또는 디렉토리(/로 끝남), vault-relative" },
          "dest_dir": { "type": "string", "description": "이동 대상 디렉토리, /로 끝남. source의 basename 유지" }
        },
        "required": ["source","dest_dir"]
      }
    },
    "overwrite": { "type": "boolean", "default": false }
  },
  "required": ["moves"]
}
```
디렉토리로 이동. `source`의 basename을 유지한 채 `dest_dir` 안으로 옮긴다.
- **N → 1-dir**: 여러 item에 같은 `dest_dir`를 주면 여러 노트를 한 폴더로 모음.
- **dir → dir**: `source`가 디렉토리면 subtree 통째 이동.
- `dest_dir`가 없으면 중간 경로 포함 자동 생성(`create_notes`의 parent 생성과 동일 관례). 단 그 위치에 **노트(비-디렉토리)가 있으면 거부**.
- `source`가 디렉토리일 때 `dest_dir`가 그 **하위면 거부**(자기 안으로 이동 불가).
- 충돌(`dest_dir`에 동일 이름 존재) + overwrite:false → batch 전체 거부(merge 아님).
- 출력(성공): `{ moved: [{ from, to }] }` — `to`=`dest_dir` + basename(디렉토리 대상이면 `/`로 끝남). 체이닝(예: 이동 후 rename)에 이 `to`를 그대로 쓴다.

이름 변경 + 이동 동시 작업은 `relocate_notes` → `rename_notes` 2콜(각각 all-or-nothing이라 한 콜로 못 묶음). markdown 상대 링크 `[..](path.md)`는 이동으로 깨질 수 있음 — 링크 자동 갱신은 §4 참조.

### delete_notes (파괴적 · all-or-nothing)
```json
{
  "properties": {
    "paths": { "type": "array", "items": { "type": "string" } }
  },
  "required": ["paths"]
}
```
파일·디렉토리 모두 대상(디렉토리는 `/`로 끝나는 path, subtree 재귀 삭제). 배치 내 path가 서로 동일·포함관계(디렉토리와 그 하위)면 거부(§4 대상 겹침 금지). 존재하지 않는 path 포함 시 batch 전체 거부(부분 삭제로 인한 모호한 상태 방지). 복구 가능하도록 영구 삭제 대신 trash 이동을 권장(trash 목록·restore 도구, overwrite/edit의 이전 버전 복구 대칭은 **별도 ADR**). 출력(성공): `{ deleted: [path…], trashed_to }` — trash 위치를 함께 줘 복구 경로를 안다.

---

## 4. 설계 고려사항 (구현 시 필수)

- **Path traversal 차단**: 모든 path를 vault root 기준 canonicalize 후 root 밖 이탈 검증. `..`, symlink escape, absolute path 거부. 이 부류 MCP의 1순위 보안 결함. vault root 자체를 가리키는 path(빈 문자열·`/`)에 대한 파괴적 연산(delete/rename/relocate)도 거부.
- **Atomic write**: 동일 디렉토리에 temp 파일 작성 후 `rename(2)`로 교체. partial write 방지. cross-filesystem rename은 atomic 보장 안 되므로 vault 내부 temp 사용. **다중 파일 batch의 원자성은 서버가 보장**: 적용 전 영향받는 모든 파일의 원본을 백업(또는 write-ahead 저널 기록)하고 전 item을 temp로 선작성 → 일괄 rename으로 commit. 도중 어느 rename이라도 실패하면 백업으로 **전부 롤백**해 무적용으로 되돌린다. commit 중 crash가 나면 서버 재시작 시 저널을 보고 incomplete batch를 롤백해 일관성을 복구한다. 롤백은 op별 undo(create→생성파일 삭제, delete→trash 복원, edit/overwrite→백업 복원). (이 트랜잭션 메커니즘은 ADR 대상.)
- **동시성·격리**: readers-writer 락 — 쓰기는 직렬화(단일 exclusive write lease), 읽기는 서로 동시 허용하되 **commit 단계와는 배타**. 따라서 read는 진행 중 commit과 겹치지 않아 다중 파일 read도 torn snapshot(파일 간 신·구 혼재)을 보지 않는다. 외부 sync의 변경 감지는 `expected_hash`가 담당하는 별개 층.
- **내부 상태 격리**: 트랜잭션 저널·원본 백업·delete trash는 노트 네임스페이스 **밖**에 둔다(vault 외부 또는 `.md-mcp/` 숨김 디렉토리). `list_notes`/`search_notes`는 dot-디렉토리와 이 내부 디렉토리를 기본 제외해 메모리·sync 오염을 막는다.
- **Batch transaction 경계**: 파괴적 도구의 all-or-nothing은 "전부 적용 또는 전부 미적용"을 실제로 보장해야 한다. batch 검증(전 item 대상 해석·충돌·hash 확인)을 모두 통과한 뒤에야 write 단계로 진입. 검증/적용 단계를 분리한다.
- **대상 겹침 금지 (파괴적 batch 공통)**: 한 batch에서 두 item의 대상이 **동일하거나 포함관계**면 거부 — path 레벨(조상↔자손 디렉토리·파일: `rename`/`relocate`의 source, `delete`의 path), 섹션 레벨(조상↔자손 heading_path: `edit_sections`). `rename`/`relocate`는 추가로 **최종 destination 중복**·**destination이 batch 내 다른 source와 동일**(swap·순서의존)도 거부 — swap은 temp 이름 경유. 도구가 늘어도 이 원칙을 따른다.
- **heading_path 매칭·정규화**: heading_path는 **조상 heading을 포함**하므로 부모가 다른 동명 heading(`# Q1 > ## Status` vs `# Q2 > ## Status`)은 서로 다른 path라 구분된다 — `occurrence`는 *full heading_path가 완전히 같은*(같은 부모 아래 형제 중복) 경우에만 발생/필요. 그때 `occurrence` 없으면 다수 매칭 → error, 있으면 문서 순서 1-based 선택. heading 비교 시 trailing `#`, 공백, inline markdown 정규화 기준을 명시(읽기/쓰기 도구가 동일 기준을 써야 `read_outlines`로 본 `occurrence`가 `edit_sections`에서 동일하게 해석됨). path 비교·heading_path 매칭·dedup은 **Unicode NFC 정규화** 후 수행(macOS NFD↔Linux NFC 다기기 sync 대응). heading 텍스트는 위 정규화 후 **대소문자 구분**(case-sensitive).
- **Frontmatter 직렬화 안정성**: 미변경 시 원본 byte 보존, 변경 시 고정 직렬화기. `edit_properties` 외 경로로는 YAML 미변경. YAML 파싱 실패(깨진 frontmatter)면 read 계열은 `error{code:FRONTMATTER_PARSE}`로 보고하고 `edit_properties`는 거부(깨진 YAML 위에 쓰지 않음).
- **줄바꿈·인코딩**: 파싱은 LF·CRLF 모두 인식, **write는 항상 LF(`\n`)로 정규화**(CRLF·혼재를 LF로 통일). frontmatter "원본 byte 보존"도 줄바꿈은 LF 기준. 파일 인코딩 UTF-8.
- **content_hash 정의·출처**: **대상 섹션의 content**(대상 heading 라인 제외, **LF 정규화한 canonical 형태** — CRLF/LF 차이로 false conflict가 나지 않게)를 주어진 `scope`로 해석한 해시(예: SHA-256). **노트 전체 파일 해시 개념은 없다.** **scope 의존** — `scope=body`면 lead 본문만, `section`이면 하위 subsection 포함 범위로 해시하므로, read와 edit의 `scope`가 같아야 `expected_hash`가 일치한다. 섹션 단위라 겹치지 않는 섹션(또는 body vs subsection)을 동시 편집해도 false conflict가 없다. **생산처는 `read_sections`(읽은 섹션)와 `edit_sections`(편집 후 섹션)**, **소비처는 `edit_sections`의 `expected_hash`**. 빈 `heading_path`(노트 전체 body)도 하나의 섹션으로 취급. `read_notes`·`read_outlines`·`create`/`append`·`edit_properties`는 hash와 무관 — frontmatter는 섹션이 아니므로 `edit_properties`엔 optimistic concurrency가 없다(last-write-wins).
- **조건부 필수(서버 검증)**: JSON Schema로 표현이 어려운 조건부 필수는 서버가 강제한다 — `edit_sections`는 `operation`이 replace/append/insert_*면 `content`, `rename`이면 `new_heading`, `move`면 `destination` 누락을 거부(`delete`는 추가 필드 없음). `edit_properties`는 `value` 필드 유무로 set/remove를 가르며(생략=제거), 이 "필드 부재" 의미를 서버가 명시적으로 해석한다. `search_notes`는 `query`·`frontmatter`·`frontmatter_exists` 중 최소 하나가 없으면 거부.
- **상대 링크 무결성(범위 밖·후속)**: `rename_notes`·`relocate_notes`는 노트 경로를 바꿔 markdown 상대 링크 `[..](path.md)`를 깨뜨릴 수 있다. 링크 자동 갱신은 현 spec 범위 밖(후속 옵션) — 두 도구가 §4의 이 항목을 참조한다.
- **콜 간 비원자성**: 여러 도구 호출에 걸친 트랜잭션은 없다 — `relocate_notes`→`rename_notes` 2콜 중 후자가 실패해도 전자는 이미 적용된다. 각 콜이 결과를 보고하므로 에이전트가 부분완료를 감지·복구해야 한다. 무인 대규모 재구성이 필요해지면 별도 `begin/commit_transaction`을 후속 ADR로 검토.
- **토큰 비용 통제**: `list_notes`/`search_notes`에 limit 강제 + cursor paging. 본문 전체 반환은 read 계열로 한정. 큰 노트는 `read_outlines`→`read_sections` 경로 유도. 본문 반환 응답이 과대하면 도구가 **item 단위로** 잘라 빠진 index를 `omitted:[…]`로 보고(반환된 item은 완전 — content 중간 절단 금지). item당 content 크기·단일 노트 heading 수에도 서버 상한을 둬 단일 거대 입력/출력을 막는다.
- **Cursor 안정성**: paging 정렬 기준을 고정(list=path 사전순, search=content score 있으면 score·없으면 path 사전순)해야 cursor가 중복·누락 없이 이어진다. cursor는 정렬 기준 위치를 인코딩한 opaque 문자열. vault 변경 중 paging은 best-effort(중간 삽입/삭제까지 보장하지 않음)임을 명시.
- **structured output**: MCP `outputSchema` 정의. `exists`·`found`·`content_hash`·`error` 같은 분기 필드를 명시해 LLM이 안정적으로 파싱. error 객체는 전 도구 공통 `{ index, item?, operation?, code, message }`(위 출력 envelope) — `operation`은 `edit_sections`처럼 item마다 명령이 다른 경우에만.
