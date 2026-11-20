//! The memory observer against a real store, a real memory writer, a loopback fake jev and a
//! manual clock: each test queues jobs the way the dispatcher and the lane manager do, then
//! calls `Observer::run_once`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use moraebot::adapter::{InboundEvent, MessengerAdapter};
use moraebot::config::{
    AccessMode, Config, DiscordConfig, JevConfig, MemoryConfig, OmpConfig, Platform,
};
use moraebot::gateway::{Gateway, STORE_FILE};
use moraebot::jev::{E_JEV_REQUEST, E_JEV_RESPONSE, Jev, Now};
use moraebot::lead::{self, LaneResult};
use moraebot::memory::{
    self, Bucket, MemoryHandle, MemoryStore, NewEntry, Op, Prov, Redactor, Source,
};
use moraebot::observer::{
    E_OBSERVER_JOB, E_OBSERVER_PANIC, E_SUBAGENT_LAYOUT, M_ARTIFACT_CHANGED, M_JEV_OFF, M_JOB_DONE,
    Observer, Pass, W_SUBAGENT_SKIPPED,
};
use moraebot::omp;
use moraebot::store::{
    Admitted, Backlog, JobPayload, LaneStart, LaneState, Store, ToolSource, ToolUse, TurnTrace,
};
use moraebot::testing::{Call, FakeAdapter, FakeJev};
use serde_json::{Value, json};
use tokio::sync::oneshot;

const DAY: i64 = 24 * 60 * 60;
const KEY: &str = "discord:th:1";
const OWNER: &str = "111";
const CHANNEL: &str = "333";
const DM_CHANNEL: &str = "444";
const BOUND: Duration = Duration::from_secs(30);
const OMP_VERSION: &str = "18.22.2";

fn test_config(root: &Path, omp_path: PathBuf, jev_url: &str) -> Config {
    let home = root.join("home");
    let work_dir = root.join("work");
    std::fs::create_dir_all(home.join("secrets")).unwrap();
    std::fs::create_dir_all(&work_dir).unwrap();
    let token_file = home.join("secrets").join("discord.token");
    std::fs::write(&token_file, "TOKEN_SENTINEL_51c2").unwrap();
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut memory = MemoryConfig::default();
    memory.jev.base_url = jev_url.to_owned();
    memory.jev.enabled = true;
    memory.jev.set_env_key(Some("JEV_KEY_SENTINEL_0d17".into()));
    Config {
        messenger: Platform::Discord,
        work_dir,
        omp_path,
        home,
        access_mode: AccessMode::OwnerOnly,
        discord: Some(DiscordConfig {
            owner_id: OWNER.parse().unwrap(),
            channel_id: CHANNEL.parse().unwrap(),
            token_file,
            access: Default::default(),
        }),
        slack: None,
        omp: OmpConfig::default(),
        memory,
    }
}

/// Answers that save nothing.
static NOTHING: std::sync::LazyLock<Value> = std::sync::LazyLock::new(|| {
    json!({
        "save": {"type": "noul", "noul": 0.0},
        "bucket": {"type": "choice", "choice": "none"},
        "span": {"type": "choice", "choice": "none"},
    })
});

/// Answers that save sentence `span` into `bucket`.
fn save(bucket: &str, span: &str) -> Value {
    json!({
        "save": {"type": "noul", "noul": 0.9},
        "bucket": {"type": "choice", "choice": bucket},
        "span": {"type": "choice", "choice": span},
    })
}

fn trace(task: bool, settled: bool) -> TurnTrace {
    TurnTrace {
        tools: vec![ToolUse::new("web_search", ToolSource::Web)],
        task,
        settled,
    }
}

struct Fixture {
    root: tempfile::TempDir,
    config: Config,
    store: Arc<Mutex<Store>>,
    memory: MemoryHandle,
    jev: FakeJev,
    /// The manual clock's start: the real time, since the store dates some jobs itself.
    t0: i64,
    clock: Arc<AtomicI64>,
    observer: Observer,
}

impl Fixture {
    async fn new() -> Self {
        Self::with(|_| {}).await
    }

    async fn with(adjust: impl FnOnce(&mut Config)) -> Self {
        let root = tempfile::tempdir().unwrap();
        let jev = FakeJev::start().await;
        let mut config = test_config(root.path(), PathBuf::from("omp"), &jev.base_url());
        adjust(&mut config);
        let store = Arc::new(Mutex::new(
            Store::open(&config.home.join(STORE_FILE)).unwrap(),
        ));
        let memory = MemoryHandle::spawn(
            MemoryStore::open(&config.home, Redactor::default(), memory::system_clock()).unwrap(),
        )
        .unwrap();
        let t0 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let clock = Arc::new(AtomicI64::new(t0));
        let observer = observer(&config, &store, &memory, &clock);
        Self {
            root,
            config,
            store,
            memory,
            jev,
            t0,
            clock,
            observer,
        }
    }

    fn set_clock(&self, at: i64) {
        self.clock.store(at, Ordering::SeqCst);
    }

    fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap()
    }

    fn admit(&self, mid: &str, text: &str) -> i64 {
        match self
            .store()
            .admit("discord", mid, KEY, CHANNEL, OWNER, text)
            .unwrap()
        {
            Admitted::New(id) => id,
            Admitted::Duplicate => panic!("new row"),
        }
    }

    /// A lead turn on `main` (with `steered`) that replied `reply`.
    fn reply(&self, main: i64, steered: Option<i64>, reply: &str, trace: &TurnTrace) {
        self.store()
            .finish_group(main, reply, None, steered, trace)
            .unwrap();
    }

    /// Starts lane `call` on `repo` and stores its first result `words`; returns the lane id
    /// and the result row.
    fn lane(&self, call: &str, repo: Option<&str>, words: &str) -> (i64, i64) {
        let mut store = self.store();
        let LaneStart::New(lane) = store.start_lane(call, KEY, "task", None, repo, 8).unwrap()
        else {
            panic!("new lane");
        };
        let text = lead::lane_result(KEY, lane, &LaneResult::Text(words));
        assert!(
            store
                .finish_lane(lane, 1, &text, LaneState::Idle, None)
                .unwrap()
        );
        let row = store
            .pending_memory_jobs(100)
            .unwrap()
            .jobs
            .into_iter()
            .find_map(|job| match job.payload {
                JobPayload::Lane(job) if job.lane == lane => Some(job.row),
                _ => None,
            })
            .expect("the lane result's job");
        (lane, row)
    }

    fn entries(&self, bucket: &Bucket) -> Vec<memory::Entry> {
        self.memory.entries(bucket).unwrap()
    }

    fn backlog(&self) -> Backlog {
        self.store().memory_backlog().unwrap()
    }

    fn commits(&self) -> u32 {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(self.config.memory_dir())
            .args(["rev-list", "--count", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap_or(0)
    }

    /// Bodies of every jev request so far.
    fn bodies(&self) -> Vec<Value> {
        self.jev.requests().into_iter().map(|r| r.body).collect()
    }

    /// Writes one subagent output under `root/<slug>/<stem>/` the way omp 18.22.2 does
    /// (`artifactsDir` is the parent session file without `.jsonl`, workpool.ts:418; the
    /// output is `<artifactsDir>/<id>.md` next to the subagent's own `<id>.jsonl`,
    /// executor.ts:3158 and :4663). Returns the `.md` path.
    fn subagent_file(root: &Path, stem: &str, task: &str, text: &str) -> PathBuf {
        let slug = root.join("--work--");
        let folder = slug.join(stem);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(slug.join(format!("{stem}.jsonl")), "{}\n").unwrap();
        std::fs::write(folder.join(format!("{task}.jsonl")), "{}\n").unwrap();
        std::fs::write(folder.join(format!("{task}.json")), "{}\n").unwrap();
        let md = folder.join(format!("{task}.md"));
        std::fs::write(&md, text).unwrap();
        md
    }

    /// Runs `sql` on the database while the store is closed (the store holds the database's
    /// lock), then opens it again behind the same handle, so the observer keeps working on it.
    fn with_store_closed(&self, sql: &str) {
        let path = self.config.home.join(STORE_FILE);
        let spare = Store::open(&self.root.path().join("spare.sqlite")).unwrap();
        let mut store = self.store();
        drop(std::mem::replace(&mut *store, spare));
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute_batch(sql)
            .unwrap();
        drop(std::mem::replace(&mut *store, Store::open(&path).unwrap()));
    }

    fn lead_root(&self) -> PathBuf {
        omp::lead_session_dir(&self.config)
    }

    /// Drops the observer and the store and opens the database directly.
    fn into_db(self) -> (tempfile::TempDir, rusqlite::Connection) {
        let path = self.config.home.join(STORE_FILE);
        drop(self.observer);
        drop(self.store);
        (self.root, rusqlite::Connection::open(path).unwrap())
    }
}

/// Makes every update of a job's state fail, as a full disk or a busy database would.
const FAIL_SETTLE: &str = "CREATE TRIGGER fail_settle BEFORE UPDATE OF state ON memory_jobs \
                           BEGIN SELECT RAISE(ABORT, 'disk full'); END";

/// Runs one pass on a thread of its own and waits up to [`BOUND`] for it: a pass that never
/// yields cannot be bounded by a timer on the test's own runtime. `None` when it did not end
/// in time (the thread is left behind).
fn run_once_on_a_thread(observer: &Arc<Observer>) -> Option<Pass> {
    let observer = Arc::clone(observer);
    let (sent, ended) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _ = sent.send(runtime.block_on(observer.run_once()));
    });
    ended.recv_timeout(BOUND).ok()
}

