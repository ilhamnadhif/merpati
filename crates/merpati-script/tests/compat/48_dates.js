// @event: prerequest
// @expect: {"tests": [["dates", true]], "errors": 0}
const now = new Date();
pm.environment.set('iso', now.toISOString());
pm.environment.set('httpDate', moment.utc(0).format('ddd, DD MMM YYYY HH:mm:ss [GMT]'));
pm.test('dates', () => {
    pm.expect(pm.environment.get('iso')).to.match(/^\d{4}-\d{2}-\d{2}T/);
    pm.expect(pm.environment.get('httpDate')).to.equal('Thu, 01 Jan 1970 00:00:00 GMT');
    pm.expect(Date.parse('2024-01-01T00:00:00Z')).to.equal(1704067200000);
    pm.expect(new Date(Date.UTC(2020, 1, 29)).getUTCDate()).to.equal(29);
    pm.expect(moment('2024-03-10').add(1, 'month').format('YYYY-MM-DD')).to.equal('2024-04-10');
});
