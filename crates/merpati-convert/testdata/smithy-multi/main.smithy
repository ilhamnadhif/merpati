$version: "2.0"

metadata authors = ["Merpati tests"]
metadata team = {name: "shop", oncall: "shop-oncall"}

namespace example.shop

use aws.protocols#restJson1

/// A small shop split over several files.
@restJson1
@title("Shop Service")
@httpBearerAuth
service Shop {
    version: "2024-06-01"
    operations: [GetItem, ListItems, PutPrice]
    errors: [example.common#ServiceError]
}
