"use strict";

const test = require("node:test");
const assert = require("node:assert/strict");
const { add, multiply } = require("./calc");

test("add returns the sum", () => {
  assert.equal(add(2, 3), 5);
  assert.equal(add(-1, 1), 0);
  assert.equal(add(0, 0), 0);
});

test("multiply returns the product", () => {
  assert.equal(multiply(3, 4), 12);
  assert.equal(multiply(-2, 5), -10);
});