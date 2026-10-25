// @response: {"code": 200, "headers": [["Content-Type", "text/plain"]], "body": "not json"}
// @expect: {"errors": 1, "error_contains": ["JSONError"]}
const j = pm.response.json();
