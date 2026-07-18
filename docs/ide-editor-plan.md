<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# Flowmux 정밀 IDE 편집 환경 구현 계획

## 1. 문서 목적

이 문서는 Flowmux에 확장 마켓이나 VS Code 호환 계층을 넣지 않고도, 일상적인 개발 작업에 충분히 정밀하고 안정적인 편집 환경을 추가하기 위한 구현 계획이다.

핵심 목표는 다음과 같다.

- 파일 뷰어에서 파일을 더블클릭하거나 `Enter`를 누르면 적절한 pane에서 즉시 편집한다.
- 편집, 저장, 찾기, 바꾸기, 빠른 파일 열기, 워크스페이스 검색을 제품 기본 기능으로 제공한다.
- 파일 변경 충돌, dirty 문서 종료, 비정상 종료 복구까지 포함해 데이터 손실을 방지한다.
- Flowmux의 기존 pane, surface, focus, tab, workspace 복원 구조에 자연스럽게 통합한다.
- 언어별 지능형 기능은 확장 시스템 대신 표준 Language Server Protocol(LSP)을 직접 연결한다.
- Flowmux를 작은 VS Code처럼 복제하지 않고, terminal·browser·file browser와 조화를 이루는 Flowmux 고유의 편집 경험을 만든다.

이 계획에서 `Editor surface`는 Flowmux pane의 바깥 tab 하나를 뜻하며, 그 안에서 여러 파일을 문서 tab으로 관리한다.

## 2. 확정 범위와 비목표

### 2.1 포함 범위

- Monaco Editor 기반 텍스트 편집기
- 여러 문서 열기와 문서 tab 전환
- 저장, 모두 저장, 다른 이름으로 저장
- undo/redo, multi-cursor, 들여쓰기, 주석, 접기, bracket matching
- 파일 내 찾기·바꾸기
- 빠른 파일 열기
- 워크스페이스 전체 검색·바꾸기
- syntax highlighting과 언어 자동 판별
- 외부 파일 변경 감지와 충돌 처리
- dirty 문서 종료 보호
- 비정상 종료 후 문서 복구
- 테마, 글꼴, zoom, word wrap, minimap 등 기본 설정
- 선택적 직접 LSP 연동
- Problems/diagnostics 표시
- 기존 pane 이동, 분할, 닫기, tear-off, workspace 복원과의 통합

### 2.2 명시적 비목표

다음 항목은 이번 설계와 구현 범위에서 제외한다.

- VS Code extension API 또는 extension host
- Open VSX, Visual Studio Marketplace, VSIX 설치
- Theia, code-server, OpenVSCode Server 임베딩
- 확장을 통한 테마, keybinding, language package 설치
- 편집기 내부의 별도 terminal과 별도 file explorer
- Debug Adapter Protocol과 debugger UI
- notebook, remote SSH, dev container
- 완전한 Git 클라이언트 UI
- AI completion, 실시간 공동 편집
- multi-root workspace
- 언어 서버 자동 다운로드 또는 임의 실행 파일 설치

이 경계를 유지해야 제품 복잡도, 공급망 위험, 메모리 사용량, 라이선스 검토 범위를 통제할 수 있다. 나중에 plugin 시스템이 필요해지더라도 이번 Editor API와 직접 결합하지 않고 별도 ADR로 다룬다.

## 3. 현재 구조에서 확인된 연결 지점

현재 Flowmux에는 `Terminal`과 `Browser` 두 surface 종류가 있으며, pane 하나가 여러 surface를 tab으로 관리한다.

- surface 모델: `crates/flowmux-core/src/lib.rs`
- surface 렌더링 및 `PaneRegistry`: `crates/flowmux/src/ui/workspace_view.rs`
- 파일 더블클릭과 기본 애플리케이션 열기: `crates/flowmux/src/ui/file_browser.rs`
- pane focus MRU와 첫 leaf fallback: `crates/flowmux/src/ui/window/mod.rs`
- surface 생성·활성화 명령: `crates/flowmux/src/ui/window/pane_commands.rs`
- GTK command bridge: `crates/flowmux/src/bridge/mod.rs`

현재 파일 뷰어의 더블클릭은 Markdown viewer 또는 운영체제 기본 애플리케이션을 직접 실행한다. 이를 `OpenFileInEditor` 명령으로 전환하되, 이미지·영상·PDF·binary와 `Open Externally` 동작은 보존한다.

새 pane 체계를 만들지는 않는다. 기존 surface 구조에 `Editor`를 추가하고, 기존 focus·drag·close·restore 흐름을 그대로 확장한다.

## 4. 핵심 제품 결정

### 4.1 편집 엔진은 Monaco Editor를 직접 사용한다

Monaco는 편집 품질과 접근 가능한 API가 충분하고, Flowmux 전용 UI를 구성하기 쉽다. 다만 Monaco는 독립 편집기일 뿐 VS Code 확장 실행 환경이 아니며, 이번 계획에서도 그런 호환성을 만들지 않는다.

