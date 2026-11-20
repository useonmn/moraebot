//! The jev client against a loopback fake jev server, with a manual clock.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use moraebot::config::JevConfig;
use moraebot::jev::{
    Answer, E_JEV_AUTH, E_JEV_REQUEST, E_JEV_RESPONSE, Jev, JevError, JevStatus, Now, Question,
    Request, W_JEV_BUSY, W_JEV_NETWORK,
};
use moraebot::testing::FakeJev;
use serde_json::json;

const T0: i64 = 1_800_000_000;
const LIMIT: Duration = Duration::from_secs(10);

struct Clock(Arc<AtomicI64>);

impl Clock {
    fn new() -> Self {
        Self(Arc::new(AtomicI64::new(T0)))
    }

    fn now(&self) -> Now {
        let time = self.0.clone();
        Arc::new(move || time.load(Ordering::SeqCst))
    }

    fn set(&self, offset: i64) {
        self.0.store(T0 + offset, Ordering::SeqCst);
    }
}

fn config(base_url: String) -> JevConfig {
    let mut config = JevConfig {
        enabled: true,
        base_url,
        ..JevConfig::default()
    };
    config.set_env_key(Some("KEY_SENTINEL_1".into()));
    config
}

fn client(fake: &FakeJev, clock: &Clock) -> Jev {
    Jev::new(config(fake.base_url()), clock.now())
}

fn noul(instructions: &str) -> Question {
    Question::Noul {
        instructions: instructions.into(),
        yes: "Yes.".into(),
        no: "No.".into(),
    }
}

fn choice(options: &[&str]) -> Question {
    Question::Choice {
        instructions: "Pick one.".into(),
        options: options
            .iter()
            .map(|o| ((*o).to_owned(), format!("Option {o}.")))
            .collect(),
    }
}

fn request() -> Request {
    Request {
        model: "jev-latest".into(),
        state: json!({"kind": "owner_message", "sentences": ["s0: a", "s3: b"]}),
        questions: vec![
            ("save".into(), noul("Keep?")),
            ("span".into(), choice(&["s3", "s0", "none"])),
        ],
    }
}

fn good_answers() -> serde_json::Value {
    json!({
        "save": {"type": "noul", "noul": 0.9},
        "span": {"type": "choice", "choice": "s3", "confidence": 0.8},
    })
}

fn transient(code: &'static str, status: Option<u16>, retry_at: i64) -> JevError {
    JevError::Transient {
        code,
        status,
        retry_at,
    }
}

#[tokio::test]
async fn jev_request_matches_the_documented_shape() {
    let fake = FakeJev::start().await;
    let clock = Clock::new();
    let jev = client(&fake, &clock);
    fake.answer(good_answers());

    let response = jev.ask(&request(), LIMIT).await.expect("answered");

    assert_eq!(response.model, "jev-1.14.0");
    assert_eq!(response.answers["save"], Answer::Noul { noul: 0.9 });
    let seen = fake.requests();
    assert_eq!(seen.len(), 1);
    let sent = &seen[0];
    assert_eq!(sent.path, "/v1/systemone");
    assert_eq!(sent.authorization.as_deref(), Some("Bearer KEY_SENTINEL_1"));
    assert_eq!(sent.body["model"], "jev-latest");
    assert_eq!(sent.body["state"]["kind"], "owner_message");
    assert_eq!(
        sent.body["questions"]["save"],
        json!({"type": "noul", "instructions": "Keep?", "criteria": {"true": "Yes.", "false": "No."}})
    );
    assert_eq!(sent.body["questions"]["span"]["type"], "choice");
    let at = |needle: &str| sent.raw.find(needle).expect(needle);
    assert!(
        at("\"save\"") < at("\"span\""),
        "questions in the given order"
    );
    assert!(
        at("\"s3\"") < at("\"s0\"") && at("\"s0\"") < at("\"none\""),
        "options in the given order: {}",
        sent.raw
    );
}

