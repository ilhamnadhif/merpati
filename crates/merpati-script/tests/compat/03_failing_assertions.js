// @response: {"code": 404, "status": "Not Found", "headers": [["Content-Type", "text/html"]], "body": "<h1>nope</h1>"}
// @expect: {"tests": [["status", false], ["equal", false], ["ok", false], ["header", false], ["json", false], ["include", false], ["deep", false]], "test_errors": {"status": "AssertionError: expected response to have status code 200 but got 404", "equal": "AssertionError: expected 404 to equal 200", "ok": "expected response code to be 200 but found 404", "header": "expected response to have header with key 'X-Missing'", "json": "valid json", "include": "expected [ 1, 2, 3 ] to include 4", "deep": "expected { a: 1 } to deeply equal { a: 2 }"}, "errors": 0}
pm.test("status", () => pm.response.to.have.status(200));
pm.test("equal", () => pm.expect(pm.response.code).to.equal(200));
pm.test("ok", () => pm.response.to.be.ok);
pm.test("header", () => pm.response.to.have.header("X-Missing"));
pm.test("json", () => pm.response.to.have.jsonBody());
pm.test("include", () => pm.expect([1, 2, 3]).to.include(4));
pm.test("deep", () => pm.expect({a: 1}).to.eql({a: 2}));
