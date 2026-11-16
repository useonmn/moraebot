# moraebot 전체 구조

> 상태: 설계 초안 v2, 2026-11-15, 구현 전.
> 이 문서는 설계만 적습니다. 아직 코드는 없습니다.
> "미검증"이라고 적은 부분은 조사에서 직접 확인하지 못한 내용입니다.

## 한 장 요약

moraebot은 Discord로 받은 메시지를 이 컴퓨터의 코딩 에이전트 omp에게 넘기고, omp의 답을 다시 Discord로 보내는 프로그램입니다.
처음 나오는 말은 바로 아래 "용어 풀이"에서 풀어 둡니다.

```
 Discord (주인의 메시지)
     |
     |  Gateway WebSocket (계속 연결된 통신)
     v
 moraebot 게이트웨이 (Rust, 항상 켜져 있음)
     |  1. 접근 제어 검사
     |  2. SQLite에 먼저 저장 (메시지 + 중복 방지 키)
     v
 SQLite (inbox.sqlite)
     |
     |  대화마다 줄 하나 (큐)
     v
 omp --mode rpc --session-dir <대화 폴더>/session   (대화 하나에 omp 하나)
     |
     |  prompt_result 도착
     v
 Discord에 답장
```

## 용어 풀이

이 문서에 자주 나오는 말을 먼저 풀어 둡니다.

- **Rust**: 프로그램을 만드는 언어 하나입니다. 게이트웨이를 이 언어로 만듭니다.
- **SQLite**: 파일 하나로 된 작은 데이터베이스입니다. 받은 메시지를 여기에 저장합니다.
- **게이트웨이**: Discord와 omp 사이에서 메시지를 받고 넘기는 moraebot의 본체 프로그램입니다.
- **프로세스**: 컴퓨터에서 실행 중인 프로그램 하나입니다. "자식 프로세스"는 게이트웨이가 직접 실행하고 관리하는 프로그램입니다.
- **RPC**: 다른 프로그램에 명령을 보내고 결과를 돌려받는 방식입니다. 여기서는 게이트웨이가 omp에 명령을 보내는 방식을 말합니다.
- **큐**: 들어온 순서대로 하나씩 꺼내 처리하는 대기 줄입니다.
- **트랜잭션**: 여러 저장 작업을 "전부 되거나, 하나도 안 되거나"로 묶는 단위입니다.
- **중복 방지 키**: 같은 메시지를 두 번 저장하지 않도록 메시지마다 붙이는 고유 값입니다. 여기서는 Discord 메시지 id를 씁니다.
- **토큰**: 봇이 Discord에 로그인할 때 쓰는 비밀 문자열입니다. 남이 알면 봇을 가져갈 수 있습니다.
- **DM**: Discord의 1:1 개인 메시지입니다.
- **id**: 사용자, 채널, 메시지마다 Discord가 붙이는 고유 번호입니다.
- **Discord 스레드**: Discord 채널 안에서 갈라져 나온 작은 대화방입니다. 이 문서에서 "스레드"만 쓰면 이것을 뜻합니다.
- **별도 작업 흐름(스레드)**: 프로그램 안에서 다른 일과 따로 도는 작업 단위입니다. Discord 스레드와는 다른 말입니다. 3장과 7장에만 나옵니다.
- **launchd / LaunchAgent**: launchd는 macOS가 프로그램을 자동으로 켜고 다시 켜 주는 기능입니다. LaunchAgent는 사용자가 로그인했을 때 그 사용자 권한으로 도는 launchd 등록입니다.
- **커밋**: git(코드 버전 관리 도구)으로 관리하는 코드의 특정 저장 시점입니다. 커밋으로 고정하면 늘 같은 버전의 코드를 씁니다.
- **hermes**: 조사한 다른 오픈소스 메신저 봇 프로젝트(hermes-agent)입니다. Discord 연결과 접근 제어를 참고합니다.
- **dinobot**: 조사한 또 다른 메신저 봇 프로젝트입니다. "먼저 저장하고 처리" 원칙을 참고합니다. 비상업용 라이선스라 코드는 가져오지 않습니다.
- **omp**: 이 컴퓨터에서 실제로 코딩 일을 하는 에이전트 프로그램(oh-my-pi)입니다.
- **Message Content intent**: 봇이 서버 메시지의 본문을 읽으려면 Discord 개발자 포털에서 켜야 하는 권한 설정입니다.
- **prompt_result**: omp가 메시지 하나에 대한 일을 마쳤을 때 보내는 "끝났다" 신호입니다. 성공, 중단, 오류 중 어느 쪽인지 들어 있습니다.

## 확정된 결정

아래 결정은 이미 정해졌습니다. 이 문서는 이 결정을 바꾸지 않습니다.

