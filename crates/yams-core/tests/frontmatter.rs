use std::{collections::BTreeMap, path::Path};

use yams_core::{parse_frontmatter, title_for};

fn fields(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

#[test]
fn real_frontmatter_is_removed_and_nested_fields_are_not_flattened() {
    let source = "\u{feff}---\r\nname: undo-history\r\nmetadata:\r\n  node_type: memory\r\n---\r\n\r\nBody.\r\n";
    let parsed = parse_frontmatter(source);
    assert_eq!(
        parsed.fields.get("name").map(String::as_str),
        Some("undo-history")
    );
    assert!(!parsed.fields.contains_key("node_type"));
    assert_eq!(parsed.body, "Body.\r\n");
}

#[test]
fn prose_between_horizontal_rules_is_not_frontmatter() {
    let source = "---\nThis is prose.\ntitle: A hijack\n---\n\nBody.\n";
    let parsed = parse_frontmatter(source);
    assert!(parsed.fields.is_empty());
    assert_eq!(parsed.body, source);
}

#[test]
fn filename_supplies_a_title_when_frontmatter_does_not() {
    assert_eq!(
        title_for(
            Path::new("prisma-insensitive-ilike.md"),
            &Default::default()
        ),
        "prisma insensitive ilike"
    );
}

#[test]
fn parser_cases_match_the_python_contract() {
    struct Case {
        name: &'static str,
        source: &'static str,
        fields: &'static [(&'static str, &'static str)],
        body: &'static str,
    }

    let cases = [
        Case {
            name: "exact three-dash fences",
            source: "---\nname: alpha\n---\n\nBody.\n",
            fields: &[("name", "alpha")],
            body: "Body.\n",
        },
        Case {
            name: "four-dash opener is prose",
            source: "----\nname: alpha\n---\n\nBody.\n",
            fields: &[],
            body: "----\nname: alpha\n---\n\nBody.\n",
        },
        Case {
            name: "suffix on opener is prose",
            source: "---x\nname: alpha\n---\n\nBody.\n",
            fields: &[],
            body: "---x\nname: alpha\n---\n\nBody.\n",
        },
        Case {
            name: "suffix on closer is unterminated",
            source: "---\nname: alpha\n----\n\nBody.\n",
            fields: &[],
            body: "---\nname: alpha\n----\n\nBody.\n",
        },
        Case {
            name: "BOM",
            source: "\u{feff}---\nname: alpha\n---\n\nBody.\n",
            fields: &[("name", "alpha")],
            body: "Body.\n",
        },
        Case {
            name: "LF",
            source: "---\nname: alpha\n---\n\nBody.\n",
            fields: &[("name", "alpha")],
            body: "Body.\n",
        },
        Case {
            name: "CRLF",
            source: "---\r\nname: alpha\r\n---\r\n\r\nBody.\r\n",
            fields: &[("name", "alpha")],
            body: "Body.\r\n",
        },
        Case {
            name: "CR",
            source: "---\rname: alpha\r---\r\rBody.\r",
            fields: &[("name", "alpha")],
            body: "Body.\r",
        },
        Case {
            name: "empty block",
            source: "---\n---\n\nBody.\n",
            fields: &[],
            body: "---\n---\n\nBody.\n",
        },
        Case {
            name: "all-indented block",
            source: "---\n  name: alpha\n---\n\nBody.\n",
            fields: &[],
            body: "---\n  name: alpha\n---\n\nBody.\n",
        },
        Case {
            name: "unterminated block",
            source: "---\nname: alpha\n\nBody.\n",
            fields: &[],
            body: "---\nname: alpha\n\nBody.\n",
        },
        Case {
            name: "blank line",
            source: "---\nname: alpha\n\nowner: shared\n---\n\nBody.\n",
            fields: &[],
            body: "---\nname: alpha\n\nowner: shared\n---\n\nBody.\n",
        },
        Case {
            name: "quoted values",
            source: "---\ntitle: \"A title\"\nname: 'Fallback'\n---\n\nBody.\n",
            fields: &[("title", "A title"), ("name", "Fallback")],
            body: "Body.\n",
        },
        Case {
            name: "unmatched quotes remain scalar content",
            source: "---\ntitle: \"A title'\n---\n\nBody.\n",
            fields: &[("title", "\"A title'")],
            body: "Body.\n",
        },
        Case {
            name: "dotted and hyphenated field names",
            source: "---\nbuild.id: abc\nnode-type: memory\n---\n\nBody.\n",
            fields: &[("build.id", "abc"), ("node-type", "memory")],
            body: "Body.\n",
        },
        Case {
            name: "vertical tab",
            source: "---\u{000b}name: alpha\u{000b}---\u{000b}\u{000b}Body.\u{000b}",
            fields: &[("name", "alpha")],
            body: "Body.\u{000b}",
        },
        Case {
            name: "form feed",
            source: "---\u{000c}name: alpha\u{000c}---\u{000c}\u{000c}Body.\u{000c}",
            fields: &[("name", "alpha")],
            body: "Body.\u{000c}",
        },
        Case {
            name: "file separator",
            source: "---\u{001c}name: alpha\u{001c}---\u{001c}\u{001c}Body.\u{001c}",
            fields: &[("name", "alpha")],
            body: "Body.\u{001c}",
        },
        Case {
            name: "group separator",
            source: "---\u{001d}name: alpha\u{001d}---\u{001d}\u{001d}Body.\u{001d}",
            fields: &[("name", "alpha")],
            body: "Body.\u{001d}",
        },
        Case {
            name: "record separator",
            source: "---\u{001e}name: alpha\u{001e}---\u{001e}\u{001e}Body.\u{001e}",
            fields: &[("name", "alpha")],
            body: "Body.\u{001e}",
        },
        Case {
            name: "NEL",
            source: "---\u{0085}name: alpha\u{0085}---\u{0085}\u{0085}Body.\u{0085}",
            fields: &[("name", "alpha")],
            body: "Body.\u{0085}",
        },
        Case {
            name: "line separator",
            source: "---\u{2028}name: alpha\u{2028}---\u{2028}\u{2028}Body.\u{2028}",
            fields: &[("name", "alpha")],
            body: "Body.\u{2028}",
        },
        Case {
            name: "paragraph separator",
            source: "---\u{2029}name: alpha\u{2029}---\u{2029}\u{2029}Body.\u{2029}",
            fields: &[("name", "alpha")],
            body: "Body.\u{2029}",
        },
    ];

    for case in cases {
        let parsed = parse_frontmatter(case.source);
        assert_eq!(parsed.fields, fields(case.fields), "{} fields", case.name);
        assert_eq!(parsed.body, case.body, "{} body", case.name);
    }
}

#[test]
fn malformed_candidates_preserve_the_original_source() {
    let source = "\u{feff}---\n  child: value\n---\n\nBody.\n";
    let parsed = parse_frontmatter(source);
    assert!(parsed.fields.is_empty());
    assert_eq!(parsed.body, source);
}

#[test]
fn whitespace_only_first_body_line_is_preserved() {
    let source = "---\nname: alpha\n---\n \t\nBody";
    let parsed = parse_frontmatter(source);

    assert_eq!(parsed.body, " \t\nBody");
}

#[test]
fn title_prefers_frontmatter_and_caps_at_a_character_boundary() {
    let oversized = "é".repeat(201);
    let title_fields = fields(&[("title", &oversized), ("name", "Fallback")]);

    assert_eq!(
        title_for(
            Path::new("fallback_name.md"),
            &fields(&[("name", "Chosen")])
        ),
        "Chosen"
    );
    assert_eq!(
        title_for(Path::new("fallback_name.md"), &Default::default()),
        "fallback name"
    );
    assert_eq!(
        title_for(Path::new("fallback_name.md"), &title_fields)
            .chars()
            .count(),
        200
    );
}
