use std::collections::{BTreeMap, HashSet};

use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;
use unicode_casefold::UnicodeCaseFold;
use unicode_general_category::{GeneralCategory, get_general_category};
use unicode_normalization::{UnicodeNormalization, char::canonical_combining_class};

pub(crate) const SLUG_MAX_BYTES: usize = 64;
const CANONICAL_KEYS: [&str; 8] = [
    "slug", "title", "type", "status", "owner", "updated", "verified", "summary",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SlugProblem {
    Empty,
    TooLong,
    InvalidCharacter,
}

pub(crate) fn validate_slug(value: &str) -> Result<(), SlugProblem> {
    if value.is_empty() {
        return Err(SlugProblem::Empty);
    }
    if value.len() > SLUG_MAX_BYTES {
        return Err(SlugProblem::TooLong);
    }
    if value
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        Ok(())
    } else {
        Err(SlugProblem::InvalidCharacter)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{0}")]
pub struct SchemaError(String);

impl SchemaError {
    fn refusal(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PageType {
    Gotcha,
    Pattern,
    ProjectState,
    Feature,
    Workflow,
    Decision,
}

impl PageType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gotcha => "gotcha",
            Self::Pattern => "pattern",
            Self::ProjectState => "project-state",
            Self::Feature => "feature",
            Self::Workflow => "workflow",
            Self::Decision => "decision",
        }
    }

    pub const fn heading(self) -> &'static str {
        match self {
            Self::Gotcha => "Gotchas",
            Self::Pattern => "Patterns",
            Self::Decision => "Decisions",
            Self::Workflow => "Workflow",
            Self::ProjectState => "Project state",
            Self::Feature => "Features — architecture pointers",
        }
    }

    fn parse(value: &str) -> Result<Self, SchemaError> {
        match value {
            "gotcha" => Ok(Self::Gotcha),
            "pattern" => Ok(Self::Pattern),
            "project-state" => Ok(Self::ProjectState),
            "feature" => Ok(Self::Feature),
            "workflow" => Ok(Self::Workflow),
            "decision" => Ok(Self::Decision),
            _ => Err(SchemaError::refusal(format!(
                "page has type: {value} — expected one of decision | feature | gotcha | pattern | project-state | workflow"
            ))),
        }
    }
}

