//! The link to the agent: the [`Agent`] trait (one method per call of `craft-cloud-app` the app makes; the real one is
//! [`ClientAgent`], tests use a fake), and [`Link`], which carries out the model's agent calls off the UI thread.
//!
//! Every call runs on a thread of its own and its answer comes back as an [`Event`] on a channel, after which the
//! link wakes the UI (`wake`), so the UI thread never waits for the agent; a slow call (a join waits for the server)
//! holds up nothing else. A clock thread sends [`Event::Tick`] every 2 s, from which the model asks for the status.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use craft_cloud_app::{
    AccountInfo, AdminCode, AdminConfirm, AdminCreateUser, AdminCreatedUser, AdminDevices, AdminPasswordReset, AdminServerInfo, AdminUpdateUser,
    AdminUserInfo, AdminUserList, Client, DeviceId, DeviceInfo, Error, JoinRequest, PairingInfo, PairingStatus, RecoveryCodes, RevokedOthers, Status,
    TwoFactorProofRequest, TwoFactorSetupResponse, TwoFactorStatus, UserQuery, Uuid,
};

use crate::admin::{Action, AdminEvent, Outcome};
use crate::model::{Effect, Event};
use crate::security::{SecurityAnswer, SecurityCall};

/// How often the clock ticks (and the status is asked for).
pub const TICK: Duration = Duration::from_secs(2);

/// The calls of the agent's local API the app makes.
pub trait Agent: Send + Sync {
    /// Finds the running agent and asks its status; [`Error::NotRunning`] when none answers.
    fn connect(&self) -> Result<Status, Error>;
    fn status(&self) -> Result<Status, Error>;
    fn join(&self, request: &JoinRequest) -> Result<Status, Error>;
    fn logout(&self, local: bool) -> Result<Status, Error>;
    fn pairing(&self) -> Result<PairingInfo, Error>;
    fn pairing_state(&self, id: Uuid) -> Result<PairingStatus, Error>;
    fn cancel_pairing(&self, id: Uuid) -> Result<(), Error>;
    fn devices(&self) -> Result<Vec<DeviceInfo>, Error>;
    fn remove_device(&self, id: DeviceId) -> Result<Vec<DeviceInfo>, Error>;
    fn pause(&self) -> Result<Status, Error>;
    fn resume(&self) -> Result<Status, Error>;
    fn quit(&self) -> Result<Status, Error>;
    fn set_autostart(&self, on: bool) -> Result<Status, Error>;
    /// The Security page's calls: the account's two-factor sign-in, its password and its other computers.
    fn two_factor_status(&self) -> Result<TwoFactorStatus, Error>;
    fn two_factor_setup(&self, password: &str) -> Result<TwoFactorSetupResponse, Error>;
    fn two_factor_confirm(&self, totp_code: &str) -> Result<RecoveryCodes, Error>;
    fn two_factor_disable(&self, request: &TwoFactorProofRequest) -> Result<(), Error>;
    fn two_factor_recovery_codes(&self, password: &str, totp_code: &str) -> Result<RecoveryCodes, Error>;
    fn change_password(&self, current: &str, new: &str) -> Result<(), Error>;
    fn revoke_other_devices(&self) -> Result<RevokedOthers, Error>;
    // The admin calls: each asks the server, which decides every one.
    fn account(&self) -> Result<AccountInfo, Error>;
    fn admin_server(&self) -> Result<AdminServerInfo, Error>;
    fn admin_users(&self, query: &UserQuery) -> Result<AdminUserList, Error>;
    fn admin_devices(&self, user: Uuid) -> Result<AdminDevices, Error>;
    fn admin_create_user(&self, request: &AdminCreateUser) -> Result<AdminCreatedUser, Error>;
    fn admin_update_user(&self, id: Uuid, request: &AdminUpdateUser) -> Result<AdminUserInfo, Error>;
    fn admin_disable_user(&self, id: Uuid, confirm: &AdminConfirm) -> Result<(), Error>;
    fn admin_enable_user(&self, id: Uuid, confirm: &AdminConfirm) -> Result<(), Error>;
    fn admin_remove_user(&self, id: Uuid, confirm: &AdminConfirm) -> Result<(), Error>;
    fn admin_issue_invite(&self, id: Uuid, confirm: &AdminConfirm) -> Result<AdminCode, Error>;
    fn admin_revoke_invite(&self, id: Uuid, confirm: &AdminConfirm) -> Result<(), Error>;
    fn admin_issue_reset(&self, id: Uuid, request: &AdminPasswordReset) -> Result<AdminCode, Error>;
    fn admin_reset_two_factor(&self, id: Uuid, confirm: &AdminConfirm) -> Result<(), Error>;
    fn admin_revoke_device(&self, id: Uuid, device: DeviceId, confirm: &AdminConfirm) -> Result<(), Error>;
}

