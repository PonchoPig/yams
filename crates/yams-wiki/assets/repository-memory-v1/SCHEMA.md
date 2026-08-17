# Shared Agent Memory — Schema

This is the canonical authoring and layout contract for version-controlled repository memory. Read it completely before writing or changing a memory page.

`yams-wiki write` enforces canonical create and update requests, writes the page transactionally, and transactionally regenerates the index. `yams-wiki check` validates repository structure, required metadata values, links, and the generated index. `yams-wiki compat` applies the strict frontmatter parser and checks the supported Obsidian profile. Neither `check` nor `compat` silently rewrites pages.

## Pages and frontmatter

Store one durable fact per Markdown file directly under `pages/`. The filename must be `<slug>.md`, and the filename stem must equal the page's `slug`.

Every canonical page begins with a frontmatter block containing exactly these eight scalar keys, once each. Unknown, missing, and duplicate keys are outside the canonical authoring contract.

```yaml
---
slug: example-slug
title: A concise title
type: pattern
status: current
owner: shared
updated: 2026-01-01
verified: 2026-01-01
summary: A nonempty one-line summary of the fact.
---
```

A slug is nonempty, at most 64 bytes, and contains only lowercase ASCII letters, ASCII digits, or `-`. Leading, trailing, and consecutive hyphens are allowed. For new pages, the writer normalizes and case-folds the title into ASCII lowercase letters, digits, and hyphens, then bounds or truncates the result to the slug limit. Callers must inspect the slug returned by `yams-wiki write` rather than assume an exact Unicode transliteration.

The allowed `type` alternatives are exactly `gotcha | pattern | project-state | feature | workflow | decision`. The allowed `status` alternatives are exactly `current | historical | in-progress`. The owner must be exactly `claude | codex | shared`.

Canonical stored dates use ASCII digits in exact `YYYY-MM-DD` form, and `verified` must not be earlier than `updated`. For compatibility with existing material, the parser retains Python-compatible acceptance of Unicode decimal digits in the digit positions and compares stored date strings without digit-script normalization. Do not use non-ASCII digit scripts or mix digit scripts in canonical repository memory. Change `updated` only when substantive page content changes, and change `verified` whenever the fact is checked.

`title` and `summary` must be nonempty scalar text. They must not contain logical line boundaries, C0 or C1 control characters, HTML comment delimiters, index-link-shaped text such as `(pages/example.md)`, or the code-fence markers `` ``` `` and `~~~`. After surrounding whitespace and one matching pair of outer quotes are interpreted by the frontmatter parser, the value must still be nonempty. The summary is inserted into generated Markdown as authored after these safety checks; prefer plain prose unless intentional Markdown is desired.

## Body

`yams-wiki write` emits a canonical body containing the plain fact followed by these required labeled paragraphs:

```markdown
The durable fact stated plainly.

**Why:** Evidence supporting the fact.

**How to apply:** The reusable action or implication.

**Falsified by:** Evidence that would show the fact is no longer true.
```

When related pages exist, one optional final line follows after a blank line:

```markdown
Related: [[another-slug]], [[second-slug]]
```

Related targets follow the same slug rule. The writer refuses self-links and keeps each related target only once.

This body shape is the prescriptive canonical authoring format emitted by `write`. The structural `check` command validates metadata, index membership, links, and drifting line-number references, but it does not enforce the exact body paragraph layout. Use `compat` separately for strict frontmatter parsing and supported Obsidian-profile checks.

## Writing pages

A create request contains the title, page type, owner, four body values, summary, and related slugs:

```json
{
  "title": "A fictional console requires blue mode",
  "type": "gotcha",
  "owner": "shared",
  "fact": "The fictional console rejects jobs unless blue mode is enabled.",
  "why": "A synthetic test observes successful jobs only in blue mode.",
  "how_to_apply": "Enable blue mode before submitting a fictional job.",
  "falsified_by": "A fictional job succeeding while blue mode is disabled.",
  "summary": "fictional console jobs require blue mode",
  "related": []
}
```

Write it with:

```sh
yams-wiki write .agents/memory < request.json
```

An update keeps the stored owner and status, requires `update: true`, identifies the existing slug with `target`, and supplies the complete canonical body and related list:

```json
{
  "title": "A fictional console requires blue mode",
  "type": "gotcha",
  "fact": "The fictional console rejects jobs unless blue mode is enabled.",
  "why": "Two synthetic tests observe successful jobs only in blue mode.",
  "how_to_apply": "Enable blue mode before submitting a fictional job.",
  "falsified_by": "A fictional job succeeding while blue mode is disabled.",
  "summary": "fictional console jobs require blue mode",
  "related": [],
  "update": true,
  "target": "a-fictional-console-requires-blue-mode",
  "expected_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
}
```

Run the same write command for an update. `expected_sha256` is optional; when present, replace the example value with the lowercase SHA-256 digest of the exact current page bytes. It provides compare-and-swap protection against updating a page that changed after it was read.

## Evidence and lifecycle

Record only knowledge that is verified, durable, and reusable. Verify consequential claims against current code, tests, documentation, or Git history. Never store secrets, transcripts, speculation, or temporary task progress.

Update an existing page instead of creating a duplicate. When a fact is superseded, mark it `historical` and add a forward link to the replacement rather than deleting it.

Do not cite source line numbers outside fenced code or URLs because they drift. Name the durable symbol, command, test, document, or commit instead.

## Generated index

`INDEX.md` contains exactly one generated region delimited by these complete-line markers, in this order:

```text
<!-- BEGIN GENERATED INDEX — edited by yams-wiki catalog, not by hand -->
<!-- END GENERATED INDEX -->
```

Do not hand-edit the generated region. Bytes outside the markers are preserved. Within the region, nonempty type groups appear only when needed and in this order: `Gotchas`, `Patterns`, `Decisions`, `Workflow`, `Project state`, `Features — architecture pointers`. Entries are sorted by slug and rendered exactly as `- [slug](pages/slug.md) — summary`.

The bundled `index-template.md` is an initialization seed, not the fixed point of an initialized wiki. After the initializer installs `pages/project-context.md`, it must run `yams-wiki catalog .agents/memory` before validation. A normal `yams-wiki write` regenerates the catalog automatically and transactionally. Run manual `yams-wiki catalog .agents/memory` after direct page edits or recovery, then run `yams-wiki check .agents/memory`.

## Write discipline

Before changing repository memory, inspect `git status --short .agents/memory/`. Do not stack changes on another writer's uncommitted work. Show the proposed durable fact or edit before writing it. Do not commit, push, or open a pull request without separate authorization.
