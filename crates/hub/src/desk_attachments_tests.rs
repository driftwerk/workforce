use super::*;

fn png(width: u32, height: u32) -> Vec<u8> {
    let mut b = vec![
        0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n', 0, 0, 0, 13,
    ];
    b.extend_from_slice(b"IHDR");
    b.extend_from_slice(&width.to_be_bytes());
    b.extend_from_slice(&height.to_be_bytes());
    b.extend_from_slice(&[8, 6, 0, 0, 0]);
    b
}

#[test]
fn images_tell_their_size_from_their_first_bytes() {
    assert_eq!(image_size(&png(640, 480)), Some((640, 480)));
    let mut gif = b"GIF89a".to_vec();
    gif.extend_from_slice(&[0x20, 0x03, 0x58, 0x02]);
    assert_eq!(image_size(&gif), Some((800, 600)));
    // A JPEG whose frame header follows an APP0 block.
    let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00];
    jpeg.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08, 0x01, 0xE0, 0x02, 0x80, 0x03]);
    assert_eq!(image_size(&jpeg), Some((640, 480)));
    let mut webp = b"RIFF\0\0\0\0WEBPVP8X".to_vec();
    webp.extend_from_slice(&[0; 8]);
    webp.extend_from_slice(&[0x7F, 0x02, 0x00, 0xDF, 0x01, 0x00]);
    assert_eq!(image_size(&webp), Some((640, 480)));
    assert_eq!(image_size(b"%PDF-1.7"), None);
    assert_eq!(image_size(&[0xFF, 0xD8, 0xFF]), None, "a cut-off JPEG");
}

#[test]
fn an_upload_is_what_its_bytes_say_not_what_it_is_called() {
    assert_eq!(sniff_upload(&png(1, 1), "notes.txt"), "image/png");
    assert_eq!(sniff_upload(b"%PDF-1.7\n", "photo.jpg"), "application/pdf");
    assert_eq!(
        sniff_upload(b"\0\0\0\x20ftypisom\0\0", "clip.bin"),
        "video/mp4"
    );
    assert_eq!(
        sniff_upload(b"PK\x03\x04rest", "q3.xlsx"),
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    );
    assert_eq!(sniff_upload(b"PK\x03\x04rest", "q3.pdf"), "application/zip");
    assert_eq!(sniff_upload(b"a,b\n1,2\n", "rows.csv"), "text/csv");
    assert_eq!(sniff_upload(b"just words", "readme"), "text/plain");
    assert_eq!(sniff_upload(b"just words", "fake.png"), "text/plain");
    assert_eq!(
        sniff_upload(b"\0\x01\x02binary", "data.csv"),
        "application/octet-stream"
    );
    // A multi-byte character cut off at the end of the head is still text.
    assert_eq!(
        sniff_upload("caf\u{e9}".as_bytes()[..4].as_ref(), "a.txt"),
        "text/plain"
    );
}

#[test]
fn ranges_are_served_as_asked_or_whole() {
    let h = |v: &str| HeaderValue::from_str(v).unwrap();
    assert_eq!(range_of(None, 100), Ok(None));
    assert_eq!(range_of(Some(&h("bytes=0-9")), 100), Ok(Some((0, 9))));
    assert_eq!(range_of(Some(&h("bytes=90-")), 100), Ok(Some((90, 99))));
    assert_eq!(range_of(Some(&h("bytes=-10")), 100), Ok(Some((90, 99))));
    assert_eq!(range_of(Some(&h("bytes=50-500")), 100), Ok(Some((50, 99))));
    assert_eq!(range_of(Some(&h("bytes=100-")), 100), Err(()));
    assert_eq!(range_of(Some(&h("bytes=0-1,5-6")), 100), Ok(None));
    assert_eq!(range_of(Some(&h("items=0-1")), 100), Ok(None));
    assert_eq!(range_of(Some(&h("bytes=9-1")), 100), Ok(None));
}

#[test]
fn a_file_name_keeps_no_directories() {
    assert_eq!(safe_filename("../../etc/passwd"), "passwd");
    assert_eq!(safe_filename("  "), "upload");
    assert_eq!(safe_filename("a\u{7}b.pdf"), "ab.pdf");
    assert_eq!(safe_filename(".."), "upload");
    assert_eq!(safe_filename("..\\..\\Windows\\win.ini"), "win.ini");
    assert_eq!(safe_filename("reports/q3/"), "q3");
}

#[test]
fn limits_come_from_desk_config() {
    let desk = workforce_common::settings::DeskConfig {
        attachment_max_mb: 5,
        attachment_retention_days: 2,
        ..Default::default()
    };
    let limits = Limits::from(&desk);
    assert_eq!(limits.max_bytes, 5 * 1024 * 1024);
    assert_eq!(limits.retention, Duration::from_secs(2 * 24 * 3600));
    assert_eq!(Limits::default().max_bytes, 100 * 1024 * 1024);
}

