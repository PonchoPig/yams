use super::AGENT_POLICY;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyInspection {
    pub heading_count: usize,
    pub exact: bool,
}

pub fn inspect_policy(source: &str) -> PolicyInspection {
    let mut headings = Vec::new();
    let mut level_two = Vec::new();
    let mut fence = None;
    let mut offset = 0;

    for raw_line in source.split_inclusive('\n') {
        let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some((marker, width)) = fence {
            if closes_fence(line, marker, width) {
                fence = None;
            }
        } else if let Some(opening) = opens_fence(line) {
            fence = Some(opening);
        } else {
            if line == "## Project memory" {
                headings.push(offset);
            }
            if line.starts_with("## ") {
                level_two.push(offset);
            }
        }
        offset += raw_line.len();
    }

    let exact = if headings.len() == 1 {
        let start = headings[0];
        let end = level_two
            .iter()
            .copied()
            .find(|candidate| *candidate > start)
            .unwrap_or(source.len());
        source[start..end].trim_end() == AGENT_POLICY.trim_end()
    } else {
        false
    };

    PolicyInspection {
        heading_count: headings.len(),
        exact,
    }
}

fn opens_fence(line: &str) -> Option<(u8, usize)> {
    let bytes = line.as_bytes();
    let indent = bytes.iter().take_while(|byte| **byte == b' ').count();
    if indent > 3 {
        return None;
    }
    let marker = *bytes.get(indent)?;
    if !matches!(marker, b'`' | b'~') {
        return None;
    }
    let width = bytes[indent..]
        .iter()
        .take_while(|byte| **byte == marker)
        .count();
    if width < 3 || (marker == b'`' && bytes[indent + width..].contains(&b'`')) {
        return None;
    }
    Some((marker, width))
}

fn closes_fence(line: &str, marker: u8, opening_width: usize) -> bool {
    let bytes = line.as_bytes();
    let indent = bytes.iter().take_while(|byte| **byte == b' ').count();
    if indent > 3 {
        return false;
    }
    let width = bytes[indent..]
        .iter()
        .take_while(|byte| **byte == marker)
        .count();
    width >= opening_width
        && bytes[indent + width..]
            .iter()
            .all(|byte| matches!(byte, b' ' | b'\t'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_heading_is_not_exact() {
        assert_eq!(
            inspect_policy("# Fictional repository\n\nNo project policy here.\n"),
            PolicyInspection {
                heading_count: 0,
                exact: false,
            }
        );
    }

    #[test]
    fn only_exact_unfenced_logical_headings_count() {
        let source = concat!(
            "A ## Project memory substring\n",
            "\"## Project memory\"\n",
            "> ## Project memory\n",
            " ### Project memory\n",
            "### Project memory\n",
            "```md\n## Project memory\n```\n",
            "~~~\n## Project memory\n~~~\n",
        );
        assert_eq!(inspect_policy(source).heading_count, 0);
    }

    #[test]
    fn canonical_policy_through_eof_is_exact() {
        assert_eq!(
            inspect_policy(AGENT_POLICY),
            PolicyInspection {
                heading_count: 1,
                exact: true,
            }
        );
    }

    #[test]
    fn canonical_owned_section_stops_at_next_level_two_heading() {
        let source = format!("# Fictional\n\n{AGENT_POLICY}\n## Another section\nText.\n");
        assert_eq!(
            inspect_policy(&source),
            PolicyInspection {
                heading_count: 1,
                exact: true,
            }
        );
    }

    #[test]
    fn changed_missing_and_extra_bullets_are_not_exact() {
        let missing = AGENT_POLICY.replacen(
            "- Never initialize a missing corpus unless the user explicitly asks.\n",
            "",
            1,
        );
        let changed = AGENT_POLICY.replacen("Search early", "Search late", 1);
        let extra = format!("{AGENT_POLICY}- Fictional extra instruction.\n");
        for source in [missing, changed, extra] {
            let inspected = inspect_policy(&source);
            assert_eq!(inspected.heading_count, 1);
            assert!(!inspected.exact);
        }
    }

    #[test]
    fn duplicate_exact_headings_are_not_exact() {
        let source = format!("{AGENT_POLICY}\n{AGENT_POLICY}");
        assert_eq!(
            inspect_policy(&source),
            PolicyInspection {
                heading_count: 2,
                exact: false,
            }
        );
    }

    #[test]
    fn byte_boundaries_control_exactness() {
        let crlf = AGENT_POLICY.replace('\n', "\r\n");
        let no_final_lf = AGENT_POLICY.trim_end_matches('\n');
        assert_eq!(
            inspect_policy(&crlf),
            PolicyInspection {
                heading_count: 1,
                exact: false,
            }
        );
        assert!(inspect_policy(no_final_lf).exact);
        assert!(!inspect_policy(&format!("x{AGENT_POLICY}")).exact);
    }

    #[test]
    fn crlf_fences_hide_only_the_headings_inside_them() {
        let source = concat!(
            "```markdown\r\n",
            "## Project memory\r\n",
            "```\r\n",
            "## Project memory\r\n",
        );
        assert_eq!(inspect_policy(source).heading_count, 1);
    }

    #[test]
    fn invalid_backtick_info_string_does_not_open_a_fence() {
        let source = concat!("```markdown`invalid\n", "## Project memory\n", "```\n",);
        assert_eq!(inspect_policy(source).heading_count, 1);
    }
}
