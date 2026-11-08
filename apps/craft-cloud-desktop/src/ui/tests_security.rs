//! The Security page and the join's code field: every step against the fake agent (the model, effect by effect), the screens as
//! kittest draws them (snapshots `second-factor*` and `security-*` in `tests/snapshots/`, written with `UPDATE_SNAPSHOTS=1`),
//! and that nothing secret outlives the screen that showed it.

use std::path::PathBuf;

use craft_cloud_app::{Error, JoinRequest, RunState, Status};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

use super::*;
use crate::agent::fake::{CODE, FakeAgent, PASSWORD, RECOVERY};
use crate::agent::{Agent, Starter, call};
use crate::model::{Effect, JoinDefaults, Model, Screen};
use crate::security::{SecurityEvent, Step, TwoFactorInfo};

const NOW: i64 = 1_790_000_000_000;
const TOLERANCE: u8 = 48;
const MAX_DIFFERENT: usize = 4;

fn defaults() -> JoinDefaults {
    JoinDefaults { folder: "/home/fabio/Craft Cloud".into(), device_name: "lptp1".into() }
}

fn signed_in() -> Status {
    Status {
        api: 1,
        signed_in: true,
        account: Some("fabio".into()),
        server: Some("https://cloud.shadows.ch".into()),
        folder: Some("/home/fabio/Craft Cloud".into()),
        machine: Some("lptp1".into()),
        online: true,
        run_state: RunState::Idle,
        autostart: Some(true),
        last_sync_ms: Some(NOW - 120_000),
        removed: false,
        spaces: Vec::new(),
        from_folder: None,
    }
}

fn signed_out() -> Status {
    Status {
        signed_in: false,
        account: None,
        server: None,
        folder: None,
        machine: None,
        online: false,
        run_state: RunState::SignedOut,
        ..signed_in()
    }
}

fn no_start() -> Starter {
    std::sync::Arc::new(|_: &dyn Agent| Err("no agent in tests".to_owned()))
}

/// Feeds `event` to the model and answers every effect with the fake, as the link does, until nothing is left.
fn feed(model: &mut Model, agent: &FakeAgent, event: Event) {
    let start = no_start();
    let mut pending = model.update(event);
    while let Some(effect) = pending.pop() {
        if let Some(answer) = call(agent, &start, effect) {
            pending.extend(model.update(answer));
        }
    }
}

fn model_after(agent: &FakeAgent, events: Vec<Event>) -> Model {
    let mut model = Model::new(defaults(), true, "run.log".into());
    for event in std::iter::once(Event::Start).chain(events) {
        feed(&mut model, agent, event);
    }
    model
}

fn on_security(agent: &FakeAgent) -> Model {
    model_after(agent, vec![Event::OpenSecurity])
}

/// A folder that is a full path on every platform (the join form refuses anything else).
fn absolute_folder() -> String {
    std::env::temp_dir().to_string_lossy().into_owned()
}

fn page(model: &mut Model) -> &mut crate::security::SecurityPage {
    match &mut model.screen {
        Screen::Security(page) => page,
        other => panic!("not the Security page: {other:?}"),
    }
}

fn type_into(model: &mut Model, fill: impl FnOnce(&mut Step)) {
    fill(&mut page(model).step);
}

// ---- the model ----

#[test]
fn the_page_asks_the_server_and_shows_what_it_says() {
    let agent = FakeAgent::new(signed_in());
    let mut model = on_security(&agent);
    assert_eq!(
        page(&mut model).info,
        TwoFactorInfo::Known(craft_cloud_app::TwoFactorStatus { enabled: false, enabled_ms: None, recovery_codes_left: 0, pending: false })
    );
    assert!(agent.asked.lock().unwrap().iter().any(|a| a == "two_factor_status"));
    // A server from before two-factor sign-in: said so, the page doesn't break.
    let old = FakeAgent::new(signed_in());
    old.factor.lock().unwrap().unsupported = true;
    let mut model = on_security(&old);
    assert_eq!(page(&mut model).info, TwoFactorInfo::Unsupported);
    feed(&mut model, &old, Event::Security(SecurityEvent::StartSetup));
    assert_eq!(page(&mut model).step, Step::Home, "nothing to set up");
    // Back to the signed-in view.
    feed(&mut model, &old, Event::Security(SecurityEvent::Close));
    assert!(matches!(model.screen, Screen::SignedIn(_)));
}