/// The process-wide artifact store, which the first caller installs for
/// good; agents keep their writes apart.
fn store() -> Arc<dyn ArtifactStore> {
    static DIR: std::sync::LazyLock<tempfile::TempDir> = std::sync::LazyLock::new(|| {
        let dir = tempfile::TempDir::new().expect("tempdir");
        workforce_agent::artifacts::set_global_store(
            workforce_agent::artifacts::FsArtifactStore::new(dir.path().to_path_buf())
                .expect("store"),
        );
        dir
    });
    std::sync::LazyLock::force(&DIR);
    global_store().expect("installed")
}

/// `bytes` staged in `session` of `agent`'s as a delivering tool stages
/// them.
fn staged(
    agent: &str,
    session: Option<&str>,
    bytes: &[u8],
    media_type: &str,
    name: &str,
) -> ArtifactId {
    let descriptor = ArtifactDescriptor {
        session_id: session.map(str::to_string),
        ..ArtifactDescriptor::from_context("browser.pdf", ContentType::OctetStream, agent)
            .with_file_meta(name, media_type)
    };
    store()
        .write(
            agent,
            descriptor,
            workforce_agent::artifacts::ArtifactBody::Bytes(bytes.to_vec()),
        )
        .expect("staged")
        .id
}

/// An agent `id` whose model answers everything with `reply`, after
/// `delay`. A test mounts what its model says first at priority 1.
async fn agent(
    dir: &Path,
    id: &str,
    reply: &str,
    delay: Duration,
) -> (Arc<crate::serve::AgentService>, wiremock::MockServer) {
    use crate::serve::Compose as _;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, ResponseTemplate};
    let (config, server) = crate::serve::testing::mock_llm_config(&dir.join(id), |c| {
        c.agents = vec![workforce_common::settings::AgentConfig {
            id: id.into(),
            ..Default::default()
        }];
    })
    .await;
    let chunk = |body: serde_json::Value| format!("data: {body}\n\n");
    let sse = [
        chunk(json!({"id": "c1", "object": "chat.completion.chunk", "model": "mock",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": reply}, "finish_reason": null}]})),
        chunk(json!({"id": "c1", "object": "chat.completion.chunk", "model": "mock",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}})),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({"stream": true})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse)
                .set_delay(delay),
        )
        .with_priority(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "id": "c1", "object": "chat.completion", "model": "mock",
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": reply}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
                }))
                .set_delay(delay),
        )
        .with_priority(3)
        .mount(&server)
        .await;
    let service = crate::serve::AgentService::from_config(config, None)
        .await
        .unwrap();
    std::fs::create_dir_all(&service.workspace).unwrap();
    (service, server)
}

/// A gateway serving `service`, with Alice and Bob as members: who each
/// is, and what signs them in.
fn gateway(
    service: &Arc<crate::serve::AgentService>,
) -> (Arc<GatewayState>, [(String, String); 2]) {
    use workforce_identity::store::IdentityStore;
    use workforce_identity::types::{Identity, IdentityRole};
    let fixture = crate::remote_mcp::testing::fixture(false);
    let jwt =
        crate::jwt::JwtConfig::new("a-test-secret-of-at-least-32-bytes!!".into(), 3600).unwrap();
    let people = ["Alice", "Bob"].map(|name| {
        let person = Identity::new(name, IdentityRole::Member);
        fixture.identities.create(&person).unwrap();
        let token = crate::jwt::generate_token_with_identity(
            &jwt,
            name,
            person.role,
            Some(person.id.to_string()),
        )
        .unwrap();
        (person.id.to_string(), token)
    });
    let agent = service.agent_id.to_string();
    let state = Arc::new(GatewayState {
        registry: Arc::new(crate::agent_registry::AgentRegistry::from_parts(
            [(agent.clone(), Arc::clone(service))].into(),
            agent,
        )),
        broadcast_tx: tokio::sync::broadcast::channel(4096).0,
        jwt_config: Some(jwt),
        ..Arc::try_unwrap(fixture.state)
            .ok()
            .expect("the fixture's only handle")
    });
    service.set_agent_registry(Arc::downgrade(&state.registry));
    service.set_image_broadcast(state.broadcast_tx.clone());
    (state, people)
}

/// The next text frame on `socket`.
async fn frame(
    socket: &mut (impl futures::Stream<
        Item = Result<
            tokio_tungstenite::tungstenite::Message,
            tokio_tungstenite::tungstenite::Error,
        >,
    > + Unpin),
) -> serde_json::Value {
    use futures::StreamExt;
    loop {
        let next = tokio::time::timeout(Duration::from_secs(30), socket.next())
            .await
            .expect("a frame")
            .unwrap()
            .unwrap();
        if let Ok(text) = next.to_text() {
            return serde_json::from_str(text).unwrap();
        }
    }
}

