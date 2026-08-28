---
name: homerunner
description: Debug GitHub Actions jobs that ran on this machine's homerunner self-hosted runners. Use when CI fails (or is slow/stuck) on a repo whose workflows run self-hosted — job history, step logs, failure digests, and kept failed-job workspaces are all local, so prefer this over fetching logs from GitHub with gh.
---

# homerunner: debugging CI locally

homerunner supervises this machine's ephemeral self-hosted GitHub Actions
runners and journals every job: conclusion, timings, full runner step logs,
and — for failures — the entire workspace frozen as a local Docker image.

If the `homerunner` MCP server is connected, its tools (`runner_status`,
`list_jobs`, `job_logs`, `why_failed`) are these same queries; otherwise use
the CLI verbs below. Every query verb takes `--json`.

## When a CI job fails

1. `homerunner why` — digest of the most recent failed job: what ran, the log
   excerpt around the error, whether its workspace was kept. Pass a numeric
   job id for a specific job.
2. Need more than the excerpt? `homerunner logs <job>` prints the full
   captured step logs (`latest`, `latest-failed`, or an id) — grep these
   instead of paging GitHub's log viewer.
3. Reproduce inside the kept workspace (failed jobs only, docker runtime
   only). The checkout lives at `/home/runner/_work/<repo-name>/<repo-name>`
   (repo name twice, not owner/name — don't glob, `_work` holds other dirs),
   with build state and tmpdirs exactly as the job left them:

   ```sh
   homerunner exec <job> -- bash -lc 'cd /home/runner/_work/myrepo/myrepo && <failing command>'
   ```

   The `-- <cmd>` form runs one command with exact argv and no TTY. Do NOT
   run bare `homerunner exec <job>` from an agent shell — that form is an
   interactive shell for humans and will hang without a TTY.
4. After pushing a fix, `homerunner jobs -n 5` shows the rerun's conclusion.

## If something seems wrong with the runners themselves

- `homerunner status` needs the supervisor running; if the dashboard is
  unreachable, job history (`jobs`/`logs`/`why`) still works — only live pool
  state is unavailable.
- `homerunner doctor` checks token, container runtime, runner image, and
  per-repo access.
- The supervisor runs under launchd as `dev.highstorm.homerunner`; restart it
  with `launchctl kickstart -k gui/$(id -u)/dev.highstorm.homerunner`, logs in
  `~/Library/Logs/homerunner/`.
- `homerunner events` tails the live event stream (`job_started`,
  `job_result`, `burst`, …) when you need to watch a run land.

## Caveats

- The journal only covers jobs that ran on this machine's runners. For
  GitHub-hosted jobs, fall back to `gh run view --log-failed`.
- Kept workspaces are a bounded budget (`keep_failed_workspaces`, default 2,
  oldest GC'd) — a workspace referenced by an old failure may be gone; `why`
  and `jobs` tell you whether one exists.
