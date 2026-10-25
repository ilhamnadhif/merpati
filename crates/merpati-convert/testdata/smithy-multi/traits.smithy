$version: "2.0"

namespace example.shop

// Traits for shapes of other files: an operation, a member of a JSON AST
// shape and a member inherited from a mixin of another namespace.
apply GetItem @documentation("Gets an item by id.")

apply Item$name @length(min: 1, max: 80)

apply ListItemsInput$pageSize @range(min: 1, max: 100)
