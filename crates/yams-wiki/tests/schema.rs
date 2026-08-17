use std::collections::BTreeMap;

use serde_json::json;
use sha2::{Digest, Sha256};
use yams_core::parse_frontmatter as parse_core_frontmatter;
use yams_wiki::{
    CreateRequest, Owner, PageType, RenderedUpdate, Status, UpdateRequest, parse_wiki_page,
    render_create, render_update, slugify,
};

fn create_request() -> CreateRequest {
    serde_json::from_value(json!({
        "title": "A lunar console requires blue mode",
        "type": "gotcha",
        "owner": "codex",
        "fact": "A fictional lunar console rejects jobs unless blue mode is enabled.",
        "why": "Synthetic observations show jobs succeed only after enabling blue mode.",
        "how_to_apply": "Enable blue mode before submitting a fictional job.",
        "falsified_by": "A fictional job succeeding with blue mode disabled.",
        "summary": "fictional jobs require blue mode",
        "related": ["lunar-console-layout"]
    }))
    .unwrap()
}

fn update_request() -> UpdateRequest {
    serde_json::from_value(json!({
        "title": "A lunar console requires blue mode",
        "type": "gotcha",
        "fact": "A fictional lunar console rejects jobs unless blue mode is enabled.",
        "why": "Synthetic observations show jobs succeed only after enabling blue mode.",
        "how_to_apply": "Enable blue mode before submitting a fictional job.",
        "falsified_by": "A fictional job succeeding with blue mode disabled.",
        "summary": "fictional jobs require blue mode",
        "related": ["lunar-console-layout"],
        "update": true,
        "target": "a-lunar-console-requires-blue-mode"
    }))
    .unwrap()
}

