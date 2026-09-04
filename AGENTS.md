<!-- kanspec:begin -->
## kanspec
This repo tracks work, specs, and standing rules with kanspec. `kanspec prime` is auto-injected
at session start; run it yourself if context feels missing.
- Find work: `kanspec ready --json`. Claim before coding: `kanspec start <id>` (creates branch/worktree).
- Diff ready: `kanspec ship <id> --pr <n>`. Finish: `kanspec done <id>` — it will gate you; answer its flags.
- Never state whether something is merged. Merge state is git-detected; report `kanspec show <id>` output.
- Unsure what you owe, or whether you are stuck: `kanspec status` — every line names its own fix.
- Review feedback is work, not scrollback: `kanspec comments --unresolved --json` gives
  {target, quote, body}; answer with `kanspec comment reply <cm-id> --body "..."` and close it
  with `kanspec comment resolve <cm-id> --note "what changed"`.
- Standing rules are `kanspec rules` output ONLY. Closed proposals bind nothing — never read
  .kanspec/proposals/closed/. Never edit an accepted decision; propose one with `kanspec decide`.
- Out-of-scope work you uncover (>5 min): `kanspec new "..."` — one command; it auto-links
  discovered_in to your claimed ticket. Park it and keep going; do NOT expand your current ticket.
  Gotcha learned the hard way: `kanspec quirk add "..." --paths <glob>`.
- Change state ONLY via kanspec verbs — never hand-edit frontmatter. Workflow details: `kanspec instructions`.
<!-- kanspec:end -->
