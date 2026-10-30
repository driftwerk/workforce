use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{Duration, Utc};
use workforce_identity::sqlite::SqliteIdentityStore;
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityRole};

use super::postgres::PostgresPictures;
use super::refresh::{
    due, fetchable, Refresher, SlackDirectory, SlackFace, RETRY_AFTER, STALE_AFTER,
};
use super::*;

fn png(seed: u8) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend([seed; 16]);
    bytes
}

async fn with_store<F, Fut>(test: F)
where
    F: FnOnce(Arc<dyn PictureStore>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    workforce_postgres::test_support::with_test_pool(
        PostgresPictures::build_pool,
        |pool| async move {
            test(Arc::new(PostgresPictures::with_pool(pool).await.unwrap())).await;
        },
    )
    .await;
}

fn slack_picture(seed: u8, origin: &str) -> SlackPicture {
    SlackPicture {
        image: Image::accept(png(seed)).unwrap(),
        origin: origin.into(),
        workspace: "acme".into(),
    }
}

#[test]
fn only_png_jpeg_and_webp_of_a_bounded_size_are_pictures() {
    assert_eq!(Image::accept(png(1)).unwrap().media_type, "image/png");
    assert_eq!(
        Image::accept(b"\xFF\xD8\xFF\xE0rest".to_vec())
            .unwrap()
            .media_type,
        "image/jpeg"
    );
    assert_eq!(
        Image::accept(b"RIFF\0\0\0\0WEBPVP8 ".to_vec())
            .unwrap()
            .media_type,
        "image/webp"
    );
    assert_eq!(
        Image::accept(b"GIF89a......".to_vec()),
        Err(Refusal::NotAnImage)
    );
    assert_eq!(Image::accept(b"<svg/>".to_vec()), Err(Refusal::NotAnImage));
    let mut huge = png(1);
    huge.resize(MAX_BYTES + 1, 0);
    assert_eq!(Image::accept(huge), Err(Refusal::TooLarge));
    assert_eq!(
        Image::accept(png(1)).unwrap().etag,
        Image::accept(png(1)).unwrap().etag
    );
    assert_ne!(
        Image::accept(png(1)).unwrap().etag,
        Image::accept(png(2)).unwrap().etag
    );
}

#[test]
fn only_https_addresses_are_fetched() {
    assert!(fetchable("https://avatars.slack-edge.com/a_512.png"));
    assert!(!fetchable("http://avatars.slack-edge.com/a_512.png"));
    assert!(!fetchable("file:///etc/passwd"));
    assert!(!fetchable("not a url"));
}

#[test]
fn a_slack_picture_is_due_when_new_changed_or_a_day_old() {
    let now = Utc::now();
    assert!(due(None, "member:acme:U1", now));
    assert!(!due(Some(("member:acme:U1", now)), "member:acme:U1", now));
    assert!(due(
        Some(("member:acme:U1", now)),
        "icon:https://x/y.png",
        now
    ));
    let stale = now - Duration::from_std(STALE_AFTER).unwrap();
    assert!(due(Some(("member:acme:U1", stale)), "member:acme:U1", now));
}

#[tokio::test]
async fn a_chosen_picture_shows_over_the_slack_one_and_neither_means_none() {
    with_store(|store| async move {
        let allan = Owner::agent("allan");
        assert_eq!(face(store.as_ref(), &allan).await.unwrap(), None);
        assert!(store.shown(&allan).await.unwrap().is_none());

        store
            .put_slack(&allan, &slack_picture(7, "member:acme:U1"))
            .await
            .unwrap();
        let from_slack = face(store.as_ref(), &allan).await.unwrap().unwrap();
        assert_eq!(
            (from_slack.source, from_slack.slack.as_deref()),
            (Source::Slack, Some("acme"))
        );
        assert_eq!(
            store.shown(&allan).await.unwrap().unwrap().image.bytes,
            png(7)
        );

        let chosen = Image::accept(png(1)).unwrap();
        store.choose(&allan, &chosen).await.unwrap();
        let shown = face(store.as_ref(), &allan).await.unwrap().unwrap();
        assert_eq!(
            (shown.source, shown.etag.as_str(), shown.slack.as_deref()),
            (Source::Chosen, chosen.etag.as_str(), Some("acme"))
        );
        assert_eq!(
            store.shown(&allan).await.unwrap().unwrap().image.bytes,
            png(1)
        );
        assert_eq!(
            faces(store.as_ref(), Kind::Agent).await.unwrap()["allan"],
            shown
        );
        assert!(
            faces(store.as_ref(), Kind::Identity)
                .await
                .unwrap()
                .is_empty(),
            "agents and people are apart"
        );

        assert!(store.remove(&allan, Source::Chosen).await.unwrap());
        assert_eq!(
            face(store.as_ref(), &allan).await.unwrap().unwrap().source,
            Source::Slack
        );
        assert_eq!(store.forget(&allan).await.unwrap(), 1);
        assert_eq!(face(store.as_ref(), &allan).await.unwrap(), None);
    })
    .await;
}