| 번호 | 결정 | 쉬운 설명 |
|---|---|---|
| D1 | 일하는 에이전트는 omp | omp는 oh-my-pi(github.com/can1357/oh-my-pi)입니다. 설치된 버전은 18.20.2입니다. |
| D2 | omp는 yolo 모드 | omp가 도구를 쓸 때 묻지 않고 바로 실행합니다. |
| D3 | 첫 메신저는 Discord | Gateway WebSocket으로 일반 메시지를 받습니다. 이를 위해 Message Content intent를 켜야 합니다. Telegram, Slack은 나중입니다. |
| D4 | 게이트웨이는 Rust로 만든 별도 프로그램 | 항상 켜져 있는 프로그램 하나입니다. |
| D5 | 접근 제어는 바꿀 수 있는 설정 | `owner_only`, `allowlist`, `pairing`, `open` 중 하나를 고릅니다. 기본값은 `owner_only`입니다. (`allowlist`는 적어 둔 사용자만, `pairing`은 모르는 사람이 코드를 받고 주인이 승인하는 방식, 2.6절) |
| D6 | 먼저 저장하고, 그다음 처리 | 받은 메시지를 SQLite에 먼저 저장하고 처리합니다. dinobot의 원칙만 참고하고 코드는 새로 씁니다. |
| D7 | 대화마다 omp 하나 | `omp --mode rpc --session-dir <대화 폴더>/session`을 대화마다 하나 띄웁니다. 그 메시지의 `prompt_result`가 오면 답장합니다. |
| D8 | 작게 시작해 하나씩 | 한 번에 기능 하나만 만듭니다. 지금은 문서만 씁니다. |
| D9 | 쉬운 한국어 | 주인은 개발자가 아닐 수 있습니다. 어려운 말은 처음 나올 때 풀어 씁니다. |

**가장 중요한 위험.** omp가 yolo 모드라서, 봇과 말할 수 있는 사람은 사실상 이 컴퓨터에서 아무 명령이나 실행할 수 있습니다. 파일 삭제, 비밀번호 파일 읽기, 외부로 데이터 보내기도 됩니다. 접근을 허락하는 것은 이 컴퓨터를 맡기는 것과 같은 결정입니다. 이 위험은 2장에서 다시 설명합니다.

## 1. 전체 구조

moraebot은 세 부분으로 이루어집니다.

1. **게이트웨이**: Rust로 만든 프로그램 하나입니다. Discord와 계속 연결되어 메시지를 받고, 저장하고, omp에 넘기고, 답장을 보냅니다.
2. **SQLite**: 파일 하나로 된 작은 데이터베이스입니다. 별도 서버가 필요 없습니다. 받은 메시지, 대화 목록, 허용 사용자 같은 상태를 저장합니다.
3. **omp 자식 프로세스들**: 자식 프로세스는 게이트웨이가 직접 실행하고 관리하는 프로그램입니다. 대화 하나에 omp 하나가 돕니다.

> **slice 2 메모.** 이제 Slack도 붙습니다. Slack은 Socket Mode(WebSocket)로 연결하고, 코드는 `src/slack.rs`에 있습니다. 메신저는 한 번에 하나만 켜집니다(`config.toml`의 `messenger`). 앱 설정 방법은 `docs/guides/`의 메신저별 안내서에 있습니다. 아래 그림의 Discord 자리에 Slack이 들어가도 흐름은 같습니다.

메시지 하나가 지나가는 길은 이렇습니다.

```
 Discord Gateway (WebSocket)
          |
          v
 [1] 접근 제어 검사 ---- 거부 --> 무시, 또는 pairing 안내
          |
          v
 [2] 제어 명령? (/stop, /new) --예--> 바로 실행 (줄 서지 않음)
          | 아니오
          v
 [3] SQLite 트랜잭션: 메시지 + 대화 키 + 중복 방지 키  (상태 = queued)
          |
          v
 [4] 대화 키별 큐
     th:111 [m1][m2]   th:222 [m3]   dm:333 [m4]
        |                  |              |
        v                  v              v
     omp #1             omp #2         omp #3   (서로 기다리지 않음)
          |
          v
 [5] 접근 제어 다시 검사 -> prompt -> prompt_result -> Discord 답장
```

## 2. 메시지 받기와 접근 제어

### 2.1 Discord에 붙는 방법

게이트웨이는 Discord Gateway에 WebSocket으로 늘 연결해 둡니다. WebSocket은 한 번 연결하면 계속 메시지를 주고받는 통신 방식입니다. hermes도 이 방식으로 일반 메시지를 받습니다.

- **intent**: 봇이 어떤 이벤트를 받을지 Discord에 미리 알리는 설정입니다. 서버 메시지와 DM 메시지 intent가 필요합니다.
- **Message Content intent**: 메시지 본문을 읽으려면 이 특권(privileged) intent를 켜야 합니다. 주인이 Discord 개발자 포털에서 직접 켭니다. 꺼져 있으면 일반 서버 메시지의 본문이 비어서 올 수 있습니다. DM과 봇을 멘션한 메시지는 예외일 수 있습니다. 이 동작은 조사 자료에 근거가 없어 **미검증**입니다.
- **재연결과 이어받기(resume)**: 연결이 끊기면 다시 붙고, 가능하면 끊긴 곳부터 이어받습니다. Discord 라이브러리가 이 일을 해 준다고 조사 자료에 적혀 있지만, 어디까지 해 주는지는 **미검증**입니다.
- **놓치는 메시지**: Discord는 오래 끊긴 동안의 메시지를 나중에 다시 보내 주지 않습니다. 게이트웨이가 꺼져 있던 동안 온 메시지는 놓칠 수 있습니다(열린 질문 5).

dinobot은 Discord를 interactions 웹훅으로만 받습니다. 이 방식은 슬래시 명령만 받고 일반 메시지는 못 받습니다. moraebot은 이 방식을 쓰지 않습니다.

**Rust 라이브러리.** Discord 쪽 후보는 `serenity` 0.12.5와 `twilight-gateway` 0.18.0입니다. serenity는 기능이 다 들어 있어 쓰기 쉽고, twilight는 필요한 부분만 골라 쓰는 가벼운 구성입니다. **serenity로 시작**하기를 권장합니다. 첫 단계는 받아서 저장하고 넘기는 일뿐이라 쉬운 쪽이 낫습니다. SQLite는 Rust용 라이브러리 `rusqlite` 0.43.0을 씁니다. 쓰기는 한 곳(쓰기만 맡는 별도 작업 흐름(스레드) 하나)에서만 합니다.