/// The agent through `craft_cloud_app::Client`. It finds the agent again whenever a call finds it gone (the agent
/// picks a new port at every start).
pub struct ClientAgent {
    /// The agent's data folder (`None`: found as apps find it, `$CRAFT_CLOUD_AGENT_DIR` or the per-user one).
    dir: Option<PathBuf>,
    client: Mutex<Option<Client>>,
}

impl ClientAgent {
    pub fn new(dir: Option<PathBuf>) -> Self {
        Self { dir, client: Mutex::new(None) }
    }

    fn found(&self) -> Result<Client, Error> {
        let mut slot = self.client.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(client) = slot.as_ref() {
            return Ok(client.clone());
        }
        let client = match &self.dir {
            Some(dir) => Client::connect_in(dir)?,
            None => Client::connect()?,
        };
        *slot = Some(client.clone());
        Ok(client)
    }

    /// Runs `call` against the agent; one that finds it gone forgets it, so the next call looks for it afresh.
    fn with<T>(&self, call: impl FnOnce(&Client) -> Result<T, Error>) -> Result<T, Error> {
        let client = self.found()?;
        let result = call(&client);
        if matches!(result, Err(Error::NotRunning(_) | Error::Io(_) | Error::Protocol(_))) {
            *self.client.lock().unwrap_or_else(PoisonError::into_inner) = None;
        }
        result
    }
}

impl Agent for ClientAgent {
    fn connect(&self) -> Result<Status, Error> {
        *self.client.lock().unwrap_or_else(PoisonError::into_inner) = None;
        self.with(Client::status)
    }
    fn status(&self) -> Result<Status, Error> {
        self.with(Client::status)
    }
    fn join(&self, request: &JoinRequest) -> Result<Status, Error> {
        self.with(|c| c.join(request))
    }
    fn logout(&self, local: bool) -> Result<Status, Error> {
        self.with(|c| c.logout(local))
    }
    fn pairing(&self) -> Result<PairingInfo, Error> {
        self.with(Client::pairing)
    }
    fn pairing_state(&self, id: Uuid) -> Result<PairingStatus, Error> {
        self.with(|c| c.pairing_state(id))
    }
    fn cancel_pairing(&self, id: Uuid) -> Result<(), Error> {
        self.with(|c| c.cancel_pairing(id))
    }
    fn devices(&self) -> Result<Vec<DeviceInfo>, Error> {
        self.with(Client::devices)
    }
    fn remove_device(&self, id: DeviceId) -> Result<Vec<DeviceInfo>, Error> {
        self.with(|c| c.remove_device(id))
    }
    fn pause(&self) -> Result<Status, Error> {
        self.with(Client::pause)
    }
    fn resume(&self) -> Result<Status, Error> {
        self.with(Client::resume)
    }
    fn quit(&self) -> Result<Status, Error> {
        self.with(Client::quit)
    }
    fn set_autostart(&self, on: bool) -> Result<Status, Error> {
        self.with(|c| c.set_autostart(on))
    }
    fn two_factor_status(&self) -> Result<TwoFactorStatus, Error> {
        self.with(Client::two_factor_status)
    }
    fn two_factor_setup(&self, password: &str) -> Result<TwoFactorSetupResponse, Error> {
        self.with(|c| c.two_factor_setup(password))
    }
    fn two_factor_confirm(&self, totp_code: &str) -> Result<RecoveryCodes, Error> {
        self.with(|c| c.two_factor_confirm(totp_code))
    }
    fn two_factor_disable(&self, request: &TwoFactorProofRequest) -> Result<(), Error> {
        self.with(|c| c.two_factor_disable(request))
    }
    fn two_factor_recovery_codes(&self, password: &str, totp_code: &str) -> Result<RecoveryCodes, Error> {
        self.with(|c| c.two_factor_recovery_codes(password, totp_code))
    }
    fn change_password(&self, current: &str, new: &str) -> Result<(), Error> {
        self.with(|c| c.change_password(current, new))
    }
    fn revoke_other_devices(&self) -> Result<RevokedOthers, Error> {
        self.with(Client::revoke_other_devices)
    }
    fn account(&self) -> Result<AccountInfo, Error> {
        self.with(Client::account)
    }
    fn admin_reset_two_factor(&self, id: Uuid, confirm: &AdminConfirm) -> Result<(), Error> {
        self.with(|c| c.admin_reset_two_factor(id, confirm))
    }
    fn admin_server(&self) -> Result<AdminServerInfo, Error> {
        self.with(Client::admin_server)
    }
    fn admin_users(&self, query: &UserQuery) -> Result<AdminUserList, Error> {
        self.with(|c| c.admin_users(query))
    }
    fn admin_devices(&self, user: Uuid) -> Result<AdminDevices, Error> {
        self.with(|c| c.admin_devices(user))
    }
    fn admin_create_user(&self, request: &AdminCreateUser) -> Result<AdminCreatedUser, Error> {
        self.with(|c| c.admin_create_user(request))
    }
    fn admin_update_user(&self, id: Uuid, request: &AdminUpdateUser) -> Result<AdminUserInfo, Error> {
        self.with(|c| c.admin_update_user(id, request))
    }
    fn admin_disable_user(&self, id: Uuid, confirm: &AdminConfirm) -> Result<(), Error> {
        self.with(|c| c.admin_disable_user(id, confirm))
    }
    fn admin_enable_user(&self, id: Uuid, confirm: &AdminConfirm) -> Result<(), Error> {
        self.with(|c| c.admin_enable_user(id, confirm))
    }
    fn admin_remove_user(&self, id: Uuid, confirm: &AdminConfirm) -> Result<(), Error> {
        self.with(|c| c.admin_remove_user(id, confirm))
    }
    fn admin_issue_invite(&self, id: Uuid, confirm: &AdminConfirm) -> Result<AdminCode, Error> {
        self.with(|c| c.admin_issue_invite(id, confirm))
    }
    fn admin_revoke_invite(&self, id: Uuid, confirm: &AdminConfirm) -> Result<(), Error> {
        self.with(|c| c.admin_revoke_invite(id, confirm))
    }
    fn admin_issue_reset(&self, id: Uuid, request: &AdminPasswordReset) -> Result<AdminCode, Error> {
        self.with(|c| c.admin_issue_reset(id, request))
    }
    fn admin_revoke_device(&self, id: Uuid, device: DeviceId, confirm: &AdminConfirm) -> Result<(), Error> {
        self.with(|c| c.admin_revoke_device(id, device, confirm))
    }
}

