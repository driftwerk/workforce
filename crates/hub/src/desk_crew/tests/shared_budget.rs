//! One Act fully budget for Sarah, Acme's boss on the Mac, counted across
//! the Mac and every machine of her group (designs/act-fully-machines-design.md): each
//! machine says what its sessions spent, the Mac counts it with its own,
//! tells each machine what was spent everywhere else, and once the total
//! reaches her budget she acts fully nowhere. A machine that doesn't count
//! the budget (an older engine) is never told Act fully is in force.

use super::*;
use workforce_common::oversight::Level;

use super::super::relay::{RelayAnswer, RelayCall};
use super::super::relayed_decide::TAKE;
use super::super::served::ServedStatus;

const SARAH_FULL: &str = "[oversight]\nenabled = true\ngroup = \"acme\"\nlevel = \"full\"\n\
    [oversight.full]\nrepos = []\nworkflows = []\nbudget_usd_per_day = 25\n";

#[tokio::test(flavor = "multi_thread")]
async fn one_budget_is_counted_across_the_mac_and_every_machine_and_ends_act_fully_everywhere() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_sarah, crew, d, _llm) = overseeing(dir.path(), pool, "ok", SARAH_FULL, &[]).await;
        let (s1, s2, s3) = (
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
        );
        d.change_layout(|l| {
            l.create(
                "acme",
                "Acme",
                &[
                    format!("session:devbox/{s1}"),
                    format!("session:atlas/{s2}"),
                    format!("session:oldbox/{s3}"),
                ],
                None,
            )
        })
        .await
        .unwrap();
        crew.read_spent_today(&crew.overseers()).await;
        let (_, o) = crew.overseer_named("desk").expect("Sarah oversees Acme");
        assert_eq!(crew.level_now("desk", &o).0, Level::Full);
        // What each machine says it spent; oldbox runs an engine that says
        // nothing of it.
        let spent: Arc<parking_lot::Mutex<HashMap<String, f64>>> =
            Arc::new(parking_lot::Mutex::new(HashMap::from([
                ("devbox".to_string(), 4.0),
                ("atlas".to_string(), 6.0),
            ])));
        let seen: Arc<parking_lot::Mutex<Vec<RelayCall>>> = Arc::default();
        let poller = {
            let (crew, seen, spent) = (Arc::clone(&crew), Arc::clone(&seen), Arc::clone(&spent));
            tokio::spawn(async move {
                loop {
                    for c in crew.relay_take(Duration::from_millis(200)).await {
                        seen.lock().push(c.clone());
                        let mut body = json!({"full": c.input["inForce"] == true, "decisions": []});
                        if let Some(usd) = spent.lock().get(&c.machine) {
                            body["countFrom"] = c.input["countFrom"].clone();
                            body["spentUsd"] = json!(usd);
                        }
                        crew.relay_answer(
                            c.id,
                            RelayAnswer {
                                status: ServedStatus::Ok,
                                body,
                            },
                        );
                    }
                }
            })
        };
        let takes_to = |machine: &str| -> Vec<RelayCall> {
            seen.lock()
                .iter()
                .filter(|c| c.tool == TAKE && c.machine == machine)
                .cloned()
                .collect()
        };
        // Once each has said, devbox and atlas are told it is in force, each
        // with what the other spent (the Mac spent nothing).
        let told = eventually("devbox told it is in force", async || {
            takes_to("devbox")
                .into_iter()
                .rev()
                .find(|c| c.input["inForce"] == true && c.input["spentElsewhereUsd"] == 6.0)
        })
        .await;
        assert_eq!(
            told.input["countFrom"],
            takes_to("devbox")[0].input["countFrom"]
        );
        eventually("atlas told it is in force", async || {
            takes_to("atlas")
                .into_iter()
                .find(|c| c.input["inForce"] == true && c.input["spentElsewhereUsd"] == 4.0)
        })
        .await;
        assert_eq!(crew.level_now("desk", &o).0, Level::Full);
        crew.read_spent_today(&crew.overseers()).await;
        assert_eq!(
            d.full_budget_of("desk").unwrap().spent_usd,
            Some(10.0),
            "the Floor shows the one budget's spend"
        );
        // Kept, so a restart doesn't forget an offline machine's spend.
        let kept = d.store().machine_spent_all().await.unwrap();
        assert!(
            kept.iter().any(|k| k.machine == "devbox" && k.usd == 4.0),
            "{kept:?}"
        );
        assert!(
            kept.iter().any(|k| k.machine == "atlas" && k.usd == 6.0),
            "{kept:?}"
        );

        // devbox spends on: $4 + $22 is over $25. Act fully ends on the Mac,
        // and every take after says it isn't in force.
        spent.lock().insert("devbox".into(), 22.0);
        eventually("over the budget here", async || {
            (crew.level_now("desk", &o).0 == Level::Act).then_some(())
        })
        .await;
        let (_, why) = crew.level_now("desk", &o);
        assert!(why.unwrap().contains("$28.00 of $25.00"));
        let n = seen.lock().len();
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        assert!(
            seen.lock()[n..]
                .iter()
                .all(|c| c.tool != TAKE || c.input["inForce"] == false),
            "no machine is told Act fully is in force"
        );
        // A late answer from before never lowers what was counted.
        crew.machine_reported("desk", "devbox", "acme", crew.count_from(&o).unwrap(), 1.0)
            .await;
        assert_eq!(crew.level_now("desk", &o).0, Level::Act);

        // oldbox never said what it spent: it was never told.
        let old = takes_to("oldbox");
        assert!(!old.is_empty());
        assert!(old.iter().all(|c| c.input["inForce"] == false), "{old:?}");
        poller.abort();
    })
    .await;
}