**Slack은 이렇게 붙습니다(slice 2).** `src/slack.rs`가 Socket Mode로 연결합니다. 공개 HTTP 주소가 필요 없습니다. 고정 설정(scope, event)은 `docs/guides/slack.md`에만 적습니다. 알아 둘 점 두 가지입니다.

- Slack 이벤트는 저장소에 들어가기 전에 ack합니다. 그래서 ack와 `admit` 사이에 죽으면 그 메시지는 사라집니다. Discord와 같은 위험입니다.
- Slack의 `file_share` 메시지는 조용히 버립니다. Discord처럼 `N_UNSUPPORTED_INPUT` 알림을 보내지 않습니다. 이번 단계에서는 받아들인 차이입니다.

### 2.2 무엇을 "대화 하나"로 볼까

대화 하나는 omp 하나, 세션 폴더 하나와 짝을 이룹니다. 그래서 메시지마다 **대화 키**를 정합니다. 대화 키는 이 메시지가 어느 대화에 속하는지 나타내는 문자열입니다.

| 메시지가 온 곳 | 대화 키 | 비고 |
|---|---|---|
| DM | `discord:dm:<채널id>` | 1:1이라 단순합니다 |
| 서버 채널 | `discord:ch:<채널id>` | 채널 하나가 대화 하나 |
| Discord 스레드 | `discord:th:<스레드id>` | **권장 기본 단위** |

**권장: Discord 스레드 하나가 대화 하나입니다.** 작업마다 스레드를 열면 대화가 깔끔하게 나뉘고, 여러 작업을 동시에 돌리기도 쉽습니다. hermes도 플랫폼, 채팅, 스레드, 사용자를 이어 붙여 키를 만듭니다. 대화 폴더 이름은 대화 키에서 `:`를 `-`로 바꿔 만듭니다(예: `discord-th-111`).

### 2.3 먼저 저장하고, 그다음 처리

메시지를 받으면 **처리하기 전에 SQLite에 먼저 저장**합니다. 이 원칙은 dinobot에서 가져왔고, 코드는 새로 씁니다. dinobot 라이선스는 비상업용이라 참고만 합니다.

- 한 트랜잭션 안에서 메시지 본문, 대화 키, 중복 방지 키(Discord 메시지 id)를 함께 넣습니다. 트랜잭션은 "전부 저장되거나, 하나도 저장되지 않거나" 둘 중 하나만 일어나게 하는 묶음입니다. 나중에 받은 위치 표시(커서)를 쓰는 메신저가 생기면 그 값도 같은 트랜잭션에 넣습니다.
- 중복 방지 키에는 UNIQUE 제약(같은 값이 두 번 들어갈 수 없다는 규칙)을 겁니다. 같은 메시지가 두 번 오면 두 번째는 저장되지 않고 건너뜁니다.
- 저장이 끝난 뒤에만 처리를 시작합니다.

dinobot은 상태 6개(`queued`, `processing`, `ready`, `sending`, `done`, `dead`)를 씁니다. moraebot은 4개로 시작합니다. 메시지 상태는 `queued`(대기), `processing`(처리 중), `done`(완료)로 바뀌고, 실패가 반복되면 `dead`가 됩니다. omp가 이상하게 끝나서 결과를 믿기 어려우면 "확인 필요"로 남깁니다(3장).

### 2.4 순서: 대화마다 큐 하나

큐는 들어온 순서대로 하나씩 꺼내 처리하는 줄입니다. moraebot은 **대화마다 큐를 하나씩** 둡니다.

- 같은 대화 안에서는 순서를 지킵니다. 앞 메시지가 끝나야 다음 메시지가 갑니다.
- 다른 대화끼리는 서로 기다리지 않습니다.

dinobot의 Discord와 Slack은 전체에 큐 하나(동시 처리 1개)를 씁니다. 그러면 한 Discord 스레드에서 긴 코딩 작업이 돌 때 다른 Discord 스레드가 전부 멈춰 기다립니다. 이것을 head-of-line blocking(맨 앞 작업이 뒤를 전부 막는 현상)이라고 합니다. 코딩 작업은 길어서 이 문제가 크게 드러나므로 따라 하지 않습니다.

처리 중인 대화에 새 메시지가 오면, 기본으로는 지금 일이 끝난 뒤에 이어서 처리합니다(3.4절, 열린 질문 4).

### 2.5 제어 명령은 줄 서지 않음

`/stop`(지금 작업 멈추기)과 `/new`(새 대화 시작)는 큐에 넣지 않고 **바로 실행**합니다. hermes가 이렇게 합니다. 큐에 넣으면 작업이 끝날 때까지 `/stop`이 기다려야 하므로 멈출 수가 없습니다.

- `/stop`: 그 대화의 omp에 `abort`를 보냅니다.
- `/new`: 지금 대화를 보관하고, 같은 키에 새 세션 폴더를 연결합니다. dinobot도 옛 대화를 `<키>#n`으로 보관합니다.

제어 명령도 접근 제어는 똑같이 거칩니다.

### 2.6 접근 제어: 누가 봇과 말할 수 있나

