//! The kit on a big document, in release (`cargo test --release -p craft-cloud-kit-demo --test speed`): the demo with a
//! 1 MB story and 5,000 layers. A keystroke's `local_change` stays under 2 ms (median), a remote keystroke's `poll`
//! under 5 ms (median), and opening the document on the other machine under 300 ms.

use std::sync::Arc;
use std::time::{Duration, Instant};

use craft_cloud_app::fake::{Fake, Hub};
use craft_cloud_kit::{Agent, ElemId, Open, RichText, Session, Value};
use craft_cloud_kit_demo::model::{self, BOLD, DemoDoc, Layer};

fn big() -> DemoDoc {
    let mut text = String::with_capacity(1_100_000);
    let mut i = 0;
    while text.len() < 1_000_000 {
        text.push_str(&format!("Paragraph {i}: the quick brown fox jumps over the lazy dog, again and again, as stories do.\n"));
        i += 1;
    }
    let mut story = RichText::plain(&text);
    // A bold word every fifth paragraph, a heading every fiftieth.
    let mut at = 0;
    let mut runs = Vec::new();
    for (p, line) in text.split_inclusive('\n').enumerate() {
        if p % 5 == 0 {
            runs.push((at + 13..at + 18, [(BOLD.to_owned(), Value::Bool(true))].into()));
        }
        if p % 50 == 0
            && let Some(attrs) = story.paragraphs.get_mut(p)
        {
            attrs.insert(model::HEADING.to_owned(), Value::I64(1));
        }
        at += line.len();
    }
    story.runs = runs;
    let layers: Vec<Arc<Layer>> = (0..50)
        .map(|g| {
            let children = (0..99).map(|c| Arc::new(Layer { id: ElemId::new(), name: format!("Layer {g}.{c}"), children: Vec::new() })).collect();
            Arc::new(Layer { id: ElemId::new(), name: format!("Group {g}"), children })
        })
        .collect();
    let mut doc = DemoDoc { story: Arc::new(story), layers: Arc::new(layers), paragraphs: 0 };
    <model::DemoApp as craft_cloud_kit::LiveDocument>::refresh_derived(&mut doc);
    doc
}

fn open(fake: &Fake, doc: DemoDoc) -> Session<model::DemoApp> {
    let client = fake.client();
    match Session::<model::DemoApp>::open(&Agent::from_fn(move || Ok(client.clone())), "big.kitdemo", Arc::new(doc), None).unwrap() {
        Open::Live(s) => *s,
        Open::NotLive(why) => panic!("not live: {why:?}"),
    }
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

#[test]
#[cfg_attr(debug_assertions, ignore = "a speed test: run it in release")]
fn a_big_document_stays_quick() {
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let doc = big();
    assert!(doc.story.text.len() >= 1_000_000 && model::flatten(&doc.layers).len() == 5000);
    let t = Instant::now();
    let mut a = open(&pc, doc);
    let import = t.elapsed();
    let t = Instant::now();
    let mut b = open(&laptop, DemoDoc::default());
    let opened = t.elapsed();
    assert_eq!(b.model().story.text.len(), a.model().story.text.len());
    assert_eq!(model::flatten(&b.model().layers).len(), 5000);
    eprintln!("SPEED import on the pc {import:?}; open on the laptop {opened:?}");
    assert!(opened < Duration::from_millis(300), "opening took {opened:?}");

    let (mut local, mut remote) = (Vec::new(), Vec::new());
    let len = a.model().story.text.len();
    for k in 0..120usize {
        // A keystroke somewhere in the story.
        let at = (k * 7919) % len;
        let at = (0..=at).rev().find(|i| a.model().story.text.is_char_boundary(*i)).unwrap_or(0);
        let mut text = a.model().story.text.clone();
        text.insert(at, 'x');
        let old = a.model().clone();
        let new = Arc::new(DemoDoc { story: Arc::new(model::edit_text(&old.story, &text)), ..(*old).clone() });
        let t = Instant::now();
        a.local_change(&old, &new).unwrap();
        local.push(t.elapsed());
        // The laptop's poll that takes it.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let t = Instant::now();
            let polled = b.poll();
            if polled.changed {
                remote.push(t.elapsed());
                break;
            }
            assert!(Instant::now() < deadline, "the keystroke didn't come");
            std::thread::sleep(Duration::from_micros(200));
        }
    }
    assert_eq!(b.model().story.text, a.model().story.text);
    let (l, r) = (median(local.clone()), median(remote.clone()));
    eprintln!(
        "SPEED keystroke local_change median {l:?} (worst {:?}); remote poll median {r:?} (worst {:?})",
        local.iter().max(),
        remote.iter().max()
    );
    assert!(l < Duration::from_millis(2), "local_change median {l:?}");
    assert!(r < Duration::from_millis(5), "remote poll median {r:?}");

    // A layer renamed: the other side rebuilds only that layer.
    let first = model::flatten(&a.model().layers)[10].1.id;
    let old = a.model().clone();
    let new = Arc::new(DemoDoc { layers: Arc::new(model::rename(&old.layers, first, "renamed")), ..(*old).clone() });
    let t = Instant::now();
    a.local_change(&old, &new).unwrap();
    let rename = t.elapsed();
    let deadline = Instant::now() + Duration::from_secs(10);
    let poll = loop {
        let t = Instant::now();
        if b.poll().changed {
            break t.elapsed();
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_micros(200));
    };
    eprintln!("SPEED a layer renamed: local_change {rename:?}, remote poll {poll:?}");
    assert_eq!(model::find(&b.model().layers, first).map(|l| l.name.as_str()), Some("renamed"));
}