#[test]
fn request_types_deny_unknown_and_mode_specific_fields() {
    for raw in [r#"{"z": 1, "a": 2}"#, r#"{"a": 2, "z": 1}"#] {
        let value = serde_json::from_str(raw).unwrap();
        assert_eq!(
            serde_json::from_value::<CreateRequest>(value)
                .unwrap_err()
                .to_string(),
            "unknown field(s): a, z"
        );
    }

    let value = serde_json::from_str(r#"{"update": true, "status": null, "a": 0}"#).unwrap();
    assert_eq!(
        serde_json::from_value::<CreateRequest>(value)
            .unwrap_err()
            .to_string(),
        "unknown field(s): a, status, update"
    );

    for raw in [r#"{"z": 1, "a": 2}"#, r#"{"a": 2, "z": 1}"#] {
        let value = serde_json::from_str(raw).unwrap();
        assert_eq!(
            serde_json::from_value::<UpdateRequest>(value)
                .unwrap_err()
                .to_string(),
            "unknown field(s): a, z"
        );
    }
}

#[test]
fn update_owner_and_status_keep_their_deliberate_refusal_messages() {
    let cases = [
        (
            "owner",
            "owner is refused on update — the stored value is preserved; changing it is a deliberate edit",
        ),
        (
            "status",
            "status is refused on update — the stored value is preserved; changing it is a deliberate edit",
        ),
    ];
    for (field, expected) in cases {
        for supplied in [json!("shared"), serde_json::Value::Null] {
            let mut update = serde_json::to_value(update_request()).unwrap();
            update[field] = supplied;
            assert_eq!(
                serde_json::from_value::<UpdateRequest>(update)
                    .unwrap_err()
                    .to_string(),
                expected
            );
        }
    }

    for raw in [
        r#"{"status": null, "owner": null, "typo": true}"#,
        r#"{"typo": true, "owner": null, "status": null}"#,
    ] {
        let value = serde_json::from_str(raw).unwrap();
        assert_eq!(
            serde_json::from_value::<UpdateRequest>(value)
                .unwrap_err()
                .to_string(),
            "owner is refused on update — the stored value is preserved; changing it is a deliberate edit"
        );
    }
}

#[test]
fn non_object_and_required_request_fields_have_stable_semantic_errors() {
    for value in [json!(null), json!([]), json!(7), json!("payload")] {
        assert_eq!(
            serde_json::from_value::<CreateRequest>(value.clone())
                .unwrap_err()
                .to_string(),
            "payload is not a JSON object"
        );
        assert_eq!(
            serde_json::from_value::<UpdateRequest>(value)
                .unwrap_err()
                .to_string(),
            "payload is not a JSON object"
        );
    }

    let create_fields = [
        "title",
        "type",
        "owner",
        "fact",
        "why",
        "how_to_apply",
        "falsified_by",
        "summary",
    ];
    for field in create_fields {
        for bad in [json!(null), json!(7), json!([]), json!(" \u{001c} ")] {
            let mut value = serde_json::to_value(create_request()).unwrap();
            value[field] = bad;
            assert_eq!(
                serde_json::from_value::<CreateRequest>(value)
                    .unwrap_err()
                    .to_string(),
                format!("{field} is required")
            );
        }
        let mut value = serde_json::to_value(create_request()).unwrap();
        value.as_object_mut().unwrap().remove(field);
        assert_eq!(
            serde_json::from_value::<CreateRequest>(value)
                .unwrap_err()
                .to_string(),
            format!("{field} is required")
        );
    }

    let update_fields = [
        "title",
        "type",
        "fact",
        "why",
        "how_to_apply",
        "falsified_by",
        "summary",
        "target",
    ];
    for field in update_fields {
        for bad in [json!(null), json!(7), json!([]), json!(" \u{001f} ")] {
            let mut value = serde_json::to_value(update_request()).unwrap();
            value[field] = bad;
            assert_eq!(
                serde_json::from_value::<UpdateRequest>(value)
                    .unwrap_err()
                    .to_string(),
                format!("{field} is required")
            );
        }
        let mut value = serde_json::to_value(update_request()).unwrap();
        value.as_object_mut().unwrap().remove(field);
        assert_eq!(
            serde_json::from_value::<UpdateRequest>(value)
                .unwrap_err()
                .to_string(),
            format!("{field} is required")
        );
    }
}

#[test]
fn invalid_request_enums_keep_exact_public_refusals() {
    let cases = [
        (
            "type",
            "note",
            "type: note — expected one of decision | feature | gotcha | pattern | project-state | workflow",
        ),
        (
            "owner",
            "nobody",
            "owner: nobody — expected one of claude | codex | shared",
        ),
    ];
    for (field, bad, expected) in cases {
        let mut value = serde_json::to_value(create_request()).unwrap();
        value[field] = json!(bad);
        assert_eq!(
            serde_json::from_value::<CreateRequest>(value)
                .unwrap_err()
                .to_string(),
            expected
        );
    }
}

#[test]
fn update_requires_an_explicit_related_list() {
    for bad in [None, Some(json!(null)), Some(json!(7))] {
        let mut value = serde_json::to_value(update_request()).unwrap();
        if let Some(bad) = bad {
            value["related"] = bad;
        } else {
            value.as_object_mut().unwrap().remove("related");
        }
        assert_eq!(
            serde_json::from_value::<UpdateRequest>(value)
                .unwrap_err()
                .to_string(),
            "related is required on update; [] clears it"
        );
    }

    let mut value = serde_json::to_value(update_request()).unwrap();
    value["related"] = json!(["valid", 7]);
    assert_eq!(
        serde_json::from_value::<UpdateRequest>(value)
            .unwrap_err()
            .to_string(),
        "related must be a list of slugs"
    );
}

#[test]
fn create_defaults_an_omitted_related_list_to_empty() {
    for related in [None, Some(serde_json::Value::Null)] {
        let mut value = serde_json::to_value(create_request()).unwrap();
        if let Some(related) = related {
            value["related"] = related;
        } else {
            value.as_object_mut().unwrap().remove("related");
        }
        let request: CreateRequest = serde_json::from_value(value).unwrap();
        assert!(request.related.is_empty());
        assert!(
            !render_create(&request, "2026-08-07")
                .unwrap()
                .contains("Related:")
        );
    }

    for bad in [json!(7), json!({"slug": "alpha"}), json!(["alpha", 7])] {
        let mut value = serde_json::to_value(create_request()).unwrap();
        value["related"] = bad;
        assert_eq!(
            serde_json::from_value::<CreateRequest>(value)
                .unwrap_err()
                .to_string(),
            "related must be a list of slugs"
        );
    }
}

#[test]
fn create_treats_every_python_falsey_related_value_as_omitted() {
    for raw_related in ["null", "false", "0", "-0.0", "\"\"", "{}", "[]"] {
        let mut value = serde_json::to_value(create_request()).unwrap();
        value["related"] = serde_json::from_str(raw_related).unwrap();
        let raw = serde_json::to_string(&value).unwrap();
        let decoded: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let request: CreateRequest = serde_json::from_value(decoded).unwrap();
        assert!(request.related.is_empty(), "{raw_related}");
    }

    for raw_related in [
        "true",
        "1",
        "-1",
        "1.0",
        r#""alpha""#,
        r#"{"slug":"alpha"}"#,
    ] {
        let mut value = serde_json::to_value(create_request()).unwrap();
        value["related"] = serde_json::from_str(raw_related).unwrap();
        let raw = serde_json::to_string(&value).unwrap();
        let decoded: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            serde_json::from_value::<CreateRequest>(decoded)
                .unwrap_err()
                .to_string(),
            "related must be a list of slugs",
            "{raw_related}"
        );
    }
}

