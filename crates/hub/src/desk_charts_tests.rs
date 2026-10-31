//! A chart and the spec it was drawn from: where each is shown.

use super::*;
use workforce_agent::runner::{ProgressEvent, SessionContext};
use workforce_agent::tools::image_gen::{GENERATED_IMAGES_KEY, SPEC_KEY};
use workforce_common::channels::desk::{DeskPlace, DeskScope, DeskTarget};
use workforce_common::rich::RichFeatures;

const SPEC: &[u8] = br#"{"kind":"bar","labels":["A"],"series":[{"data":[1.0]}],"stacked":false,"horizontal":false}"#;

/// A chart staged as its picture and its spec, as the `chart` tool stages
/// it, and the marker naming both.
fn chart(session: Option<&str>) -> (ArtifactId, ArtifactId, serde_json::Value) {
    let picture = staged("scout", session, &png(4, 3), "image/png", "q3.png");
    let spec = staged("scout", session, SPEC, CHART_SPEC_MEDIA_TYPE, "q3.json");
    let marker = json!({
        "artifact_id": picture.as_str(), "media_type": "image/png", "filename": "q3.png",
        "caption": "Pro leads.",
        SPEC_KEY: { "artifact_id": spec.as_str(), "media_type": CHART_SPEC_MEDIA_TYPE, "filename": "q3.json" }
    });
    (picture, spec, marker)
}

fn file(marker: &serde_json::Value) -> GeneratedImageRef {
    workforce_agent::tools::image_gen::parse_generated_images(
        &json!({ GENERATED_IMAGES_KEY: [marker] }),
    )
    .remove(0)
}

fn reading(channel: &str, rendered: &str) -> Session {
    let mut session = Session::new(channel, "person-1", "scout");
    session.set_rendered_rich(RichFeatures::from_names([rendered]));
    session
}

/// Only a Desk conversation whose reader said it draws chart specs is
/// shown the spec, captioned as the picture was; anyone else, and a file
/// with no spec, is shown the file.
#[test]
fn a_chart_is_its_spec_only_to_a_desk_reader_that_draws_chart_specs() {
    let (picture, spec, marker) = chart(None);
    let drawn = file(&marker);
    let shown = |session: Option<&Session>| shown_to(session, &drawn);

    let desk = reading(DESK_CHANNEL, "tables,charts,chart_specs");
    let it = shown(Some(&desk));
    assert_eq!(
        (
            it.artifact_id,
            it.media_type.as_str(),
            it.filename.as_deref()
        ),
        (spec.clone(), CHART_SPEC_MEDIA_TYPE, Some("q3.json"))
    );
    assert_eq!(it.caption.as_deref(), Some("Pro leads."));
    assert!(it.spec.is_none());

    for (session, why) in [
        (
            Some(reading(DESK_CHANNEL, "tables,charts")),
            "an older Desk",
        ),
        (
            Some(reading("ios", "charts,chart_specs")),
            "not a Desk conversation",
        ),
        (None, "no conversation known"),
    ] {
        assert_eq!(shown(session.as_ref()).artifact_id, picture, "{why}");
    }
    let plain = GeneratedImageRef {
        spec: None,
        ..drawn.clone()
    };
    assert_eq!(shown_to(Some(&desk), &plain).artifact_id, picture);
}

/// The answer a Desk turn saves carries what its stream showed: the spec
/// where its reader draws charts, the picture where it does not.
#[test]
fn a_desk_turn_s_answer_carries_the_chart_its_reader_draws() {
    let (picture, spec, marker) = chart(None);
    let invocation = workforce_agent::runner::ToolInvocation {
        tool_name: "chart".into(),
        input: json!({}),
        output: json!({ GENERATED_IMAGES_KEY: [marker] }),
        success: true,
        policy_decision: "allow".into(),
        duration_ms: 1,
        denial_reason: None,
        error: None,
    };
    for (rendered, expected, media_type) in [
        ("charts,chart_specs", &spec, CHART_SPEC_MEDIA_TYPE),
        ("charts", &picture, "image/png"),
    ] {
        let mut session = reading(DESK_CHANNEL, rendered);
        session.add_assistant_message("Here it is.", 3);
        crate::turn_loop::carry_delivered_files(&mut session, std::slice::from_ref(&invocation));
        let files = &session
            .messages
            .last()
            .unwrap()
            .desk
            .as_ref()
            .unwrap()
            .files;
        assert_eq!(
            (files[0].artifact_id.as_str(), files[0].media_type.as_str()),
            (expected.as_str(), media_type),
            "{rendered}"
        );
    }
}

