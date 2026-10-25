// @response: {"code": 200, "body": "[{\"name\": \"a\"}]"}
// @expect: {"visualization": true, "errors": 0}
const template = `<table>{{#each response}}<tr><td>{{name}}</td></tr>{{/each}}</table>`;
pm.visualizer.set(template, { response: pm.response.json() });