#[test]
fn setting_it_up_turns_it_on_and_shows_the_recovery_codes_once() {
    let agent = FakeAgent::new(signed_in());
    let mut model = on_security(&agent);
    feed(&mut model, &agent, Event::Security(SecurityEvent::StartSetup));
    // Nothing typed: nothing is asked of the server.
    let asked = agent.asked.lock().unwrap().len();
    feed(&mut model, &agent, Event::Security(SecurityEvent::Submit));
    assert!(page(&mut model).error.is_some());
    assert_eq!(agent.asked.lock().unwrap().len(), asked);
    // A wrong password stays on the step, with the password gone from the field.
    type_into(&mut model, |step| {
        if let Step::SetupPassword { password } = step {
            *password = "wrong password!".into();
        }
    });
    feed(&mut model, &agent, Event::Security(SecurityEvent::Submit));
    assert_eq!(page(&mut model).error.as_deref(), Some("That password isn't right."));
    assert!(matches!(&page(&mut model).step, Step::SetupPassword { password } if password.is_empty()));
    type_into(&mut model, |step| {
        if let Step::SetupPassword { password } = step {
            *password = PASSWORD.into();
        }
    });
    feed(&mut model, &agent, Event::Security(SecurityEvent::Submit));
    let Step::SetupCode { secret, uri, .. } = &page(&mut model).step else { panic!("{:?}", page(&mut model).step) };
    assert_eq!(secret, "JBSWY3DPEHPK3PXP");
    assert!(uri.starts_with("otpauth://totp/"), "{uri}");
    // A code that isn't six digits is told so; a wrong one is refused by the server and the secret stays.
    type_into(&mut model, |step| {
        if let Step::SetupCode { code, .. } = step {
            *code = "12345".into();
        }
    });
    feed(&mut model, &agent, Event::Security(SecurityEvent::Submit));
    assert_eq!(page(&mut model).error.as_deref(), Some("Type the six digits the app shows."));
    type_into(&mut model, |step| {
        if let Step::SetupCode { code, .. } = step {
            *code = "654 321".into();
        }
    });
    feed(&mut model, &agent, Event::Security(SecurityEvent::Submit));
    assert!(page(&mut model).error.as_deref().is_some_and(|e| e.contains("isn't right")), "{:?}", page(&mut model).error);
    assert!(matches!(&page(&mut model).step, Step::SetupCode { .. }));
    type_into(&mut model, |step| {
        if let Step::SetupCode { code, .. } = step {
            *code = " 123 456 ".into();
        }
    });
    feed(&mut model, &agent, Event::Security(SecurityEvent::Submit));
    let Step::Codes { codes, .. } = &page(&mut model).step else { panic!("{:?}", page(&mut model).step) };
    assert_eq!(codes.len(), 10);
    assert!(page(&mut model).is_on(), "the status was asked again");
    // Done: the codes are dropped, and so is everything secret of the page.
    feed(&mut model, &agent, Event::Security(SecurityEvent::Home));
    assert_eq!(page(&mut model).step, Step::Home);
    let shown = format!("{:?}", model.screen);
    assert!(!shown.contains("AAAA-BBBB") && !shown.contains("JBSWY"), "{shown}");
}

#[test]
fn leaving_the_page_drops_the_recovery_codes() {
    let agent = FakeAgent::new(signed_in());
    agent.factor.lock().unwrap().enabled = true;
    let mut model = on_security(&agent);
    feed(&mut model, &agent, Event::Security(SecurityEvent::StartRegenerate));
    type_into(&mut model, |step| {
        if let Step::Regenerate { password, code } = step {
            *password = PASSWORD.into();
            *code = CODE.into();
        }
    });
    feed(&mut model, &agent, Event::Security(SecurityEvent::Submit));
    assert!(matches!(&page(&mut model).step, Step::Codes { .. }));
    feed(&mut model, &agent, Event::Security(SecurityEvent::Close));
    assert!(matches!(model.screen, Screen::SignedIn(_)));
    let shown = format!("{:?}", model.screen);
    assert!(!shown.contains("AAAA"), "{shown}");
    // Opened again, the page starts at its start with nothing shown.
    feed(&mut model, &agent, Event::OpenSecurity);
    assert_eq!(page(&mut model).step, Step::Home);
}

