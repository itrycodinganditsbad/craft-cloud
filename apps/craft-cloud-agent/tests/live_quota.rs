//! A live document's quota is one way: once the log holds the cap, nothing more is stored, so no later, smaller update
//! of a device is stored past a refused (`quota_exceeded`) one it depends on, and every stored update applies on
//! another machine (a hole there would make every other machine hold it for good).

use craft_cloud_agent::live::recents::{Imported, Recents};
use craft_cloud_meta::{Meta, MetaError, Role};
use craft_cloud_proto::live::{AppName, recents_doc};
use craft_cloud_proto::local::RecentEntry;

fn entry(path: &str, opened_ms: i64) -> RecentEntry {
    RecentEntry { node: None, path: path.into(), title: path.into(), opened_ms, saved_ms: None, machine: "pc".into() }
}

fn key(n: u32) -> [u8; 16] {
    let mut k = [0u8; 16];
    k[..4].copy_from_slice(&n.to_le_bytes());
    k[15] = 0xaa;
    k
}

#[test]
fn a_full_document_leaves_no_hole() {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(quota_hole());
}

async fn quota_hole() {
    let meta = Meta::open("sqlite::memory:").await.unwrap();
    let p = Recents::new(42);
    let u1 = p.add(key(1), &entry("one.txt", 1)).unwrap();
    let u2 = p.add(key(2), &entry(&format!("{}.txt", "long".repeat(40)), 2)).unwrap();
    let u3 = p.add(key(3), &entry("t.txt", 3)).unwrap();
    assert!(u2.len() > u3.len(), "{} {}", u2.len(), u3.len());
    let cap = (u1.len() + u3.len()) as u64;
    let meta = meta.with_live_doc_cap(cap);
    let user = meta.create_user("ana", "$argon2id$stub", Role::Member, 0).await.unwrap();
    meta.create_device(user.id, "pc", &[7; 32], 0).await.unwrap();
    let auth = meta.device_by_token(&[7; 32]).await.unwrap().unwrap();
    let app = AppName::new("wordcraft").unwrap();
    let doc = recents_doc(auth.space, &app);
    meta.create_live_doc(auth.space, doc, "recents:wordcraft", None, 0).await.unwrap();
    assert_eq!(meta.append_live_update(auth.space, doc, auth.device, 1, &u1, 0).await.unwrap(), 1);
    // u2 crosses the cap: stored. From then on nothing is, however small.
    assert_eq!(meta.append_live_update(auth.space, doc, auth.device, 2, &u2, 0).await.unwrap(), 2);
    let full = meta.append_live_update(auth.space, doc, auth.device, 3, &u3, 0).await;
    assert!(matches!(full, Err(MetaError::DocFull { .. })), "the smaller one was stored past the cap: {full:?}");
    let q = Recents::new(43);
    for u in meta.live_updates(auth.space, doc, 0, 100).await.unwrap() {
        let got = q.import(&u.bytes).unwrap();
        assert!(matches!(got, Imported::Applied { .. }), "seq {} waits on another machine: {got:?}", u.seq);
    }
}