초기 선택은 다음과 같다.

| 항목 | 결정 |
|---|---|
| 편집 엔진 | Monaco Editor |
| 화면 호스트 | 기존 플랫폼 WebView 계층 |
| 프론트엔드 | 최소 TypeScript 애플리케이션 |
| 파일 접근 | Rust `DocumentService`만 수행 |
| 런타임 | Node.js 없음 |
| 프론트엔드 빌드 | 고정된 패키지 버전으로 빌드 시에만 Node.js 사용 |
| 언어 기능 | 선택적 직접 LSP 연결 |
| 확장 기능 | 제공하지 않음 |

Monaco 또는 WebView가 Phase 0의 필수 기준을 통과하지 못하면 GtkSourceView 5를 대체 후보로 평가한다. 두 엔진을 동시에 유지하지는 않는다.

### 4.2 파일 하나마다 WebView를 만들지 않는다

pane마다 workspace root가 같은 Editor surface를 최대 하나 재사용하고, 그 안에서 Monaco document model 여러 개를 전환한다.

```text
Workspace
└─ Pane
   ├─ Terminal surface
   ├─ Editor surface
   │  ├─ src/main.rs
   │  ├─ Cargo.toml
   │  └─ README.md
   └─ Browser surface
```

이 구조의 장점은 다음과 같다.

- 기존 Flowmux tab은 도구 역할(Terminal/Editor/Browser)을 명확히 유지한다.
- Editor 내부 tab은 파일만 나타내므로 tab 중첩의 의미가 분명하다.
- 파일 수가 늘어도 WebView와 worker 수가 파일 수만큼 증가하지 않는다.
- 저장, 검색, LSP, 복구 상태를 Editor 단위에서 일관되게 관리할 수 있다.

동일 workspace에서 이미 열린 파일은 기존 문서를 찾아 focus한다. v1에서는 같은 파일을 여러 Editor surface에 중복해서 열거나 split editor로 보여주지 않는다.

### 4.3 Flowmux가 파일 상태의 최종 소유자다

WebView JavaScript가 경로를 받아 직접 파일을 읽거나 쓰게 하지 않는다. Rust의 `DocumentService`가 다음을 전담한다.

- 경로 정규화와 workspace 경계 확인
- 파일 유형, 크기, encoding, line ending 판별
- 읽기, atomic save, Save As
- dirty version과 저장 version 관리
- 외부 변경 감지
- 충돌 처리와 복구 snapshot
- 검색 결과와 열린 문서 내용의 일관성 유지

Monaco는 화면과 편집 모델을 담당하고, 디스크 상태와 수명주기 정책은 Rust가 담당한다.

## 5. 목표 아키텍처

```text
File Browser
  └─ OpenFileInEditor(path, source_pane)
      └─ EditorTargetResolver
          ├─ source pane
          ├─ workspace pane MRU
          └─ first leaf fallback
              └─ Editor surface
                  ├─ EditorPane (platform WebView)
                  │   └─ flowmux-editor-web
                  │       ├─ Monaco editor
                  │       ├─ document tab strip
                  │       ├─ context rail
                  │       └─ status strip
                  └─ flowmux-editor Rust service
                      ├─ DocumentService
                      ├─ SearchService
                      ├─ RecoveryService
                      ├─ FileWatcher
                      └─ optional LspBroker
```

### 5.1 제안 디렉터리

```text
crates/
└─ flowmux-editor/
   ├─ src/document.rs
   ├─ src/search.rs
   ├─ src/recovery.rs
   ├─ src/protocol.rs
   ├─ src/web_assets.rs
   └─ src/lsp/                 # LSP phase에서 추가

editor/
└─ flowmux-editor-web/
   ├─ src/main.ts
   ├─ src/editor.ts
   ├─ src/tabs.ts
   ├─ src/status.ts
   ├─ src/bridge.ts
   └─ package.json
```

`flowmux-editor`는 UI framework에 의존하지 않는 Rust 서비스로 유지한다. GTK/WebView widget과 pane 연결은 기존 `crates/flowmux/src/ui`에 둔다.

### 5.2 상태 모델

영속 상태와 실행 중인 문서 내용을 분리한다.

```rust
SurfaceKind::Editor {
    workspace_root: PathBuf,
    session: EditorSessionState,
}

EditorSessionState {
    open_files: Vec<EditorFileState>,
    active_file: Option<PathBuf>,
}

EditorFileState {
    path: PathBuf,
    cursor_line: u32,
    cursor_column: u32,
    scroll_top: f64,
}
```

실제 문서 본문, undo stack, dirty buffer는 일반 workspace state JSON에 넣지 않는다. dirty buffer는 별도 recovery 저장소에 보관한다. 기존 상태 파일을 그대로 읽을 수 있도록 새 variant 추가 후 serde 하위 호환 테스트를 둔다.