/// Frames on `socket` until one of type `kind`, which it answers with.
async fn until(
    socket: &mut (impl futures::Stream<
        Item = Result<
            tokio_tungstenite::tungstenite::Message,
            tokio_tungstenite::tungstenite::Error,
        >,
    > + Unpin),
    kind: &str,
) -> serde_json::Value {
    loop {
        let next = frame(socket).await;
        if next["type"] == kind {
            return next;
        }
    }
}

/// `state`'s gateway on a port of its own: where it listens.
async fn serve_on_a_port(state: &Arc<GatewayState>) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = crate::gateway::router(Arc::clone(state));
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
    });
    addr
}

fn context(session: &str, agent: &str) -> workforce_agent::runner::SessionContext {
    workforce_agent::runner::SessionContext {
        id: Some(session.to_string()),
        agent_id: Some(agent.to_string()),
        ..Default::default()
    }
}

async fn call(
    state: &Arc<GatewayState>,
    request: axum::http::Request<axum::body::Body>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    use tower::ServiceExt;
    let response = crate::gateway::router(Arc::clone(state))
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), 64 << 20)
        .await
        .unwrap()
        .to_vec();
    (status, headers, body)
}

fn get(
    uri: &str,
    token: Option<&str>,
    range: Option<&str>,
) -> axum::http::Request<axum::body::Body> {
    let mut request = axum::http::Request::get(uri);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    if let Some(range) = range {
        request = request.header("range", range);
    }
    request.body(axum::body::Body::empty()).unwrap()
}