#[test]
fn turning_it_off_takes_the_password_and_a_code_or_a_recovery_code() {
    let agent = FakeAgent::new(signed_in());
    agent.factor.lock().unwrap().enabled = true;
    let mut model = on_security(&agent);
    feed(&mut model, &agent, Event::Security(SecurityEvent::StartDisable));
    let set = |model: &mut Model, pw: &str, code: &str| {
        type_into(model, |step| {
            if let Step::Disable { password, code: c, .. } = step {
                *password = pw.into();
                *c = code.into();
            }
        });
    };
    set(&mut model, PASSWORD, "12");
    feed(&mut model, &agent, Event::Security(SecurityEvent::Submit));
    assert_eq!(page(&mut model).error.as_deref(), Some("A code from the app is six digits."));
    set(&mut model, "wrong password!", CODE);
    feed(&mut model, &agent, Event::Security(SecurityEvent::Submit));
    assert_eq!(page(&mut model).error.as_deref(), Some("That password isn't right."));
    // A lock says how long to wait, and nothing changes.
    agent.factor.lock().unwrap().locked = true;
    set(&mut model, PASSWORD, CODE);
    feed(&mut model, &agent, Event::Security(SecurityEvent::Submit));
    assert_eq!(page(&mut model).error.as_deref(), Some("Too many wrong codes. Try again in 14 minutes."));
    assert!(agent.factor.lock().unwrap().enabled);
    agent.factor.lock().unwrap().locked = false;
    // A recovery code, once the field is switched.
    feed(&mut model, &agent, Event::Security(SecurityEvent::ToggleRecovery));
    set(&mut model, PASSWORD, RECOVERY);
    feed(&mut model, &agent, Event::Security(SecurityEvent::Submit));
    assert_eq!(page(&mut model).step, Step::Home);
    assert!(!page(&mut model).is_on());
    assert!(page(&mut model).note.as_deref().is_some_and(|n| n.contains("is off")));
    // Passwords never stay in the page.
    assert!(!format!("{:?}", model.screen).contains(PASSWORD));
}

#[test]
fn the_password_and_the_other_computers() {
    let agent = FakeAgent::new(signed_in());
    agent.factor.lock().unwrap().other_computers = 2;
    let mut model = on_security(&agent);
    let set = |model: &mut Model, cur: &str, new: &str, again: &str| {
        let fields = &mut page(model).password;
        fields.current = cur.into();
        fields.new = new.into();
        fields.again = again.into();
    };
    set(&mut model, PASSWORD, "a brand new password", "another one");
    feed(&mut model, &agent, Event::Security(SecurityEvent::SubmitPassword));
    assert_eq!(page(&mut model).error.as_deref(), Some("The two new passwords aren't the same."));
    set(&mut model, PASSWORD, "short", "short");
    feed(&mut model, &agent, Event::Security(SecurityEvent::SubmitPassword));
    assert_eq!(page(&mut model).error.as_deref(), Some("passwords are 8 to 1024 bytes"));
    set(&mut model, PASSWORD, "a brand new password", "a brand new password");
    feed(&mut model, &agent, Event::Security(SecurityEvent::SubmitPassword));
    assert!(page(&mut model).note.as_deref().is_some_and(|n| n.starts_with("Password changed")));
    feed(&mut model, &agent, Event::Security(SecurityEvent::StartSignOutOthers));
    feed(&mut model, &agent, Event::Security(SecurityEvent::Submit));
    assert_eq!(page(&mut model).note.as_deref(), Some("Signed 2 other computers out."));
    assert!(!format!("{:?}", model.screen).contains("brand new"));
}

#[test]
fn the_password_form_checks_what_was_typed_and_locks_while_a_call_is_under_way() {
    let agent = FakeAgent::new(signed_in());
    let mut model = on_security(&agent);
    // Nothing typed: nothing is sent, and the page says what is missing.
    assert!(model.update(Event::Security(SecurityEvent::SubmitPassword)).is_empty());
    assert_eq!(page(&mut model).error.as_deref(), Some("Type your current password and the new one."));
    let fields = &mut page(&mut model).password;
    fields.current = PASSWORD.into();
    fields.new = "a brand new password".into();
    fields.again = "a brand new password".into();
    let effects = model.update(Event::Security(SecurityEvent::SubmitPassword));
    assert!(matches!(effects.as_slice(), [Effect::Security(crate::security::SecurityCall::ChangePassword { .. })]), "{effects:?}");
    // The typed passwords moved into the call: the form is empty, and a second click while the call is under way sends nothing.
    assert_eq!(page(&mut model).password, crate::security::PasswordFields::default());
    assert!(page(&mut model).busy);
    page(&mut model).password.current = "x".into();
    assert!(model.update(Event::Security(SecurityEvent::SubmitPassword)).is_empty());
    assert!(!format!("{:?}", model.screen).contains("brand new"));
}

