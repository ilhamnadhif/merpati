// @event: prerequest
// @timeout: 200
// @expect: {"error_contains": ["timed out after 200 ms"], "environment": {"early": "yes"}, "max_ms": 1500}
pm.environment.set('early', 'yes');
setTimeout(() => pm.environment.set('late', 'yes'), 5000);
