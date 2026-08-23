//! Integration tests against the mock console.
//!
//! Most of these are regressions for defects in the previous implementation. Each names the
//! behaviour it protects rather than the bug number, so a failure says what broke.

use std::time::{Duration, Instant};

use rcp_mock::MockConsole;
use yamaha_rcp::{ConnectionConfig, Curve, Error, Fade, LabelColor, Mixer, Strip, NEG_INF};

/// Timeouts tuned down so tests fail fast instead of waiting out production defaults.
fn test_config() -> ConnectionConfig {
    ConnectionConfig {
        connect_timeout: Duration::from_millis(500),
        response_timeout: Duration::from_millis(200),
        min_command_interval: Duration::from_millis(5),
        keepalive_interval: Duration::from_secs(60),
        reconnect_min_backoff: Duration::from_millis(20),
        reconnect_max_backoff: Duration::from_millis(50),
    }
}

async fn connect(console: &MockConsole) -> Mixer {
    Mixer::connect_with(console.addr(), test_config())
        .await
        .expect("mock console should accept the connection")
}

/// Poll until `f` holds, so tests do not depend on a fixed sleep.
async fn eventually(label: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for: {label}");
}

#[tokio::test(flavor = "multi_thread")]
async fn basic_set_and_get_round_trip() {
    let console = MockConsole::start().await;
    let mixer = connect(&console).await;

    mixer.set_fader_db(Strip::Input, 4, -10.0).await.unwrap();
    assert_eq!(
        console
            .value("MIXER:Current/InCh/Fader/Level", 4, 0)
            .as_deref(),
        Some("-1000")
    );
    assert_eq!(mixer.fader_db(Strip::Input, 4).await.unwrap(), -10.0);
}

/// Regression: the reader allocated a fresh buffer per TCP read, so a reply split across two
/// reads was discarded and the caller waited forever.
#[tokio::test(flavor = "multi_thread")]
async fn reply_split_across_tcp_segments_is_still_correlated() {
    let console = MockConsole::builder().split_responses().start().await;
    let mixer = connect(&console).await;

    console.set_value("MIXER:Current/InCh/Fader/Level", 0, 0, "-1234");
    assert_eq!(mixer.fader_level(Strip::Input, 0).await.unwrap(), -1234);

    // And again, to prove the leftover partial line did not poison the next reply.
    console.set_value("MIXER:Current/InCh/Fader/Level", 1, 0, "-5678");
    assert_eq!(mixer.fader_level(Strip::Input, 1).await.unwrap(), -5678);
}

/// Regression: unsolicited NOTIFY lines used to be filtered by prefix matching. If one were ever
/// counted as a reply, every later command would be paired with the previous command's response.
#[tokio::test(flavor = "multi_thread")]
async fn notifications_do_not_consume_replies() {
    let console = MockConsole::start().await;
    let mixer = connect(&console).await;
    let mut notifications = mixer.notifications();

    console.set_value("MIXER:Current/InCh/Fader/Level", 7, 0, "-700");
    console.notify("NOTIFY set MIXER:Current/InCh/Fader/On 4 0 0");
    console.notify("NOTIFY sscurrent_ex scene_a 12");

    // The reply to this must be *this* command's reply, not a notification.
    assert_eq!(mixer.fader_level(Strip::Input, 7).await.unwrap(), -700);

    let first = notifications.recv().await.unwrap();
    assert_eq!(first.verb(), "set");
    assert_eq!(first.path(), Some("MIXER:Current/InCh/Fader/On"));
    let second = notifications.recv().await.unwrap();
    assert_eq!(second.verb(), "sscurrent_ex");
}

/// Regression: there was no response timeout, so a lost reply hung the caller forever.
#[tokio::test(flavor = "multi_thread")]
async fn missing_reply_times_out_rather_than_hanging() {
    let console = MockConsole::builder().swallow_replies(1).start().await;
    let mixer = connect(&console).await;

    let started = Instant::now();
    let err = mixer.fader_level(Strip::Input, 0).await.unwrap_err();

    assert!(matches!(err, Error::Timeout { .. }), "got {err:?}");
    assert!(err.is_transient(), "a timeout should be retryable");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "timeout was not prompt"
    );

    // The queue must keep moving: a timed-out command cannot block the one behind it.
    console.set_value("MIXER:Current/InCh/Fader/Level", 0, 0, "-100");
    assert_eq!(mixer.fader_level(Strip::Input, 0).await.unwrap(), -100);
}