접근 제어는 코드에 고정하지 않고 **주인이 바꿀 수 있는 설정**입니다. 아래 네 가지 모드는 moraebot이 정한 것입니다. hermes에서는 기본 거부(명단에 없으면 막기), 허용 목록, pairing 방식을 가져옵니다.

| 모드 | 뜻 | 위험도 |
|---|---|---|
| `owner_only` (기본값) | 주인 한 명만 | 가장 낮음 |
| `allowlist` | 설정에 적은 사용자 id만 | 명단 관리에 달림 |
| `pairing` | 모르는 사람이 DM하면 짧은 코드를 받고, 주인이 승인해야 허용 | 주인 판단에 달림 |
| `open` | 누구나 | 매우 높음 |

hermes의 pairing은 8자리 코드, 1시간 만료, 플랫폼당 대기 요청 3개, 5번 실패하면 잠금 같은 안전장치를 둡니다. 그룹 채널에서는 pairing을 받지 않습니다. moraebot도 같은 제한을 권장합니다.

> **경고: omp는 yolo 모드로 돕니다.** 봇과 말할 수 있는 사람은 사실상 이 컴퓨터에서 아무 명령이나 실행할 수 있습니다. 파일 삭제, 비밀번호 파일 읽기, 외부로 데이터 보내기도 됩니다. 접근 설정이 유일한 안전장치입니다. `open`은 "Discord에서 봇을 찾은 누구나 이 컴퓨터를 조작할 수 있다"는 뜻입니다. `allowlist`와 `pairing`에도 정말 믿는 사람만 넣으세요. 모드를 넓힐 때 봇은 이 경고를 다시 보여 줘야 합니다.

검사는 두 번 합니다. 메시지를 받을 때 한 번, 큐에서 꺼내 omp로 보내기 직전에 한 번입니다(dinobot 방식). 그 사이에 주인이 권한을 뺐을 수 있기 때문입니다.

## 3. omp에 전달하기 (개발자용)

> omp-rpc 의존성: oh-my-pi v18.20.2 커밋(7c49c11a)을 git 의존성으로 직접 받습니다. 복사본(vendor)은 쓰지 않았습니다.

이 장은 구현할 사람을 위한 세부 내용입니다. 주인은 3.2절의 yolo 예외 설명과 3.5절의 첫 항목만 읽어도 됩니다.

### 3.1 용어

- **RPC 모드**: omp를 화면 없이 실행하고, 명령과 결과를 글자 줄로 주고받는 방식입니다. 한 줄에 JSON 객체 하나가 들어갑니다(JSONL).
- **stdin / stdout**: 자식 프로세스의 입력 통로와 출력 통로입니다. 게이트웨이는 stdin에 명령을 쓰고 stdout에서 결과를 읽습니다.
- **세션 폴더**: 대화 기록 파일이 저장되는 폴더입니다. 대화 하나에 폴더 하나를 쓰고, 위치는 늘 `<대화 폴더>/session`입니다.

### 3.2 한 대화에 omp 하나

대화마다 omp를 하나씩 띄웁니다.

```
omp --mode rpc --session-dir <대화 폴더>/session --cwd <작업 폴더> --approval-mode yolo
```

`--cwd`는 omp가 일하는 폴더입니다. yolo에도 예외가 하나 있습니다. omp 설정의 "제한 폴더" 안에서 하는 작업은 yolo여도 승인을 요청하고, 화면이 없으면 거부됩니다.

### 3.3 흐름

```
 대화별 큐에서 메시지 하나 꺼냄
          |
          v
 [자식 없음] omp 실행 --> stdout에 ready
          |
          +--> negotiate_protocol (버전 2)
          +--> open_session {세션 폴더}
          +--> prompt {id = Discord 메시지 id}
          |
 stdout 이벤트: agent_start, message_update, tool_execution_* ...
          +--> Discord "입력 중" 표시
          |
 prompt_result {id, status}
          +--> 이벤트에서 모은 답장 글을 Discord에 보냄, SQLite에 done 기록
```

1. **시작.** omp는 켜지자마자 stdout에 `ready` 한 줄을 씁니다. 지원하는 통신 규칙(프로토콜) 버전(1과 2)이 들어 있습니다. 게이트웨이는 곧바로 버전 2를 요청합니다(`negotiate_protocol`). 버전 2에서는 1 MiB(약 100만 바이트)가 넘는 큰 결과도 조각으로 나뉘어 잃지 않고 도착합니다.
2. **대화 열기.** `open_session`으로 세션 폴더를 엽니다. 이때 넘기는 `sessionDir`는 실행할 때 `--session-dir`로 준 `<대화 폴더>/session`과 같은 폴더입니다. 기록이 있으면 이어 가고, 없으면 새로 시작합니다. 응답의 `resumed` 값으로 어느 쪽인지 알 수 있습니다. `--continue` 옵션과 같은 일을 실행 중에 하는 것입니다.
3. **메시지 보내기.** `prompt` 명령의 `id`에 Discord 메시지 id를 넣습니다. 응답은 순서대로 오지 않을 수 있으니, 순서가 아니라 `id`로 맞춥니다.
4. **진행 중.** omp는 일하는 동안 이벤트를 계속 보냅니다. 게이트웨이는 이걸 보고 Discord에 "입력 중" 표시를 유지합니다. Discord "입력 중" API의 유지 시간은 **미검증**입니다(열린 질문 8).
5. **끝.** 같은 `id`의 `prompt_result`가 오면 omp가 그 메시지에 대한 한 턴을 마친 것입니다. `status`는 `completed`, `aborted`, `error` 중 하나입니다. 모델 쪽에서 실패해도 명령 응답은 성공으로 오고, 실패는 `status: error`로만 나타납니다. 그래서 보통은 명령 응답이 아니라 `prompt_result`를 기다립니다.
   - **예외: `agentInvoked`가 `false`일 때.** `prompt` 명령 응답의 `data.agentInvoked`가 `false`이면 `prompt_result`가 오지 않습니다(omp 자체 슬래시 명령 등). 이때는 그 명령 응답이 끝 신호입니다. 이 경우를 처리하지 않으면 게이트웨이가 영원히 기다립니다.
   - **`sessionSettled`가 `false`일 때.** `prompt_result`가 와도 백그라운드 작업이 남아 있으면 나중에 omp를 다시 깨울 수 있습니다. 그래서 "턴이 끝났다"와 "omp가 완전히 조용해졌다"는 다릅니다.