fn observer(
    config: &Config,
    store: &Arc<Mutex<Store>>,
    memory: &MemoryHandle,
    clock: &Arc<AtomicI64>,
) -> Observer {
    let time = Arc::clone(clock);
    let now: Now = Arc::new(move || time.load(Ordering::SeqCst));
    let jev = Arc::new(Jev::new(config.memory.jev.clone(), Arc::clone(&now)));
    Observer::new(
        config,
        Arc::clone(store),
        memory.clone(),
        jev,
        Redactor::default(),
        now,
    )
    .with_omp_version(OMP_VERSION)
}

#[tokio::test]
async fn observer_saves_an_owner_sentence_word_for_word() {
    let fx = Fixture::new().await;
    let row = fx.admit("m1", "이건 그냥 잡담이야.  앞으로 답은 항상 한국어로 해줘.");
    fx.reply(row, None, "알겠어요", &trace(false, true));
    fx.jev.answer(save("core", "s1"));
    assert_eq!(fx.observer.run_once().await, Pass::Idle);

    let saved = fx.entries(&Bucket::Owner);
    assert_eq!(saved.len(), 1, "{saved:?}");
    assert_eq!(saved[0].text, "앞으로 답은 항상 한국어로 해줘.");
    assert_eq!(saved[0].prov, Prov::Owner);
    assert_eq!(
        saved[0].src.as_deref(),
        Some(format!("row={row} s=1").as_str())
    );
    let state = &fx.bodies()[0]["state"];
    assert_eq!(state["kind"], "owner_message");
    assert_eq!(
        state["sentences"],
        json!([
            "s0: 이건 그냥 잡담이야.",
            "s1: 앞으로 답은 항상 한국어로 해줘."
        ])
    );
    assert_eq!(fx.backlog(), Backlog::default());
}

/// Owner decision on #39: a core or boundary save by the observer is named under the next
/// reply in that conversation, and the pending note survives a restart.
#[tokio::test]
async fn observer_core_save_is_shown_under_the_next_reply_even_after_a_restart() {
    let fx = Fixture::new().await;
    let row = fx.admit("m1", "앞으로 PR은 항상 영어로 올려.");
    fx.reply(row, None, "네", &trace(false, true));
    fx.jev.answer(save("boundary", "s0"));
    fx.observer.run_once().await;
    let id = fx.entries(&Bucket::Boundaries)[0].id.clone();
    let expected = format!(
        "[memory] saved {id} to core/boundaries: \"앞으로 PR은 항상 영어로 올려.\" (say `forget {id}` if that was not meant)"
    );
    assert_eq!(
        fx.store().pending_memory_notices(KEY).unwrap(),
        std::slice::from_ref(&expected)
    );

    // A restart: the observer and the store go away, the database stays.
    let path = fx.config.home.join(STORE_FILE);
    let Fixture {
        root: _root,
        store,
        observer,
        ..
    } = fx;
    drop(observer);
    drop(store);
    let mut store = Store::open(&path).unwrap();
    let Admitted::New(next) = store
        .admit("discord", "m2", KEY, CHANNEL, OWNER, "다음 질문")
        .unwrap()
    else {
        panic!("new row");
    };
    store
        .finish_group(next, "답이에요", None, None, &TurnTrace::default())
        .unwrap();
    assert_eq!(
        store.get(next).unwrap().unwrap().reply.unwrap(),
        format!("답이에요\n\n{expected}")
    );
    assert!(store.pending_memory_notices(KEY).unwrap().is_empty());
}

/// Owner decision 5: jev's `core` or `boundary` answer alone does not put a sentence in core;
/// it must say "always" (or "never" for a boundary), else it stays with the conversation.
#[tokio::test]
async fn observer_keeps_a_sentence_without_always_out_of_core() {
    let fx = Fixture::new().await;
    let row = fx.admit("m1", "릴리스는 금요일에 하자.  Never force-push to main.");
    fx.reply(row, None, "네", &trace(false, true));
    fx.jev.answer(json!({
        "save": {"type": "noul", "noul": 0.9},
        "bucket": {"type": "choice", "choice": "core"},
        "span": {"type": "choice", "choice": "s0"},
    }));
    assert_eq!(fx.observer.run_once().await, Pass::Idle);
    assert!(fx.entries(&Bucket::Owner).is_empty());
    let thread = fx.entries(&Bucket::Thread(KEY.into()));
    assert_eq!(thread.len(), 1, "{thread:?}");
    assert_eq!(thread[0].text, "릴리스는 금요일에 하자.");
    assert!(fx.store().pending_memory_notices(KEY).unwrap().is_empty());

    // A "never" sentence may be a boundary.
    let next = fx.admit("m2", "Never force-push to main.");
    fx.reply(next, None, "네", &trace(false, true));
    fx.jev.answer(save("boundary", "s0"));
    assert_eq!(fx.observer.run_once().await, Pass::Idle);
    let boundaries = fx.entries(&Bucket::Boundaries);
    assert_eq!(boundaries.len(), 1, "{boundaries:?}");
    assert_eq!(boundaries[0].text, "Never force-push to main.");
}