/// Regression: EOF was unhandled, so the reader spun at 100% CPU and the mixer never recovered.
#[tokio::test(flavor = "multi_thread")]
async fn console_hangup_is_survived_and_the_client_reconnects() {
    let console = MockConsole::builder().hangup_after(3).start().await;
    let mixer = connect(&console).await;

    // Command 1 is the keepalive handshake; this is command 2.
    mixer.set_fader_db(Strip::Input, 0, -6.0).await.unwrap();
    // Command 3 gets its reply, then the console hangs up.
    let _ = mixer.set_fader_db(Strip::Input, 1, -6.0).await;

    console.clear_faults();
    eventually("client to reconnect", || console.connection_count() >= 2).await;

    // A fresh command over the new connection must work.
    eventually("commands to work again", || mixer.is_connected()).await;
    mixer.set_fader_db(Strip::Input, 2, -12.0).await.unwrap();
    assert_eq!(
        console
            .value("MIXER:Current/InCh/Fader/Level", 2, 0)
            .as_deref(),
        Some("-1200")
    );
}

/// Regression: a failed write returned early and dropped the connection on the floor, so the
/// pool starved and every later command blocked. In-flight commands must fail, not hang.
#[tokio::test(flavor = "multi_thread")]
async fn commands_in_flight_when_the_link_drops_fail_promptly() {
    let console = MockConsole::builder()
        .swallow_replies(1)
        .hangup_after(2)
        .start()
        .await;
    let mixer = connect(&console).await;

    let started = Instant::now();
    let err = mixer.fader_level(Strip::Input, 0).await.unwrap_err();

    assert!(err.is_transient(), "got {err:?}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "in-flight command should fail with the link, not wait out a timeout"
    );
}

/// Regression: `muted()` returned the console's `Fader/On` value verbatim, so it reported the
/// opposite of the truth and contradicted `set_muted()`.
#[tokio::test(flavor = "multi_thread")]
async fn muted_is_the_true_inverse_of_on() {
    let console = MockConsole::start().await;
    let mixer = connect(&console).await;

    mixer.set_muted(Strip::Input, 3, true).await.unwrap();
    assert_eq!(
        console
            .value("MIXER:Current/InCh/Fader/On", 3, 0)
            .as_deref(),
        Some("0"),
        "muting a channel must turn Fader/On off"
    );
    assert!(mixer.muted(Strip::Input, 3).await.unwrap());
    assert!(!mixer.on(Strip::Input, 3).await.unwrap());

    mixer.set_muted(Strip::Input, 3, false).await.unwrap();
    assert!(!mixer.muted(Strip::Input, 3).await.unwrap());
    assert!(mixer.on(Strip::Input, 3).await.unwrap());
}