### 5.3 Rust-WebView protocol

문자열 명령을 산발적으로 추가하지 않고 version이 있는 typed message를 사용한다.

Rust → WebView의 최소 메시지:

- `InitializeEditor`
- `OpenDocument`
- `UpdateDocumentFromDisk`
- `SaveCompleted`
- `SaveFailed`
- `CloseDocument`
- `SetActiveDocument`
- `SetTheme`
- `SetDiagnostics`
- `ShowConflict`

WebView → Rust의 최소 메시지:

- `EditorReady`
- `DocumentChanged`
- `SaveRequested`
- `SaveAsRequested`
- `CloseRequested`
- `ActiveDocumentChanged`
- `CursorChanged`
- `FindInWorkspaceRequested`
- `OpenPathRequested`
- `LspActionRequested`

모든 메시지는 protocol version, editor surface ID, document ID, document version을 포함한다. 저장 응답은 요청 version과 일치할 때만 clean 상태로 전환한다. 대용량 본문 전송 방식은 Phase 0에서 측정해 JSON 한계가 확인되면 chunking 또는 별도 loopback endpoint를 사용한다.

### 5.4 WebView 보안 경계

Monaco worker가 안정적으로 동작하도록 `file://` 대신 loopback의 임시 local origin에서 정적 asset을 제공한다.

- `127.0.0.1`에만 bind하고 매 실행마다 임의 token을 사용한다.
- 임의 외부 URL navigation을 차단한다.
- editor용 WebView session을 일반 Browser surface와 분리한다.
- Content Security Policy로 script, worker, connect source를 필요한 local origin으로 제한한다.
- WebView에서 운영체제 경로나 환경 변수에 직접 접근할 API를 노출하지 않는다.
- bridge 입력의 message 크기, path, document ID, version을 Rust에서 검증한다.
- editor WebView의 download, popup, permission 요청은 기본 거부한다.

## 6. 파일 열기와 pane 선택 규칙

### 6.1 대상 pane 결정

`EditorTargetResolver`는 다음 순서를 항상 동일하게 적용한다.

1. 파일 뷰어를 연 `source_pane`이 현재 workspace에 남아 있으면 선택한다.
2. 그렇지 않으면 현재 workspace의 가장 최근 focus pane을 선택한다.
3. MRU가 없으면 workspace의 `first_leaf_id()`를 선택한다.
4. 선택한 pane에 같은 workspace root의 Editor surface가 있으면 재사용한다.
5. 없으면 Editor surface를 새 tab으로 추가하고 활성화한다.
6. 동일 파일이 현재 workspace의 다른 Editor surface에 이미 열려 있으면 그 surface를 focus한다.
7. 기존 Terminal 또는 Browser surface를 교체하거나 닫지 않는다.

resolver는 UI와 분리된 순수 함수로 작성해 pane 삭제, 빈 MRU, 오래된 source pane, 이미 열린 파일 조건을 단위 테스트한다.

### 6.2 파일 종류별 동작

| 파일 종류 | 기본 동작 |
|---|---|
| 일반 텍스트 | Editor에서 편집 |
| Markdown | Editor에서 편집 |
| 이미지·영상·PDF | 기존 preview/viewer 유지 |
| 명백한 binary | 편집하지 않고 viewer 또는 외부 열기 안내 |
| 유효하지 않은 UTF-8 | v1에서는 read-only 또는 명확한 오류 안내 |
| 설정 한도를 넘는 대용량 파일 | large-file mode 또는 외부 열기 안내 |

Markdown preview와 `Open Externally`는 context menu의 명시적 동작으로 보존한다. 확장자만 믿지 않고 null byte, UTF-8 유효성, 파일 크기 등 최소한의 content sniffing을 함께 사용한다.

### 6.3 중복 열기 정책

경로는 canonical identity로 비교한다. 단, symlink를 열었을 때 저장 대상이 의도치 않게 바뀌지 않도록 표시 경로와 실제 identity를 분리해 보관한다.

- `display_path`: 사용자가 열었던 경로와 UI 표시용
- `identity_path`: 중복 판별과 file watcher용 canonical path
- `save_path`: 실제 저장 대상이며 symlink 정책을 따른다

workspace 밖을 가리키는 symlink를 열 수 있는지는 기존 Flowmux 보안 정책과 함께 결정한다. 정책 확정 전에는 경계를 넘는 파일을 read-only로 여는 것이 안전한 기본값이다.

## 7. 편집기 UX

### 7.1 화면 구성

Flowmux Editor는 별도 IDE shell이나 activity bar를 만들지 않는다.

