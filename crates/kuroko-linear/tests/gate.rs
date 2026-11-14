#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use hmac::{Hmac, KeyInit, Mac};
use kuroko_core::{
    DelegateId, IssueId, LabelName, Millis, Source, TaskKey, TaskRequest, TaskSpec, TriggerId,
};
use kuroko_linear::{
    DropReason, Gate, Intake, PromptTemplate, Rejection, TriggerConfig, WebhookSecret, is_fresh,
    parse_hex_32, verify_signature,
};
use serde_json::{Value, json};
use sha2::Sha256;

const SECRET: &str = "whsec_golden_vector";
const NOW: Millis = Millis(1_700_000_000_000);
const ISSUE_ID: &str = "11111111-1111-4111-8111-111111111111";
const AGENT: &str = "agent-1";
const LABEL: &str = "delegate to AI agent";

const GOLDEN_BODY: &str = r#"{"action":"create","data":{"description":"Gate the webhook.","id":"11111111-1111-4111-8111-111111111111","identifier":"MOK-150","title":"Ship the gate","url":"https://linear.app/mok/issue/MOK-150"},"type":"Issue","webhookTimestamp":1700000000000}"#;
const GOLDEN_MAC: &str = "514396d43f5b4693ccb606f3f792e4e63f5ad22813c1e8154fd61d6153d3527f";

