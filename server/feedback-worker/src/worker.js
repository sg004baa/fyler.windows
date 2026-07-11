import { validateFeedback } from "./validate.js";

const MAX_REQUEST_BYTES = 32 * 1024;
const JSON_HEADERS = { "content-type": "application/json; charset=utf-8" };

function jsonResponse(status, body, extraHeaders = {}) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { ...JSON_HEADERS, ...extraHeaders },
  });
}

function errorResponse(status, error, extraHeaders = {}) {
  return jsonResponse(status, { ok: false, error }, extraHeaders);
}

async function readBodyWithLimit(request) {
  const contentLength = request.headers.get("content-length");
  if (contentLength !== null) {
    const declaredLength = Number(contentLength);
    if (Number.isFinite(declaredLength) && declaredLength > MAX_REQUEST_BYTES) {
      return { ok: false, status: 413 };
    }
  }

  if (request.body === null) {
    return { ok: true, text: "" };
  }

  const reader = request.body.getReader();
  const chunks = [];
  let totalBytes = 0;
  while (true) {
    const { done, value } = await reader.read();
    if (done) {
      break;
    }
    totalBytes += value.byteLength;
    if (totalBytes > MAX_REQUEST_BYTES) {
      try {
        await reader.cancel();
      } catch {
        // The size rejection is authoritative even if stream cancellation fails.
      }
      return { ok: false, status: 413 };
    }
    chunks.push(value);
  }

  const bytes = new Uint8Array(totalBytes);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.byteLength;
  }
  try {
    return {
      ok: true,
      text: new TextDecoder("utf-8", { fatal: true }).decode(bytes),
    };
  } catch {
    return { ok: false, status: 400 };
  }
}

export default {
  async fetch(request, env) {
    try {
      const url = new URL(request.url);
      if (url.pathname !== "/") {
        return errorResponse(404, "not found");
      }
      if (request.method !== "POST") {
        return errorResponse(405, "method not allowed", { allow: "POST" });
      }

      const contentType = request.headers.get("content-type");
      if (contentType === null || contentType.split(";", 1)[0].trim().toLowerCase() !== "application/json") {
        return errorResponse(415, "content type must be application/json");
      }

      const body = await readBodyWithLimit(request);
      if (!body.ok) {
        return body.status === 413
          ? errorResponse(413, "request body is too large")
          : errorResponse(400, "invalid JSON");
      }

      let parsed;
      try {
        parsed = JSON.parse(body.text);
      } catch {
        return errorResponse(400, "invalid JSON");
      }

      const validation = validateFeedback(parsed);
      if (!validation.ok) {
        return errorResponse(400, "invalid feedback");
      }

      const rateLimit = await env.RATE_LIMITER.limit({
        key: request.headers.get("cf-connecting-ip") ?? "unknown",
      });
      if (!rateLimit.success) {
        return errorResponse(429, "rate limit exceeded");
      }

      const feedback = validation.value;
      const result = await env.DB.prepare(
        "INSERT INTO feedback (received_at, schema_version, kind, body, app_version, os, arch) VALUES (?, ?, ?, ?, ?, ?, ?)",
      )
        .bind(
          new Date().toISOString(),
          feedback.schema_version,
          feedback.kind,
          feedback.body,
          feedback.app_version,
          feedback.os,
          feedback.arch,
        )
        .run();
      if (result.success === false) {
        return errorResponse(500, "failed to store feedback");
      }

      return jsonResponse(201, { ok: true });
    } catch {
      return errorResponse(500, "internal server error");
    }
  },
};