#[test]
fn scalar_hazards_precede_enum_conversion_in_semantic_requests() {
    let cases = [
        (
            "title",
            "bad\nvalue",
            "title: summary contains a line boundary ('\\n')",
        ),
        (
            "summary",
            "bad ``` value",
            "summary: contains a code fence marker (```) — a one-line frontmatter scalar cannot open a block",
        ),
        (
            "title",
            "'''",
            "title: is nothing but quotes and whitespace — the frontmatter parser strips those, leaving an empty field",
        ),
    ];

    for (field, bad, expected) in cases {
        let mut create = serde_json::to_value(create_request()).unwrap();
        create[field] = json!(bad);
        create["type"] = json!("note");
        create["owner"] = json!("nobody");
        let raw = serde_json::to_string(&create).unwrap();
        let decoded = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            serde_json::from_value::<CreateRequest>(decoded)
                .unwrap_err()
                .to_string(),
            expected,
            "create {field}: {bad:?}"
        );

        let mut update = serde_json::to_value(update_request()).unwrap();
        update[field] = json!(bad);
        update["type"] = json!("note");
        let raw = serde_json::to_string(&update).unwrap();
        let decoded = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            serde_json::from_value::<UpdateRequest>(decoded)
                .unwrap_err()
                .to_string(),
            expected,
            "update {field}: {bad:?}"
        );
    }

    let mut update = serde_json::to_value(update_request()).unwrap();
    update["title"] = json!("bad\nvalue");
    update["type"] = json!("note");
    update["expected_sha256"] = json!("bad");
    assert_eq!(
        serde_json::from_value::<UpdateRequest>(update)
            .unwrap_err()
            .to_string(),
        "expected_sha256 must be 64 lowercase hex characters"
    );

    for update_mode in [false, true] {
        let mut value = if update_mode {
            serde_json::to_value(update_request()).unwrap()
        } else {
            serde_json::to_value(create_request()).unwrap()
        };
        value["title"] = json!("bad\nvalue");
        value["type"] = json!("note");
        value["unexpected"] = json!(true);
        let error = if update_mode {
            serde_json::from_value::<UpdateRequest>(value.clone())
                .unwrap_err()
                .to_string()
        } else {
            serde_json::from_value::<CreateRequest>(value.clone())
                .unwrap_err()
                .to_string()
        };
        assert_eq!(error, "unknown field(s): unexpected");

        value.as_object_mut().unwrap().remove("unexpected");
        value.as_object_mut().unwrap().remove("fact");
        let error = if update_mode {
            serde_json::from_value::<UpdateRequest>(value)
                .unwrap_err()
                .to_string()
        } else {
            serde_json::from_value::<CreateRequest>(value)
                .unwrap_err()
                .to_string()
        };
        assert_eq!(error, "fact is required");
    }
}

#[test]
fn create_empty_slug_precedes_related_shape_decoding() {
    let mut create = serde_json::to_value(create_request()).unwrap();
    create["title"] = json!("日本語");
    create["related"] = json!(7);

    let mut unknown = create.clone();
    unknown["unexpected"] = json!(true);
    assert_eq!(
        serde_json::from_value::<CreateRequest>(unknown)
            .unwrap_err()
            .to_string(),
        "unknown field(s): unexpected"
    );

    let mut missing = create.clone();
    missing.as_object_mut().unwrap().remove("fact");
    assert_eq!(
        serde_json::from_value::<CreateRequest>(missing)
            .unwrap_err()
            .to_string(),
        "fact is required"
    );

    let mut invalid_type = create.clone();
    invalid_type["type"] = json!("note");
    assert_eq!(
        serde_json::from_value::<CreateRequest>(invalid_type)
            .unwrap_err()
            .to_string(),
        "type: note — expected one of decision | feature | gotcha | pattern | project-state | workflow"
    );

    let mut invalid_owner = create.clone();
    invalid_owner["owner"] = json!("nobody");
    assert_eq!(
        serde_json::from_value::<CreateRequest>(invalid_owner)
            .unwrap_err()
            .to_string(),
        "owner: nobody — expected one of claude | codex | shared"
    );

    assert_eq!(
        serde_json::from_value::<CreateRequest>(create)
            .unwrap_err()
            .to_string(),
        "title produces an empty slug"
    );
}

#[test]
fn update_empty_slug_precedes_related_element_decoding() {
    let mut update = serde_json::to_value(update_request()).unwrap();
    update["title"] = json!("日本語");
    update["related"] = json!([7]);

    let mut unknown = update.clone();
    unknown["unexpected"] = json!(true);
    assert_eq!(
        serde_json::from_value::<UpdateRequest>(unknown)
            .unwrap_err()
            .to_string(),
        "unknown field(s): unexpected"
    );

    let mut missing = update.clone();
    missing.as_object_mut().unwrap().remove("fact");
    assert_eq!(
        serde_json::from_value::<UpdateRequest>(missing)
            .unwrap_err()
            .to_string(),
        "fact is required"
    );

    let mut invalid_type = update.clone();
    invalid_type["type"] = json!("note");
    assert_eq!(
        serde_json::from_value::<UpdateRequest>(invalid_type)
            .unwrap_err()
            .to_string(),
        "type: note — expected one of decision | feature | gotcha | pattern | project-state | workflow"
    );

    assert_eq!(
        serde_json::from_value::<UpdateRequest>(update)
            .unwrap_err()
            .to_string(),
        "title produces an empty slug"
    );
}

#[test]
fn update_mode_and_expected_hash_shapes_have_exact_errors() {
    for bad in [
        None,
        Some(json!(null)),
        Some(json!(false)),
        Some(json!("true")),
    ] {
        let mut value = serde_json::to_value(update_request()).unwrap();
        if let Some(bad) = bad {
            value["update"] = bad;
        } else {
            value.as_object_mut().unwrap().remove("update");
        }
        assert_eq!(
            serde_json::from_value::<UpdateRequest>(value)
                .unwrap_err()
                .to_string(),
            "update must be true"
        );
    }

    let mut value = serde_json::to_value(update_request()).unwrap();
    value["expected_sha256"] = serde_json::Value::Null;
    assert!(serde_json::from_value::<UpdateRequest>(value).is_ok());

    for bad in [json!(7), json!("A".repeat(64)), json!("abc")] {
        let mut value = serde_json::to_value(update_request()).unwrap();
        value["expected_sha256"] = bad;
        assert_eq!(
            serde_json::from_value::<UpdateRequest>(value)
                .unwrap_err()
                .to_string(),
            "expected_sha256 must be 64 lowercase hex characters"
        );
    }
}