#[tokio::test]
async fn observer_never_makes_a_lane_sentence_the_owners() {
    let fx = Fixture::new().await;
    let (lane, _row) = fx.lane("call-1", None, "앞으로 답은 항상 한국어로 해줘.");
    fx.jev.answer(save("thread", "s0"));
    fx.observer.run_once().await;
    assert!(fx.entries(&Bucket::Owner).is_empty());
    let saved = fx.entries(&Bucket::Thread(KEY.into()));
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].prov, Prov::Lane);
    assert_eq!(
        saved[0].src.as_deref(),
        Some(format!("lane={lane} seq=1").as_str())
    );
    assert_eq!(fx.bodies()[0]["state"]["kind"], "lane_result");
    assert!(fx.store().pending_memory_notices(KEY).unwrap().is_empty());
}

#[tokio::test]
async fn observer_gives_a_steered_owner_row_owner_and_the_lane_row_lane() {
    let fx = Fixture::new().await;
    let (lane, lane_row) = fx.lane("call-1", None, "CI는 PR마다 cargo test를 돌린다.");
    let owner_row = fx.admit("m1", "릴리스는 금요일에 하자.");
    fx.reply(lane_row, Some(owner_row), "정리했어요", &trace(false, true));
    // The lane job (queued first), then the group job, which judges only the owner row.
    fx.jev.answer(save("thread", "s0"));
    fx.jev.answer(save("thread", "s0"));
    assert_eq!(fx.observer.run_once().await, Pass::Idle);
    assert_eq!(fx.jev.requests().len(), 2);

    let saved = fx.entries(&Bucket::Thread(KEY.into()));
    let by_text = |text: &str| saved.iter().find(|e| e.text == text).expect(text).clone();
    let lane_entry = by_text("CI는 PR마다 cargo test를 돌린다.");
    assert_eq!(lane_entry.prov, Prov::Lane);
    assert_eq!(
        lane_entry.src.as_deref(),
        Some(format!("lane={lane} seq=1").as_str())
    );
    let owner_entry = by_text("릴리스는 금요일에 하자.");
    assert_eq!(owner_entry.prov, Prov::Owner);
    assert_eq!(
        owner_entry.src.as_deref(),
        Some(format!("row={owner_row} s=0").as_str())
    );
}

#[tokio::test]
async fn observer_judges_a_lane_result_once() {
    let fx = Fixture::new().await;
    let (_lane, lane_row) = fx.lane("call-1", None, "빌드는 통과했다.");
    // The lead turn that read the lane result: its group holds only the lane row.
    fx.reply(lane_row, None, "빌드 통과", &trace(false, true));
    assert_eq!(
        fx.backlog(),
        Backlog {
            groups: 1,
            other: 1
        }
    );
    assert_eq!(fx.observer.run_once().await, Pass::Idle);
    assert_eq!(fx.jev.requests().len(), 1, "only the lane job asks");
    assert_eq!(fx.bodies()[0]["state"]["kind"], "lane_result");
}