/// Starts the agent and waits for it ([`crate::start_agent::start_and_wait`]), for [`Effect::StartAgent`].
pub type Starter = Arc<dyn Fn(&dyn Agent) -> Result<Status, String> + Send + Sync>;

/// Carries out the model's agent calls on threads, and hands their answers back.
pub struct Link {
    agent: Arc<dyn Agent>,
    start: Starter,
    wake: Arc<dyn Fn() + Send + Sync>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
}

impl Link {
    /// A link to `agent`; `start` starts it when it doesn't run; `wake` is called after every answer (it asks the UI
    /// for a frame, also while the window is hidden).
    pub fn new(agent: Arc<dyn Agent>, start: Starter, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        let (tx, rx) = channel();
        Self { agent, start, wake, tx, rx }
    }

    /// A sender for events from elsewhere (the tray, a second start), which wakes the UI like an answer does.
    pub fn sender(&self) -> (Sender<Event>, Arc<dyn Fn() + Send + Sync>) {
        (self.tx.clone(), self.wake.clone())
    }

    /// Starts the clock: [`Event::Tick`] every [`TICK`], until the link is gone.
    pub fn start_clock(&self, now_ms: fn() -> i64) {
        let (tx, wake) = (self.tx.clone(), self.wake.clone());
        let _ = std::thread::Builder::new().name("craft-cloud-clock".into()).spawn(move || {
            while tx.send(Event::Tick { now_ms: now_ms() }).is_ok() {
                wake();
                std::thread::sleep(TICK);
            }
        });
    }

    /// The answers that came since the last look.
    pub fn events(&self) -> Vec<Event> {
        self.rx.try_iter().collect()
    }

    /// Starts `effect` if it is a call of the agent (its answer comes as an event); any other effect is handed back
    /// for the caller to carry out.
    pub fn perform(&self, effect: Effect) -> Option<Effect> {
        if !is_call(&effect) {
            return Some(effect);
        }
        let (agent, start, wake, tx) = (self.agent.clone(), self.start.clone(), self.wake.clone(), self.tx.clone());
        let spawned = std::thread::Builder::new().name("craft-cloud-call".into()).spawn(move || {
            if let Some(event) = call(agent.as_ref(), &start, effect) {
                let _ = tx.send(event);
                wake();
            }
        });
        if let Err(e) = spawned {
            // No thread to run it on: said as the call's failure.
            let _ = self.tx.send(Event::Polled(Err(Error::Io(format!("can't start a thread: {e}")))));
        }
        None
    }
}

/// Whether `effect` is a call of the agent.
fn is_call(effect: &Effect) -> bool {
    !matches!(effect, Effect::OpenFolder(_) | Effect::QuitApp | Effect::QuitTray | Effect::AppAutostart(_) | Effect::PickFolder(_))
}