/// Until what the machines spent has been read back after a start, a boss
/// here doesn't act fully: a restart never forgets it, nor what a machine
/// said in all before its grants were counted apart.
#[tokio::test(flavor = "multi_thread")]
async fn act_fully_waits_until_what_the_machines_spent_is_read_back() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (_sarah, crew, d, _llm) =
            overseeing(dir.path(), pool.clone(), "ok", SARAH_FULL, &[]).await;
        let remote = uuid::Uuid::new_v4();
        d.change_layout(|l| l.create("acme", "Acme", &[format!("session:devbox/{remote}")], None))
            .await
            .unwrap();
        let (_, o) = eventually("Sarah oversees Acme", async || crew.overseer_named("desk")).await;
        let from = crew.count_from(&o).unwrap();
        // What devbox said before a restart, as kept.
        d.store()
            .machine_spent_put(&workforce_desk::machine_spent_store::MachineSpent {
                agent: "desk".into(),
                machine: "devbox".into(),
                grant: "acme".into(),
                count_from: from,
                usd: 30.0,
            })
            .await
            .unwrap();
        // atlas, as an older hub kept it: one count for the machine.
        pool.get()
            .await
            .unwrap()
            .execute(
                "INSERT INTO machine_spent (agent, machine, count_from, usd, updated_at) \
                 VALUES ('desk', 'atlas', $1, 5.0, now())",
                &[&from],
            )
            .await
            .unwrap();
        // The hub starts again: nothing is known until it is read back.
        crew.forget_machines_spent();
        assert_eq!(crew.machines_spent("desk", from), None, "not read yet");
        assert_eq!(crew.level_now("desk", &o).0, Level::Act, "Act fully waits");
        crew.read_spent_today(&crew.overseers()).await;
        assert_eq!(crew.machines_spent("desk", from), Some(35.0));
        let (level, why) = crew.level_now("desk", &o);
        assert_eq!(level, Level::Act);
        assert!(why.unwrap().contains("$35.00 of $25.00"));
        // A count for another day counts nothing today.
        let earlier = from - chrono::Duration::days(1);
        assert_eq!(crew.machines_spent("desk", earlier), Some(0.0));
    })
    .await;
}