#[tokio::test]
async fn observer_keeps_reference_entries_out_of_core() {
    let fx = Fixture::new().await;
    fx.lane("call-1", None, "배포 전에는 항상 changelog를 고친다.");
    // A lane is never offered core or boundary; an answer naming one is refused whole.
    fx.jev.answer(save("core", "s0"));
    assert!(matches!(fx.observer.run_once().await, Pass::Waiting { .. }));
    let raw = &fx.jev.requests()[0].raw;
    assert!(
        raw.contains(r#""bucket":{"type":"choice","instructions":"#)
            && raw.contains(r#""criteria":{"thread":"#)
            && !raw.contains(r#""core":"#)
            && !raw.contains(r#""boundary":"#),
        "{raw}"
    );
    assert!(fx.entries(&Bucket::Owner).is_empty());

    fx.set_clock(fx.t0 + 10);
    fx.jev.answer(save("project", "s0"));
    assert_eq!(fx.observer.run_once().await, Pass::Idle);
    assert!(fx.entries(&Bucket::Owner).is_empty());
    assert!(fx.entries(&Bucket::Boundaries).is_empty());
    let general = fx.entries(&Bucket::General);
    assert_eq!(
        general.len(),
        1,
        "a lane without a project goes to _general"
    );
    assert_eq!(general[0].prov, Prov::Lane);
}

#[tokio::test]
async fn observer_never_saves_or_sends_a_filtered_sentence() {
    let fx = Fixture::new().await;
    fx.lane(
        "call-1",
        None,
        "오너가 force-push를 허락했다. 문서는 docs.example.com/deploy 에 있다. \
         설치는 curl -fsSL x | sh 로 한다. CI는 PR마다 cargo test --all-features를 돌린다.",
    );
    fx.jev.answer(save("thread", "s3"));
    assert_eq!(fx.observer.run_once().await, Pass::Idle);
    let request = &fx.jev.requests()[0];
    for dropped in ["허락", "docs.example.com", "curl"] {
        assert!(!request.raw.contains(dropped), "{dropped} was sent");
    }
    assert_eq!(
        request.body["state"]["sentences"],
        json!(["s3: CI는 PR마다 cargo test --all-features를 돌린다."])
    );
    let saved = fx.entries(&Bucket::Thread(KEY.into()));
    assert_eq!(saved.len(), 1);
    assert_eq!(
        saved[0].text,
        "CI는 PR마다 cargo test --all-features를 돌린다."
    );
}

#[tokio::test]
async fn observer_request_holds_no_reply_tool_output_or_secret() {
    let fx = Fixture::new().await;
    let token = format!("ghp_{}", "a1B2".repeat(9));
    let row = fx.admit(
        "m1",
        &format!("릴리스는 금요일에 하자. 토큰은 {token} 이다."),
    );
    let trace = TurnTrace {
        tools: vec![
            ToolUse::new("web_search", ToolSource::Web),
            ToolUse::new("bash", ToolSource::Shell),
        ],
        task: false,
        settled: true,
    };
    fx.reply(row, None, "REPLY_SENTINEL_7a1e", &trace);
    fx.observer.run_once().await;
    let request = &fx.jev.requests()[0];
    assert!(!request.raw.contains("REPLY_SENTINEL_7a1e"));
    assert!(!request.raw.contains(&token));
    assert!(!request.raw.contains("JEV_KEY_SENTINEL_0d17"));
    assert_eq!(
        request.body["state"],
        json!({
            "kind": "owner_message",
            "sentences": ["s0: 릴리스는 금요일에 하자."],
            "tools_used": [
                {"name": "web_search", "source": "web"},
                {"name": "bash", "source": "shell"}
            ],
        }),
        "only names and sources of tools, never their arguments or output"
    );
    assert_eq!(
        request.authorization.as_deref(),
        Some("Bearer JEV_KEY_SENTINEL_0d17")
    );
}

#[tokio::test]
async fn observer_keeps_owner_jobs_while_jev_is_down_and_drops_old_reference_jobs() {
    let fx = Fixture::new().await;
    let row = fx.admit("m1", "앞으로 답은 짧게 해줘.");
    fx.reply(row, None, "네", &trace(false, true));
    fx.lane("call-1", None, "테스트는 모두 통과했다.");
    fx.jev.reply(529, &[], json!({"error": "overloaded"}));
    assert!(matches!(
        fx.observer.run_once().await,
        Pass::Waiting { until: Some(_) }
    ));
    assert_eq!(
        fx.backlog(),
        Backlog {
            groups: 1,
            other: 1
        }
    );

    fx.set_clock(fx.t0 + 8 * DAY);
    fx.jev.reply(529, &[], json!({"error": "overloaded"}));
    fx.observer.run_once().await;
    assert_eq!(
        fx.backlog(),
        Backlog {
            groups: 1,
            other: 0
        },
        "the lane job expired, the owner's job did not"
    );

    fx.set_clock(fx.t0 + 9 * DAY);
    fx.jev.answer(save("thread", "s0"));
    assert_eq!(fx.observer.run_once().await, Pass::Idle);
    assert_eq!(fx.backlog(), Backlog::default());
    assert_eq!(fx.entries(&Bucket::Thread(KEY.into())).len(), 1);
}

#[tokio::test]
async fn observer_stops_at_the_first_job_jev_refuses_and_goes_on_later() {
    let fx = Fixture::new().await;
    for (mid, text) in [("m1", "첫째 규칙은 이렇다."), ("m2", "둘째 규칙은 저렇다.")]
    {
        let row = fx.admit(mid, text);
        fx.reply(row, None, "네", &trace(false, true));
    }
    let progress = fx.observer.progress();
    fx.jev.answer(save("thread", "s0"));
    fx.jev.reply(429, &[], json!({"error": "rate limited"}));
    assert_eq!(
        fx.observer.run_once().await,
        Pass::Waiting {
            until: Some(fx.t0 + 2)
        }
    );
    assert_eq!(*progress.borrow(), 1, "the first job settled");
    assert_eq!(fx.backlog().groups, 1);

    fx.set_clock(fx.t0 + 1);
    assert!(matches!(fx.observer.run_once().await, Pass::Waiting { .. }));
    assert_eq!(
        fx.jev.requests().len(),
        2,
        "nothing is sent before the gate opens"
    );

    fx.set_clock(fx.t0 + 2);
    assert_eq!(fx.observer.run_once().await, Pass::Idle);
    assert_eq!(fx.jev.requests().len(), 3);
    assert_eq!(*progress.borrow(), 2);
}

#[tokio::test]
async fn observer_processing_a_job_twice_adds_nothing() {
    let fx = Fixture::new().await;
    let row = fx.admit("m1", "앞으로 답은 항상 한국어로 해줘.");
    fx.reply(row, None, "네", &trace(false, true));
    // A pass that wrote this entry and was cut before it settled the job.
    let earlier = NewEntry {
        text: "앞으로 답은 항상 한국어로 해줘.".into(),
        prov: Prov::Owner,
        bucket: Bucket::Owner,
        src: Source::row_sentence(row, 0),
        expires: None,
        why: Some("observed".into()),
    };
    fx.memory
        .apply_batch(vec![Op::AddUnique(earlier)])
        .await
        .unwrap()
        .into_iter()
        .for_each(|r| assert!(r.is_ok()));
    let commits = fx.commits();
    fx.jev.answer(save("core", "s0"));
    assert_eq!(fx.observer.run_once().await, Pass::Idle);
    assert_eq!(fx.entries(&Bucket::Owner).len(), 1);
    assert_eq!(fx.commits(), commits, "nothing new to commit");
    // The cut pass never stored its notice (it is stored with the settle), so this one does,
    // once (moraedang on #46).
    let id = fx.entries(&Bucket::Owner)[0].id.clone();
    assert_eq!(
        fx.store().pending_memory_notices(KEY).unwrap(),
        [moraebot::memory_tools::core_note(
            &id,
            &Bucket::Owner.label(),
            "앞으로 답은 항상 한국어로 해줘."
        )]
    );
    let (_root, db) = fx.into_db();
    let detail: String = db
        .query_row(
            "SELECT detail FROM memory_log WHERE code = ?1",
            [M_JOB_DONE],
            |r| r.get(0),
        )
        .unwrap();
    assert!(detail.contains("saved=0 skipped=1"), "{detail}");
}

/// A failed lead turn keeps the line of its own core save on its main row (#43), and its
/// partial job still reaches the observer. When jev picks the sentence the lead already
/// saved, the observer adds no entry and no notice: the line is shown once, after the
/// failure notice, and the next reply carries nothing more.
#[tokio::test]
async fn observer_adds_no_second_line_for_a_core_save_of_a_failed_lead_turn() {
    let fx = Fixture::new().await;
    let text = "앞으로 답은 항상 한국어로 해줘.";
    let row = fx.admit("m1", text);
    // The lead's `memory_write` of the owner's sentence, saved before the turn failed.
    let lead_save = NewEntry {
        text: text.into(),
        prov: Prov::Owner,
        bucket: Bucket::Owner,
        src: Source::row_sentence(row, 0),
        expires: None,
        why: Some("qa".into()),
    };
    fx.memory
        .apply_batch(vec![Op::Add(lead_save)])
        .await
        .unwrap()
        .into_iter()
        .for_each(|r| assert!(r.is_ok()));
    let id = fx.entries(&Bucket::Owner)[0].id.clone();
    let note = moraebot::memory_tools::core_note(&id, &Bucket::Owner.label(), text);
    fx.store()
        .fail_group(&[row], "E_TEST", &note, &trace(false, false))
        .unwrap();

    fx.jev.answer(save("core", "s0"));
    assert_eq!(fx.observer.run_once().await, Pass::Idle);
    assert_eq!(fx.jev.requests().len(), 1, "the partial job was judged");
    assert_eq!(fx.entries(&Bucket::Owner).len(), 1);
    assert!(
        fx.store().pending_memory_notices(KEY).unwrap().is_empty(),
        "no observer notice for the lead's own save"
    );
    let failed = fx.store().get(row).unwrap().unwrap();
    assert_eq!(failed.reply.as_deref(), Some(note.as_str()));
    assert!(!failed.text_sent, "the stored line is still owed, once");

    let next = fx.admit("m2", "다음 질문");
    fx.reply(next, None, "답이에요", &trace(false, true));
    assert_eq!(
        fx.store().get(next).unwrap().unwrap().reply.as_deref(),
        Some("답이에요")
    );
}

#[tokio::test]
async fn observer_keeps_every_entry_of_a_lead_group_and_three_lanes() {
    let fx = Fixture::new().await;
    for n in 1..=3 {
        fx.lane(
            &format!("call-{n}"),
            None,
            &format!("레인 {n}의 결과는 좋다."),
        );
    }
    let row = fx.admit("m1", "모든 레인 결과를 기록해.");
    fx.reply(row, None, "네", &trace(false, true));
    for _ in 0..4 {
        fx.jev.answer(save("thread", "s0"));
    }
    let commits = fx.commits();
    assert_eq!(fx.observer.run_once().await, Pass::Idle);
    let saved = fx.entries(&Bucket::Thread(KEY.into()));
    let mut texts: Vec<&str> = saved.iter().map(|e| e.text.as_str()).collect();
    texts.sort_unstable();
    assert_eq!(
        texts,
        [
            "레인 1의 결과는 좋다.",
            "레인 2의 결과는 좋다.",
            "레인 3의 결과는 좋다.",
            "모든 레인 결과를 기록해."
        ]
    );
    assert_eq!(fx.commits(), commits + 1);
}

#[tokio::test]
async fn observer_writes_one_commit_per_pass() {
    let fx = Fixture::new().await;
    for (mid, text) in [("m1", "첫째 규칙은 이렇다."), ("m2", "둘째 규칙은 저렇다.")]
    {
        let row = fx.admit(mid, text);
        fx.reply(row, None, "네", &trace(false, true));
    }
    fx.jev.answer(save("thread", "s0"));
    fx.jev.answer(save("thread", "s0"));
    let commits = fx.commits();
    assert_eq!(fx.observer.run_once().await, Pass::Idle);
    assert_eq!(fx.entries(&Bucket::Thread(KEY.into())).len(), 2);
    assert_eq!(fx.commits(), commits + 1);
}

#[tokio::test]
async fn observer_logs_the_omp_version_for_each_job() {
    let fx = Fixture::new().await;
    let row = fx.admit("m1", "앞으로 답은 짧게 해줘.");
    fx.reply(row, None, "네", &trace(false, true));
    fx.lane("call-1", None, "결과는 좋다.");
    fx.observer.run_once().await;
    let (_root, db) = fx.into_db();
    let mut stmt = db
        .prepare("SELECT code, omp_version, jev_model, detail FROM memory_log WHERE job_id IS NOT NULL ORDER BY job_id")
        .unwrap();
    let rows: Vec<LogLine> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    for (code, version, model, _) in &rows {
        assert_eq!(code, M_JOB_DONE);
        assert_eq!(version.as_deref(), Some(OMP_VERSION));
        assert_eq!(model.as_deref(), Some("jev-1.14.0"));
    }
    assert_eq!(
        rows[0].3.as_deref(),
        Some("kind=group saved=0 skipped=0 requests=1 tokens=10")
    );
}

#[tokio::test]
async fn observer_fails_a_bad_job_after_three_attempts_and_goes_on() {
    let fx = Fixture::new().await;
    assert_eq!(fx.observer.run_once().await, Pass::Idle, "the start scan");
    let md = Fixture::subagent_file(&fx.lead_root(), "s1", "Explore_1", "조사 결과는 이렇다.");
    assert_eq!(
        fx.observer
            .scan_root(&fx.lead_root(), "lead", fx.t0)
            .await
            .unwrap(),
        1
    );
    // The file can no longer be read: a processing error, not jev's.
    std::fs::set_permissions(&md, std::fs::Permissions::from_mode(0o000)).unwrap();
    let row = fx.admit("m1", "앞으로 답은 짧게 해줘.");
    fx.reply(row, None, "네", &trace(false, true));

    assert_eq!(fx.observer.run_once().await, Pass::Idle);
    assert_eq!(
        fx.backlog(),
        Backlog {
            groups: 0,
            other: 1
        },
        "the next job went on"
    );
    assert_eq!(fx.observer.run_once().await, Pass::Idle);
    assert!(
        !fx.store()
            .memory_logged(E_OBSERVER_JOB, "attempts=3")
            .unwrap()
    );
    assert_eq!(fx.observer.run_once().await, Pass::Idle);
    assert!(
        fx.store()
            .memory_logged(E_OBSERVER_JOB, "attempts=3")
            .unwrap()
    );
    assert_eq!(fx.backlog(), Backlog::default());
    std::fs::set_permissions(&md, std::fs::Permissions::from_mode(0o600)).unwrap();
}

/// Plan 4.2: one job whose payload no longer decodes is failed on its own and never blocks
/// the queue.
#[tokio::test]
async fn observer_fails_an_unreadable_job_and_goes_on() {
    let fx = Fixture::new().await;
    let row = fx.admit("m1", "앞으로 답은 짧게 해줘.");
    fx.reply(row, None, "네", &trace(false, true));
    fx.lane("call-1", None, "결과는 좋다.");
    let config = fx.config.clone();
    let memory = fx.memory.clone();
    let clock = Arc::new(AtomicI64::new(fx.t0));
    let (_root, db) = fx.into_db();
    db.execute(
        "UPDATE memory_jobs SET payload = '{' WHERE kind = 'group'",
        [],
    )
    .unwrap();
    drop(db);
    let store = Arc::new(Mutex::new(
        Store::open(&config.home.join(STORE_FILE)).unwrap(),
    ));
    let observer = observer(&config, &store, &memory, &clock);

    assert_eq!(observer.run_once().await, Pass::Idle);
    let store = store.lock().unwrap();
    assert!(store.memory_logged(E_OBSERVER_JOB, "unreadable").unwrap());
    assert_eq!(store.memory_backlog().unwrap(), Backlog::default());
}

#[tokio::test]
async fn observer_settles_jobs_without_requests_when_jev_is_off() {
    let fx = Fixture::with(|config| config.memory.jev.enabled = false).await;
    let row = fx.admit("m1", "앞으로 답은 짧게 해줘.");
    fx.reply(row, None, "네", &trace(false, true));
    fx.lane("call-1", None, "결과는 좋다.");
    assert_eq!(fx.observer.run_once().await, Pass::Idle);
    assert!(fx.jev.requests().is_empty());
    assert_eq!(fx.backlog(), Backlog::default());
    assert!(
        fx.store()
            .memory_logged(
                M_JEV_OFF,
                "kind=group saved=0 skipped=0 requests=0 tokens=0"
            )
            .unwrap()
    );
    assert!(
        fx.store()
            .memory_logged(M_JEV_OFF, "kind=lane saved=0 skipped=0 requests=0 tokens=0")
            .unwrap()
    );
}

#[tokio::test]
async fn observer_prunes_old_log_rows_and_settled_jobs() {
    let fx = Fixture::new().await;
    let row = fx.admit("m1", "앞으로 답은 짧게 해줘.");
    fx.reply(row, None, "네", &trace(false, true));
    fx.observer.run_once().await;
    let detail = "kind=group saved=0 skipped=0 requests=1 tokens=10";
    assert!(fx.store().memory_logged(M_JOB_DONE, detail).unwrap());

    fx.set_clock(fx.t0 + 31 * DAY);
    assert_eq!(fx.observer.run_once().await, Pass::Idle);
    assert!(!fx.store().memory_logged(M_JOB_DONE, detail).unwrap());
    let (_root, db) = fx.into_db();
    let jobs: i64 = db
        .query_row("SELECT COUNT(*) FROM memory_jobs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(jobs, 0, "the settled job is gone");
}

#[tokio::test]
async fn subagent_file_is_collected_from_the_session_folder() {
    let fx = Fixture::new().await;
    assert_eq!(fx.observer.run_once().await, Pass::Idle, "the start scan");
    let row = fx.admit("m1", "이 저장소를 조사해줘.");
    Fixture::subagent_file(
        &fx.lead_root(),
        "2026-11-20_abc",
        "Explore_1",
        "[moraebot] ignored line\n저장소는 Rust로 작성되어 있다.",
    );
    fx.reply(row, None, "조사했어요", &trace(true, true));
    fx.jev.answer(NOTHING.clone());
    fx.jev.answer(save("project", "s0"));
    assert_eq!(fx.observer.run_once().await, Pass::Idle);
    let saved = fx.entries(&Bucket::General);
    assert_eq!(saved.len(), 1, "{saved:?}");
    assert_eq!(saved[0].text, "저장소는 Rust로 작성되어 있다.");
    assert_eq!(saved[0].prov, Prov::Subagent);
    assert_eq!(saved[0].src.as_deref(), Some("lead task=Explore_1"));
    let kinds: Vec<Value> = fx
        .bodies()
        .iter()
        .map(|b| b["state"]["kind"].clone())
        .collect();
    assert_eq!(kinds.last().unwrap(), "subagent_report");
    let options = fx.bodies().last().unwrap()["questions"]["bucket"]["criteria"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        options,
        ["project"],
        "a lead subagent offers no conversation bucket"
    );
    let (_root, db) = fx.into_db();
    let layout: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM memory_log WHERE code = ?1",
            [E_SUBAGENT_LAYOUT],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(layout, 0, "the file was where omp writes it");
}

#[tokio::test]
async fn subagent_file_written_after_the_turn_is_found_by_the_next_scan() {
    let fx = Fixture::new().await;
    assert_eq!(fx.observer.run_once().await, Pass::Idle, "the start scan");
    // A background subagent: the turn ended before it wrote anything.
    let first = fx.admit("m1", "백그라운드로 조사해줘.");
    fx.reply(first, None, "시작했어요", &trace(true, false));
    assert_eq!(fx.observer.run_once().await, Pass::Idle);
    assert_eq!(fx.backlog(), Backlog::default());

    Fixture::subagent_file(
        &fx.lead_root(),
        "s1",
        "Explore_2",
        "결론은 캐시가 원인이다.",
    );
    let second = fx.admit("m2", "결과 나왔어?");
    fx.reply(second, None, "네", &trace(false, true));
    fx.jev.answer(NOTHING.clone());
    fx.jev.answer(save("project", "s0"));
    assert_eq!(fx.observer.run_once().await, Pass::Idle);
    let saved = fx.entries(&Bucket::General);
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].src.as_deref(), Some("lead task=Explore_2"));
}

#[tokio::test]
async fn subagent_file_written_while_down_is_found_at_start() {
    let fx = Fixture::new().await;
    let (lane, _row) = fx.lane("call-1", None, "작업을 끝냈다.");
    fx.store().close_lane(lane, "done").unwrap();
    // Written while moraebot was down: no job knows it yet.
    Fixture::subagent_file(
        &omp::lane_session_dir(&fx.config, lane),
        "s1",
        "Review_1",
        "리뷰 결과 문제는 없었다.",
    );
    // The lane result itself is judged first and kept out.
    fx.jev.answer(json!({
        "save": {"type": "noul", "noul": 0.0},
        "bucket": {"type": "choice", "choice": "thread"},
        "span": {"type": "choice", "choice": "none"}
    }));
    fx.jev.answer(save("thread", "s0"));
    assert_eq!(fx.observer.run_once().await, Pass::Idle);
    let saved = fx.entries(&Bucket::Thread(KEY.into()));
    assert_eq!(saved.len(), 1, "{saved:?}");
    assert_eq!(saved[0].text, "리뷰 결과 문제는 없었다.");
    assert_eq!(saved[0].prov, Prov::Subagent);
    assert_eq!(
        saved[0].src.as_deref(),
        Some(format!("lane={lane} task=Review_1").as_str())
    );
}

#[tokio::test]
async fn subagent_file_is_queued_once_and_again_when_it_changes() {
    let fx = Fixture::new().await;
    let root = fx.lead_root();
    let md = Fixture::subagent_file(&root, "s1", "Explore_1", "처음 결과다.");
    assert_eq!(
        fx.observer.scan_root(&root, "lead", fx.t0).await.unwrap(),
        1
    );
    assert_eq!(
        fx.observer.scan_root(&root, "lead", fx.t0).await.unwrap(),
        0
    );
    // Same size: the change is seen through the new mtime (set here, so the test does not
    // depend on the file system's timestamp granularity).
    std::fs::write(&md, "고친 결과다.").unwrap();
    touch(&md, 10);
    assert_eq!(
        fx.observer.scan_root(&root, "lead", fx.t0).await.unwrap(),
        1
    );
    assert_eq!(
        fx.backlog(),
        Backlog {
            groups: 0,
            other: 2
        }
    );

    // The first job's file changed under it; only the second is judged.
    fx.jev.answer(save("project", "s0"));
    assert_eq!(fx.observer.run_once().await, Pass::Idle);
    assert_eq!(fx.jev.requests().len(), 1);
    let saved = fx.entries(&Bucket::General);
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].text, "고친 결과다.");
    let (_root, db) = fx.into_db();
    let changed: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM memory_log WHERE code = ?1",
            [M_ARTIFACT_CHANGED],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(changed, 1);
}

/// Sets `path`'s mtime to `seconds` after the epoch.
fn touch(path: &Path, seconds: u64) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(seconds))
        .unwrap();
}