#[test]
fn page_types_have_exact_values_and_fixed_headings() {
    let cases = [
        (PageType::Gotcha, "gotcha", "Gotchas"),
        (PageType::Pattern, "pattern", "Patterns"),
        (PageType::Decision, "decision", "Decisions"),
        (PageType::Workflow, "workflow", "Workflow"),
        (PageType::ProjectState, "project-state", "Project state"),
        (
            PageType::Feature,
            "feature",
            "Features — architecture pointers",
        ),
    ];

    for (page_type, serialized, heading) in cases {
        assert_eq!(serde_json::to_value(page_type).unwrap(), json!(serialized));
        assert_eq!(page_type.as_str(), serialized);
        assert_eq!(page_type.heading(), heading);
    }
}

#[test]
fn slugging_matches_nfkd_and_full_casefold_contract() {
    let cases = [
        (
            "A lunar console requires blue mode",
            "a-lunar-console-requires-blue-mode",
        ),
        ("Already-Kebab", "already-kebab"),
        ("Ünïcodé Títle", "unicode-title"),
        ("Straße", "strasse"),
        ("ẞ", "ss"),
        ("İstanbul", "istanbul"),
        ("ﬃ", "ffi"),
        ("ſearch", "search"),
        ("ＡＦ＿ＵＮＩＸ", "af-unix"),
        ("Øl og lefse", "l-og-lefse"),
        ("łódź", "odz"),
        ("  padded  ", "padded"),
        ("multiple---dashes", "multiple-dashes"),
    ];

    for (title, expected) in cases {
        assert_eq!(slugify(title).unwrap(), expected, "{title:?}");
    }
    assert_eq!(
        slugify(&("alpha ".repeat(12) + "omegaomega")).unwrap(),
        "alpha-alpha-alpha-alpha-alpha-alpha-alpha-alpha-alpha-alpha"
    );
    assert_eq!(slugify(&"x".repeat(200)).unwrap(), "x".repeat(64));
    assert_eq!(
        slugify("日本語").unwrap_err().to_string(),
        "title produces an empty slug"
    );
}

#[test]
fn canonical_slug_boundary_is_shared_by_stored_pages_and_related_links() {
    let canonical = render_create(&create_request(), "2026-08-07").unwrap();
    for length in 1..=64 {
        let slug = "a".repeat(length);
        let page = canonical.replacen(
            "slug: a-lunar-console-requires-blue-mode",
            &format!("slug: {slug}"),
            1,
        );
        assert_eq!(parse_wiki_page(&page).unwrap().slug, slug, "{length}");

        let mut create = create_request();
        create.related = vec![slug.clone()];
        assert!(render_create(&create, "2026-08-07").is_ok(), "{length}");

        let mut update = update_request();
        update.related = vec![slug];
        assert!(
            render_update(&update, &canonical, "2026-08-08").is_ok(),
            "{length}"
        );
    }

    let too_long = "a".repeat(65);
    let page = canonical.replacen(
        "slug: a-lunar-console-requires-blue-mode",
        &format!("slug: {too_long}"),
        1,
    );
    assert_eq!(
        parse_wiki_page(&page).unwrap_err().to_string(),
        format!("page slug must be at most 64 bytes: {too_long}")
    );

    let related_error = format!("related: '{too_long}' — slug must be at most 64 bytes");
    let mut create = create_request();
    create.related = vec![too_long.clone()];
    assert_eq!(
        render_create(&create, "2026-08-07")
            .unwrap_err()
            .to_string(),
        related_error
    );

    let mut update = update_request();
    update.related = vec![too_long];
    assert_eq!(
        render_update(&update, &canonical, "2026-08-08")
            .unwrap_err()
            .to_string(),
        related_error
    );
}

#[test]
fn slug_unicode_tables_and_combining_rule_match_python_3_12() {
    assert_eq!(unicode_normalization::UNICODE_VERSION, (15, 0, 0));
    assert_eq!(unicode_casefold::UNICODE_VERSION, (9, 0, 0));

    for code in [0x034f, 0x0903, 0x1ccd6] {
        let ch = char::from_u32(code).unwrap();
        assert_eq!(slugify(&format!("a{ch}b")).unwrap(), "a-b", "U+{code:04X}");
    }
}

#[test]
fn general_category_tables_match_python_3_12_unicode_15() {
    assert_eq!(unicode_general_category::UNICODE_VERSION, (15, 0, 0));
}

#[test]
fn every_unicode_scalar_slug_matches_the_python_3_12_digest() {
    let mut standalone = Sha256::new();
    let mut surrounded = Sha256::new();
    for code in 0..=0x10ffff {
        let Some(ch) = char::from_u32(code) else {
            continue;
        };
        for (hasher, title) in [
            (&mut standalone, ch.to_string()),
            (&mut surrounded, format!("a{ch}b")),
        ] {
            let slug = slugify(&title).unwrap_or_default();
            hasher.update(code.to_be_bytes());
            hasher.update([1]);
            hasher.update(u32::try_from(slug.len()).unwrap().to_be_bytes());
            hasher.update(slug.as_bytes());
        }
    }
    assert_eq!(
        format!("{:x}", standalone.finalize()),
        concat!(
            "d27eec73", "b4a48694", "2f1dec8f", "78d3fe20", "8c63987c", "d7d1be96", "a9156cd7",
            "a91351b3",
        )
    );
    assert_eq!(
        format!("{:x}", surrounded.finalize()),
        concat!(
            "eccea927", "4f9fe75d", "419808e4", "acdf8b99", "a7339447", "83e8dab7", "6db4a9ee",
            "660a46e5",
        )
    );
}