/// A client that declared nothing (iOS) is sent a delivered image, inline
/// as before, and never a PDF; a client that declared `*/*` is sent both,
/// each with its reference and size.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_declared_nothing_is_never_sent_a_pdf() {
    let dir = tempfile::tempdir().unwrap();
    let (scout, _llm) = agent(dir.path(), "scout", "ok", Duration::ZERO).await;
    let (state, [(_, alice), _]) = gateway(&scout);
    let addr = serve_on_a_port(&state).await;
    let connect = |accept: &str| {
        let url = format!("ws://{addr}/ws?token={alice}&channel=desk{accept}");
        async move { tokio_tungstenite::connect_async(url).await.unwrap().0 }
    };
    let mut ios = connect("").await;
    // Its session made before the second client asks for it: two clients
    // connecting at once can each make one.
    let session = frame(&mut ios).await["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut desk = connect("&accept=*/*").await;
    assert_eq!(frame(&mut desk).await["session_id"], session.as_str());

    let sink = crate::mcp_gate::media_sinks(&scout).deliver;
    let pdf = staged(
        "scout",
        Some(&session),
        b"%PDF-1.7 report",
        "application/pdf",
        "report.pdf",
    );
    let chart = staged(
        "scout",
        Some(&session),
        &png(4, 3),
        "image/png",
        "chart.png",
    );
    let ctx = context(&session, "scout");
    sink.deliver(&ctx, &pdf, "application/pdf", Some("report.pdf"), None)
        .await
        .unwrap();
    sink.deliver(
        &ctx,
        &chart,
        "image/png",
        Some("chart.png"),
        Some("the chart"),
    )
    .await
    .unwrap();

    let first = frame(&mut ios).await;
    assert_eq!(first["type"], "image");
    assert_eq!(
        first["attachments"],
        json!([{"filename": "chart.png", "mime_type": "image/png",
                "data": base64_of(&png(4, 3))}]),
        "the image alone, as such a client was always sent it"
    );
    let shown = [frame(&mut desk).await, frame(&mut desk).await];
    assert_eq!(
        shown[0]["attachments"],
        json!([{"filename": "report.pdf", "mime_type": "application/pdf", "data": "",
                "artifact_id": pdf.as_str(), "size": 15}])
    );
    assert_eq!(shown[1]["attachments"][0]["artifact_id"], chart.as_str());
    assert_eq!(shown[1]["attachments"][0]["width"], 4);
    assert_eq!(shown[1]["attachments"][0]["height"], 3);
    assert_eq!(shown[1]["content"], "the chart");
}

/// In a live Desk turn a delivered file goes down the turn's own stream,
/// described; in a run nobody attends that answers to a Desk conversation
/// it lands there as a message of its own, in its owner's conversation
/// whoever the run is as, kept for the conversation and pushed to whoever
/// watches it.
#[tokio::test(flavor = "multi_thread")]
async fn a_desk_turn_s_file_rides_its_stream_and_an_unattended_run_s_lands_in_its_conversation() {
    use workforce_agent::runner::{ProgressEvent, SessionContext};
    use workforce_common::channels::desk::{DeliveryKind, DeskPlace, DeskScope, DeskTarget};
    let dir = tempfile::tempdir().unwrap();
    let (scout, _llm) = agent(dir.path(), "scout", "ok", Duration::ZERO).await;
    let sink = crate::mcp_gate::media_sinks(&scout).deliver;
    let person = uuid::Uuid::new_v4().to_string();
    let dm = DeskTarget::of_turn(&DeskPlace::Dm, DESK_CHANNEL, None, false);
    let conversation = scout
        .sessions
        .get_or_create(DESK_CHANNEL, &person, "scout")
        .unwrap();
    let as_run = |session: &str, attended: bool| SessionContext {
        user_id: Some("runs-as-someone-else".into()),
        desk: Some(DeskScope {
            target: dm.clone(),
            attended,
            owner: person.clone(),
        }),
        ..context(session, "scout")
    };

    let chart = staged("scout", None, &png(4, 3), "image/png", "chart.png");
    let (progress, mut stream) = tokio::sync::mpsc::channel(4);
    workforce_agent::runner::scope_turn(
        Some(progress),
        None,
        sink.deliver(
            &as_run(&conversation.id, true),
            &chart,
            "image/png",
            Some("chart.png"),
            Some("the chart"),
        ),
    )
    .await
    .unwrap();
    let Ok(ProgressEvent::File(live)) = stream.try_recv() else {
        panic!("the file goes down the turn's stream");
    };
    assert_eq!(
        (
            live.artifact_id.as_str(),
            live.agent_id.as_str(),
            live.width,
            live.height,
            live.caption.as_deref()
        ),
        (chart.as_str(), "scout", Some(4), Some(3), Some("the chart"))
    );

    let mut pushes = crate::session_push::subscribe_for_tests();
    let pdf_bytes = b"%PDF-1.7 the report";
    let pdf = staged("scout", None, pdf_bytes, "application/pdf", "report.pdf");
    sink.deliver(
        &as_run("scheduler:job-1", false),
        &pdf,
        "application/pdf",
        Some("report.pdf"),
        Some("your report"),
    )
    .await
    .unwrap();
    let head = scout.sessions.get(&conversation.id).unwrap().unwrap();
    let saved = head.messages.last().unwrap();
    assert_eq!(saved.content, "your report");
    let mark = saved.desk.as_ref().unwrap();
    assert_eq!(mark.delivery, Some(DeliveryKind::File));
    assert_eq!(
        (
            mark.files[0].artifact_id.as_str(),
            mark.files[0].size,
            mark.files[0].media_type.as_str()
        ),
        (pdf.as_str(), pdf_bytes.len() as u64, "application/pdf")
    );
    assert_eq!(
        saved.attachments.as_ref().unwrap()[0]
            .artifact_id
            .as_deref(),
        Some(pdf.as_str())
    );
    let pushed = std::iter::from_fn(|| pushes.try_recv().ok())
        .find(|f| f.session_id == head.id && f.msg_type == "image")
        .expect("pushed to whoever watches the conversation");
    assert_eq!(pushed.id.as_deref(), Some(saved.id.as_str()));
    assert_eq!(
        store().metadata("scout", &pdf).unwrap().unwrap().session_id,
        Some(head.id.clone()),
        "filed under the conversation, so its reader may download it"
    );
}

/// A file a workflow run delivers into its person's Desk conversation is
/// served to them, though the run filed it under its own instance.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_a_workflow_run_delivers_into_desk_is_served_to_its_person() {
    use workforce_common::channels::desk::{DeskPlace, DeskScope, DeskTarget};
    let dir = tempfile::tempdir().unwrap();
    let (scout, _llm) = agent(dir.path(), "scout", "ok", Duration::ZERO).await;
    let (state, [(alice_id, alice), (_, bob)]) = gateway(&scout);
    let sink = crate::mcp_gate::media_sinks(&scout).deliver;
    let report = workforce_agent::runner::CURRENT_ARTIFACT_SCOPE.sync_scope(
        "workflow:instance-1".to_string(),
        || {
            workforce_agent::runner::CURRENT_USER_ID.sync_scope(alice_id.clone(), || {
                let descriptor = ArtifactDescriptor::from_context(
                    "export.csv",
                    ContentType::OctetStream,
                    "scout",
                )
                .with_file_meta("report.csv", "text/csv");
                store()
                    .write(
                        "scout",
                        descriptor,
                        workforce_agent::artifacts::ArtifactBody::Bytes(b"a,b\n1,2\n".to_vec()),
                    )
                    .unwrap()
                    .id
            })
        },
    );
    sink.deliver(
        &workforce_agent::runner::SessionContext {
            user_id: Some(alice_id.clone()),
            desk: Some(DeskScope {
                target: DeskTarget::of_turn(&DeskPlace::Dm, DESK_CHANNEL, None, false),
                attended: false,
                owner: alice_id.clone(),
            }),
            ..context("workflow:instance-1", "scout")
        },
        &report,
        "text/csv",
        Some("report.csv"),
        Some("your report"),
    )
    .await
    .unwrap();
    let uri = format!("/api/agents/scout/attachments/{}", report.as_str());
    let (status, _, body) = call(&state, get(&uri, Some(&alice), None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"a,b\n1,2\n");
    let (status, ..) = call(&state, get(&uri, Some(&bob), None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "nor anyone else's");
}

/// A conversation's files for `view_attachment` are those its people
/// attached: a message that names another person's upload, or some other
/// tool's artifact, lists neither.
#[tokio::test(flavor = "multi_thread")]
async fn a_conversation_s_files_are_what_its_people_attached_not_what_a_message_names() {
    use workforce_common::session::SessionAttachment;
    use workforce_skills::adapters::image_view::ConversationFiles as _;
    let dir = tempfile::tempdir().unwrap();
    let (scout, _llm) = agent(dir.path(), "scout", "ok", Duration::ZERO).await;
    let mut dm = scout
        .sessions
        .get_or_create(DESK_CHANNEL, "alice", "scout")
        .unwrap();
    let upload = |by: &str, session: &str| {
        let descriptor = ArtifactDescriptor {
            session_id: Some(session.to_string()),
            user_id: Some(by.to_string()),
            ..ArtifactDescriptor::from_context(
                SESSION_UPLOAD_PRODUCER,
                ContentType::OctetStream,
                "scout",
            )
            .with_file_meta("photo.png", "image/png")
        };
        store()
            .write(
                "scout",
                descriptor,
                workforce_agent::artifacts::ArtifactBody::Bytes(png(1, 1)),
            )
            .unwrap()
            .id
    };
    let hers = upload("alice", &dm.id);
    let his = upload("bob", "bob-dm");
    let report = staged(
        "scout",
        Some("elsewhere"),
        b"%PDF-1.7",
        "application/pdf",
        "r.pdf",
    );
    dm.add_user_message_with_attachments(
        "look at these",
        None,
        Some(
            [&hers, &his, &report]
                .map(|id| SessionAttachment::from_artifact("f", "image/png", id.as_str()))
                .to_vec(),
        ),
    );
    scout.sessions.save(&dm).unwrap();
    let listed = SessionFiles::new("scout", Arc::clone(&scout.sessions))
        .files(&dm.id)
        .await
        .unwrap();
    assert_eq!(
        listed.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(),
        [hers.as_str()]
    );
}

/// A Desk turn whose tool delivers a file is sent it in the turn's own
/// stream, named by the turn, and its answer carries it.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_a_desk_turn_s_tool_delivers_is_in_its_stream_and_on_its_answer() {
    use futures::SinkExt;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, ResponseTemplate};
    store();
    let dir = tempfile::tempdir().unwrap();
    let (scout, llm) = agent(dir.path(), "scout", "Here are the rows.", Duration::ZERO).await;
    let chunk = |body: serde_json::Value| format!("data: {body}\n\n");
    let arguments = json!({"data": [{"region": "north", "total": 3}], "filename": "rows.csv"});
    let call = [
        chunk(
            json!({"id": "c0", "object": "chat.completion.chunk", "model": "mock",
            "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{
                "index": 0, "id": "call_1", "type": "function",
                "function": {"name": "export_csv", "arguments": arguments.to_string()}
            }]}, "finish_reason": null}]}),
        ),
        chunk(
            json!({"id": "c0", "object": "chat.completion.chunk", "model": "mock",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}}),
        ),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({"stream": true})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(call),
        )
        .with_priority(1)
        .up_to_n_times(1)
        .mount(&llm)
        .await;
    let (state, [(alice_id, alice), _]) = gateway(&scout);
    let addr = serve_on_a_port(&state).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(format!(
        "ws://{addr}/ws?token={alice}&agent_id=scout&channel=desk&desk=1&accept=*/*"
    ))
    .await
    .unwrap();
    until(&mut socket, "connected").await;
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            json!({"type": "chat", "content": "Export the totals.", "turn_ref": "ref-1",
                   "desk": {"place": {"kind": "dm"}, "speaker": {"kind": "person"}}})
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    let image = until(&mut socket, "image").await;
    let end = until(&mut socket, "turn_end").await;
    assert_eq!(image["turn_ref"], "ref-1");
    assert_eq!(image["id"], end["id"], "the file is the answer's");
    let sent = &image["attachments"][0];
    assert_eq!(
        (&sent["filename"], &sent["mime_type"]),
        (&json!("rows.csv"), &json!("text/csv"))
    );
    let dm = scout
        .sessions
        .find(DESK_CHANNEL, &alice_id)
        .unwrap()
        .unwrap();
    let answer = dm
        .messages
        .iter()
        .find(|m| json!(m.id) == end["id"])
        .expect("the answer is saved under the id the turn ended with");
    assert_eq!(answer.content, "Here are the rows.");
    assert_eq!(
        json!(answer.desk.as_ref().unwrap().files[0].artifact_id),
        sent["artifact_id"]
    );
}