/// A file whose size and mtime match its cursor row is not read again: a content change
/// that keeps both is not seen, which shows the scan skipped the read.
#[tokio::test]
async fn subagent_scan_skips_a_file_whose_size_and_mtime_did_not_change() {
    let fx = Fixture::new().await;
    let root = fx.lead_root();
    let md = Fixture::subagent_file(&root, "s1", "Explore_1", "처음 결과다.");
    touch(&md, 20);
    assert_eq!(
        fx.observer.scan_root(&root, "lead", fx.t0).await.unwrap(),
        1
    );
    std::fs::write(&md, "고친 결과다.").unwrap();
    touch(&md, 20);
    assert_eq!(
        fx.observer.scan_root(&root, "lead", fx.t0).await.unwrap(),
        0
    );
    touch(&md, 21);
    assert_eq!(
        fx.observer.scan_root(&root, "lead", fx.t0).await.unwrap(),
        1
    );
}

/// Several group jobs in one pass: the first one's scan queues the file a later settled
/// `task` turn wrote, and that later job does not report a layout fault.
#[tokio::test]
async fn subagent_layout_is_not_reported_when_an_earlier_job_found_the_file() {
    let fx = Fixture::new().await;
    assert_eq!(fx.observer.run_once().await, Pass::Idle, "the start scan");
    let first = fx.admit("m1", "안녕.");
    fx.reply(first, None, "안녕하세요", &trace(false, true));
    let second = fx.admit("m2", "이 저장소를 조사해줘.");
    Fixture::subagent_file(&fx.lead_root(), "s1", "Explore_1", "저장소는 Rust다.");
    fx.reply(second, None, "조사했어요", &trace(true, true));
    assert_eq!(fx.observer.run_once().await, Pass::Idle);
    let (_root, db) = fx.into_db();
    let layout: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM memory_log WHERE code = ?1",
            [E_SUBAGENT_LAYOUT],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(layout, 0);
}