#[test]
fn required_scalars_are_nonempty_with_exact_refusals() {
    let cases = [
        ("title", "title is required"),
        ("fact", "fact is required"),
        ("why", "why is required"),
        ("how_to_apply", "how_to_apply is required"),
        ("falsified_by", "falsified_by is required"),
        ("summary", "summary is required"),
    ];

    for (field, expected) in cases {
        let mut request = create_request();
        match field {
            "title" => request.title = " \t ".into(),
            "fact" => request.fact = " \t ".into(),
            "why" => request.why = " \t ".into(),
            "how_to_apply" => request.how_to_apply = " \t ".into(),
            "falsified_by" => request.falsified_by = " \t ".into(),
            "summary" => request.summary = " \t ".into(),
            _ => unreachable!(),
        }
        assert_eq!(
            render_create(&request, "2026-08-07")
                .unwrap_err()
                .to_string(),
            expected,
            "{field}"
        );
    }
}

#[test]
fn programmatic_requests_use_python_whitespace_and_strip_precedence() {
    for code in 0x1c..=0x1f {
        let whitespace = char::from_u32(code).unwrap().to_string();
        let mut request = create_request();
        request.fact = whitespace.clone();
        assert_eq!(
            render_create(&request, "2026-08-07")
                .unwrap_err()
                .to_string(),
            "fact is required",
            "U+{code:04X}"
        );

        request = create_request();
        request.title = whitespace;
        assert_eq!(
            render_create(&request, "2026-08-07")
                .unwrap_err()
                .to_string(),
            "title is required",
            "U+{code:04X}"
        );
    }

    let mut request = create_request();
    request.title = "' '".into();
    assert_eq!(
        render_create(&request, "2026-08-07")
            .unwrap_err()
            .to_string(),
        "title produces an empty slug"
    );

    request = create_request();
    request.summary = "' '".into();
    assert_eq!(
        render_create(&request, "2026-08-07")
            .unwrap_err()
            .to_string(),
        "summary: summary is empty"
    );

    for field in ["title", "summary"] {
        let mut request = create_request();
        if field == "title" {
            request.title = " ' ".into();
        } else {
            request.summary = " ' ".into();
        }
        assert_eq!(
            render_create(&request, "2026-08-07")
                .unwrap_err()
                .to_string(),
            format!(
                "{field}: is nothing but quotes and whitespace — the frontmatter parser strips those, leaving an empty field"
            )
        );
    }
}

#[test]
fn hostile_scalar_refusals_match_the_public_python_strings() {
    let cases = [
        (
            "summary",
            "a\nstatus: historical",
            "summary: summary contains a line boundary ('\\n')",
        ),
        (
            "summary",
            "a\u{000b}b",
            "summary: summary contains a line boundary ('\\x0b')",
        ),
        (
            "summary",
            "a\u{0085}b",
            "summary: summary contains a line boundary ('\\x85')",
        ),
        (
            "summary",
            "a\u{2028}b",
            "summary: summary contains a line boundary ('\\u2028')",
        ),
        (
            "summary",
            "a\u{0000}b",
            "summary: summary contains a control character ('\\x00')",
        ),
        (
            "summary",
            "a\u{009b}b",
            "summary: summary contains a control character ('\\x9b')",
        ),
        (
            "summary",
            "ends with <!--",
            "summary: summary contains an HTML comment delimiter",
        ),
        (
            "summary",
            "has --> inside",
            "summary: summary contains an HTML comment delimiter",
        ),
        (
            "summary",
            "see (pages/ghost.md) for more",
            "summary: summary contains index-link-shaped text",
        ),
        (
            "title",
            "a\u{2029}status: historical",
            "title: summary contains a line boundary ('\\u2029')",
        ),
        (
            "title",
            "Fences ``` here",
            "title: contains a code fence marker (```) — a one-line frontmatter scalar cannot open a block",
        ),
        (
            "summary",
            "Fences ~~~ here",
            "summary: contains a code fence marker (~~~) — a one-line frontmatter scalar cannot open a block",
        ),
        (
            "summary",
            "'''",
            "summary: is nothing but quotes and whitespace — the frontmatter parser strips those, leaving an empty field",
        ),
    ];

    for (field, bad, expected) in cases {
        let mut request = create_request();
        if field == "title" {
            request.title = bad.into();
        } else {
            request.summary = bad.into();
        }
        assert_eq!(
            render_create(&request, "2026-08-07")
                .unwrap_err()
                .to_string(),
            expected,
            "{field}: {bad:?}"
        );
    }
}

#[test]
fn edge_quotes_render_with_the_same_core_strict_interpretation() {
    for field in ["title", "summary"] {
        for bad in ["''abc''", "'abc", "abc\""] {
            let mut value = serde_json::to_value(create_request()).unwrap();
            value[field] = json!(bad);
            let request: CreateRequest = serde_json::from_value(value).unwrap();
            let page = render_create(&request, "2026-08-07").unwrap();
            assert_eq!(
                parse_wiki_page(&page).unwrap().fields(),
                &parse_core_frontmatter(&page).fields,
                "{field}: {bad:?}"
            );
        }
    }
}