#[tokio::test]
async fn jev_answers_are_read_by_name_whatever_the_order() {
    let fake = FakeJev::start().await;
    let clock = Clock::new();
    let jev = client(&fake, &clock);
    // The reply object comes back with its keys sorted, so "bucket" and "save" arrive before
    // "span" while the request asked "span" first.
    let answers = json!({
        "span": {"type": "choice", "choice": "s0"},
        "save": {"type": "noul", "noul": 0.25},
        "bucket": {"type": "choice", "choice": "core"},
    });
    let mut results = Vec::new();
    for (spans, buckets) in [
        (["s0", "s3", "none"], ["thread", "core", "none"]),
        (["none", "s3", "s0"], ["none", "core", "thread"]),
    ] {
        fake.answer(answers.clone());
        let asked = Request {
            questions: vec![
                ("span".into(), choice(&spans)),
                ("save".into(), noul("Keep?")),
                ("bucket".into(), choice(&buckets)),
            ],
            ..request()
        };
        let response = jev.ask(&asked, LIMIT).await.expect("answered");
        results.push(response.answers);
    }
    assert_eq!(results[0], results[1]);
    assert!(matches!(
        &results[0]["span"],
        Answer::Choice { choice, .. } if choice == "s0"
    ));
    assert!(matches!(
        &results[0]["bucket"],
        Answer::Choice { choice, .. } if choice == "core"
    ));
    assert_eq!(results[0]["save"], Answer::Noul { noul: 0.25 });
}

#[tokio::test]
async fn jev_401_and_403_pause_and_are_never_retried() {
    for status in [401, 403] {
        let fake = FakeJev::start().await;
        let clock = Clock::new();
        let jev = client(&fake, &clock);
        fake.reply(status, &[], json!({"error": "nope"}));

        let first = jev.ask(&request(), LIMIT).await.unwrap_err();
        assert_eq!(
            first,
            JevError::Permanent {
                code: E_JEV_AUTH,
                status: Some(status)
            }
        );
        clock.set(100_000);
        let again = jev.ask(&request(), LIMIT).await.unwrap_err();
        assert_eq!(again, JevError::Paused { code: E_JEV_AUTH });
        assert_eq!(jev.status(), JevStatus::Paused { code: E_JEV_AUTH });
        assert_eq!(fake.requests().len(), 1, "status {status}: nothing resent");
    }
}

#[tokio::test]
async fn jev_without_a_key_is_paused_and_sends_nothing() {
    let fake = FakeJev::start().await;
    let clock = Clock::new();
    let enabled = JevConfig {
        enabled: true,
        base_url: fake.base_url(),
        ..JevConfig::default()
    };
    let jev = Jev::new(enabled, clock.now());
    assert_eq!(jev.status(), JevStatus::Paused { code: E_JEV_AUTH });
    assert_eq!(
        jev.ask(&request(), LIMIT).await.unwrap_err(),
        JevError::Paused { code: E_JEV_AUTH }
    );

    let unset = JevConfig {
        base_url: fake.base_url(),
        ..JevConfig::default()
    };
    let off = Jev::new(unset, clock.now());
    assert_eq!(off.status(), JevStatus::Off);
    assert_eq!(off.ask(&request(), LIMIT).await.unwrap_err(), JevError::Off);

    let mut keyed_but_off = config(fake.base_url());
    keyed_but_off.enabled = false;
    let off = Jev::new(keyed_but_off, clock.now());
    assert_eq!(off.ask(&request(), LIMIT).await.unwrap_err(), JevError::Off);

    // Owner decision on #46: jev is opt-in, so a key with the default settings stays off.
    let mut keyed_by_default = JevConfig {
        base_url: fake.base_url(),
        ..JevConfig::default()
    };
    keyed_by_default.set_env_key(Some("KEY_SENTINEL_1".into()));
    let off = Jev::new(keyed_by_default, clock.now());
    assert_eq!(off.status(), JevStatus::Off);
    assert_eq!(off.ask(&request(), LIMIT).await.unwrap_err(), JevError::Off);

    assert!(fake.requests().is_empty());
}

#[tokio::test]
async fn jev_other_4xx_pauses_with_the_request_code() {
    for status in [400, 404, 413, 415, 422] {
        let fake = FakeJev::start().await;
        let clock = Clock::new();
        let jev = client(&fake, &clock);
        fake.reply(status, &[], json!({"error": "bad"}));

        assert_eq!(
            jev.ask(&request(), LIMIT).await.unwrap_err(),
            JevError::Permanent {
                code: E_JEV_REQUEST,
                status: Some(status)
            }
        );
        assert_eq!(
            jev.ask(&request(), LIMIT).await.unwrap_err(),
            JevError::Paused {
                code: E_JEV_REQUEST
            }
        );
        assert_eq!(fake.requests().len(), 1, "status {status}");
    }
}

