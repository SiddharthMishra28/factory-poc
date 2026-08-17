/// <reference types="@cloudflare/workers-types" />

// GoalRoom: one Durable Object per goal (id = goal id). Owns the entire
// state machine and every dispatch decision. Stateless API layer above.

export interface Env {
  GITHUB_TOKEN: string;
  GITHUB_OWNER: string;
  GITHUB_REPO: string;
  GITHUB_BRANCH?: string;
  AGENT_TOKEN: string;
  WEBHOOK_SECRET?: string;
  GOAL_APPROVAL_TOKEN?: string;
  LLM_PROVIDER?: string;
  RETRY_LIMIT?: string;
  GOAL_ROOM: DurableObjectNamespace;
  GOAL_INDEX: KVNamespace;
}

export interface StageRecord {
  id: string;
  role: "planner" | "developer" | "qa" | "evaluator" | "fixer";
  objective: string;
  acceptance_criteria: string[];
  status: "queued" | "running" | "completed" | "failed";
  result?: any;
}

export interface GoalState {
  id: string;
  description: string;
  status: "new" | "planning" | "in_progress" | "awaiting_acceptance" | "accepted" | "failed";
  stages: StageRecord[];
  cursor: number;
  history: string[];
  attempts: number;
  retry_limit: number;
  skills: string[];
  created_at: string;
  updated_at: string;
}

const GUARDRAILS = [
  "Only edit files under the work dir",
  "Never remove tests",
  "No unrelated changes",
];

export class GoalRoom {
  state: DurableObjectState;
  env: Env;
  goal!: GoalState;

  constructor(state: DurableObjectState, env: Env) {
    this.state = state;
    this.env = env;
    this.state.blockConcurrencyWhile(async () => {
      const stored = await this.state.storage.get<GoalState>("goal");
      if (stored) this.goal = stored;
    });
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const stage = url.searchParams.get("stage") || "";
    switch (url.pathname) {
      case "/get":
        if (!this.goal) return new Response("not found", { status: 404 });
        return Response.json(this.goal);
      case "/init": {
        const body: any = await request.json();
        return Response.json(await this.init(body.description));
      }
      case "/context": {
        try {
          return Response.json(this.contextFor(stage));
        } catch (e: any) {
          return new Response(String(e?.message || e), { status: 404 });
        }
      }
      case "/result": {
        const result = await request.json();
        return Response.json(await this.applyResult(stage, result));
      }
      case "/accept":
        return Response.json(await this.accept());
      default:
        return new Response("not found", { status: 404 });
    }
  }

  private async persist() {
    this.goal.updated_at = new Date().toISOString();
    await this.state.storage.put("goal", this.goal);
  }

  async init(description: string): Promise<GoalState> {
    if (!this.goal) {
      this.goal = {
        id: this.state.id.name!,
        description,
        status: "new",
        stages: [],
        cursor: 0,
        history: [],
        attempts: 0,
        retry_limit: Number(this.env.RETRY_LIMIT || 3),
        skills: ["javascript", "testing"],
        created_at: new Date().toISOString(),
        updated_at: new Date().toISOString(),
      };
      await this.persist();
      await this.dispatchStage("planner", "0", "Produce the minimal plan", []);
    }
    return this.goal;
  }

  get(): GoalState {
    return this.goal;
  }

  stageById(id: string): StageRecord | undefined {
    return this.goal.stages.find((s) => s.id === id);
  }

  // ---- dispatch ----------------------------------------------------------

  private async dispatchStage(
    role: StageRecord["role"],
    id: string,
    objective: string,
    acceptance_criteria: string[],
    insertAt?: number
  ) {
    // Only create a new record when the stage isn't already present:
    // runCurrent() dispatches a stage that already exists at `cursor`.
    let stage = this.stageById(id);
    if (!stage) {
      stage = {
        id,
        role,
        objective,
        acceptance_criteria,
        status: "queued",
      };
      if (insertAt !== undefined) {
        this.goal.stages.splice(insertAt, 0, stage);
      } else {
        this.goal.stages.push(stage);
      }
    }
    this.goal.status = "in_progress";
    await this.persist();

    // Fire the GitHub Actions workflow for this stage.
    const resp = await fetch(
      `https://api.github.com/repos/${this.env.GITHUB_OWNER}/${this.env.GITHUB_REPO}/actions/workflows/agent.yml/dispatches`,
      {
        method: "POST",
        headers: {
          Authorization: `Bearer ${this.env.GITHUB_TOKEN}`,
          "X-GitHub-Api-Version": "2022-11-28",
          "User-Agent": "factory-poc-worker",
          "Content-Type": "application/json",
        },
        body: JSON.stringify({
          ref: this.env.GITHUB_BRANCH || "main",
          inputs: { goal_id: this.goal.id, stage_id: id, role },
        }),
      }
    );
    if (!resp.ok) {
      // Surface the failure in the stage record; a later webhook ping retries.
      stage.status = "failed";
      await this.persist();
      throw new Error(`GitHub dispatch failed: ${resp.status} ${await resp.text()}`);
    }
  }

