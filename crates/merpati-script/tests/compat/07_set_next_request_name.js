// @response: {"code": 200, "body": "{\"hasMore\": true}"}
// @expect: {"next_request": "Get page 2", "errors": 0}
if (pm.response.json().hasMore) {
    postman.setNextRequest("Get page 2");
}
