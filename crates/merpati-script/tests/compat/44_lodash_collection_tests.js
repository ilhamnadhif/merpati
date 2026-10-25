// @response: {"code": 200, "body": "{\"data\": {\"items\": [{\"id\": 1, \"price\": 10, \"active\": true}, {\"id\": 2, \"price\": 25, \"active\": true}, {\"id\": 3, \"price\": 5, \"active\": false}]}}"}
// @expect: {"tests": [["every item has id", true], ["find works", true], ["totals", true], ["nested property", true], ["sorted", false]], "errors": 0}
const items = pm.response.json().data.items;
pm.test('every item has id', () => pm.expect(_.every(items, (i) => typeof i.id === 'number')).to.be.true);
pm.test('find works', () => pm.expect(_.find(items, { id: 2 })).to.not.be.undefined);
pm.test('totals', () => {
    pm.expect(_.sumBy(_.filter(items, 'active'), 'price')).to.equal(35);
    pm.expect(_.maxBy(items, 'price').id).to.equal(2);
});
pm.test('nested property', () => pm.expect(pm.response.json()).to.have.nested.property('data.items[0].id', 1));
pm.test('sorted', () => pm.expect(_.map(items, 'price')).to.eql(_.sortBy(_.map(items, 'price'))));