/// `messaging.upload_file` on Desk stages the bytes as a file of the
/// agent's and delivers it into the conversation it names, the current one
/// when it names none.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_uploaded_to_desk_is_delivered_into_the_conversation() {
    use workforce_common::channels::desk::{DeskPlace, DeskScope, DeskTarget};
    use workforce_common::channels::ChannelSurface as _;
    store();
    let dir = tempfile::tempdir().unwrap();
    let (scout, _llm) = agent(dir.path(), "scout", "ok", Duration::ZERO).await;
    let person = uuid::Uuid::new_v4().to_string();
    let scope = DeskScope {
        target: DeskTarget::of_turn(&DeskPlace::Dm, DESK_CHANNEL, None, false),
        attended: true,
        owner: person.clone(),
    };
    let surface = crate::desk_surface::DeskSurface::new(&scout);
    assert!(surface.capabilities().file_upload);
    let link = workforce_agent::runner::CURRENT_USER_ID
        .scope(
            person.clone(),
            workforce_agent::runner::with_desk(
                Some(scope),
                surface.upload_file("", "../notes.txt", b"the notes", Some("notes"), None),
            ),
        )
        .await
        .unwrap();
    let dm = scout.sessions.find(DESK_CHANNEL, &person).unwrap().unwrap();
    let saved = dm.messages.last().unwrap();
    let file = &saved.desk.as_ref().unwrap().files[0];
    assert_eq!(
        (file.filename.as_str(), file.media_type.as_str(), file.size),
        ("notes.txt", "text/plain", 9)
    );
    assert_eq!(
        link,
        format!("/api/agents/scout/attachments/{}", file.artifact_id)
    );
    let id = ArtifactId::parse(&file.artifact_id).unwrap();
    assert_eq!(
        store().read("scout", &id).unwrap().into_bytes().unwrap(),
        b"the notes"
    );
}

