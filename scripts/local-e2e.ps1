# Local end-to-end run of the agent loop using the MOCK LLM provider.
# Exercises: planner -> developer -> qa -> evaluator(FAIL) -> fixer ->
#            qa -> evaluator(PASS), exactly as the Worker would dispatch.
#
# Usage: powershell -File scripts/local-e2e.ps1

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$runner = Join-Path $root "target\debug\agent-runner.exe"
$scratch = Join-Path $env:TEMP "agent-e2e-$(Get-Date -Format yyyyMMddHHmmss)"

Write-Host "==> Scratch repo: $scratch"
New-Item -ItemType Directory -Path $scratch | Out-Null
Set-Location $scratch
git init -q
git config user.email "agent@factory.local"
git config user.name "factory-agent"
Copy-Item -Recurse (Join-Path $root "work") (Join-Path $scratch "work")
git add -A; git commit -qm "seed: factory-poc work app (seeded defect)"

$env:LLM_PROVIDER = "mock"

function New-Context($role, $stageId, $objective, $history) {
  return @{
    goal               = "Make the calculator in work/ correct: add(a,b) must return the sum; all node --test tests must pass."
    project            = "factory-poc"
    agent              = @{ role = $role; personality = ""; skills = @("javascript", "testing") }
    stage              = @{ id = $stageId; objective = $objective }
    tasks              = @($objective)
    acceptance_criteria = @("node --test work/ passes", "add(2,3) == 5", "multiply(3,4) == 12")
    guardrails         = @("Only edit files under work/", "Never remove tests", "No other changes")
    environment        = @{ work_dir = "work"; llm_provider = "mock"; model = "mock"; retry_limit = 3; attempt = 1 }
    history            = $history
    repository         = @{ commit_hash = ""; recent_commits = @(); work_dir_files = @(); files = @(); test_output = "" }
    tools              = @("git", "node --test")
    mcp                = @()
  } | ConvertTo-Json -Depth 6
}

function Run-Stage([string]$role, [string]$stageId, [string]$objective, $history) {
  Set-Content -Path (Join-Path $scratch "context.json") -Value (New-Context $role $stageId $objective $history)
  & $runner --goal-id "local-e2e" --stage-id $stageId --role $role --context-file context.json | Out-Null
  if ($LASTEXITCODE -ne 0) { throw "agent-runner failed for $role" }
  return Get-Content result.json | ConvertFrom-Json
}

$goalId = "local-e2e"
$historyResults = @()
$fixCount = 0

$plan = (Run-Stage "planner" "0" "Produce the minimal plan" @{ summary = "fresh goal"; previous_results = @() }).plan
Write-Host "==> PLAN: $($plan.stages.Count) stage(s) -> $((($plan.stages | ForEach-Object { $_.role }) -join ', '))"

$queue = @($plan.stages)
while ($queue.Count -gt 0) {
  $stage = $queue[0]
  $queue = @($queue[1..($queue.Count - 1)])

  if ($stage.role -eq "evaluator") {
    $result = Run-Stage "evaluator" $stage.id $stage.objective @{ summary = "verify independently"; previous_results = $historyResults }
    Write-Host "==> EVALUATOR: $($result.decision)"
    if ($result.decision -eq "PASS") { Write-Host "==> GOAL PASSED"; exit 0 }
    if ($fixCount -ge 2) { Write-Host "==> GOAL FAILED (retry limit)"; exit 1 }
    $fixCount++
    $historyResults += "evaluator FAIL: $($result.summary)"
    $queue = @(
      @{ id = "fix-$fixCount"; role = "fixer"; objective = "Fix the bugs the evaluator found"; acceptance_criteria = @("tests pass") },
      @{ id = "qa-$fixCount"; role = "qa"; objective = "Re-verify after the fix"; acceptance_criteria = @("tests pass") },
      @{ id = "eval-$fixCount"; role = "evaluator"; objective = "Re-verify independently"; acceptance_criteria = @("tests pass") }
    ) + $queue
    continue
  }

  $result = Run-Stage $stage.role $stage.id $stage.objective @{ summary = "history"; previous_results = $historyResults }
  Write-Host "==> $($stage.role.ToUpper()): $($result.summary) (commit: $($result.commit))"
  $historyResults += "$($stage.role): $($result.summary)"
}