interface Env {
  VISITS_DB: D1Database;
}

type CounterRow = {
  value: number;
};

const COUNTER_NAME = "terminal";

function json(body: object, status = 200): Response {
  return Response.json(body, {
    status,
    headers: {
      "Cache-Control": "no-store",
      "Content-Type": "application/json; charset=utf-8",
      "X-Content-Type-Options": "nosniff",
    },
  });
}

async function readCounter(database: D1Database): Promise<number | null> {
  const row = await database
    .prepare("SELECT value FROM terminal_counters WHERE name = ?")
    .bind(COUNTER_NAME)
    .first<CounterRow>();

  return row ? Number(row.value) : null;
}

export const onRequestGet: PagesFunction<Env> = async ({ env }) => {
  try {
    const hits = await readCounter(env.VISITS_DB);
    return hits === null
      ? json({ error: "counter unavailable" }, 503)
      : json({ hits });
  } catch {
    return json({ error: "counter unavailable" }, 503);
  }
};

export const onRequestPost: PagesFunction<Env> = async ({
  env,
  request,
}) => {
  const fetchSite = request.headers.get("Sec-Fetch-Site");
  if (fetchSite && fetchSite !== "same-origin" && fetchSite !== "none") {
    return json({ error: "forbidden" }, 403);
  }

  try {
    const row = await env.VISITS_DB.prepare(
      "UPDATE terminal_counters SET value = value + 1 WHERE name = ? RETURNING value",
    )
      .bind(COUNTER_NAME)
      .first<CounterRow>();

    return row
      ? json({ hits: Number(row.value) })
      : json({ error: "counter unavailable" }, 503);
  } catch {
    return json({ error: "counter unavailable" }, 503);
  }
};
