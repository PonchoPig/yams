use yams_core::{TerminalText, sanitize_terminal};

fn c0_c1_controls() -> Vec<char> {
    (0_u32..=0x1f)
        .chain([0x7f])
        .chain(0x80..=0x9f)
        .map(|value| char::from_u32(value).unwrap())
        .collect()
}

#[test]
fn inline_text_strips_every_c0_del_and_c1_control() {
    for control in c0_c1_controls() {
        let input = format!("left{control}right");
        assert_eq!(sanitize_terminal(&input, TerminalText::Inline), "leftright");
    }
}

#[test]
fn multiline_and_rendered_frames_preserve_only_tab_and_line_feed_layout() {
    let input = "one\ttwo\nthree\rfour";
    assert_eq!(
        sanitize_terminal(input, TerminalText::Multiline),
        "one\ttwo\nthreefour"
    );
    assert_eq!(
        sanitize_terminal(input, TerminalText::RenderedFrame),
        "one\ttwo\nthreefour"
    );
    assert_eq!(
        sanitize_terminal(input, TerminalText::Inline),
        "onetwothreefour"
    );
}

#[test]
fn rendered_frames_whitelist_only_reset_bold_and_underline() {
    let escape = '\u{1b}';
    let input = format!(
        "{escape}[1mheading{escape}[4mlink{escape}[0mplain{escape}[7mreverse{escape}[31mred"
    );
    let expected = format!("{escape}[1mheading{escape}[4mlink{escape}[0mplain[7mreverse[31mred");

    assert_eq!(
        sanitize_terminal(&input, TerminalText::RenderedFrame),
        expected
    );
    assert_eq!(
        sanitize_terminal(&input, TerminalText::Inline),
        "[1mheading[4mlink[0mplain[7mreverse[31mred"
    );
}

#[test]
fn hostile_escape_introducers_are_removed_but_printable_evidence_remains() {
    let escape = '\u{1b}';
    let bell = '\u{7}';
    let c1_csi = '\u{9b}';
    let input = format!("before{escape}]52;c;ZmFrZQ=={bell}{escape}Pq{escape}\\{c1_csi}2Jafter");
    let output = sanitize_terminal(&input, TerminalText::RenderedFrame);

    assert_eq!(output, "before]52;c;ZmFrZQ==Pq\\2Jafter");
    assert!(!output.chars().any(|character| character.is_control()));
}

#[test]
fn ordinary_unicode_is_preserved_and_sanitization_is_idempotent() {
    let input = "café 🚀 漢字";
    let once = sanitize_terminal(input, TerminalText::Inline);
    let twice = sanitize_terminal(&once, TerminalText::Inline);

    assert_eq!(once, input);
    assert_eq!(twice, once);
}

#[test]
fn inline_and_multiline_strip_bidi_and_format_controls() {
    let rlo = '\u{202e}';
    let lri = '\u{2066}';
    let rlm = '\u{200f}';
    let zwsp = '\u{200b}';
    let input = format!("trusted{rlo}despoiler{lri}nested{rlm}{zwsp}tail");

    for context in [
        TerminalText::Inline,
        TerminalText::Multiline,
        TerminalText::RenderedFrame,
    ] {
        let output = sanitize_terminal(&input, context);
        assert_eq!(output, "trusteddespoilernestedtail", "{context:?}");
        assert!(!output.chars().any(|character| {
            matches!(
                character,
                '\u{061c}'
                    | '\u{200b}'..='\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
            )
        }));
    }
}

#[test]
fn inline_user_fields_cannot_insert_commands_or_counterfeit_lines() {
    let escape = '\u{1b}';
    for field in ["path", "title", "status", "snippet", "diagnostic"] {
        let input = format!("{field}\nforged{escape}[2Jheader");
        let output = sanitize_terminal(&input, TerminalText::Inline);
        assert_eq!(output, format!("{field}forged[2Jheader"));
        assert!(!output.contains('\n'));
        assert!(!output.contains(escape));
    }
}
