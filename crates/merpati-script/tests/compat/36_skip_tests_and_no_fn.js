// @event: prerequest
// @expect: {"tests": [["a", true], ["skipped one", true], ["no fn", true]], "skipped": ["skipped one", "no fn"], "errors": 0}
pm.test('a', () => {});
pm.test.skip('skipped one', () => { throw new Error('should not run'); });
pm.test('no fn');