/// Regression: `fade()` computed `duration_ms / 50` and divided by the result, panicking with a
/// divide-by-zero for any duration under 50ms.
#[tokio::test(flavor = "multi_thread")]
async fn fade_shorter_than_one_tick_is_a_jump_not_a_panic() {
    let console = MockConsole::start().await;
    let mixer = connect(&console).await;

    for ms in [0u64, 1, 10, 49] {
        mixer
            .fade(
                Strip::Input,
                0,
                Fade::to(-20_00, Duration::from_millis(ms)).from(0),
            )
            .await
            .unwrap();
        assert_eq!(
            console
                .value("MIXER:Current/InCh/Fader/Level", 0, 0)
                .as_deref(),
            Some("-2000"),
            "a {ms}ms fade should land on target"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn fade_lands_exactly_on_target_and_moves_monotonically() {
    let console = MockConsole::start().await;
    let mixer = connect(&console).await;

    mixer
        .fade(
            Strip::Input,
            0,
            Fade::to(-40_00, Duration::from_millis(300))
                .from(0)
                .tick(Duration::from_millis(30)),
        )
        .await
        .unwrap();

    let levels: Vec<i32> = console
        .commands_excluding_keepalive()
        .iter()
        .filter_map(|c| c.strip_prefix("set MIXER:Current/InCh/Fader/Level 0 0 "))
        .map(|v| v.parse().unwrap())
        .collect();

    assert!(levels.len() > 3, "expected several steps, got {levels:?}");
    assert_eq!(
        *levels.last().unwrap(),
        -40_00,
        "must land exactly on target"
    );
    assert!(
        levels.windows(2).all(|w| w[1] <= w[0]),
        "a downward fade must not go back up: {levels:?}"
    );
}

/// -inf is a sentinel (-32768), not a point on the dB scale. Ramping through it would slam the
/// fader to the bottom on the first step.
#[tokio::test(flavor = "multi_thread")]
async fn fade_to_negative_infinity_ramps_then_places_the_sentinel() {
    let console = MockConsole::start().await;
    let mixer = connect(&console).await;

    mixer
        .fade(
            Strip::Input,
            0,
            Fade::to(NEG_INF, Duration::from_millis(200))
                .from(0)
                .tick(Duration::from_millis(30)),
        )
        .await
        .unwrap();

    let levels: Vec<i32> = console
        .commands_excluding_keepalive()
        .iter()
        .filter_map(|c| c.strip_prefix("set MIXER:Current/InCh/Fader/Level 0 0 "))
        .map(|v| v.parse().unwrap())
        .collect();

    assert_eq!(
        *levels.last().unwrap(),
        NEG_INF,
        "must finish at the sentinel"
    );
    assert!(
        levels[..levels.len() - 1].iter().all(|&v| v > NEG_INF),
        "intermediate steps must stay on the dB scale: {levels:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fade_reads_the_current_level_when_no_start_is_given() {
    let console = MockConsole::start().await;
    let mixer = connect(&console).await;

    console.set_value("MIXER:Current/InCh/Fader/Level", 0, 0, "-500");
    mixer
        .fade(
            Strip::Input,
            0,
            Fade::to(-1500, Duration::from_millis(150)).tick(Duration::from_millis(30)),
        )
        .await
        .unwrap();

    let commands = console.commands_excluding_keepalive();
    assert_eq!(
        commands.iter().filter(|c| c.starts_with("get ")).count(),
        1,
        "exactly one read to find the starting level: {commands:?}"
    );
    let first_set = commands
        .iter()
        .find(|c| c.starts_with("set "))
        .expect("a fade must write something");
    let first: i32 = first_set.rsplit(' ').next().unwrap().parse().unwrap();
    assert!(
        first < -500 && first > -1500,
        "first step should continue from -500, got {first}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cosine_fade_eases_rather_than_stepping_evenly() {
    let console = MockConsole::start().await;
    let mixer = connect(&console).await;

    mixer
        .fade(
            Strip::Input,
            0,
            Fade::to(-10_00, Duration::from_millis(300))
                .from(0)
                .tick(Duration::from_millis(30))
                .curve(Curve::Cosine),
        )
        .await
        .unwrap();

    let levels: Vec<i32> = console
        .commands_excluding_keepalive()
        .iter()
        .filter_map(|c| c.strip_prefix("set MIXER:Current/InCh/Fader/Level 0 0 "))
        .map(|v| v.parse().unwrap())
        .collect();

    // An eased fade starts slower than a linear one covering the same span.
    let first_step = levels[0].abs();
    let linear_step = 10_00 / levels.len() as i32;
    assert!(
        first_step < linear_step,
        "cosine should ease in: first step {first_step} vs linear {linear_step}"
    );
    assert_eq!(*levels.last().unwrap(), -10_00);
}

/// Out-of-range indices are caught against the parameter table, so they cost neither a round trip
/// nor an ERROR reply mid-cue.
#[tokio::test(flavor = "multi_thread")]
async fn out_of_range_index_never_reaches_the_console() {
    let console = MockConsole::start().await;
    let mixer = connect(&console).await;

    let err = mixer.set_fader_db(Strip::Dca, 8, -6.0).await.unwrap_err();
    assert!(matches!(err, Error::IndexOutOfRange { .. }), "got {err:?}");
    assert!(
        !err.is_transient(),
        "a bad index will not fix itself on retry"
    );
    assert!(
        console.commands_excluding_keepalive().is_empty(),
        "nothing should have been sent"
    );

    mixer.set_fader_db(Strip::Dca, 7, -6.0).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn console_errors_surface_as_errors() {
    let console = MockConsole::builder()
        .error_on("MIXER:Current/InCh/Fader/Level")
        .start()
        .await;
    let mixer = connect(&console).await;

    let err = mixer.set_fader_db(Strip::Input, 0, -6.0).await.unwrap_err();
    assert!(matches!(err, Error::Console(_)), "got {err:?}");
    assert!(!err.is_transient());
}

#[tokio::test(flavor = "multi_thread")]
async fn labels_with_spaces_survive_the_round_trip() {
    let console = MockConsole::start().await;
    let mixer = connect(&console).await;

    mixer.set_label(Strip::Input, 4, "Lead  Vox").await.unwrap();
    assert_eq!(mixer.label(Strip::Input, 4).await.unwrap(), "Lead  Vox");
}

#[tokio::test(flavor = "multi_thread")]
async fn over_long_labels_are_truncated_instead_of_rejected() {
    let console = MockConsole::start().await;
    let mixer = connect(&console).await;

    let long = "x".repeat(200);
    mixer.set_label(Strip::Input, 0, &long).await.unwrap();
    assert_eq!(mixer.label(Strip::Input, 0).await.unwrap().len(), 64);
}

#[tokio::test(flavor = "multi_thread")]
async fn colours_round_trip_through_the_quoted_wire_format() {
    let console = MockConsole::start().await;
    let mixer = connect(&console).await;

    for colour in LabelColor::ALL {
        mixer.set_color(Strip::Input, 0, colour).await.unwrap();
        assert_eq!(mixer.color(Strip::Input, 0).await.unwrap(), colour);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn scene_recall_uses_the_consoles_bank_names() {
    let console = MockConsole::start().await;
    let mixer = connect(&console).await;

    mixer
        .recall_scene(yamaha_rcp::SceneList::A, 12)
        .await
        .unwrap();
    assert_eq!(
        console.commands_excluding_keepalive(),
        ["ssrecall_ex scene_a 12"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sends_address_the_right_destination_bus() {
    let console = MockConsole::start().await;
    let mixer = connect(&console).await;

    mixer.set_send_db(Strip::Input, 3, 5, -12.0).await.unwrap();
    assert_eq!(
        console
            .value("MIXER:Current/InCh/ToMix/Level", 3, 5)
            .as_deref(),
        Some("-1200")
    );

    mixer.set_send_on(Strip::Mix, 1, 2, true).await.unwrap();
    assert_eq!(
        console
            .value("MIXER:Current/Mix/ToMtrx/On", 1, 2)
            .as_deref(),
        Some("1")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn mute_masters_are_addressable() {
    let console = MockConsole::start().await;
    let mixer = connect(&console).await;

    mixer.set_mute_master(2, true).await.unwrap();
    assert_eq!(
        console
            .value("MIXER:Current/MuteMaster/On", 2, 0)
            .as_deref(),
        Some("1")
    );
    assert!(mixer.mute_master(2).await.unwrap());

    assert!(
        mixer.set_mute_master(6, true).await.is_err(),
        "only 6 groups exist"
    );
}

/// The console absorbs roughly one command per 5ms. Offering commands faster than that must not
/// flood it — the writer paces, so a burst spreads out instead of arriving all at once.
#[tokio::test(flavor = "multi_thread")]
async fn bursts_are_paced_to_the_consoles_rate() {
    let console = MockConsole::start().await;
    let mixer = connect(&console).await.confirm_writes(false);

    let started = Instant::now();
    for i in 0..20 {
        mixer
            .set_fader_level(Strip::Input, 0, -i * 10)
            .await
            .unwrap();
    }
    eventually("all 20 commands to arrive", || {
        console.commands_excluding_keepalive().len() >= 20
    })
    .await;

    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(80),
        "20 commands at 5ms spacing should take ~95ms, took {elapsed:?}"
    );
    assert_eq!(
        console
            .commands_excluding_keepalive()
            .last()
            .map(String::as_str),
        Some("set MIXER:Current/InCh/Fader/Level 0 0 -190"),
        "pacing must not reorder or drop the final value"
    );
}

/// Fire-and-forget is the cue path: it must return without waiting for the console.
#[tokio::test(flavor = "multi_thread")]
async fn unconfirmed_writes_return_without_a_round_trip() {
    let console = MockConsole::builder().swallow_replies(5).start().await;
    let mixer = connect(&console).await.confirm_writes(false);

    let started = Instant::now();
    mixer.set_fader_db(Strip::Input, 0, -6.0).await.unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(50),
        "an unconfirmed write must not wait for a reply it will never get"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn connecting_to_a_closed_port_fails_fast_and_reports_it() {
    // Bind then drop, so the port is almost certainly free and refuses connections.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let started = Instant::now();
    let err = Mixer::connect_with(addr, test_config()).await.unwrap_err();

    assert!(err.is_transient(), "got {err:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
}
