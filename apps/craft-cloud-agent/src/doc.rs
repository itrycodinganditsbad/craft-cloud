//! `craft-cloud-agent doc <path> [--append <text>] [--watch]`: a file of the synced folder as a live plain-text
//! document, through the running agent's local API, a client like an app (`craft-cloud-app`'s live documents). The
//! mapping is the plainest there is: one Loro text under `text`. A file that has never been live is imported from its
//! content as UTF-8 (one that isn't is refused). It prints the text, appends to it, or prints it again after every
//! change. A test tool: it never writes the file.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use craft_cloud_app::live::{Event, Opened};
use loro::{ExportMode, LoroDoc};

use crate::account::DataDir;
use crate::cli::{AgentError, DocArgs, Exit, blocking, local_error, relative_to_folder};

/// The app this tool opens documents as.
pub const APP: &str = "plaintext";
/// The text's container in the document.
const TEXT: &str = "text";
/// How long an appended change may take to be on the agent's disk.
const ACK_TIMEOUT: Duration = Duration::from_secs(10);

/// A document of one text holding `text`, as a Loro snapshot (the import of a file that has never been live).
pub fn plain_text_snapshot(text: &str) -> Result<Vec<u8>, String> {
    let doc = LoroDoc::new();
    doc.get_text(TEXT).insert(0, text).map_err(|e| e.to_string())?;
    doc.commit();
    doc.export(ExportMode::Snapshot).map_err(|e| e.to_string())
}

fn usage(message: impl Into<String>) -> AgentError {
    AgentError::Usage(message.into())
}

/// What the tool's thread tells the command to print.
enum Line {
    Out(String),
    Done(Exit),
}

pub async fn doc(data: &DataDir, args: DocArgs, out: &mut impl Write) -> Result<Exit, AgentError> {
    let data_path = data.path().to_owned();
    let client = blocking(move || craft_cloud_app::Client::connect_in(&data_path)).await.map_err(|e| match e {
        AgentError::Local(craft_cloud_app::Error::NotRunning(why)) => {
            usage(format!("the agent isn't running ({why}); start `craft-cloud-agent run` first"))
        }
        other => other,
    })?;
    let status = {
        let c = client.clone();
        blocking(move || c.status()).await?
    };
    let folder = status.folder.map(PathBuf::from).ok_or(AgentError::NotSignedIn)?;
    let path = relative_to_folder(&args.path, Some(&folder))?;
    // The crate blocks: it runs on a thread of its own, and what it has to say comes back here.
    let (lines_tx, mut lines) = tokio::sync::mpsc::unbounded_channel::<Line>();
    let task = tokio::task::spawn_blocking(move || {
        let said = lines_tx.clone();
        let say = move |line: String| {
            let _ = said.send(Line::Out(line));
        };
        let exit = session(&client, &folder, &path, args.append.as_deref(), args.watch, &say);
        if let Ok(exit) = &exit {
            let _ = lines_tx.send(Line::Done(*exit));
        }
        exit
    });
    let mut exit = Exit::Done;
    while let Some(line) = lines.recv().await {
        match line {
            Line::Out(text) => {
                writeln!(out, "{text}").map_err(|e| AgentError::Io { path: Path::new("standard output").to_owned(), source: e })?;
                let _ = out.flush();
            }
            Line::Done(done) => exit = done,
        }
    }
    task.await.map_err(|e| usage(format!("the document's session failed: {e}")))?.map_err(local_error)?;
    Ok(exit)
}

fn session(
    client: &craft_cloud_app::Client,
    folder: &Path,
    path: &str,
    append: Option<&str>,
    watch: bool,
    say: &dyn Fn(String),
) -> Result<Exit, AgentError> {
    let docs = client.documents().map_err(AgentError::Local)?;
    let document = match docs.open(path, APP).map_err(AgentError::Local)? {
        Opened::Live(document) => document,
        Opened::NeedsImport(importer) => {
            let file = folder.join(path);
            let bytes = std::fs::read(&file).map_err(|source| AgentError::Io { path: file.clone(), source })?;
            let text = String::from_utf8(bytes).map_err(|_| usage(format!("{path} isn't UTF-8 text; `doc` reads plain text only")))?;
            let snapshot = plain_text_snapshot(&text).map_err(usage)?;
            importer.import(&snapshot).map_err(AgentError::Local)?
        }
        Opened::NotLive(reason) => {
            say(format!("{path} isn't live ({reason:?}); apps open it as a plain file for now"));
            return Ok(Exit::Failed);
        }
    };
    let mirror = LoroDoc::new();
    mirror.import(document.state()).map_err(|e| usage(format!("the document's state doesn't read: {e}")))?;
    for update in document.state_updates() {
        // (An update this copy can't take is the other side's; the agent has it, and so will a fresh open.)
        let _ = mirror.import(update);
    }
    let text = |mirror: &LoroDoc| mirror.get_text(TEXT).to_string();
    // What came right after the state (updates still waiting for their history), before anything else.
    let take = |mirror: &LoroDoc, event: &Event| {
        if let Event::Update(bytes) = event {
            // (An update this copy can't take is the other side's; the agent has it, and so will a fresh open.)
            let _ = mirror.import(bytes);
        }
    };
    while let Some(event) = document.try_recv() {
        take(&mirror, &event);
    }
    if let Some(addition) = append {
        let before = mirror.oplog_vv();
        let t = mirror.get_text(TEXT);
        t.insert(t.len_unicode(), addition).map_err(|e| usage(e.to_string()))?;
        mirror.commit();
        let update = mirror.export(ExportMode::updates(&before)).map_err(|e| usage(e.to_string()))?;
        let seq = document.send(&update).map_err(AgentError::Local)?;
        let until = Instant::now() + ACK_TIMEOUT;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            match document.recv_timeout(left) {
                Some(Event::Acked(acked)) if acked == seq => break,
                Some(Event::Refused { seq: refused, message, .. }) if refused == seq => {
                    return Err(usage(format!("the agent refused the change: {message}")));
                }
                Some(Event::Closed(why)) => return Err(usage(format!("the connection to the agent ended: {why}"))),
                Some(event) => take(&mirror, &event),
                None => return Err(usage("the agent didn't take the change in time")),
            }
        }
    }
    say(text(&mirror));
    if !watch {
        return Ok(Exit::Done);
    }
    loop {
        match document.recv_timeout(Duration::from_secs(1)) {
            Some(Event::Update(bytes)) => {
                let _ = mirror.import(&bytes);
                say("--".into());
                say(text(&mirror));
            }
            Some(Event::Status { online, waiting, deleted, stopped }) => {
                let deleted = if deleted { "; its file was deleted" } else { "" };
                let stopped = if stopped { "; the agent stopped following it until it restarts" } else { "" };
                say(format!("({}, {waiting} change(s) waiting{deleted}{stopped})", if online { "online" } else { "offline" }));
            }
            Some(Event::Closed(why)) => {
                say(format!("the connection to the agent ended: {why}"));
                return Ok(Exit::Failed);
            }
            Some(Event::Presence(_) | Event::Acked(_) | Event::Refused { .. } | Event::File(_)) | None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_text_document_holds_its_text() {
        let snapshot = plain_text_snapshot("héllo\nwörld").unwrap();
        let doc = LoroDoc::new();
        doc.import(&snapshot).unwrap();
        assert_eq!(doc.get_text(TEXT).to_string(), "héllo\nwörld");
    }
}
