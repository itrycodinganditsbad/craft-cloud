//! The Security page and the second step of a join: the signed-in account's two-factor sign-in (status, setting it up with a
//! secret and a QR code, the recovery codes shown once, turning it off, new recovery codes), the password change and signing
//! the other computers out; and the code field a join asks for when the account has two-factor sign-in.
//!
//! Pure like the rest of the model: [`SecurityPage::update`] takes an event and returns the calls to make, answers come back
//! as [`SecurityAnswer`]s. **Nothing secret is kept longer than it is shown**: the password a form held is moved into its call
//! and cleared, the secret and the recovery codes live in the page's step only, and leaving the page (or a signed-out or
//! removed screen) drops them. Every type that holds one hides it in `Debug`.

use craft_cloud_app::{
    AuditEvent, AuditPage, AuditQuery, Error, RecoveryCodes, RevokedOthers, TwoFactorProofRequest, TwoFactorSetupResponse, TwoFactorStatus, codes,
};
use craft_cloud_proto::api::codes as api_codes;

use crate::model::Overview;

/// The server doesn't know two-factor sign-in (it is older than this app), or the agent doesn't pass it on.
pub const UNSUPPORTED: &str = "This server doesn't support two-factor sign-in yet";

/// What the page knows of the account's two-factor sign-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TwoFactorInfo {
    /// Asking the server.
    Loading,
    /// The server doesn't have it ([`UNSUPPORTED`]).
    Unsupported,
    Known(TwoFactorStatus),
    /// The status couldn't be read (the message).
    Failed(String),
}

/// Why recovery codes are shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodesWhy {
    /// Two-factor sign-in was just turned on.
    TurnedOn,
    /// New codes were made; the old ones don't work.
    New,
}

/// What the page is doing.
#[derive(Clone, PartialEq, Eq)]
pub enum Step {
    /// The status and the actions.
    Home,
    /// Set-up, first: the password.
    SetupPassword { password: String },
    /// Set-up, second: the secret to put into the authenticator app, and the first code it shows.
    SetupCode { secret: String, uri: String, expires_ms: i64, code: String },
    /// The recovery codes, shown once.
    Codes { codes: Vec<String>, why: CodesWhy },
    /// Turning it off: the password, and a code or a recovery code.
    Disable { password: String, code: String, recovery: bool },
    /// New recovery codes: the password and a code.
    Regenerate { password: String, code: String },
    /// Signing the other computers out: asked once more.
    SignOutOthers,
}

impl std::fmt::Debug for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Which step, never what was typed or shown in it.
        f.write_str(match self {
            Step::Home => "Home",
            Step::SetupPassword { .. } => "SetupPassword(<redacted>)",
            Step::SetupCode { .. } => "SetupCode(<redacted>)",
            Step::Codes { .. } => "Codes(<redacted>)",
            Step::Disable { .. } => "Disable(<redacted>)",
            Step::Regenerate { .. } => "Regenerate(<redacted>)",
            Step::SignOutOthers => "SignOutOthers",
        })
    }
}

/// The three fields of the password form in the page's Password cell: the current password, the new one twice. Nothing of
/// what is typed shows in `Debug`.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct PasswordFields {
    pub current: String,
    pub new: String,
    pub again: String,
}

impl std::fmt::Debug for PasswordFields {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PasswordFields(<redacted>)")
    }
}

impl PasswordFields {
    fn clear(&mut self) {
        *self = Self::default();
    }
}

/// How many of the newest security events the page lists.
pub const SECURITY_ACTIVITY_ROWS: usize = 5;

/// How many of the account's newest events are asked for, to find [`SECURITY_ACTIVITY_ROWS`] security-relevant ones among
/// them (the account's activity is capped at 100 a page).
const ACTIVITY_ASKED: u32 = 100;

/// Whether an event of the account's activity is about signing in or the account's security: sign-ins (and the refused
/// ones), the password, two-factor sign-in and its recovery codes, and computers signed out.
pub fn is_security_action(action: &str) -> bool {
    action.starts_with("auth.") || action.starts_with("account.") || matches!(action, "device.revoked" | "device.revoked_others")
}

