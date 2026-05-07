<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# flowmux 프로젝트 공통 룰

이 문서는 flowmux 작업 시 항상 따라야 하는 컨벤션을 정리한다.

## 호칭

UI 라벨, 사용자 대상 메시지(노티/오류/툴팁), 커밋 메시지 본문, README,
사용자와의 대화 등 사람이 읽는 모든 표현에서 아래 호칭을 사용한다.
코드 식별자(타입/변수/함수 이름)는 영문 원형을 유지한다.

| 호칭 | 정의 | 대응 코드 식별자 |
|---|---|---|
| 사이드 패널 | 왼쪽 사이드 패널 전체 영역 | `Sidebar` |
| 채널 | 사이드 패널의 각 탭 (작업 단위) | `Workspace` |
| 채널 네임 | 사이드 패널의 각 탭에 표시되는 이름 | `Workspace.name` |
| pane | 화면 내부에서 split으로 분할되는 단일 창 | `Pane` |
| 탭 | pane 내부에 출력되는 각 terminal | `PaneSurface` (`SurfaceKind::Terminal`) |
| 탭브라우저 | 탭과 동등한 수준으로 pane 내부에 출력되는 브라우저 | `PaneSurface` (`SurfaceKind::Browser`) |
| 탭 네임 | terminal 상단에 탭으로 출력되는 이름 | `PaneSurface.title` |

### 적용 규칙

- 사용자에게 보이는 텍스트(UI, 알림, 에러, CLI 도움말, 사용자와의 대화)는
  위 호칭만 사용한다. "workspace", "surface", "side panel" 같은 영문/이전
  표현을 새로 작성하지 않는다.
- 코드 식별자는 기존 영문 이름을 유지한다. 일괄 rename은 별도 결정이
  있을 때만 수행한다.
- 코드 주석에서 동작을 설명할 때 사람이 읽기 쉬운 흐름이면 호칭을
  사용하고(예: "채널 전환 시 …"), 식별자를 직접 가리키면 영문 식별자를
  쓴다(예: "the active `Workspace` row").
- 한 문서/한 화면 안에서 같은 개념을 두 호칭으로 섞어 부르지 않는다.

## 커밋/푸시 후 로컬 설치

커밋을 만들고 GitHub에 푸시하는 작업까지 수행했다면, 이어서 반드시
release 빌드와 로컬 설치까지 진행한다.

- 기본 검증은 푸시 전에 수행하고, 실패하면 원인과 미완료 상태를 보고한다.
- 푸시 후 `cargo build --release -p flowmux-app -p flowmux-cli`로 빌드한다.
- 빌드가 성공하면 `cargo install --path crates/flowmux-app --force --locked`와
  `cargo install --path crates/flowmux-cli --force --locked`로 사용자 로컬
  Cargo bin 디렉터리에 설치한다.
- GUI 실행을 위해 `resources/desktop/com.flowmux.App.desktop`을
  `~/.local/share/applications/com.flowmux.App.desktop`에도 설치하고,
  가능하면 데스크톱 DB를 갱신한다.
- 빌드나 설치가 실패하면 실패한 명령과 이유를 사용자에게 명확히 보고한다.