#[test]
fn every_python_splitlines_boundary_has_its_exact_refusal() {
    let cases = [
        ('\n', "'\\n'"),
        ('\r', "'\\r'"),
        ('\u{000b}', "'\\x0b'"),
        ('\u{000c}', "'\\x0c'"),
        ('\u{001c}', "'\\x1c'"),
        ('\u{001d}', "'\\x1d'"),
        ('\u{001e}', "'\\x1e'"),
        ('\u{0085}', "'\\x85'"),
        ('\u{2028}', "'\\u2028'"),
        ('\u{2029}', "'\\u2029'"),
    ];

    for (separator, python_repr) in cases {
        let mut request = create_request();
        request.summary = format!("a{separator}b");
        assert_eq!(
            render_create(&request, "2026-08-07")
                .unwrap_err()
                .to_string(),
            format!("summary: summary contains a line boundary ({python_repr})")
        );
    }
}

#[test]
fn every_remaining_c0_c1_control_has_its_exact_refusal() {
    let boundaries = [0x0a, 0x0b, 0x0c, 0x0d, 0x1c, 0x1d, 0x1e, 0x85];
    for code in (0x00..=0x1f).chain(0x7f..=0x9f) {
        if boundaries.contains(&code) {
            continue;
        }
        let control = char::from_u32(code).unwrap();
        let python_repr = if control == '\t' {
            "'\\t'".to_owned()
        } else {
            format!("'\\x{code:02x}'")
        };
        let mut request = create_request();
        request.summary = format!("a{control}b");
        assert_eq!(
            render_create(&request, "2026-08-07")
                .unwrap_err()
                .to_string(),
            format!("summary: summary contains a control character ({python_repr})"),
            "U+{code:04X}"
        );
    }
}

#[test]
fn line_references_and_related_links_are_validated_like_python() {
    let mut request = create_request();
    request.fact = "see module.py:123".into();
    assert_eq!(
        render_create(&request, "2026-08-07")
            .unwrap_err()
            .to_string(),
        "the page cites module.py:123 — line numbers drift; name the symbol instead"
    );

    request.fact = "Example:\n\n```\nmodule.py:123\n```".into();
    assert!(render_create(&request, "2026-08-07").is_ok());

    request.fact = "See https://example.test/module.py:123 for context.".into();
    assert!(render_create(&request, "2026-08-07").is_ok());

    request.related = vec!["Not-A-Slug".into()];
    assert_eq!(
        render_create(&request, "2026-08-07")
            .unwrap_err()
            .to_string(),
        "related: 'Not-A-Slug' is not slug-shaped"
    );

    request.related = vec!["a-lunar-console-requires-blue-mode".into()];
    assert_eq!(
        render_create(&request, "2026-08-07")
            .unwrap_err()
            .to_string(),
        "related links the page to itself"
    );
}

#[test]
fn related_refusals_use_python_repr_without_raw_controls() {
    let cases = [
        ("bad\u{0007}", "related: 'bad\\x07' is not slug-shaped"),
        ("bad\nline", "related: 'bad\\nline' is not slug-shaped"),
        ("has'quote", "related: \"has'quote\" is not slug-shaped"),
        ("has\"quote", "related: 'has\"quote' is not slug-shaped"),
        ("bad\u{00ad}", "related: 'bad\\xad' is not slug-shaped"),
        ("bad\u{0378}", "related: 'bad\\u0378' is not slug-shaped"),
        ("bad\u{061c}", "related: 'bad\\u061c' is not slug-shaped"),
        ("bad\u{1680}", "related: 'bad\\u1680' is not slug-shaped"),
        ("bad\u{200b}", "related: 'bad\\u200b' is not slug-shaped"),
        ("bad\u{202e}", "related: 'bad\\u202e' is not slug-shaped"),
        ("bad\u{2060}", "related: 'bad\\u2060' is not slug-shaped"),
        ("bad\u{e000}", "related: 'bad\\ue000' is not slug-shaped"),
        ("bad\u{feff}", "related: 'bad\\ufeff' is not slug-shaped"),
        (
            "bad\u{10ffff}",
            "related: 'bad\\U0010ffff' is not slug-shaped",
        ),
    ];
    for (related, expected) in cases {
        let mut request = create_request();
        request.related = vec![related.into()];
        let error = render_create(&request, "2026-08-07")
            .unwrap_err()
            .to_string();
        assert_eq!(error, expected);
        assert!(!error.chars().any(char::is_control));
    }
}

#[test]
fn python_only_whitespace_ends_the_url_line_reference_exemption() {
    for code in 0x1c..=0x1f {
        let separator = char::from_u32(code).unwrap();
        let mut request = create_request();
        request.fact = format!("https://example.test/a{separator}module.py:123");
        assert_eq!(
            render_create(&request, "2026-08-07")
                .unwrap_err()
                .to_string(),
            "the page cites module.py:123 — line numbers drift; name the symbol instead",
            "U+{code:04X}"
        );
    }
}

#[test]
fn canonical_render_matches_the_repository_memory_schema() {
    let page = render_create(&create_request(), "2026-08-07").unwrap();
    assert_eq!(
        page,
        "---\n\
slug: a-lunar-console-requires-blue-mode\n\
title: A lunar console requires blue mode\n\
type: gotcha\n\
status: current\n\
owner: codex\n\
updated: 2026-08-07\n\
verified: 2026-08-07\n\
summary: fictional jobs require blue mode\n\
---\n\
\n\
A fictional lunar console rejects jobs unless blue mode is enabled.\n\
\n\
**Why:** Synthetic observations show jobs succeed only after enabling blue mode.\n\
\n\
**How to apply:** Enable blue mode before submitting a fictional job.\n\
\n\
**Falsified by:** A fictional job succeeding with blue mode disabled.\n\
\n\
Related: [[lunar-console-layout]]\n"
    );
    assert!(!page.contains("## Why"));
}