/// The page's list of the account's newest security events.
#[derive(Debug, Clone, PartialEq)]
pub enum SecurityActivity {
    /// Asked for, not answered yet.
    Loading,
    /// The newest security events, newest first (at most [`SECURITY_ACTIVITY_ROWS`]).
    Loaded(Vec<AuditEvent>),
    /// The server couldn't tell (the message).
    Failed(String),
}

/// The Security page.
#[derive(Debug, Clone, PartialEq)]
pub struct SecurityPage {
    /// The signed-in view the page was opened from and returns to.
    pub overview: Overview,
    pub info: TwoFactorInfo,
    pub step: Step,
    /// What the password form of the Password cell holds.
    pub password: PasswordFields,
    /// The newest security events of the account.
    pub activity: SecurityActivity,
    /// What went wrong with the last try, for people.
    pub error: Option<String>,
    /// What the last action did ("Password changed").
    pub note: Option<String>,
    /// A call is under way: nothing can be sent twice.
    pub busy: bool,
}

/// What a button of the page asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityEvent {
    /// Back to the signed-in view; whatever the page showed is dropped.
    Close,
    /// Back to the page's start (the recovery codes shown are dropped).
    Home,
    StartSetup,
    StartDisable,
    StartRegenerate,
    StartSignOutOthers,
    /// The Password cell's button: change the password.
    SubmitPassword,
    /// The current step's button: continue, turn on, turn off, make, change, sign out.
    Submit,
    /// Disable's switch between a code and a recovery code.
    ToggleRecovery,
    /// Ask the server for the status again.
    Refresh,
}

/// A call of the agent the page makes.
#[derive(Clone, PartialEq, Eq)]
pub enum SecurityCall {
    Status,
    Setup {
        password: String,
    },
    Confirm {
        code: String,
    },
    Disable(TwoFactorProofRequest),
    RecoveryCodes {
        password: String,
        code: String,
    },
    ChangePassword {
        current: String,
        new: String,
    },
    SignOutOthers,
    /// The account's newest events, for the page's security activity.
    Activity,
}

impl std::fmt::Debug for SecurityCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SecurityCall::Status => "Status",
            SecurityCall::Setup { .. } => "Setup(<redacted>)",
            SecurityCall::Confirm { .. } => "Confirm(<redacted>)",
            SecurityCall::Disable(_) => "Disable(<redacted>)",
            SecurityCall::RecoveryCodes { .. } => "RecoveryCodes(<redacted>)",
            SecurityCall::ChangePassword { .. } => "ChangePassword(<redacted>)",
            SecurityCall::SignOutOthers => "SignOutOthers",
            SecurityCall::Activity => "Activity",
        })
    }
}

/// An agent's answer to a [`SecurityCall`].
#[derive(Clone, PartialEq)]
pub enum SecurityAnswer {
    Status(Result<TwoFactorStatus, Error>),
    Setup(Result<TwoFactorSetupResponse, Error>),
    Confirm(Result<RecoveryCodes, Error>),
    Disable(Result<(), Error>),
    RecoveryCodes(Result<RecoveryCodes, Error>),
    ChangePassword(Result<(), Error>),
    SignOutOthers(Result<RevokedOthers, Error>),
    Activity(Result<AuditPage, Error>),
}

impl std::fmt::Debug for SecurityAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // An answer may hold a secret or recovery codes: only whether it worked.
        let (name, ok) = match self {
            SecurityAnswer::Status(r) => ("Status", r.is_ok()),
            SecurityAnswer::Setup(r) => ("Setup", r.is_ok()),
            SecurityAnswer::Confirm(r) => ("Confirm", r.is_ok()),
            SecurityAnswer::Disable(r) => ("Disable", r.is_ok()),
            SecurityAnswer::RecoveryCodes(r) => ("RecoveryCodes", r.is_ok()),
            SecurityAnswer::ChangePassword(r) => ("ChangePassword", r.is_ok()),
            SecurityAnswer::SignOutOthers(r) => ("SignOutOthers", r.is_ok()),
            SecurityAnswer::Activity(r) => ("Activity", r.is_ok()),
        };
        write!(f, "{name}({})", if ok { "ok" } else { "refused" })
    }
}