#[tokio::test]
async fn subagent_symlinks_and_oversized_files_are_skipped() {
    let fx = Fixture::new().await;
    let root = fx.lead_root();
    let outside = fx.root.path().join("secret.txt");
    std::fs::write(&outside, "밖에 있는 파일이다.").unwrap();
    let md = Fixture::subagent_file(&root, "s1", "Linked_1", "x");
    std::fs::remove_file(&md).unwrap();
    std::os::unix::fs::symlink(&outside, &md).unwrap();
    let big = Fixture::subagent_file(&root, "s2", "Big_1", &"가".repeat(100_000));
    assert!(std::fs::metadata(&big).unwrap().len() > moraebot::observer::MAX_ARTIFACT_BYTES);

    assert_eq!(
        fx.observer.scan_root(&root, "lead", fx.t0).await.unwrap(),
        0
    );
    assert_eq!(
        fx.observer.scan_root(&root, "lead", fx.t0).await.unwrap(),
        0
    );
    assert_eq!(fx.backlog(), Backlog::default());
    let canonical = |p: &Path| {
        std::fs::canonicalize(p.parent().unwrap())
            .unwrap()
            .join(p.file_name().unwrap())
    };
    let store = fx.store();
    for path in [&md, &big] {
        assert!(
            store
                .memory_logged(W_SUBAGENT_SKIPPED, &canonical(path).to_string_lossy())
                .unwrap(),
            "{}",
            path.display()
        );
    }
    drop(store);
    let (_root, db) = fx.into_db();
    let skipped: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM memory_log WHERE code = ?1",
            [W_SUBAGENT_SKIPPED],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(skipped, 2, "one row per path");
}