6. **답장 글 모으기.** 답장할 글은 `prompt_result`에 들어 있지 않습니다. `prompt_result`에는 상태 같은 정보만 있습니다. 답장 글은 omp가 일하는 동안 보내는 이벤트에서 모읍니다. 공식 Rust 클라이언트를 쓰면 `assistant_text`로 받을 수 있습니다.

### 3.4 일하는 중에 새 메시지가 오면

omp가 답을 만드는 중에 `prompt`를 보내려면 `streamingBehavior`를 꼭 넣어야 합니다. 빠뜨리면 명령이 실패합니다.

| 방식 | 뜻 | 언제 전달되나 |
|---|---|---|
| `steer` | 지금 하는 일에 끼어들어 방향을 바꿈 | 가능하면 즉시, 아니면 다음 도구 묶음이나 턴 경계 |
| `followUp` | 지금 일이 끝난 뒤 이어서 처리 | omp가 원래 멈출 시점 |
| `abort` | 지금 일을 멈춤 | 별도 명령. 결과는 `status: aborted`로 옴 |

`steer`와 `followUp`은 `prompt`의 `streamingBehavior` 값입니다. 같은 이름의 별도 명령(`steer`, `follow_up`)도 있습니다. 기본은 `followUp`입니다. Discord의 `/stop`은 `abort`로 보냅니다. 큐에 남은 메시지까지 돌려받고 멈추려면 `abort_and_restore_queue`를 씁니다.

### 3.5 자식 프로세스 관리 규칙

- **한 세션에 자식은 하나만.** 같은 세션 기록(세션 id)에 omp 두 개가 동시에 쓰면 오류가 나지 않습니다. 대신 두 번째 프로세스가 기록을 새 파일로 복사해 따로 씁니다. 대화 기록이 경고 없이 둘로 갈라집니다. 이걸 막는 것은 게이트웨이의 책임입니다.
- **다시 띄우기 전에 완전히 정리.** 새 자식을 띄우기 전에 옛 자식을 종료하고, 정말 끝났는지 확인합니다. 이것을 reap(끝난 프로세스를 운영체제에서 거둬 가는 일)이라고 합니다.
- **stdout은 끝까지 읽기.** 게이트웨이가 읽기를 멈추면 omp는 출력을 임시 파일에 쌓습니다. 종료가 무한정 늦어지고 디스크가 찰 수 있습니다. stdin을 닫은 뒤에도 계속 읽습니다.
- **정상 종료.** stdin을 닫으면 omp는 받은 명령을 마저 처리하고 코드 0으로 끝납니다.
- **종료 코드 1은 경고.** 세션 저장 실패가 정리 단계까지 남으면 omp는 코드 1로 끝납니다. 출력 실패도 치명적입니다. 마지막 턴이 기록되지 않았을 수 있으므로, 그 메시지를 "확인 필요"로 남기고 주인에게 알립니다.
- **남는 백그라운드 작업.** bash 도구가 백그라운드로 남긴 작업은 omp 프로세스 그룹 밖에 있어서, omp를 꺼도 같이 꺼지지 않습니다.

### 3.6 Rust 클라이언트

omp 저장소에는 공식 Rust 클라이언트 `sdk/rust/omp-rpc`가 있습니다. 메시지 형식 코드는 omp 스키마에서 자동으로 만들어지고, `ready` 기다리기, 버전 2 요청, 큰 결과 조각 합치기, `prompt_result` 기다리기를 대신 해 줍니다. 종료할 때는 프로세스 그룹 전체에 SIGTERM(종료 요청 신호)을 보내고, 1초 뒤에도 남아 있으면 SIGKILL(강제 종료 신호)을 보낸 다음 거둡니다.

주의할 점이 두 가지 있습니다.

- **crates.io의 `omp-rpc`가 아닙니다.** crates.io는 Rust 라이브러리를 내려받는 공개 저장소입니다. 저장소의 크레이트(Rust 라이브러리 단위)는 `publish = false`라서 crates.io에 없습니다. crates.io에 있는 같은 이름은 다른 것이거나 출처가 확인되지 않았습니다. 그래서 git 주소와 특정 커밋으로 고정해서 씁니다.
- **기다리는 방식입니다.** 이 클라이언트는 결과가 올 때까지 호출한 쪽을 멈춰 세웁니다. 게이트웨이는 tokio(여러 일을 동시에 처리하는 Rust 실행 환경)로 돌기 때문에, 이 클라이언트는 별도 작업 흐름(스레드)이나 `spawn_blocking`(오래 기다리는 일을 따로 돌리는 tokio 기능)에서 불러야 합니다.

## 4. 상태, 설정, 운영

