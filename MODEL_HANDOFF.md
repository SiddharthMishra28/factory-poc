# Factory POC Handoff

## Purpose

Factory is an autonomous coding proof of concept. A Cloudflare Worker creates
and tracks a goal, dispatches role-specific GitHub Actions jobs, and accepts
their structured results. The Rust runner is responsible for the LLM call,
local repository inspection, controlled edits, tests, commits, and reporting.

## Execution Architecture

1. `worker/src/index.ts` exposes the goal, context, and result APIs.
2. `worker/src/goal_room.ts` is the Durable Object state machine. It dispatches
   `agent.yml` for the planner, then developer/QA/evaluator stages. Evaluator
   failure queues fixer/QA/evaluator loops. `RETRY_LIMIT` is 12 by default.
3. `.github/workflows/agent.yml` checks out the target repository, fetches
   context, and starts `agent-runner`. It intentionally uses one global
   concurrency group so NVIDIA NIM traffic is bounded across goals.
4. An evaluator PASS moves the goal to `awaiting_acceptance`; the dashboard
   calls the approval-protected API to mark it `accepted`.
5. `crates/agent-runner/src/main.rs` selects the role, enriches the prompt,
   runs the controlled developer/fixer tool loop, executes `node --test`,
   commits, writes `result.json`, and reports the result to the Worker.
5. `crates/agent-core/src/tools.rs` confines edits to `work/`, scrubs secrets
   from subprocesses, and caps command/file/transcript budgets.

## LLM Providers

`crates/agent-core/src/llm.rs` supports `zen`, `groq`, `nim`, and `mock` via
`LLM_PROVIDER`. NVIDIA NIM is the deployment default:

- Endpoint: `https://integrate.api.nvidia.com/v1/chat/completions`
- Secret: `NVIDIA_API_KEY` only, never source controlled
- Default model: `stepfun-ai/step-3.7-flash` (override with `NIM_MODEL`)
- Payload: non-streaming, temperature `1`, top-p `0.95`, max tokens `16384`,
  seed `42`
- Reliability: 180 second timeout, retry backoff, a two-second request gate,
  and global Actions serialization. Together those limit NIM use to no more
  than 30 requests per minute for the managed workflow.

Set `NVIDIA_API_KEY` and `LLM_PROVIDER=nim` as GitHub Actions secrets before
deployment. The supplied API key must not be placed in `.env.example`, source,
or MCP configuration.

## MCP Integration

`crates/agent-core/src/mcp.rs` implements JSON-RPC for both stdio and
Streamable HTTP MCP servers. Configure the runner with `MCP_CONFIG` (a path to
a JSON file) or `MCP_SERVERS_JSON` (the JSON itself). Use
`mcp.servers.example.json` as the schema reference.

The agent can call `mcp_list_tools` then `mcp_call` in its existing tool loop.
Configuration supports `${ENV_NAME}` expansion for headers and stdio server
environment values; expanded secrets are not inserted into prompts or command
environments. MCP calls have 1-300 second timeouts, defaulting to 60 seconds.

`opencode.json` was changed to read the GitHub MCP token from the environment;
the previously embedded credential is no longer present. Rotate that old token
in GitHub because it had been exposed in the working configuration.

## Safety and Known Limits

- `work/` is intentionally the only agent-writable directory. Broader coding
  scope requires changing `ToolState`, the Worker guardrails, and tests.
- `run_command` is deliberately general but has a scrubbed environment and a
  five-minute timeout. It should gain an allowlist before use against
  untrusted repositories.
- A goal needs an evaluator PASS and an explicit dashboard acceptance before
  it reaches its `accepted` terminal state. Set `GOAL_APPROVAL_TOKEN` as a
  Worker secret and enter it in the dashboard only on the accepting device.
- Worker goal creation is currently public. Production deployment needs an
  authenticated dashboard/session layer or it can be abused to consume CI.
- Current tests are Rust unit tests plus `scripts/local-e2e.ps1`; no remote NIM
  request is made during tests.

## Verification Commands

```powershell
cargo test --workspace
cd worker; npx wrangler check
powershell -ExecutionPolicy Bypass -File scripts\local-e2e.ps1
```