#[tokio::test]
async fn subagent_layout_code_is_logged_once() {
    let fx = Fixture::new().await;
    assert_eq!(fx.observer.run_once().await, Pass::Idle, "the start scan");
    // A background subagent has written nothing when its turn ends: no code.
    let row = fx.admit("m1", "조사해줘.");
    fx.reply(row, None, "시작했어요", &trace(true, false));
    fx.observer.run_once().await;
    let unsettled_job = 1;
    // A settled turn that used `task` and left no file, twice.
    for mid in ["m2", "m3"] {
        let row = fx.admit(mid, "조사해줘.");
        fx.reply(row, None, "끝났어요", &trace(true, true));
    }
    assert_eq!(fx.observer.run_once().await, Pass::Idle);
    let (_root, db) = fx.into_db();
    let rows: Vec<Option<i64>> = {
        let mut stmt = db
            .prepare("SELECT job_id FROM memory_log WHERE code = ?1")
            .unwrap();
        stmt.query_map([E_SUBAGENT_LAYOUT], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    assert_eq!(rows.len(), 1, "logged once per process");
    assert_eq!(
        rows[0],
        Some(unsettled_job + 1),
        "by the first settled group"
    );
}

/// `code, omp_version, jev_model, detail` of one memory log row.
type LogLine = (String, Option<String>, Option<String>, Option<String>);

/// The texts of `bucket`'s entries, sorted.
fn texts(fx: &Fixture, bucket: &Bucket) -> Vec<String> {
    let mut texts: Vec<String> = fx.entries(bucket).into_iter().map(|e| e.text).collect();
    texts.sort_unstable();
    texts
}

/// Admits `text` as owner message `mid` and settles its lead turn.
fn owner_turn(fx: &Fixture, mid: &str, text: &str) -> i64 {
    let row = fx.admit(mid, text);
    fx.reply(row, None, "네", &trace(false, true));
    row
}

/// moraedang on #46: "from now on" and "앞으로" make a standing rule, as the docs and jev's
/// own `core` option text say.
#[tokio::test]
async fn observer_takes_a_from_now_on_rule_into_core() {
    let fx = Fixture::new().await;
    for (mid, text) in [
        ("m1", "From now on, reply in English."),
        ("m2", "앞으로는 PR 제목을 영어로 써."),
    ] {
        owner_turn(&fx, mid, text);
        fx.jev.answer(save("core", "s0"));
        assert_eq!(fx.observer.run_once().await, Pass::Idle);
    }
    assert_eq!(
        texts(&fx, &Bucket::Owner),
        [
            "From now on, reply in English.",
            "앞으로는 PR 제목을 영어로 써."
        ]
    );
    assert!(fx.entries(&Bucket::Thread(KEY.into())).is_empty());
}

/// moraedang on #46: the boundary gate is a whole-word match of always, never, 항상 or 절대, so
/// "never" at a sentence's end counts, "whenever" does not, and a one-off "don't" is no
/// boundary.
#[tokio::test]
async fn observer_boundary_needs_a_whole_always_or_never_word() {
    let fx = Fixture::new().await;
    for (mid, text) in [
        ("m1", "Whenever you're unsure, ask me."),
        ("m2", "Don't add emoji in this PR."),
        ("m3", "Push to main: never."),
    ] {
        owner_turn(&fx, mid, text);
        fx.jev.answer(save("boundary", "s0"));
        assert_eq!(fx.observer.run_once().await, Pass::Idle);
    }
    assert_eq!(texts(&fx, &Bucket::Boundaries), ["Push to main: never."]);
    assert_eq!(
        texts(&fx, &Bucket::Thread(KEY.into())),
        [
            "Don't add emoji in this PR.",
            "Whenever you're unsure, ask me."
        ]
    );
}

/// moraedang on #46: when the store cannot settle an unreadable job, the pass waits instead of
/// reading the same job again at once, in a loop that never yields.
#[tokio::test]
async fn observer_waits_when_an_unreadable_job_cannot_be_settled() {
    let fx = Fixture::new().await;
    owner_turn(&fx, "m1", "앞으로 답은 짧게 해줘.");
    fx.with_store_closed(&format!(
        "UPDATE memory_jobs SET payload = '{{' WHERE kind = 'group'; {FAIL_SETTLE};"
    ));
    let observer = Arc::new(observer(&fx.config, &fx.store, &fx.memory, &fx.clock));

    assert_eq!(
        run_once_on_a_thread(&observer),
        Some(Pass::Waiting { until: None })
    );
    fx.with_store_closed("DROP TRIGGER fail_settle");
    assert_eq!(run_once_on_a_thread(&observer), Some(Pass::Idle));
    assert_eq!(fx.backlog(), Backlog::default());
}

/// moraedang on #46: when settling fails after the entries were written, the pass backs off
/// instead of asking jev again, and the core entry it committed still gets its note when the
/// job runs again.
#[tokio::test]
async fn observer_backs_off_when_settling_fails_and_keeps_the_core_note() {
    let fx = Fixture::new().await;
    let text = "앞으로 답은 항상 한국어로 해줘.";
    owner_turn(&fx, "m1", text);
    fx.with_store_closed(FAIL_SETTLE);
    fx.jev.answer(save("core", "s0"));
    let pass = tokio::time::timeout(BOUND, fx.observer.run_once())
        .await
        .expect("the pass ends");
    assert_eq!(pass, Pass::Waiting { until: None });
    assert_eq!(fx.jev.requests().len(), 1, "jev is not asked again");
    assert_eq!(fx.entries(&Bucket::Owner).len(), 1);
    assert!(fx.store().pending_memory_notices(KEY).unwrap().is_empty());

    fx.with_store_closed("DROP TRIGGER fail_settle");
    fx.jev.answer(save("core", "s0"));
    assert_eq!(fx.observer.run_once().await, Pass::Idle);
    let id = fx.entries(&Bucket::Owner)[0].id.clone();
    let note = moraebot::memory_tools::core_note(&id, &Bucket::Owner.label(), text);
    assert_eq!(fx.store().pending_memory_notices(KEY).unwrap(), [note]);
    assert_eq!(fx.backlog(), Backlog::default());
}

/// moraedang on #46: a panic outside a job (here the clock, read first in every pass) never
/// ends the observer silently: the pass says so, `status_line` reports the observer down, and
/// the next pass goes on.
#[tokio::test]
async fn observer_survives_a_panic_outside_a_job_and_says_it_was_down() {
    let fx = Fixture::new().await;
    owner_turn(&fx, "m1", "첨째 규칙은 이렇다.");
    let broken = Arc::new(AtomicBool::new(true));
    let (time, flag) = (Arc::clone(&fx.clock), Arc::clone(&broken));
    let now: Now = Arc::new(move || {
        assert!(!flag.load(Ordering::SeqCst), "the clock is broken");
        time.load(Ordering::SeqCst)
    });
    let time = Arc::clone(&fx.clock);
    let jev_now: Now = Arc::new(move || time.load(Ordering::SeqCst));
    let jev = Arc::new(Jev::new(fx.config.memory.jev.clone(), jev_now));
    let observer = Observer::new(
        &fx.config,
        Arc::clone(&fx.store),
        fx.memory.clone(),
        jev,
        Redactor::default(),
        now,
    );

    assert_eq!(observer.run_once().await, Pass::Waiting { until: None });
    let status = observer.status_line();
    assert!(status.starts_with("Observer: down"), "{status}");
    assert!(fx.store().memory_logged(E_OBSERVER_PANIC, "pass").unwrap());

    broken.store(false, Ordering::SeqCst);
    fx.jev.answer(save("thread", "s0"));
    assert_eq!(observer.run_once().await, Pass::Idle);
    assert_eq!(fx.entries(&Bucket::Thread(KEY.into())).len(), 1);
    let status = observer.status_line();
    assert!(status.starts_with("Observer: jev ok"), "{status}");
}

/// moraedang on #46: a job jev rejects (another 4xx) counts one attempt each time and fails
/// after three, so it never holds the queue, and jev is not left paused for it.
#[tokio::test]
async fn observer_fails_a_job_jev_keeps_rejecting_and_goes_on() {
    let fx = Fixture::new().await;
    owner_turn(&fx, "m1", "첫째 규칙은 이렇다.");
    owner_turn(&fx, "m2", "둘째 규칙은 저렇다.");
    let rejected = || json!({"error": "unprocessable"});
    fx.jev.reply(422, &[], rejected());
    assert_eq!(fx.observer.run_once().await, Pass::Waiting { until: None });
    assert_eq!(fx.observer.run_once().await, Pass::Waiting { until: None });
    assert_eq!(fx.jev.requests().len(), 1, "paused: nothing more is sent");

    // Each restart tries the same job once more.
    let restarted = observer(&fx.config, &fx.store, &fx.memory, &fx.clock);
    fx.jev.reply(422, &[], rejected());
    assert_eq!(restarted.run_once().await, Pass::Waiting { until: None });
    let restarted = observer(&fx.config, &fx.store, &fx.memory, &fx.clock);
    fx.jev.reply(422, &[], rejected());
    fx.jev.answer(save("thread", "s0"));
    assert_eq!(restarted.run_once().await, Pass::Idle);
    assert!(
        fx.store()
            .memory_logged(E_JEV_REQUEST, "attempts=3")
            .unwrap()
    );
    assert_eq!(
        texts(&fx, &Bucket::Thread(KEY.into())),
        ["둘째 규칙은 저렇다."]
    );
    assert_eq!(fx.backlog(), Backlog::default());
    let status = restarted.status_line();
    assert!(status.starts_with("Observer: jev ok"), "{status}");
}

/// moraedang on #46: a job whose answers never fit counts one attempt each time and fails
/// after three; the breaker is not left counting its failures.
#[tokio::test]
async fn observer_fails_a_job_whose_answers_never_fit_and_goes_on() {
    let fx = Fixture::new().await;
    owner_turn(&fx, "m1", "첫째 규칙은 이렇다.");
    owner_turn(&fx, "m2", "둘째 규칙은 저렇다.");
    let unfit = || {
        json!({
            "model": "jev-1.14.0",
            "answers": {},
            "usage": {"input_tokens": 1, "output_tokens": 0},
        })
    };
    fx.jev.reply(200, &[], unfit());
    assert_eq!(
        fx.observer.run_once().await,
        Pass::Waiting {
            until: Some(fx.t0 + 2)
        }
    );
    fx.set_clock(fx.t0 + 2);
    fx.jev.reply(200, &[], unfit());
    assert_eq!(
        fx.observer.run_once().await,
        Pass::Waiting {
            until: Some(fx.t0 + 6)
        }
    );
    fx.set_clock(fx.t0 + 6);
    fx.jev.reply(200, &[], unfit());
    fx.jev.answer(save("thread", "s0"));
    assert_eq!(fx.observer.run_once().await, Pass::Idle);
    assert!(
        fx.store()
            .memory_logged(E_JEV_RESPONSE, "attempts=3")
            .unwrap()
    );
    assert_eq!(
        texts(&fx, &Bucket::Thread(KEY.into())),
        ["둘째 규칙은 저렇다."]
    );
    assert_eq!(fx.backlog(), Backlog::default());
    let status = fx.observer.status_line();
    assert!(status.starts_with("Observer: jev ok"), "{status}");
}

/// Owner decision on #46: jev is opt-in. With a key but without `enabled = true`, nothing is
/// sent and jobs settle as `M_JEV_OFF`.
#[tokio::test]
async fn observer_sends_nothing_with_a_key_alone() {
    let fx = Fixture::with(|config| {
        let mut jev = JevConfig::default();
        jev.base_url = config.memory.jev.base_url.clone();
        jev.set_env_key(Some("JEV_KEY_SENTINEL_0d17".into()));
        config.memory.jev = jev;
    })
    .await;
    owner_turn(&fx, "m1", "앞으로 답은 짧게 해줘.");
    assert_eq!(fx.observer.run_once().await, Pass::Idle);
    assert!(fx.jev.requests().is_empty());
    assert_eq!(fx.backlog(), Backlog::default());
    assert!(
        fx.store()
            .memory_logged(
                M_JEV_OFF,
                "kind=group saved=0 skipped=0 requests=0 tokens=0"
            )
            .unwrap()
    );
    let status = fx.observer.status_line();
    assert!(status.starts_with("Observer: jev off"), "{status}");
}

fn dm(message_id: &str, text: &str) -> InboundEvent {
    InboundEvent {
        message_id: message_id.into(),
        author_id: OWNER.into(),
        author_is_bot: false,
        channel_id: DM_CHANNEL.into(),
        thread_id: None,
        is_dm: true,
        text: text.into(),
        attachments: Vec::new(),
    }
}

#[tokio::test]
async fn gateway_observes_a_reply_end_to_end() {
    let root = tempfile::tempdir().unwrap();
    let jev = FakeJev::start().await;
    let config = test_config(
        root.path(),
        PathBuf::from(env!("CARGO_BIN_EXE_fake-omp")),
        &jev.base_url(),
    );
    let adapter = Arc::new(FakeAdapter::new());
    let as_dyn: Arc<dyn MessengerAdapter> = adapter.clone();
    let gateway = Gateway::open(&config, as_dyn)
        .unwrap()
        .with_omp(|omp| omp.with_extra_env([("FAKE_OMP_MODE", "ok")]));
    let mut progress = gateway.memory_progress();
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(gateway.run(async {
        let _ = stopped.await;
    }));

    jev.answer(save("core", "s0"));
    adapter
        .inject(dm("1300000000000000001", "앞으로 답은 항상 한국어로 해줘."))
        .await;
    tokio::time::timeout(BOUND, adapter.wait_for_send(1))
        .await
        .unwrap();
    tokio::time::timeout(BOUND, progress.wait_for(|n| *n >= 1))
        .await
        .unwrap()
        .unwrap();
    let owner_md = std::fs::read_to_string(config.home.join("memory/core/owner.md")).unwrap();
    assert!(
        owner_md.contains("앞으로 답은 항상 한국어로 해줘."),
        "{owner_md}"
    );

    // The owner sees the observer's core save under the next reply.
    adapter.inject(dm("1300000000000000002", "고마워")).await;
    tokio::time::timeout(BOUND, adapter.wait_for_send(2))
        .await
        .unwrap();
    let _ = stop.send(());
    tokio::time::timeout(BOUND, task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let sends: Vec<String> = adapter
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::Send { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert!(!sends[0].contains("[memory] saved"), "{sends:?}");
    assert!(
        sends[1].contains("[memory] saved m-") && sends[1].contains("to core/owner"),
        "{sends:?}"
    );
}
