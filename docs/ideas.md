# Ideas / deferred

## Local Actions cache server

local-ci (redwoodjs) serves the `actions/cache` REST API from local disk at
~0ms, so `actions/cache` + setup-* built-in caching never leave the machine.
For homerunner the named cache volumes already cover the heavy warm state
(uv, pnpm, Playwright), so the residual win is only the cache-tarball
round-trips to GitHub's cache service — tens of seconds per cache-heavy job.

Deferred because it's a real emulation project: the cache v2 protocol is a
Twirp service plus a blob API, and a *registered* runner receives its cache
URL from GitHub's job message, so redirecting jobs at a local server is
unproven (candidates: `.env`-injected `ACTIONS_CACHE_URL` for the v1 path,
or a proxy). Prior art to study when picking this up: local-ci's DTU server,
nektos/act's `--cache-server`, Gitea's actions cache implementation.

Revisit if a repo leans on large `actions/cache` artifacts.
