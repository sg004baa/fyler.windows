import assert from "node:assert/strict";
import test from "node:test";

import { validateFeedback } from "../src/validate.js";

function validFeedback(overrides = {}) {
  return {
    schema_version: 1,
    kind: "impression",
    body: "使いやすいです",
    app_version: "0.1.0",
    os: "windows",
    arch: "x86_64",
    ...overrides,
  };
}

test("accepts valid feedback", () => {
  const value = validFeedback();
  assert.deepEqual(validateFeedback(value), { ok: true, value });
});

test("rejects empty and whitespace-only bodies", () => {
  for (const body of ["", " \t\n", "　\r\n"]) {
    assert.equal(validateFeedback(validFeedback({ body })).ok, false);
  }
});

test("counts body length by Unicode code point", () => {
  assert.equal(validateFeedback(validFeedback({ body: "𩸽".repeat(4000) })).ok, true);
  assert.equal(validateFeedback(validFeedback({ body: "😀".repeat(4001) })).ok, false);
  assert.equal([..."𩸽"].length, 1);
  assert.equal("𩸽".length, 2);
});

test("rejects invalid kinds", () => {
  for (const kind of ["other", "Bug", "", 1, null]) {
    assert.equal(validateFeedback(validFeedback({ kind })).ok, false);
  }
});

test("rejects unknown fields", () => {
  const result = validateFeedback(validFeedback({ root_path: "C:\\secret" }));
  assert.equal(result.ok, false);
  assert.equal(result.reason.includes("C:\\secret"), false);
});

test("rejects invalid schema versions", () => {
  for (const schema_version of [0, 2, "1", null]) {
    assert.equal(validateFeedback(validFeedback({ schema_version })).ok, false);
  }
});

test("rejects non-object payloads", () => {
  for (const value of [null, [], "feedback", 1, true]) {
    assert.equal(validateFeedback(value).ok, false);
  }
});

test("limits metadata fields to 64 Unicode code points", () => {
  for (const field of ["app_version", "os", "arch"]) {
    assert.equal(validateFeedback(validFeedback({ [field]: "𩸽".repeat(64) })).ok, true);
    assert.equal(validateFeedback(validFeedback({ [field]: "𩸽".repeat(65) })).ok, false);
    assert.equal(validateFeedback(validFeedback({ [field]: 1 })).ok, false);
  }
});
