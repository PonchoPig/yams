use sha2::{Digest, Sha256};

pub const LAYOUT_VERSION: u32 = 1;
pub const SCHEMA: &str = include_str!("../../assets/repository-memory-v1/SCHEMA.md");
pub const PAGE_TEMPLATE: &str = include_str!("../../assets/repository-memory-v1/page-template.md");
pub const AGENT_POLICY: &str = include_str!("../../assets/repository-memory-v1/agent-policy.md");
pub const INDEX_TEMPLATE: &str =
    include_str!("../../assets/repository-memory-v1/index-template.md");
pub const MEMORY_GITIGNORE: &str =
    include_str!("../../assets/repository-memory-v1/memory-gitignore");

pub fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::{
        BEGIN_MARKER, END_MARKER, ReindexOptions, capabilities, check_wiki, parse_wiki_page,
        reindex_wiki,
    };

    const EXPECTED_POLICY: &str = concat!(
        "## Project memory\n\n",
        "- Search early with `yams --json -k 5 \"<focused question>\"` when project history, conventions, decisions, or prior failures may matter.\n",
        "- Treat hits as leads and verify consequential claims against current code, tests, documentation, or Git history.\n",
        "- Interpret exit status 1 as an empty result from a searched store. Retry exit status 3, below the confidence gate, once with `--no-gate` for possible leads. Treat exit status 4 as an operational failure whose JSON names the fault — when the JSON `code` is `store_missing`, run `yams --index` from the project — and never as missing memory.\n",
        "- Never initialize a missing corpus unless the user explicitly asks.\n",
        "- Before changing `.agents/memory/`, inspect `git status --short .agents/memory/` and do not stack changes on another writer's uncommitted work.\n",
        "- Before any memory write, read `.agents/memory/SCHEMA.md` completely and follow it. `yams-wiki write` regenerates the catalog automatically; after direct page edits or recovery, run `yams-wiki catalog .agents/memory`. Never hand-edit the generated index region.\n",
        "- Preserve only verified, durable, reusable knowledge; never preserve secrets, transcripts, speculation, or temporary task progress.\n",
    );

    fn rendered_project_context() -> String {
        PAGE_TEMPLATE
            .replace("{{slug}}", "project-context")
            .replace("{{title}}", "Fictional project context")
            .replace("{{type}}", "project-state")
            .replace("{{date}}", "2026-08-12")
            .replace(
                "{{summary}}",
                "fictional project uses a stable context page",
            )
            .replace(
                "{{fact}}",
                "The fictional project keeps durable context here.",
            )
            .replace(
                "{{evidence}}",
                "A synthetic repository test verifies this page.",
            )
            .replace(
                "{{application}}",
                "Read this page before changing the fictional project.",
            )
            .replace(
                "{{falsifier}}",
                "The fictional project no longer using this context page.",
            )
    }

    #[test]
    fn embedded_assets_match_the_wiki_contract() {
        assert!(SCHEMA.contains("owner must be exactly `claude | codex | shared`"));
        assert!(SCHEMA.contains("yams-wiki catalog .agents/memory"));
        assert!(PAGE_TEMPLATE.contains("**Falsified by:** {{falsifier}}"));
        assert!(AGENT_POLICY.starts_with("## Project memory\n"));
        assert_eq!(INDEX_TEMPLATE.matches(BEGIN_MARKER).count(), 1);
        assert_eq!(INDEX_TEMPLATE.matches(END_MARKER).count(), 1);
        assert_eq!(sha256(SCHEMA.as_bytes()).len(), 64);
    }

    #[test]
    fn embedded_assets_pin_immutable_v1_bytes() {
        assert_eq!(LAYOUT_VERSION, 1);
        assert_eq!(AGENT_POLICY, EXPECTED_POLICY);
        assert_eq!(
            AGENT_POLICY
                .lines()
                .filter(|line| line.starts_with("- "))
                .count(),
            7
        );
        assert_eq!(INDEX_TEMPLATE, format!("{BEGIN_MARKER}\n\n{END_MARKER}\n"));
        for (name, asset) in [
            ("SCHEMA.md", SCHEMA),
            ("page-template.md", PAGE_TEMPLATE),
            ("agent-policy.md", AGENT_POLICY),
            ("index-template.md", INDEX_TEMPLATE),
            ("memory-gitignore", MEMORY_GITIGNORE),
        ] {
            assert!(asset.ends_with('\n'), "{name} lacks its final newline");
            assert!(
                !asset.strip_suffix('\n').unwrap().ends_with('\n'),
                "{name} has more than one final newline"
            );
            assert!(!asset.contains(['\r', '\0']), "{name} has unsafe bytes");
        }
        assert!(SCHEMA.starts_with("# Shared Agent Memory — Schema\n"));
        assert!(PAGE_TEMPLATE.starts_with("---\nslug: {{slug}}\n"));
        assert!(AGENT_POLICY.starts_with("## Project memory\n"));
        assert_eq!(
            MEMORY_GITIGNORE,
            "# Recreated by yams-wiki write and catalog. Do not commit.\n.write.lock\n"
        );

        assert_eq!(
            sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        // These hashes make v1 byte changes require deliberate review and a version decision.
        for (name, asset, expected) in [
            (
                "SCHEMA.md",
                SCHEMA,
                "7390450d1cffac8b9d1dd9202de49d3e6d1682c46dbe49b929466c084bff2d1b",
            ),
            (
                "page-template.md",
                PAGE_TEMPLATE,
                "f8c37433ea209c14cd96b4853f368bd55e0732fe5a39d89ca3a0e57f103f4759",
            ),
            (
                "agent-policy.md",
                AGENT_POLICY,
                "8c9206fa774c3893c162c77ed9822e51945488346f678e00291862c754c4dee4",
            ),
            (
                "index-template.md",
                INDEX_TEMPLATE,
                "87b5017ad7db1320aca0e3e58b153ed45bde028368259857e2b094ab566e0732",
            ),
            (
                "memory-gitignore",
                MEMORY_GITIGNORE,
                "7fc9dfeaaa2ddca82e0572ad0f4e0bf109e30db8de47c0c1d48f54eda7376a9f",
            ),
        ] {
            assert_eq!(sha256(asset.as_bytes()), expected, "{name}");
        }

        let contracts = capabilities().contracts;
        assert_eq!(contracts.repository_layout, LAYOUT_VERSION);
        assert_eq!(contracts.init_manifest, 3);
    }

    #[test]
    fn workspace_agents_md_matches_the_canonical_policy() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../AGENTS.md");
        let workspace_agents_md = fs::read_to_string(&path).unwrap();
        assert_eq!(workspace_agents_md, AGENT_POLICY);
    }

    #[test]
    fn embedded_assets_render_a_canonical_page_template() {
        let page = rendered_project_context();
        assert!(!page.contains("{{"));
        let parsed = parse_wiki_page(&page).unwrap();
        assert_eq!(parsed.fields().len(), 8);
        assert_eq!(parsed.slug, "project-context");
        assert_eq!(parsed.title, "Fictional project context");
        assert_eq!(parsed.page_type.as_str(), "project-state");
        assert_eq!(parsed.status.as_str(), "current");
        assert_eq!(parsed.owner.as_str(), "shared");
        assert_eq!(parsed.updated, "2026-08-12");
        assert_eq!(parsed.verified, "2026-08-12");
        assert_eq!(
            parsed.summary,
            "fictional project uses a stable context page"
        );
        for label in ["**Why:**", "**How to apply:**", "**Falsified by:**"] {
            assert_eq!(page.matches(label).count(), 1, "{label}");
        }
    }

    #[test]
    fn embedded_assets_seed_reindexes_to_a_valid_idempotent_wiki() {
        let temporary = tempfile::tempdir().unwrap();
        let pages = temporary.path().join("pages");
        fs::create_dir(&pages).unwrap();
        fs::write(temporary.path().join("INDEX.md"), INDEX_TEMPLATE).unwrap();
        fs::write(pages.join("project-context.md"), rendered_project_context()).unwrap();

        let first = reindex_wiki(temporary.path(), &ReindexOptions::default()).unwrap();
        assert!(first.changed);
        let indexed = fs::read(temporary.path().join("INDEX.md")).unwrap();
        let report = check_wiki(temporary.path()).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);

        let second = reindex_wiki(temporary.path(), &ReindexOptions::default()).unwrap();
        assert!(!second.changed);
        assert_eq!(
            fs::read(temporary.path().join("INDEX.md")).unwrap(),
            indexed
        );
    }
}
