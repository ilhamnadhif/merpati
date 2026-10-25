// @event: prerequest
// @expect: {"environment": {"date": "2024-02-29", "plus": "2024-03-01", "unix": "1709164800", "valid": "false"}, "tests": [["moment", true]], "errors": 0}
const moment = require('moment');
const d = moment.utc('2024-02-29T00:00:00Z');
pm.environment.set('date', d.format('YYYY-MM-DD'));
pm.environment.set('plus', d.clone().add(1, 'day').format('YYYY-MM-DD'));
pm.environment.set('unix', d.unix());
pm.environment.set('valid', moment('not a date', 'YYYY-MM-DD', true).isValid());
pm.test('moment', () => {
    pm.expect(moment().isAfter(moment().subtract(1, 'hours'))).to.be.true;
    pm.expect(moment.duration(90, 'minutes').asHours()).to.equal(1.5);
});
