// @response: {"code": 200, "body": "{\"items\": [{\"id\": 1, \"type\": \"a\"}, {\"id\": 2, \"type\": \"b\"}, {\"id\": 3, \"type\": \"a\"}]}"}
// @expect: {"tests": [["lodash works", true]], "environment": {"ids": "1,3", "deep": "fallback", "grouped": "2"}, "errors": 0}
const items = pm.response.json().items;
const ids = _.map(_.filter(items, { type: 'a' }), 'id');
pm.environment.set('ids', ids.join(','));
pm.environment.set('deep', _.get(pm.response.json(), 'missing.path', 'fallback'));
const lodash = require('lodash');
pm.environment.set('grouped', Object.keys(lodash.groupBy(items, 'type')).length);
pm.test('lodash works', () => {
    pm.expect(_.isEqual(_.uniq([1, 1, 2]), [1, 2])).to.be.true;
    pm.expect(_.chunk([1, 2, 3, 4, 5], 2)).to.have.lengthOf(3);
    pm.expect(require('lodash')).to.equal(_);
});