```text
┌ document tabs ───────────────────────────────────────┐
│ context rail: workspace / relative/path.rs  ● dirty │
├─────────────────────────────────────────────────────┤
│                                                     │
│                   Monaco editor                     │
│                                                     │
├─────────────────────────────────────────────────────┤
│ Ln 24, Col 8   Spaces: 4   UTF-8   LF   Rust       │
└─────────────────────────────────────────────────────┘
```

- 바깥 Flowmux tab: Terminal, Editor, Browser 같은 도구 전환
- 안쪽 document tab: Editor 안에서 열린 파일 전환
- context rail: workspace 색과 상대 경로, dirty/read-only/external-change 상태
- status strip: cursor, indentation, encoding, line ending, language 상태
- 기존 file browser: 프로젝트 탐색의 유일한 기본 UI
- 기존 right panel: 후속 phase에서 Files/Search/Problems mode를 갖는 `Workspace Tools`로 확장

context rail을 Editor의 시각적 signature로 사용해 현재 파일의 workspace 소속과 저장 상태를 한눈에 구분한다. VS Code의 activity bar와 sidebar 배치를 복제하지 않는다.

### 7.2 focus와 shortcut 정책

shortcut은 focus context에 따라 해석한다. 기존 Flowmux 전역 shortcut을 빼앗지 않는다.

| 입력 | Editor focus일 때 |
|---|---|
| `Ctrl+S` | 현재 문서 저장 |
| `Ctrl+Shift+S` | 다른 이름으로 저장 |
| `Ctrl+Alt+S` | 열린 문서 모두 저장 |
| `Ctrl+F` | 현재 파일 찾기 |
| `Ctrl+H` | 현재 파일 바꾸기 |
| `Ctrl+Shift+F` | workspace 검색 |
| `Ctrl+P` | 빠른 파일 열기 |
| `Ctrl+G` | 줄로 이동 |
| `Ctrl+W` | 현재 문서 닫기 |
| `Alt+W` | 기존 Flowmux surface 닫기 유지 |
| `Ctrl+Shift+P` | 기존 Flowmux command palette 유지, Editor 명령도 등록 |

outer surface 이동 shortcut과 document tab 이동 shortcut은 충돌하지 않게 별도 keymap으로 확정한다. IME 조합 중에는 전역 command가 키 입력을 가로채지 않도록 composition 상태를 반드시 확인한다.

### 7.3 접근성과 테마

- GTK theme의 배경·전경·accent·selection token을 Editor theme로 전달한다.
- 편집 글꼴은 사용자 설정 가능한 monospace stack을 사용한다.
- 고대비 테마와 screen reader mode를 검증한다.
- tab, context rail, conflict banner의 모든 상태를 색만으로 전달하지 않는다.
- keyboard만으로 문서 열기, 저장, 검색, 충돌 해결, 닫기가 가능해야 한다.
- animation은 최소화하고 reduced-motion 설정을 존중한다.
- 한글·일본어 IME, dead key, emoji, 복사·붙여넣기를 Linux와 macOS에서 검증한다.

## 8. 파일 안전성과 문서 수명주기

### 8.1 읽기와 저장

파일을 열 때 다음 정보를 함께 기록한다.

- encoding과 UTF-8 BOM 유무
- `LF`/`CRLF`
- final newline 유무
- 파일 권한
- 수정 시각과 가능한 경우 file identity
- 읽어 온 content hash

저장은 같은 디렉터리의 임시 파일에 쓴 뒤 flush와 rename을 사용하는 atomic save를 기본으로 한다. 원본 권한을 보존하고 실패 시 원본을 손상시키지 않는다. symlink, 권한 부족, read-only filesystem, 디스크 부족을 각각 구분해 사용자에게 알린다.

자동 format과 저장이 함께 일어날 때는 하나의 document version transaction으로 취급한다. 저장 완료 전에 더 편집되었다면 이전 저장 응답이 현재 문서를 clean으로 표시해서는 안 된다.

### 8.2 dirty close guard

다음 모든 종료 경로가 하나의 `DocumentCloseGuard`를 거쳐야 한다.

- document tab 닫기
- Editor surface 닫기
- pane 닫기
- workspace 닫기
- tear-off window 닫기
- Flowmux 종료

dirty 문서가 있으면 `Save`, `Discard`, `Cancel`을 제공한다. 여러 문서가 dirty이면 문서별 선택이 가능한 목록을 표시한다. 저장 실패 후에는 닫기를 계속 진행하지 않는다.

### 8.3 외부 변경 충돌

file watcher가 디스크 변경을 감지했을 때 정책은 다음과 같다.

| Editor 상태 | 처리 |
|---|---|
| clean | cursor와 view state를 보존하며 자동 reload |
| dirty, 디스크 내용 동일 | 아무 작업 없음 |
| dirty, 디스크 내용 변경 | conflict banner 표시 |
| 디스크에서 삭제 | deleted 상태 표시, Save As 또는 재생성 선택 |