  // ---- result handling (the state machine) -------------------------------

  async applyResult(stageId: string, result: any): Promise<GoalState> {
    if (!this.goal) throw new Error("goal not found");
    const stage = this.stageById(stageId);
    if (!stage) throw new Error(`unknown stage ${stageId}`);

    stage.status = result.status === "completed" ? "completed" : "failed";
    stage.result = result;
    const summary = result.summary || `stage ${stageId} (${stage.role})`;
    this.goal.history.unshift(`${stage.role}: ${summary}`);
    this.goal.history = this.goal.history.slice(0, 20);

    switch (stage.role) {
      case "planner": {
        const plan = result.plan;
        if (!plan?.stages?.length) return this.fail("planner produced no plan");
        this.goal.stages = plan.stages.map((s: any, i: number) => ({
          id: String(i + 1),
          role: s.role,
          objective: s.objective,
          acceptance_criteria: s.acceptance_criteria || [],
          status: "queued",
        }));
        this.goal.cursor = 0;
        await this.persist();
        await this.runCurrent();
        break;
      }
      case "evaluator": {
        if (result.decision === "PASS") {
          this.goal.status = "awaiting_acceptance";
          this.goal.history.unshift("evaluator passed: awaiting user acceptance");
          await this.persist();
          break;
        }
        this.goal.attempts += 1;
        if (this.goal.attempts >= this.goal.retry_limit) {
          return this.fail(`evaluator FAILED ${this.goal.attempts}x (retry limit reached)`);
        }
        const fix = `fix-${this.goal.attempts}`;
        const qa = `qa-${this.goal.attempts}`;
        const ev = `ev-${this.goal.attempts}`;
        this.goal.stages.push(
          { id: fix, role: "fixer", objective: "Fix the bugs the evaluator found", acceptance_criteria: [], status: "queued" },
          { id: qa, role: "qa", objective: "Re-verify after the fix", acceptance_criteria: [], status: "queued" },
          { id: ev, role: "evaluator", objective: "Re-verify independently", acceptance_criteria: [], status: "queued" }
        );
        this.goal.cursor = this.goal.stages.findIndex((s) => s.id === fix);
        await this.persist();
        await this.runCurrent();
        break;
      }
      default:
        // developer / qa / fixer: advance to the next queued stage
        this.goal.cursor += 1;
        await this.persist();
        await this.runCurrent();
    }
    return this.goal;
  }

  private async runCurrent() {
    const stage = this.goal.stages[this.goal.cursor];
    if (!stage) {
      await this.fail("ran out of stages without a PASS");
      return;
    }
    stage.status = "running";
    await this.persist();
    try {
      await this.dispatchStage(stage.role, stage.id, stage.objective, stage.acceptance_criteria);
    } catch (e) {
      console.error("dispatch failed", e);
    }
  }

  private async fail(reason: string): Promise<GoalState> {
    this.goal.status = "failed";
    this.goal.history.unshift(`goal failed: ${reason}`);
    await this.persist();
    return this.goal;
  }

  private async accept(): Promise<GoalState> {
    if (!this.goal) throw new Error("goal not found");
    if (this.goal.status !== "awaiting_acceptance") {
      throw new Error(`goal cannot be accepted while status is ${this.goal.status}`);
    }
    this.goal.status = "accepted";
    this.goal.history.unshift("goal accepted by user");
    await this.persist();
    return this.goal;
  }

  // ---- context for agents --------------------------------------------------

  contextFor(stageId: string) {
    if (!this.goal) throw new Error("goal not found");
    const stage = this.stageById(stageId);
    if (!stage) throw new Error(`unknown stage ${stageId}`);
    return {
      goal: this.goal.description,
      project: this.env.GITHUB_REPO,
      agent: { role: stage.role, personality: "", skills: this.goal.skills },
      stage: { id: stage.id, objective: stage.objective },
      tasks: [stage.objective],
      acceptance_criteria:
        stage.acceptance_criteria.length > 0
          ? stage.acceptance_criteria
          : ["node --test work/ passes"],
      guardrails: GUARDRAILS,
      environment: {
        work_dir: "work",
        llm_provider: this.env.LLM_PROVIDER || "zen",
        model: "",
        retry_limit: this.goal.retry_limit,
        attempt: this.goal.attempts + 1,
      },
      history: { summary: "orchestrator state", previous_results: this.goal.history },
      repository: {
        commit_hash: "",
        recent_commits: [],
        work_dir_files: [],
        files: [],
        test_output: "",
      },
      tools: ["git", "node --test"],
      mcp: [],
    };
  }
}
