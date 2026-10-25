// @event: prerequest
// @variables: {"environment": {"skipIt": "true"}}
// @expect: {"skip_request": true, "errors": 0}
if (pm.environment.get('skipIt') === 'true') {
    pm.execution.skipRequest();
}
