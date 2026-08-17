use std::borrow::Cow;

use unicode_general_category::{GeneralCategory, get_general_category};

const SAFE_STYLES: [&str; 3] = ["\u{1b}[0m", "\u{1b}[1m", "\u{1b}[4m"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalText {
    Inline,
    Multiline,
    RenderedFrame,
}

/// Remove terminal-active and bidi/format controls while retaining visible payload evidence.
///
/// Machine-readable JSON must not call this function: its values remain exact
/// and its renderer is responsible for escaping controls. Human renderers use
/// `Inline` for untrusted fields, `Multiline` for full bodies, and
/// `RenderedFrame` as the final choke point around trusted styling.
pub fn sanitize_terminal(input: &str, context: TerminalText) -> Cow<'_, str> {
    let mut output = String::with_capacity(input.len());
    let mut index = 0;

    while index < input.len() {
        let remainder = &input[index..];
        if context == TerminalText::RenderedFrame
            && let Some(style) = SAFE_STYLES
                .iter()
                .find(|style| remainder.starts_with(**style))
        {
            output.push_str(style);
            index += style.len();
            continue;
        }

        let character = remainder
            .chars()
            .next()
            .expect("index remains at a UTF-8 boundary inside the string");
        index += character.len_utf8();
        if is_unsafe_control(character) {
            let layout_allowed =
                context != TerminalText::Inline && matches!(character, '\t' | '\n');
            if layout_allowed {
                output.push(character);
            }
        } else {
            output.push(character);
        }
    }

    if output == input {
        Cow::Borrowed(input)
    } else {
        Cow::Owned(output)
    }
}

fn is_unsafe_control(character: char) -> bool {
    matches!(character, '\u{0000}'..='\u{001f}' | '\u{007f}'..='\u{009f}')
        || matches!(get_general_category(character), GeneralCategory::Format)
}