#[tokio::test]
async fn a_merge_hands_over_a_chosen_picture_unless_the_target_chose_one() {
    with_store(|store| async move {
        let (a, b, c) = (
            Owner::identity("a"),
            Owner::identity("b"),
            Owner::identity("c"),
        );
        store
            .choose(&a, &Image::accept(png(1)).unwrap())
            .await
            .unwrap();
        store
            .put_slack(&a, &slack_picture(2, "member:U1"))
            .await
            .unwrap();
        assert!(store.hand_over(&a, &b).await.unwrap());
        assert_eq!(store.shown(&b).await.unwrap().unwrap().image.bytes, png(1));
        assert!(store.kept_for(&a).await.unwrap().is_empty());

        store
            .choose(&c, &Image::accept(png(3)).unwrap())
            .await
            .unwrap();
        assert!(!store.hand_over(&b, &c).await.unwrap());
        assert_eq!(store.shown(&c).await.unwrap().unwrap().image.bytes, png(3));
        assert!(store.kept_for(&b).await.unwrap().is_empty());
    })
    .await;
}

/// Slack as the refresher sees it, set by the test.
#[derive(Default)]
struct FakeSlack {
    starting: AtomicBool,
    faces: Mutex<HashMap<String, SlackFace>>,
    failing: AtomicBool,
    fetches: AtomicUsize,
    seed: AtomicU8,
}

impl FakeSlack {
    fn set(&self, agent: &str, face: Option<SlackFace>) {
        let mut faces = self.faces.lock().unwrap();
        match face {
            Some(face) => faces.insert(agent.into(), face),
            None => faces.remove(agent),
        };
    }

