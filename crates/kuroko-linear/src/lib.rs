//! The gate: HMAC, parse, freshness, distillation, prompt rendering.

use hmac::{Hmac, KeyInit, Mac};
use kuroko_core::{
    DelegateId, IssueId, LabelName, Millis, Source, TaskKey, TaskRequest, TaskSpec, TriggerId,
};
use serde::Deserialize;
use sha2::Sha256;

/// The signing secret Linear shows once, when the webhook is created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebhookSecret(String);

impl WebhookSecret {
    #[must_use]
    pub fn new(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }
}

/// Which deliveries become work.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TriggerConfig {
    pub any_delegate: bool,
    pub delegate_ids: Vec<DelegateId>,
    pub label_names: Vec<LabelName>,
}

/// The text handed to a coding agent, with placeholders substituted at admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptTemplate(String);

impl PromptTemplate {
    #[must_use]
    pub fn new(template: impl Into<String>) -> Self {
        Self(template.into())
    }

    /// Substitutes `{identifier}`, `{title}`, `{description}`, and `{url}`.
    ///
    /// Unknown braces stay literal and a missing field renders empty.
    #[must_use]
    pub fn render(&self, spec: &TaskSpec) -> String {
        let mut out = String::with_capacity(self.0.len());
        let mut rest = self.0.as_str();
        while let Some(open) = rest.find('{') {
            out.push_str(&rest[..open]);
            let after = &rest[open + 1..];
            let Some(close) = after.find('}') else {
                out.push_str(&rest[open..]);
                return out;
            };
            match &after[..close] {
                "identifier" => out.push_str(&spec.identifier),
                "title" => out.push_str(&spec.title),
                "description" => out.push_str(spec.description.as_deref().unwrap_or_default()),
                "url" => out.push_str(spec.url.as_deref().unwrap_or_default()),
                literal => {
                    out.push('{');
                    out.push_str(literal);
                    out.push('}');
                }
            }
            rest = &after[close + 1..];
        }
        out.push_str(rest);
        out
    }
}

/// What one verified delivery asks of the engine.
#[derive(Clone, PartialEq, Eq)]
pub enum Intake {
    Admit(TaskRequest),
    Withdraw(TaskKey),
    Drop(DropReason),
}

// `TaskRequest` carries no `Debug`, so the derive cannot reach it.
impl std::fmt::Debug for Intake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Admit(request) => f
                .debug_struct("Admit")
                .field("key", &request.key)
                .field("spec", &request.spec)
                .field("prompt", &request.prompt)
                .finish(),
            Self::Withdraw(key) => f.debug_tuple("Withdraw").field(key).finish(),
            Self::Drop(reason) => f.debug_tuple("Drop").field(reason).finish(),
        }
    }
}

/// Why a verified delivery carries no work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropReason {
    NotAnIssue,
    NotTriggered,
    Undelegated,
    Unroutable,
}

/// Why a delivery never reached distillation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejection {
    MissingSignature,
    BadSignature,
    Stale,
    Malformed,
}

impl Rejection {
    /// The HTTP status the app answers with.
    #[must_use]
    pub const fn status(&self) -> u16 {
        match self {
            Self::MissingSignature | Self::BadSignature => 401,
            Self::Stale | Self::Malformed => 400,
        }
    }
}

/// Verifies, parses, and distills Linear deliveries.
#[derive(Clone, Debug)]
pub struct Gate {
    secret: WebhookSecret,
    window: Millis,
    triggers: TriggerConfig,
    template: PromptTemplate,
}

impl Gate {
    #[must_use]
    pub const fn new(
        secret: WebhookSecret,
        window: Millis,
        triggers: TriggerConfig,
        template: PromptTemplate,
    ) -> Self {
        Self {
            secret,
            window,
            triggers,
            template,
        }
    }

    /// Runs the gate. The order is fixed: HMAC over the raw bytes, parse,
    /// freshness, distill.
    ///
    /// # Errors
    ///
    /// Returns [`Rejection`] for a missing or wrong signature, a body that does
    /// not parse, or a timestamp outside the freshness window.
    pub fn accept(
        &self,
        signature: Option<&str>,
        raw: &[u8],
        now: Millis,
    ) -> Result<Intake, Rejection> {
        verify_signature(&self.secret, raw, signature)?;
        let delivery: Delivery = serde_json::from_slice(raw).map_err(|_| Rejection::Malformed)?;
        if !is_fresh(Millis(delivery.webhook_timestamp), now, self.window) {
            return Err(Rejection::Stale);
        }
        Ok(distill(&delivery, &self.triggers, &self.template))
    }
}

