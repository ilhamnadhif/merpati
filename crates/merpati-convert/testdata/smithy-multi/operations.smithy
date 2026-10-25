$version: "2.0"

namespace example.shop

use example.common#Money
use example.common#Paged
use example.common#audited

@readonly
@http(method: "GET", uri: "/items/{itemId}")
operation GetItem {
    input := {
        @required
        @httpLabel
        itemId: ItemId
    }
    output := {
        @required
        item: Item
    }
    errors: [NotFound]
}

@readonly
@http(method: "GET", uri: "/items")
operation ListItems {
    input := with [Paged] {}
    output := {
        items: ItemList
        nextToken: String
    }
}

@audited(level: "high")
@idempotent
@http(method: "PUT", uri: "/items/{itemId}/price")
operation PutPrice {
    input := {
        @required
        @httpLabel
        itemId: ItemId
        @required
        price: Money
    }
}

list ItemList {
    member: Item
}

@error("client")
@httpError(404)
structure NotFound {
    message: String
}