fn sign_with(secret: &str, raw: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(raw.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn sign(raw: &str) -> String {
    sign_with(SECRET, raw)
}

fn gate() -> Gate {
    Gate::new(
        WebhookSecret::new(SECRET),
        Millis(60_000),
        TriggerConfig {
            any_delegate: false,
            delegate_ids: vec![DelegateId::new(AGENT)],
            label_names: vec![LabelName::new(LABEL)],
        },
        PromptTemplate::new("{identifier} | {title} | {description} | {url}"),
    )
}

fn any_delegate_gate() -> Gate {
    Gate::new(
        WebhookSecret::new(SECRET),
        Millis(60_000),
        TriggerConfig {
            any_delegate: true,
            delegate_ids: Vec::new(),
            label_names: Vec::new(),
        },
        PromptTemplate::new("{identifier}"),
    )
}

fn payload(kind: &str, action: &str, timestamp: i64, data: &Value) -> String {
    json!({
        "action": action,
        "type": kind,
        "webhookTimestamp": timestamp,
        "data": data,
    })
    .to_string()
}

fn issue(action: &str, data: &Value) -> String {
    payload("Issue", action, NOW.0, data)
}

fn issue_data() -> Value {
    json!({
        "id": ISSUE_ID,
        "identifier": "MOK-150",
        "title": "Ship the gate",
        "description": "Gate the webhook.",
        "url": "https://linear.app/mok/issue/MOK-150",
    })
}

fn spec() -> TaskSpec {
    TaskSpec {
        identifier: "MOK-150".to_owned(),
        title: "Ship the gate".to_owned(),
        description: Some("Gate the webhook.".to_owned()),
        url: Some("https://linear.app/mok/issue/MOK-150".to_owned()),
    }
}

fn key(trigger: TriggerId) -> TaskKey {
    TaskKey {
        source: Source::Linear,
        issue: IssueId::new(ISSUE_ID),
        trigger,
    }
}

fn delegate_key(id: &str) -> TaskKey {
    key(TriggerId::Delegate(DelegateId::new(id)))
}

fn accept(gate: &Gate, raw: &str) -> Result<Intake, Rejection> {
    gate.accept(Some(&sign(raw)), raw.as_bytes(), NOW)
}

fn admitted(intake: Intake) -> TaskRequest {
    match intake {
        Intake::Admit(request) => request,
        Intake::Withdraw(key) => panic!("expected an admission, got a withdrawal for {key:?}"),
        Intake::Drop(reason) => panic!("expected an admission, got a drop: {reason:?}"),
    }
}

fn withdrawn(intake: Intake) -> TaskKey {
    match intake {
        Intake::Withdraw(key) => key,
        Intake::Admit(_) => panic!("expected a withdrawal, got an admission"),
        Intake::Drop(reason) => panic!("expected a withdrawal, got a drop: {reason:?}"),
    }
}

fn dropped(intake: Intake) -> DropReason {
    match intake {
        Intake::Drop(reason) => reason,
        Intake::Withdraw(key) => panic!("expected a drop, got a withdrawal for {key:?}"),
        Intake::Admit(_) => panic!("expected a drop, got an admission"),
    }
}

#[test]
fn the_golden_signature_verifies() {
    let secret = WebhookSecret::new(SECRET);
    assert_eq!(
        verify_signature(&secret, GOLDEN_BODY.as_bytes(), Some(GOLDEN_MAC)),
        Ok(())
    );
    assert_eq!(
        parse_hex_32(GOLDEN_MAC).map(hex::encode).as_deref(),
        Some(GOLDEN_MAC)
    );
    assert_eq!(
        gate().accept(Some(GOLDEN_MAC), GOLDEN_BODY.as_bytes(), NOW),
        Ok(Intake::Drop(DropReason::Undelegated))
    );
}

#[test]
fn a_flipped_signature_rejects() {
    let secret = WebhookSecret::new(SECRET);
    let mut bytes = hex::decode(GOLDEN_MAC).unwrap();
    bytes[0] ^= 1;
    let flipped = hex::encode(bytes);
    assert_eq!(
        verify_signature(&secret, GOLDEN_BODY.as_bytes(), Some(&flipped)),
        Err(Rejection::BadSignature)
    );
}

#[test]
fn a_truncated_signature_rejects() {
    let secret = WebhookSecret::new(SECRET);
    assert_eq!(
        verify_signature(&secret, GOLDEN_BODY.as_bytes(), Some(&GOLDEN_MAC[..62])),
        Err(Rejection::BadSignature)
    );
}

#[test]
fn a_padded_signature_rejects() {
    let secret = WebhookSecret::new(SECRET);
    let padded = format!("{GOLDEN_MAC}00");
    assert_eq!(
        verify_signature(&secret, GOLDEN_BODY.as_bytes(), Some(&padded)),
        Err(Rejection::BadSignature)
    );
}

#[test]
fn an_odd_length_signature_rejects() {
    let secret = WebhookSecret::new(SECRET);
    assert_eq!(
        verify_signature(&secret, GOLDEN_BODY.as_bytes(), Some(&GOLDEN_MAC[..63])),
        Err(Rejection::BadSignature)
    );
}

#[test]
fn a_non_hex_signature_rejects() {
    let secret = WebhookSecret::new(SECRET);
    let garbled = format!("z{}", &GOLDEN_MAC[1..]);
    assert_eq!(
        verify_signature(&secret, GOLDEN_BODY.as_bytes(), Some(&garbled)),
        Err(Rejection::BadSignature)
    );
}

#[test]
fn uppercase_hex_decodes_to_the_same_mac() {
    let secret = WebhookSecret::new(SECRET);
    let upper = GOLDEN_MAC.to_uppercase();
    assert_eq!(
        verify_signature(&secret, GOLDEN_BODY.as_bytes(), Some(&upper)),
        Ok(())
    );
    assert_eq!(parse_hex_32(&upper), parse_hex_32(GOLDEN_MAC));
}

#[test]
fn a_missing_signature_rejects() {
    let secret = WebhookSecret::new(SECRET);
    assert_eq!(
        verify_signature(&secret, GOLDEN_BODY.as_bytes(), None),
        Err(Rejection::MissingSignature)
    );
    assert_eq!(
        gate().accept(None, GOLDEN_BODY.as_bytes(), NOW),
        Err(Rejection::MissingSignature)
    );
}

#[test]
fn a_bad_signature_wins_over_unparsable_json() {
    let wrong = sign_with("whsec_other", "not json");
    assert_eq!(
        gate().accept(Some(&wrong), b"not json", NOW),
        Err(Rejection::BadSignature)
    );
    assert_eq!(
        gate().accept(Some(&sign("not json")), b"not json", NOW),
        Err(Rejection::Malformed)
    );
}

#[test]
fn freshness_holds_through_the_window_and_not_past_it() {
    let now = Millis(1_000_000);
    let window = Millis(60_000);
    assert!(is_fresh(now, now, window));
    assert!(is_fresh(Millis(now.0 - 30_000), now, window));
    assert!(is_fresh(Millis(now.0 - 60_000), now, window));
    assert!(!is_fresh(Millis(now.0 - 60_001), now, window));
    assert!(is_fresh(Millis(now.0 + 60_000), now, window));
    assert!(!is_fresh(Millis(now.0 + 60_001), now, window));
}

#[test]
fn a_delivery_outside_the_window_rejects() {
    let mut data = issue_data();
    data["delegateId"] = json!(AGENT);
    let stale = payload("Issue", "create", NOW.0 - 60_001, &data);
    assert_eq!(accept(&gate(), &stale), Err(Rejection::Stale));
    let future = payload("Issue", "create", NOW.0 + 60_001, &data);
    assert_eq!(accept(&gate(), &future), Err(Rejection::Stale));
    let edge = payload("Issue", "create", NOW.0 - 60_000, &issue_data());
    assert_eq!(
        dropped(accept(&gate(), &edge).unwrap()),
        DropReason::Undelegated
    );
}

#[test]
fn a_delegate_arrives_as_a_bare_id() {
    let mut data = issue_data();
    data["delegate"] = json!(AGENT);
    let request = admitted(accept(&gate(), &issue("create", &data)).unwrap());
    assert_eq!(request.key, delegate_key(AGENT));
    assert_eq!(request.spec, spec());
    assert_eq!(
        request.prompt,
        "MOK-150 | Ship the gate | Gate the webhook. | https://linear.app/mok/issue/MOK-150"
    );
}

#[test]
fn a_delegate_arrives_as_an_object() {
    let mut data = issue_data();
    data["delegate"] = json!({ "id": AGENT });
    let request = admitted(accept(&gate(), &issue("create", &data)).unwrap());
    assert_eq!(request.key, delegate_key(AGENT));
}

#[test]
fn a_delegate_arrives_as_delegate_id() {
    let mut data = issue_data();
    data["delegateId"] = json!(AGENT);
    let request = admitted(accept(&gate(), &issue("update", &data)).unwrap());
    assert_eq!(request.key, delegate_key(AGENT));
}

#[test]
fn an_empty_delegate_does_not_admit() {
    let mut data = issue_data();
    data["delegate"] = json!({ "id": "" });

    assert_eq!(
        dropped(accept(&any_delegate_gate(), &issue("create", &data)).unwrap()),
        DropReason::Undelegated
    );
}

#[test]
fn any_delegate_admits_an_unlisted_id() {
    let mut data = issue_data();
    data["delegate"] = json!({ "id": "agent-9" });
    let request = admitted(accept(&any_delegate_gate(), &issue("create", &data)).unwrap());
    assert_eq!(request.key, delegate_key("agent-9"));
}

#[test]
fn a_label_admits_without_a_delegate() {
    let mut data = issue_data();
    data["labels"] = json!([{ "id": "label-1", "name": LABEL, "color": "#fff" }]);
    let request = admitted(accept(&gate(), &issue("create", &data)).unwrap());
    assert_eq!(request.key, key(TriggerId::Label(LabelName::new(LABEL))));
}

#[test]
fn a_label_admits_despite_an_unauthorized_delegate() {
    let mut data = issue_data();
    data["delegateId"] = json!("agent-9");
    data["labels"] = json!([{ "name": LABEL }]);
    let request = admitted(accept(&gate(), &issue("create", &data)).unwrap());
    assert_eq!(request.key, key(TriggerId::Label(LabelName::new(LABEL))));
}

#[test]
fn an_authorized_delegate_wins_over_the_label() {
    let mut data = issue_data();
    data["delegateId"] = json!(AGENT);
    data["labels"] = json!([{ "name": LABEL }]);
    let request = admitted(accept(&gate(), &issue("create", &data)).unwrap());
    assert_eq!(request.key, delegate_key(AGENT));
}

#[test]
fn an_unauthorized_delegate_drops() {
    let mut data = issue_data();
    data["delegateId"] = json!("agent-9");
    assert_eq!(
        dropped(accept(&gate(), &issue("create", &data)).unwrap()),
        DropReason::NotTriggered
    );
}

#[test]
fn an_issue_without_a_trigger_drops() {
    assert_eq!(
        dropped(accept(&gate(), &issue("create", &issue_data())).unwrap()),
        DropReason::Undelegated
    );
}

#[test]
fn a_remove_action_withdraws_the_delegate() {
    let data = json!({ "id": ISSUE_ID, "delegateId": AGENT });
    assert_eq!(
        withdrawn(accept(&gate(), &issue("remove", &data)).unwrap()),
        delegate_key(AGENT)
    );
}

#[test]
fn an_untouched_remove_withdraws_the_object_delegate() {
    let data = json!({ "id": ISSUE_ID, "delegate": { "id": AGENT } });
    assert_eq!(
        withdrawn(accept(&gate(), &issue("remove", &data)).unwrap()),
        delegate_key(AGENT)
    );
}

#[test]
fn a_remove_action_without_a_delegate_drops() {
    let data = json!({ "id": ISSUE_ID, "identifier": "MOK-150" });
    assert_eq!(
        dropped(accept(&gate(), &issue("remove", &data)).unwrap()),
        DropReason::Undelegated
    );
}

#[test]
fn an_update_that_removes_the_delegate_withdraws() {
    let raw = json!({
        "action": "update",
        "type": "Issue",
        "webhookTimestamp": NOW.0,
        "data": { "id": ISSUE_ID, "identifier": "MOK-150" },
        "updatedFrom": { "delegateId": AGENT },
    })
    .to_string();
    assert_eq!(
        withdrawn(accept(&gate(), &raw).unwrap()),
        delegate_key(AGENT)
    );
}

#[test]
fn an_update_that_keeps_the_delegate_admits() {
    let raw = json!({
        "action": "update",
        "type": "Issue",
        "webhookTimestamp": NOW.0,
        "data": { "id": ISSUE_ID, "delegateId": AGENT },
        "updatedFrom": { "delegateId": AGENT },
    })
    .to_string();
    assert_eq!(
        admitted(accept(&gate(), &raw).unwrap()).key,
        delegate_key(AGENT)
    );
}

#[test]
fn a_non_issue_drops() {
    let data = json!({ "id": "comment-1", "body": "hello" });
    assert_eq!(
        dropped(accept(&gate(), &payload("Comment", "remove", NOW.0, &data)).unwrap()),
        DropReason::NotAnIssue
    );
}

#[test]
fn an_issue_without_an_id_is_unroutable() {
    let data = json!({ "id": "", "delegateId": AGENT });
    assert_eq!(
        dropped(accept(&gate(), &issue("create", &data)).unwrap()),
        DropReason::Unroutable
    );
}

#[test]
fn unknown_and_missing_fields_parse() {
    let data = json!({
        "id": ISSUE_ID,
        "identifier": "MOK-150",
        "title": "Ship the gate",
        "delegateId": AGENT,
        "priority": 2,
        "state": { "type": "started", "name": "In Progress" },
        "labels": [{ "id": "label-1", "name": "bug", "color": "#f00" }],
    });
    let raw = json!({
        "action": "create",
        "type": "Issue",
        "actor": { "id": "user-1", "name": "mok" },
        "createdAt": "2026-04-01T00:00:00.000Z",
        "url": "https://linear.app/mok/issue/MOK-150",
        "data": data,
        "webhookId": "hook-1",
        "webhookTimestamp": NOW.0,
        "unknownTopLevel": true,
    })
    .to_string();
    let request = admitted(accept(&gate(), &raw).unwrap());
    assert_eq!(request.key, delegate_key(AGENT));
    assert_eq!(request.spec.identifier, "MOK-150");
    assert_eq!(request.spec.description, None);
    assert_eq!(request.spec.url, None);
    assert_eq!(request.prompt, "MOK-150 | Ship the gate |  | ");
}

#[test]
fn the_template_fills_every_placeholder() {
    let template = PromptTemplate::new("{identifier}: {title} [{url}] {description}");
    assert_eq!(
        template.render(&spec()),
        "MOK-150: Ship the gate [https://linear.app/mok/issue/MOK-150] Gate the webhook."
    );
}

#[test]
fn the_template_leaves_unknown_braces_literal() {
    let template = PromptTemplate::new("{identifier} {unknown} {Identifier}");
    assert_eq!(template.render(&spec()), "MOK-150 {unknown} {Identifier}");
}

#[test]
fn the_template_renders_missing_fields_empty() {
    let template = PromptTemplate::new("<{description}><{url}>");
    let bare = TaskSpec {
        identifier: "MOK-1".to_owned(),
        title: String::new(),
        description: None,
        url: None,
    };
    assert_eq!(template.render(&bare), "<><>");
}

#[test]
fn the_template_keeps_plain_text_and_dangling_braces() {
    assert_eq!(
        PromptTemplate::new("plain text").render(&spec()),
        "plain text"
    );
    assert_eq!(
        PromptTemplate::new("open {title").render(&spec()),
        "open {title"
    );
    assert_eq!(PromptTemplate::new("").render(&spec()), "");
}

#[test]
fn the_intake_debug_names_each_variant() {
    let mut data = issue_data();
    data["delegateId"] = json!(AGENT);
    let admission = accept(&gate(), &issue("create", &data)).unwrap();
    let text = format!("{admission:?}");
    assert!(text.starts_with("Admit { key: TaskKey { source: Linear"));
    assert!(text.contains("MOK-150 | Ship the gate"));

    assert_eq!(
        format!("{:?}", Intake::Withdraw(delegate_key(AGENT))),
        format!("Withdraw({:?})", delegate_key(AGENT))
    );
    assert_eq!(
        format!("{:?}", Intake::Drop(DropReason::Unroutable)),
        "Drop(Unroutable)"
    );
}

#[test]
fn rejection_statuses_are_401_or_400() {
    assert_eq!(Rejection::MissingSignature.status(), 401);
    assert_eq!(Rejection::BadSignature.status(), 401);
    assert_eq!(Rejection::Stale.status(), 400);
    assert_eq!(Rejection::Malformed.status(), 400);
}