/// While the sign-out of the other computers is asked for, the password form is off: a submit would be a silent no-op and
/// starting the question clears what was typed.
#[test]
fn the_password_form_is_off_while_the_sign_out_of_the_others_is_asked() {
    let agent = FakeAgent::new(signed_in());
    let mut model = on_security(&agent);
    model.update(Event::Security(SecurityEvent::StartSignOutOthers));
    assert_eq!(page(&mut model).step, crate::security::Step::SignOutOthers);
    page(&mut model).password.current = "typed".into();
    let mut h = harness(model, egui::Theme::Light);
    h.run_steps(3);
    h.get_by_label("Change password").click();
    h.run_steps(2);
    assert!(h.state().1.is_empty(), "{:?}", h.state().1);
    assert_eq!(page(&mut h.state_mut().0).password.current, "typed");
}

#[test]
fn the_security_activity_lists_only_what_is_about_signing_in_and_the_account() {
    use crate::security::{SecurityActivity, SecurityAnswer, is_security_action};
    for action in ["auth.login", "auth.login_failed", "account.password_changed", "device.revoked_others", "device.revoked"] {
        assert!(is_security_action(action), "{action}");
    }
    for action in ["admin.quota_changed", "join.pairing_redeemed", "cli.password_reset", "admin.device_revoked"] {
        assert!(!is_security_action(action), "{action}");
    }
    let agent = FakeAgent::new(signed_in());
    let mut model = on_security(&agent);
    // The page asked for the account's activity when it opened.
    assert!(agent.asked.lock().unwrap().iter().any(|a| a == "account_activity"));
    let events = ["admin.quota_changed", "auth.login", "join.pairing_redeemed", "account.password_changed"]
        .iter()
        .enumerate()
        .map(|(n, action)| FakeAgent::sample_event(n as u64, action))
        .collect();
    model.update(Event::SecurityAnswer(SecurityAnswer::Activity(Ok(craft_cloud_app::AuditPage { events, next: None }))));
    let SecurityActivity::Loaded(shown) = &page(&mut model).activity else { panic!("not loaded") };
    assert_eq!(shown.iter().map(|e| e.action.as_str()).collect::<Vec<_>>(), ["auth.login", "account.password_changed"]);
    // A failed call is said, and nothing breaks.
    model.update(Event::SecurityAnswer(SecurityAnswer::Activity(Err(Error::Io("down".into())))));
    assert!(matches!(page(&mut model).activity, SecurityActivity::Failed(_)));
}