#[tokio::test]
async fn jev_429_and_529_wait_longer_each_time() {
    let fake = FakeJev::start().await;
    let clock = Clock::new();
    let jev = client(&fake, &clock);
    let busy = json!({"error": "busy"});

    fake.reply(429, &[], busy.clone());
    assert_eq!(
        jev.ask(&request(), LIMIT).await.unwrap_err(),
        transient(W_JEV_BUSY, Some(429), T0 + 2)
    );
    clock.set(1);
    assert_eq!(
        jev.ask(&request(), LIMIT).await.unwrap_err(),
        JevError::NotYet { retry_at: T0 + 2 }
    );
    assert_eq!(jev.status(), JevStatus::Waiting { until: T0 + 2 });
    assert_eq!(fake.requests().len(), 1);

    clock.set(2);
    fake.reply(529, &[], busy.clone());
    assert_eq!(
        jev.ask(&request(), LIMIT).await.unwrap_err(),
        transient(W_JEV_BUSY, Some(529), T0 + 6)
    );
    assert_eq!(fake.requests().len(), 2);

    clock.set(5);
    assert!(matches!(
        jev.ask(&request(), LIMIT).await,
        Err(JevError::NotYet { .. })
    ));
    assert_eq!(fake.requests().len(), 2);

    clock.set(6);
    fake.reply(503, &[], busy);
    assert_eq!(
        jev.ask(&request(), LIMIT).await.unwrap_err(),
        transient(W_JEV_BUSY, Some(503), T0 + 14)
    );
    assert_eq!(fake.requests().len(), 3);
}

#[tokio::test]
async fn jev_retry_after_is_honored() {
    let fake = FakeJev::start().await;
    let clock = Clock::new();
    let jev = client(&fake, &clock);
    let busy = json!({"error": "busy"});
    // Each step: the header the server sends, the time of the call, the expected retry time.
    // The fourth failure would back off 16 s; its smaller header does not shorten that.
    let steps: [(&str, &str, i64, i64); 4] = [
        ("retry-after", "30", 0, 30),
        ("retry-after-ms", "7500", 30, 38),
        ("Retry-After", "100000", 38, 38 + 300),
        ("Retry-After", "1", 338, 338 + 16),
    ];
    for (n, (header, value, at, retry_at)) in steps.into_iter().enumerate() {
        clock.set(at);
        fake.reply(429, &[(header, value)], busy.clone());
        assert_eq!(
            jev.ask(&request(), LIMIT).await.unwrap_err(),
            transient(W_JEV_BUSY, Some(429), T0 + retry_at),
            "step {n}"
        );
    }
    assert_eq!(fake.requests().len(), 4);
}

#[tokio::test]
async fn jev_breaker_opens_and_probes_once_per_interval() {
    let fake = FakeJev::start().await;
    let clock = Clock::new();
    let mut settings = config(fake.base_url());
    settings.breaker_failures = 3;
    let jev = client_with(settings, &clock);
    let busy = json!({"error": "busy"});

    for (at, retry_at) in [(0, 2), (2, 6), (6, 306)] {
        clock.set(at);
        fake.reply(529, &[], busy.clone());
        assert_eq!(
            jev.ask(&request(), LIMIT).await.unwrap_err(),
            transient(W_JEV_BUSY, Some(529), T0 + retry_at)
        );
    }
    assert_eq!(jev.status(), JevStatus::Open { until: T0 + 306 });
    clock.set(305);
    assert!(matches!(
        jev.ask(&request(), LIMIT).await,
        Err(JevError::NotYet { .. })
    ));
    assert_eq!(fake.requests().len(), 3);

    // Both calls check the gate before either is answered: one is the probe, the other waits.
    clock.set(306);
    fake.reply(529, &[], busy);
    let first = request();
    let second = request();
    let (probe, other) = tokio::join!(jev.ask(&first, LIMIT), jev.ask(&second, LIMIT));
    assert_eq!(
        probe.unwrap_err(),
        transient(W_JEV_BUSY, Some(529), T0 + 606)
    );
    assert!(matches!(other, Err(JevError::NotYet { .. })));
    assert_eq!(fake.requests().len(), 4, "one probe per interval");

    clock.set(605);
    assert!(matches!(
        jev.ask(&request(), LIMIT).await,
        Err(JevError::NotYet { .. })
    ));
    assert_eq!(fake.requests().len(), 4);
}

