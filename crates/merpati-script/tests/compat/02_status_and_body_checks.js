// @response: {"code": 201, "status": "Created", "headers": [["Content-Type", "application/json"], ["X-Request-Id", "r-1"], ["Set-Cookie", "session=xyz; Path=/; HttpOnly"]], "body": "{\"id\": 42, \"name\": \"Ada\", \"tags\": [\"a\", \"b\"], \"nested\": {\"deep\": {\"value\": 3}}}", "responseTime": 87}
// @expect: {"tests": [["201", true], ["success class", true], ["reason", true], ["header present", true], ["header value", true], ["json body", true], ["jsonBody path", true], ["response time", true], ["content type is json", true], ["body includes", true], ["not 404", true], ["cookie", true]], "errors": 0}
pm.test("201", () => pm.response.to.have.status(201));
pm.test("success class", () => { pm.response.to.be.success; });
pm.test("reason", () => pm.response.to.have.status("Created"));
pm.test("header present", () => pm.response.to.have.header("x-request-id"));
pm.test("header value", () => pm.response.to.have.header("Content-Type", "application/json"));
pm.test("json body", () => { pm.response.to.be.json; pm.response.to.have.jsonBody(); });
pm.test("jsonBody path", () => { pm.response.to.have.jsonBody("nested.deep.value", 3); pm.response.to.have.jsonBody("tags[1]", "b"); });
pm.test("response time", () => { pm.expect(pm.response.responseTime).to.be.below(200); pm.response.to.have.responseTime.below(100); });
pm.test("content type is json", () => pm.expect(pm.response.headers.get('content-type')).to.include('json'));
pm.test("body includes", () => pm.expect(pm.response.text()).to.include('Ada'));
pm.test("not 404", () => pm.response.to.not.have.status(404));
pm.test("cookie", () => { pm.response.to.have.cookie('session'); pm.expect(pm.response.cookies.get('session')).to.eql('xyz'); });
