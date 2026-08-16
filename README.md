# factory-poc

Minimal autonomous multi-agent coding PoC: an orchestrator dispatches
single-purpose agents (planner, developer, qa, evaluator, fixer) that each
run as a one-shot GitHub Actions workflow, inspect a small repo, edit files,
run `node --test`, commit `[skip ci]`, and report back. The orchestrator
drives the state machine until an independent evaluator passes.

```
Browser (dashboard)         Worker (Durable Object)          GitHub Actions
        |  POST /api/goals        | dispatch stage            | checkout repo
        |------------------------>|-------------------------->| run agent-runner
        |                         |                           |   fetch context
        |                         |<--- POST /api/results ----|   LLM -> edit -> test
        |  GET /api/goals/:id     | next stage / pass/fail    |   commit [skip ci]
```

## Layout

- `crates/agent-core` — schema, context enricher, LLM completions (Zen free
  models default, Groq fallback, `mock` provider for offline E2E).
- `crates/agent-runner` — single binary for every role:
  `agent-runner --goal-id <g> --stage-id <s> --role <r> --context-file <f>`.
  Loads context (file or worker `/api/context/...`), inspects the repo
  (git log, file map, `node --test work`), enriches a prompt, calls the LLM,
  writes files, runs tests, commits, pushes, reports.
- `worker/` — Cloudflare Worker + `GoalRoom` Durable Object: goal state
  machine, context serving, GitHub Actions dispatch, result ingestion.
- `dashboard/` — static Pages dashboard (vanilla HTML/JS).
- `work/` — the app the agents work on (seeded defect in `work/calc.js`).
- `schema/agent_context.yml` — wire format of the context each agent gets.
- `scripts/local-e2e.ps1` — offline E2E with the mock LLM provider.
- `.github/workflows/agent.yml` — the actual agent executor (dispatch-only).

## Local E2E (no Cloudflare, no GitHub)

Requires `cargo` and `node`.

```powershell
cargo test --workspace
powershell -ExecutionPolicy Bypass -File scripts\local-e2e.ps1
```

Expected: planner -> developer -> qa (finds the seeded bug) -> evaluator FAIL
-> fixer (fixes `work/calc.js`) -> qa (clean) -> evaluator PASS -> GOAL PASSED.

## Secrets

`.env.example` lists them. The worker needs `GITHUB_TOKEN` (PAT with
`repo` + `workflow` scopes), `GITHUB_OWNER`, `GITHUB_REPO`, `GITHUB_BRANCH`,
`AGENT_TOKEN` (worker-side bearer), `WEBHOOK_SECRET`. The workflow needs
`WORKER_URL`, `AGENT_TOKEN`, `LLM_PROVIDER`, `ZEN_API_KEY`/`ZEN_MODEL` or
`GROQ_API_KEY`/`GROQ_MODEL`.

## Deploy

```powershell
# repo: create via REST API, push, set secrets (see scripts/ or README)
# worker:
cd worker; npx wrangler deploy
# dashboard:
npx wrangler pages deploy dashboard --project-name factory-poc-dashboard
```

Then POST a goal:

```
curl -X POST $WORKER_URL/api/goals -H "x-agent-token: $AGENT_TOKEN" `
  -d '{"description":"Make the calculator in work/ correct; all node --test tests must pass"}'
```

Watch `GET $WORKER_URL/api/goals/:id` (or the dashboard) until `passed`.