#[test]
fn render_round_trips_through_independent_parsers_with_one_of_each_key() {
    let page = render_create(&create_request(), "2026-08-07").unwrap();
    let wiki = parse_wiki_page(&page).unwrap();
    let core = parse_core_frontmatter(&page);

    assert_eq!(wiki.fields(), &core.fields);
    assert_eq!(wiki.summary, create_request().summary);

    let canonical_keys = [
        "slug", "title", "type", "status", "owner", "updated", "verified", "summary",
    ];
    assert_eq!(wiki.fields().len(), canonical_keys.len());
    for key in canonical_keys {
        assert_eq!(
            page.lines()
                .filter(|line| line.starts_with(&format!("{key}: ")))
                .count(),
            1,
            "{key}"
        );
    }
}

#[test]
fn independent_parser_rejects_missing_duplicate_and_unknown_canonical_fields() {
    let page = render_create(&create_request(), "2026-08-07").unwrap();
    let missing = page.replacen("summary: fictional jobs require blue mode\n", "", 1);
    assert_eq!(
        parse_wiki_page(&missing).unwrap_err().to_string(),
        "page is missing summary"
    );

    let duplicate = page.replacen(
        "summary: fictional jobs require blue mode\n",
        "summary: first\nsummary: second\n",
        1,
    );
    assert_eq!(
        parse_wiki_page(&duplicate).unwrap_err().to_string(),
        "page has duplicate frontmatter key: summary"
    );

    let unknown = page.replacen(
        "summary: fictional jobs require blue mode\n",
        "summary: fictional jobs require blue mode\nextra: refused\n",
        1,
    );
    assert_eq!(
        parse_wiki_page(&unknown).unwrap_err().to_string(),
        "page has unknown frontmatter key: extra"
    );

    let backwards_dates = page.replace("verified: 2026-08-07", "verified: 2026-08-01");
    assert_eq!(
        parse_wiki_page(&backwards_dates).unwrap_err().to_string(),
        "page has verified: 2026-08-01 before updated: 2026-08-07 — editing a page verifies it"
    );
}

#[test]
fn independent_wiki_parser_matches_core_strict_frontmatter_mechanics() {
    let page = render_create(&create_request(), "2026-08-07").unwrap();
    let invalid = [
        page.replacen("---\n", " ---\n", 1),
        page.replacen("\n---\n\n", "\n--- \n\n", 1),
        format!("\u{feff}\u{feff}{page}"),
    ];
    for source in invalid {
        assert!(parse_core_frontmatter(&source).fields.is_empty());
        assert_eq!(
            parse_wiki_page(&source).unwrap_err().to_string(),
            "page has no parseable frontmatter block"
        );
    }

    let continued = page.replacen(
        "slug: a-lunar-console-requires-blue-mode\n",
        "slug: a-lunar-console-requires-blue-mode\n  nested: ignored\n",
        1,
    );
    assert_eq!(
        parse_wiki_page(&continued).unwrap().fields(),
        &parse_core_frontmatter(&continued).fields
    );

    for title in ["'abc", "abc\"", "''abc''", "'abc'"] {
        let source = page.replace(
            "title: A lunar console requires blue mode",
            &format!("title: {title}"),
        );
        assert_eq!(
            parse_wiki_page(&source).unwrap().fields(),
            &parse_core_frontmatter(&source).fields,
            "{title:?}"
        );
    }
}

#[test]
fn duplicate_related_links_collapse_in_first_seen_order() {
    let mut request = create_request();
    request.related = vec!["alpha".into(), "alpha".into(), "beta".into()];
    let page = render_create(&request, "2026-08-07").unwrap();
    assert_eq!(page.matches("[[alpha]]").count(), 1);
    assert!(page.find("[[alpha]]").unwrap() < page.find("[[beta]]").unwrap());
}

#[test]
fn unchanged_updates_move_only_verified_while_changes_move_both_dates() {
    let current = render_create(&create_request(), "2026-08-07").unwrap();
    let unchanged = render_update(&update_request(), &current, "2026-08-09").unwrap();
    assert_eq!(
        unchanged,
        RenderedUpdate {
            page: current.replace("verified: 2026-08-07", "verified: 2026-08-09"),
            content_changed: false,
        }
    );

    let mut changed_request = update_request();
    changed_request.fact = "Genuinely different.".into();
    let changed = render_update(&changed_request, &current, "2026-08-09").unwrap();
    assert!(changed.content_changed);
    assert!(changed.page.contains("updated: 2026-08-09"));
    assert!(changed.page.contains("verified: 2026-08-09"));
}

