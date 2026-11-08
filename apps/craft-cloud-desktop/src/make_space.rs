//! "Make a team space from this folder": the form on the home page's "On this computer" list and the notice of how the agent's
//! work goes, as a pure function of what happened, like [`crate::spaces`] (which it does not depend on; the model hosts both).
//!
//! - **Where it is offered**: on a row of the folder list (a top-level folder of the personal folder), to an administrator only,
//!   because the server lets only administrators create team spaces (the form asks for the administrator's password, and the code
//!   of two-factor sign-in). Anyone else sees nothing, and the server would refuse the call all the same.
//! - **What the form says**: the files are copied into a new team space, nothing travels again (the team space takes them from the
//!   personal space on the server), and the old folder is deleted here only after the team space is confirmed to hold every file;
//!   it then goes to the trash on the server, where it can be restored.
//! - **The work is the agent's**: the call answers when the agent has made the space and started; the rest shows in the agent's
//!   status ([`Status::from_folder`]), which the app polls. The notice is drawn from that status alone, so it shows after the app
//!   was closed and opened again, and after the agent was restarted (the agent reports interrupted work as such).
//! - **Failures are not final**: a failed or interrupted run says where it stopped and that the old folder was not changed. "Try
//!   again" asks for the same folder and name; the agent goes on with the same team space and needs no password.
//! - **Too old**: an agent without the call is `agent_too_old`, a server without team spaces `server_too_old`; the form says so in
//!   one sentence and offers nothing else.
//! - **Answers** carry the number of the call they answer; an answer for another call is dropped.

use std::path::Path;

use craft_cloud_app::{Error, FromFolderRequest, FromFolderStatus, FromFolderStep, SpaceConfirm};

use crate::admin::{Access, Confirmation, error_text, fresh};
use crate::model::{Effect, Model, Overview};
use crate::spaces::is_old;

/// What the form tells the person, in one paragraph.
pub const EXPLAIN: &str = "The files in this folder are copied into a new team space. They don't travel again: the team space takes them from your own \
     cloud space. When the team space has every file, checked one by one, the old folder is deleted from this computer. It then \
     sits in the trash on the server, where you can restore it. Until then it stays as it is.";

/// What the form says about who does this.
pub const ADMIN_NOTE: &str =
    "Making a team space is an administrator's change: it asks for your password. You become its owner and can invite the others afterwards.";

/// The most characters of a team space's name (the server's limit).
pub const MAX_NAME_CHARS: usize = craft_cloud_proto::MAX_SPACE_NAME_CHARS;

/// The form that is open: which folder, what the space is called, the administrator's confirmation.
#[derive(Debug, Clone, PartialEq)]
pub struct MakeSpace {
    /// The folder's name in the personal folder.
    pub folder: String,
    /// The folder's full path, as the agent is asked for it.
    pub path: String,
    /// The space's name, typed (it starts as the folder's).
    pub name: String,
    pub confirmation: Confirmation,
    /// Whether the account has two-factor sign-in, so that the confirmation asks for a code too.
    pub needs_code: bool,
    /// Asking again after a run that stopped with its space made: no confirmation is needed, and the name is the space's.
    pub resume: bool,
    /// The call is under way.
    pub busy: bool,
    /// Why the last try did not start.
    pub error: Option<String>,
    /// The agent or the server is too old for this: nothing else is offered.
    pub old: bool,
    /// Which answer is the current one.
    pub ticket: u64,
}

impl MakeSpace {
    /// Whether enough was typed to ask: a name, and the confirmation (not when going on with a space that was made).
    pub fn ready(&self) -> bool {
        !self.busy && !self.old && !self.name.trim().is_empty() && (self.resume || self.confirmation.ready(self.needs_code))
    }
}

/// A call to the agent ([`Effect::MakeSpace`]).
#[derive(Clone, PartialEq)]
pub enum MakeSpaceCall {
    Start { request: FromFolderRequest, ticket: u64 },
}

impl std::fmt::Debug for MakeSpaceCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The request hides the confirmation in its own `Debug`.
            Self::Start { request, ticket } => f.debug_struct("Start").field("request", request).field("ticket", ticket).finish(),
        }
    }
}

