// @event: prerequest
// @location: collection "API"
// @expect: {"environment": {"fromUtil": "HELLO"}, "errors": 0}
utils = {
    shout: function (s) { return s.toUpperCase(); }
};
// @split request "R"
pm.environment.set('fromUtil', utils.shout('hello'));