/// Makes the call `effect` names, blocking, and gives its answer as the event the model expects.
pub fn call(agent: &dyn Agent, start: &Starter, effect: Effect) -> Option<Event> {
    Some(match effect {
        Effect::Connect => Event::Found(agent.connect()),
        Effect::StartAgent => Event::AgentStarted(start(agent)),
        Effect::Status => Event::Polled(agent.status()),
        Effect::Join(request) => Event::Joined(agent.join(&request)),
        Effect::SetAutostart(on) => Event::AutostartSet(agent.set_autostart(on)),
        Effect::Pairing => Event::PairingMade(agent.pairing()),
        Effect::PairingState(id) => Event::PairingPolled(agent.pairing_state(id)),
        // Nothing waits for a cancel: a code that stays open expires in ten minutes.
        Effect::CancelPairing(id) => {
            let _ = agent.cancel_pairing(id);
            return None;
        }
        Effect::Devices => Event::Devices(agent.devices()),
        Effect::RemoveDevice(id) => Event::Devices(agent.remove_device(id)),
        Effect::Logout { local } => Event::SignedOut(agent.logout(local)),
        Effect::Pause => Event::Answered(agent.pause()),
        Effect::Resume => Event::Answered(agent.resume()),
        Effect::Account { session } => Event::Admin(AdminEvent::Account { session, result: agent.account() }),
        Effect::AdminServer { session } => Event::Admin(AdminEvent::Server { session, result: agent.admin_server() }),
        Effect::AdminUsers { query, ticket } => Event::Admin(AdminEvent::Users { ticket, result: agent.admin_users(&query) }),
        Effect::AdminDevices { user, ticket } => {
            Event::Admin(AdminEvent::Devices { user, ticket, result: agent.admin_devices(user).map(|d| d.devices) })
        }
        Effect::AdminDo { action, confirm } => Event::Admin(AdminEvent::Done(change(agent, &action, confirm))),
        // Not running is as good as stopped.
        Effect::QuitAgent => {
            let _ = agent.quit();
            Event::AgentQuit
        }
        Effect::Security(call) => Event::SecurityAnswer(match call {
            SecurityCall::Status => SecurityAnswer::Status(agent.two_factor_status()),
            SecurityCall::Setup { password } => SecurityAnswer::Setup(agent.two_factor_setup(&password)),
            SecurityCall::Confirm { code } => SecurityAnswer::Confirm(agent.two_factor_confirm(&code)),
            SecurityCall::Disable(request) => SecurityAnswer::Disable(agent.two_factor_disable(&request)),
            SecurityCall::RecoveryCodes { password, code } => SecurityAnswer::RecoveryCodes(agent.two_factor_recovery_codes(&password, &code)),
            SecurityCall::ChangePassword { current, new } => SecurityAnswer::ChangePassword(agent.change_password(&current, &new)),
            SecurityCall::SignOutOthers => SecurityAnswer::SignOutOthers(agent.revoke_other_devices()),
        }),
        Effect::OpenFolder(_) | Effect::QuitApp | Effect::QuitTray | Effect::AppAutostart(_) | Effect::PickFolder(_) => return None,
    })
}

