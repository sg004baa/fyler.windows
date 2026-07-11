const ALLOWED_FIELDS = new Set([
  "schema_version",
  "kind",
  "body",
  "app_version",
  "os",
  "arch",
]);
const ALLOWED_KINDS = new Set(["impression", "request", "bug"]);

function codePointLength(value) {
  return [...value].length;
}

/**
 * Validate a value against the v1 feedback schema.
 *
 * The failure reason is deliberately limited to field names and error classes;
 * it never includes user-provided content.
 */
export function validateFeedback(value) {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return { ok: false, reason: "payload must be an object" };
  }

  for (const field of Object.keys(value)) {
    if (!ALLOWED_FIELDS.has(field)) {
      return { ok: false, reason: "payload contains an unknown field" };
    }
  }

  if (value.schema_version !== 1) {
    return { ok: false, reason: "schema_version must be 1" };
  }
  if (!ALLOWED_KINDS.has(value.kind)) {
    return { ok: false, reason: "kind is invalid" };
  }
  if (typeof value.body !== "string") {
    return { ok: false, reason: "body must be a string" };
  }
  const bodyLength = codePointLength(value.body);
  if (
    bodyLength < 1 ||
    bodyLength > 4000 ||
    /^\p{White_Space}*$/u.test(value.body)
  ) {
    return { ok: false, reason: "body length is invalid" };
  }

  for (const field of ["app_version", "os", "arch"]) {
    const fieldValue = value[field];
    if (typeof fieldValue !== "string" || codePointLength(fieldValue) > 64) {
      return { ok: false, reason: `${field} is invalid` };
    }
  }

  return { ok: true, value };
}