#[test]
fn a_join_for_an_account_with_two_factor_asks_for_the_code_and_keeps_the_invite() {
    let agent = FakeAgent::new(signed_out());
    let required = Error::RefusedWith {
        code: "two_factor_required".into(),
        message: "needs a code".into(),
        two_factor: Some(craft_cloud_app::TwoFactorChallenge { challenge: String::new(), expires_ms: 0 }),
        retry_after_secs: None,
    };
    agent.joins.lock().unwrap().push(Ok(signed_in()));
    agent.joins.lock().unwrap().push(Err(Error::Refused { code: "two_factor_invalid".into(), message: "wrong".into(), name_taken: None }));
    agent.joins.lock().unwrap().push(Err(required));
    let mut model = model_after(&agent, vec![]);
    let Screen::Join(form) = &mut model.screen else { panic!("{:?}", model.screen) };
    form.join = "cloud.shadows.ch/K7M4-Q9TX".into();
    form.mode = crate::model::JoinMode::JoinString;
    form.folder = absolute_folder();
    let mut sent = Vec::new();
    let start = no_start();
    let run = |model: &mut Model, event: Event, sent: &mut Vec<JoinRequest>| {
        let mut pending = model.update(event);
        while let Some(effect) = pending.pop() {
            if let Effect::Join(request) = &effect {
                sent.push(request.clone());
            }
            if let Some(answer) = call(&agent, &start, effect) {
                pending.extend(model.update(answer));
            }
        }
    };
    run(&mut model, Event::SubmitJoin, &mut sent);
    let Screen::SecondFactor(second) = &mut model.screen else { panic!("{:?}", model.screen) };
    assert_eq!(second.error, None);
    assert_eq!((sent[0].totp_code.clone(), sent[0].recovery_code.clone()), (None, None), "the first join carries no code");
    // Six digits are needed; the same join goes again with them.
    second.code = "12".into();
    run(&mut model, Event::SubmitSecondFactor, &mut sent);
    let Screen::SecondFactor(second) = &mut model.screen else { panic!("{:?}", model.screen) };
    assert_eq!(second.error.as_deref(), Some("Type the six digits the app shows."));
    assert_eq!(sent.len(), 1);
    second.code = "654 321".into();
    run(&mut model, Event::SubmitSecondFactor, &mut sent);
    assert_eq!(sent.len(), 2);
    assert_eq!(
        (sent[1].join.as_str(), sent[1].totp_code.as_deref(), sent[1].recovery_code.as_deref()),
        (sent[0].join.as_str(), Some("654321"), None)
    );
    // The server said it was wrong: back at the field, with the reason, the code cleared.
    let Screen::SecondFactor(second) = &mut model.screen else { panic!("{:?}", model.screen) };
    assert!(second.error.as_deref().is_some_and(|e| e.contains("isn't right")), "{:?}", second.error);
    assert!(second.code.is_empty());
    // A recovery code instead.
    second.recovery = true;
    second.code = RECOVERY.into();
    *agent.status.lock().unwrap() = signed_in();
    run(&mut model, Event::SubmitSecondFactor, &mut sent);
    assert_eq!(sent[2].recovery_code.as_deref(), Some(RECOVERY));
    assert!(matches!(model.screen, Screen::SignedIn(_)), "{:?}", model.screen);
    // Back from the field goes to the join form, which keeps what was typed.
    *agent.status.lock().unwrap() = signed_out();
    let mut again = model_after(&agent, vec![]);
    if let Screen::Join(form) = &mut again.screen {
        form.join = "cloud.shadows.ch/K7M4-Q9TX".into();
        form.mode = crate::model::JoinMode::JoinString;
        form.folder = absolute_folder();
    }
    agent.joins.lock().unwrap().push(Err(Error::RefusedWith {
        code: "two_factor_required".into(),
        message: "m".into(),
        two_factor: None,
        retry_after_secs: None,
    }));
    run(&mut again, Event::SubmitJoin, &mut sent);
    run(&mut again, Event::CancelSecondFactor, &mut sent);
    assert!(matches!(&again.screen, Screen::Join(form) if form.join == "cloud.shadows.ch/K7M4-Q9TX"));
}

// ---- the screens ----

fn harness(model: Model, theme: egui::Theme) -> Harness<'static, (Model, Vec<Event>)> {
    Harness::builder().with_size(egui::Vec2::from(WINDOW_SIZE)).with_theme(theme).wgpu_setup(super::tests::shared_wgpu()).build_ui_state(
        |ui, (model, events): &mut (Model, Vec<Event>)| {
            events.extend(show(ui, model, NOW));
        },
        (model, Vec::new()),
    )
}

fn snapshot(model: Model, name: &str) {
    let mut h = harness(model, egui::Theme::Light);
    h.run_steps(4);
    let image = h.render().unwrap();
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("snapshots");
    let path = dir.join(format!("{name}.png"));
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::create_dir_all(&dir).unwrap();
        image.save(&path).unwrap();
        return;
    }
    let new = dir.join(format!("{name}.new.png"));
    let Ok(old) = image::open(&path) else {
        image.save(&new).unwrap();
        panic!("no snapshot {} (UPDATE_SNAPSHOTS=1 writes it; this run's is {})", path.display(), new.display());
    };
    let old = old.into_rgba8();
    let same_size = old.dimensions() == image.dimensions();
    let different = old.pixels().zip(image.pixels()).filter(|(a, b)| a.0.iter().zip(b.0.iter()).any(|(x, y)| x.abs_diff(*y) > TOLERANCE)).count();
    let allowed = (image.width() * image.height()) as usize * MAX_DIFFERENT / 1000;
    if !same_size || different > allowed {
        image.save(&new).unwrap();
        panic!("{name}: {different} pixels differ (at most {allowed}); this run's is {}", new.display());
    }
    let _ = std::fs::remove_file(new);
}

