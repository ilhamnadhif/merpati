// @event: prerequest
// @expect: {"error_contains": ["call stack"], "max_ms": 5000}
function f(n) { return f(n + 1) + 1; }
f(0);