### 4.1 moraebot 홈 폴더

모든 상태는 `~/.moraebot/` 한 곳에 둡니다. 백업하거나 지울 대상이 하나라서 관리가 쉽습니다.

```
~/.moraebot/
  config.toml              설정 파일 (사람이 읽고 고치는 파일)
  secrets/discord.token    Discord 봇 토큰 (주인만 읽기, 권한 600)
  inbox.sqlite             받은 메시지 보관함
  state.sqlite             대화 목록, 허용 사용자, pairing 대기
  conversations/
    <대화 폴더 이름>/
      session/             omp 세션 기록 (--session-dir로 넘김)
  logs/
    gateway.log            게이트웨이 동작 기록
    launchd.out / .err     launchd가 받아 주는 출력과 오류
```

받은 메시지 보관함(`inbox.sqlite`)을 따로 둡니다. dinobot도 보관함을 본 DB와 다른 파일로 두고, 배타 잠금(한 프로그램만 쓰게 막는 잠금)과 `synchronous=FULL`(디스크에 기록된 것을 확인한 뒤 진행)을 씁니다. omp는 원래 대화 기록을 `~/.omp/agent/sessions/`에 저장하지만, `--session-dir`로 위치를 대화 폴더로 바꿉니다.

### 4.2 설정

| 항목 | 뜻 | 기본값 | 채팅에서 바꾸기 |
|---|---|---|---|
| Discord 토큰 위치 | 토큰 파일 경로. 토큰 자체는 설정 파일에 쓰지 않음 | `secrets/discord.token` | 불가 |
| 접근 모드 | `owner_only`, `allowlist`, `pairing`, `open` | `owner_only` | 주인만 가능 |
| 주인 id | 주인의 Discord 사용자 id | 없음 (필수) | 불가 |
| 허용 사용자 | `allowlist` 모드에서 허용할 사용자 id 목록 | 빈 목록 | 주인만 가능 |
| 작업 폴더 | omp가 일하는 폴더 (`--cwd`) | 없음 (필수) | 불가 (열린 질문 11) |
| 모델 | omp가 쓸 AI 모델 | omp 기본값 | 주인만 가능 (열린 질문 12) |

규칙은 하나입니다. **봇을 빼앗을 수 있는 항목은 파일에서만 바꿉니다.**

토큰과 주인 id가 그런 항목입니다. 채팅으로 주인 id를 바꿀 수 있다면, 주인 계정이 한 번만 털려도 봇을 영영 빼앗깁니다.

접근 모드와 허용 목록은 주인이 채팅으로 바꿀 수 있습니다. 바뀐 값은 `state.sqlite`에 저장해 재시작 뒤에도 유지합니다.

주인 id가 비어 있으면 게이트웨이는 시작하지 않습니다.

### 4.3 맥에서 계속 켜 두기

launchd는 macOS가 프로그램을 자동으로 켜고, 죽으면 다시 켜 주는 기능입니다. `~/Library/LaunchAgents/`에 설정 파일(plist) 하나를 두고 `KeepAlive`와 `RunAtLoad`를 켜면, 로그인할 때 시작하고 죽으면 다시 띄웁니다. 이 키의 정확한 동작은 **미검증**입니다. 사용자 로그인 단위(LaunchAgent)로 돌려야 omp가 주인의 홈 폴더와 인증 정보를 쓸 수 있다고 보지만, 근거를 확인하지 못해 **미검증**입니다.

### 4.4 재시작했을 때

```
 launchd --(죽으면 다시 실행)--> moraebot 게이트웨이
   1. inbox.sqlite 열기
   2. processing으로 남은 메시지 처리 (아래 설명)
   3. 남아 있는 옛 omp 자식 프로세스 종료하고 거두기
   4. Discord Gateway 다시 연결
   5. 대화가 오면 omp를 다시 띄우고 open_session으로 이전 대화 이어 받기
   6. 대기(queued) 메시지를 순서대로 처리 (같은 메시지 id는 한 번만)
```

- **2번**: dinobot은 `processing`을 `queued`로 되돌려 다시 처리합니다. 하지만 moraebot은 yolo라서 같은 명령이 두 번 실행되면 위험합니다. 그래서 자동으로 다시 보내지 않고, 주인에게 "재시작으로 중단되었습니다. 다시 보낼까요?"라고 묻기를 권장합니다(열린 질문 3).
- **3번**: 옛 자식이 세션을 쥐고 있으면 새 자식의 기록이 조용히 갈라집니다(3.5절). 그래서 옛 자식을 반드시 먼저 끝냅니다.
- **끊긴 작업**: 재시작하면 omp가 실행하던 도구 작업은 이어지지 않습니다. 대화 기록은 남지만, 하던 명령은 끊깁니다.

## 5. 단계별 로드맵

한 번에 하나씩, 앞 단계가 실제로 동작한 뒤에 다음으로 갑니다(D8). 단계 하나는 처음부터 끝까지 실제로 동작하는 작은 기능 단위입니다.