/// What happened on the form or in its call.
#[derive(Debug, Clone, PartialEq)]
pub enum MakeSpaceMsg {
    /// The answer of [`MakeSpaceCall::Start`].
    Started {
        ticket: u64,
        result: Result<FromFolderStatus, Error>,
    },
    /// "Make a team space…" on a folder's row (the folder's name in the personal folder).
    Open(String),
    /// "Try again" on a notice of work that stopped: the same folder and name.
    Retry,
    Cancel,
    /// The form's button.
    Submit,
    /// "Dismiss" on a notice of work that ended.
    Dismiss,
}

/// How the notice about the agent's work looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Working,
    Good,
    Bad,
}

/// The notice about the agent's work, drawn from its status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub tone: Tone,
    pub title: String,
    pub text: String,
    /// The number of files done and all of them, for the progress bar of a step that counts.
    pub progress: Option<(u64, u64)>,
    /// "Try again" is offered.
    pub retry: bool,
    /// "Dismiss" is offered.
    pub dismiss: bool,
    /// "Open the team space" is offered.
    pub open_shared: bool,
}

/// Which notice has been dismissed: the folder and the step it was at, so a notice about newer work is shown again.
pub fn dismissal_key(status: &FromFolderStatus) -> String {
    format!("{}|{}|{:?}", status.folder, status.name, status.step)
}

/// The notice for the agent's work, `None` when there is none or it was dismissed.
pub fn notice(status: &FromFolderStatus, dismissed: Option<&str>) -> Option<Notice> {
    if dismissed == Some(dismissal_key(status).as_str()) && !status.running() {
        return None;
    }
    let name = &status.name;
    let folder = Path::new(&status.folder).file_name().map_or_else(|| status.folder.clone(), |n| n.to_string_lossy().into_owned());
    let counted = || Some((status.done, status.total));
    let notice = match status.step {
        FromFolderStep::Checking | FromFolderStep::Creating => Notice {
            tone: Tone::Working,
            title: format!("Making the team space “{name}”"),
            text: "Asking the server for the new team space…".into(),
            progress: None,
            retry: false,
            dismiss: false,
            open_shared: false,
        },
        FromFolderStep::SettingUp => Notice {
            tone: Tone::Working,
            title: format!("Making the team space “{name}”"),
            text: format!("Setting up its folder beside your own. “{folder}” has not been changed."),
            progress: None,
            retry: false,
            dismiss: false,
            open_shared: false,
        },
        FromFolderStep::Copying => Notice {
            tone: Tone::Working,
            title: format!("Making the team space “{name}”"),
            text: format!("Copying the files of “{folder}”. The old folder has not been changed."),
            progress: counted(),
            retry: false,
            dismiss: false,
            open_shared: false,
        },
        FromFolderStep::Syncing => Notice {
            tone: Tone::Working,
            title: format!("Making the team space “{name}”"),
            text: format!("Waiting until the team space has every file. “{folder}” stays as it is until then."),
            progress: counted(),
            retry: false,
            dismiss: false,
            open_shared: false,
        },
        FromFolderStep::Verifying => Notice {
            tone: Tone::Working,
            title: format!("Making the team space “{name}”"),
            text: format!("Checking every file of “{folder}” against the team space before anything is deleted."),
            progress: counted(),
            retry: false,
            dismiss: false,
            open_shared: false,
        },
        FromFolderStep::Trashing => Notice {
            tone: Tone::Working,
            title: format!("Making the team space “{name}”"),
            text: format!("Every file is in the team space. Deleting “{folder}” from this computer…"),
            progress: None,
            retry: false,
            dismiss: false,
            open_shared: false,
        },
        FromFolderStep::Done => {
            let mut text = format!(
                "“{name}” is a team space now, and its files are in the folder beside your own. “{folder}” was deleted from this computer \
                 and is in the trash on the server, where you can restore it."
            );
            if status.kept > 0 {
                text = format!(
                    "“{name}” is a team space now. {} file{} stayed in “{folder}”: changed after the copy, or of a kind that doesn't sync. \
                     The rest is in the trash on the server.",
                    status.kept,
                    if status.kept == 1 { "" } else { "s" }
                );
            }
            Notice { tone: Tone::Good, title: "The team space is ready".into(), text, progress: None, retry: false, dismiss: true, open_shared: true }
        }
        FromFolderStep::Failed | FromFolderStep::Interrupted => {
            let at = status.failed_at.map_or("somewhere", step_word);
            let why = sentence(status.message.as_deref().unwrap_or_default());
            let lead = if status.step == FromFolderStep::Interrupted { "Stopped while" } else { "Couldn't finish while" };
            let space = if status.space.is_some() {
                format!(" The team space “{name}” was made and stays; trying again goes on with it.")
            } else {
                String::new()
            };
            let kept = if status.failed_at == Some(FromFolderStep::Trashing) {
                " Every file is in the team space.".to_owned()
            } else {
                format!(" “{folder}” was not changed.")
            };
            Notice {
                tone: Tone::Bad,
                title: format!("The team space “{name}” isn't finished"),
                text: format!("{lead} {at}. {why}{kept}{space}").replace("  ", " "),
                progress: None,
                retry: status.space.is_some() || status.step == FromFolderStep::Interrupted,
                dismiss: true,
                open_shared: false,
            }
        }
    };
    Some(notice)
}

