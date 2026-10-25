// @event: prerequest
// @location: collection "API"
// @expect: {"errors": 1, "error_contains": ["ReferenceError", "collection \"API\""], "environment": {"ok": "yes"}, "console": ["first", "ReferenceError: notDefined is not defined", "second"]}
console.log('first');
notDefined.call();
// @split request "R"
console.log('second');
pm.environment.set('ok', 'yes');
