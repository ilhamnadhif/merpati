// @response: {"code": 200, "headers": [["Content-Type", "text/plain"]], "body": "status=ready"}
// @expect: {"tests": [["body string", true], ["body regex", true], ["text type", true], ["not json", true], ["wrong body", false]], "test_errors": {"wrong body": "expected response body to equal 'status=busy' but got 'status=ready'"}, "errors": 0}
pm.test('body string', () => pm.response.to.have.body('status=ready'));
pm.test('body regex', () => pm.response.to.have.body(/ready$/));
pm.test('text type', () => pm.response.to.be.text);
pm.test('not json', () => pm.response.to.not.be.json);
pm.test('wrong body', () => pm.response.to.have.body('status=busy'));
