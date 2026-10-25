$version: "2"

metadata authors = ["Merpati tests"]
metadata "suppressions" = [
    { id: "HttpMethodSemantics", namespace: "example.weather" }
]

namespace example.weather

use aws.protocols#restJson1

/// Provides weather forecasts for cities.
@title("Weather Service")
@restJson1
@httpBearerAuth
@paginated(inputToken: "nextToken", outputToken: "nextToken", pageSize: "pageSize")
service Weather {
    version: "2006-03-01"
    resources: [City]
    operations: [GetCurrentTime]
}

/// A city and its forecasts.
resource City {
    identifiers: { cityId: CityId }
    properties: { coordinates: CityCoordinates, name: String }
    create: CreateCity
    read: GetCity
    list: ListCities
    resources: [Forecast]
}

resource Forecast {
    identifiers: { cityId: CityId }
    properties: { chanceOfRain: Float }
    read: GetForecast
}

// "pattern" is a trait.
@pattern("^[A-Za-z0-9 ]+$")
string CityId

/// Gets a city by id.
@readonly
@http(method: "GET", uri: "/cities/{cityId}")
@examples([
    {
        title: "Seattle"
        input: { cityId: "seattle" }
        output: { name: "Seattle", coordinates: { latitude: 47.6, longitude: -122.3 } }
    }
    {
        title: "Missing city"
        input: { cityId: "atlantis" }
        error: { shapeId: NoSuchResource, content: { resourceType: "City" } }
    }
])
operation GetCity {
    input := for City {
        // "cityId" provides the identifier for the resource and
        // has to be marked as required.
        @required
        @httpLabel
        $cityId
    }

    output := for City {
        @required
        @notProperty
        name: String

        @required
        $coordinates
    }

    errors: [NoSuchResource]
}

/// Registers a new city.
@idempotent
@http(method: "PUT", uri: "/cities", code: 201)
operation CreateCity {
    input := with [ClientTokenMixin] {
        @required
        name: String

        coordinates: CityCoordinates

        @jsonName("climate_zone")
        climate: Climate = "TEMPERATE"

        tags: TagMap

        @timestampFormat("date-time")
        foundedAt: Timestamp
    }

    output := {
        @required
        cityId: CityId
    }

    errors: [NoSuchResource, ServiceUnavailable]
}

@mixin
structure ClientTokenMixin {
    /// Makes retries safe.
    @idempotencyToken
    @httpHeader("X-Client-Token")
    clientToken: String
}

// This structure is nested within GetCityOutput.
structure CityCoordinates {
    @required
    latitude: Float

    @required
    longitude: Float
}

enum Climate {
    TEMPERATE
    TROPICAL = "tropical"
    POLAR
}

map TagMap {
    key: String
    value: String
}

/// The requested resource does not exist.
@error("client")
@httpError(404)
structure NoSuchResource {
    @required
    resourceType: String
}

@error("server")
@retryable
structure ServiceUnavailable {
    message: String
}

// The paginated trait indicates that the operation may
// return truncated results.
@readonly
@paginated(items: "items")
@http(method: "GET", uri: "/cities")
operation ListCities {
    input := {
        @httpQuery("nextToken")
        nextToken: String

        @httpQuery("pageSize")
        @range(min: 1, max: 100)
        pageSize: Integer
    }

    output := {
        nextToken: String

        @required
        items: CitySummaries
    }
}

// CitySummaries is a list of CitySummary structures.
list CitySummaries {
    member: CitySummary
}

// CitySummary contains a reference to a City.
@references([
    {
        resource: City
    }
])
structure CitySummary {
    @required
    cityId: CityId

    @required
    name: String
}

@readonly
@http(method: "GET", uri: "/current-time")
operation GetCurrentTime {
    output := {
        @required
        time: Timestamp
    }
}

@readonly
@http(method: "GET", uri: "/cities/{cityId}/forecast")
operation GetForecast {
    input := for Forecast {
        // "cityId" provides the only identifier for the resource since
        // a Forecast doesn't have its own.
        @required
        @httpLabel
        $cityId

        @httpQuery("units")
        units: Units
    }

    output := for Forecast {
        $chanceOfRain

        precipitation: Precipitation
    }
}

intEnum Units {
    METRIC = 1
    IMPERIAL = 2
}

union Precipitation {
    rain: Float
    snow: Float
}

apply GetCurrentTime @documentation("""
    Returns the current time of the service.
      Indented line stays indented.
    """)

apply CityCoordinates$latitude @range(min: -90, max: 90)