/// `text` as a sentence of its own: it starts with a capital letter and ends with a full stop (empty stays empty). The agent's
/// messages are fragments ("the team space has taken 3 of 10 files").
fn sentence(text: &str) -> String {
    let text = text.trim();
    let mut chars = text.chars();
    let Some(first) = chars.next() else { return String::new() };
    let mut out: String = first.to_uppercase().chain(chars).collect();
    if !out.ends_with(['.', '!', '?']) {
        out.push('.');
    }
    out
}

/// A step as a phrase after "while".
fn step_word(step: FromFolderStep) -> &'static str {
    match step {
        FromFolderStep::Checking => "looking at the folder",
        FromFolderStep::Creating => "making the team space",
        FromFolderStep::SettingUp => "setting up the team folder",
        FromFolderStep::Copying => "copying the files",
        FromFolderStep::Syncing => "waiting for the team space to take the files",
        FromFolderStep::Verifying => "checking the files",
        FromFolderStep::Trashing => "deleting the old folder",
        FromFolderStep::Done | FromFolderStep::Failed | FromFolderStep::Interrupted => "working",
    }
}

/// Whether the notice, or the form, asks for the page to be redrawn on its own (work under way).
pub fn working(status: Option<&FromFolderStatus>) -> bool {
    status.is_some_and(FromFolderStatus::running)
}

impl Overview {
    /// Whether "Make a team space…" is offered: to an administrator, while no such work is under way and no form is open.
    pub fn can_make_space(&self) -> bool {
        matches!(self.access, Access::Admin { .. }) && !working(self.status.from_folder.as_ref()) && self.make_space.is_none()
    }
}

