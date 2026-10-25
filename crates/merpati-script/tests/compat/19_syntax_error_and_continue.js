// @event: prerequest
// @location: collection "API"
// @expect: {"errors": 1, "error_contains": ["collection \"API\"", "SyntaxError"], "environment": {"ran": "folder,request"}}
pm.environment.set('ran', 'collection'
// @split folder "Users"
pm.environment.set('ran', 'folder');
// @split request "List"
pm.environment.set('ran', pm.environment.get('ran') + ',request');