/// The sink sends a live Desk turn's stream the spec, and a run nobody
/// attends delivers it into the conversation, when that conversation's
/// reader draws charts; kept and filed under the conversation, as the
/// picture would have been.
#[tokio::test(flavor = "multi_thread")]
async fn the_sink_delivers_a_chart_s_spec_where_the_conversation_draws_it() {
    let dir = tempfile::tempdir().unwrap();
    let (scout, _llm) = agent(dir.path(), "scout", "ok", Duration::ZERO).await;
    let sink = crate::mcp_gate::media_sinks(&scout).deliver;
    let person = uuid::Uuid::new_v4().to_string();
    let mut conversation = scout
        .sessions
        .get_or_create(DESK_CHANNEL, &person, "scout")
        .unwrap();
    conversation.set_rendered_rich(RichFeatures::from_names(["charts,chart_specs"]));
    scout.sessions.save(&conversation).unwrap();
    let dm = DeskTarget::of_turn(&DeskPlace::Dm, DESK_CHANNEL, None, false);
    let as_run = |session: &str, attended: bool| SessionContext {
        desk: Some(DeskScope {
            target: dm.clone(),
            attended,
            owner: person.clone(),
        }),
        ..context(session, "scout")
    };

    let (_, spec, marker) = chart(None);
    let (progress, mut stream) = tokio::sync::mpsc::channel(4);
    workforce_agent::runner::scope_turn(
        Some(progress),
        None,
        sink.deliver_file(&as_run(&conversation.id, true), &file(&marker)),
    )
    .await
    .unwrap();
    let Ok(ProgressEvent::File(live)) = stream.try_recv() else {
        panic!("the spec goes down the turn's stream");
    };
    assert_eq!(
        (
            live.artifact_id.as_str(),
            live.media_type.as_str(),
            live.caption.as_deref()
        ),
        (spec.as_str(), CHART_SPEC_MEDIA_TYPE, Some("Pro leads."))
    );
    assert_eq!(
        store()
            .metadata("scout", &spec)
            .unwrap()
            .unwrap()
            .session_id,
        Some(conversation.id.clone()),
        "filed under the conversation, so its reader may download it"
    );

    let (_, spec, marker) = chart(None);
    sink.deliver_file(&as_run("scheduler:job-1", false), &file(&marker))
        .await
        .unwrap();
    let head = scout.sessions.get(&conversation.id).unwrap().unwrap();
    let mark = head.messages.last().unwrap().desk.clone().unwrap();
    assert_eq!(
        (
            mark.files[0].artifact_id.as_str(),
            mark.files[0].media_type.as_str()
        ),
        (spec.as_str(), CHART_SPEC_MEDIA_TYPE)
    );
}

/// A conversation whose reader does not draw charts is delivered the
/// picture, as before.
#[tokio::test(flavor = "multi_thread")]
async fn the_sink_delivers_the_picture_where_the_conversation_does_not_draw_charts() {
    let dir = tempfile::tempdir().unwrap();
    let (scout, _llm) = agent(dir.path(), "scout", "ok", Duration::ZERO).await;
    let sink = crate::mcp_gate::media_sinks(&scout).deliver;
    let conversation = scout
        .sessions
        .get_or_create(DESK_CHANNEL, "person-2", "scout")
        .unwrap();
    let (picture, _, marker) = chart(None);
    let ctx = SessionContext {
        desk: Some(DeskScope {
            target: DeskTarget::of_turn(&DeskPlace::Dm, DESK_CHANNEL, None, false),
            attended: true,
            owner: "person-2".into(),
        }),
        ..context(&conversation.id, "scout")
    };
    let (progress, mut stream) = tokio::sync::mpsc::channel(4);
    workforce_agent::runner::scope_turn(
        Some(progress),
        None,
        sink.deliver_file(&ctx, &file(&marker)),
    )
    .await
    .unwrap();
    let Ok(ProgressEvent::File(live)) = stream.try_recv() else {
        panic!("the picture goes down the turn's stream");
    };
    assert_eq!(
        (live.artifact_id.as_str(), live.media_type.as_str()),
        (picture.as_str(), "image/png")
    );
}
