// @event: prerequest
// @timeout: 300
// @expect: {"error_contains": ["timed out after 300 ms"], "max_ms": 1500}
console.log('before');
while (true) {}