충돌 banner는 `Compare`, `Keep Mine`, `Reload from Disk`를 제공한다. `Compare`는 Monaco diff view를 사용한다. `Keep Mine`은 즉시 덮어쓰지 않고 다음 저장 시 덮어쓸 대상임을 명확히 표시한다.

### 8.4 비정상 종료 복구

dirty buffer는 debounce된 recovery snapshot으로 저장한다.

- XDG state 위치 또는 플랫폼별 application state 디렉터리 사용
- 사용자만 읽을 수 있는 권한 적용
- workspace ID, identity path, base hash, document version과 함께 저장
- 정상 저장 또는 discard 시 해당 snapshot 삭제
- 다음 실행에서 원본 hash가 같으면 복원 제안
- 원본도 변경되었으면 diff와 함께 충돌 복원 제안
- recovery 파일에는 token, 환경 변수, 불필요한 workspace metadata를 넣지 않음

복구 기능이 없으면 Editor v1 완료로 보지 않는다.

## 9. 검색과 탐색

### 9.1 빠른 파일 열기

`SearchService`가 workspace 파일 index를 background에서 만든다.

- `.gitignore`, 숨김 파일 정책, Flowmux exclude 설정을 존중한다.
- fuzzy score와 최근 열었던 파일 가중치를 함께 사용한다.
- 결과는 상대 경로와 상위 디렉터리를 함께 표시한다.
- index 갱신은 file watcher event를 증분 반영한다.
- 대형 workspace에서는 index 진행 상태와 제한 모드를 표시한다.

### 9.2 workspace 검색

- Rust background task에서 실행하고 입력 debounce와 cancellation을 지원한다.
- 결과 수와 파일 크기에 상한을 두며 잘린 결과임을 표시한다.
- binary, generated directory, ignore pattern을 기본 제외한다.
- case, whole word, regex, include/exclude glob을 지원한다.
- 결과는 파일별로 묶고 line preview와 match highlight를 제공한다.
- 결과 선택 시 기존 Editor surface에서 정확한 range를 reveal한다.

초기 구현은 검증된 Rust 검색 라이브러리 또는 ripgrep과 동일한 ignore/regex 계층을 재사용한다. 쉘 문자열을 조립해 검색 명령을 실행하지 않는다.

### 9.3 workspace 바꾸기

replace는 검색보다 강한 안전장치를 둔다.

- 적용 전 변경 파일 수와 match 수를 표시한다.
- preview에서 파일별 변경 내용을 확인할 수 있게 한다.
- dirty로 열린 문서는 디스크 내용이 아니라 현재 Editor model에 적용한다.
- dirty model과 디스크 version이 충돌하면 일괄 적용을 중단한다.
- 적용 중 일부 파일 저장이 실패하면 성공·실패 목록을 정확히 보고한다.
- undo 범위를 명확히 표시하며, 완전한 project transaction을 보장할 수 없다면 그렇게 표현하지 않는다.

## 10. 언어 지능 기능

언어 기능은 Editor 핵심이 안정된 뒤 직접 LSP client로 추가한다. extension이나 language package 설치 UI는 만들지 않는다.

### 10.1 지원 방식

- 사용자의 `PATH` 또는 명시적 설정에 이미 설치된 language server만 실행한다.
- Rust에 소수의 검토된 built-in profile을 둔다.
- profile은 실행 파일 이름, root marker, language ID, 초기화 option만 정의한다.
- 서버 실행 전에 workspace trust와 실행 명령을 사용자에게 보여준다.
- 자동 다운로드와 임의 package manager 실행은 금지한다.
- 서버가 없거나 실패해도 기본 편집·저장·검색은 정상 동작한다.

초기 후보는 실제 사용자 수요와 테스트 가능성을 기준으로 2~3개만 선택한다. 예를 들어 Rust는 `rust-analyzer`, TypeScript/JavaScript는 별도 설치된 language server를 연결할 수 있다. 후보 목록은 구현 시작 시 별도 ADR에서 확정한다.

### 10.2 단계별 기능

1. diagnostics와 Problems 목록
2. completion, hover, signature help
3. go to definition, references, symbol navigation
4. rename, formatting, code action

문서 version과 LSP version을 일치시키고, 오래된 응답은 폐기한다. language server process는 workspace별로 관리하며 마지막 Editor가 닫힐 때 종료하거나 짧은 idle timeout 뒤 종료한다.

## 11. 단계별 구현 계획

기간은 1인 전담 개발의 계획 추정치이며 약속된 일정이 아니다. Phase 0 측정 결과에 따라 다시 산정한다.

### Phase 0 — 기술 타당성 spike, 약 1주

최소 Monaco 화면을 기존 WebView에 올리고 다음 항목만 검증한다.