/// Makes the change `action` names with the admin's `confirm`ation, blocking. This is the one place where the
/// confirmation goes into a call's body (the create and change calls carry it in their own fields).
fn change(agent: &dyn Agent, action: &Action, confirm: AdminConfirm) -> Result<Outcome, Error> {
    let AdminConfirm { password, totp_code, recovery_code } = confirm.clone();
    match action {
        Action::CreateUser { username, role } => {
            let request = AdminCreateUser { username: username.clone(), role: role.as_str().to_owned(), password, totp_code, recovery_code };
            agent.admin_create_user(&request).map(Outcome::Created)
        }
        Action::UpdateUser { id, role, quota, .. } => {
            let request = AdminUpdateUser { role: role.map(|r| r.as_str().to_owned()), quota_bytes: *quota, password, totp_code, recovery_code };
            agent.admin_update_user(*id, &request).map(Outcome::Updated)
        }
        Action::Disable { id, .. } => agent.admin_disable_user(*id, &confirm).map(|()| Outcome::Done),
        Action::Enable { id, .. } => agent.admin_enable_user(*id, &confirm).map(|()| Outcome::Done),
        Action::Remove { id, .. } => agent.admin_remove_user(*id, &confirm).map(|()| Outcome::Done),
        Action::IssueInvite { id, .. } => agent.admin_issue_invite(*id, &confirm).map(Outcome::Code),
        Action::RevokeInvite { id, .. } => agent.admin_revoke_invite(*id, &confirm).map(|()| Outcome::Done),
        Action::IssueReset { id, reset_two_factor, .. } => {
            let request = AdminPasswordReset { password, totp_code, recovery_code, reset_two_factor: *reset_two_factor };
            agent.admin_issue_reset(*id, &request).map(Outcome::Code)
        }
        Action::ResetTwoFactor { id, .. } => agent.admin_reset_two_factor(*id, &confirm).map(|()| Outcome::Done),
        Action::RevokeDevice { id, device, .. } => agent.admin_revoke_device(*id, *device, &confirm).map(|()| Outcome::Done),
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! A fake agent: a signed-in or signed-out status, answers set per call, and a switch that makes it go away.

    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// The password, code and recovery code the fake's account takes.
    pub const PASSWORD: &str = "correct horse battery";
    pub const CODE: &str = "123456";
    pub const RECOVERY: &str = "ABCD-EFGH-JKMN-PQRS";

    /// What the fake agent's server has of the account's two-factor sign-in.
    #[derive(Debug, Clone, Default)]
    pub struct FakeFactor {
        pub enabled: bool,
        pub pending: bool,
        /// A server from before two-factor sign-in: its calls are `not_found`.
        pub unsupported: bool,
        /// Every check of a code is refused as locked (840 s to wait).
        pub locked: bool,
        pub other_computers: u32,
    }

    pub struct FakeAgent {
        pub status: Mutex<Status>,
        pub gone: AtomicBool,
        /// What `join` answers, in turn (then the status).
        pub joins: Mutex<Vec<Result<Status, Error>>>,
        pub asked: Mutex<Vec<String>>,
        pub factor: Mutex<FakeFactor>,
        /// Whether the account is an admin (else every admin call is refused as `forbidden`).
        pub admin: AtomicBool,
        /// The accounts the admin calls list.
        pub users: Mutex<Vec<AdminUserInfo>>,
        /// What the admin changes answer instead of succeeding, taken from the end.
        pub admin_fails: Mutex<Vec<Error>>,
        /// The password each admin change was sent with.
        pub passwords: Mutex<Vec<Option<String>>>,
        /// The code (`totp_code`, `recovery_code`) each admin change was sent with.
        pub factors: Mutex<Vec<(Option<String>, Option<String>)>>,
        /// Whether the admin account has two-factor sign-in (what `account` says).
        pub two_factor: AtomicBool,
        /// The `reset_two_factor` flag of each password reset code asked for.
        pub resets: Mutex<Vec<bool>>,
    }

    fn refused(code: &str, message: &str) -> Error {
        Error::Refused { code: code.into(), message: message.into() }
    }

    impl FakeAgent {
        pub fn new(status: Status) -> Self {
            Self {
                status: Mutex::new(status),
                gone: AtomicBool::new(false),
                joins: Mutex::new(Vec::new()),
                asked: Mutex::new(Vec::new()),
                factor: Mutex::new(FakeFactor::default()),
                admin: AtomicBool::new(false),
                users: Mutex::new(Vec::new()),
                admin_fails: Mutex::new(Vec::new()),
                passwords: Mutex::new(Vec::new()),
                factors: Mutex::new(Vec::new()),
                two_factor: AtomicBool::new(false),
                resets: Mutex::new(Vec::new()),
            }
        }

        /// A refusal or nothing: whether a call of the account's two-factor sign-in gets through at all.
        fn factor_gate(&self, what: &str) -> Result<(), Error> {
            self.answer(what, |_| ())?;
            if self.factor.lock().unwrap().unsupported {
                return Err(refused("not_found", "no such endpoint"));
            }
            Ok(())
        }

        /// A code is checked: locked, wrong or right.
        fn check_code(&self, code: Option<&str>) -> Result<(), Error> {
            if self.factor.lock().unwrap().locked {
                return Err(Error::RefusedWith {
                    code: "two_factor_locked".into(),
                    message: "locked".into(),
                    two_factor: None,
                    retry_after_secs: Some(840),
                });
            }
            if code == Some(CODE) || code == Some(RECOVERY) { Ok(()) } else { Err(refused("two_factor_invalid", "the code is wrong")) }
        }

        fn codes() -> RecoveryCodes {
            RecoveryCodes { recovery_codes: (0..10).map(|n| format!("AAAA-BBBB-CCCC-{n:04}")).collect() }
        }

        /// An admin call: refused as a member's is unless the account is an admin.
        fn admin_call<T>(&self, what: &str, ok: impl FnOnce() -> T) -> Result<T, Error> {
            self.asked.lock().unwrap().push(what.to_owned());
            if self.gone.load(Ordering::SeqCst) {
                return Err(Error::NotRunning("can't reach it: connection refused".into()));
            }
            if !self.admin.load(Ordering::SeqCst) {
                return Err(Error::Refused { code: "forbidden".into(), message: "this needs an administrator".into() });
            }
            Ok(ok())
        }

        /// An admin change: the password it was sent with is kept, and a failure set up is answered first.
        fn admin_change<T>(
            &self,
            what: &str,
            password: Option<String>,
            factor: (Option<String>, Option<String>),
            ok: impl FnOnce() -> T,
        ) -> Result<T, Error> {
            self.passwords.lock().unwrap().push(password);
            self.factors.lock().unwrap().push(factor);
            self.admin_call(what, || ())?;
            if let Some(e) = self.admin_fails.lock().unwrap().pop() {
                return Err(e);
            }
            Ok(ok())
        }

        fn user(&self, id: Uuid) -> AdminUserInfo {
            self.users.lock().unwrap().iter().find(|u| u.id == id).cloned().unwrap_or_else(|| sample_user("someone", id))
        }

        fn answer<T>(&self, what: &str, ok: impl FnOnce(&Status) -> T) -> Result<T, Error> {
            self.asked.lock().unwrap().push(what.to_owned());
            if self.gone.load(Ordering::SeqCst) {
                return Err(Error::NotRunning("can't reach it: connection refused".into()));
            }
            Ok(ok(&self.status.lock().unwrap()))
        }
    }

    impl Agent for FakeAgent {
        fn connect(&self) -> Result<Status, Error> {
            self.answer("connect", Status::clone)
        }
        fn status(&self) -> Result<Status, Error> {
            self.answer("status", Status::clone)
        }
        fn join(&self, _request: &JoinRequest) -> Result<Status, Error> {
            self.answer("join", |_| ())?;
            let next = self.joins.lock().unwrap().pop();
            next.unwrap_or_else(|| Ok(self.status.lock().unwrap().clone()))
        }
        fn logout(&self, _local: bool) -> Result<Status, Error> {
            self.answer("logout", Status::clone)
        }
        fn pairing(&self) -> Result<PairingInfo, Error> {
            self.answer("pairing", |_| PairingInfo {
                id: Uuid::nil(),
                join: "cloud.shadows.ch/K7M4-Q9TX".into(),
                link: "https://cloud.shadows.ch/join/K7M4-Q9TX".into(),
                expires_ms: 600_000,
            })
        }
        fn pairing_state(&self, _id: Uuid) -> Result<PairingStatus, Error> {
            self.answer("pairing_state", |_| PairingStatus { state: craft_cloud_app::PairingState::Waiting, device_name: None })
        }
        fn cancel_pairing(&self, _id: Uuid) -> Result<(), Error> {
            self.answer("cancel_pairing", |_| ())
        }
        fn devices(&self) -> Result<Vec<DeviceInfo>, Error> {
            self.answer("devices", |_| Vec::new())
        }
        fn remove_device(&self, _id: DeviceId) -> Result<Vec<DeviceInfo>, Error> {
            self.answer("remove_device", |_| Vec::new())
        }
        fn pause(&self) -> Result<Status, Error> {
            self.answer("pause", Status::clone)
        }
        fn resume(&self) -> Result<Status, Error> {
            self.answer("resume", Status::clone)
        }
        fn quit(&self) -> Result<Status, Error> {
            self.answer("quit", Status::clone)
        }
        fn set_autostart(&self, on: bool) -> Result<Status, Error> {
            self.answer("set_autostart", |s| Status { autostart: Some(on), ..s.clone() })
        }
        fn two_factor_status(&self) -> Result<TwoFactorStatus, Error> {
            self.factor_gate("two_factor_status")?;
            let f = self.factor.lock().unwrap();
            Ok(TwoFactorStatus {
                enabled: f.enabled,
                enabled_ms: f.enabled.then_some(1_000),
                recovery_codes_left: if f.enabled { 10 } else { 0 },
                pending: f.pending,
            })
        }
        fn two_factor_setup(&self, password: &str) -> Result<TwoFactorSetupResponse, Error> {
            self.factor_gate("two_factor_setup")?;
            if password != PASSWORD {
                return Err(refused("wrong_password", "wrong"));
            }
            let mut f = self.factor.lock().unwrap();
            if f.enabled {
                return Err(refused("conflict", "on already"));
            }
            f.pending = true;
            Ok(TwoFactorSetupResponse {
                secret: "JBSWY3DPEHPK3PXP".into(),
                otpauth_uri: "otpauth://totp/Craft%20Cloud:fabio?secret=JBSWY3DPEHPK3PXP&issuer=Craft%20Cloud".into(),
                expires_ms: 1_790_000_600_000,
            })
        }
        fn two_factor_confirm(&self, totp_code: &str) -> Result<RecoveryCodes, Error> {
            self.factor_gate("two_factor_confirm")?;
            if !self.factor.lock().unwrap().pending {
                return Err(refused("conflict", "no set-up to confirm"));
            }
            if totp_code != CODE {
                return Err(refused("two_factor_invalid", "the code is wrong"));
            }
            let mut f = self.factor.lock().unwrap();
            f.enabled = true;
            f.pending = false;
            Ok(Self::codes())
        }
        fn two_factor_disable(&self, request: &TwoFactorProofRequest) -> Result<(), Error> {
            self.factor_gate("two_factor_disable")?;
            if request.password != PASSWORD {
                return Err(refused("wrong_password", "wrong"));
            }
            self.check_code(request.totp_code.as_deref().or(request.recovery_code.as_deref()))?;
            self.factor.lock().unwrap().enabled = false;
            Ok(())
        }
        fn two_factor_recovery_codes(&self, password: &str, totp_code: &str) -> Result<RecoveryCodes, Error> {
            self.factor_gate("two_factor_recovery_codes")?;
            if password != PASSWORD {
                return Err(refused("wrong_password", "wrong"));
            }
            self.check_code(Some(totp_code))?;
            Ok(Self::codes())
        }
        fn change_password(&self, current: &str, new: &str) -> Result<(), Error> {
            self.answer("change_password", |_| ())?;
            if current != PASSWORD {
                return Err(refused("wrong_password", "wrong"));
            }
            if new.len() < 8 {
                return Err(refused("weak_password", "passwords are 8 to 1024 bytes"));
            }
            Ok(())
        }
        fn revoke_other_devices(&self) -> Result<RevokedOthers, Error> {
            self.answer("revoke_other_devices", |_| ())?;
            let mut f = self.factor.lock().unwrap();
            let revoked = std::mem::take(&mut f.other_computers);
            Ok(RevokedOthers { revoked })
        }
        fn account(&self) -> Result<AccountInfo, Error> {
            self.answer("account", |s| AccountInfo {
                user_id: Uuid::nil(),
                username: s.account.clone().unwrap_or_default(),
                role: if self.admin.load(Ordering::SeqCst) { "admin" } else { "member" }.into(),
                created_ms: 1_780_000_000_000,
                password_changed_ms: None,
                two_factor_enabled: self.two_factor.load(Ordering::SeqCst),
            })
        }
        fn admin_reset_two_factor(&self, _id: Uuid, confirm: &AdminConfirm) -> Result<(), Error> {
            self.admin_change("admin_reset_two_factor", confirm.password.clone(), (confirm.totp_code.clone(), confirm.recovery_code.clone()), || ())
        }
        fn admin_server(&self) -> Result<AdminServerInfo, Error> {
            self.admin_call("admin_server", || AdminServerInfo {
                version: "0.1.0".into(),
                users: self.users.lock().unwrap().len() as u64,
                devices_active: 3,
                stored_bytes: 5_000_000_000,
                live_bytes: 120_000,
                storage_kind: "disk".into(),
                database_kind: "sqlite".into(),
            })
        }
        fn admin_users(&self, query: &UserQuery) -> Result<AdminUserList, Error> {
            self.admin_call("admin_users", || {
                let needle = query.query.as_deref().unwrap_or_default().to_lowercase();
                let all: Vec<AdminUserInfo> =
                    self.users.lock().unwrap().iter().filter(|u| u.username.to_lowercase().contains(&needle)).cloned().collect();
                // The cursor is the index of the page's first account.
                let from = query.cursor.as_deref().and_then(|c| c.parse::<usize>().ok()).unwrap_or(0);
                let limit = query.limit.unwrap_or(50) as usize;
                let users: Vec<AdminUserInfo> = all.iter().skip(from).take(limit).cloned().collect();
                let next = (from + limit < all.len()).then(|| (from + limit).to_string());
                AdminUserList { users, next }
            })
        }
        fn admin_devices(&self, user: Uuid) -> Result<AdminDevices, Error> {
            self.admin_call("admin_devices", || AdminDevices {
                devices: vec![DeviceInfo {
                    id: DeviceId::generate(),
                    name: format!("{} laptop", self.user(user).username),
                    created_ms: 0,
                    last_used_ms: 1_789_990_000_000,
                    revoked: false,
                    current: false,
                    client_kind: "agent".into(),
                    platform: "linux".into(),
                    app_version: "0.1.0".into(),
                    revoked_ms: None,
                    revoke_reason: None,
                }],
            })
        }
        fn admin_create_user(&self, request: &AdminCreateUser) -> Result<AdminCreatedUser, Error> {
            self.admin_change("admin_create_user", request.password.clone(), (request.totp_code.clone(), request.recovery_code.clone()), || {
                AdminCreatedUser {
                    user: AdminUserInfo { role: request.role.clone(), invite_pending: true, ..sample_user(&request.username, Uuid::now_v7()) },
                    invite: AdminCode {
                        code: "K7M4Q9TXK7M4Q9TX".into(),
                        join_url: Some("cloud.shadows.ch/K7M4-Q9TX-K7M4-Q9TX".into()),
                        expires_ms: 1_790_600_000_000,
                    },
                }
            })
        }
        fn admin_update_user(&self, id: Uuid, request: &AdminUpdateUser) -> Result<AdminUserInfo, Error> {
            self.admin_change("admin_update_user", request.password.clone(), (request.totp_code.clone(), request.recovery_code.clone()), || {
                let mut user = self.user(id);
                if let Some(role) = &request.role {
                    user.role = role.clone();
                }
                if let Some(quota) = request.quota_bytes {
                    user.quota_bytes = quota;
                }
                user
            })
        }
        fn admin_disable_user(&self, _id: Uuid, confirm: &AdminConfirm) -> Result<(), Error> {
            self.admin_change("admin_disable_user", confirm.password.clone(), (confirm.totp_code.clone(), confirm.recovery_code.clone()), || ())
        }
        fn admin_enable_user(&self, _id: Uuid, confirm: &AdminConfirm) -> Result<(), Error> {
            self.admin_change("admin_enable_user", confirm.password.clone(), (confirm.totp_code.clone(), confirm.recovery_code.clone()), || ())
        }
        fn admin_remove_user(&self, _id: Uuid, confirm: &AdminConfirm) -> Result<(), Error> {
            self.admin_change("admin_remove_user", confirm.password.clone(), (confirm.totp_code.clone(), confirm.recovery_code.clone()), || ())
        }
        fn admin_issue_invite(&self, _id: Uuid, confirm: &AdminConfirm) -> Result<AdminCode, Error> {
            self.admin_change("admin_issue_invite", confirm.password.clone(), (confirm.totp_code.clone(), confirm.recovery_code.clone()), || {
                AdminCode {
                    code: "ABCDEFGHJKMNPQRS".into(),
                    join_url: Some("cloud.shadows.ch/ABCD-EFGH-JKMN-PQRS".into()),
                    expires_ms: 1_790_600_000_000,
                }
            })
        }
        fn admin_revoke_invite(&self, _id: Uuid, confirm: &AdminConfirm) -> Result<(), Error> {
            self.admin_change("admin_revoke_invite", confirm.password.clone(), (confirm.totp_code.clone(), confirm.recovery_code.clone()), || ())
        }
        fn admin_issue_reset(&self, _id: Uuid, request: &AdminPasswordReset) -> Result<AdminCode, Error> {
            self.resets.lock().unwrap().push(request.reset_two_factor);
            self.admin_change("admin_issue_reset", request.password.clone(), (request.totp_code.clone(), request.recovery_code.clone()), || {
                AdminCode { code: "TUVWXYZ234567ABC".into(), join_url: None, expires_ms: 1_790_003_600_000 }
            })
        }
        fn admin_revoke_device(&self, _id: Uuid, _device: DeviceId, confirm: &AdminConfirm) -> Result<(), Error> {
            self.admin_change("admin_revoke_device", confirm.password.clone(), (confirm.totp_code.clone(), confirm.recovery_code.clone()), || ())
        }
    }

    /// An enabled member account that stores nothing.
    pub fn sample_user(username: &str, id: Uuid) -> AdminUserInfo {
        AdminUserInfo {
            id,
            username: username.to_owned(),
            role: "member".into(),
            created_ms: 1_780_000_000_000,
            disabled_ms: None,
            two_factor_enabled: false,
            devices_active: 1,
            stored_bytes: 0,
            quota_bytes: None,
            invite_pending: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeAgent;
    use super::*;
    use crate::model::{Model, Screen};
    use std::sync::atomic::Ordering;

    fn signed_in() -> Status {
        crate::model::tests::status(true)
    }

    fn no_start() -> Starter {
        Arc::new(|_: &dyn Agent| Err("not started in tests".to_owned()))
    }

    /// Runs the model against the fake until no call is left: each effect's answer is fed back, as the link does.
    fn settle(model: &mut Model, agent: &FakeAgent, first: Vec<Effect>) {
        let start = no_start();
        let mut pending = first;
        while let Some(effect) = pending.pop() {
            if let Some(event) = call(agent, &start, effect) {
                pending.extend(model.update(event));
            }
        }
    }

    /// Review focus: a fake agent that goes away mid-session. The next poll shows it, never a hang or a panic; once
    /// it is back, the next poll shows the signed-in view again.
    #[test]
    fn an_agent_that_goes_away_mid_session_is_shown_at_the_next_poll() {
        let agent = FakeAgent::new(signed_in());
        let mut model = Model::new(crate::model::JoinDefaults::default(), true, "run.log".into());
        let first = model.update(Event::Start);
        settle(&mut model, &agent, first);
        assert!(matches!(model.screen, Screen::SignedIn(_)), "{:?}", model.screen);
        agent.gone.store(true, Ordering::SeqCst);
        let tick = model.update(Event::Tick { now_ms: 2_000 });
        settle(&mut model, &agent, tick);
        let Screen::NotRunning(problem) = &model.screen else { panic!("{:?}", model.screen) };
        assert_eq!(problem.line, crate::model::STOPPED);
        agent.gone.store(false, Ordering::SeqCst);
        let tick = model.update(Event::Tick { now_ms: 4_000 });
        settle(&mut model, &agent, tick);
        assert!(matches!(model.screen, Screen::SignedIn(_)), "{:?}", model.screen);
    }

    /// The link runs a call on a thread, wakes the UI, and hands the answer over; other effects come back at once.
    #[test]
    fn the_link_answers_on_its_channel_and_wakes_the_ui() {
        let agent = Arc::new(FakeAgent::new(signed_in()));
        let (woke_tx, woke) = channel();
        let wake = Arc::new(move || {
            let _ = woke_tx.send(());
        });
        let link = Link::new(agent.clone(), no_start(), wake);
        assert_eq!(link.perform(Effect::QuitApp), Some(Effect::QuitApp));
        assert_eq!(link.perform(Effect::Status), None);
        woke.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(link.events(), vec![Event::Polled(Ok(signed_in()))]);
        assert_eq!(agent.asked.lock().unwrap().as_slice(), ["status"]);
    }
}
