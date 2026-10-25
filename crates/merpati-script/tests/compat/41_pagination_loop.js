// @response: {"code": 200, "body": "{\"page\": 2, \"totalPages\": 3, \"items\": [1, 2]}"}
// @variables: {"environment": {"collected": "5"}}
// @expect: {"environment": {"collected": "7", "page": "3"}, "next_request": "List items", "errors": 0}
const body = pm.response.json();
const collected = Number(pm.environment.get('collected') || 0) + body.items.length;
pm.environment.set('collected', collected);
if (body.page < body.totalPages) {
    pm.environment.set('page', body.page + 1);
    pm.execution.setNextRequest('List items');
} else {
    pm.environment.unset('page');
    pm.execution.setNextRequest(null);
}
