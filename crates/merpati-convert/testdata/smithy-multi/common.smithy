$version: "2.0"

metadata authors = ["Common types team"]
metadata team = {name: "shop", oncall: "shop-oncall"}

namespace example.common

/// An amount of money in minor units.
structure Money {
    @required
    amount: Long
    @required
    currency: Currency
}

@pattern("^[A-Z]{3}$")
string Currency

@mixin
structure Paged {
    @httpQuery("nextToken")
    nextToken: String
    @httpQuery("pageSize")
    pageSize: Integer
}

@trait(selector: "operation")
structure audited {
    level: String
}

@error("server")
@httpError(503)
structure ServiceError {
    message: String
}