/// The files a live Desk turn delivered go on the answer it saved, so a
/// Desk that missed them live reads them with it.
#[test]
fn a_desk_turn_s_answer_carries_the_files_it_delivered() {
    let chart = staged("scout", None, &png(4, 3), "image/png", "chart.png");
    let invocation = |success: bool| workforce_agent::runner::ToolInvocation {
        tool_name: "chart".into(),
        input: json!({}),
        output: json!({workforce_agent::tools::image_gen::GENERATED_IMAGES_KEY: [
            {"artifact_id": chart.as_str(), "media_type": "image/png", "filename": "chart.png"}
        ]}),
        success,
        policy_decision: "allow".into(),
        duration_ms: 1,
        denial_reason: None,
        error: None,
    };
    let mut session = Session::new(DESK_CHANNEL, "person-1", "scout");
    session.add_assistant_message("Here it is.", 3);
    crate::turn_loop::carry_delivered_files(&mut session, &[invocation(true), invocation(false)]);
    let reply = session.messages.last().unwrap();
    let files = &reply.desk.as_ref().unwrap().files;
    assert_eq!(files.len(), 1, "only what a tool that succeeded delivered");
    assert_eq!(
        (files[0].artifact_id.as_str(), files[0].width),
        (chart.as_str(), Some(4))
    );
    assert_eq!(reply.attachments.as_ref().unwrap().len(), 1);
}