- WebKitGTK와 WKWebView에서 Monaco worker 로드
- 한글·일본어 IME와 composition event
- clipboard, multi-cursor, undo/redo
- Flowmux global shortcut과 Editor shortcut의 focus 분리
- theme 전환, zoom, screen reader 기본 동작
- 10만 줄 파일, 긴 한 줄 파일, 다수 document model의 반응성
- 한 pane당 WebView 메모리와 첫 usable frame 시간
- Rust-WebView 대용량 message 왕복

성공 조건:

- 입력 누락, 조합 중복, focus 유실이 재현되지 않는다.
- worker와 local origin 구성이 두 플랫폼에서 안정적이다.
- 일반 파일 입력 시 사용자에게 보이는 지연이 없다.
- 메모리와 시작 시간이 합의한 예산 안에 들어온다.

실패 조건:

- WebKit에서 해결하기 어려운 IME 또는 focus blocker가 있다.
- 보안 경계를 약화해야만 worker를 실행할 수 있다.
- 단일 Editor surface의 idle 메모리가 제품 예산을 지속적으로 초과한다.

실패하면 GtkSourceView 5 spike를 진행하고 엔진 하나를 최종 선택한다. 이 decision gate 전에는 본 구현을 넓히지 않는다.

### Phase 1 — Editor surface와 상태 모델, 약 1주

- `SurfaceKind::Editor`와 session state 추가
- state store의 생성·갱신·복원 API 추가
- `PaneRegistry`에 editor와 active-editor map 추가
- surface build, activate, detach, move, tear-off, cleanup의 모든 match 확장
- tab icon, title, tooltip, active 상태 추가
- 이전 state JSON 하위 호환 테스트
- Editor가 없는 build configuration의 처리 결정

완료 조건:

- 빈 Editor surface가 Terminal/Browser와 같은 방식으로 생성·이동·닫기·복원된다.
- pane와 workspace 삭제 후 registry에 orphan이 남지 않는다.
- 기존 저장 상태를 오류 없이 읽는다.

### Phase 2 — EditorPane과 DocumentService, 약 1~2주

- Monaco web bundle과 재현 가능한 asset build 추가
- local asset server와 격리된 WebView context 추가
- typed bridge protocol 구현
- document open/change/save/save-as/close 구현
- encoding, BOM, line ending, final newline 보존
- atomic save와 권한 오류 처리
- file watcher와 clean auto-reload 구현
- dirty state와 기본 close guard 구현

완료 조건:

- 실제 파일을 열고 수정·저장한 내용이 terminal에서 즉시 동일하게 확인된다.
- 저장 실패가 원본 파일을 손상시키지 않는다.
- 외부 수정과 Editor 수정이 조용히 서로 덮어쓰지 않는다.

### Phase 3 — 파일 뷰어 연결, 약 1주

- `file_browser.rs`의 직접 `open_file()` 대신 open callback 주입
- `GtkCommand::OpenFileInEditor` 추가
- 순수 `EditorTargetResolver` 구현
- source pane → MRU → first leaf fallback 적용
- Editor surface 생성·재사용과 동일 파일 dedup 적용
- text/binary/preview/large-file classifier 구현
- Markdown preview와 `Open Externally` context action 보존
- Editor 준비 중 loading 및 실패 UI 추가

완료 조건:

- 더블클릭과 `Enter`가 동일한 resolver를 사용한다.
- 대상 pane이 삭제되거나 focus 기록이 오래되어도 결정적인 fallback이 동작한다.
- 같은 파일을 반복해서 열어도 중복 document가 생기지 않는다.
- 기존 이미지·영상·PDF preview가 회귀하지 않는다.

### Phase 4 — 정밀 편집 UX, 약 1~2주

- document tab strip, context rail, status strip 완성
- save all, close guard, 여러 dirty 문서 dialog 완성
- find/replace, go to line, command 등록
- syntax highlighting, folding, bracket matching, multi-cursor 노출
- word wrap, whitespace, minimap, font, zoom 설정
- theme와 focus 상태 연동
- keyboard navigation, screen reader, IME 검증
- external conflict banner와 diff view 완성

완료 조건:

- mouse 없이 핵심 편집 흐름을 완료할 수 있다.
- dirty, read-only, deleted, external-change 상태가 서로 구분된다.
- pane focus와 Editor focus를 오갈 때 입력 대상이 명확하다.

### Phase 5 — workspace 탐색과 검색, 약 1~2주

- ignore-aware workspace index 구현
- quick open과 recent weighting 구현
- 취소 가능한 workspace search 구현
- 검색 결과 preview와 range reveal 구현
- 확인·preview가 있는 workspace replace 구현
- 기존 right panel을 Files/Search mode의 Workspace Tools로 확장

완료 조건:

- 대형 검색을 취소한 뒤 background task나 stale 결과가 남지 않는다.
- dirty document가 검색·바꾸기에서 누락되거나 디스크 내용으로 덮이지 않는다.
- index와 실제 파일 트리의 변경이 수렴한다.

### Phase 6 — 직접 LSP 연동, 약 2~3주, 선택적 v1.1