#[test]
fn update_preserves_stored_owner_and_status_and_validates_request_guards() {
    let current = render_create(&create_request(), "2026-08-07")
        .unwrap()
        .replace("status: current", "status: historical")
        .replace("owner: codex", "owner: shared");
    let rendered = render_update(&update_request(), &current, "2026-08-09").unwrap();
    assert!(rendered.page.contains("status: historical"));
    assert!(rendered.page.contains("owner: shared"));

    let mut request = update_request();
    request.update = false;
    assert_eq!(
        render_update(&request, &current, "2026-08-09")
            .unwrap_err()
            .to_string(),
        "update must be true"
    );

    request = update_request();
    request.target = "different".into();
    assert_eq!(
        render_update(&request, &current, "2026-08-09")
            .unwrap_err()
            .to_string(),
        "title does not slug to target — a rename is two pages and a forward link, which yams-wiki catalog owns"
    );

    request = update_request();
    request.expected_sha256 = Some("ABC".into());
    assert_eq!(
        render_update(&request, &current, "2026-08-09")
            .unwrap_err()
            .to_string(),
        "expected_sha256 must be 64 lowercase hex characters"
    );

    request = update_request();
    request.expected_sha256 = Some(format!("{:x}", Sha256::digest(current.as_bytes())));
    assert!(render_update(&request, &current, "2026-08-09").is_ok());

    request.expected_sha256 = Some("0".repeat(64));
    assert_eq!(
        render_update(&request, &current, "2026-08-09")
            .unwrap_err()
            .to_string(),
        "the page changed since it was read"
    );
}

#[test]
fn date_inputs_and_stored_dates_have_exact_refusals() {
    assert_eq!(
        render_create(&create_request(), "August 7")
            .unwrap_err()
            .to_string(),
        "today: August 7 — expected YYYY-MM-DD"
    );

    let current = render_create(&create_request(), "2026-08-07")
        .unwrap()
        .replace("updated: 2026-08-07", "updated: 2026-09-01");
    assert_eq!(
        render_update(&update_request(), &current, "2026-08-09")
            .unwrap_err()
            .to_string(),
        "pages/a-lunar-console-requires-blue-mode.md has updated: 2026-09-01 — expected YYYY-MM-DD no later than today (2026-08-09)"
    );

    let current = render_create(&create_request(), "2026-08-07")
        .unwrap()
        .replace("updated: 2026-08-07", "updated: yesterday");
    assert_eq!(
        render_update(&update_request(), &current, "2026-08-09")
            .unwrap_err()
            .to_string(),
        "pages/a-lunar-console-requires-blue-mode.md has updated: yesterday — expected YYYY-MM-DD no later than today (2026-08-09)"
    );
}

#[test]
fn iso_dates_accept_python_unicode_decimal_digits() {
    let page = render_create(&create_request(), "٢٠٢٦-٠٨-٠٧").unwrap();
    assert!(page.contains("updated: ٢٠٢٦-٠٨-٠٧"));
    assert!(page.contains("verified: ٢٠٢٦-٠٨-٠٧"));

    assert_eq!(
        render_create(&create_request(), "ⅫⅫⅫⅫ-ⅫⅫ-ⅫⅫ")
            .unwrap_err()
            .to_string(),
        "today: ⅫⅫⅫⅫ-ⅫⅫ-ⅫⅫ — expected YYYY-MM-DD"
    );

    assert_eq!(
        (0..=0x10ffff)
            .filter_map(char::from_u32)
            .filter(|ch| {
                unicode_general_category::get_general_category(*ch)
                    == unicode_general_category::GeneralCategory::DecimalNumber
            })
            .count(),
        680
    );

    let unicode_16_digit = char::from_u32(0x10d40).unwrap();
    let too_new = format!("{0}{0}{0}{0}-{0}{0}-{0}{0}", unicode_16_digit);
    assert_eq!(
        render_create(&create_request(), &too_new)
            .unwrap_err()
            .to_string(),
        format!("today: {too_new} — expected YYYY-MM-DD")
    );
}

#[test]
fn enum_round_trip_values_are_exact() {
    let fields = BTreeMap::from([
        ("owner", serde_json::to_string(&Owner::Shared).unwrap()),
        (
            "status",
            serde_json::to_string(&Status::InProgress).unwrap(),
        ),
    ]);
    assert_eq!(fields["owner"], "\"shared\"");
    assert_eq!(fields["status"], "\"in-progress\"");
}

#[test]
fn obsidian_normalized_frontmatter_is_accepted_and_recanonicalized() {
    let canonical = render_create(&create_request(), "2026-08-07").unwrap();
    // Obsidian normalization: quoted scalar + reordered keys.
    let normalized = canonical.replace(
        "slug: a-lunar-console-requires-blue-mode\ntitle: A lunar console requires blue mode",
        "title: \"A lunar console requires blue mode\"\nslug: a-lunar-console-requires-blue-mode",
    );
    assert_ne!(normalized, canonical);
    assert_eq!(
        parse_wiki_page(&normalized).unwrap().fields(),
        parse_wiki_page(&canonical).unwrap().fields(),
    );
    let rendered = render_update(&update_request(), &normalized, "2026-08-09").unwrap();
    let expected = render_update(&update_request(), &canonical, "2026-08-09").unwrap();
    assert_eq!(rendered, expected);
}

#[test]
fn obsidian_added_frontmatter_keys_remain_a_hard_rejection() {
    let canonical = render_create(&create_request(), "2026-08-07").unwrap();
    let tagged = canonical.replace(
        "summary: fictional jobs require blue mode",
        "summary: fictional jobs require blue mode\ntags: [memory]",
    );
    assert_eq!(
        parse_wiki_page(&tagged).unwrap_err().to_string(),
        "page has unknown frontmatter key: tags"
    );
    assert_eq!(
        render_update(&update_request(), &tagged, "2026-08-09")
            .unwrap_err()
            .to_string(),
        "page has unknown frontmatter key: tags"
    );
}