/// Checks the `Linear-Signature` header against a secret.
///
/// # Errors
///
/// Returns [`Rejection::MissingSignature`] when the header is absent and
/// [`Rejection::BadSignature`] when it is not 32 hex-encoded bytes or does not
/// match the body in constant time.
pub fn verify_signature(
    secret: &WebhookSecret,
    raw: &[u8],
    provided: Option<&str>,
) -> Result<(), Rejection> {
    let text = provided.ok_or(Rejection::MissingSignature)?;
    let provided = parse_hex_32(text).ok_or(Rejection::BadSignature)?;
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.0.as_bytes()).map_err(|_| Rejection::BadSignature)?;
    mac.update(raw);
    mac.verify_slice(&provided)
        .map_err(|_| Rejection::BadSignature)
}

/// Decodes exactly 32 bytes of case-insensitive hex.
#[must_use]
pub fn parse_hex_32(text: &str) -> Option<[u8; 32]> {
    let bytes = hex::decode(text).ok()?;
    bytes.try_into().ok()
}

/// Reports whether a delivery timestamp sits inside the replay window.
#[must_use]
pub const fn is_fresh(sent: Millis, now: Millis, window: Millis) -> bool {
    now.0.abs_diff(sent.0) <= window.0.unsigned_abs()
}

fn distill(delivery: &Delivery, triggers: &TriggerConfig, template: &PromptTemplate) -> Intake {
    if delivery.kind != "Issue" {
        return Intake::Drop(DropReason::NotAnIssue);
    }
    let data = &delivery.data;
    if data.id.is_empty() {
        return Intake::Drop(DropReason::Unroutable);
    }
    let issue = IssueId::new(data.id.clone());
    let delegate = data.delegate();
    if delivery.action == "remove" {
        return delegate.map_or(Intake::Drop(DropReason::Undelegated), |id| {
            Intake::Withdraw(key(&issue, TriggerId::Delegate(DelegateId::new(id))))
        });
    }
    if delivery.action == "update"
        && delegate.is_none()
        && let Some(previous) = delivery.updated_from.as_ref().and_then(IssueData::delegate)
    {
        return Intake::Withdraw(key(&issue, TriggerId::Delegate(DelegateId::new(previous))));
    }
    if let Some(id) = delegate.filter(|id| authorized(id, triggers)) {
        return admit(
            &issue,
            TriggerId::Delegate(DelegateId::new(id)),
            data,
            template,
        );
    }
    if let Some(name) = matched_label(data, triggers) {
        return admit(
            &issue,
            TriggerId::Label(LabelName::new(name)),
            data,
            template,
        );
    }
    if delegate.is_some() {
        return Intake::Drop(DropReason::NotTriggered);
    }
    Intake::Drop(DropReason::Undelegated)
}

fn authorized(id: &str, triggers: &TriggerConfig) -> bool {
    triggers.any_delegate || triggers.delegate_ids.contains(&DelegateId::new(id))
}

fn matched_label<'a>(data: &'a IssueData, triggers: &TriggerConfig) -> Option<&'a str> {
    data.labels
        .iter()
        .map(|label| label.name.as_str())
        .find(|name| triggers.label_names.contains(&LabelName::new(*name)))
}

fn key(issue: &IssueId, trigger: TriggerId) -> TaskKey {
    TaskKey {
        source: Source::Linear,
        issue: issue.clone(),
        trigger,
    }
}

fn admit(
    issue: &IssueId,
    trigger: TriggerId,
    data: &IssueData,
    template: &PromptTemplate,
) -> Intake {
    let spec = TaskSpec {
        identifier: data.identifier.clone(),
        title: data.title.clone(),
        description: data.description.clone(),
        url: data.url.clone(),
    };
    let prompt = template.render(&spec);
    Intake::Admit(TaskRequest {
        key: key(issue, trigger),
        spec,
        prompt,
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Delivery {
    action: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    data: IssueData,
    updated_from: Option<IssueData>,
    webhook_timestamp: i64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IssueData {
    #[serde(default)]
    id: String,
    #[serde(default)]
    identifier: String,
    #[serde(default)]
    title: String,
    description: Option<String>,
    url: Option<String>,
    #[serde(default)]
    labels: Vec<Label>,
    delegate: Option<Delegate>,
    delegate_id: Option<String>,
}

impl IssueData {
    fn delegate(&self) -> Option<&str> {
        self.delegate
            .as_ref()
            .and_then(Delegate::id)
            .or(self.delegate_id.as_deref())
    }
}

/// Linear ships the delegation field in both shapes, and tolerating both costs
/// nothing while a missed spelling would silently drop real work.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Delegate {
    Id(String),
    Ref { id: Option<String> },
}

impl Delegate {
    fn id(&self) -> Option<&str> {
        match self {
            Self::Id(id) => Some(id),
            Self::Ref { id } => id.as_deref(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct Label {
    name: String,
}