- workspace별 `LspBroker`와 process lifecycle 구현
- 제한된 built-in language profile 추가
- text document sync와 version 폐기 규칙 구현
- diagnostics와 Problems mode 추가
- completion, hover, definition, references 추가
- 안정화 뒤 rename, formatting, code action 추가

완료 조건:

- language server가 없거나 crash해도 Editor 핵심 기능은 유지된다.
- 오래된 응답이 현재 문서의 diagnostics나 edit를 덮지 않는다.
- workspace trust 없이 language server process를 자동 실행하지 않는다.

### Phase 7 — 복구·성능·출시 안정화, 약 1~2주

- crash recovery snapshot과 restore UI 완성
- large-file mode 임계값을 benchmark로 확정
- 장시간 open/edit/search 반복의 CPU·RSS 누수 검사
- pane split/drag/tear-off/close와 앱 종료 회귀 검사
- frontend dependency lock, license notice, SBOM 갱신
- Linux 패키징과 macOS bundle asset 경로 검증
- 실제 실행 중인 Flowmux에서 사용자 시나리오 검증

완료 조건:

- 강제 종료 뒤 dirty 문서를 복구할 수 있다.
- 숨겨진 Editor surface가 지속적으로 CPU를 사용하지 않는다.
- 설치 패키지에 editor asset과 필수 license notice가 포함된다.

### 일정 요약

| 범위 | 1인 예상 |
|---|---:|
| Phase 0~4, 편집 중심 MVP | 약 5~7주 |
| Phase 0~5와 Phase 7, 정밀 v1 | 약 8~11주 |
| Phase 6 직접 LSP 추가 | 추가 약 2~3주 |

## 12. 테스트와 검증 전략

### 12.1 단위 테스트

- target pane resolver의 모든 fallback
- state serde 하위 호환
- display/identity/save path 처리
- text/binary/large-file 판별
- BOM, encoding, line ending, final newline 보존
- atomic save 실패와 권한 보존
- document version과 save acknowledgement
- external-change conflict 상태 전이
- search cancellation과 결과 제한
- recovery snapshot 생성·삭제·충돌 복원

### 12.2 통합 테스트

- Rust-WebView protocol schema와 version mismatch
- 동일 파일 반복 open과 기존 surface focus
- Editor 준비 전 여러 open 요청의 순서 보존
- dirty document가 있는 surface/pane/workspace/window close
- pane move, split, tear-off 후 document 상태 유지
- workspace restore 후 open file, active file, cursor 복원
- search result에서 파일과 정확한 range 열기
- language server crash와 재시작

### 12.3 실제 UI 검증

자동 테스트만으로 완료 처리하지 않는다. 최소한 Ubuntu 24.04의 실행 중인 Flowmux에서 다음 시나리오를 재현한다.

1. terminal이 최근 focus된 pane과 file browser source pane이 다른 상태를 만든다.
2. file browser에서 텍스트 파일을 더블클릭한다.
3. 규칙에 맞는 pane에 Editor surface가 생성되는지 확인한다.
4. 한글을 입력하고 `Ctrl+S`로 저장한다.
5. terminal에서 파일 내용이 즉시 일치하는지 확인한다.
6. terminal에서 같은 파일을 수정해 clean reload와 dirty conflict를 각각 확인한다.
7. dirty 상태로 document, surface, pane, window 닫기를 각각 시도한다.
8. 앱을 강제 종료하고 recovery를 확인한다.
9. pane 이동·분할·tear-off 후 focus와 shortcut을 확인한다.
10. 기존 Terminal, Browser, Markdown preview, media preview를 회귀 검사한다.

macOS 지원 build에서는 같은 핵심 시나리오와 WKWebView IME/clipboard 검증을 반복한다. live verification이 불가능하면 완료 보고에 정확한 blocker와 미검증 시나리오를 남긴다.

## 13. 잠정 성능 예산

Phase 0에서 동일한 reference hardware와 fixture를 정한 뒤 수치를 확정한다. 아래 값은 초기 목표이지 측정 전 보장이 아니다.

| 항목 | 잠정 목표 |
|---|---:|
| 준비된 Editor에서 1 MiB 이하 파일 표시 | 200 ms 이내 |
| Editor cold start에서 첫 입력 가능 상태 | 1.5 s 이내 |
| 일반 입력 frame time p95 | 16 ms 이내 |
| 숨겨진 idle Editor CPU | 지속 1% 미만 |
| 10,000 파일 workspace 검색 첫 결과 | 500 ms 이내 |
| clean 외부 변경 반영 | watcher event 후 500 ms 이내 |

large-file mode는 syntax tokenization, minimap, semantic 기능을 단계적으로 끄고 파일을 read-only로 강제하지 않는 방향을 우선한다. 실제 임계값은 줄 수가 아니라 byte 크기, 최대 line 길이, tokenization 비용을 함께 측정해 정한다.