#[test]
fn snapshot_second_factor() {
    let agent = FakeAgent::new(signed_out());
    agent.joins.lock().unwrap().push(Err(Error::RefusedWith {
        code: "two_factor_required".into(),
        message: "m".into(),
        two_factor: None,
        retry_after_secs: None,
    }));
    let mut model = model_after(&agent, vec![]);
    if let Screen::Join(form) = &mut model.screen {
        form.join = "cloud.shadows.ch/K7M4-Q9TX".into();
        form.mode = crate::model::JoinMode::JoinString;
        form.folder = absolute_folder();
    }
    feed(&mut model, &agent, Event::SubmitJoin);
    assert!(matches!(model.screen, Screen::SecondFactor(_)));
    snapshot(model, "second-factor");
}

#[test]
fn snapshot_second_factor_with_a_wrong_code() {
    let form = crate::security::SecondFactorForm::new(
        crate::model::JoinForm { join: "cloud.shadows.ch/K7M4-Q9TX".into(), folder: "/home/fabio/Craft Cloud".into(), ..Default::default() },
        None,
        Some("That code isn't right, or it was used already. Wait for the app's next code and try again.".into()),
    );
    let mut model = Model::new(defaults(), true, String::new());
    model.screen = Screen::SecondFactor(form);
    snapshot(model, "second-factor-wrong-code");
}

fn security_model(agent: &FakeAgent, events: Vec<Event>) -> Model {
    let mut model = on_security(agent);
    for event in events {
        feed(&mut model, agent, event);
    }
    model
}

#[test]
fn snapshot_security_off() {
    snapshot(security_model(&FakeAgent::new(signed_in()), vec![]), "security-off");
}

#[test]
fn snapshot_security_on() {
    let agent = FakeAgent::new(signed_in());
    agent.factor.lock().unwrap().enabled = true;
    snapshot(security_model(&agent, vec![]), "security-on");
}

#[test]
fn snapshot_security_unsupported() {
    let agent = FakeAgent::new(signed_in());
    agent.factor.lock().unwrap().unsupported = true;
    snapshot(security_model(&agent, vec![]), "security-unsupported");
}

#[test]
fn snapshot_security_setup_with_its_qr_code() {
    let agent = FakeAgent::new(signed_in());
    let mut model = security_model(&agent, vec![Event::Security(SecurityEvent::StartSetup)]);
    type_into(&mut model, |step| {
        if let Step::SetupPassword { password } = step {
            *password = PASSWORD.into();
        }
    });
    feed(&mut model, &agent, Event::Security(SecurityEvent::Submit));
    assert!(matches!(&page(&mut model).step, Step::SetupCode { .. }));
    snapshot(model, "security-setup");
}

#[test]
fn snapshot_security_recovery_codes() {
    let agent = FakeAgent::new(signed_in());
    agent.factor.lock().unwrap().enabled = true;
    let mut model = security_model(&agent, vec![Event::Security(SecurityEvent::StartRegenerate)]);
    type_into(&mut model, |step| {
        if let Step::Regenerate { password, code } = step {
            *password = PASSWORD.into();
            *code = CODE.into();
        }
    });
    feed(&mut model, &agent, Event::Security(SecurityEvent::Submit));
    assert!(matches!(&page(&mut model).step, Step::Codes { .. }));
    snapshot(model, "security-recovery-codes");
}

/// The one-time codes are on screen: the footer's Sign out and Quit are off, like the tabs, so they can't drop the codes.
#[test]
fn the_footer_is_off_while_the_recovery_codes_are_shown() {
    let agent = FakeAgent::new(signed_in());
    agent.factor.lock().unwrap().enabled = true;
    let mut model = security_model(&agent, vec![Event::Security(SecurityEvent::StartRegenerate)]);
    type_into(&mut model, |step| {
        if let Step::Regenerate { password, code } = step {
            *password = PASSWORD.into();
            *code = CODE.into();
        }
    });
    feed(&mut model, &agent, Event::Security(SecurityEvent::Submit));
    assert!(matches!(&page(&mut model).step, Step::Codes { .. }));
    let mut h = harness(model, egui::Theme::Light);
    h.run_steps(2);
    h.get_by_label("Sign out").click();
    h.get_by_label("Quit").click();
    h.run_steps(2);
    assert!(h.state().1.is_empty(), "{:?}", h.state().1);
}