impl<'de> Deserialize<'de> for PageType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "gotcha" => Ok(Self::Gotcha),
            "pattern" => Ok(Self::Pattern),
            "project-state" => Ok(Self::ProjectState),
            "feature" => Ok(Self::Feature),
            "workflow" => Ok(Self::Workflow),
            "decision" => Ok(Self::Decision),
            _ => Err(D::Error::custom(format!(
                "type: {value} — expected one of decision | feature | gotcha | pattern | project-state | workflow"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Owner {
    Claude,
    Codex,
    Shared,
}

impl Owner {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Shared => "shared",
        }
    }

    fn parse(value: &str) -> Result<Self, SchemaError> {
        match value {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "shared" => Ok(Self::Shared),
            _ => Err(SchemaError::refusal(format!(
                "page has owner: {value} — expected one of claude | codex | shared"
            ))),
        }
    }
}

impl<'de> Deserialize<'de> for Owner {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "shared" => Ok(Self::Shared),
            _ => Err(D::Error::custom(format!(
                "owner: {value} — expected one of claude | codex | shared"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Current,
    Historical,
    InProgress,
}

impl Status {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Historical => "historical",
            Self::InProgress => "in-progress",
        }
    }

    fn parse(value: &str) -> Result<Self, SchemaError> {
        match value {
            "current" => Ok(Self::Current),
            "historical" => Ok(Self::Historical),
            "in-progress" => Ok(Self::InProgress),
            _ => Err(SchemaError::refusal(format!(
                "page has status: {value} — expected one of current | historical | in-progress"
            ))),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CreateRequest {
    pub title: String,
    #[serde(rename = "type")]
    pub page_type: PageType,
    pub owner: Owner,
    pub fact: String,
    pub why: String,
    pub how_to_apply: String,
    pub falsified_by: String,
    pub summary: String,
    #[serde(default)]
    pub related: Vec<String>,
}

impl<'de> Deserialize<'de> for CreateRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Self::from_json(value).map_err(D::Error::custom)
    }
}

impl CreateRequest {
    fn from_json(value: Value) -> Result<Self, SchemaError> {
        const ALLOWED: &[&str] = &[
            "title",
            "type",
            "owner",
            "fact",
            "why",
            "how_to_apply",
            "falsified_by",
            "summary",
            "related",
        ];
        let object = json_object(&value)?;
        reject_unknown_fields(object, ALLOWED)?;

        let title = json_required_string(object, "title")?;
        let page_type = json_required_string(object, "type")?;
        let owner = json_required_string(object, "owner")?;
        let fact = json_required_string(object, "fact")?;
        let why = json_required_string(object, "why")?;
        let how_to_apply = json_required_string(object, "how_to_apply")?;
        let falsified_by = json_required_string(object, "falsified_by")?;
        let summary = json_required_string(object, "summary")?;

        validate_frontmatter_scalar("title", &title)?;
        validate_frontmatter_scalar("summary", &summary)?;
        let page_type = request_page_type(&page_type)?;
        let owner = request_owner(&owner)?;
        slugify(&title)?;
        let related = match object.get("related") {
            None => Vec::new(),
            Some(value) if !json_is_python_truthy(value) => Vec::new(),
            Some(Value::Array(values)) => json_string_list(values)?,
            Some(_) => {
                return Err(SchemaError::refusal("related must be a list of slugs"));
            }
        };

        Ok(Self {
            title,
            page_type,
            owner,
            fact,
            why,
            how_to_apply,
            falsified_by,
            summary,
            related,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UpdateRequest {
    pub title: String,
    #[serde(rename = "type")]
    pub page_type: PageType,
    pub fact: String,
    pub why: String,
    pub how_to_apply: String,
    pub falsified_by: String,
    pub summary: String,
    pub related: Vec<String>,
    pub update: bool,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_sha256: Option<String>,
}

impl<'de> Deserialize<'de> for UpdateRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Self::from_json(value).map_err(D::Error::custom)
    }
}

impl UpdateRequest {
    fn from_json(value: Value) -> Result<Self, SchemaError> {
        const ALLOWED: &[&str] = &[
            "title",
            "type",
            "fact",
            "why",
            "how_to_apply",
            "falsified_by",
            "summary",
            "related",
            "update",
            "target",
            "expected_sha256",
        ];
        let object = json_object(&value)?;
        if object.contains_key("owner") {
            return Err(SchemaError::refusal(
                "owner is refused on update — the stored value is preserved; changing it is a deliberate edit",
            ));
        }
        if object.contains_key("status") {
            return Err(SchemaError::refusal(
                "status is refused on update — the stored value is preserved; changing it is a deliberate edit",
            ));
        }
        reject_unknown_fields(object, ALLOWED)?;

        let title = json_required_string(object, "title")?;
        let page_type = json_required_string(object, "type")?;
        let fact = json_required_string(object, "fact")?;
        let why = json_required_string(object, "why")?;
        let how_to_apply = json_required_string(object, "how_to_apply")?;
        let falsified_by = json_required_string(object, "falsified_by")?;
        let summary = json_required_string(object, "summary")?;
        let related_values = match object.get("related") {
            Some(Value::Array(values)) => values,
            _ => {
                return Err(SchemaError::refusal(
                    "related is required on update; [] clears it",
                ));
            }
        };
        if object.get("update") != Some(&Value::Bool(true)) {
            return Err(SchemaError::refusal("update must be true"));
        }
        let target = json_required_string(object, "target")?;
        let expected_sha256 = match object.get("expected_sha256") {
            None | Some(Value::Null) => None,
            Some(Value::String(value)) if is_sha256(value) => Some(value.clone()),
            Some(_) => {
                return Err(SchemaError::refusal(
                    "expected_sha256 must be 64 lowercase hex characters",
                ));
            }
        };

        validate_frontmatter_scalar("title", &title)?;
        validate_frontmatter_scalar("summary", &summary)?;
        let page_type = request_page_type(&page_type)?;
        slugify(&title)?;
        let related = json_string_list(related_values)?;
        Ok(Self {
            title,
            page_type,
            fact,
            why,
            how_to_apply,
            falsified_by,
            summary,
            related,
            update: true,
            target,
            expected_sha256,
        })
    }
}

fn json_object(value: &Value) -> Result<&Map<String, Value>, SchemaError> {
    value
        .as_object()
        .ok_or_else(|| SchemaError::refusal("payload is not a JSON object"))
}

fn reject_unknown_fields(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), SchemaError> {
    let mut unknown = object
        .keys()
        .filter(|key| !allowed.contains(&key.as_str()))
        .map(String::as_str)
        .collect::<Vec<_>>();
    if unknown.is_empty() {
        return Ok(());
    }
    unknown.sort_unstable();
    Err(SchemaError::refusal(format!(
        "unknown field(s): {}",
        unknown.join(", ")
    )))
}

fn json_required_string(object: &Map<String, Value>, field: &str) -> Result<String, SchemaError> {
    match object.get(field) {
        Some(Value::String(value)) if !python_trim(value).is_empty() => Ok(value.clone()),
        _ => Err(SchemaError::refusal(format!("{field} is required"))),
    }
}

fn json_string_list(values: &[Value]) -> Result<Vec<String>, SchemaError> {
    values
        .iter()
        .map(|value| match value {
            Value::String(value) => Ok(value.clone()),
            _ => Err(SchemaError::refusal("related must be a list of slugs")),
        })
        .collect()
}

fn json_is_python_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => {
            value.as_i64().is_some_and(|value| value != 0)
                || value.as_u64().is_some_and(|value| value != 0)
                || value.as_f64().is_some_and(|value| value != 0.0)
        }
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn request_page_type(value: &str) -> Result<PageType, SchemaError> {
    match value {
        "gotcha" => Ok(PageType::Gotcha),
        "pattern" => Ok(PageType::Pattern),
        "project-state" => Ok(PageType::ProjectState),
        "feature" => Ok(PageType::Feature),
        "workflow" => Ok(PageType::Workflow),
        "decision" => Ok(PageType::Decision),
        _ => Err(SchemaError::refusal(format!(
            "type: {value} — expected one of decision | feature | gotcha | pattern | project-state | workflow"
        ))),
    }
}

fn request_owner(value: &str) -> Result<Owner, SchemaError> {
    match value {
        "claude" => Ok(Owner::Claude),
        "codex" => Ok(Owner::Codex),
        "shared" => Ok(Owner::Shared),
        _ => Err(SchemaError::refusal(format!(
            "owner: {value} — expected one of claude | codex | shared"
        ))),
    }
}

fn python_trim(value: &str) -> &str {
    value.trim_matches(is_python_whitespace)
}

pub(crate) fn is_python_whitespace(ch: char) -> bool {
    matches!(
        ch,
        '\u{0009}'..='\u{000d}'
            | '\u{001c}'..='\u{0020}'
            | '\u{0085}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
    )
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedWikiPage {
    fields: BTreeMap<String, String>,
    pub slug: String,
    pub title: String,
    pub page_type: PageType,
    pub status: Status,
    pub owner: Owner,
    pub updated: String,
    pub verified: String,
    pub summary: String,
}

impl ParsedWikiPage {
    pub fn fields(&self) -> &BTreeMap<String, String> {
        &self.fields
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderedUpdate {
    pub page: String,
    pub content_changed: bool,
}

pub fn slugify(title: &str) -> Result<String, SchemaError> {
    let folded = title
        .nfkd()
        .case_fold()
        .filter(|ch| canonical_combining_class(*ch) == 0);
    let mut slug = String::new();
    let mut pending_separator = false;

    for ch in folded {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            if pending_separator && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(ch);
            pending_separator = false;
        } else {
            pending_separator = true;
        }
    }

    if slug.len() > SLUG_MAX_BYTES {
        slug.truncate(SLUG_MAX_BYTES);
        if let Some(boundary) = slug.rfind('-') {
            slug.truncate(boundary);
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }

    match validate_slug(&slug) {
        Ok(()) => Ok(slug),
        Err(SlugProblem::Empty) => Err(SchemaError::refusal("title produces an empty slug")),
        Err(SlugProblem::TooLong | SlugProblem::InvalidCharacter) => {
            unreachable!("slugify emits only bounded canonical slug bytes")
        }
    }
}

pub fn render_create(request: &CreateRequest, today: &str) -> Result<String, SchemaError> {
    validate_today(today)?;
    let slug = validate_create(request)?;
    let page = render_page(
        CommonRequest::Create(request),
        &slug,
        request.owner,
        Status::Current,
        today,
        today,
    );
    ensure_rendered_page(&page)?;
    Ok(page)
}

pub(crate) fn validate_today_input(today: &str) -> Result<(), SchemaError> {
    validate_today(today)
}

pub(crate) fn validate_update_request(
    request: &UpdateRequest,
    today: &str,
) -> Result<(), SchemaError> {
    validate_today(today)?;
    validate_update(request)?;
    Ok(())
}

pub fn render_update(
    request: &UpdateRequest,
    current: &str,
    today: &str,
) -> Result<RenderedUpdate, SchemaError> {
    validate_today(today)?;
    validate_update(request)?;

    if let Some(expected) = &request.expected_sha256 {
        let actual = format!("{:x}", Sha256::digest(current.as_bytes()));
        if &actual != expected {
            return Err(SchemaError::refusal("the page changed since it was read"));
        }
    }

    let stored = parse_frontmatter_fields(current)?;
    let slug = required_page_field(&stored, "slug")?;
    validate_page_slug(&slug)?;
    if slug != request.target {
        return Err(SchemaError::refusal(format!(
            "target does not match stored page slug: {}",
            slug
        )));
    }
    let owner = Owner::parse(&stored["owner"]).map_err(|_| {
        SchemaError::refusal(format!(
            "pages/{slug}.md has owner: {} — expected one of claude | codex | shared",
            display_missing(&stored["owner"])
        ))
    })?;
    let status = Status::parse(&stored["status"]).map_err(|_| {
        SchemaError::refusal(format!(
            "pages/{slug}.md has status: {} — expected one of current | historical | in-progress",
            display_missing(&stored["status"])
        ))
    })?;
    let updated = &stored["updated"];
    if !is_iso_date(updated) || updated.as_str() > today {
        return Err(SchemaError::refusal(format!(
            "pages/{}.md has updated: {} — expected YYYY-MM-DD no later than today ({today})",
            slug,
            display_missing(updated)
        )));
    }

    let candidate = render_page(
        CommonRequest::Update(request),
        &slug,
        owner,
        status,
        updated,
        today,
    );
    let recanonicalized = recanonicalize_frontmatter(current, &stored);
    let content_changed = without_dates(&candidate) != without_dates(&recanonicalized);
    let page = if content_changed {
        render_page(
            CommonRequest::Update(request),
            &slug,
            owner,
            status,
            today,
            today,
        )
    } else {
        candidate
    };
    ensure_rendered_page(&page)?;

    Ok(RenderedUpdate {
        page,
        content_changed,
    })
}

/// Re-emits `source` with its frontmatter block rewritten in canonical key
/// order from the already-parsed `fields`, keeping the body after the closing
/// `---` byte-for-byte. Makes change detection insensitive to third-party
/// frontmatter normalization (key order, quoted scalars).
fn recanonicalize_frontmatter(source: &str, fields: &BTreeMap<String, String>) -> String {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let lines = python_splitlines(source);
    let Some(end) = lines
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(index, line)| (*line == "---").then_some(index))
    else {
        return source.to_owned();
    };
    let body = lines.get(end + 1).map_or("", |line| {
        &source[line.as_ptr() as usize - source.as_ptr() as usize..]
    });

    let mut recanonicalized = String::from("---\n");
    for key in CANONICAL_KEYS {
        recanonicalized.push_str(key);
        recanonicalized.push_str(": ");
        recanonicalized.push_str(&fields[key]);
        recanonicalized.push('\n');
    }
    recanonicalized.push_str("---\n");
    recanonicalized.push_str(body);
    recanonicalized
}

pub fn parse_wiki_page(source: &str) -> Result<ParsedWikiPage, SchemaError> {
    let fields = parse_frontmatter_fields(source)?;
    let slug = required_page_field(&fields, "slug")?;
    validate_page_slug(&slug)?;
    let title = required_page_field(&fields, "title")?;
    validate_frontmatter_scalar("title", &title)?;
    let summary = required_page_field(&fields, "summary")?;
    validate_frontmatter_scalar("summary", &summary)?;
    let page_type = PageType::parse(&fields["type"])?;
    let status = Status::parse(&fields["status"])?;
    let owner = Owner::parse(&fields["owner"])?;
    let updated = required_page_field(&fields, "updated")?;
    let verified = required_page_field(&fields, "verified")?;
    if !is_iso_date(&updated) {
        return Err(SchemaError::refusal(format!(
            "page has updated: {updated} — expected YYYY-MM-DD"
        )));
    }
    if !is_iso_date(&verified) {
        return Err(SchemaError::refusal(format!(
            "page has verified: {verified} — expected YYYY-MM-DD"
        )));
    }
    if verified < updated {
        return Err(SchemaError::refusal(format!(
            "page has verified: {verified} before updated: {updated} — editing a page verifies it"
        )));
    }
    Ok(ParsedWikiPage {
        fields,
        slug,
        title,
        page_type,
        status,
        owner,
        updated,
        verified,
        summary,
    })
}

fn parse_frontmatter_fields(source: &str) -> Result<BTreeMap<String, String>, SchemaError> {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let lines = python_splitlines(source);
    if lines.first().is_none_or(|line| *line != "---") {
        return Err(SchemaError::refusal(
            "page has no parseable frontmatter block",
        ));
    }
    let Some(end) = lines
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(index, line)| (*line == "---").then_some(index))
    else {
        return Err(SchemaError::refusal(
            "page has no parseable frontmatter block",
        ));
    };

    let mut fields = BTreeMap::new();
    for line in &lines[1..end] {
        if line.trim().is_empty() {
            return Err(SchemaError::refusal(
                "page has no parseable frontmatter block",
            ));
        }
        if line.starts_with([' ', '\t']) {
            if fields.is_empty() {
                return Err(SchemaError::refusal(
                    "page has no parseable frontmatter block",
                ));
            }
            continue;
        }
        let Some((key, value)) = strict_field_line(line) else {
            return Err(SchemaError::refusal(
                "page has no parseable frontmatter block",
            ));
        };
        if !canonical_key(key) {
            return Err(SchemaError::refusal(format!(
                "page has unknown frontmatter key: {key}"
            )));
        }
        let value = strict_unquote(value).to_owned();
        if fields.insert(key.to_owned(), value).is_some() {
            return Err(SchemaError::refusal(format!(
                "page has duplicate frontmatter key: {key}"
            )));
        }
    }

    for key in CANONICAL_KEYS {
        if !fields.contains_key(key) {
            return Err(SchemaError::refusal(format!("page is missing {key}")));
        }
    }

    Ok(fields)
}

fn strict_field_line(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once(':')?;
    let first = key.chars().next()?;
    let legal = first.is_ascii_alphabetic() || first == '_';
    let rest_legal = key
        .chars()
        .skip(1)
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'));
    (legal && rest_legal).then_some((key, value.trim()))
}

fn strict_unquote(value: &str) -> &str {
    match value.as_bytes() {
        [b'\'', .., b'\''] | [b'"', .., b'"'] if value.len() >= 2 => &value[1..value.len() - 1],
        _ => value,
    }
}

enum CommonRequest<'a> {
    Create(&'a CreateRequest),
    Update(&'a UpdateRequest),
}

impl CommonRequest<'_> {
    fn title(&self) -> &str {
        match self {
            Self::Create(request) => &request.title,
            Self::Update(request) => &request.title,
        }
    }

    fn page_type(&self) -> PageType {
        match self {
            Self::Create(request) => request.page_type,
            Self::Update(request) => request.page_type,
        }
    }

    fn fact(&self) -> &str {
        match self {
            Self::Create(request) => &request.fact,
            Self::Update(request) => &request.fact,
        }
    }

    fn why(&self) -> &str {
        match self {
            Self::Create(request) => &request.why,
            Self::Update(request) => &request.why,
        }
    }

    fn how_to_apply(&self) -> &str {
        match self {
            Self::Create(request) => &request.how_to_apply,
            Self::Update(request) => &request.how_to_apply,
        }
    }

    fn falsified_by(&self) -> &str {
        match self {
            Self::Create(request) => &request.falsified_by,
            Self::Update(request) => &request.falsified_by,
        }
    }

    fn summary(&self) -> &str {
        match self {
            Self::Create(request) => &request.summary,
            Self::Update(request) => &request.summary,
        }
    }

    fn related(&self) -> &[String] {
        match self {
            Self::Create(request) => &request.related,
            Self::Update(request) => &request.related,
        }
    }
}

fn validate_create(request: &CreateRequest) -> Result<String, SchemaError> {
    validate_common(CommonRequest::Create(request))
}

fn validate_update(request: &UpdateRequest) -> Result<String, SchemaError> {
    require_nonempty("target", &request.target)?;
    if !request.update {
        return Err(SchemaError::refusal("update must be true"));
    }
    if let Some(expected) = &request.expected_sha256
        && !is_sha256(expected)
    {
        return Err(SchemaError::refusal(
            "expected_sha256 must be 64 lowercase hex characters",
        ));
    }
    let slug = validate_common(CommonRequest::Update(request))?;
    if slug != request.target {
        return Err(SchemaError::refusal(
            "title does not slug to target — a rename is two pages and a forward link, which yams-wiki catalog owns",
        ));
    }
    Ok(slug)
}

fn validate_common(request: CommonRequest<'_>) -> Result<String, SchemaError> {
    for (field, value) in [
        ("title", request.title()),
        ("fact", request.fact()),
        ("why", request.why()),
        ("how_to_apply", request.how_to_apply()),
        ("falsified_by", request.falsified_by()),
        ("summary", request.summary()),
    ] {
        require_nonempty(field, value)?;
    }
    validate_frontmatter_scalar("title", request.title())?;
    validate_frontmatter_scalar("summary", request.summary())?;

    let slug = slugify(request.title())?;
    for related in request.related() {
        match validate_slug(related) {
            Ok(()) => {}
            Err(SlugProblem::TooLong) => {
                return Err(SchemaError::refusal(format!(
                    "related: {} — slug must be at most {SLUG_MAX_BYTES} bytes",
                    python_repr_string(related)
                )));
            }
            Err(SlugProblem::Empty | SlugProblem::InvalidCharacter) => {
                return Err(SchemaError::refusal(format!(
                    "related: {} is not slug-shaped",
                    python_repr_string(related)
                )));
            }
        }
        if related == &slug {
            return Err(SchemaError::refusal("related links the page to itself"));
        }
    }

    let probe = render_page(
        request,
        &slug,
        Owner::Shared,
        Status::Current,
        "2026-01-01",
        "2026-01-01",
    );
    if let Some(reference) = first_line_reference(&probe) {
        return Err(SchemaError::refusal(format!(
            "the page cites {reference} — line numbers drift; name the symbol instead"
        )));
    }
    Ok(slug)
}

fn validate_frontmatter_scalar(field: &str, value: &str) -> Result<(), SchemaError> {
    if let Some(problem) = scalar_problem(value) {
        return Err(SchemaError::refusal(format!("{field}: {problem}")));
    }
    if python_trim(value).trim_matches(['\'', '"']).is_empty() {
        return Err(SchemaError::refusal(format!(
            "{field}: is nothing but quotes and whitespace — the frontmatter parser strips those, leaving an empty field"
        )));
    }
    for marker in ["```", "~~~"] {
        if value.contains(marker) {
            return Err(SchemaError::refusal(format!(
                "{field}: contains a code fence marker ({marker}) — a one-line frontmatter scalar cannot open a block"
            )));
        }
    }
    Ok(())
}

pub(crate) fn scalar_problem(value: &str) -> Option<String> {
    if python_trim(value).is_empty() {
        return Some("summary is empty".to_owned());
    }
    for ch in value.chars() {
        if is_splitlines_boundary(ch) {
            return Some(format!(
                "summary contains a line boundary ({})",
                python_repr_char(ch)
            ));
        }
        let code = u32::from(ch);
        if code < 0x20 || (0x7f..=0x9f).contains(&code) {
            return Some(format!(
                "summary contains a control character ({})",
                python_repr_char(ch)
            ));
        }
    }
    if value.contains("<!--") || value.contains("-->") {
        return Some("summary contains an HTML comment delimiter".to_owned());
    }
    if value.contains("(pages/") && value.contains(".md)") {
        let link = Regex::new(r"\(pages/[^)]+\.md\)").expect("constant regex");
        if link.is_match(value) {
            return Some("summary contains index-link-shaped text".to_owned());
        }
    }
    None
}

fn render_page(
    request: CommonRequest<'_>,
    slug: &str,
    owner: Owner,
    status: Status,
    updated: &str,
    verified: &str,
) -> String {
    let mut page = format!(
        "---\nslug: {slug}\ntitle: {}\ntype: {}\nstatus: {}\nowner: {}\nupdated: {updated}\nverified: {verified}\nsummary: {}\n---\n\n{}\n\n**Why:** {}\n\n**How to apply:** {}\n\n**Falsified by:** {}\n",
        request.title(),
        request.page_type().as_str(),
        status.as_str(),
        owner.as_str(),
        request.summary(),
        request.fact(),
        request.why(),
        request.how_to_apply(),
        request.falsified_by(),
    );
    let mut seen = HashSet::new();
    let related = request
        .related()
        .iter()
        .filter(|slug| seen.insert(slug.as_str()))
        .map(|slug| format!("[[{slug}]]"))
        .collect::<Vec<_>>();
    if !related.is_empty() {
        page.push_str("\nRelated: ");
        page.push_str(&related.join(", "));
        page.push('\n');
    }
    page
}

fn first_line_reference(source: &str) -> Option<String> {
    let fence = Regex::new(r"(?s)```.*?```|~~~.*?~~~").expect("constant regex");
    let url = Regex::new(r"https?://[^\s\x1C-\x1F]+").expect("constant regex");
    let reference = Regex::new(r"[A-Za-z0-9_./-]+\.py:\d+").expect("constant regex");
    let without_fences = fence.replace_all(source, "");
    let without_exemptions = url.replace_all(&without_fences, "");
    reference
        .find(&without_exemptions)
        .map(|matched| matched.as_str().to_owned())
}

fn ensure_rendered_page(page: &str) -> Result<(), SchemaError> {
    let wiki = parse_wiki_page(page)?;
    let core = yams_core::parse_frontmatter(page);
    if wiki.fields() != &core.fields {
        let field = CANONICAL_KEYS
            .into_iter()
            .find(|key| wiki.fields().get(*key) != core.fields.get(*key))
            .unwrap_or("frontmatter");
        return Err(SchemaError::refusal(format!(
            "{field}: wiki and search frontmatter parsers disagree about the rendered value"
        )));
    }
    if wiki.fields().len() != CANONICAL_KEYS.len() {
        return Err(SchemaError::refusal(
            "rendered page does not contain exactly one of every canonical frontmatter key",
        ));
    }
    Ok(())
}

fn require_nonempty(field: &str, value: &str) -> Result<(), SchemaError> {
    if python_trim(value).is_empty() {
        Err(SchemaError::refusal(format!("{field} is required")))
    } else {
        Ok(())
    }
}

fn validate_today(today: &str) -> Result<(), SchemaError> {
    if is_iso_date(today) {
        Ok(())
    } else {
        Err(SchemaError::refusal(format!(
            "today: {today} — expected YYYY-MM-DD"
        )))
    }
}

pub(crate) fn is_iso_date(value: &str) -> bool {
    let mut chars = value.chars();
    for index in 0..10 {
        let Some(ch) = chars.next() else {
            return false;
        };
        if matches!(index, 4 | 7) {
            if ch != '-' {
                return false;
            }
        } else if get_general_category(ch) != GeneralCategory::DecimalNumber {
            return false;
        }
    }
    chars.next().is_none()
}

fn validate_page_slug(slug: &str) -> Result<(), SchemaError> {
    match validate_slug(slug) {
        Ok(()) => Ok(()),
        Err(SlugProblem::TooLong) => Err(SchemaError::refusal(format!(
            "page slug must be at most {SLUG_MAX_BYTES} bytes: {slug}"
        ))),
        Err(SlugProblem::Empty | SlugProblem::InvalidCharacter) => Err(SchemaError::refusal(
            format!("page has non-slug value: {slug}"),
        )),
    }
}

fn canonical_key(key: &str) -> bool {
    CANONICAL_KEYS.contains(&key)
}

fn required_page_field(
    fields: &BTreeMap<String, String>,
    field: &str,
) -> Result<String, SchemaError> {
    let value = &fields[field];
    if value.is_empty() {
        Err(SchemaError::refusal(format!("page is missing {field}")))
    } else {
        Ok(value.clone())
    }
}

fn display_missing(value: &str) -> &str {
    if value.is_empty() { "(none)" } else { value }
}

fn is_splitlines_boundary(ch: char) -> bool {
    matches!(
        ch,
        '\n' | '\r'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{001c}'
            | '\u{001d}'
            | '\u{001e}'
            | '\u{0085}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

fn python_repr_char(ch: char) -> String {
    let escaped = match ch {
        '\t' => "\\t".to_owned(),
        '\n' => "\\n".to_owned(),
        '\r' => "\\r".to_owned(),
        '\u{2028}' => "\\u2028".to_owned(),
        '\u{2029}' => "\\u2029".to_owned(),
        _ if u32::from(ch) <= 0xff => format!("\\x{:02x}", u32::from(ch)),
        _ => ch.to_string(),
    };
    format!("'{escaped}'")
}

fn python_repr_string(value: &str) -> String {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut escaped = String::new();
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '\t' => escaped.push_str("\\t"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            _ if ch == quote => {
                escaped.push('\\');
                escaped.push(ch);
            }
            _ if !is_python_printable(ch) => push_python_unicode_escape(&mut escaped, ch),
            _ => escaped.push(ch),
        }
    }
    format!("{quote}{escaped}{quote}")
}

fn is_python_printable(ch: char) -> bool {
    ch == ' '
        || !matches!(
            get_general_category(ch),
            GeneralCategory::Control
                | GeneralCategory::Format
                | GeneralCategory::LineSeparator
                | GeneralCategory::ParagraphSeparator
                | GeneralCategory::PrivateUse
                | GeneralCategory::SpaceSeparator
                | GeneralCategory::Surrogate
                | GeneralCategory::Unassigned
        )
}

fn push_python_unicode_escape(output: &mut String, ch: char) {
    let code = u32::from(ch);
    if code <= 0xff {
        output.push_str(&format!("\\x{code:02x}"));
    } else if code <= 0xffff {
        output.push_str(&format!("\\u{code:04x}"));
    } else {
        output.push_str(&format!("\\U{code:08x}"));
    }
}

fn python_splitlines(source: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut indices = source.char_indices().peekable();
    while let Some((index, ch)) = indices.next() {
        if !is_splitlines_boundary(ch) {
            continue;
        }
        lines.push(&source[start..index]);
        start = index + ch.len_utf8();
        if ch == '\r'
            && let Some(&(next_index, '\n')) = indices.peek()
        {
            indices.next();
            start = next_index + 1;
        }
    }
    if start < source.len() {
        lines.push(&source[start..]);
    }
    lines
}

fn without_dates(source: &str) -> String {
    let mut frontmatter = false;
    let mut closed = false;
    source
        .split_inclusive('\n')
        .filter(|line| {
            let content = line.strip_suffix('\n').unwrap_or(line);
            if !frontmatter && content == "---" {
                frontmatter = true;
                return true;
            }
            if frontmatter && !closed && content == "---" {
                closed = true;
                return true;
            }
            !(frontmatter
                && !closed
                && (content.starts_with("updated:") || content.starts_with("verified:")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::python_repr_string;

    #[test]
    fn every_unicode_scalar_repr_matches_the_python_3_12_digest() {
        let mut digest = Sha256::new();
        for code in 0..=0x10ffff {
            let Some(ch) = char::from_u32(code) else {
                continue;
            };
            let representation = python_repr_string(&ch.to_string());
            digest.update(code.to_be_bytes());
            digest.update([1]);
            digest.update(u32::try_from(representation.len()).unwrap().to_be_bytes());
            digest.update(representation.as_bytes());
        }
        assert_eq!(
            format!("{:x}", digest.finalize()),
            concat!(
                "1d367889", "297c5142", "a1f3d4ea", "faa80afb", "d2b6bcd0", "627d6449", "063b08b4",
                "e2729bb9",
            )
        );
    }
}
