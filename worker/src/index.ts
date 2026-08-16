/// <reference types="@cloudflare/workers-types" />

import { GoalRoom, Env } from "./goal_room";

const CORS = {
  "Access-Control-Allow-Origin": "*",
  "Access-Control-Allow-Methods": "GET, POST, OPTIONS",
  "Access-Control-Allow-Headers": "Content-Type, x-agent-token, x-webhook-secret",
  "Access-Control-Max-Age": "86400",
};

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body, null, 2), {
    status,
    headers: { "Content-Type": "application/json", ...CORS },
  });
}

function auth(request: Request, env: Env): boolean {
  const token = request.headers.get("x-agent-token");
  return !!env.AGENT_TOKEN && token === env.AGENT_TOKEN;
}

function room(env: Env, id: string): DurableObjectStub {
  const stubId = env.GOAL_ROOM.idFromName(id);
  return env.GOAL_ROOM.get(stubId);
}

async function listGoals(env: Env): Promise<any[]> {
  const ns = env.GOAL_ROOM as any;
  const ids = await ns.list();
  const goals = await Promise.all(
    ids.keys.map(async (k: any) => {
      try {
        const stub = room(env, k.name);
        const r = await stub.fetch("http://internal/get");
        return await r.json();
      } catch {
        return null;
      }
    })
  );
  return goals.filter(Boolean).sort((a: any, b: any) => (b.created_at < a.created_at ? -1 : 1));
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    if (request.method === "OPTIONS") return new Response(null, { status: 204, headers: CORS });

    const url = new URL(request.url);
    const parts = url.pathname.split("/").filter(Boolean);

    if (url.pathname === "/health") return json({ ok: true });

    // Public reads (dashboard) need no token; writes require it.
    const needAuth =
      request.method === "POST" && !url.pathname.startsWith("/api/webhook/");
    if (needAuth && !auth(request, env)) return json({ error: "unauthorized" }, 401);

    try {
      // --- POST /api/goals {description} ---
      if (request.method === "POST" && url.pathname === "/api/goals") {
        const body: any = await request.json();
        if (!body?.description) return json({ error: "description required" }, 400);
        const id = crypto.randomUUID();
        const stub = room(env, id);
        const init = await stub.fetch("http://internal/init", {
          method: "POST",
          body: JSON.stringify({ description: body.description }),
        });
        if (!init.ok) return json({ error: "init failed", detail: await init.text() }, 500);
        return json(await init.json(), 201);
      }

      // --- GET /api/goals ---
      if (request.method === "GET" && url.pathname === "/api/goals") {
        return json(await listGoals(env));
      }

      // --- GET /api/goals/:id ---
      if (request.method === "GET" && parts[0] === "api" && parts[1] === "goals" && parts[2]) {
        const stub = room(env, parts[2]);
        return json(await stub.fetch("http://internal/get").then((r) => r.json()));
      }

      // --- GET /api/context/:goal/:stage ---
      if (request.method === "GET" && parts[0] === "api" && parts[1] === "context" && parts[2] && parts[3]) {
        if (!auth(request, env)) return json({ error: "unauthorized" }, 401);
        const stub = room(env, parts[2]);
        const r = await stub.fetch(`http://internal/context?stage=${parts[3]}`);
        if (r.status === 404) return json({ error: "stage not found" }, 404);
        return json(await r.json());
      }

      // --- POST /api/results/:goal ---
      if (request.method === "POST" && parts[0] === "api" && parts[1] === "results" && parts[2]) {
        const result: any = await request.json();
        const stageId = result?.stage_id;
        if (!stageId) return json({ error: "stage_id required" }, 400);
        const stub = room(env, parts[2]);
        const r = await stub.fetch(`http://internal/result?stage=${stageId}`, {
          method: "POST",
          body: JSON.stringify(result),
        });
        return json(await r.json());
      }

      // --- POST /api/webhook/github ---
      if (request.method === "POST" && url.pathname === "/api/webhook/github") {
        const secret = request.headers.get("x-webhook-secret");
        if (env.WEBHOOK_SECRET && secret !== env.WEBHOOK_SECRET) {
          return json({ error: "bad webhook secret" }, 401);
        }
        // Re-check the most recent in-progress goal (simple safety net).
        const goals = await listGoals(env);
        const active = goals.find((g) => g.status === "in_progress");
        if (!active) return json({ ok: true, note: "no active goal" });
        return json({ ok: true, goal: active.id });
      }

      return json({ error: "not found" }, 404);
    } catch (e: any) {
      console.error(e);
      return json({ error: String(e?.message || e) }, 500);
    }
  },
} satisfies ExportedHandler<Env>;