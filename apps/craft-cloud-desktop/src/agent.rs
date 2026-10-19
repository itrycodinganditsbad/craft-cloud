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

use craft_cloud_app::{Client, DeviceId, DeviceInfo, Error, JoinRequest, PairingInfo, PairingStatus, Status, Uuid};

use crate::model::{Effect, Event};

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
        // Not running is as good as stopped.
        Effect::QuitAgent => {
            let _ = agent.quit();
            Event::AgentQuit
        }
        Effect::OpenFolder(_) | Effect::QuitApp | Effect::QuitTray | Effect::AppAutostart(_) | Effect::PickFolder(_) => return None,
    })
}

#[cfg(test)]
pub(crate) mod fake {
    //! A fake agent: a signed-in or signed-out status, answers set per call, and a switch that makes it go away.

    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    pub struct FakeAgent {
        pub status: Mutex<Status>,
        pub gone: AtomicBool,
        /// What `join` answers, in turn (then the status).
        pub joins: Mutex<Vec<Result<Status, Error>>>,
        pub asked: Mutex<Vec<String>>,
    }

    impl FakeAgent {
        pub fn new(status: Status) -> Self {
            Self { status: Mutex::new(status), gone: AtomicBool::new(false), joins: Mutex::new(Vec::new()), asked: Mutex::new(Vec::new()) }
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
