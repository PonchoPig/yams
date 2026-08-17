# Obsidian compatibility profile

Yams memory corpora (`.agents/memory`) can be opened directly as Obsidian
vaults for browsing, graph, and backlinks. Yams remains the sole write
authority for memory pages; Obsidian is for reading and light body edits.

Reference: `kepano/obsidian-skills` @ `a1dc48e68138490d522c04cbf5822214c6eb1202`
(used as guidance only, not adopted wholesale).

## Supported in Obsidian

- Browsing, graph view, and backlinks over `pages/*.md`.
- Light body edits: typos, prose, callouts (`> [!note]`), highlights
  (`==x==`), inline `#tags`, and `%%` comments. All of these are ordinary
  searchable Markdown text to Yams — `%%` comments stay searchable so no
  content is invisible to agents.
- Wikilinks `[[slug]]`, `[[slug|alias]]`, and `[[slug#heading]]`. The Yams
  validator resolves aliases and headings to their target slug for
  dangling-link and reachability notes.

## Refused

- Frontmatter changes. Pages have exactly the eight canonical scalar keys
  (`slug`, `title`, `type`, `status`, `owner`, `updated`, `verified`,
  `summary`). Any key Obsidian adds (e.g. `tags`, `aliases`, `cssclasses`)
  makes `yams-wiki check`, `yams-wiki compat`, and `yams --write`
  reject the page until the key is removed. Do not use the Obsidian
  properties panel to add properties to memory pages.
- Embeds (`![[...]]`), block references (`[[slug#^id]]`), and block IDs
  (`^id`). `yams-wiki compat` flags them; they create no link-graph edges.
- Non-slug link targets such as `[[Some Page]]`. Use the page slug.

## Ignored by search

- `.obsidian/`, `.trash/`, and any other dot-directory are never scanned or
  indexed.
- Non-Markdown files, including `.base` and `.canvas`, are never indexed.

## Checking a corpus

```sh
yams-wiki compat /path/to/.agents/memory
```

Exit code is zero when the corpus is within profile; violations are listed
on stderr with a non-zero exit. The check never modifies files.

## Opening the vault

Open `.agents/memory` itself as the vault root (Obsidian → Open vault →
open folder). Obsidian does not index dot-folders, so opening a parent
directory as the vault makes `.agents/memory` invisible — no graph, no
backlinks, no Bases results.

## Dashboard

Copy `Memory.base` from this directory into the vault root (alongside
`pages/`) for a table view over all memory pages.
