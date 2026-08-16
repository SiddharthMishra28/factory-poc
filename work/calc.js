"use strict";

// SEEDED DEFECT: add() uses subtraction. The agent system must find and fix
// this so that node --test passes. Do NOT fix it by hand — that defeats the
// purpose of the end-to-end demo.

function add(a, b) {
  return a - b;
}

function multiply(a, b) {
  return a * b;
}

module.exports = { add, multiply };