## 14. 위험과 중단 조건

| 위험 | 대응 | 중단 또는 축소 조건 |
|---|---|---|
| WebKit IME/focus 불안정 | Phase 0에서 실제 조합 입력 검증 | 제품 수준으로 해결할 수 없으면 GtkSourceView 평가 |
| WebView 메모리 증가 | pane당 Editor 하나, lazy start, hidden idle 측정 | 예산 초과가 지속되면 동시 Editor 수 정책 축소 |
| global shortcut 충돌 | focus-context dispatch, composition 보호 | 핵심 shortcut을 안정적으로 분리 못하면 UX 재설계 |
| 파일 손실 | 중앙 `DocumentService`, atomic save, close guard, recovery | 모든 종료 경로가 guard를 통과하기 전 출시 금지 |
| 검색 replace 오류 | preview, version check, partial failure 보고 | dirty 문서 일관성을 보장 못하면 replace를 v1에서 제외 |
| LSP 범위 팽창 | 핵심 Editor 뒤 별도 phase, profile 수 제한 | 핵심 일정에 영향이 생기면 v1.1로 이동 |
| frontend 공급망 증가 | lockfile, vendored build output, SBOM, audit | runtime dependency 또는 불명확한 license가 필요하면 채택 중단 |
| state write 과다 | cursor/scroll 변경 debounce, 종료 시 flush | UI thread I/O가 관찰되면 저장 전략 변경 |

## 15. 라이선스와 배포 원칙

- Monaco Editor의 MIT license와 notice를 배포물에 포함한다.
- frontend의 직접·전이 dependency license를 lockfile 기준으로 수집한다.
- GPL-3.0-or-later인 Flowmux source와 함께 필요한 source·notice 제공 의무를 유지한다.
- minified JavaScript만 배포하지 않고 대응하는 source와 재현 가능한 build 절차를 저장소에 둔다.
- editor asset 목록을 기존 third-party license 문서 또는 전용 generated notice에 반영한다.
- 이름, icon, UI 문구에서 Visual Studio Code 제품으로 오인될 branding을 사용하지 않는다.
- 확장 마켓과 확장 패키지를 다루지 않으므로 개별 extension license 및 marketplace 이용조건은 제품 범위에 들어오지 않는다.
- 새 frontend dependency를 추가할 때는 license, 유지보수 상태, bundle 크기, 알려진 보안 이슈를 PR gate에서 확인한다.

최종 출시 전에는 lockfile과 실제 bundle을 기준으로 다시 검토한다. 이 문서는 법률 자문을 대체하지 않는다.

## 16. 출시 승인 기준

다음 조건을 모두 만족해야 정밀 IDE 편집 기능 v1으로 간주한다.

- 파일 더블클릭과 `Enter`가 source pane → MRU → first pane 규칙대로 동작한다.
- 기존 Editor surface와 열린 문서를 중복 없이 재사용한다.
- 편집, 저장, 모두 저장, Save As, find/replace, quick open, workspace search가 동작한다.
- encoding, BOM, line ending, final newline과 파일 권한을 의도치 않게 바꾸지 않는다.
- dirty close, 외부 변경 충돌, 저장 실패, 비정상 종료에서 데이터 손실이 없다.
- 앱 재시작 후 open file, active file, cursor와 recovery 상태를 복원한다.
- 한글·일본어 IME, clipboard, keyboard-only 흐름, 고대비 상태를 검증한다.
- pane split, drag, tear-off, close, workspace restore가 Editor 상태를 손상시키지 않는다.
- Terminal, Browser, file preview의 기존 동작에 회귀가 없다.
- 정한 reference hardware에서 확정된 성능 예산을 통과한다.
- runtime Node.js, extension host, 외부 extension registry 없이 동작한다.
- dependency license notice, source, build 절차가 배포물과 저장소에 준비되어 있다.
- 실행 중인 Flowmux에서 사용자 시나리오 검증을 완료한다.

## 17. 구현 시작 전 필요한 ADR

구현을 시작하기 전에 다음 네 결정을 짧은 ADR로 확정한다.

1. Monaco WebView spike 결과와 GtkSourceView 대체 여부
2. editor local origin과 Rust-WebView transport 방식
3. symlink 및 workspace 밖 파일의 open/save 정책
4. v1에 직접 LSP를 포함할지 v1.1로 분리할지

ADR가 확정되기 전에도 Phase 0 spike는 진행할 수 있지만, Phase 1 이후의 영구 API와 저장 형식은 먼저 고정하지 않는다.

## 참고 자료

- [Monaco Editor](https://github.com/microsoft/monaco-editor)
- [Monaco Editor API](https://microsoft.github.io/monaco-editor/)
- [Language Server Protocol](https://microsoft.github.io/language-server-protocol/)
- [GtkSourceView](https://gitlab.gnome.org/GNOME/gtksourceview)