#[test]
fn snapshot_security_turning_it_off_when_locked() {
    let agent = FakeAgent::new(signed_in());
    agent.factor.lock().unwrap().enabled = true;
    let mut model = security_model(&agent, vec![Event::Security(SecurityEvent::StartDisable)]);
    agent.factor.lock().unwrap().locked = true;
    type_into(&mut model, |step| {
        if let Step::Disable { password, code, .. } = step {
            *password = PASSWORD.into();
            *code = CODE.into();
        }
    });
    feed(&mut model, &agent, Event::Security(SecurityEvent::Submit));
    assert!(page(&mut model).error.is_some());
    snapshot(model, "security-disable-locked");
}

// ---- the buttons ----

#[test]
fn the_overview_has_a_security_button_and_the_page_its_buttons() {
    let model = model_after(&FakeAgent::new(signed_in()), vec![]);
    let mut h = harness(model, egui::Theme::Light);
    h.run_steps(2);
    h.get_by_label("Security").click();
    h.run_steps(2);
    assert!(h.state().1.contains(&Event::Tab(crate::model::Tab::Security)), "{:?}", h.state().1);

    let agent = FakeAgent::new(signed_in());
    let mut h = harness(security_model(&agent, vec![]), egui::Theme::Light);
    h.run_steps(2);
    h.get_by_label("Set up two-factor sign-in").click();
    h.run_steps(2);
    h.get_by_label("Change password").click();
    h.run_steps(2);
    // The Overview tab leaves the page; the page has no Back of its own.
    h.get_by_label("Overview").click();
    h.run_steps(2);
    let events = &h.state().1;
    assert!(events.contains(&Event::Security(SecurityEvent::StartSetup)), "{events:?}");
    assert!(events.contains(&Event::Security(SecurityEvent::SubmitPassword)), "{events:?}");
    assert!(events.contains(&Event::Tab(crate::model::Tab::Overview)), "{events:?}");
}

// ---- what egui remembers of what was typed ----

type H = Harness<'static, (Model, Vec<Event>)>;

/// Whether egui still has state for the field: and whether Undo could bring typed text back into an empty field.
fn remembered(h: &H, name: &str) -> (bool, bool) {
    use egui::text_edit::TextEditState;
    let state = TextEditState::load(&h.ctx, super::security::secret_id(name));
    let undoable = state.as_ref().is_some_and(|s| s.undoer().undo(&(egui::text::CCursorRange::default(), String::new())).is_some());
    (state.is_some(), undoable)
}

fn undo(h: &mut H, role: egui::accesskit::Role) {
    h.get_by_role(role).focus();
    h.run_steps(2);
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::Z);
    h.run_steps(3);
}