    fn fetches(&self) -> usize {
        self.fetches.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl SlackDirectory for FakeSlack {
    async fn ready(&self) -> bool {
        !self.starting.load(Ordering::SeqCst)
    }

    async fn agent(&self, agent_id: &str) -> Option<SlackFace> {
        self.faces.lock().unwrap().get(agent_id).cloned()
    }

    async fn locate(&self, face: &SlackFace) -> eyre::Result<(String, String)> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        if self.failing.load(Ordering::SeqCst) {
            eyre::bail!("Slack is down");
        }
        Ok((
            format!("https://slack.test/{}", face.origin()),
            "acme".into(),
        ))
    }

    async fn download(&self, _url: &str) -> eyre::Result<Image> {
        Ok(Image::accept(png(self.seed.load(Ordering::SeqCst)))?)
    }
}

fn bot(user_id: &str) -> SlackFace {
    SlackFace::Member {
        user_id: user_id.into(),
        workspace: Some("acme".into()),
    }
}

fn refresher(
    store: &Arc<dyn PictureStore>,
    slack: &Arc<FakeSlack>,
    identities: Option<Arc<dyn IdentityStore>>,
) -> Refresher {
    Refresher::new(
        Arc::clone(store),
        Arc::clone(slack) as Arc<dyn SlackDirectory>,
        Arc::new(|| vec!["allan".to_string(), "brian".to_string()]),
        identities,
    )
}

#[tokio::test]
async fn an_agents_slack_picture_is_fetched_when_it_appears_changes_or_ages() {
    with_store(|store| async move {
        let slack = Arc::new(FakeSlack::default());
        let refresher = refresher(&store, &slack, None);
        let allan = Owner::agent("allan");
        let now = Utc::now();

        refresher.refresh(now).await.unwrap();
        assert_eq!(slack.fetches(), 0, "no agent has Slack");

        slack.set("allan", Some(bot("U1")));
        slack.seed.store(5, Ordering::SeqCst);
        refresher.refresh(now).await.unwrap();
        let kept = store.kept_for(&allan).await.unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(
            (kept[0].source, kept[0].origin.as_deref()),
            (Source::Slack, Some("member:acme:U1"))
        );
        assert_eq!(
            store.shown(&allan).await.unwrap().unwrap().image.bytes,
            png(5)
        );

        refresher.refresh(now).await.unwrap();
        assert_eq!(slack.fetches(), 1, "fresh, so not fetched again");

        slack.set(
            "allan",
            Some(SlackFace::Icon {
                url: "https://example.com/allan.png".into(),
                workspace: Some("acme".into()),
            }),
        );
        slack.seed.store(6, Ordering::SeqCst);
        refresher.refresh(now).await.unwrap();
        assert_eq!(slack.fetches(), 2, "its Slack identity changed");
        assert_eq!(
            store.shown(&allan).await.unwrap().unwrap().image.bytes,
            png(6)
        );

        refresher
            .refresh(now + Duration::from_std(STALE_AFTER).unwrap() + Duration::minutes(1))
            .await
            .unwrap();
        assert_eq!(slack.fetches(), 3, "a day later");

        store
            .choose(&allan, &Image::accept(png(9)).unwrap())
            .await
            .unwrap();
        slack.set("allan", None);
        refresher.refresh(now).await.unwrap();
        let kept = store.kept_for(&allan).await.unwrap();
        assert_eq!(
            kept.iter().map(|k| k.source).collect::<Vec<_>>(),
            vec![Source::Chosen],
            "Slack gone, the chosen one stays"
        );
    })
    .await;
}

#[tokio::test]
async fn a_failed_fetch_leaves_the_letter_and_is_tried_again_later() {
    with_store(|store| async move {
        let slack = Arc::new(FakeSlack::default());
        let refresher = refresher(&store, &slack, None);
        let brian = Owner::agent("brian");
        let now = Utc::now();
        slack.set("brian", Some(bot("U2")));
        slack.failing.store(true, Ordering::SeqCst);

        refresher
            .refresh(now)
            .await
            .expect("a failed fetch is no error");
        assert_eq!(face(store.as_ref(), &brian).await.unwrap(), None);
        refresher.refresh(now + Duration::minutes(2)).await.unwrap();
        assert_eq!(slack.fetches(), 1, "not hammered");

        slack.failing.store(false, Ordering::SeqCst);
        slack.seed.store(4, Ordering::SeqCst);
        refresher
            .refresh(now + Duration::from_std(RETRY_AFTER).unwrap())
            .await
            .unwrap();
        assert_eq!(slack.fetches(), 2);
        assert_eq!(
            face(store.as_ref(), &brian).await.unwrap().unwrap().source,
            Source::Slack
        );
    })
    .await;
}

#[tokio::test]
async fn a_person_linked_to_slack_gets_their_slack_picture_and_loses_it_when_unlinked() {
    with_store(|store| async move {
        let identities = Arc::new(SqliteIdentityStore::in_memory().unwrap());
        let mut sam = Identity::new("Sam", IdentityRole::Member);
        sam.linked_ids.insert("slack".into(), "U9".into());
        identities.create(&sam).unwrap();
        let unlinked = Identity::new("Kit", IdentityRole::Member);
        identities.create(&unlinked).unwrap();
        let slack = Arc::new(FakeSlack::default());
        slack.seed.store(8, Ordering::SeqCst);
        let refresher = refresher(
            &store,
            &slack,
            Some(identities.clone() as Arc<dyn IdentityStore>),
        );
        let owner = Owner::identity(sam.id.to_string());

        refresher.refresh(Utc::now()).await.unwrap();
        let kept = store.kept_for(&owner).await.unwrap();
        assert_eq!(kept[0].origin.as_deref(), Some("member:U9"));
        assert!(store
            .kept_for(&Owner::identity(unlinked.id.to_string()))
            .await
            .unwrap()
            .is_empty());

        sam.linked_ids.remove("slack");
        identities.update(&sam).unwrap();
        refresher.refresh(Utc::now()).await.unwrap();
        assert!(store.kept_for(&owner).await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn nothing_is_fetched_or_removed_before_slack_has_started() {
    with_store(|store| async move {
        let slack = Arc::new(FakeSlack::default());
        let allan = Owner::agent("allan");
        store
            .put_slack(&allan, &slack_picture(7, "member:acme:U1"))
            .await
            .unwrap();
        slack.starting.store(true, Ordering::SeqCst);
        let refresher = refresher(&store, &slack, None);

        refresher.refresh(Utc::now()).await.unwrap();
        assert_eq!(
            face(store.as_ref(), &allan).await.unwrap().unwrap().source,
            Source::Slack,
            "kept while Slack starts"
        );
        assert_eq!(slack.fetches(), 0);

        slack.starting.store(false, Ordering::SeqCst);
        refresher.refresh(Utc::now()).await.unwrap();
        assert_eq!(
            face(store.as_ref(), &allan).await.unwrap(),
            None,
            "allan has no Slack identity once it's up"
        );
    })
    .await;
}

#[tokio::test]
async fn everyone_linked_to_slack_gets_a_picture_however_many_there_are() {
    with_store(|store| async move {
        let identities = Arc::new(SqliteIdentityStore::in_memory().unwrap());
        for n in 0..505 {
            let mut person = Identity::new(format!("Person {n}"), IdentityRole::Member);
            person.linked_ids.insert("slack".into(), format!("U{n}"));
            identities.create(&person).unwrap();
        }
        let slack = Arc::new(FakeSlack::default());
        refresher(&store, &slack, Some(identities as Arc<dyn IdentityStore>))
            .refresh(Utc::now())
            .await
            .unwrap();
        assert_eq!(store.kept(Kind::Identity).await.unwrap().len(), 505);
    })
    .await;
}