/// Six digits (spaces allowed), the shape of a code from the authenticator app.
pub fn is_code(text: &str) -> bool {
    let digits: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_digit())
}

pub fn digits(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// What the page asks of the account's activity.
pub fn activity_query() -> AuditQuery {
    AuditQuery { limit: Some(ACTIVITY_ASKED), ..AuditQuery::default() }
}

/// A wait in words.
fn wait_in_words(secs: u64) -> String {
    match secs {
        0..=90 => format!("{secs} seconds"),
        91..=5_400 => format!("{} minutes", secs.div_ceil(60)),
        _ => format!("{} hours", secs.div_ceil(3_600)),
    }
}

/// What a refusal says, for people, and what to do about it.
pub fn explain(e: &Error) -> String {
    let wait = e.retry_after_secs().map(wait_in_words);
    match e.code() {
        Some(codes::WRONG_PASSWORD) => "That password isn't right.".into(),
        Some(codes::TWO_FACTOR_INVALID) => "That code isn't right, or it was used already. Wait for the app's next code and try again.".into(),
        Some(codes::TWO_FACTOR_LOCKED) => match wait {
            Some(wait) => format!("Too many wrong codes. Try again in {wait}."),
            None => "Too many wrong codes. Try again later.".into(),
        },
        Some(codes::TWO_FACTOR_UNAVAILABLE) => "This server can't set up two-factor sign-in yet: its admin has to give it a key first.".into(),
        Some(codes::TWO_FACTOR_PENDING) => "Setting it up is open on another computer. Finish it there, or wait ten minutes.".into(),
        Some(codes::OFFLINE) => "Can't reach the server. Try again when this computer is online.".into(),
        Some(craft_cloud_app::codes::NEEDS_YOU) => "This computer can't use the account any more. Sign out, then join again.".into(),
        Some("rate_limited") => match wait {
            Some(wait) => format!("Too many tries. Try again in {wait}."),
            None => "Too many tries. Try again in a few minutes.".into(),
        },
        _ => match e {
            Error::Refused { message, .. } | Error::RefusedWith { message, .. } => message.clone(),
            other => other.to_string(),
        },
    }
}

impl SecurityPage {
    /// The page, opened from `overview`; its first call asks the server for the status.
    pub fn open(overview: Overview) -> (Self, Vec<SecurityCall>) {
        (
            Self {
                overview,
                info: TwoFactorInfo::Loading,
                step: Step::Home,
                password: PasswordFields::default(),
                activity: SecurityActivity::Loading,
                error: None,
                note: None,
                busy: false,
            },
            vec![SecurityCall::Status, SecurityCall::Activity],
        )
    }

    /// Whether two-factor sign-in is known to be on.
    pub fn is_on(&self) -> bool {
        matches!(&self.info, TwoFactorInfo::Known(s) if s.enabled)
    }

    fn start(&mut self, step: Step) {
        self.step = step;
        // A password typed in the Password cell is dropped with the page's start over.
        self.password.clear();
        self.error = None;
        self.note = None;
    }

    /// A button of the page. Returns the calls to make; the page is `busy` while one is under way.
    pub fn update(&mut self, event: SecurityEvent) -> Vec<SecurityCall> {
        match event {
            SecurityEvent::Close => Vec::new(),
            SecurityEvent::Home => {
                if !self.busy {
                    self.start(Step::Home);
                }
                Vec::new()
            }
            SecurityEvent::Refresh => {
                self.info = TwoFactorInfo::Loading;
                vec![SecurityCall::Status]
            }
            SecurityEvent::StartSetup => {
                if !self.busy && !self.is_on() && matches!(self.info, TwoFactorInfo::Known(_)) {
                    self.start(Step::SetupPassword { password: String::new() });
                }
                Vec::new()
            }
            SecurityEvent::StartDisable => {
                if !self.busy && self.is_on() {
                    self.start(Step::Disable { password: String::new(), code: String::new(), recovery: false });
                }
                Vec::new()
            }
            SecurityEvent::StartRegenerate => {
                if !self.busy && self.is_on() {
                    self.start(Step::Regenerate { password: String::new(), code: String::new() });
                }
                Vec::new()
            }
            SecurityEvent::StartSignOutOthers => {
                if !self.busy {
                    self.start(Step::SignOutOthers);
                }
                Vec::new()
            }
            SecurityEvent::ToggleRecovery => {
                if let Step::Disable { code, recovery, .. } = &mut self.step {
                    *recovery = !*recovery;
                    code.clear();
                    self.error = None;
                }
                Vec::new()
            }
            SecurityEvent::SubmitPassword => self.submit_password(),
            SecurityEvent::Submit => self.submit(),
        }
    }

    /// The current step's call. A step that isn't filled in says what is missing and calls nothing; the typed password moves into
    /// the call and is cleared from the form.
    fn submit(&mut self) -> Vec<SecurityCall> {
        if self.busy {
            return Vec::new();
        }
        self.error = None;
        let call = match &mut self.step {
            Step::SetupPassword { password } => {
                if password.is_empty() {
                    self.error = Some("Type your password to start.".into());
                    return Vec::new();
                }
                SecurityCall::Setup { password: std::mem::take(password) }
            }
            Step::SetupCode { code, .. } => {
                if !is_code(code) {
                    self.error = Some("Type the six digits the app shows.".into());
                    return Vec::new();
                }
                SecurityCall::Confirm { code: digits(&std::mem::take(code)) }
            }
            Step::Disable { password, code, recovery } => {
                if password.is_empty() || code.trim().is_empty() {
                    self.error = Some("Type your password and a code (or a recovery code).".into());
                    return Vec::new();
                }
                if !*recovery && !is_code(code) {
                    self.error = Some("A code from the app is six digits.".into());
                    return Vec::new();
                }
                let typed = std::mem::take(code);
                let password = std::mem::take(password);
                if *recovery {
                    SecurityCall::Disable(TwoFactorProofRequest { password, totp_code: None, recovery_code: Some(typed.trim().to_owned()) })
                } else {
                    SecurityCall::Disable(TwoFactorProofRequest { password, totp_code: Some(digits(&typed)), recovery_code: None })
                }
            }
            Step::Regenerate { password, code } => {
                if password.is_empty() || !is_code(code) {
                    self.error = Some("Type your password and the six digits the app shows.".into());
                    return Vec::new();
                }
                SecurityCall::RecoveryCodes { password: std::mem::take(password), code: digits(&std::mem::take(code)) }
            }
            Step::SignOutOthers => SecurityCall::SignOutOthers,
            Step::Home | Step::Codes { .. } => return Vec::new(),
        };
        self.busy = true;
        vec![call]
    }

    /// The Password cell's call. A form that isn't filled in says what is missing and calls nothing; the typed passwords move
    /// into the call and are cleared from the form (the repeat is dropped).
    fn submit_password(&mut self) -> Vec<SecurityCall> {
        if self.busy || !matches!(self.step, Step::Home) {
            return Vec::new();
        }
        self.error = None;
        self.note = None;
        let PasswordFields { current, new, again } = &mut self.password;
        if current.is_empty() || new.is_empty() {
            self.error = Some("Type your current password and the new one.".into());
            return Vec::new();
        }
        if new != again {
            self.error = Some("The two new passwords aren't the same.".into());
            return Vec::new();
        }
        let call = SecurityCall::ChangePassword { current: std::mem::take(current), new: std::mem::take(new) };
        again.clear();
        self.busy = true;
        vec![call]
    }

    /// An answer. May ask for more (the status, after a change).
    pub fn answer(&mut self, answer: SecurityAnswer, now_ms: i64) -> Vec<SecurityCall> {
        match answer {
            SecurityAnswer::Status(result) => {
                self.info = match result {
                    Ok(status) => TwoFactorInfo::Known(status),
                    Err(e) if e.code() == Some(api_codes::NOT_FOUND) || e.code() == Some(codes::AGENT_TOO_OLD) => TwoFactorInfo::Unsupported,
                    Err(e) => TwoFactorInfo::Failed(explain(&e)),
                };
                Vec::new()
            }
            SecurityAnswer::Setup(result) => {
                self.busy = false;
                match result {
                    Ok(setup) => {
                        self.step =
                            Step::SetupCode { secret: setup.secret, uri: setup.otpauth_uri, expires_ms: setup.expires_ms, code: String::new() };
                    }
                    Err(e) => self.error = Some(explain(&e)),
                }
                Vec::new()
            }
            SecurityAnswer::Confirm(result) => {
                self.busy = false;
                match result {
                    Ok(codes) => {
                        self.step = Step::Codes { codes: codes.recovery_codes, why: CodesWhy::TurnedOn };
                        self.note = Some("Two-factor sign-in is on. Your other computers were signed out.".into());
                        return vec![SecurityCall::Status, SecurityCall::Activity];
                    }
                    Err(e) => {
                        // A set-up that ran out or was used up (five wrong codes) has to start again.
                        if e.code() == Some(codes::CONFLICT) {
                            self.step = Step::Home;
                        }
                        self.error = Some(explain(&e));
                        // Past its ten minutes: the secret is of no use.
                        if matches!(&self.step, Step::SetupCode { expires_ms, .. } if now_ms >= *expires_ms) {
                            self.step = Step::Home;
                        }
                    }
                }
                Vec::new()
            }
            SecurityAnswer::Disable(result) => {
                self.busy = false;
                match result {
                    Ok(()) => {
                        self.step = Step::Home;
                        self.note = Some("Two-factor sign-in is off. Your other computers were signed out.".into());
                        return vec![SecurityCall::Status, SecurityCall::Activity];
                    }
                    Err(e) => self.error = Some(explain(&e)),
                }
                Vec::new()
            }
            SecurityAnswer::RecoveryCodes(result) => {
                self.busy = false;
                match result {
                    Ok(codes) => {
                        self.step = Step::Codes { codes: codes.recovery_codes, why: CodesWhy::New };
                        self.note = Some("New recovery codes. The old ones don't work any more.".into());
                        return vec![SecurityCall::Status, SecurityCall::Activity];
                    }
                    Err(e) => self.error = Some(explain(&e)),
                }
                Vec::new()
            }
            SecurityAnswer::ChangePassword(result) => {
                self.busy = false;
                match result {
                    Ok(()) => {
                        self.step = Step::Home;
                        self.note = Some("Password changed. Your other computers were signed out.".into());
                        return vec![SecurityCall::Activity];
                    }
                    Err(e) => self.error = Some(explain(&e)),
                }
                Vec::new()
            }
            SecurityAnswer::SignOutOthers(result) => {
                self.busy = false;
                match result {
                    Ok(done) => {
                        self.step = Step::Home;
                        self.note = Some(match done.revoked {
                            0 => "No other computer was signed in.".to_owned(),
                            1 => "Signed 1 other computer out.".to_owned(),
                            n => format!("Signed {n} other computers out."),
                        });
                        return vec![SecurityCall::Activity];
                    }
                    Err(e) => self.error = Some(explain(&e)),
                }
                Vec::new()
            }
            SecurityAnswer::Activity(result) => {
                self.activity = match result {
                    Ok(page) => SecurityActivity::Loaded(
                        page.events.into_iter().filter(|e| is_security_action(&e.action)).take(SECURITY_ACTIVITY_ROWS).collect(),
                    ),
                    Err(e) => SecurityActivity::Failed(explain(&e)),
                };
                Vec::new()
            }
        }
    }
}

/// The second step of a join: the account has two-factor sign-in, so the code comes with the same join.
#[derive(Clone, PartialEq, Eq)]
pub struct SecondFactorForm {
    pub form: crate::model::JoinForm,
    /// The new account's password, when the join came by an invite that needed one (an account that has two-factor sign-in has
    /// one already, so this is only ever `None` for it).
    pub password: Option<crate::model::PasswordForm>,
    /// What was typed: a code from the app, or a recovery code.
    pub code: String,
    /// The field takes a recovery code instead.
    pub recovery: bool,
    /// What went wrong with the last try, for people.
    pub error: Option<String>,
}

impl std::fmt::Debug for SecondFactorForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecondFactorForm")
            .field("form", &self.form)
            .field("recovery", &self.recovery)
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl SecondFactorForm {
    pub fn new(form: crate::model::JoinForm, password: Option<crate::model::PasswordForm>, error: Option<String>) -> Self {
        Self { form, password, code: String::new(), recovery: false, error }
    }

    /// The code as the join takes it: `(totp_code, recovery_code)`, or what is wrong with what was typed.
    pub fn factor(&self) -> Result<(Option<String>, Option<String>), String> {
        if self.recovery {
            let typed = self.code.trim();
            if typed.is_empty() {
                return Err("Type a recovery code.".into());
            }
            return Ok((None, Some(typed.to_owned())));
        }
        if !is_code(&self.code) {
            return Err("Type the six digits the app shows.".into());
        }
        Ok((Some(digits(&self.code)), None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_is_six_digits_with_spaces_allowed() {
        assert!(is_code("123456") && is_code(" 123 456 "));
        for bad in ["12345", "1234567", "12345a", "", "١٢٣٤٥٦"] {
            assert!(!is_code(bad), "{bad:?}");
        }
    }

    #[test]
    fn refusals_are_explained_for_people() {
        let refused = |code: &str| Error::Refused { code: code.into(), message: "m".into(), name_taken: None };
        assert!(explain(&refused("wrong_password")).contains("password isn't right"));
        assert!(explain(&refused("two_factor_invalid")).contains("next code"));
        let locked = Error::RefusedWith { code: "two_factor_locked".into(), message: "m".into(), two_factor: None, retry_after_secs: Some(840) };
        assert_eq!(explain(&locked), "Too many wrong codes. Try again in 14 minutes.");
        assert!(explain(&refused("two_factor_unavailable")).contains("admin has to give it a key"));
        assert_eq!(explain(&refused("weak_password")), "m", "an unknown code says what the server said");
    }

    #[test]
    fn secrets_stay_out_of_debug_output() {
        let step = Step::SetupCode { secret: "JBSWY3DPEHPK3PXP".into(), uri: "otpauth://totp/x".into(), expires_ms: 1, code: "918273".into() };
        let codes = Step::Codes { codes: vec!["ABCD-EFGH-JKMN-PQRS".into()], why: CodesWhy::TurnedOn };
        let password = Step::Disable { password: "pw-secret-1".into(), code: "918273".into(), recovery: false };
        for shown in [format!("{step:?}"), format!("{codes:?}"), format!("{password:?}")] {
            assert!(!shown.contains("JBSWY") && !shown.contains("ABCD") && !shown.contains("pw-secret") && !shown.contains("918273"), "{shown}");
        }
        let call = SecurityCall::Setup { password: "pw-secret-1".into() };
        assert!(!format!("{call:?}").contains("pw-secret"));
        let answer = SecurityAnswer::Confirm(Ok(RecoveryCodes { recovery_codes: vec!["ABCD-EFGH-JKMN-PQRS".into()] }));
        assert!(!format!("{answer:?}").contains("ABCD"));
        let form = SecondFactorForm { form: Default::default(), password: None, code: "918273".into(), recovery: false, error: None };
        assert!(!format!("{form:?}").contains("918273"));
    }
}