fn client_with(settings: JevConfig, clock: &Clock) -> Jev {
    Jev::new(settings, clock.now())
}

#[tokio::test]
async fn jev_a_closed_connection_is_transient() {
    let fake = FakeJev::start().await;
    let clock = Clock::new();
    let jev = client(&fake, &clock);
    fake.drop_next();
    assert_eq!(
        jev.ask(&request(), LIMIT).await.unwrap_err(),
        transient(W_JEV_NETWORK, None, T0 + 2)
    );
    fake.wait_requests(1).await;

    let refused = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        format!("http://{addr}")
    };
    let jev = client_with(config(refused), &clock);
    assert_eq!(
        jev.ask(&request(), LIMIT).await.unwrap_err(),
        transient(W_JEV_NETWORK, None, T0 + 2)
    );
}

#[tokio::test]
async fn jev_a_bad_answer_is_a_transient_response_error() {
    let fake = FakeJev::start().await;
    let clock = Clock::new();
    let jev = client(&fake, &clock);
    let replies = [
        json!({"save": {"type": "noul", "noul": 0.9}}),
        json!({"save": {"type": "noul", "noul": 1.5}, "span": {"type": "choice", "choice": "s3"}}),
        json!({"save": {"type": "noul", "noul": 0.9}, "span": {"type": "choice", "choice": "s9"}}),
        json!({"save": {"type": "choice", "choice": "s3"}, "span": {"type": "choice", "choice": "s3"}}),
    ];
    let mut at = 0;
    for (n, answers) in replies.into_iter().enumerate() {
        clock.set(at);
        fake.answer(answers);
        let err = jev.ask(&request(), LIMIT).await.unwrap_err();
        let JevError::Transient { code, retry_at, .. } = err else {
            panic!("reply {n}: {err:?}");
        };
        assert_eq!(code, E_JEV_RESPONSE, "reply {n}");
        at = retry_at - T0;
    }
    clock.set(at);
    fake.reply(200, &[], json!({"not": "a response"}));
    assert!(matches!(
        jev.ask(&request(), LIMIT).await,
        Err(JevError::Transient {
            code: E_JEV_RESPONSE,
            ..
        })
    ));
}

#[tokio::test]
async fn jev_a_success_closes_the_breaker() {
    let fake = FakeJev::start().await;
    let clock = Clock::new();
    let mut settings = config(fake.base_url());
    settings.breaker_failures = 2;
    let jev = client_with(settings, &clock);
    let busy = json!({"error": "busy"});
    for at in [0, 2] {
        clock.set(at);
        fake.reply(429, &[], busy.clone());
        jev.ask(&request(), LIMIT).await.unwrap_err();
    }
    assert_eq!(jev.status(), JevStatus::Open { until: T0 + 302 });

    clock.set(302);
    fake.answer(good_answers());
    jev.ask(&request(), LIMIT)
        .await
        .expect("the probe succeeds");
    assert_eq!(jev.status(), JevStatus::Ready);

    fake.reply(429, &[], busy);
    assert_eq!(
        jev.ask(&request(), LIMIT).await.unwrap_err(),
        transient(W_JEV_BUSY, Some(429), T0 + 302 + 2),
        "the failure count starts again"
    );
    assert_eq!(fake.requests().len(), 4);
}

#[tokio::test]
async fn jev_a_quick_call_that_times_out_leaves_the_gate_open() {
    let fake = FakeJev::start().await;
    let clock = Clock::new();
    let jev = client(&fake, &clock);
    fake.hang_next();
    assert_eq!(
        jev.ask_quick(&request(), Duration::from_millis(200))
            .await
            .unwrap_err(),
        transient(W_JEV_NETWORK, None, 0)
    );
    assert_eq!(jev.status(), JevStatus::Ready);
    fake.answer(good_answers());
    assert!(jev.ask(&request(), LIMIT).await.is_ok());
    assert_eq!(fake.requests().len(), 2);

    // The same timeout through `ask` counts as a failure and closes the gate.
    fake.hang_next();
    assert_eq!(
        jev.ask(&request(), Duration::from_millis(200))
            .await
            .unwrap_err(),
        transient(W_JEV_NETWORK, None, T0 + 2)
    );
    assert_eq!(jev.status(), JevStatus::Waiting { until: T0 + 2 });
}