1. **1단계: 왕복 한 번.** Discord 메시지 -> 게이트웨이 -> inbox 저장 -> omp 대화 하나 -> `prompt_result` -> Discord 답장. 주인 한 명(`owner_only`), 대화 하나, 설정 파일만 씁니다. launchd 등록과 재시작 처리(4.4절)를 포함합니다. 토큰은 권한 600(주인만 읽을 수 있음) 파일에서만 읽고, 로그와 자식 프로세스 환경변수에 넣지 않습니다. 메시지 본문은 셸(명령 실행기)을 거치지 않고 JSON 한 줄로만 omp에 보냅니다. 대화는 Discord 스레드 하나입니다. 가짜 Discord와 가짜 omp로 자동 테스트하고, 실제 Discord에서 한 번 수동으로 확인합니다.
2. **2단계: 접근 모드.** `owner_only`, `allowlist`, `pairing`, `open`과 주인의 채팅 명령. 받을 때와 보내기 직전에 두 번 검사합니다.
3. **3단계: 여러 대화.** Discord 스레드마다 omp 하나, 대화별 큐. 바쁠 때 `followUp`과 `steer`, `/stop`과 `/new`.
4. **4단계: 일 나누기(나중).** dori-mode는 예전에 쓰던, 큰 일을 여러 에이전트에게 나눠 맡기던 방식입니다. 그 아이디어를 옮깁니다.
   - 주인 채널에 "리드 대화" 하나를 둡니다.
   - 일 하나씩을 맡는 추가 대화(레인)를 게이트웨이가 띄웁니다.
   - 레인이 "끝났다"고 하면, 게이트웨이가 완료 조건을 직접 확인한 뒤 주인에게 보고합니다.
   - 오래 소식이 없으면 재촉하는 감시는 게이트웨이 안의 타이머로 합니다.
   - dori-mode와 달리 화면을 읽지 않고 omp RPC의 결과를 씁니다.

   자세한 설계는 이 단계에 들어갈 때 합니다.

## 6. 열린 질문

각 질문에 권장 기본값을 적었습니다. 따로 정하지 않으면 권장 기본값으로 갑니다.

1. **홈 폴더 위치.** 권장: `~/.moraebot/`.
2. **대화 단위를 스레드로 강제할까?** 권장: 채널에 바로 쓴 메시지는 봇이 스레드를 자동으로 만들고, 그 스레드를 대화 키로 씁니다.
3. **재시작이나 이상 종료 뒤 끊긴 메시지는?** `processing`으로 남은 메시지, omp가 종료 코드 1로 끝난 메시지가 여기에 해당합니다. 권장: 자동으로 다시 보내지 않고 "확인 필요"로 표시한 뒤 주인에게 다시 보낼지 묻습니다. yolo라서 같은 명령이 두 번 실행되면 위험합니다.
4. **처리 중에 온 새 메시지는 `followUp`인가 `steer`인가?** 권장: `followUp`. 끼어들기(`steer`)는 사용자가 명시할 때만 씁니다.
5. **게이트웨이가 꺼져 있던 동안 온 메시지를 찾아 읽을까?** 권장: 처음에는 하지 않습니다. 켜질 때 재시작 알림만 보내고, 놓친 메시지는 다시 보내 달라고 안내합니다.
6. **주인 id는 어디에 적을까?** 권장: 설정 파일에 Discord 사용자 id 하나. 비어 있으면 게이트웨이가 시작하지 않습니다.
7. **어떤 omp-rpc 커밋에 고정할까?** 권장: 설치된 omp 18.20.2와 같은 태그의 커밋. omp를 올릴 때 함께 올립니다.
8. **진행 상황을 얼마나 보여 줄까?** 권장: 처음에는 "입력 중" 표시만. 도구 실행 요약은 다음 단계에서.
9. **쉬는 대화의 omp는 언제 끌까?** 권장: 30분 동안 메시지가 없으면 stdin을 닫아 정상 종료합니다. 다음 메시지가 오면 다시 띄웁니다.
10. **공식 Rust 클라이언트를 쓸까, 직접 짤까?** 권장: 공식 클라이언트를 전용 스레드에서 씁니다. 문제가 생기면 tokio용 얇은 클라이언트를 직접 짭니다.
11. **작업 폴더를 채팅에서 바꿀 수 있게 할까?** 권장: 3단계까지는 설정 파일에서만. 바꿀 수 있으면 에이전트가 다른 폴더를 마음대로 건드리게 됩니다.
12. **모델을 채팅에서 바꿀까?** 권장: 주인만, 대화 단위로. 2단계 이후.
13. **로그 보관 기간.** 권장: 하루 단위로 나누고 14일 보관.
14. **`open` 모드를 아예 허용할까?** 권장: 설정에는 두되, 켤 때 위험 경고와 주인 재확인을 필수로 합니다.

## 7. 근거 (개발자용)

코드 위치는 각 프로젝트의 공개 저장소 기준입니다. `wavesN/...` 표기는 공개 저장소에 포함하지 않는 로컬 조사 메모입니다.

**Discord와 게이트웨이**
- Discord Gateway와 `message_content` intent(hermes): `ext/hermes-agent/repo/plugins/platforms/discord/adapter.py:1423-1430`, `:1488` (waves4/hermes-agent.md:25)
- 게이트웨이 한 프로세스가 항상 켜짐(hermes): waves4/hermes-agent.md:9, :33
- dinobot Discord는 interactions 웹훅만, 일반 메시지 안 받음: `discord.ts:74` (waves4/dinobot.md:48)
- serenity 0.12.5, twilight-gateway 0.18.0 버전: waves5/verify.md:129. 재연결과 resume 처리 주장: waves5/rust.md:43 (범위 미검증, rust.md:86)
- Discord Gateway는 영구 재전송 없음, 메시지 id로 중복 제거: waves5/rust.md:92
- rusqlite 0.43.0, `Transaction`, `TransactionBehavior::Immediate`: waves5/verify.md:79, :119. 단일 쓰기 권장: waves5/rust.md:54