fn base64_of(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// On a hub with the desk on: a PDF staged during a Desk DM turn rides on
/// that turn's answer and is still there when the history is read again;
/// one delivered after the answer is a message of its own. Its bytes are
/// served to the person whose conversation it is, in part when asked, and
/// to nobody else. An upload is sniffed, capped, put on the person's next
/// message, and `view_attachment` finds it.
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn a_desk_dms_files_ride_on_its_messages_and_are_served_to_their_reader_alone() {
    use workforce_desk::Conversations as _;
    use workforce_postgres::test_support::with_test_pool;
    use workforce_skills::adapters::image_view::ConversationFiles as _;
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let (scout, _llm) = agent(
            dir.path(),
            "scout",
            "Here is the report.",
            Duration::from_millis(1500),
        )
        .await;
        let (state, [(alice_id, alice), (_, bob)]) = gateway(&scout);
        let d = crate::desk_crew::tests::desk(pool, dir.path()).await;
        assert!(
            crate::desk_driver::install_desk_for_test(d.clone()),
            "only this test installs the hub's desk"
        );
        let _crew = crate::desk_crew::CrewHub::start(
            Arc::clone(&state),
            d.clone(),
            crate::desk_crew::Timing::default(),
            tokio_util::sync::CancellationToken::new(),
        );
        store();

        // Alice's Desk opens her DM, showing every file.
        use futures::SinkExt;
        let addr = serve_on_a_port(&state).await;
        let (mut socket, _) = tokio_tungstenite::connect_async(format!(
            "ws://{addr}/ws?token={alice}&agent_id=scout&channel=desk&accept=*/*"
        ))
        .await
        .unwrap();
        let session = frame(&mut socket).await["session_id"]
            .as_str()
            .unwrap()
            .to_string();
        let say = |content: &str, attachments: serde_json::Value| {
            tokio_tungstenite::tungstenite::Message::Text(
                json!({"type": "chat", "content": content, "attachments": attachments})
                    .to_string()
                    .into(),
            )
        };
        // She asks; while the turn runs, a tool delivers a PDF that names
        // no session of its own.
        socket
            .send(say("Make me the Q3 report.", json!([])))
            .await
            .unwrap();
        // The turn has begun once her question is in the session (the
        // model takes a while to answer).
        crate::desk_crew::tests::eventually("the question is kept", async || {
            let kept = scout.sessions.get(&session).ok()??;
            kept.messages
                .iter()
                .any(|m| m.role == MessageRole::User)
                .then_some(())
        })
        .await;
        let sink = crate::mcp_gate::media_sinks(&scout).deliver;
        let pdf_bytes = b"%PDF-1.7\n1 0 obj\n<<>>\nendobj\n";
        let pdf = staged("scout", None, pdf_bytes, "application/pdf", "q3.pdf");
        sink.deliver(
            &context(&session, "scout"),
            &pdf,
            "application/pdf",
            Some("q3.pdf"),
            None,
        )
        .await
        .unwrap();
        let live = until(&mut socket, "image").await;
        assert_eq!(live["attachments"][0]["artifact_id"], pdf.as_str());
        assert_eq!(live["attachments"][0]["size"], pdf_bytes.len());
        until(&mut socket, "stream_end").await;
        let conversations = crate::desk_conversations::HubConversations(Arc::clone(&state));
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {alice}").parse().unwrap());
        let read = || {
            conversations.page(
                &headers,
                None,
                "scout",
                DESK_CHANNEL,
                workforce_desk::Cursor::Newest,
                50,
            )
        };
        crate::desk_crew::tests::eventually("the PDF is on the answer", async || {
            let page = read().await.ok()?;
            page.messages
                .iter()
                .any(|m| m.text == "Here is the report." && !m.attachments.is_empty())
                .then_some(())
        })
        .await;

        // After the answer: a file of its own.
        let late = staged("scout", Some(&session), &png(2, 2), "image/png", "late.png");
        sink.deliver(
            &context(&session, "scout"),
            &late,
            "image/png",
            Some("late.png"),
            Some("one more"),
        )
        .await
        .unwrap();

        let page = read().await.unwrap();
        let texts: Vec<(&str, usize)> = page
            .messages
            .iter()
            .map(|m| (m.text.as_str(), m.attachments.len()))
            .collect();
        assert_eq!(
            texts,
            [
                ("Make me the Q3 report.", 0),
                ("Here is the report.", 1),
                ("one more", 1)
            ]
        );
        let on_answer = &page.messages[1].attachments[0];
        assert_eq!(
            (
                on_answer.artifact_id.as_str(),
                on_answer.media_type.as_str(),
                on_answer.filename.as_str(),
                on_answer.size
            ),
            (
                pdf.as_str(),
                "application/pdf",
                "q3.pdf",
                pdf_bytes.len() as u64
            )
        );
        let own = &page.messages[2];
        assert_eq!(own.id, format!("file:{}", late.as_str()));
        assert_eq!(
            (own.attachments[0].width, own.attachments[0].height),
            (Some(2), Some(2))
        );
        assert_eq!(read().await.unwrap(), page, "a reload reads the same");
        // The file outlives the hour a staged file is kept.
        let kept = store().metadata("scout", &pdf).unwrap().unwrap();
        assert!(kept.ttl_seconds >= limits().retention.as_secs());

        // Served to Alice, in part when asked; to nobody else.
        let uri = format!("/api/agents/scout/attachments/{}", pdf.as_str());
        let (status, served, body) = call(&state, get(&uri, Some(&alice), None)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, pdf_bytes);
        assert_eq!(served[header::CONTENT_TYPE], "application/pdf");
        assert_eq!(
            served[header::CONTENT_DISPOSITION],
            "attachment; filename=\"q3.pdf\""
        );
        assert_eq!(served[header::ACCEPT_RANGES], "bytes");
        let (status, served, body) = call(&state, get(&uri, Some(&alice), Some("bytes=0-3"))).await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(body, b"%PDF");
        assert_eq!(
            served[header::CONTENT_RANGE],
            format!("bytes 0-3/{}", pdf_bytes.len())
        );
        let (status, ..) = call(&state, get(&uri, Some(&alice), Some("bytes=999-"))).await;
        assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
        let (status, ..) = call(&state, get(&uri, Some(&bob), None)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "not Bob's conversation");
        let (status, ..) = call(&state, get(&uri, None, None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let bystander = staged("scout", None, b"%PDF-1.7", "application/pdf", "x.pdf");
        let (status, ..) = call(
            &state,
            get(
                &format!("/api/agents/scout/attachments/{}", bystander.as_str()),
                Some(&alice),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "a file of no conversation");

        // An upload: its type from its bytes, its size capped.
        configure(&workforce_common::settings::DeskConfig {
            attachment_max_mb: 3,
            ..Default::default()
        });
        let upload = |name: &str, bytes: Vec<u8>, token: &str| {
            axum::http::Request::post(format!(
                "/api/agents/scout/attachments?channel=desk&filename={name}"
            ))
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "text/plain")
            .body(axum::body::Body::from(bytes))
            .unwrap()
        };
        let (status, _, body) = call(&state, upload("photo.txt", png(30, 20), &alice)).await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "{}",
            String::from_utf8_lossy(&body)
        );
        let uploaded: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(uploaded["media_type"], "image/png");
        assert_eq!(uploaded["filename"], "photo.txt");
        assert_eq!(
            (uploaded["width"].clone(), uploaded["height"].clone()),
            (json!(30), json!(20))
        );
        assert_eq!(uploaded["session_id"], session.as_str());
        let photo = uploaded["artifact_id"].as_str().unwrap().to_string();
        // Past the gateway's 1 MB body limit, up to the cap, raw or as a form.
        let two_mb = [b"%PDF-1.7\n".as_slice(), &vec![b'x'; 2 * 1024 * 1024]].concat();
        let (status, _, body) = call(&state, upload("big.pdf", two_mb.clone(), &alice)).await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "{}",
            String::from_utf8_lossy(&body)
        );
        let boundary = "desk-boundary";
        let form = [
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
                 filename=\"form.pdf\"\r\nContent-Type: image/png\r\n\r\n"
            )
            .into_bytes(),
            two_mb,
            format!("\r\n--{boundary}--\r\n").into_bytes(),
        ]
        .concat();
        let (status, _, body) = call(
            &state,
            axum::http::Request::post("/api/agents/scout/attachments?channel=desk")
                .header("authorization", format!("Bearer {alice}"))
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(axum::body::Body::from(form))
                .unwrap(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "{}",
            String::from_utf8_lossy(&body)
        );
        let formed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            (formed["filename"].clone(), formed["media_type"].clone()),
            (json!("form.pdf"), json!("application/pdf")),
            "what the form said it was is not asked"
        );
        let (status, ..) = call(
            &state,
            upload("big.bin", vec![7; 3 * 1024 * 1024 + 1], &alice),
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        let (status, ..) = call(&state, upload("photo.png", png(1, 1), &bob)).await;
        assert_eq!(status, StatusCode::CREATED, "Bob may upload to his own DM");
        configure(&workforce_common::settings::DeskConfig::default());

        // It goes on her next message, and the agent can look at it.
        socket
            .send(say(
                "And this photo?",
                json!([{"filename": "photo.txt", "mime_type": "image/png", "artifact_id": photo}]),
            ))
            .await
            .unwrap();
        until(&mut socket, "stream_end").await;
        crate::desk_crew::tests::eventually("the photo is on her message", async || {
            let page = read().await.ok()?;
            page.messages
                .iter()
                .any(|m| m.text == "And this photo?" && !m.attachments.is_empty())
                .then_some(())
        })
        .await;
        let page = read().await.unwrap();
        let asked = page
            .messages
            .iter()
            .find(|m| m.text == "And this photo?")
            .unwrap();
        assert_eq!(asked.attachments.len(), 1);
        assert_eq!(asked.attachments[0].artifact_id, photo);
        let files = SessionFiles::new("scout", Arc::clone(&scout.sessions));
        let current = scout
            .sessions
            .find(DESK_CHANNEL, &alice_id)
            .unwrap()
            .unwrap();
        let listed = files.files(&current.id).await.unwrap();
        let file = listed
            .iter()
            .find(|f| f.id == photo)
            .expect("the upload is listed");
        assert_eq!(files.read(&current.id, file).await.unwrap(), png(30, 20));
        assert!(
            listed.iter().any(|f| f.id == pdf.as_str()),
            "and the agent's PDF"
        );

        // Deleted, the conversation takes its files with it: none shows in
        // the one that follows on its channel.
        let (status, ..) = call(
            &state,
            axum::http::Request::delete(format!("/api/sessions/{}", current.id))
                .header("authorization", format!("Bearer {alice}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        for gone in [pdf.as_str(), late.as_str(), photo.as_str()] {
            assert!(
                d.store().attachment(gone).await.unwrap().is_none(),
                "{gone}"
            );
        }
    })
    .await;
}

#[path = "desk_charts_tests.rs"]
mod charts;