#[test]
fn a_typed_password_or_code_is_not_left_in_eguis_undo_history() {
    use egui::accesskit::Role;
    let agent = FakeAgent::new(signed_in());
    agent.factor.lock().unwrap().enabled = true;
    let model = security_model(&agent, vec![Event::Security(SecurityEvent::StartDisable)]);
    let mut h = harness(model, egui::Theme::Light);
    h.run_steps(3);
    let typed = |h: &H| match &h.state().0.screen {
        Screen::Security(page) => match &page.step {
            Step::Disable { password, code, .. } => (password.clone(), code.clone()),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };

    h.get_by_role(Role::PasswordInput).focus();
    h.get_by_role(Role::PasswordInput).type_text("hunter2-secret");
    h.run_steps(8);
    assert!(remembered(&h, "disable-password").1, "while typed, egui has the field's history (the test sees what it guards against)");
    h.get_by_role(Role::TextInput).focus();
    h.get_by_role(Role::TextInput).type_text("123456");
    h.run_steps(8);
    assert_eq!(typed(&h), ("hunter2-secret".into(), "123456".into()));
    assert!(remembered(&h, "disable-code").1, "and the code's");

    // Sent: the model moved the typed text into the call and cleared the fields.
    let effects = h.state_mut().0.update(Event::Security(SecurityEvent::Submit));
    assert!(matches!(effects.as_slice(), [Effect::Security(_)]), "{effects:?}");
    h.run_steps(2);
    assert!(!remembered(&h, "disable-password").1, "once sent: no history (a field still drawn has an empty state)");
    assert!(!remembered(&h, "disable-code").1, "once sent: no history (a field still drawn has an empty state)");
    let refused = Error::Refused { code: "wrong_password".into(), message: "no".into(), name_taken: None };
    h.state_mut().0.update(Event::SecurityAnswer(crate::security::SecurityAnswer::Disable(Err(refused))));
    h.run_steps(3);
    undo(&mut h, Role::PasswordInput);
    undo(&mut h, Role::TextInput);
    assert_eq!(typed(&h), (String::new(), String::new()), "Undo brings nothing back");

    // Typed again and cancelled, then the step opened again.
    h.get_by_role(Role::PasswordInput).focus();
    h.get_by_role(Role::PasswordInput).type_text("another-secret");
    h.run_steps(8);
    assert!(remembered(&h, "disable-password").1);
    h.state_mut().0.update(Event::Security(SecurityEvent::Home));
    h.run_steps(2);
    assert_eq!(remembered(&h, "disable-password"), (false, false), "once cancelled");
    h.state_mut().0.update(Event::Security(SecurityEvent::StartDisable));
    h.run_steps(3);
    undo(&mut h, Role::PasswordInput);
    assert_eq!(typed(&h).0, "");

    // Left with something typed: the same.
    h.get_by_role(Role::PasswordInput).focus();
    h.get_by_role(Role::PasswordInput).type_text("third-secret");
    h.run_steps(3);
    h.state_mut().0.update(Event::Security(SecurityEvent::Close));
    h.run_steps(2);
    assert_eq!(remembered(&h, "disable-password"), (false, false), "once the page is left");
}

#[test]
fn a_typed_join_code_is_not_left_in_eguis_undo_history() {
    use egui::accesskit::Role;
    let form = crate::security::SecondFactorForm::new(
        crate::model::JoinForm { join: "cloud.shadows.ch/K7M4-Q9TX".into(), folder: absolute_folder(), ..Default::default() },
        None,
        None,
    );
    let mut model = Model::new(defaults(), true, String::new());
    model.screen = Screen::SecondFactor(form);
    let mut h = harness(model, egui::Theme::Light);
    h.run_steps(3);
    h.get_by_role(Role::TextInput).focus();
    h.get_by_role(Role::TextInput).type_text("654321");
    h.run_steps(8);
    assert!(remembered(&h, "second-factor-code").1, "while typed, egui has its history");
    // Back to the join form, then the field again.
    h.state_mut().0.update(Event::CancelSecondFactor);
    h.run_steps(2);
    assert_eq!(remembered(&h, "second-factor-code"), (false, false), "once cancelled");
    let form = crate::security::SecondFactorForm::new(crate::model::JoinForm::default(), None, None);
    h.state_mut().0.screen = Screen::SecondFactor(form);
    h.run_steps(3);
    undo(&mut h, Role::TextInput);
    match &h.state().0.screen {
        Screen::SecondFactor(form) => assert_eq!(form.code, "", "Undo brings nothing back"),
        other => panic!("{other:?}"),
    }
    // A code sent (the model clears or leaves the screen): forgotten as well.
    h.get_by_role(Role::TextInput).type_text("111111");
    h.run_steps(8);
    h.state_mut().0.screen = Screen::Join(Default::default());
    h.run_steps(2);
    assert_eq!(remembered(&h, "second-factor-code"), (false, false), "once the screen is left");
}

#[test]
fn a_join_password_keeps_its_text_while_it_is_typed() {
    use egui::accesskit::Role;
    let form = crate::model::JoinForm { step: crate::model::JoinStep::Account, ..Default::default() };
    let mut model = Model::new(defaults(), true, String::new());
    model.screen = Screen::Join(form);
    let mut h = harness(model, egui::Theme::Light);
    h.run_steps(3);
    h.get_by_role(Role::PasswordInput).focus();
    h.get_by_role(Role::PasswordInput).type_text("hunter2-secret");
    h.run_steps(8);
    // Several more frames: `forget_secrets` runs every frame and must leave a field that holds typed text alone.
    h.run_steps(5);
    match &h.state().0.screen {
        Screen::Join(form) => assert_eq!(form.password, "hunter2-secret"),
        other => panic!("{other:?}"),
    }
    assert!(remembered(&h, super::join::PASSWORD_FIELD).0, "while typed, egui keeps the field's state");
}