**저장, 순서, 제어 명령**
- 대화 키 구성(hermes): `gateway/session.py:722-766` (waves4/hermes-agent.md:69)
- 메시지, 중복 키, 커서를 한 트랜잭션에: `messaging.ts:623-651`. 중복 방지 검사: `messaging.ts:630`, UNIQUE 제약: `messaging.ts:684`. 재시작 때 `processing`을 `queued`로: `messaging.ts:713` (waves4/verify.md:52, :54; waves4/dinobot.md:59-61, :104-107)
- dinobot 메시지 상태 6개(queued, processing, ready, sending, done, dead): `messaging.ts:144` (waves4/dinobot.md:78)
- inbox 별도 파일, 배타 잠금, `synchronous=FULL`: waves4/dinobot.md:78, waves4/verify.md:52
- dinobot의 Discord와 Slack 전역 큐(동시 1개, Telegram은 토픽별)와 head-of-line blocking: `messaging.ts:104`, `:240` (waves4/dinobot.md:86, :133)
- `/new`가 옛 대화를 `<키>#n`으로 보관: `threads.ts:431-478` (waves4/dinobot.md:83)
- `/stop`, `/new` 큐 우회(hermes): `gateway/platforms/base.py:3946-3950` (waves4/verify.md:11)
- dinobot은 launchd 아래에서 자체 재시작을 하지 않음: waves4/dinobot.md:111

**접근 제어**
- 네 모드는 moraebot 설계. hermes에서 가져온 것은 기본 거부, 허용 목록, pairing. 기본 거부와 allowlist 순서: `gateway/authz_mixin.py:858-862, 905-935`. pairing 규칙: `gateway/pairing.py:1-32`. 그룹은 pairing 안 함: `run_inbound.py:228-247` (waves4/verify.md:12; waves4/hermes-agent.md:58, :75, :97)
- 받을 때와 보낼 때 두 번 검사(dinobot): `authorizeStored`, `messaging.ts:379` (waves4/verify.md:53)

**omp RPC**
- 한 줄에 JSON 하나(JSONL), JSON-RPC 2.0 아님: `oh-my-pi/docs/rpc.md:1-9` (waves5/verify.md §1)
- `ready` 내용: `rpc.md:41-52`. 버전 2와 `rpc_chunk`: `rpc.md:54-75`
- stdin을 닫으면 처리 후 코드 0, 저장 실패 시 코드 1: `rpc.md:34`, `:1679`
- 출력이 밀리면 임시 파일, stdout 계속 읽기: `rpc.md:79`, `:81`
- `id` 응답과 순서 무관: `rpc.md:111-123`, `:233-238`(bash 명령 예시). `prompt`는 앞선 명령 뒤에 시작: `rpc.md:343`
- `prompt_result`와 status, 실패 턴도 성공 응답: `rpc.md:339-369`. `agentInvoked:false`면 `prompt_result` 없음: `rpc.md:355-357`. `sessionSettled`: `rpc.md:358`, `:365`, `:373`. 답장 글은 이벤트 또는 `assistant_text`: `sdk/rust/omp-rpc/README.md:29`
- `open_session`과 `resumed`, `--continue`와 같음: `rpc.md:381-405` (waves5/omp-rpc.md:100-107, waves5/verify.md:23, :101)
- 스트리밍 중 `streamingBehavior` 필수, steer와 followUp 전달 시점: `rpc.md:1264-1275`. steering의 `interruptMode` 설명(abort와 다름): `rpc.md:1277-1298`. abort 결과 `status: "aborted"`: `rpc.md:363`. `abort_and_restore_queue`: `rpc.md:447-461`
- 같은 세션(세션 id 기준 임대)에 두 프로세스면 조용히 기록 분기: `docs/session.md:558-559` (waves5/verify.md:40, :137)
- 기본 승인 모드 yolo, 제한 폴더 예외: `docs/approval-mode.md:25`, `:50`, `:63`
- 실행 명령 예시와 `--cwd`, `--approval-mode`: waves5/omp-rpc.md:137
- 세션 기본 위치 `~/.omp/agent/sessions/`: waves5/omp-rpc.md:102. 한 프로세스에 세션 하나는 추론: waves5/omp-rpc.md:101
- Rust 클라이언트 구조, 블로킹 전송, v2 자동 협상: `sdk/rust/omp-rpc/README.md:5-15`, `:37`. 쓰기 스레드와 종료 절차: `README.md:43-59`. bash 작업 생존: `README.md:61`. `prompt_and_wait`: `README.md:67`. README 예시는 `--no-session`: `README.md:22`
- `publish = false`와 crates.io 이름 충돌: waves5/verify.md R4. `spawn_blocking` 필요: waves5/verify.md R5

**dori-mode (4단계용)**
- 리드, 레인, 완료 검증, 모니터, 부팅 점검 흐름: waves3/verify.md:105-116. 상태 값과 자동 닫힘: waves3/verify.md:31, :85, :88. 화면 읽기 방식: waves3/verify.md:89

**미검증**
- launchd plist 키(`KeepAlive`, `RunAtLoad`)의 정확한 동작
- LaunchAgent로 돌려야 omp가 홈 폴더와 인증 정보를 쓴다는 주장
- Message Content intent가 꺼졌을 때 DM과 멘션 메시지의 본문 동작
- Discord 라이브러리의 재연결과 resume 범위, 재연결 후 놓친 메시지 처리
- Discord "입력 중" 표시 API와 유지 시간