impl Model {
    /// The form and its call.
    pub(crate) fn make_space(&mut self, msg: MakeSpaceMsg) -> Vec<Effect> {
        let Some(overview) = self.overview_mut() else { return Vec::new() };
        match msg {
            MakeSpaceMsg::Open(folder) => {
                if !overview.can_make_space() {
                    return Vec::new();
                }
                let Some(root) = overview.status.folder.clone() else { return Vec::new() };
                let path = Path::new(&root).join(&folder).display().to_string();
                let needs_code = matches!(overview.access, Access::Admin { two_factor: true });
                overview.make_space = Some(MakeSpace {
                    name: folder.clone(),
                    folder,
                    path,
                    confirmation: Confirmation::default(),
                    needs_code,
                    resume: false,
                    busy: false,
                    error: None,
                    old: false,
                    ticket: 0,
                });
                Vec::new()
            }
            MakeSpaceMsg::Retry => {
                let Some(status) = overview.status.from_folder.clone() else { return Vec::new() };
                if status.running() || overview.make_space.is_some() {
                    return Vec::new();
                }
                // The same folder and name; with the space made, the agent needs no confirmation.
                let folder = Path::new(&status.folder).file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                let needs_code = matches!(overview.access, Access::Admin { two_factor: true });
                overview.make_space = Some(MakeSpace {
                    folder,
                    path: status.folder.clone(),
                    name: status.name.clone(),
                    confirmation: Confirmation::default(),
                    needs_code,
                    resume: status.space.is_some(),
                    busy: false,
                    error: None,
                    old: false,
                    ticket: 0,
                });
                Vec::new()
            }
            MakeSpaceMsg::Cancel => {
                if overview.make_space.as_ref().is_some_and(|m| !m.busy) {
                    overview.make_space = None;
                }
                Vec::new()
            }
            MakeSpaceMsg::Dismiss => {
                if let Some(status) = overview.status.from_folder.as_ref().filter(|s| !s.running()) {
                    overview.from_folder_dismissed = Some(dismissal_key(status));
                }
                Vec::new()
            }
            MakeSpaceMsg::Submit => {
                let Some(form) = overview.make_space.as_mut().filter(|m| m.ready()) else { return Vec::new() };
                form.busy = true;
                form.error = None;
                form.ticket = fresh();
                let confirm = if form.resume {
                    SpaceConfirm::default()
                } else {
                    let c = form.confirmation.take();
                    SpaceConfirm { password: c.password, totp_code: c.totp_code, recovery_code: c.recovery_code }
                };
                let request = FromFolderRequest { folder: form.path.clone(), name: form.name.trim().to_owned(), quota_bytes: None, confirm };
                vec![Effect::MakeSpace(MakeSpaceCall::Start { request, ticket: form.ticket })]
            }
            MakeSpaceMsg::Started { ticket, result } => {
                let Some(form) = overview.make_space.as_mut().filter(|m| m.ticket == ticket && m.busy) else { return Vec::new() };
                match result {
                    Ok(status) => {
                        overview.status.from_folder = Some(status);
                        overview.from_folder_dismissed = None;
                        overview.make_space = None;
                    }
                    Err(e) => {
                        form.busy = false;
                        form.old = is_old(&e);
                        form.error = Some(error_text(&e));
                    }
                }
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::tests::{agent, run, signed_in};
    use crate::agent::fake::FakeAgent;
    use crate::model::{Event, Screen};
    use craft_cloud_app::{SpaceId, codes};

    fn overview(model: &Model) -> &Overview {
        model.overview().expect("signed in")
    }

    fn form(model: &Model) -> &MakeSpace {
        overview(model).make_space.as_ref().expect("the form is open")
    }

    fn form_mut(model: &mut Model) -> &mut MakeSpace {
        model.overview_mut().and_then(|o| o.make_space.as_mut()).expect("the form is open")
    }

    fn form_free(model: &Model) -> bool {
        overview(model).make_space.is_none()
    }

    fn admin_model() -> (FakeAgent, Model) {
        let agent = agent(true, 0);
        let model = signed_in(&agent);
        (agent, model)
    }

    fn status_at(step: FromFolderStep) -> FromFolderStatus {
        FromFolderStatus {
            folder: "/home/ana/Craft Cloud/Shots".into(),
            name: "Shots team".into(),
            step,
            space: Some(SpaceId::generate()),
            team_folder: Some("/home/ana/Craft Cloud Shared/Shots team".into()),
            done: 0,
            total: 0,
            failed_at: None,
            message: None,
            kept: 0,
        }
    }

    fn type_confirmation(model: &mut Model, password: &str) {
        form_mut(model).confirmation.password = password.into();
    }

    #[test]
    fn the_form_is_offered_to_an_administrator_only_and_not_while_it_is_open_or_the_agent_works() {
        let (_, mut model) = admin_model();
        assert!(overview(&model).can_make_space());
        // A member gets nothing, and the call does nothing.
        let member = agent(false, 0);
        let mut other = signed_in(&member);
        assert!(!overview(&other).can_make_space());
        assert!(other.update(Event::MakeSpace(MakeSpaceMsg::Open("Shots".into()))).is_empty());
        assert!(form_free(&other));
        // Opened: no second form; the agent at work: no form either.
        model.update(Event::MakeSpace(MakeSpaceMsg::Open("Shots".into())));
        assert!(!overview(&model).can_make_space());
        model.update(Event::MakeSpace(MakeSpaceMsg::Cancel));
        assert!(form_free(&model));
        model.overview_mut().unwrap().status.from_folder = Some(status_at(FromFolderStep::Copying));
        assert!(!overview(&model).can_make_space());
        model.update(Event::MakeSpace(MakeSpaceMsg::Open("Shots".into())));
        assert!(form_free(&model), "not while the work of another folder is under way");
        model.overview_mut().unwrap().status.from_folder = Some(status_at(FromFolderStep::Done));
        assert!(overview(&model).can_make_space(), "the finished work does not block the next");
    }

    #[test]
    fn the_form_starts_with_the_folders_name_and_asks_for_the_password_and_a_code_when_the_account_has_one() {
        let (_, mut model) = admin_model();
        model.update(Event::MakeSpace(MakeSpaceMsg::Open("Shots".into())));
        let f = form(&model);
        assert_eq!((f.folder.as_str(), f.name.as_str()), ("Shots", "Shots"));
        assert_eq!(f.path, std::path::Path::new("/home/ana/Craft Cloud").join("Shots").display().to_string());
        assert!(!f.ready(), "no password yet");
        assert!(!f.needs_code);
        type_confirmation(&mut model, "secret");
        assert!(form(&model).ready());
        form_mut(&mut model).name = "   ".into();
        assert!(!form(&model).ready(), "a name is needed");
        // Two-factor sign-in: the code is needed too.
        let mut two = admin_model().1;
        two.overview_mut().unwrap().access = Access::Admin { two_factor: true };
        two.update(Event::MakeSpace(MakeSpaceMsg::Open("Shots".into())));
        assert!(form(&two).needs_code);
        type_confirmation(&mut two, "secret");
        assert!(!form(&two).ready());
        form_mut(&mut two).confirmation.code = "123 456".into();
        assert!(form(&two).ready());
    }

    #[test]
    fn submitting_sends_the_folder_the_trimmed_name_and_the_confirmation_and_hides_the_password_in_debug() {
        let (_, mut model) = admin_model();
        model.update(Event::MakeSpace(MakeSpaceMsg::Open("Shots".into())));
        form_mut(&mut model).name = "  Shots team ".into();
        type_confirmation(&mut model, "hunter2-password");
        let effects = model.update(Event::MakeSpace(MakeSpaceMsg::Submit));
        let [Effect::MakeSpace(MakeSpaceCall::Start { request, ticket })] = effects.as_slice() else { panic!("{effects:?}") };
        assert_eq!(request.name, "Shots team");
        assert!(request.folder.ends_with("Shots"), "{}", request.folder);
        assert_eq!(request.confirm.password.as_deref(), Some("hunter2-password"));
        assert_eq!(form(&model).ticket, *ticket);
        assert!(form(&model).busy);
        assert!(!format!("{effects:?}").contains("hunter2"), "{effects:?}");
        assert!(form(&model).confirmation.password.is_empty(), "what was typed is cleared once it is sent");
        // A second click while it is under way sends nothing, and the form can't be cancelled meanwhile.
        assert!(model.update(Event::MakeSpace(MakeSpaceMsg::Submit)).is_empty());
        assert!(model.update(Event::MakeSpace(MakeSpaceMsg::Cancel)).is_empty());
        assert!(form(&model).busy);
    }

    #[test]
    fn the_agents_answer_closes_the_form_and_the_status_carries_the_work() {
        let (agent, mut model) = admin_model();
        model.update(Event::MakeSpace(MakeSpaceMsg::Open("Shots".into())));
        type_confirmation(&mut model, "secret");
        run(&mut model, &agent, vec![Event::MakeSpace(MakeSpaceMsg::Submit)]);
        assert!(form_free(&model), "the call went through the fake agent and answered");
        let status = overview(&model).status.from_folder.clone().expect("the status has the work");
        assert_eq!((status.name.as_str(), status.step), ("Shots", FromFolderStep::SettingUp));
        assert!(agent.team.lock().unwrap().from_folder.iter().any(|r| r.name == "Shots" && r.confirm.password.as_deref() == Some("secret")));
        assert_eq!(notice(&status, None).unwrap().tone, Tone::Working);
    }

    #[test]
    fn a_refusal_keeps_the_form_open_with_the_servers_words_and_asks_for_the_password_again() {
        let (agent, mut model) = admin_model();
        model.update(Event::MakeSpace(MakeSpaceMsg::Open("Shots".into())));
        type_confirmation(&mut model, "secret");
        agent.team.lock().unwrap().fails.push(Error::Refused {
            code: "conflict".into(),
            message: "a team space with this name exists".into(),
            name_taken: None,
        });
        run(&mut model, &agent, vec![Event::MakeSpace(MakeSpaceMsg::Submit)]);
        let f = form(&model);
        assert!(!f.busy && !f.old);
        assert_eq!(f.error.as_deref(), Some("a team space with this name exists"));
        assert!(f.confirmation.password.is_empty(), "the password has to be typed again");
        assert!(overview(&model).status.from_folder.is_none());
        // Typed again, it goes through.
        type_confirmation(&mut model, "secret");
        run(&mut model, &agent, vec![Event::MakeSpace(MakeSpaceMsg::Submit)]);
        assert!(form_free(&model));
    }

    #[test]
    fn an_old_agent_or_server_is_told_in_one_sentence_and_nothing_else_is_offered() {
        for (code, words) in [(codes::AGENT_TOO_OLD, crate::spaces::TOO_OLD), (codes::SERVER_TOO_OLD, crate::spaces::SERVER_TOO_OLD)] {
            let (agent, mut model) = admin_model();
            model.update(Event::MakeSpace(MakeSpaceMsg::Open("Shots".into())));
            type_confirmation(&mut model, "secret");
            agent.team.lock().unwrap().fails.push(Error::Refused { code: code.into(), message: "x".into(), name_taken: None });
            run(&mut model, &agent, vec![Event::MakeSpace(MakeSpaceMsg::Submit)]);
            let f = form(&model);
            assert_eq!(f.error.as_deref(), Some(words), "{code}");
            assert!(f.old && !f.busy, "{code}");
            type_confirmation(&mut model, "secret");
            assert!(!form(&model).ready(), "{code}: submitting is off");
            assert!(model.update(Event::MakeSpace(MakeSpaceMsg::Submit)).is_empty());
            model.update(Event::MakeSpace(MakeSpaceMsg::Cancel));
            assert!(form_free(&model));
        }
    }

    #[test]
    fn an_answer_for_another_call_is_dropped() {
        let (_, mut model) = admin_model();
        model.update(Event::MakeSpace(MakeSpaceMsg::Open("Shots".into())));
        type_confirmation(&mut model, "secret");
        model.update(Event::MakeSpace(MakeSpaceMsg::Submit));
        let other = form(&model).ticket + 1;
        model.update(Event::MakeSpace(MakeSpaceMsg::Started { ticket: other, result: Ok(status_at(FromFolderStep::Copying)) }));
        assert!(form(&model).busy && overview(&model).status.from_folder.is_none());
        // And an answer when no form is open goes nowhere.
        let ticket = form(&model).ticket;
        model.update(Event::MakeSpace(MakeSpaceMsg::Started { ticket, result: Err(Error::NotRunning("gone".into())) }));
        assert!(!form(&model).busy);
        model.update(Event::MakeSpace(MakeSpaceMsg::Cancel));
        model.update(Event::MakeSpace(MakeSpaceMsg::Started { ticket, result: Ok(status_at(FromFolderStep::Copying)) }));
        assert!(overview(&model).status.from_folder.is_none());
        assert!(matches!(model.screen, Screen::SignedIn(_)));
    }

    #[test]
    fn every_step_has_a_notice_that_says_where_the_work_is_and_that_the_old_folder_is_not_changed_yet() {
        let working = [
            (FromFolderStep::Checking, "Asking the server"),
            (FromFolderStep::Creating, "Asking the server"),
            (FromFolderStep::SettingUp, "has not been changed"),
            (FromFolderStep::Copying, "has not been changed"),
            (FromFolderStep::Syncing, "stays as it is until then"),
            (FromFolderStep::Verifying, "before anything is deleted"),
            (FromFolderStep::Trashing, "Every file is in the team space"),
        ];
        for (step, words) in working {
            let mut status = status_at(step);
            status.done = 3;
            status.total = 10;
            let n = notice(&status, None).unwrap_or_else(|| panic!("{step:?}"));
            assert_eq!(n.tone, Tone::Working, "{step:?}");
            assert!(n.text.contains(words), "{step:?}: {}", n.text);
            assert!(n.title.contains("Shots team"), "{step:?}");
            assert!(!n.dismiss && !n.retry && !n.open_shared, "{step:?}: no buttons while it works");
            let counts = matches!(step, FromFolderStep::Copying | FromFolderStep::Syncing | FromFolderStep::Verifying);
            assert_eq!(n.progress, counts.then_some((3, 10)), "{step:?}");
            // A working notice cannot be dismissed away.
            assert!(notice(&status, Some(dismissal_key(&status).as_str())).is_some(), "{step:?}");
        }
    }

    #[test]
    fn a_finished_notice_offers_the_shared_tab_and_dismisses_until_newer_work_comes() {
        let mut status = status_at(FromFolderStep::Done);
        let n = notice(&status, None).unwrap();
        assert_eq!(n.tone, Tone::Good);
        assert!(n.text.contains("trash on the server") && n.open_shared && n.dismiss && !n.retry, "{n:?}");
        status.kept = 2;
        let kept = notice(&status, None).unwrap();
        assert!(kept.text.contains("2 files stayed in “Shots”"), "{}", kept.text);
        let key = dismissal_key(&status);
        assert!(notice(&status, Some(&key)).is_none());
        // Another run's notice is shown again.
        status.name = "Other".into();
        assert!(notice(&status, Some(&key)).is_some());
        // Dismiss through the model.
        let (_, mut model) = admin_model();
        model.overview_mut().unwrap().status.from_folder = Some(status_at(FromFolderStep::Done));
        model.update(Event::MakeSpace(MakeSpaceMsg::Dismiss));
        let o = overview(&model);
        assert!(notice(o.status.from_folder.as_ref().unwrap(), o.from_folder_dismissed.as_deref()).is_none());
        // The work under way is not dismissed.
        let mut running = admin_model().1;
        running.overview_mut().unwrap().status.from_folder = Some(status_at(FromFolderStep::Copying));
        running.update(Event::MakeSpace(MakeSpaceMsg::Dismiss));
        assert!(overview(&running).from_folder_dismissed.is_none());
    }

    #[test]
    fn work_that_failed_or_was_interrupted_says_where_and_that_nothing_was_lost_and_offers_to_try_again() {
        for (outcome, lead) in [(FromFolderStep::Failed, "Couldn't finish while"), (FromFolderStep::Interrupted, "Stopped while")] {
            let mut status = status_at(outcome);
            status.failed_at = Some(FromFolderStep::Syncing);
            status.message = Some("the team space has taken 3 of 10 files".into());
            let n = notice(&status, None).unwrap();
            assert_eq!(n.tone, Tone::Bad);
            assert!(n.text.starts_with(lead) && n.text.contains("waiting for the team space"), "{}", n.text);
            assert!(n.text.contains("was not changed") && n.text.contains("stays; trying again goes on with it"), "{}", n.text);
            assert!(n.retry && n.dismiss && !n.open_shared, "{n:?}");
        }
        let mut said = status_at(FromFolderStep::Failed);
        said.failed_at = Some(FromFolderStep::Copying);
        said.message = Some("the file is locked".into());
        assert!(notice(&said, None).unwrap().text.contains("copying the files. The file is locked. “Shots” was not changed."));
        // Stopped while deleting: every file is in the team space already.
        let mut late = status_at(FromFolderStep::Failed);
        late.failed_at = Some(FromFolderStep::Trashing);
        late.message = Some("x".into());
        let n = notice(&late, None).unwrap();
        assert!(n.text.contains("Every file is in the team space") && !n.text.contains("was not changed"), "{}", n.text);
        // Failed before the space was made: nothing to go on with, so no retry button, only the words.
        let mut early = status_at(FromFolderStep::Failed);
        early.space = None;
        early.failed_at = Some(FromFolderStep::Creating);
        let n = notice(&early, None).unwrap();
        assert!(!n.retry && n.dismiss && !n.text.contains("stays;"), "{n:?}");
    }

    #[test]
    fn trying_again_goes_on_with_the_same_space_and_needs_no_password() {
        let (agent, mut model) = admin_model();
        let mut status = status_at(FromFolderStep::Failed);
        status.failed_at = Some(FromFolderStep::Copying);
        model.overview_mut().unwrap().status.from_folder = Some(status);
        model.update(Event::MakeSpace(MakeSpaceMsg::Retry));
        let f = form(&model);
        assert!(f.resume && f.ready(), "{f:?}");
        assert_eq!((f.name.as_str(), f.path.as_str()), ("Shots team", "/home/ana/Craft Cloud/Shots"));
        run(&mut model, &agent, vec![Event::MakeSpace(MakeSpaceMsg::Submit)]);
        assert!(form_free(&model));
        let sent = agent.team.lock().unwrap().from_folder.last().cloned().unwrap();
        assert_eq!((sent.name.as_str(), sent.confirm.password.as_deref()), ("Shots team", None));
        // A run that is under way can't be retried.
        model.overview_mut().unwrap().status.from_folder = Some(status_at(FromFolderStep::Copying));
        model.update(Event::MakeSpace(MakeSpaceMsg::Retry));
        assert!(form_free(&model));
    }
}
